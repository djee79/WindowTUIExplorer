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

/// A machine from a source: pinned, recent, the inventory CSV or `source_cmd`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineRecord {
    pub host: String,
    pub user: Option<String>,
    /// Other columns / fields (`laptop`, `Piping`, `Windows 11`…): searchable, shown.
    pub details: Vec<String>,
    /// `pinned`, `recent`, `inventory`, `command`.
    pub source: &'static str,
}

const HOST_KEYS: &[&str] = &[
    "host",
    "hostname",
    "computer",
    "computername",
    "name",
    "machine",
    "cn",
    "dnshostname",
];
const USER_KEYS: &[&str] = &[
    "user",
    "username",
    "owner",
    "samaccountname",
    "managedby",
    "assigneduser",
    "lastlogonuser",
];

/// `CN=Jane Doe,OU=…` → `Jane Doe`; anything else as it is.
fn tidy_user(u: &str) -> String {
    let u = u.trim();
    match u.strip_prefix("CN=").or_else(|| u.strip_prefix("cn=")) {
        Some(rest) => rest.split(',').next().unwrap_or(rest).to_string(),
        None => u.to_string(),
    }
}

/// A DNS name's host part, so `jdoe-lt042.corp.example` and `JDOE-LT042` meet.
fn host_part(h: &str) -> &str {
    let h = h.trim();
    // An IPv4 address stays whole.
    if h.split('.').all(|p| p.parse::<u8>().is_ok()) {
        return h;
    }
    h.split('.').next().unwrap_or(h)
}

/// One CSV line into fields: commas, double-quoted fields with `""` escapes.
fn csv_fields(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, quoted) {
            ('"', true) if chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            ('"', _) => quoted = !quoted,
            (',' | ';', false) => out.push(std::mem::take(&mut cur).trim().to_string()),
            _ => cur.push(c),
        }
    }
    out.push(cur.trim().to_string());
    out
}

/// An inventory CSV (from PDQ, AD, a spreadsheet): a header row, then one machine a line.
/// The host column is the first named host/computer/name/…; the user column
/// user/owner/samaccountname/…; every other non-empty value is a detail.
pub fn parse_csv(text: &str) -> Vec<MachineRecord> {
    let mut lines = text
        .lines()
        .map(|l| l.trim_start_matches('\u{feff}'))
        .filter(|l| !l.trim().is_empty());
    let Some(header) = lines.next() else {
        return Vec::new();
    };
    let cols: Vec<String> = csv_fields(header)
        .iter()
        .map(|c| c.to_lowercase())
        .collect();
    let find = |keys: &[&str]| keys.iter().find_map(|k| cols.iter().position(|c| c == k));
    let Some(hi) = find(HOST_KEYS) else {
        return Vec::new();
    };
    let ui = find(USER_KEYS);
    lines
        .filter_map(|l| {
            let f = csv_fields(l);
            let host = host_part(f.get(hi)?).to_string();
            if host.is_empty() {
                return None;
            }
            let user = ui
                .and_then(|i| f.get(i))
                .map(|u| tidy_user(u))
                .filter(|u| !u.is_empty());
            let details = f
                .iter()
                .enumerate()
                .filter(|(i, v)| *i != hi && Some(*i) != ui && !v.is_empty())
                .map(|(_, v)| v.clone())
                .collect();
            Some(MachineRecord {
                host,
                user,
                details,
                source: "inventory",
            })
        })
        .collect()
}

/// `source_cmd`'s output: a JSON array of objects (host/user keys as for the CSV, any
/// case; other string fields are details) or of plain host names. Also accepted: an object
/// whose first array field holds them (`{"computers": […]}`).
pub fn parse_json(text: &str) -> Result<Vec<MachineRecord>, String> {
    use serde_json::Value;
    let v: Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .map_err(|e| format!("not JSON: {e}"))?;
    let items = match v {
        Value::Array(a) => a,
        Value::Object(o) => o
            .into_iter()
            .find_map(|(_, v)| match v {
                Value::Array(a) => Some(a),
                _ => None,
            })
            .ok_or_else(|| "no array of machines in the JSON".to_string())?,
        _ => return Err("expected a JSON array of machines".to_string()),
    };
    let text_of = |v: &Value| match v {
        Value::String(s) => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    };
    Ok(items
        .into_iter()
        .filter_map(|item| match item {
            Value::String(h) => Some(MachineRecord {
                host: host_part(&h).to_string(),
                user: None,
                details: Vec::new(),
                source: "command",
            }),
            Value::Object(o) => {
                let get = |keys: &[&str]| {
                    keys.iter().find_map(|k| {
                        o.iter()
                            .find(|(name, _)| name.eq_ignore_ascii_case(k))
                            .and_then(|(name, v)| Some((name.clone(), text_of(v)?)))
                    })
                };
                let (host_key, host) = get(HOST_KEYS)?;
                let user = get(USER_KEYS);
                let user_key = user.as_ref().map(|(k, _)| k.clone());
                let details = o
                    .iter()
                    .filter(|(k, _)| **k != host_key && Some(*k) != user_key.as_ref())
                    .filter_map(|(_, v)| text_of(v))
                    .filter(|s| !s.is_empty())
                    .collect();
                Some(MachineRecord {
                    host: host_part(&host).to_string(),
                    user: user.map(|(_, u)| tidy_user(&u)).filter(|u| !u.is_empty()),
                    details,
                    source: "command",
                })
            }
            _ => None,
        })
        .filter(|r| !r.host.is_empty())
        .collect())
}

/// All sources into one list, one row per host (case-insensitive), in order of first
/// appearance: the first user found wins, details are combined, the first source names it.
pub fn merge(lists: Vec<Vec<MachineRecord>>) -> Vec<MachineRecord> {
    let mut out: Vec<MachineRecord> = Vec::new();
    for r in lists.into_iter().flatten() {
        match out
            .iter_mut()
            .find(|o| o.host.eq_ignore_ascii_case(&r.host))
        {
            Some(o) => {
                if o.user.is_none() {
                    o.user = r.user;
                }
                for d in r.details {
                    if !o.details.contains(&d) {
                        o.details.push(d);
                    }
                }
            }
            None => out.push(r),
        }
    }
    out
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
/// A PC name as typed (`JDOE-LT042`, `\\JDOE-LT042`, `\\JDOE-LT042\C$\Users`, an IP, a
/// full DNS name) → the host. Names are letters, digits, `-`, `_` and `.`.
pub fn host_name(text: &str) -> Result<String, String> {
    let t = text.trim().trim_start_matches(['\\', '/']);
    let host = t.split(['\\', '/']).next().unwrap_or_default().trim();
    if host.is_empty() {
        return Err("a PC name, please".to_string());
    }
    if host.len() > 253
        || !host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(format!(
            "{host:?} isn't a PC name (letters, digits, - _ . only)"
        ));
    }
    Ok(host.to_string())
}

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
    if cfg!(test) {
        return Some(
            std::env::temp_dir()
                .join(format!("sentier-test-store-{}", std::process::id()))
                .join(RECENT_FILE),
        );
    }
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
    fn host_names_as_typed() {
        for (typed, want) in [
            ("JDOE-LT042", "JDOE-LT042"),
            ("  pc-007 ", "pc-007"),
            (r"\\JDOE-LT042", "JDOE-LT042"),
            (r"\\JDOE-LT042\C$\Users", "JDOE-LT042"),
            ("//pc-1/c$", "pc-1"),
            ("10.1.2.3", "10.1.2.3"),
            ("pc-9.corp.example", "pc-9.corp.example"),
        ] {
            assert_eq!(host_name(typed).as_deref(), Ok(want), "{typed}");
        }
        assert!(host_name("").is_err());
        assert!(host_name(r"\\").is_err());
        assert!(host_name("bad name").is_err_and(|e| e.contains("isn't a PC name")));
        assert!(host_name("pc#1").is_err());
    }

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

    #[test]
    fn inventory_csv_table() {
        let csv = "\u{feff}Name,Description,Owner,OS\n\
                   JDOE-LT042,Piping laptop,\"CN=Jane Doe,OU=Users,DC=corp\",Windows 11\n\
                   pc-007.corp.example,\"Desk 4, floor 2\",asmith,\n\
                   ,orphan row,nobody,\n";
        let r = parse_csv(csv);
        assert_eq!(r.len(), 2, "{r:?}");
        assert_eq!(r[0].host, "JDOE-LT042");
        assert_eq!(r[0].user.as_deref(), Some("Jane Doe"), "CN= tidied");
        assert_eq!(r[0].details, ["Piping laptop", "Windows 11"]);
        assert_eq!(r[1].host, "pc-007", "DNS name to host part");
        assert_eq!(r[1].details, ["Desk 4, floor 2"], "quoted comma kept");
        assert_eq!(r[1].source, "inventory");
        // Semicolons (Excel in many locales) and no user column.
        let r = parse_csv("computer;site\nPC-1;Site-A\n");
        assert_eq!((r[0].host.as_str(), r[0].user.as_deref()), ("PC-1", None));
        assert!(parse_csv("foo,bar\n1,2\n").is_empty(), "no host column");
        assert!(parse_csv("").is_empty());
    }

    #[test]
    fn command_json_table() {
        let objects = r#"[
            {"Name": "JDOE-LT042", "SamAccountName": "jdoe", "OperatingSystem": "Windows 11", "Enabled": true},
            {"dNSHostName": "pc-007.corp.example", "Description": "Desk 4"},
            {"nothing": "here"}
        ]"#;
        let Ok(r) = parse_json(objects) else { panic!() };
        assert_eq!(r.len(), 2);
        assert_eq!(
            (r[0].host.as_str(), r[0].user.as_deref()),
            ("JDOE-LT042", Some("jdoe"))
        );
        assert_eq!(r[0].details, ["Windows 11"], "bools skipped");
        assert_eq!(r[1].host, "pc-007");
        assert_eq!(r[1].source, "command");
        let Ok(names) = parse_json(r#"["PC-1", "PC-2.corp.example", "10.0.0.5"]"#) else {
            panic!()
        };
        let hosts: Vec<&str> = names.iter().map(|m| m.host.as_str()).collect();
        assert_eq!(hosts, ["PC-1", "PC-2", "10.0.0.5"], "an IP stays whole");
        let Ok(wrapped) = parse_json(r#"{"count": 1, "computers": [{"host": "PC-9"}]}"#) else {
            panic!()
        };
        assert_eq!(wrapped[0].host, "PC-9");
        assert!(parse_json("not json").is_err());
        assert!(parse_json("42").is_err());
    }

    #[test]
    fn merge_keeps_one_row_per_host() {
        let rec = |h: &str, u: Option<&str>, d: &[&str], s| MachineRecord {
            host: h.into(),
            user: u.map(str::to_string),
            details: d.iter().map(|x| x.to_string()).collect(),
            source: s,
        };
        let merged = merge(vec![
            vec![rec("PC-042", None, &[], "pinned")],
            vec![
                rec("pc-042", Some("jdoe"), &["laptop"], "inventory"),
                rec("PC-7", None, &[], "inventory"),
            ],
            vec![rec(
                "PC-042",
                Some("other"),
                &["laptop", "Piping"],
                "command",
            )],
        ]);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].host, "PC-042", "first spelling and source");
        assert_eq!(merged[0].source, "pinned");
        assert_eq!(merged[0].user.as_deref(), Some("jdoe"), "first user found");
        assert_eq!(merged[0].details, ["laptop", "Piping"]);
    }
}
