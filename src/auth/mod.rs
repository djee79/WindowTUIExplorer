//! Credentials (CLAUDE.md section 15): the pure part. The Win32 calls are in
//! `platform` (`net_connect`, `net_disconnect`, `net_connections`, `cred_write`).
//!
//! Most of the time there is no prompt: Windows sends the current login, like Explorer.
//! When a share refuses it (access denied, logon failure), sentier asks for a user and
//! password in a masked prompt and connects with `WNetAddConnection2W`, which lasts for the
//! logon session, so robocopy, Explorer and other sentier windows reuse it. A password is
//! never stored by sentier, never logged, never put on a command line: it lives in a
//! [`Secret`] that is wiped as soon as it's dropped.

use crate::fs::path::Loc;
use serde::Deserialize;
use std::collections::BTreeMap;
use zeroize::Zeroizing;

/// A password as the W APIs want it: UTF-16, NUL-terminated, wiped on drop. Its `Debug`
/// never shows it.
#[derive(Clone, Default)]
pub struct Secret(Zeroizing<Vec<u16>>);

impl Secret {
    pub fn from_chars(chars: &[char]) -> Secret {
        let mut v = Zeroizing::new(Vec::with_capacity(chars.len() * 2 + 1));
        let mut buf = [0u16; 2];
        for c in chars {
            v.extend_from_slice(c.encode_utf16(&mut buf));
        }
        v.push(0);
        Secret(v)
    }

    /// With the trailing NUL (for `WNetAddConnection2W`).
    pub fn with_nul(&self) -> &[u16] {
        &self.0
    }

    /// Without it (for `CredWriteW`).
    pub fn without_nul(&self) -> &[u16] {
        self.0.strip_suffix(&[0]).unwrap_or(&self.0)
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.without_nul().is_empty()
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// The Win32 codes that mean "these credentials won't do": ask for others.
pub fn needs_login(code: u32) -> bool {
    matches!(code, 5 | 86 | 1244 | 1326)
}

/// Plain words for the codes `WNetAddConnection2W` and listing return (spec section 15).
pub fn error_message(code: u32) -> String {
    match code {
        0 => "the connection attempt failed".to_string(),
        5 => "access denied".to_string(),
        121 => "timed out (the server didn't answer)".to_string(),
        53 => "path not found or offline".to_string(),
        67 => "no share by that name on the server".to_string(),
        86 => "wrong password".to_string(),
        1219 => "already connected to that server as another user".to_string(),
        1244 => "not authenticated".to_string(),
        1311 => "no logon server available".to_string(),
        1326 => "logon failure: wrong user name or password".to_string(),
        1330 => "password expired".to_string(),
        1331 => "account disabled".to_string(),
        1909 => "account locked".to_string(),
        2202 => "bad user name".to_string(),
        2401 => "files are open on that connection".to_string(),
        other => format!("Windows error {other}"),
    }
}

/// What to connect to for `loc`: its share root (`\\server\share`), or `\\server\IPC$`
/// for a server node (that logs on to the server without naming a share). None for
/// local paths.
pub fn connect_target(loc: &Loc) -> Option<String> {
    match loc {
        Loc::Server(server) => Some(format!(r"\\{server}\IPC$")),
        Loc::Fs(p) if p.is_network() => Some(p.volume_root().display()),
        Loc::Results { origin, .. } => connect_target(origin),
        _ => None,
    }
}

/// The server a target is on: `\\srv01\share` → `srv01`.
pub fn server_of(target: &str) -> &str {
    target
        .trim_start_matches('\\')
        .split('\\')
        .next()
        .unwrap_or_default()
}

/// `\\srv01\share\dir` with the server replaced by `alias` (an IP or another name of the
/// same server): Windows treats it as a different server, which gets around error 1219.
pub fn with_server(path: &str, alias: &str) -> String {
    let rest = path.trim_start_matches('\\');
    match rest.split_once('\\') {
        Some((_, tail)) => format!(r"\\{alias}\{tail}"),
        None => format!(r"\\{alias}"),
    }
}

/// `[auth.<name>]` in config.toml: names an account, never a password.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct AuthProfile {
    pub user: String,
}

/// The profile for `target`: an admin share (`C$`, `D$`, `ADMIN$`) uses `admin` when there
/// is one (spec: machines default to it); everything else the current login, so None.
pub fn profile_for<'a>(
    target: &str,
    profiles: &'a BTreeMap<String, AuthProfile>,
) -> Option<(&'a str, &'a AuthProfile)> {
    let share = target.rsplit('\\').next().unwrap_or_default();
    if share.ends_with('$') && !share.eq_ignore_ascii_case("IPC$") {
        return profiles
            .get_key_value("admin")
            .map(|(k, v)| (k.as_str(), v));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loc(s: &str) -> Loc {
        let Ok(l) = Loc::parse(s) else { panic!("{s}") };
        l
    }

    #[test]
    fn secrets_are_utf16_with_and_without_nul() {
        let s = Secret::from_chars(&['p', 'é', '1']);
        assert_eq!(s.with_nul(), &[0x70, 0xe9, 0x31, 0]);
        assert_eq!(s.without_nul(), &[0x70, 0xe9, 0x31]);
        assert!(!s.is_empty());
        assert!(Secret::from_chars(&[]).is_empty());
        assert_eq!(format!("{s:?}"), "Secret(***)", "never printed");
        // A character outside the BMP is two UTF-16 units.
        assert_eq!(Secret::from_chars(&['😀']).without_nul().len(), 2);
    }

    #[test]
    fn messages_and_login_codes() {
        assert_eq!(
            error_message(1326),
            "logon failure: wrong user name or password"
        );
        assert_eq!(
            error_message(1219),
            "already connected to that server as another user"
        );
        assert_eq!(error_message(9999), "Windows error 9999");
        for code in [5, 86, 1326, 1244] {
            assert!(needs_login(code), "{code}");
        }
        for code in [53, 67, 1219, 1909] {
            assert!(!needs_login(code), "{code}");
        }
    }

    #[test]
    fn targets_table() {
        let cases: &[(&str, Option<&str>)] = &[
            (r"\\srv01\project\HIT\iso", Some(r"\\srv01\project")),
            (r"\\srv01\project", Some(r"\\srv01\project")),
            (r"\\srv01\STD$", Some(r"\\srv01\STD$")),
            (r"\\srv01", Some(r"\\srv01\IPC$")),
            (r"C:\work", None),
        ];
        for (input, want) in cases {
            assert_eq!(connect_target(&loc(input)).as_deref(), *want, "{input}");
        }
        assert_eq!(server_of(r"\\srv01\project"), "srv01");
        assert_eq!(server_of(r"\\srv01\IPC$"), "srv01");
    }

    #[test]
    fn server_connections_and_aliases() {
        // `:net drop srv01` matches by server, case-insensitively, and not srv011.
        assert!(server_of(r"\\SRV01\project").eq_ignore_ascii_case("srv01"));
        assert!(!server_of(r"\\srv011\x").eq_ignore_ascii_case("srv01"));
        assert_eq!(
            with_server(r"\\srv01\project\HIT", "10.0.0.5"),
            r"\\10.0.0.5\project\HIT"
        );
        assert_eq!(with_server(r"\\srv01\IPC$", "10.0.0.5"), r"\\10.0.0.5\IPC$");
        assert_eq!(with_server(r"\\srv01", "10.0.0.5"), r"\\10.0.0.5");
    }

    #[test]
    fn admin_shares_use_the_admin_profile() {
        let mut profiles = BTreeMap::new();
        profiles.insert(
            "admin".to_string(),
            AuthProfile {
                user: r"CORP\adm-user".to_string(),
            },
        );
        let admin =
            |t: &str| profile_for(t, &profiles).map(|(n, p)| (n.to_string(), p.user.clone()));
        assert_eq!(
            admin(r"\\PC-042\C$"),
            Some(("admin".to_string(), r"CORP\adm-user".to_string()))
        );
        assert_eq!(
            admin(r"\\srv01\project"),
            None,
            "shares use the current login"
        );
        assert_eq!(admin(r"\\srv01\IPC$"), None);
        assert_eq!(profile_for(r"\\PC-042\C$", &BTreeMap::new()), None);
    }
}
