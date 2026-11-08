//! Machines in the app (CLAUDE.md section 14, docs/screens/10-machine.txt).
//!
//! - `gm`: the Machines list (pinned machines, then recent ones, newest first). Every
//!   machine opened is remembered for `machines.recent_days` (30); `M` pins one.
//! - Opening a machine (or any `\\HOST\X$` path) records it and probes port 445 first:
//!   an asleep laptop shows `offline · last seen 3 d ago` at once, and a listing stuck on
//!   it is dropped. When it answers, who uses it is looked up (`NetWkstaUserEnum`, else the
//!   newest profile under `C$\Users`), and the path templates are checked.
//! - The machine view lists the templates (`d` Desktop, `w` Downloads, …: one key jumps
//!   there) and `C$` last. `u` picks another user. Templates that need a user nobody has
//!   picked yet are greyed, and so are folders that don't exist ("missing").
//! - `h` from the root of an admin share of a known machine goes back to its view.

use super::App;
use super::keys::PromptKind;
use crate::fs::path::Loc;
use crate::fs::worker::{ListError, MachineUsers};
use crate::fs::{Entry, EntryKind, Listing, Template};
use crate::machines::{self, Recent};
use crate::pins::{Pin, PinKind};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

/// A machine is probed again after this.
const REPROBE: Duration = Duration::from_secs(60);
/// Locations shows at most this many recent machines (pinned ones always).
const LOCATIONS_RECENT: usize = 8;

/// What sentier knows about one machine (keyed by lowercase host).
#[derive(Debug, Clone, Default)]
pub struct MachineInfo {
    /// The port-445 probe: its round trip, or why not.
    pub probe: Option<Result<Duration, String>>,
    probing: bool,
    probed_at: Option<Instant>,
    pub users: Option<Result<MachineUsers, String>>,
    users_asked: bool,
    /// Picked with `u`; otherwise the logged-on user, the pin's hint or the newest profile.
    picked: Option<String>,
    /// Template folders found (true) or not (false), by full path.
    checked: HashMap<String, bool>,
}

impl App {
    fn machine_info(&self, host: &str) -> Option<&MachineInfo> {
        self.machines.get(&host.to_lowercase())
    }

    /// The pin of `host`, if pinned.
    fn machine_pin(&self, host: &str) -> Option<&Pin> {
        self.pins.pins.iter().find(|p| {
            p.kind == PinKind::Machine
                && p.extra
                    .get("host")
                    .and_then(|v| v.as_str())
                    .is_some_and(|h| h.eq_ignore_ascii_case(host))
        })
    }

    /// Who the templates are for on `host`: picked with `u`, else logged on, else the
    /// pin's hint, else the newest profile.
    pub fn machine_user(&self, host: &str) -> Option<String> {
        let info = self.machine_info(host);
        if let Some(p) = info.and_then(|i| i.picked.clone()) {
            return Some(p);
        }
        let users = info
            .and_then(|i| i.users.as_ref())
            .and_then(|r| r.as_ref().ok());
        if let Some(u) = users.and_then(|u| u.logged_on.first()) {
            return Some(u.clone());
        }
        if let Some(hint) = self
            .machine_pin(host)
            .and_then(|p| p.extra.get("user"))
            .and_then(|v| v.as_str())
        {
            return Some(hint.to_string());
        }
        users.and_then(|u| u.likely.clone())
    }

    /// `● online · 4 ms on port 445` / `offline · last seen 3 d ago` / `checking…`.
    pub fn machine_status(&self, host: &str) -> (Option<bool>, String) {
        let seen = self
            .recent_machines
            .iter()
            .find(|r| r.host.eq_ignore_ascii_case(host))
            .and_then(|r| r.seen)
            .and_then(|t| SystemTime::now().duration_since(t).ok());
        match self.machine_info(host).and_then(|i| i.probe.as_ref()) {
            Some(Ok(rtt)) => (Some(true), format!("online · {} ms", rtt.as_millis())),
            Some(Err(why)) => {
                let when = seen.map_or("never seen".to_string(), |d| {
                    format!("last seen {} ago", machines::ago(d))
                });
                (Some(false), format!("offline ({why}) · {when}"))
            }
            None => (None, "checking…".to_string()),
        }
    }

    // ------------------------------------------------------------------ listings

    /// Pinned machines, then recent ones (newest first), without repeats.
    fn machine_hosts(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .pins
            .pins
            .iter()
            .filter(|p| p.kind == PinKind::Machine)
            .filter_map(|p| p.extra.get("host")?.as_str().map(str::to_string))
            .collect();
        for r in &self.recent_machines {
            if !out.iter().any(|h| h.eq_ignore_ascii_case(&r.host)) {
                out.push(r.host.clone());
            }
        }
        out
    }

    fn machine_entry(&self, host: &str) -> Entry {
        let (online, status) = self.machine_status(host);
        Entry::virtual_entry(host.to_string(), EntryKind::Machine(online), status)
    }

    /// The MACHINES section of Locations: pinned ones, and the most recent others.
    pub(super) fn machine_root_entries(&self) -> Vec<Entry> {
        let pinned = self
            .pins
            .pins
            .iter()
            .filter(|p| p.kind == PinKind::Machine)
            .count();
        self.machine_hosts()
            .iter()
            .take(pinned + LOCATIONS_RECENT)
            .map(|h| self.machine_entry(h))
            .collect()
    }

    fn machines_listing(&self) -> Listing {
        Listing {
            entries: self
                .machine_hosts()
                .iter()
                .map(|h| self.machine_entry(h))
                .collect(),
            loaded_at: Instant::now(),
        }
    }

    fn machine_listing(&self, host: &str) -> Listing {
        let user = self.machine_user(host);
        let info = self.machine_info(host);
        let mut entries: Vec<Entry> = self
            .config
            .templates()
            .iter()
            .map(|t| {
                let key = t.key_char().map_or(" ".to_string(), |k| k.to_string());
                let detail = format!("{key}  {}", t.name);
                match t.expand(host, user.as_deref()) {
                    Some(path) => {
                        let state = match info.and_then(|i| i.checked.get(&path)) {
                            Some(true) => Template::There,
                            Some(false) => Template::Missing,
                            None => Template::Unknown,
                        };
                        Entry::virtual_entry(path, EntryKind::Template(state), detail)
                    }
                    // Named with `{user}` left in: opening it asks who (see `open_template`).
                    None => Entry::virtual_entry(
                        format!(r"\\{host}\{}", t.path.trim_start_matches('\\')),
                        EntryKind::Template(Template::NeedsUser),
                        detail,
                    ),
                }
            })
            .collect();
        entries.push(Entry::virtual_entry(
            format!(r"\\{host}\C$"),
            EntryKind::Template(Template::There),
            "   C$".to_string(),
        ));
        Listing {
            entries,
            loaded_at: Instant::now(),
        }
    }

    /// Builds (or rebuilds) the listing of a Machines / machine location.
    pub(super) fn build_machine_view(&mut self, loc: &Loc) {
        let listing = match loc {
            Loc::Machines => self.machines_listing(),
            Loc::Machine(h) => self.machine_listing(h),
            _ => return,
        };
        let state = self.dirs.entry(loc.clone()).or_default();
        state.listing = Some(Arc::new(listing));
        state.error = None;
        state.loading = None;
        if &self.cwd == loc {
            self.rebuild_view();
        }
        self.dirty = true;
    }

    /// Rebuilds every machine view that's cached (after a probe or a user lookup).
    fn refresh_machine_views(&mut self) {
        let locs: Vec<Loc> = self
            .dirs
            .keys()
            .filter(|l| matches!(l, Loc::Machines | Loc::Machine(_)))
            .cloned()
            .collect();
        for l in locs {
            self.build_machine_view(&l);
        }
        if self.at_root() {
            self.reload_root();
        }
    }

    // ------------------------------------------------------------------ flow

    /// Entering a machine view or a path on one of its admin shares: remember it, and
    /// probe it unless that was done a moment ago.
    pub(super) fn touch_machine(&mut self, host: &str) {
        machines::touch(&mut self.recent_machines, host, SystemTime::now());
        if self.recent_loaded {
            self.worker
                .save_recent_machines(self.recent_machines.clone());
        }
        let info = self.machines.entry(host.to_lowercase()).or_default();
        let fresh = info.probed_at.is_some_and(|t| t.elapsed() < REPROBE);
        if !info.probing && !fresh {
            info.probing = true;
            self.worker.machine_probe(host.to_string());
        }
    }

    /// A probe without counting as "opened" (Locations' reachability round). `again`:
    /// even if it was probed a moment ago.
    pub(super) fn probe_machine_quietly(&mut self, host: &str, again: bool) {
        let info = self.machines.entry(host.to_lowercase()).or_default();
        let fresh = info.probed_at.is_some_and(|t| t.elapsed() < REPROBE);
        if !info.probing && (again || !fresh) && (again || info.probe.is_none()) {
            info.probing = true;
            self.worker.machine_probe(host.to_string());
        }
    }

    /// `R` in a machine view: probe and look up the users again.
    pub(super) fn reprobe_machine(&mut self, host: &str) {
        let info = self.machines.entry(host.to_lowercase()).or_default();
        info.probed_at = None;
        info.users_asked = false;
        info.checked.clear();
        self.touch_machine(host);
    }

    pub(super) fn on_machine_probe(&mut self, host: String, result: Result<Duration, String>) {
        let key = host.to_lowercase();
        let up = result.is_ok();
        {
            let info = self.machines.entry(key.clone()).or_default();
            info.probing = false;
            info.probed_at = Some(Instant::now());
            info.probe = Some(result.clone());
        }
        if up {
            if let Some(r) = self
                .recent_machines
                .iter_mut()
                .find(|r| r.host.eq_ignore_ascii_case(&host))
            {
                r.seen = Some(SystemTime::now());
            }
            if self.recent_loaded {
                self.worker
                    .save_recent_machines(self.recent_machines.clone());
            }
            let info = self.machines.entry(key).or_default();
            if !info.users_asked {
                info.users_asked = true;
                self.worker.machine_users(host.clone());
            }
        } else if let Err(why) = &result {
            // Don't wait on SMB for a machine that's off: fail the listing now.
            let cwd = self.cwd.clone();
            if cwd
                .admin_host()
                .is_some_and(|h| h.eq_ignore_ascii_case(&host))
                && !matches!(cwd, Loc::Machine(_))
            {
                if let Some(req) = self.pending.remove(&cwd) {
                    req.cancel.cancel();
                }
                let (_, status) = self.machine_status(&host);
                let state = self.dirs.entry(cwd.clone()).or_default();
                state.loading = None;
                state.error = Some(ListError::Unreachable(status));
                let _ = why;
                self.rebuild_view();
            }
        }
        self.refresh_machine_views();
    }

    pub(super) fn on_machine_users(&mut self, host: String, result: Result<MachineUsers, String>) {
        self.machines.entry(host.to_lowercase()).or_default().users = Some(result);
        self.check_templates(&host);
        self.refresh_machine_views();
    }

    /// Asks which template folders exist for the current user.
    fn check_templates(&mut self, host: &str) {
        let user = self.machine_user(host);
        let paths: Vec<String> = self
            .config
            .templates()
            .iter()
            .filter_map(|t| t.expand(host, user.as_deref()))
            .filter(|p| {
                self.machine_info(host)
                    .is_none_or(|i| !i.checked.contains_key(p))
            })
            .collect();
        if !paths.is_empty() {
            self.worker.check_paths(host.to_string(), paths);
        }
    }

    pub(super) fn on_paths_checked(&mut self, host: String, found: Vec<(String, bool)>) {
        let info = self.machines.entry(host.to_lowercase()).or_default();
        info.checked.extend(found);
        self.refresh_machine_views();
    }

    /// config.toml's `machines.recent_days`: forget machines opened longer ago.
    pub(super) fn apply_recent_days(&mut self) {
        let keep = Duration::from_secs(u64::from(self.config.recent_days()) * 86_400);
        let now = SystemTime::now();
        self.recent_machines
            .retain(|r| now.duration_since(r.opened).map_or(true, |d| d <= keep));
        self.refresh_machine_views();
    }

    pub(super) fn on_recent_machines(&mut self, result: Result<Vec<Recent>, String>) {
        match result {
            Ok(mut list) => {
                // Opened before the file was read: keep those on top.
                for r in std::mem::take(&mut self.recent_machines).into_iter().rev() {
                    machines::touch(&mut list, &r.host, r.opened);
                }
                self.recent_machines = list;
                self.recent_loaded = true;
            }
            Err(e) => tracing::warn!(error = %e, "couldn't read recent machines"),
        }
        self.refresh_machine_views();
    }

    // ------------------------------------------------------------------ keys

    /// In a machine view: a template's key opens it, `u` picks the user. True when the
    /// key was used.
    pub(super) fn machine_key(&mut self, c: char) -> bool {
        let Loc::Machine(host) = self.cwd.clone() else {
            return false;
        };
        if c == 'u' {
            self.pick_machine_user(&host);
            return true;
        }
        let templates = self.config.templates();
        let Some(t) = templates.iter().find(|t| t.key_char() == Some(c)) else {
            return false;
        };
        let user = self.machine_user(&host);
        match t.expand(&host, user.as_deref()) {
            Some(path) => self.cd(&path),
            None => self.set_status(format!("{}: who's the user? u picks one", t.name), false),
        }
        true
    }

    /// Opening a template that still has `{user}` in it.
    pub(super) fn template_needs_user(&mut self, name: &str) -> bool {
        if name.contains("{user}") {
            self.set_status("who's the user? u picks one".into(), false);
            return true;
        }
        false
    }

    fn pick_machine_user(&mut self, host: &str) {
        let now = self.machine_user(host).unwrap_or_default();
        self.machine_user_host = Some(host.to_string());
        self.open_prompt_with(PromptKind::MachineUser, &now);
    }

    /// The users to offer for `u`: logged on first, then the profiles.
    pub fn machine_user_choices(&self) -> Vec<String> {
        let Some(host) = &self.machine_user_host else {
            return Vec::new();
        };
        let Some(Ok(u)) = self.machine_info(host).and_then(|i| i.users.as_ref()) else {
            return Vec::new();
        };
        let mut out = u.logged_on.clone();
        for p in &u.profiles {
            if !out.iter().any(|o| o.eq_ignore_ascii_case(p))
                && machines::likely_user(&[(p.clone(), None)]).is_some()
            {
                out.push(p.clone());
            }
        }
        out
    }

    pub(super) fn machine_user_picked(&mut self, text: &str) {
        let Some(host) = self.machine_user_host.take() else {
            return;
        };
        let user = text.trim();
        let info = self.machines.entry(host.to_lowercase()).or_default();
        info.picked = (!user.is_empty()).then(|| user.to_string());
        self.check_templates(&host);
        self.refresh_machine_views();
        if !user.is_empty() {
            self.set_status(format!("{host}: templates for {user}"), false);
        }
    }

    /// `h` from the root of an admin share of a known machine: back to its view.
    pub(super) fn machine_parent(&self) -> Option<Loc> {
        let p = self.cwd.as_fs()?;
        if !p.is_volume_root() {
            return None;
        }
        let host = self.cwd.admin_host()?;
        let known = self.machine_pin(host).is_some()
            || self
                .recent_machines
                .iter()
                .any(|r| r.host.eq_ignore_ascii_case(host));
        known.then(|| Loc::Machine(host.to_string()))
    }

    /// `gm`.
    pub(super) fn open_machines(&mut self) {
        self.navigate(Loc::Machines, None);
    }

    /// A PC by name (the `machine:` prompt, `Space m` with nothing matching): its
    /// machine view, as if `\\NAME` had been typed. It's probed first (an asleep laptop
    /// says so at once) and remembered as a recent machine; `M` / `m{key}` pin it.
    pub(super) fn open_machine_named(&mut self, text: &str) -> Result<(), String> {
        let host = machines::host_name(text)?;
        self.navigate(Loc::Machine(host.clone()), None);
        self.set_status(
            format!("{host}: checking it answers… (M or m + a key pins it)"),
            false,
        );
        Ok(())
    }

    /// `a` in Machines, "+ Add a machine…": ask for the name (prefilled if typed).
    pub(super) fn ask_machine_name(&mut self, typed: &str) {
        let prefill = machines::host_name(typed).unwrap_or_default();
        self.open_prompt_with(PromptKind::MachineName, &prefill);
    }

    // ------------------------------------------------------------------ pins

    /// `M` on a machine, or `m{key}` in its view: pin it (`kind = "machine"`).
    pub(super) fn pin_machine(&mut self, host: &str, key: Option<String>) {
        if !self.pins_ready() {
            return;
        }
        if let Some(idx) = self.pins.pins.iter().position(|p| {
            p.kind == PinKind::Machine
                && p.extra
                    .get("host")
                    .and_then(|v| v.as_str())
                    .is_some_and(|h| h.eq_ignore_ascii_case(host))
        }) {
            match key {
                Some(k) => self.give_mark(idx, k),
                None => self.set_status(format!("{host} is already pinned"), false),
            }
            return;
        }
        let mut pin = Pin::for_loc(PinKind::Machine, host, &Loc::Machine(host.to_string()));
        pin.path = None;
        pin.extra
            .insert("host".to_string(), toml::Value::String(host.to_string()));
        if let Some(u) = self.machine_user(host) {
            pin.extra.insert("user".to_string(), toml::Value::String(u));
        }
        pin.extra
            .insert("auth".to_string(), toml::Value::String("admin".to_string()));
        self.pin_machine_draft(pin, key);
    }

    /// Opening a machine pin (`'key`, the pin picker).
    pub(super) fn open_machine_pin(&mut self, pin: &Pin) {
        match pin.extra.get("host").and_then(|v| v.as_str()) {
            Some(h) => match Loc::machine(h) {
                Ok(loc) => self.navigate(loc, None),
                Err(e) => self.set_status(e.to_string(), true),
            },
            None => self.set_status(format!("{}: no host", pin.name), true),
        }
    }

    /// The machine under the cursor in Machines / Locations, or the one being viewed.
    pub(super) fn machine_here(&self) -> Option<String> {
        if let Loc::Machine(h) = &self.cwd {
            return Some(h.clone());
        }
        let e = self.selected()?;
        matches!(e.kind, EntryKind::Machine(_)).then(|| e.name.clone())
    }
}

// ---------------------------------------------------------------- the machine picker

/// The slow sources are read again after this (the picker reopens with what's cached).
const SOURCES_STALE: Duration = Duration::from_secs(300);

impl App {
    /// `<leader>m`: pinned machines, then recent ones, then the inventory CSV and
    /// `source_cmd` (they stream in when they arrive). The rows carry the user and the
    /// other details, so typing a person's name finds their machine.
    pub(super) fn open_machine_picker(&mut self, query: &str) {
        if !self.pins_loaded {
            self.set_status("pins are still loading".into(), false);
            return;
        }
        self.load_machine_sources(false);
        let mut items = Vec::new();
        let mut pinned: Vec<String> = Vec::new();
        for (id, p) in self.pins.pins.iter().enumerate() {
            if p.kind != PinKind::Machine {
                continue;
            }
            let Some(host) = p.extra.get("host").and_then(|v| v.as_str()) else {
                continue;
            };
            let mut item = self.pin_pick_item(id, p, &HashMap::new());
            if let Some(r) = self
                .machine_found
                .iter()
                .find(|r| r.host.eq_ignore_ascii_case(host))
            {
                item.detail = record_detail(host, r);
            }
            pinned.push(host.to_string());
            items.push(item);
        }
        let recent: Vec<machines::MachineRecord> = self
            .recent_machines
            .iter()
            .map(|r| machines::MachineRecord {
                host: r.host.clone(),
                user: None,
                details: Vec::new(),
                source: "recent",
            })
            .collect();
        self.machine_rows = machines::merge(vec![recent, self.machine_found.clone()])
            .into_iter()
            .filter(|r| !pinned.iter().any(|h| h.eq_ignore_ascii_case(&r.host)))
            .collect();
        items.extend(
            self.machine_rows
                .iter()
                .enumerate()
                .map(|(id, r)| self.machine_row_item(id, r)),
        );
        // Pinned and recent machines get the port-445 probe (the dot, is:online); the
        // inventory can be thousands, so those are probed when the cursor stops on one.
        let hosts: Vec<String> = pinned
            .into_iter()
            .chain(self.recent_machines.iter().map(|r| r.host.clone()))
            .collect();
        for h in hosts {
            self.probe_machine_quietly(&h, false);
        }
        // A machine not met yet: by name (also: type it, Enter when nothing matches).
        items.push(crate::picker::PickItem {
            id: 0,
            name: "+ Add a machine…".to_string(),
            detail: "or type its name and press Enter · a in gm".to_string(),
            kind: crate::picker::ItemKind::Action,
            key: None,
            tags: Vec::new(),
            unc: false,
            local: false,
            reachable: None,
            server: None,
            score: 0,
        });
        let mut picker = crate::picker::Picker::new(
            "Machines",
            crate::picker::Source::Machines,
            items,
            self.worker.waker(),
            query,
        );
        picker.note = Some("type a name: a new one opens with Enter when nothing matches".into());
        self.show_picker(picker);
    }

    fn machine_row_item(&self, id: usize, r: &machines::MachineRecord) -> crate::picker::PickItem {
        crate::picker::PickItem {
            id,
            name: r.host.clone(),
            detail: record_detail(&r.host, r),
            kind: crate::picker::ItemKind::Machine,
            key: None,
            tags: Vec::new(),
            unc: true,
            local: false,
            reachable: self.machine_status(&r.host).0,
            server: Some(r.host.to_lowercase()),
            score: 0,
        }
    }

    /// Reads the slow sources when configured and not fresh (or `force`).
    pub(super) fn load_machine_sources(&mut self, force: bool) {
        let csv = self.config.machines.inventory_csv.clone();
        let cmd = self.config.machines.source_cmd.clone();
        if (csv.is_none() && cmd.is_none()) || self.machine_found_loading {
            return;
        }
        if !force
            && self
                .machine_found_at
                .is_some_and(|t| t.elapsed() < SOURCES_STALE)
        {
            return;
        }
        self.machine_found_loading = true;
        self.worker.machine_sources(csv, cmd);
        self.dirty = true;
    }

    pub(super) fn on_machine_sources(
        &mut self,
        results: Vec<(&'static str, Result<Vec<machines::MachineRecord>, String>)>,
    ) {
        self.machine_found_loading = false;
        self.machine_found_at = Some(Instant::now());
        self.machine_found_errors.clear();
        let mut lists = Vec::new();
        for (_, r) in results {
            match r {
                Ok(list) => lists.push(list),
                Err(e) => {
                    tracing::warn!(error = %e, "machine source");
                    self.machine_found_errors.push(e);
                }
            }
        }
        self.machine_found = machines::merge(lists);
        if let Some(e) = self.machine_found_errors.first().cloned() {
            self.set_status(e, true);
        }
        self.dirty = true;
        // An open machine picker gets the hosts it doesn't list yet.
        let open = self
            .picker
            .as_ref()
            .is_some_and(|p| p.source == crate::picker::Source::Machines);
        if !open {
            return;
        }
        let pinned: Vec<String> = self
            .pins
            .pins
            .iter()
            .filter(|p| p.kind == PinKind::Machine)
            .filter_map(|p| p.extra.get("host")?.as_str().map(str::to_string))
            .collect();
        let new: Vec<machines::MachineRecord> = self
            .machine_found
            .iter()
            .filter(|r| {
                !pinned.iter().any(|h| h.eq_ignore_ascii_case(&r.host))
                    && !self
                        .machine_rows
                        .iter()
                        .any(|o| o.host.eq_ignore_ascii_case(&r.host))
            })
            .cloned()
            .collect();
        let start = self.machine_rows.len();
        let items: Vec<_> = new
            .iter()
            .enumerate()
            .map(|(i, r)| self.machine_row_item(start + i, r))
            .collect();
        self.machine_rows.extend(new);
        if let Some(p) = self.picker.as_mut() {
            p.add(items);
        }
    }

    /// The machine picker's cursor stopped on a machine: probe it (port 445, quietly).
    pub(super) fn probe_picked_machine(&mut self) {
        let Some(item) = self.picker.as_ref().and_then(|p| p.selected()) else {
            return;
        };
        if let Some(h) = self.picked_host(item) {
            self.probe_machine_quietly(&h, false);
        }
    }

    /// The host of a machine row (pinned or not).
    pub fn picked_host(&self, item: &crate::picker::PickItem) -> Option<String> {
        match item.kind {
            crate::picker::ItemKind::Machine => Some(item.name.clone()),
            crate::picker::ItemKind::Pin(PinKind::Machine) => self
                .pins
                .pins
                .get(item.id)
                .and_then(|p| p.extra.get("host")?.as_str().map(str::to_string)),
            _ => None,
        }
    }

    /// What the sources say about a picker row: its user, details and source.
    pub fn machine_record(
        &self,
        item: &crate::picker::PickItem,
    ) -> Option<machines::MachineRecord> {
        match item.kind {
            crate::picker::ItemKind::Machine => self.machine_rows.get(item.id).cloned(),
            _ => {
                let host = self.picked_host(item)?;
                self.machine_found
                    .iter()
                    .find(|r| r.host.eq_ignore_ascii_case(&host))
                    .cloned()
            }
        }
    }
}

/// `jdoe · laptop · Piping`: what a machine row shows and search runs over.
fn record_detail(host: &str, r: &machines::MachineRecord) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if r.user.is_none() && r.details.is_empty() {
        parts.push(host);
    }
    parts.extend(r.user.as_deref());
    parts.extend(r.details.iter().map(String::as_str));
    parts.join(" · ")
}

impl App {
    /// Every probed machine (lowercase host) and whether it answered.
    pub fn machine_online(&self) -> HashMap<String, bool> {
        self.machines
            .iter()
            .filter_map(|(k, i)| Some((k.clone(), i.probe.as_ref()?.is_ok())))
            .collect()
    }
}
