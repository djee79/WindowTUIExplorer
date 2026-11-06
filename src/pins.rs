//! The one pin store (CLAUDE.md section 3): `%APPDATA%\sentier\pins.toml`.
//!
//! A pinned folder *is* a bookmark *is* a launcher entry; `kind` decides what opening it
//! does. Loading is lenient, because the file is also edited by hand:
//! - a `[[pin]]` that doesn't load (unknown kind, missing path…) is reported and kept
//!   **as written**, and goes back into the file on save, so a typo never loses a pin;
//! - fields this version doesn't use (sync/backup settings, `profile`…) round-trip in
//!   [`Pin::extra`], and other tables (`[[view]]`, `[tags.aveva]`) are written back as
//!   they were.
//!
//! Comments in a hand-edited file are not kept by a save (the `toml` crate drops them).
//!
//! Everything here is pure except [`load`] and [`save`], which block; only the worker
//! calls them.

use crate::fs::path::Loc;
use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};

const FILE_NAME: &str = "pins.toml";
const HEADER: &str = "# sentier pins: folders, files, apps, web apps, sync and backup presets.\n\
                      # Edited from inside sentier (m, M, the pin picker); hand edits load too.\n\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PinKind {
    Folder,
    File,
    App,
    Web,
    Sync,
    Backup,
    /// A user's PC reached through its admin shares (CLAUDE.md section 14): `host`,
    /// optional `user` hint and `auth` profile (default "admin") in the extra fields.
    Machine,
}

impl PinKind {
    pub fn label(self) -> &'static str {
        match self {
            PinKind::Folder => "folder",
            PinKind::File => "file",
            PinKind::App => "app",
            PinKind::Web => "web",
            PinKind::Sync => "sync",
            PinKind::Backup => "backup",
            PinKind::Machine => "machine",
        }
    }

    /// What `M` makes of the item under the cursor.
    pub fn for_item(name: &str, is_dir: bool) -> PinKind {
        if is_dir {
            return PinKind::Folder;
        }
        let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
        match ext.as_deref() {
            Some("exe" | "bat" | "cmd" | "lnk" | "com") => PinKind::App,
            _ => PinKind::File,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pin {
    pub kind: PinKind,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser: Option<String>,
    /// The `'` jump key: one letter or digit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Fields this version doesn't use, kept as they were.
    #[serde(flatten)]
    pub extra: toml::Table,
}

impl Pin {
    fn blank(kind: PinKind, name: &str) -> Pin {
        Pin {
            kind,
            name: name.to_string(),
            path: None,
            command: None,
            args: None,
            dir: None,
            url: None,
            browser: None,
            key: None,
            tags: Vec::new(),
            extra: toml::Table::new(),
        }
    }

    /// A pin for a folder or file the explorer is looking at. Executables become `app`
    /// pins that run in their own folder.
    pub fn for_loc(kind: PinKind, name: &str, loc: &Loc) -> Pin {
        let mut pin = Pin::blank(kind, name);
        let display = loc.display();
        match kind {
            PinKind::App => {
                pin.dir = loc
                    .parent()
                    .filter(|p| p.as_fs().is_some())
                    .map(|p| p.display());
                pin.command = Some(display);
            }
            _ => pin.path = Some(display),
        }
        pin
    }

    pub fn key_char(&self) -> Option<char> {
        single_char(self.key.as_deref()?)
    }

    /// Where a folder or file pin points (`~` is the home folder). None for other kinds
    /// and for paths that don't parse.
    pub fn target(&self) -> Option<Loc> {
        match self.kind {
            PinKind::Folder | PinKind::File => {
                Loc::parse(&expand_home(self.path.as_deref()?)?).ok()
            }
            PinKind::App => Loc::parse(&expand_home(self.command.as_deref()?)?).ok(),
            _ => None,
        }
    }

    /// The text shown next to the name: the path, command or URL.
    pub fn detail(&self) -> String {
        let s = match self.kind {
            PinKind::Folder | PinKind::File => self.path.as_deref(),
            PinKind::App => self.command.as_deref(),
            PinKind::Web => self.url.as_deref(),
            PinKind::Sync | PinKind::Backup => self.extra.get("source").and_then(|v| v.as_str()),
            PinKind::Machine => self.extra.get("host").and_then(|v| v.as_str()),
        };
        s.unwrap_or_default().to_string()
    }

    /// The first problem that keeps this pin from working, if any.
    fn missing(&self) -> Option<String> {
        let has = |k: &str| self.extra.get(k).and_then(|v| v.as_str()).is_some();
        let needs = match self.kind {
            PinKind::Folder | PinKind::File if self.path.is_none() => "a path",
            PinKind::Folder | PinKind::File if self.target().is_none() => "a valid path",
            PinKind::App if self.command.is_none() => "a command",
            PinKind::Web if self.url.is_none() => "a url",
            PinKind::Sync if !has("source") || !has("dest") => "source and dest",
            PinKind::Backup if !has("source") || !has("dest_dir") => "source and dest_dir",
            PinKind::Machine if !has("host") => "a host",
            _ => return None,
        };
        Some(format!("{} pin needs {needs}", self.kind.label()))
    }
}

/// `~` and `~\rest` → the home folder. Reads an environment variable, no I/O.
fn expand_home(text: &str) -> Option<String> {
    let text = text.trim();
    match text.strip_prefix('~') {
        Some("") => std::env::var("USERPROFILE").ok(),
        Some(rest) if rest.starts_with(['\\', '/']) => {
            Some(format!("{}{rest}", std::env::var("USERPROFILE").ok()?))
        }
        _ => Some(text.to_string()),
    }
}

/// A pin key: one ASCII letter or digit, like a vim mark.
pub fn valid_key(c: char) -> bool {
    c.is_ascii_alphanumeric()
}

/// `#Aveva` → `aveva`. Tags are lowercase `a-z 0-9 - _ .`, stored without the `#`.
pub fn normalize_tag(text: &str) -> Result<String, String> {
    let t = text.trim().trim_start_matches('#').to_lowercase();
    if t.is_empty() {
        return Err("empty tag".to_string());
    }
    if let Some(bad) = t
        .chars()
        .find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || "-_.".contains(*c)))
    {
        return Err(format!("tag {t:?}: {bad:?} isn't allowed (a-z 0-9 - _ .)"));
    }
    Ok(t)
}

/// `aveva, #acme servers` → `["aveva", "acme", "servers"]`, deduplicated, in order.
pub fn parse_tags(text: &str) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for word in text
        .split([',', ' ', '\t'])
        .filter(|w| !w.trim().is_empty())
    {
        let t = normalize_tag(word)?;
        if !out.contains(&t) {
            out.push(t);
        }
    }
    Ok(out)
}

/// A saved query (`:view save <name>`): `#aveva on:srv01`. Opening it opens the
/// pin picker with that query. Shares the `'` key space with pins.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct View {
    pub name: String,
    pub query: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(flatten)]
    pub extra: toml::Table,
}

impl View {
    pub fn new(name: &str, query: &str) -> View {
        View {
            name: name.to_string(),
            query: query.to_string(),
            key: None,
            extra: toml::Table::new(),
        }
    }

    pub fn key_char(&self) -> Option<char> {
        single_char(self.key.as_deref()?)
    }
}

fn single_char(s: &str) -> Option<char> {
    let mut chars = s.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => Some(c),
        _ => None,
    }
}

/// Who holds a `'` key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyOwner {
    Pin(usize),
    View(usize),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PinStore {
    pub pins: Vec<Pin>,
    pub views: Vec<View>,
    /// `[[pin]]` / `[[view]]` tables that didn't load, written back unchanged.
    broken: Vec<toml::Value>,
    broken_views: Vec<toml::Value>,
    /// Everything else in the file (`[tags.*]`, …), written back unchanged.
    rest: toml::Table,
}

impl PinStore {
    pub fn by_key(&self, c: char) -> Option<&Pin> {
        self.pins.iter().find(|p| p.key_char() == Some(c))
    }

    /// The pin or view holding key `c`.
    pub fn key_owner(&self, c: char) -> Option<KeyOwner> {
        if let Some(i) = self.pins.iter().position(|p| p.key_char() == Some(c)) {
            return Some(KeyOwner::Pin(i));
        }
        self.views
            .iter()
            .position(|v| v.key_char() == Some(c))
            .map(KeyOwner::View)
    }

    /// Takes key `c` away from everyone but `keep`; returns the name it was taken from.
    fn release_key(&mut self, c: char, keep: KeyOwner) -> Option<String> {
        let mut previous = None;
        for (i, p) in self.pins.iter_mut().enumerate() {
            if keep != KeyOwner::Pin(i) && p.key_char() == Some(c) {
                p.key = None;
                previous = Some(p.name.clone());
            }
        }
        for (i, v) in self.views.iter_mut().enumerate() {
            if keep != KeyOwner::View(i) && v.key_char() == Some(c) {
                v.key = None;
                previous = Some(v.name.clone());
            }
        }
        previous
    }

    /// Gives view `idx` the key `c` (taken from whoever had it). Returns that name.
    pub fn set_view_key(&mut self, idx: usize, c: Option<char>) -> Option<String> {
        let previous = c.and_then(|c| self.release_key(c, KeyOwner::View(idx)));
        if let Some(v) = self.views.get_mut(idx) {
            v.key = c.map(String::from);
        }
        previous
    }

    /// Saves `view`, replacing a view of the same name (case-insensitive). Its index.
    pub fn save_view(&mut self, view: View) -> usize {
        match self
            .views
            .iter()
            .position(|v| v.name.eq_ignore_ascii_case(&view.name))
        {
            Some(i) => {
                let key = self.views[i].key.take();
                self.views[i] = View { key, ..view };
                i
            }
            None => {
                self.views.push(view);
                self.views.len() - 1
            }
        }
    }

    /// The number of pins tagged exactly `tag`.
    pub fn tag_count(&self, tag: &str) -> usize {
        self.pins
            .iter()
            .filter(|p| p.tags.iter().any(|t| t == tag))
            .count()
    }

    /// `:tag rename`: `old` becomes `new` on every pin (merged where both are there),
    /// and its `[tags.old]` settings move along. Returns the pins changed.
    pub fn rename_tag(&mut self, old: &str, new: &str) -> usize {
        let mut changed = 0;
        for p in &mut self.pins {
            let Some(i) = p.tags.iter().position(|t| t == old) else {
                continue;
            };
            changed += 1;
            if p.tags.iter().any(|t| t == new) {
                p.tags.remove(i);
            } else {
                p.tags[i] = new.to_string();
            }
        }
        if let Some(toml::Value::Table(tags)) = self.rest.get_mut("tags")
            && let Some(settings) = tags.remove(old)
        {
            tags.entry(new.to_string()).or_insert(settings);
        }
        changed
    }

    /// `:tag delete`: removes `tag` from every pin, and its settings. Returns the pins
    /// changed.
    pub fn delete_tag(&mut self, tag: &str) -> usize {
        let mut changed = 0;
        for p in &mut self.pins {
            let before = p.tags.len();
            p.tags.retain(|t| t != tag);
            changed += usize::from(p.tags.len() != before);
        }
        if let Some(toml::Value::Table(tags)) = self.rest.get_mut("tags") {
            tags.remove(tag);
        }
        changed
    }

    /// The pin of this kind that points at `loc`.
    pub fn find(&self, kind: PinKind, loc: &Loc) -> Option<usize> {
        self.pins
            .iter()
            .position(|p| p.kind == kind && p.target().as_ref() == Some(loc))
    }

    /// Gives pin `idx` the key `c`, taking it from whichever pin had it (vim marks are
    /// overwritten the same way). Returns that pin's name.
    pub fn set_key(&mut self, idx: usize, c: Option<char>) -> Option<String> {
        let previous = c.and_then(|c| self.release_key(c, KeyOwner::Pin(idx)));
        if let Some(p) = self.pins.get_mut(idx) {
            p.key = c.map(String::from);
        }
        previous
    }

    /// Every tag with the number of pins carrying it, most used first.
    pub fn tag_counts(&self) -> Vec<(String, usize)> {
        let mut counts: Vec<(String, usize)> = Vec::new();
        for t in self.pins.iter().flat_map(|p| &p.tags) {
            match counts.iter_mut().find(|(n, _)| n == t) {
                Some((_, c)) => *c += 1,
                None => counts.push((t.clone(), 1)),
            }
        }
        counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        counts
    }

    /// Folder pins with where they point, in file order.
    pub fn folders(&self) -> impl Iterator<Item = (&Pin, Loc)> {
        self.pins
            .iter()
            .filter(|p| p.kind == PinKind::Folder)
            .filter_map(|p| Some((p, p.target()?)))
    }
}

/// Parses the store. Err only when the file isn't TOML at all; problems with single
/// pins come back as messages next to a store that still holds them.
pub fn parse(text: &str) -> Result<(PinStore, Vec<String>), String> {
    let mut rest: toml::Table = toml::from_str(text).map_err(|e| format!("pins.toml: {e}"))?;
    let mut store = PinStore::default();
    let mut problems = Vec::new();
    match rest.remove("pin") {
        None => {}
        Some(toml::Value::Array(values)) => {
            for value in values {
                match load_pin(&value, &store) {
                    Ok((pin, mut notes)) => {
                        problems.append(&mut notes);
                        store.pins.push(pin);
                    }
                    Err(e) => {
                        problems.push(e);
                        store.broken.push(value);
                    }
                }
            }
        }
        Some(other) => {
            problems.push("pins.toml: `pin` must be [[pin]] tables".to_string());
            rest.insert("pin".to_string(), other);
        }
    }
    match rest.remove("view") {
        None => {}
        Some(toml::Value::Array(values)) => {
            for value in values {
                match load_view(&value, &store) {
                    Ok((view, mut notes)) => {
                        problems.append(&mut notes);
                        store.views.push(view);
                    }
                    Err(e) => {
                        problems.push(e);
                        store.broken_views.push(value);
                    }
                }
            }
        }
        Some(other) => {
            problems.push("pins.toml: `view` must be [[view]] tables".to_string());
            rest.insert("view".to_string(), other);
        }
    }
    store.rest = rest;
    Ok((store, problems))
}

fn load_view(value: &toml::Value, so_far: &PinStore) -> Result<(View, Vec<String>), String> {
    let label = value.get("name").and_then(|v| v.as_str()).map_or_else(
        || "a view with no name".to_string(),
        |n| format!("view {n:?}"),
    );
    let mut view: View = value
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| format!("pins.toml: {label}: {}", e.message()))?;
    view.name = view.name.trim().to_string();
    if view.name.is_empty() {
        return Err(format!("pins.toml: {label}: empty name"));
    }
    let mut notes = Vec::new();
    if view.key.is_some() {
        match view.key_char().filter(|c| valid_key(*c)) {
            Some(c) if so_far.key_owner(c).is_some() => {
                notes.push(format!(
                    "pins.toml: {label}: key '{c} is already used, dropped"
                ));
                view.key = None;
            }
            Some(_) => {}
            None => {
                notes.push(format!(
                    "pins.toml: {label}: key must be one letter or digit, dropped"
                ));
                view.key = None;
            }
        }
    }
    Ok((view, notes))
}

/// One `[[pin]]`: Err keeps it out of use (it's written back as is); the notes are
/// fixes made on load (a bad tag or key dropped).
fn load_pin(value: &toml::Value, so_far: &PinStore) -> Result<(Pin, Vec<String>), String> {
    let label = value.get("name").and_then(|v| v.as_str()).map_or_else(
        || "a pin with no name".to_string(),
        |n| format!("pin {n:?}"),
    );
    let mut pin: Pin = value
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| format!("pins.toml: {label}: {}", e.message()))?;
    pin.name = pin.name.trim().to_string();
    if pin.name.is_empty() {
        return Err(format!("pins.toml: {label}: empty name"));
    }
    if let Some(why) = pin.missing() {
        return Err(format!("pins.toml: {label}: {why}"));
    }
    let mut notes = Vec::new();
    let mut tags = Vec::new();
    for t in std::mem::take(&mut pin.tags) {
        match normalize_tag(&t) {
            Ok(t) if !tags.contains(&t) => tags.push(t),
            Ok(_) => {}
            Err(e) => notes.push(format!("pins.toml: {label}: {e}, dropped")),
        }
    }
    pin.tags = tags;
    if pin.key.is_some() {
        match pin.key_char().filter(|c| valid_key(*c)) {
            None => {
                notes.push(format!(
                    "pins.toml: {label}: key must be one letter or digit, dropped"
                ));
                pin.key = None;
            }
            Some(c) if so_far.by_key(c).is_some() => {
                notes.push(format!(
                    "pins.toml: {label}: key '{c} is already used, dropped"
                ));
                pin.key = None;
            }
            Some(_) => {}
        }
    }
    Ok((pin, notes))
}

pub fn serialize(store: &PinStore) -> Result<String, String> {
    let mut table = store.rest.clone();
    let mut pins = Vec::with_capacity(store.pins.len() + store.broken.len());
    for pin in &store.pins {
        pins.push(toml::Value::try_from(pin).map_err(|e| e.to_string())?);
    }
    pins.extend(store.broken.iter().cloned());
    if !pins.is_empty() {
        table.insert("pin".to_string(), toml::Value::Array(pins));
    }
    let mut views = Vec::with_capacity(store.views.len() + store.broken_views.len());
    for view in &store.views {
        views.push(toml::Value::try_from(view).map_err(|e| e.to_string())?);
    }
    views.extend(store.broken_views.iter().cloned());
    if !views.is_empty() {
        table.insert("view".to_string(), toml::Value::Array(views));
    }
    let body = toml::to_string(&table).map_err(|e| e.to_string())?;
    Ok(format!("{HEADER}{body}"))
}

/// `%APPDATA%\sentier\pins.toml`, or None when APPDATA isn't set.
pub fn store_path() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")?;
    Some(PathBuf::from(base).join("sentier").join(FILE_NAME))
}

/// Reads the store. A missing file is an empty store.
pub fn load(path: &Path) -> Result<(PinStore, Vec<String>), String> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok((PinStore::default(), Vec::new())),
        Err(e) => Err(format!("pins.toml: {e}")),
    }
}

pub fn save(path: &Path, store: &PinStore) -> Result<(), String> {
    let text = serialize(store)?;
    crate::fs::write_atomic(path, &text).map_err(|e| format!("pins.toml: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r##"
[[pin]]
kind  = "folder"
name  = "AVEVA projects"
path  = '\\srv01\aveva\projects'
key   = "a"
tags  = ["aveva", "ACME", "#servers"]

[[pin]]
kind    = "web"
name    = "grafana"
url     = "https://grafana.mycorp.com"
browser = "edge"
profile = "Work"

[[pin]]
kind    = "sync"
name    = "HIT → local"
source  = '\\srv01\project\HIT'
dest    = 'D:\aveva\local\HIT'
mode    = "mirror"
exclude_dirs = ["temp"]

[[pin]]
kind = "folder"
name = "no path here"

[[pin]]
kind = "rocket"
name = "unknown kind"

[[view]]
name  = "main server"
query = "#aveva on:srv01"

[tags.aveva]
color = "#7fa6f5"
"##;

    fn sample() -> (PinStore, Vec<String>) {
        match parse(SAMPLE) {
            Ok(r) => r,
            Err(e) => panic!("{e}"),
        }
    }

    #[test]
    fn parse_keeps_good_pins_and_reports_broken_ones() {
        let (store, problems) = sample();
        let names: Vec<&str> = store.pins.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["AVEVA projects", "grafana", "HIT → local"]);
        assert_eq!(store.broken.len(), 2);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].contains("no path here") && problems[0].contains("needs a path"));
        assert!(problems[1].contains("unknown kind"));

        let a = &store.pins[0];
        assert_eq!(a.tags, ["aveva", "acme", "servers"], "normalized");
        assert_eq!(a.key_char(), Some('a'));
        assert_eq!(
            a.target().map(|l| l.display()).as_deref(),
            Some(r"\\srv01\aveva\projects")
        );
        assert_eq!(
            store.by_key('a').map(|p| p.name.as_str()),
            Some("AVEVA projects")
        );
        assert_eq!(
            store.pins[1].extra.get("profile").and_then(|v| v.as_str()),
            Some("Work")
        );
        assert_eq!(store.pins[2].detail(), r"\\srv01\project\HIT");
    }

    #[test]
    fn save_round_trips_everything() {
        let (store, _) = sample();
        let text = match serialize(&store) {
            Ok(t) => t,
            Err(e) => panic!("{e}"),
        };
        assert!(text.starts_with('#'), "header");
        let Ok((again, problems)) = parse(&text) else {
            panic!("{text}")
        };
        assert_eq!(again, store, "{text}");
        assert_eq!(problems.len(), 2, "broken pins are still there");
        assert!(text.contains("exclude_dirs"), "unknown fields kept");
        assert!(text.contains("[[view]]") && text.contains("[tags.aveva]"));
    }

    #[test]
    fn bad_keys_and_tags_are_dropped_not_fatal() {
        let text = r#"
[[pin]]
kind = "folder"
name = "one"
path = 'C:\one'
key  = "x"
[[pin]]
kind = "folder"
name = "two"
path = 'C:\two'
key  = "x"
tags = ["ok", "not ok!"]
[[pin]]
kind = "folder"
name = "three"
path = 'C:\three'
key  = "xy"
"#;
        let Ok((store, problems)) = parse(text) else {
            panic!()
        };
        assert_eq!(store.pins.len(), 3);
        assert_eq!(store.pins[0].key_char(), Some('x'));
        assert_eq!(store.pins[1].key, None, "duplicate key");
        assert_eq!(store.pins[1].tags, ["ok"]);
        assert_eq!(store.pins[2].key, None, "two letters");
        assert_eq!(problems.len(), 3, "{problems:?}");
    }

    #[test]
    fn not_toml_is_an_error() {
        assert!(parse("[[pin]\nkind=").is_err());
        assert!(parse("").is_ok_and(|(s, p)| s.pins.is_empty() && p.is_empty()));
    }

    #[test]
    fn tags_table() {
        let ok: &[(&str, &str)] = &[
            ("aveva", "aveva"),
            ("#AVEVA", "aveva"),
            (" e3d.v3 ", "e3d.v3"),
        ];
        for (input, want) in ok {
            assert_eq!(normalize_tag(input).as_deref(), Ok(*want), "{input}");
        }
        for bad in ["", "#", "a b", "été", "a/b"] {
            assert!(normalize_tag(bad).is_err(), "{bad}");
        }
        assert_eq!(
            parse_tags("aveva, #acme  servers,aveva"),
            Ok(vec!["aveva".into(), "acme".into(), "servers".into()])
        );
        assert_eq!(parse_tags("  "), Ok(vec![]));
    }

    #[test]
    fn set_key_moves_it_like_a_vim_mark() {
        let (mut store, _) = sample();
        let moved = store.set_key(1, Some('a'));
        assert_eq!(moved.as_deref(), Some("AVEVA projects"));
        assert_eq!(store.by_key('a').map(|p| p.name.as_str()), Some("grafana"));
        assert_eq!(store.pins[0].key, None);
        assert_eq!(store.set_key(1, None), None);
        assert!(store.by_key('a').is_none());
    }

    #[test]
    fn kinds_and_targets() {
        let cases: &[(&str, bool, PinKind)] = &[
            ("HIT", true, PinKind::Folder),
            ("evars.bat", false, PinKind::App),
            ("E3D.LNK", false, PinKind::App),
            ("tool.exe", false, PinKind::App),
            ("spec.xlsx", false, PinKind::File),
            ("README", false, PinKind::File),
        ];
        for (name, dir, want) in cases {
            assert_eq!(PinKind::for_item(name, *dir), *want, "{name}");
        }
        let Ok(loc) = Loc::parse(r"\\srv\STD$\tools\run.bat") else {
            panic!()
        };
        let app = Pin::for_loc(PinKind::App, "run", &loc);
        assert_eq!(app.command.as_deref(), Some(r"\\srv\STD$\tools\run.bat"));
        assert_eq!(app.dir.as_deref(), Some(r"\\srv\STD$\tools"));
        assert_eq!(app.target(), Some(loc.clone()));
        let Ok(dir) = Loc::parse(r"\\srv\STD$") else {
            panic!()
        };
        let folder = Pin::for_loc(PinKind::Folder, "std", &dir);
        assert_eq!(folder.path.as_deref(), Some(r"\\srv\STD$"));

        let mut store = PinStore::default();
        store.pins.push(folder);
        assert_eq!(store.find(PinKind::Folder, &dir), Some(0));
        assert_eq!(store.find(PinKind::File, &dir), None);
        assert_eq!(store.folders().count(), 1);
    }

    #[test]
    fn tag_counts_most_used_first() {
        let (mut store, _) = sample();
        store.pins[1].tags = vec!["servers".into()];
        let counts = store.tag_counts();
        assert_eq!(counts[0], ("servers".to_string(), 2));
        assert_eq!(counts.len(), 3);
    }

    #[test]
    fn load_and_save_files() {
        let dir = std::env::temp_dir().join(format!("sentier-pins-{}", std::process::id()));
        let path = dir.join(FILE_NAME);
        assert!(
            load(&path).is_ok_and(|(s, _)| s.pins.is_empty()),
            "missing = empty"
        );
        let (store, _) = sample();
        assert!(save(&path, &store).is_ok());
        assert!(load(&path).is_ok_and(|(s, _)| s == store));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn views_load_save_and_share_the_key_space() {
        let (mut store, _) = sample();
        assert_eq!(store.views.len(), 1);
        assert_eq!(store.views[0].query, "#aveva on:srv01");

        // 'a belongs to a pin; giving it to the view takes it away.
        let moved = store.set_view_key(0, Some('a'));
        assert_eq!(moved.as_deref(), Some("AVEVA projects"));
        assert_eq!(store.key_owner('a'), Some(KeyOwner::View(0)));
        assert!(store.by_key('a').is_none());
        // And back.
        store.set_key(0, Some('a'));
        assert_eq!(store.key_owner('a'), Some(KeyOwner::Pin(0)));
        assert_eq!(store.views[0].key, None);

        // Saving under the same name replaces the query, keeps the key.
        store.set_view_key(0, Some('v'));
        let i = store.save_view(View::new("MAIN server", "#aveva is:unc"));
        assert_eq!((i, store.views.len()), (0, 1));
        assert_eq!(store.views[0].query, "#aveva is:unc");
        assert_eq!(store.views[0].key_char(), Some('v'));
        store.save_view(View::new("other", "#acme"));

        let Ok(text) = serialize(&store) else {
            panic!()
        };
        let Ok((again, _)) = parse(&text) else {
            panic!("{text}")
        };
        assert_eq!(again.views, store.views, "{text}");

        let dup = "[[pin]]\nkind='folder'\nname='p'\npath='C:\\p'\nkey='q'\n\
                   [[view]]\nname='v'\nquery='#x'\nkey='q'\n[[view]]\nquery='no name'\n";
        let Ok((s, problems)) = parse(dup) else {
            panic!()
        };
        assert_eq!(s.views.len(), 1);
        assert_eq!(s.views[0].key, None, "key already used by a pin");
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(
            serialize(&s).is_ok_and(|t| t.contains("no name")),
            "broken view kept"
        );
    }

    #[test]
    fn rename_and_delete_tags_across_pins() {
        let (mut store, _) = sample();
        store.pins[1].tags = vec!["acme".into(), "aveva".into()];
        assert_eq!(store.tag_count("aveva"), 2);
        // Pin 1 has both: the old one goes, no duplicate.
        assert_eq!(store.rename_tag("aveva", "acme"), 2);
        assert_eq!(store.pins[0].tags, ["acme", "servers"]);
        assert_eq!(store.pins[1].tags, ["acme"]);
        let Ok(text) = serialize(&store) else {
            panic!()
        };
        assert!(
            text.contains("[tags.acme]") && !text.contains("[tags.aveva]"),
            "{text}"
        );

        assert_eq!(store.delete_tag("acme"), 2);
        assert_eq!(store.tag_count("acme"), 0);
        assert_eq!(store.pins[0].tags, ["servers"]);
        let Ok(text) = serialize(&store) else {
            panic!()
        };
        assert!(!text.contains("[tags.acme]"), "{text}");
        assert_eq!(store.rename_tag("nope", "x"), 0);
    }
}
