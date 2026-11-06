//! Saved rename presets and the panel's memory (CLAUDE.md section 16).
//!
//! - Presets live in `%APPDATA%\sentier\rename.toml`, hand-editable:
//!   ```toml
//!   [[rename.preset]]
//!   name    = "ISO rev bump"
//!   search  = '(B-\d{4})_rev(\d+)'
//!   replace = 'HIT_$1_R$2'
//!   [rename.preset.options]
//!   regex = true
//!   ```
//! - The last options and the last 50 searches / replaces (PowerRename's "restore flags
//!   on launch" and "recently used strings") live in
//!   `%LOCALAPPDATA%\sentier\rename-state.toml`.
//!
//! Pure but for `load_*` / `save_*`, which only the worker calls.

use super::engine::Options;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Kept per field.
pub const HISTORY: usize = 50;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preset {
    pub name: String,
    pub search: String,
    pub replace: String,
    pub options: Options,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct Section {
    preset: Vec<Preset>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct File {
    rename: Section,
}

/// The panel's memory between runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub options: Options,
    /// Newest first.
    pub search: Vec<String>,
    pub replace: Vec<String>,
}

/// Puts `text` at the front of `list` (once), keeping [`HISTORY`] entries.
pub fn remember(list: &mut Vec<String>, text: &str) {
    if text.is_empty() {
        return;
    }
    list.retain(|t| t != text);
    list.insert(0, text.to_string());
    list.truncate(HISTORY);
}

/// Adds or replaces (same name, any case) a preset; the list stays sorted by name.
pub fn upsert(list: &mut Vec<Preset>, p: Preset) {
    list.retain(|q| !q.name.eq_ignore_ascii_case(&p.name));
    list.push(p);
    list.sort_by_key(|p| p.name.to_lowercase());
}

pub fn parse_presets(text: &str) -> Result<Vec<Preset>, String> {
    toml::from_str::<File>(text)
        .map(|f| f.rename.preset)
        .map_err(|e| e.message().to_string())
}

pub fn presets_text(list: &[Preset]) -> String {
    toml::to_string_pretty(&File {
        rename: Section {
            preset: list.to_vec(),
        },
    })
    .unwrap_or_default()
}

fn test_dir() -> PathBuf {
    std::env::temp_dir().join(format!("sentier-test-rename-{}", std::process::id()))
}

/// `%APPDATA%\sentier\rename.toml`.
pub fn presets_path() -> Option<PathBuf> {
    if cfg!(test) {
        return Some(test_dir().join("rename.toml"));
    }
    Some(
        PathBuf::from(std::env::var_os("APPDATA")?)
            .join("sentier")
            .join("rename.toml"),
    )
}

/// `%LOCALAPPDATA%\sentier\rename-state.toml`.
pub fn state_path() -> Option<PathBuf> {
    if cfg!(test) {
        return Some(test_dir().join("rename-state.toml"));
    }
    Some(
        PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
            .join("sentier")
            .join("rename-state.toml"),
    )
}

/// Missing file: none. A broken file: the reason, so the status bar can say it.
pub fn load_presets(path: &Path) -> Result<Vec<Preset>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_presets(&text).map_err(|e| format!("rename.toml: {e}")),
        Err(_) => Ok(Vec::new()),
    }
}

pub fn save_presets(path: &Path, list: &[Preset]) -> Result<(), String> {
    write(path, &presets_text(list))
}

pub fn load_state(path: &Path) -> State {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| toml::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save_state(path: &Path, state: &State) -> Result<(), String> {
    write(path, &toml::to_string_pretty(state).unwrap_or_default())
}

fn write(path: &Path, text: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    crate::fs::write_atomic(path, text).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_round_trip_and_hand_edits_load() {
        let mut list = Vec::new();
        let o = Options {
            regex: true,
            ..Options::default()
        };
        upsert(
            &mut list,
            Preset {
                name: "ISO rev bump".into(),
                search: r"(B-\d{4})_rev(\d+)".into(),
                replace: "HIT_$1_R$2".into(),
                options: o,
            },
        );
        upsert(
            &mut list,
            Preset {
                name: "a first".into(),
                ..Preset::default()
            },
        );
        upsert(
            &mut list,
            Preset {
                name: "iso REV bump".into(),
                search: "x".into(),
                ..Preset::default()
            },
        );
        assert_eq!(list.len(), 2, "same name replaces");
        assert_eq!(list[0].name, "a first");
        assert_eq!(list[1].search, "x");
        assert_eq!(parse_presets(&presets_text(&list)), Ok(list));

        let hand = "[[rename.preset]]\nname = \"photos\"\nreplace = '$YYYY-$MM-$DD'\n[rename.preset.options]\ndates = \"taken\"\n";
        let p = parse_presets(hand).unwrap_or_default();
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].options.dates, super::super::engine::DateSource::Taken);
        assert!(p[0].options.files, "unset options keep their defaults");
        assert!(parse_presets("[[rename.preset]]\nname = 3").is_err());
    }

    #[test]
    fn history_keeps_the_newest_once() {
        let mut h = Vec::new();
        for i in 0..60 {
            remember(&mut h, &format!("s{i}"));
        }
        remember(&mut h, "s10");
        remember(&mut h, "");
        assert_eq!(h.len(), HISTORY);
        assert_eq!(h[0], "s10");
        assert_eq!(h.iter().filter(|s| *s == "s10").count(), 1);

        let path = state_path().unwrap_or_default();
        let st = State {
            search: h.clone(),
            ..State::default()
        };
        assert!(save_state(&path, &st).is_ok());
        assert_eq!(load_state(&path), st);
    }
}
