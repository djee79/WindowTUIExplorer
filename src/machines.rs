//! Machines (CLAUDE.md section 14): user PCs reached through their admin shares. The pure
//! part: path templates, the recent-machines store, picking the likely user.
//!
//! The Win32 / network calls (the port-445 probe, `NetWkstaUserEnum`, listing
//! `C$\Users`) run in the worker; the machine views are built in `app::machines`.

use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const RECENT_FILE: &str = "machines.tsv";
/// How long an opened machine stays in the recent list (`machines.recent_days`).
pub const DEFAULT_RECENT_DAYS: u32 = 30;

/// One `[[machine.path]]`: a troubleshooting folder on any machine.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PathTemplate {
    pub name: String,
    /// Relative to `\\HOST\`: `C$\Users\{user}\Desktop`. `{user}` and `{host}` expand.
    pub path: String,
    /// One key that jumps there from the machine view.
    #[serde(default)]
    pub key: Option<String>,
}

impl PathTemplate {
    fn new(name: &str, path: &str, key: Option<&str>) -> PathTemplate {
        PathTemplate {
            name: name.to_string(),
            path: path.to_string(),
            key: key.map(str::to_string),
        }
    }

    pub fn key_char(&self) -> Option<char> {
        let mut c = self.key.as_deref()?.chars();
        match (c.next(), c.next()) {
            (Some(k), None) => Some(k),
            _ => None,
        }
    }

    pub fn needs_user(&self) -> bool {
        self.path.contains("{user}")
    }

    /// The full path on `host` for `user`: `\\HOST\C$\Users\jdoe\Desktop`. None when it
    /// needs a user and there's none yet.
    pub fn expand(&self, host: &str, user: Option<&str>) -> Option<String> {
        let mut p = self
            .path
            .trim()
            .trim_start_matches(['\\', '/'])
            .replace('/', "\\");
        if self.needs_user() {
            p = p.replace("{user}", user?);
        }
        p = p.replace("{host}", host);
        Some(format!(r"\\{host}\{p}"))
    }
}

/// The spec's templates, used when config.toml has none.
pub fn default_templates() -> Vec<PathTemplate> {
    vec![
        PathTemplate::new("Desktop", r"C$\Users\{user}\Desktop", Some("d")),
        PathTemplate::new("Downloads", r"C$\Users\{user}\Downloads", Some("w")),
        PathTemplate::new(
            "User temp",
            r"C$\Users\{user}\AppData\Local\Temp",
            Some("t"),
        ),
        PathTemplate::new("Roaming", r"C$\Users\{user}\AppData\Roaming", Some("r")),
        PathTemplate::new("ProgramData", r"C$\ProgramData", Some("p")),
        PathTemplate::new("Windows temp", r"C$\Windows\Temp", None),
    ]
}

/// Profile folders under `C:\Users` that aren't people.
const NOT_USERS: &[&str] = &[
    "public",
    "default",
    "default user",
    "all users",
    "defaultapppool",
    "wdagutilityaccount",
];

/// The most likely user from the profile folders of `C$\Users` (name, last modified):
/// the most recently modified one that isn't a system profile or an admin account.
pub fn likely_user(profiles: &[(String, Option<SystemTime>)]) -> Option<String> {
    let mut people: Vec<&(String, Option<SystemTime>)> = profiles
        .iter()
        .filter(|(n, _)| {
            let l = n.to_lowercase();
            !NOT_USERS.contains(&l.as_str())
                && !l.starts_with("adm-")
                && !l.starts_with("administrator")
                && !l.ends_with('$')
        })
        .collect();
    people.sort_by_key(|p| std::cmp::Reverse(p.1));
    people.first().map(|(n, _)| n.clone())
}

/// The people `NetWkstaUserEnum` reports, without machine accounts (`HOST$`) and
/// duplicates (one user shows once per logon session), in order.
pub fn people(logged_on: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for u in logged_on {
        let u = u.trim();
        if u.is_empty() || u.ends_with('$') {
            continue;
        }
        if !out.iter().any(|o| o.eq_ignore_ascii_case(u)) {
            out.push(u.to_string());
        }
    }
    out
}

/// `3 d`, `5 h`, `12 min`, `now`.
pub fn ago(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        "now".to_string()
    } else if s < 3600 {
        format!("{} min", s / 60)
    } else if s < 86_400 {
        format!("{} h", s / 3600)
    } else {
        format!("{} d", s / 86_400)
    }
}

/// A machine opened lately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recent {
    pub host: String,
    pub opened: SystemTime,
    /// When port 445 last answered.
    pub seen: Option<SystemTime>,
}

fn secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn time(s: &str) -> Option<SystemTime> {
    let n: u64 = s.trim().parse().ok()?;
    (n > 0).then(|| UNIX_EPOCH + Duration::from_secs(n))
}

/// `host \t opened \t seen` per line (Unix seconds; 0 = never), most recent first.
/// Entries older than `days` are dropped. Bad lines are skipped.
pub fn parse_recent(text: &str, now: SystemTime, days: u32) -> Vec<Recent> {
    let keep = Duration::from_secs(u64::from(days) * 86_400);
    let mut out: Vec<Recent> = text
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let mut f = l.split('\t');
            let host = f.next()?.trim().to_string();
            let opened = time(f.next()?)?;
            let seen = f.next().and_then(time);
            (!host.is_empty()).then_some(Recent { host, opened, seen })
        })
        .filter(|r| now.duration_since(r.opened).map_or(true, |d| d <= keep))
        .collect();
    out.sort_by_key(|r| std::cmp::Reverse(r.opened));
    out.dedup_by(|a, b| a.host.eq_ignore_ascii_case(&b.host));
    out
}

pub fn serialize_recent(list: &[Recent]) -> String {
    let mut s = String::from("# sentier recent machines: host, opened, last seen (Unix seconds)\n");
    for r in list {
        s.push_str(&format!(
            "{}\t{}\t{}\n",
            r.host,
            secs(r.opened),
            r.seen.map_or(0, secs)
        ));
    }
    s
}

/// Records `host` as opened now (moving it to the front).
pub fn touch(list: &mut Vec<Recent>, host: &str, now: SystemTime) {
    let seen = list
        .iter()
        .find(|r| r.host.eq_ignore_ascii_case(host))
        .and_then(|r| r.seen);
    list.retain(|r| !r.host.eq_ignore_ascii_case(host));
    list.insert(
        0,
        Recent {
            host: host.to_string(),
            opened: now,
            seen,
        },
    );
}

/// `%LOCALAPPDATA%\sentier\machines.tsv`.
pub fn recent_path() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(base).join("sentier").join(RECENT_FILE))
}

/// Blocking: the worker calls these.
pub fn load_recent(path: &Path, days: u32) -> Result<Vec<Recent>, String> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(parse_recent(&t, SystemTime::now(), days)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("machines.tsv: {e}")),
    }
}

pub fn save_recent(path: &Path, list: &[Recent]) -> Result<(), String> {
    crate::fs::write_atomic(path, &serialize_recent(list)).map_err(|e| format!("machines.tsv: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_expand() {
        let t = default_templates();
        assert_eq!(
            t[0].expand("JDOE-LT042", Some("jdoe")).as_deref(),
            Some(r"\\JDOE-LT042\C$\Users\jdoe\Desktop")
        );
        assert_eq!(t[0].expand("JDOE-LT042", None), None, "needs a user");
        assert_eq!(
            t[4].expand("PC1", None).as_deref(),
            Some(r"\\PC1\C$\ProgramData"),
            "no user needed"
        );
        assert_eq!(t[0].key_char(), Some('d'));
        assert_eq!(t[5].key_char(), None);
        let odd = PathTemplate::new("x", r"/C$/logs/{host}", None);
        assert_eq!(
            odd.expand("PC1", None).as_deref(),
            Some(r"\\PC1\C$\logs\PC1")
        );
    }

    #[test]
    fn likely_user_skips_system_and_admin_profiles() {
        let t = |s: u64| Some(UNIX_EPOCH + Duration::from_secs(s));
        let profiles = vec![
            ("Public".to_string(), t(900)),
            ("Default".to_string(), t(999)),
            ("adm-user".to_string(), t(998)),
            ("Administrator".to_string(), t(997)),
            ("asmith".to_string(), t(100)),
            ("jdoe".to_string(), t(500)),
        ];
        assert_eq!(likely_user(&profiles).as_deref(), Some("jdoe"));
        assert_eq!(likely_user(&[]), None);
        assert_eq!(
            people(&[
                "jdoe".into(),
                "JDOE".into(),
                "PC1$".into(),
                "".into(),
                "asmith".into()
            ]),
            ["jdoe", "asmith"]
        );
    }

    #[test]
    fn recent_round_trip_prune_and_touch() {
        let now = UNIX_EPOCH + Duration::from_secs(100 * 86_400);
        let day = |d: u64| UNIX_EPOCH + Duration::from_secs(d * 86_400);
        let text = "# comment\nPC-OLD\t1\t0\nPC-A\t8600000\t8600000\nPC-B\t8640000\t0\nbad line\nPC-a\t100\t0\n";
        let list = parse_recent(text, now, 30);
        let hosts: Vec<&str> = list.iter().map(|r| r.host.as_str()).collect();
        assert_eq!(
            hosts,
            ["PC-B", "PC-A"],
            "pruned, newest first, deduplicated"
        );
        assert_eq!(
            list[1].seen,
            Some(UNIX_EPOCH + Duration::from_secs(8_600_000))
        );
        assert_eq!(list[0].seen, None);
        assert_eq!(parse_recent(&serialize_recent(&list), now, 30), list);

        let mut l = list;
        touch(&mut l, "pc-a", day(100));
        assert_eq!(l[0].host, "pc-a");
        assert_eq!(l.len(), 2);
        assert!(l[0].seen.is_some(), "keeps when it was last seen");
    }

    #[test]
    fn ago_table() {
        let cases = [
            (5, "now"),
            (125, "2 min"),
            (7200, "2 h"),
            (3 * 86_400, "3 d"),
        ];
        for (s, want) in cases {
            assert_eq!(ago(Duration::from_secs(s)), want);
        }
    }
}
