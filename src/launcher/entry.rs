//! wtm's `[[launcher]]` entries (`%APPDATA%\wtm\config.toml`, schema in wtm's
//! `src/config.rs`) turned into pins (CLAUDE.md section 6, "Import from wtm"):
//!
//! - `args = "--app=URL"` on a browser exe → `kind = "web"` with `url` and `browser`;
//! - a `command` that is a folder → `folder`;
//! - anything else → `app` (`command`, `args`, `dir`);
//! - `group` → the first tag (lowercased, spaces → `-`); wtm's global `key` is dropped
//!   (in sentier `key` is the `'` jump key).
//!
//! wtm's frecency (`%LOCALAPPDATA%\wtm\usage.tsv`) uses the same format and, for apps,
//! the same key; a web entry's key (`msedge.exe --app=url`) becomes its URL.
//!
//! Pure: whether a command is a folder is asked through `is_dir`.

use crate::frecency::Usage;
use crate::pins::{Pin, PinKind, normalize_tag};
use serde::Deserialize;

/// One `[[launcher]]` entry. Unknown fields are ignored (wtm itself is strict).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct WtmEntry {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: String,
    #[serde(default)]
    pub dir: String,
    #[serde(default)]
    pub group: String,
}

#[derive(Deserialize)]
struct WtmConfig {
    #[serde(default)]
    launcher: Vec<toml::Value>,
}

/// The entries of a wtm config; a broken entry is skipped (and reported), the rest load.
pub fn parse_config(text: &str) -> Result<(Vec<WtmEntry>, Vec<String>), String> {
    let cfg: WtmConfig = toml::from_str(text).map_err(|e| format!("wtm config.toml: {e}"))?;
    let mut out = Vec::new();
    let mut problems = Vec::new();
    for (i, v) in cfg.launcher.into_iter().enumerate() {
        match v.try_into::<WtmEntry>() {
            Ok(e) if !e.name.trim().is_empty() && !e.command.trim().is_empty() => out.push(e),
            Ok(_) => problems.push(format!("wtm entry {}: no name or command", i + 1)),
            Err(e) => problems.push(format!("wtm entry {}: {e}", i + 1)),
        }
    }
    Ok((out, problems))
}

/// `msedge.exe` / `C:\…\chrome.exe` → `edge` / `chrome`; another program: its path.
fn browser_of(command: &str) -> Option<String> {
    let file = command
        .trim_matches('"')
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(command)
        .to_ascii_lowercase();
    let name = match file.trim_end_matches(".exe") {
        "msedge" => "edge",
        "chrome" => "chrome",
        "brave" => "brave",
        "vivaldi" => "vivaldi",
        _ => return None,
    };
    Some(name.to_string())
}

/// `--app=https://x` (quoted or not) → `https://x`; other args → None.
fn app_url(args: &str) -> Option<String> {
    let rest = args.trim().strip_prefix("--app=")?;
    let url = rest.trim().trim_matches('"').split_whitespace().next()?;
    (!url.is_empty()).then(|| url.to_string())
}

/// A group name as a tag: `My Apps` → `my-apps`. None when nothing usable is left.
fn group_tag(group: &str) -> Option<String> {
    let t: String = group
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_whitespace() { '-' } else { c })
        .filter(|c| c.is_ascii_alphanumeric() || "-_.".contains(*c))
        .collect();
    normalize_tag(&t).ok()
}

/// The pin for one wtm entry.
pub fn to_pin(e: &WtmEntry, is_dir: bool) -> Pin {
    let tags: Vec<String> = group_tag(&e.group).into_iter().collect();
    let mut pin = match (browser_of(&e.command), app_url(&e.args)) {
        (Some(browser), Some(url)) => {
            let mut p = Pin::blank(PinKind::Web, e.name.trim());
            p.url = Some(url);
            p.browser = (browser != "edge").then_some(browser);
            p
        }
        _ if is_dir => {
            let mut p = Pin::blank(PinKind::Folder, e.name.trim());
            p.path = Some(e.command.trim().to_string());
            p
        }
        _ => {
            let mut p = Pin::blank(PinKind::App, e.name.trim());
            p.command = Some(e.command.trim().to_string());
            p.args = (!e.args.trim().is_empty()).then(|| e.args.trim().to_string());
            p.dir = (!e.dir.trim().is_empty()).then(|| e.dir.trim().to_string());
            p
        }
    };
    pin.tags = tags;
    pin
}

/// wtm's key for an entry (`command args`, lowercase), as in its `usage_key`.
fn wtm_key(e: &WtmEntry) -> String {
    format!("{} {}", e.command, e.args).trim().to_lowercase()
}

/// wtm's usage, re-keyed for the pins the entries became (identical for apps).
pub fn convert_usage(wtm: &Usage, entries: &[(WtmEntry, Pin)]) -> Usage {
    let mut out = Usage::default();
    for (e, pin) in entries {
        if let Some(key) = crate::frecency::pin_key(pin) {
            out.take_from(wtm, &wtm_key(e), &key);
        }
    }
    out
}

/// What importing would add: the entries not already pinned (same frecency key, or
/// same kind and name).
pub fn new_pins(entries: Vec<(WtmEntry, Pin)>, existing: &[Pin]) -> Vec<(WtmEntry, Pin)> {
    let have: Vec<(Option<String>, PinKind, String)> = existing
        .iter()
        .map(|p| (crate::frecency::pin_key(p), p.kind, p.name.to_lowercase()))
        .collect();
    entries
        .into_iter()
        .filter(|(_, p)| {
            let key = crate::frecency::pin_key(p);
            !have.iter().any(|(k, kind, name)| {
                (key.is_some() && *k == key) || (*kind == p.kind && *name == p.name.to_lowercase())
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const WTM: &str = r#"
        outer_gap = 8
        [[launcher]]
        name = "E3D — HIT (local)"
        command = 'C:\AVEVA\evars.bat'
        args = "HIT"
        dir = 'D:\aveva\HIT'
        group = "AVEVA"
        key = "win+e"

        [[launcher]]
        name = "Grafana"
        command = 'C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe'
        args = '--app="https://grafana.example/d/x"'
        group = "Web Apps"

        [[launcher]]
        name = "Forma"
        command = "chrome.exe"
        args = "--app=https://forma.example"

        [[launcher]]
        name = "Projects"
        command = '\\srv01\project'

        [[launcher]]
        name = ""
        command = "x"

        [[launcher]]
        command = "no name"
    "#;

    #[test]
    fn maps_entries_to_pins() {
        let Ok((entries, problems)) = parse_config(WTM) else {
            panic!()
        };
        assert_eq!(entries.len(), 4);
        assert_eq!(problems.len(), 2, "{problems:?}");
        let pins: Vec<Pin> = entries
            .iter()
            .map(|e| to_pin(e, e.command.starts_with(r"\\")))
            .collect();

        let e3d = &pins[0];
        assert_eq!(e3d.kind, PinKind::App);
        assert_eq!(e3d.command.as_deref(), Some(r"C:\AVEVA\evars.bat"));
        assert_eq!(
            (e3d.args.as_deref(), e3d.dir.as_deref()),
            (Some("HIT"), Some(r"D:\aveva\HIT"))
        );
        assert_eq!(e3d.tags, ["aveva"]);
        assert_eq!(e3d.key, None, "wtm's global hotkey is dropped");

        let g = &pins[1];
        assert_eq!(g.kind, PinKind::Web);
        assert_eq!(g.url.as_deref(), Some("https://grafana.example/d/x"));
        assert_eq!(g.browser, None, "edge is the default");
        assert_eq!(g.tags, ["web-apps"]);

        assert_eq!(pins[2].browser.as_deref(), Some("chrome"));
        assert!(pins[2].tags.is_empty());
        assert_eq!(pins[3].kind, PinKind::Folder);
        assert_eq!(pins[3].path.as_deref(), Some(r"\\srv01\project"));
        // Every pin loads back as valid.
        for p in &pins {
            let mut store = crate::pins::PinStore::default();
            store.pins.push(p.clone());
            let text = crate::pins::serialize(&store);
            let Ok(text) = text else { panic!() };
            assert!(
                crate::pins::parse(&text).is_ok_and(|(s, n)| s.pins.len() == 1 && n.is_empty())
            );
        }
    }

    #[test]
    fn usage_and_dedupe() {
        let Ok((entries, _)) = parse_config(WTM) else {
            panic!()
        };
        let pairs: Vec<(WtmEntry, Pin)> = entries
            .iter()
            .map(|e| (e.clone(), to_pin(e, false)))
            .collect();
        let wtm = Usage::parse(
            "7\t100\tc:\\aveva\\evars.bat hit\n\
             3\t50\tc:\\program files (x86)\\microsoft\\edge\\application\\msedge.exe --app=\"https://grafana.example/d/x\"\n",
        );
        let u = convert_usage(&wtm, &pairs);
        assert_eq!(u.score(r"c:\aveva\evars.bat hit", 100), 7 * 8);
        assert_eq!(
            u.score("https://grafana.example/d/x", 50),
            3 * 8,
            "re-keyed by URL"
        );

        // Already pinned (same key, or same kind + name): left out.
        let mut have = to_pin(&entries[0], false);
        have.name = "renamed".into();
        let mut same_name = Pin::blank(PinKind::Web, "grafana");
        same_name.url = Some("https://elsewhere".into());
        let left = new_pins(pairs, &[have, same_name]);
        let names: Vec<&str> = left.iter().map(|(_, p)| p.name.as_str()).collect();
        assert_eq!(names, ["Forma", "Projects"]);
    }
}
