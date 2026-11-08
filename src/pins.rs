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
    /// The `'` jump key: one or two letters or digits (`'h`, `'dl`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Fields this version doesn't use, kept as they were.
    #[serde(flatten)]
    pub extra: toml::Table,
}

impl Pin {
    pub(crate) fn blank(kind: PinKind, name: &str) -> Pin {
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

    /// The `'` key, when it's a valid one.
    pub fn key_str(&self) -> Option<&str> {
        self.key.as_deref().filter(|k| valid_key(k))
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
        if self.kind == PinKind::Sync {
            // The whole preset: paths, mode, excludes, and source/dest never nested.
            return crate::jobs::preset::SyncPreset::from_pin(self)
                .err()
                .map(|e| format!("sync pin: {e}"));
        }
        if self.kind == PinKind::Backup {
            return crate::jobs::zip::BackupPreset::from_pin(self)
                .err()
                .map(|e| format!("backup pin: {e}"));
        }
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
pub(crate) fn expand_home(text: &str) -> Option<String> {
    let text = expand_vars(text.trim());
    let text = text.as_str();
    match text.strip_prefix('~') {
        Some("") => std::env::var("USERPROFILE").ok(),
        Some(rest) if rest.starts_with(['\\', '/']) => {
            Some(format!("{}{rest}", std::env::var("USERPROFILE").ok()?))
        }
        _ => Some(text.to_string()),
    }
}

/// `%APPDATA%\x` and PowerShell's `$env:APPDATA\x` → the variable's value (any case, as
/// Windows does). A name that isn't set is left as typed: `50% off` can be a real folder.
pub fn expand_vars(text: &str) -> String {
    expand_vars_with(text, &|name| std::env::var(name).ok())
}

/// [`expand_vars`] with the lookup given (tests).
pub fn expand_vars_with(text: &str, var: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let pct = rest.find('%');
        let ps = rest.to_ascii_lowercase().find("$env:");
        match (pct, ps) {
            // `$env:NAME`: the name runs to the first character that can't be in one.
            (_, Some(i)) if pct.is_none_or(|p| i < p) => {
                let after = &rest[i + 5..];
                let len = after
                    .find(|c: char| {
                        !(c.is_ascii_alphanumeric() || c == '_' || c == '(' || c == ')')
                    })
                    .unwrap_or(after.len());
                let name = &after[..len];
                match var(name).filter(|_| !name.is_empty()) {
                    Some(v) => {
                        out.push_str(&rest[..i]);
                        out.push_str(&v);
                    }
                    None => out.push_str(&rest[..i + 5 + len]),
                }
                rest = &after[len..];
            }
            (Some(i), _) => {
                let after = &rest[i + 1..];
                match after.find('%') {
                    Some(j) if j > 0 && !after[..j].contains(['\\', '/']) => {
                        let name = &after[..j];
                        match var(name) {
                            Some(v) => {
                                out.push_str(&rest[..i]);
                                out.push_str(&v);
                                rest = &after[j + 1..];
                            }
                            // Not a variable: keep the first `%` and look again after it.
                            None => {
                                out.push_str(&rest[..=i]);
                                rest = after;
                            }
                        }
                    }
                    // `%%` or `%a\b%`: this `%` isn't an opening one; look again after it.
                    Some(_) => {
                        out.push_str(&rest[..=i]);
                        rest = after;
                    }
                    None => {
                        out.push_str(rest);
                        return out;
                    }
                }
            }
            (None, _) => {
                out.push_str(rest);
                return out;
            }
        }
    }
}

/// The environment variables whose name starts with `prefix` (any case), sorted: what
/// `%APP` + Tab can become.
pub fn vars_starting(prefix: &str) -> Vec<String> {
    let p = prefix.to_ascii_lowercase();
    let mut v: Vec<String> = std::env::vars()
        .map(|(k, _)| k)
        .filter(|k| k.to_ascii_lowercase().starts_with(&p))
        .collect();
    v.sort_by_key(|k| k.to_ascii_lowercase());
    v.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    v
}

/// A pin key: one or two ASCII letters or digits (`'h`, `'dl`).
pub fn valid_key(k: &str) -> bool {
    (1..=2).contains(&k.len()) && k.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// A key prompt's answer: empty for none, else one or two letters or digits.
pub fn parse_key(text: &str) -> Result<Option<String>, String> {
    match text.trim() {
        "" => Ok(None),
        k if valid_key(k) => Ok(Some(k.to_string())),
        _ => Err("one or two letters or digits, or empty for none".to_string()),
    }
}

/// Two different keys where one starts the other (`d` and `dl`): after `'d` sentier
/// couldn't tell whether to jump or wait, so a letter is a key or starts keys, never
/// both.
pub fn keys_clash(a: &str, b: &str) -> bool {
    a != b && (a.starts_with(b) || b.starts_with(a))
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

    pub fn key_str(&self) -> Option<&str> {
        self.key.as_deref().filter(|k| valid_key(k))
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
    /// Every pin and view with a key, pins first, in file order.
    pub fn keyed(&self) -> impl Iterator<Item = (KeyOwner, &str)> {
        let pins = self.pins.iter().enumerate();
        let views = self.views.iter().enumerate();
        pins.filter_map(|(i, p)| Some((KeyOwner::Pin(i), p.key_str()?)))
            .chain(views.filter_map(|(i, v)| Some((KeyOwner::View(i), v.key_str()?))))
    }

    /// The pin or view holding key `k`.
    pub fn key_owner(&self, k: &str) -> Option<KeyOwner> {
        self.keyed().find(|(_, key)| *key == k).map(|(o, _)| o)
    }

    /// The pins and views (but `except`) whose key clashes with `k`: one starts the
    /// other (see [`keys_clash`]).
    pub fn key_clashes(&self, k: &str, except: Option<KeyOwner>) -> Vec<KeyOwner> {
        self.keyed()
            .filter(|(o, key)| Some(*o) != except && keys_clash(k, key))
            .map(|(o, _)| o)
            .collect()
    }

    /// True when some two-letter key starts with `c` (`'d` then waits for the second).
    pub fn starts_keys(&self, c: char) -> bool {
        self.keyed().any(|(_, k)| k.len() == 2 && k.starts_with(c))
    }

    pub fn owner_name(&self, o: KeyOwner) -> Option<&str> {
        match o {
            KeyOwner::Pin(i) => self.pins.get(i).map(|p| p.name.as_str()),
            KeyOwner::View(i) => self.views.get(i).map(|v| v.name.as_str()),
        }
    }

    pub fn owner_key(&self, o: KeyOwner) -> Option<&str> {
        match o {
            KeyOwner::Pin(i) => self.pins.get(i)?.key_str(),
            KeyOwner::View(i) => self.views.get(i)?.key_str(),
        }
    }

    /// Sets (or clears) one owner's key, nothing else.
    pub fn put_key(&mut self, o: KeyOwner, k: Option<&str>) {
        let slot = match o {
            KeyOwner::Pin(i) => self.pins.get_mut(i).map(|p| &mut p.key),
            KeyOwner::View(i) => self.views.get_mut(i).map(|v| &mut v.key),
        };
        if let Some(slot) = slot {
            *slot = k.map(str::to_string);
        }
    }

    /// Takes key `k` away from everyone but `keep`; returns the name it was taken from.
    fn release_key(&mut self, k: &str, keep: KeyOwner) -> Option<String> {
        let owners: Vec<KeyOwner> = self
            .keyed()
            .filter(|(o, key)| *o != keep && *key == k)
            .map(|(o, _)| o)
            .collect();
        let mut previous = None;
        for o in owners {
            previous = self.owner_name(o).map(str::to_string);
            self.put_key(o, None);
        }
        previous
    }

    /// Gives view `idx` the key `k` (taken from whoever had it). Returns that name.
    pub fn set_view_key(&mut self, idx: usize, k: Option<&str>) -> Option<String> {
        let previous = k.and_then(|k| self.release_key(k, KeyOwner::View(idx)));
        self.put_key(KeyOwner::View(idx), k);
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
    pub fn set_key(&mut self, idx: usize, k: Option<&str>) -> Option<String> {
        let previous = k.and_then(|k| self.release_key(k, KeyOwner::Pin(idx)));
        self.put_key(KeyOwner::Pin(idx), k);
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
    if let Some(note) = check_loaded_key(view.key.as_deref(), &label, so_far) {
        notes.push(note);
        view.key = None;
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
    if let Some(note) = check_loaded_key(pin.key.as_deref(), &label, so_far) {
        notes.push(note);
        pin.key = None;
    }
    Ok((pin, notes))
}

/// A hand-edited key that can't be used (malformed, taken, or clashing with an earlier
/// one): the note saying so; the key is then dropped.
fn check_loaded_key(key: Option<&str>, label: &str, so_far: &PinStore) -> Option<String> {
    let k = key?;
    if !valid_key(k) {
        return Some(format!(
            "pins.toml: {label}: key must be one or two letters or digits, dropped"
        ));
    }
    if so_far.key_owner(k).is_some() {
        return Some(format!(
            "pins.toml: {label}: key '{k} is already used, dropped"
        ));
    }
    let clash = so_far.key_clashes(k, None).into_iter().next()?;
    let other = so_far.owner_key(clash).unwrap_or_default();
    Some(format!(
        "pins.toml: {label}: key '{k} clashes with '{other} (one starts the other), dropped"
    ))
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
    // Tests never read or write the user's own file.
    if cfg!(test) {
        return Some(
            std::env::temp_dir()
                .join(format!("sentier-test-store-{}", std::process::id()))
                .join(FILE_NAME),
        );
    }
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

    #[test]
    fn environment_variables_in_paths() {
        let var = |n: &str| match n.to_ascii_uppercase().as_str() {
            "APPDATA" => Some(r"C:\Users\jdoe\AppData\Roaming".to_string()),
            "TEMP" => Some(r"C:\Users\jdoe\AppData\Local\Temp".to_string()),
            "PROGRAMFILES(X86)" => Some(r"C:\Program Files (x86)".to_string()),
            _ => None,
        };
        let x = |t: &str| expand_vars_with(t, &var);
        assert_eq!(
            x(r"%APPDATA%\sentier"),
            r"C:\Users\jdoe\AppData\Roaming\sentier"
        );
        assert_eq!(
            x(r"%appdata%"),
            r"C:\Users\jdoe\AppData\Roaming",
            "any case"
        );
        assert_eq!(x(r"$env:TEMP\x"), r"C:\Users\jdoe\AppData\Local\Temp\x");
        assert_eq!(x(r"$Env:appdata"), r"C:\Users\jdoe\AppData\Roaming");
        assert_eq!(x(r"%ProgramFiles(x86)%\Foo"), r"C:\Program Files (x86)\Foo");
        assert_eq!(
            x(r"%TEMP%\a\%APPDATA%"),
            r"C:\Users\jdoe\AppData\Local\Temp\a\C:\Users\jdoe\AppData\Roaming",
            "several"
        );
        // Not variables: left as typed.
        assert_eq!(x(r"D:\50% off\100%"), r"D:\50% off\100%");
        assert_eq!(x(r"%NOPE%\x"), r"%NOPE%\x");
        assert_eq!(
            x(r"%NOPE%%TEMP%"),
            r"%NOPE%C:\Users\jdoe\AppData\Local\Temp"
        );
        assert_eq!(x(r"$env:NOPE\x"), r"$env:NOPE\x");
        assert_eq!(x("%"), "%");
        assert_eq!(x(r"C:\plain"), r"C:\plain");
    }

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
        assert_eq!(a.key_str(), Some("a"));
        assert_eq!(
            a.target().map(|l| l.display()).as_deref(),
            Some(r"\\srv01\aveva\projects")
        );
        assert_eq!(
            store.key_owner("a").and_then(|o| store.owner_name(o)),
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
        assert_eq!(store.pins[0].key_str(), Some("x"));
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
        let moved = store.set_key(1, Some("a"));
        assert_eq!(moved.as_deref(), Some("AVEVA projects"));
        assert_eq!(store.key_owner("a"), Some(KeyOwner::Pin(1)));
        assert_eq!(store.pins[0].key, None);
        assert_eq!(store.set_key(1, None), None);
        assert!(store.key_owner("a").is_none());
    }

    #[test]
    fn two_letter_keys_and_clashes() {
        for ok in ["a", "Z", "7", "dl", "D2"] {
            assert!(valid_key(ok), "{ok}");
        }
        for bad in ["", "abc", "d-", "é", " d"] {
            assert!(!valid_key(bad), "{bad:?}");
        }
        assert_eq!(parse_key("  dl "), Ok(Some("dl".to_string())));
        assert_eq!(parse_key(""), Ok(None));
        assert!(parse_key("abc").is_err());

        let cases: &[(&str, &str, bool)] = &[
            ("d", "dd", true),
            ("dl", "d", true),
            ("d", "d", false),
            ("dl", "dd", false),
            ("d", "e", false),
            ("d", "Dd", false),
        ];
        for (a, b, want) in cases {
            assert_eq!(keys_clash(a, b), *want, "{a} {b}");
        }

        let (mut store, _) = sample();
        store.set_key(0, Some("d"));
        store.set_key(1, Some("dl"));
        assert_eq!(store.key_clashes("dd", None), [KeyOwner::Pin(0)]);
        assert_eq!(
            store.key_clashes("d", Some(KeyOwner::Pin(0))),
            [KeyOwner::Pin(1)]
        );
        assert!(store.key_clashes("e", None).is_empty());
        assert!(store.starts_keys('d'));
        assert!(!store.starts_keys('e'));
        assert_eq!(store.key_owner("dl"), Some(KeyOwner::Pin(1)));
    }

    #[test]
    fn loader_drops_a_clashing_key() {
        let text = r#"
            [[pin]]
            kind = "folder"
            name = "D drive"
            path = 'D:\'
            key = "d"

            [[pin]]
            kind = "folder"
            name = "Desktop"
            path = 'C:\Users\me\Desktop'
            key = "dd"

            [[pin]]
            kind = "folder"
            name = "Docs"
            path = 'C:\Docs'
            key = "do"
        "#;
        let Ok((store, problems)) = parse(text) else {
            panic!()
        };
        assert_eq!(store.pins[0].key_str(), Some("d"));
        assert_eq!(store.pins[1].key, None);
        assert_eq!(store.pins[2].key, None);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].contains("clashes with 'd"), "{problems:?}");
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
        let moved = store.set_view_key(0, Some("a"));
        assert_eq!(moved.as_deref(), Some("AVEVA projects"));
        assert_eq!(store.key_owner("a"), Some(KeyOwner::View(0)));
        assert!(!matches!(store.key_owner("a"), Some(KeyOwner::Pin(_))));
        // And back.
        store.set_key(0, Some("a"));
        assert_eq!(store.key_owner("a"), Some(KeyOwner::Pin(0)));
        assert_eq!(store.views[0].key, None);

        // Saving under the same name replaces the query, keeps the key.
        store.set_view_key(0, Some("v"));
        let i = store.save_view(View::new("MAIN server", "#aveva is:unc"));
        assert_eq!((i, store.views.len()), (0, 1));
        assert_eq!(store.views[0].query, "#aveva is:unc");
        assert_eq!(store.views[0].key_str(), Some("v"));
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
