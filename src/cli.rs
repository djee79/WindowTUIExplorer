//! The command line for other tools (CLAUDE.md section 10), hyprctl-style, so scripts and
//! other TUIs (`getad`, `cadcommands`) can feed sentier:
//!
//! - `sentier pin <path|url> [--name N] [--tag T]… [--key K]`: adds a pin (a folder or
//!   file, an app for `.exe/.bat/.lnk`, a web app for a URL).
//! - `sentier pins [--query "#aveva is:unc"] [--json]`: lists pins, filtered like the
//!   pickers (`#tag`, `!#tag`, `is:`, `on:`, words).
//! - `sentier open <pin name>`: opens a pin as Enter would (folder / file in its Windows
//!   app, app, web app).
//!
//! pins.toml is written atomically; a running sentier notices and reloads it. The logic
//! here is pure; `main` reads and writes the files.

use crate::fs::path::Loc;
use crate::picker::query::{Facts, PinQuery};
use crate::pins::{Pin, PinKind, PinStore, normalize_tag, valid_key};
use std::io::Write;

pub const USAGE: &str = "\
usage:
  sentier pin <path|url> [--name N] [--tag T]... [--key K]
  sentier pins [--query \"#aveva is:unc\"] [--json]
  sentier open <pin name>
  sentier run <preset name> [--allow-mirror]      (sentier run --help)

  pin: a folder, a file (.exe/.bat/.lnk become apps) or a web address
  pins: filters as in the pickers: #tag  #a,b  !#tag  is:folder is:web is:unc  on:server
  open: by name (exact, case aside), as Enter would
";

/// What `sentier pin` asked for.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PinArgs {
    pub target: String,
    pub name: Option<String>,
    pub tags: Vec<String>,
    pub key: Option<String>,
}

pub fn parse_pin_args(args: &[String]) -> Result<PinArgs, String> {
    let mut out = PinArgs::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |flag: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match a.as_str() {
            "--name" => out.name = Some(value("--name")?),
            "--tag" => out.tags.push(normalize_tag(&value("--tag")?)?),
            "--key" => {
                let k = value("--key")?;
                if !valid_key(&k) {
                    return Err("--key is one or two letters or digits".into());
                }
                out.key = Some(k);
            }
            flag if flag.starts_with("--") => return Err(format!("unknown option {flag}")),
            t if out.target.is_empty() => out.target = t.to_string(),
            _ => return Err("one path or url, please (quote it if it has spaces)".into()),
        }
    }
    if out.target.is_empty() {
        return Err("what to pin? a path or a url".into());
    }
    Ok(out)
}

/// A path rather than a web address: `C:\…`, `\\…`, `.\…`, `~`, or anything with `\`.
fn looks_like_path(t: &str) -> bool {
    let b = t.as_bytes();
    t.contains('\\')
        || t.starts_with('.')
        || t.starts_with('~')
        || (b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic())
}

/// What kind of thing is at a path: Some(true) a folder, Some(false) a file, None
/// nothing (`main` asks the filesystem).
pub type Probe<'a> = &'a dyn Fn(&str) -> Option<bool>;

/// Adds the pin to `store`. Returns what to print.
pub fn add_pin(
    store: &mut PinStore,
    args: &PinArgs,
    cwd: &str,
    probe: Probe,
) -> Result<String, String> {
    let t = args.target.trim();
    // A path when it is one (resolved against the cwd), else a web address.
    let here = Loc::parse(cwd).map_err(|e| e.to_string())?;
    let as_path = (!t.contains("://"))
        .then(|| crate::pins::expand_home(t).unwrap_or_else(|| t.to_string()))
        .and_then(|text| Loc::resolve(&here, &text).ok())
        .and_then(|loc| {
            let path = loc.as_fs()?.display();
            Some((loc, probe(&path)))
        });
    let found = matches!(as_path, Some((_, Some(_))));
    let mut pin = if !found && !looks_like_path(t) {
        let url = crate::launcher::normalize_url(t)
            .ok_or_else(|| format!("{t}: not a path that exists, nor a web address"))?;
        let host = url
            .split("://")
            .nth(1)
            .and_then(|r| r.split(['/', '?', '#']).next())
            .unwrap_or_default()
            .trim_start_matches("www.")
            .to_string();
        let mut p = Pin::blank(PinKind::Web, &host);
        p.url = Some(url);
        p
    } else {
        let (loc, is_dir) = match as_path {
            Some((loc, Some(d))) => (loc, d),
            Some((loc, None)) => {
                let shown = loc.as_fs().map_or_else(|| t.to_string(), |p| p.display());
                return Err(format!("{shown}: not found"));
            }
            None => return Err(format!("{t}: not a folder or file path")),
        };
        let kind = PinKind::for_item(&loc.name(), is_dir);
        Pin::for_loc(kind, &loc.name(), &loc)
    };
    if let Some(n) = &args.name {
        pin.name = n.trim().to_string();
    }
    if pin.name.is_empty() {
        return Err("the pin needs a name (--name)".into());
    }
    pin.tags = args.tags.clone();
    let key = crate::frecency::pin_key(&pin);
    if let Some(same) = store
        .pins
        .iter()
        .find(|p| p.kind == pin.kind && key.is_some() && crate::frecency::pin_key(p) == key)
    {
        return Err(format!("already pinned as {:?}", same.name));
    }
    // No prompt here to move the other pin, as the TUI offers: refuse instead.
    if let Some(k) = &args.key
        && let Some(o) = store.key_clashes(k, None).into_iter().next()
    {
        return Err(format!(
            "'{k} clashes with '{} ({}): a letter is a key or starts keys, not both",
            store.owner_key(o).unwrap_or_default(),
            store.owner_name(o).unwrap_or_default()
        ));
    }
    let (name, kind) = (pin.name.clone(), pin.kind);
    store.pins.push(pin);
    let mut msg = format!("pinned {name} ({})", kind.label());
    if let Some(k) = &args.key {
        let idx = store.pins.len() - 1;
        if let Some(moved) = store.set_key(idx, Some(k)) {
            msg.push_str(&format!(" · '{k} moved from {moved}"));
        } else {
            msg.push_str(&format!(" · '{k}"));
        }
    }
    Ok(msg)
}

// ------------------------------------------------------------------ pins

/// The facts a query filters on, from the pin alone (no probes, no mapped drives).
fn facts(p: &Pin) -> Facts<'_> {
    let path = p
        .path
        .as_deref()
        .or(p.command.as_deref())
        .or_else(|| p.extra.get("source").and_then(|v| v.as_str()))
        .unwrap_or("");
    let unc = path.starts_with(r"\\");
    let server = if p.kind == PinKind::Machine {
        p.extra
            .get("host")
            .and_then(|v| v.as_str())
            .map(str::to_lowercase)
    } else if unc {
        path.trim_start_matches('\\')
            .split('\\')
            .next()
            .map(str::to_lowercase)
    } else {
        None
    };
    Facts {
        kind: Some(p.kind),
        view: false,
        machine: p.kind == PinKind::Machine,
        online: None,
        tags: &p.tags,
        unc,
        local: !unc && !path.is_empty(),
        unreachable: false,
        server,
    }
}

/// The pins matching `query`: its filters, and every word in name, target or tags.
pub fn matching<'a>(store: &'a PinStore, query: &str) -> Vec<&'a Pin> {
    let q = PinQuery::parse(query);
    let words: Vec<String> = q.text.split_whitespace().map(str::to_lowercase).collect();
    store
        .pins
        .iter()
        .filter(|p| q.matches(&facts(p)))
        .filter(|p| {
            let hay = format!("{} {} {}", p.name, p.detail(), p.tags.join(" ")).to_lowercase();
            words.iter().all(|w| hay.contains(w.as_str()))
        })
        .collect()
}

/// `--json`: the pins as they are in pins.toml (every field), as a JSON array.
pub fn to_json(pins: &[&Pin]) -> Result<String, String> {
    serde_json::to_string_pretty(pins).map_err(|e| e.to_string())
}

/// One line per pin: `folder  AVEVA projects  \\srv01\aveva\projects  'a #aveva`.
pub fn to_lines(pins: &[&Pin], out: &mut dyn Write) {
    for p in pins {
        let key = p.key_str().map(|k| format!(" '{k}")).unwrap_or_default();
        let tags: String = p.tags.iter().map(|t| format!(" #{t}")).collect();
        let _ = writeln!(
            out,
            "{:<8}{}  {}{key}{tags}",
            p.kind.label(),
            p.name,
            p.detail()
        );
    }
}

/// `sentier open <name>`: the pin with that name (exact, case aside).
pub fn find<'a>(store: &'a PinStore, name: &str) -> Option<&'a Pin> {
    let n = name.trim().to_lowercase();
    store.pins.iter().find(|p| p.name.to_lowercase() == n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn probe(path: &str) -> Option<bool> {
        match path {
            r"C:\work\HIT" | r"\\srv01\p" | r"C:\work\dl" | r"C:\work\hx" => Some(true),
            r"C:\work\a.pdf" | r"C:\work\tool.exe" => Some(false),
            _ => None,
        }
    }

    #[test]
    fn pin_paths_apps_and_urls() {
        let mut store = PinStore::default();
        let add = |store: &mut PinStore, a: &[&str]| -> Result<String, String> {
            let args = parse_pin_args(&s(a))?;
            add_pin(store, &args, r"C:\work", &probe)
        };
        assert_eq!(
            add(&mut store, &["HIT", "--tag", "#AVEVA", "--key", "h"]),
            Ok("pinned HIT (folder) · 'h".to_string())
        );
        assert_eq!(store.pins[0].path.as_deref(), Some(r"C:\work\HIT"));
        assert_eq!(store.pins[0].tags, ["aveva"]);
        assert!(add(&mut store, &[r"C:\work\tool.exe"]).is_ok_and(|m| m.contains("(app)")));
        assert!(
            add(&mut store, &[r".\a.pdf", "--name", "Spec"])
                .is_ok_and(|m| m.contains("Spec (file)"))
        );
        assert!(add(&mut store, &[r"\\srv01\p"]).is_ok());
        let web = add(&mut store, &["grafana.example/d/x", "--tag", "monitoring"]);
        assert_eq!(web, Ok("pinned grafana.example (web)".to_string()));
        assert_eq!(
            store.pins.last().and_then(|p| p.url.clone()).as_deref(),
            Some("https://grafana.example/d/x")
        );
        // Refusals.
        assert!(add(&mut store, &["HIT"]).is_err_and(|e| e.contains("already pinned as \"HIT\"")));
        assert!(add(&mut store, &[r"C:\nope"]).is_err_and(|e| e.contains("not found")));
        assert!(add(&mut store, &["just words"]).is_err());
        assert!(parse_pin_args(&s(&["x", "--key", "abc"])).is_err());
        // Two-letter keys; one that clashes with 'h (HIT) is refused.
        assert!(add(&mut store, &[r"C:\work\dl", "--key", "dl"]).is_ok_and(|m| m.ends_with("'dl")));
        assert!(
            add(&mut store, &[r"C:\work\hx", "--key", "hx"])
                .is_err_and(|e| e.contains("clashes with 'h (HIT)"))
        );
        assert!(parse_pin_args(&s(&["x", "--tag", "a b"])).is_err());
        assert!(parse_pin_args(&s(&[])).is_err());
        assert!(parse_pin_args(&s(&["x", "--colour"])).is_err());
    }

    #[test]
    fn pins_query_json_and_find() {
        let mut store = PinStore::default();
        for a in [
            &["HIT", "--tag", "aveva"][..],
            &[r"\\srv01\p", "--tag", "aveva", "--tag", "servers"][..],
            &["https://grafana.example", "--name", "Grafana"][..],
        ] {
            let args = parse_pin_args(&s(a)).unwrap_or_default();
            assert!(add_pin(&mut store, &args, r"C:\work", &probe).is_ok());
        }
        let names = |q: &str| -> Vec<String> {
            matching(&store, q).iter().map(|p| p.name.clone()).collect()
        };
        assert_eq!(names("#aveva"), ["HIT", "p"]);
        assert_eq!(names("#aveva is:unc"), ["p"]);
        assert_eq!(names("on:srv01"), ["p"]);
        assert_eq!(names("is:web"), ["Grafana"]);
        assert_eq!(names("!#aveva"), ["Grafana"]);
        assert_eq!(names("graf"), ["Grafana"]);
        assert_eq!(names("").len(), 3);
        let json = to_json(&matching(&store, "is:web")).unwrap_or_default();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap_or_default();
        assert_eq!(v[0]["kind"], "web");
        assert_eq!(v[0]["url"], "https://grafana.example");
        let mut out = Vec::new();
        to_lines(&matching(&store, "#servers"), &mut out);
        assert_eq!(
            String::from_utf8_lossy(&out).trim_end(),
            r"folder  p  \\srv01\p #aveva #servers"
        );
        assert_eq!(find(&store, "grafana").map(|p| p.kind), Some(PinKind::Web));
        assert!(find(&store, "graf").is_none(), "exact names only");
    }
}
