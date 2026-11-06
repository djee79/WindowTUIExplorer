//! Credentials in the app (CLAUDE.md section 15, docs/screens/11-credentials).
//!
//! When the folder you open on a share is refused (access denied, logon failure), a
//! masked prompt asks for a user and password, once per server until something works.
//! `Esc` leaves the error in place; `:connect` opens the prompt by hand.
//!
//! Keys: `Tab`/`S-Tab` move between User, Password and Remember; `Space` toggles Remember
//! (in the fields it's a space); `Enter` connects; `Esc` cancels. In the password field
//! `Backspace`, `^u` and `^w` edit; nothing is ever echoed. A wrong password asks again
//! with the reason, up to three tries.
//!
//! Error 1219 (already connected to that server as another user) keeps the password and
//! offers the two real fixes: `d` drop the server's other connections and connect again,
//! `a` connect through the server's IP address (Windows sees another server) and open the
//! same folder there.
//!
//! `:net list` shows the live connections, `:net drop <server>` disconnects the ones to a
//! server (not mapped drive letters), `:net drop-all` the ones sentier made.

use super::{App, Mode};
use crate::auth::{self, Secret};
use crate::fs::path::Loc;
use crate::fs::worker::{ConnectHow, ListError};
use crate::platform::NetConnection;
use crate::prompt::{Edit, LineEditor};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use zeroize::Zeroizing;

/// Tries before the prompt gives up (spec: up to 3).
const MAX_TRIES: u8 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    User,
    Password,
    Remember,
}

/// The open credential prompt.
pub struct CredPrompt {
    /// `\\server\share` or `\\server\IPC$`.
    pub target: String,
    /// The `[auth.<name>]` profile the user came from, if any.
    pub profile: Option<String>,
    pub user: LineEditor,
    /// Never shown, wiped when dropped.
    password: Zeroizing<Vec<char>>,
    pub remember: bool,
    pub focus: Field,
    /// Why it's asking: Windows' refusal, then the last failure.
    pub reason: String,
    pub tries: u8,
    /// Waiting for the connection.
    pub busy: bool,
    /// Error 1219 came back: `d` / `a` pick a fix (the password is kept for it).
    pub conflict: bool,
}

impl CredPrompt {
    /// How many `●` to show.
    pub fn password_len(&self) -> usize {
        self.password.len()
    }

    /// The server, for messages.
    pub fn server(&self) -> &str {
        auth::server_of(&self.target)
    }
}

/// `:net list`: the connections shown, with which ones sentier made.
pub struct NetListing {
    pub rows: Vec<(NetConnection, bool)>,
}

impl App {
    /// From a failed listing of the cwd: ask for credentials when the share refused the
    /// login, once per server until something changes.
    pub(super) fn maybe_ask_credentials(&mut self, loc: &Loc, err: &ListError) {
        let code = match err {
            ListError::AccessDenied => 5,
            ListError::LogonFailure(code) => *code,
            _ => return,
        };
        if loc != &self.cwd || self.mode != Mode::Normal || self.cred.is_some() {
            return;
        }
        let Some(target) = auth::connect_target(loc) else {
            return;
        };
        if !self
            .cred_asked
            .insert(auth::server_of(&target).to_lowercase())
        {
            return;
        }
        self.open_credentials(
            target,
            format!("Windows said: {} ({code})", auth::error_message(code)),
        );
    }

    /// `:connect`: the prompt for the cwd's share, whatever happened before.
    pub(super) fn connect_here(&mut self) {
        match auth::connect_target(&self.cwd) {
            Some(target) => {
                self.open_credentials(target, "connect with another account".to_string());
            }
            None => self.set_status(":connect works on a \\\\server\\share path".into(), false),
        }
    }

    fn open_credentials(&mut self, target: String, reason: String) {
        let profile = auth::profile_for(&target, &self.config.auth)
            .map(|(name, p)| (name.to_string(), p.user.clone()));
        let (profile, user) = match profile {
            Some((name, user)) => (Some(name), user),
            None => (None, String::new()),
        };
        let focus = if user.is_empty() {
            Field::User
        } else {
            Field::Password
        };
        self.cred = Some(CredPrompt {
            target,
            profile,
            user: LineEditor::new(&user),
            password: Zeroizing::new(Vec::new()),
            remember: false,
            focus,
            reason,
            tries: 0,
            busy: false,
            conflict: false,
        });
        self.mode = Mode::Credentials;
        self.dirty = true;
    }

    fn close_credentials(&mut self) {
        // Dropping the prompt wipes the password.
        self.cred = None;
        self.mode = Mode::Normal;
        self.dirty = true;
    }

    pub(super) fn on_credentials_key(&mut self, ev: KeyEvent) {
        let Some(p) = self.cred.as_mut() else {
            self.mode = Mode::Normal;
            return;
        };
        self.dirty = true;
        if p.busy {
            // Only Esc while connecting: the answer is dropped.
            if ev.code == KeyCode::Esc {
                self.close_credentials();
                self.set_status("connection abandoned".into(), false);
            }
            return;
        }
        if p.conflict {
            match ev.code {
                KeyCode::Char('d') => self.submit_credentials(ConnectHow::DropOthersFirst),
                KeyCode::Char('a') => self.submit_credentials(ConnectHow::ViaAlias),
                KeyCode::Esc => {
                    self.close_credentials();
                    self.set_status("not connected".into(), false);
                }
                _ => {}
            }
            return;
        }
        let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
        match (ev.code, p.focus) {
            (KeyCode::Esc, _) => {
                self.close_credentials();
                self.set_status("not connected (:connect asks again)".into(), false);
            }
            (KeyCode::Enter, _) => self.submit_credentials(ConnectHow::Plain),
            (KeyCode::Tab, f) => {
                p.focus = match f {
                    Field::User => Field::Password,
                    Field::Password => Field::Remember,
                    Field::Remember => Field::User,
                }
            }
            (KeyCode::BackTab, f) => {
                p.focus = match f {
                    Field::User => Field::Remember,
                    Field::Password => Field::User,
                    Field::Remember => Field::Password,
                }
            }
            (KeyCode::Char(' '), Field::Remember) => p.remember = !p.remember,
            (_, Field::Remember) => {}
            (_, Field::User) => {
                let _: Edit = p.user.handle(&ev, "");
            }
            (KeyCode::Backspace, Field::Password) => {
                p.password.pop();
            }
            (KeyCode::Char('u' | 'w'), Field::Password) if ctrl => p.password.clear(),
            (KeyCode::Char(c), Field::Password) if !ctrl => p.password.push(c),
            _ => {}
        }
    }

    fn submit_credentials(&mut self, how: ConnectHow) {
        let Some(p) = self.cred.as_mut() else { return };
        let user = p.user.text().trim().to_string();
        if user.is_empty() {
            p.reason = "a user name, please (DOMAIN\\user or user@domain)".to_string();
            p.focus = Field::User;
            return;
        }
        if p.password.is_empty() {
            p.reason = "a password, please".to_string();
            p.focus = Field::Password;
            return;
        }
        let secret = Secret::from_chars(&p.password);
        p.busy = true;
        if how == ConnectHow::Plain {
            p.tries += 1;
        }
        let (target, remember) = (p.target.clone(), p.remember);
        tracing::info!(%target, %user, remember, ?how, "connecting with credentials");
        self.worker.net_connect(target, user, secret, remember, how);
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn on_connected(
        &mut self,
        requested: String,
        target: String,
        user: String,
        result: Result<(), u32>,
        remembered: Option<Result<(), String>>,
        dropped: Vec<String>,
    ) {
        // Abandoned (Esc) or another prompt: nothing to do but log it.
        let ours = self
            .cred
            .as_ref()
            .is_some_and(|p| p.busy && p.target == requested);
        match result {
            Ok(()) => {
                tracing::info!(%target, %user, ?dropped, "connected");
                self.connections_made.insert(target.clone());
                self.cred_asked
                    .remove(&auth::server_of(&requested).to_lowercase());
                if ours {
                    self.close_credentials();
                }
                let mut note = String::new();
                if !dropped.is_empty() {
                    note.push_str(&format!(" · disconnected {}", dropped.join(", ")));
                }
                match remembered {
                    Some(Ok(())) => note.push_str(" · remembered in Credential Manager"),
                    Some(Err(e)) => note.push_str(&format!(" · couldn't remember it: {e}")),
                    None => {}
                }
                if target != requested {
                    // Through an alias: the same folder on the alias name.
                    let here = self.cwd.real().display();
                    let alias = auth::server_of(&target).to_string();
                    self.cd(&auth::with_server(&here, &alias));
                } else {
                    self.refresh();
                }
                // After: listing again says "refreshing…", which would hide this.
                self.set_status(format!("connected to {target} as {user}{note}"), false);
            }
            Err(code) => {
                tracing::info!(%target, %user, code, "connect failed");
                if !ours {
                    return;
                }
                let Some(p) = self.cred.as_mut() else { return };
                p.busy = false;
                let why = auth::error_message(code);
                if code == 1219 && !p.conflict {
                    p.conflict = true;
                    p.reason = format!("{why} (1219)");
                } else if auth::needs_login(code) && p.tries < MAX_TRIES {
                    p.conflict = false;
                    p.reason = format!("{why} ({code}) · try {} of {MAX_TRIES}", p.tries + 1);
                    p.password.clear();
                    p.focus = Field::Password;
                } else {
                    self.close_credentials();
                    self.set_status(format!("couldn't connect to {target}: {why}"), true);
                }
            }
        }
        self.dirty = true;
    }

    // ------------------------------------------------------------------ :net

    /// `:net list` / `:net drop <server>` / `:net drop-all`.
    pub(super) fn net_command(&mut self, arg: &str) {
        let words: Vec<&str> = arg.split_whitespace().collect();
        match words.as_slice() {
            ["list"] | [] => self.worker.net_list(),
            ["drop", server] => {
                self.net_drop_server = Some(server.trim_start_matches('\\').to_string());
                self.worker.net_list();
            }
            ["drop-all"] => {
                let names: Vec<String> = self.connections_made.iter().cloned().collect();
                if names.is_empty() {
                    self.set_status("sentier hasn't made any connection".into(), false);
                } else {
                    self.worker.net_drop(names);
                }
            }
            _ => self.set_status("net list | net drop <server> | net drop-all".into(), true),
        }
    }

    pub(super) fn on_net_list(&mut self, result: Result<Vec<NetConnection>, u32>) {
        let list = match result {
            Ok(l) => l,
            Err(code) => {
                self.net_drop_server = None;
                let why = auth::error_message(code);
                self.set_status(format!("couldn't list connections: {why}"), true);
                return;
            }
        };
        // `:net drop <server>` asked for the list to know what to drop.
        if let Some(server) = self.net_drop_server.take() {
            let (mapped, deviceless): (Vec<&NetConnection>, Vec<&NetConnection>) = list
                .iter()
                .filter(|c| auth::server_of(&c.remote).eq_ignore_ascii_case(&server))
                .partition(|c| c.local.is_some());
            let names: Vec<String> = deviceless.iter().map(|c| c.remote.clone()).collect();
            let kept: Vec<String> = mapped.iter().filter_map(|c| c.local.clone()).collect();
            if names.is_empty() {
                let more = if kept.is_empty() {
                    String::new()
                } else {
                    format!(" (mapped drives left alone: {})", kept.join(", "))
                };
                self.set_status(format!("no connection to {server}{more}"), false);
                return;
            }
            if !kept.is_empty() {
                tracing::info!(?kept, "mapped drives left alone");
            }
            self.worker.net_drop(names);
            return;
        }
        let rows = list
            .into_iter()
            .map(|c| {
                let made = self.connections_made.contains(&c.remote);
                (c, made)
            })
            .collect();
        self.net_listing = Some(NetListing { rows });
        self.mode = Mode::NetList;
        self.dirty = true;
    }

    pub(super) fn on_net_dropped(&mut self, results: Vec<(String, Result<(), u32>)>) {
        let mut done = Vec::new();
        let mut failed = Vec::new();
        for (name, r) in results {
            match r {
                Ok(()) => {
                    self.connections_made.remove(&name);
                    done.push(name);
                }
                Err(code) => failed.push(format!("{name}: {}", auth::error_message(code))),
            }
        }
        let msg = match (done.is_empty(), failed.is_empty()) {
            (_, true) => format!("disconnected {}", done.join(", ")),
            (true, false) => format!("not disconnected: {}", failed.join("; ")),
            (false, false) => format!(
                "disconnected {} · not: {}",
                done.join(", "),
                failed.join("; ")
            ),
        };
        self.set_status(msg, !failed.is_empty());
    }

    pub(super) fn on_net_list_key(&mut self, ev: KeyEvent) {
        if matches!(ev.code, KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter) {
            self.net_listing = None;
            self.mode = Mode::Normal;
            self.dirty = true;
        }
    }
}
