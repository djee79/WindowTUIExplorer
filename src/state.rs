//! What sentier remembers between starts that isn't configuration: the folder order
//! (`,m`, `,s`…). `%LOCALAPPDATA%\sentier\ui.toml`, written by the app, never needs
//! editing (config.toml stays the user's own file). Pure but for `load` / `save`, which
//! only the worker calls.

use crate::fs::sort::{DirSort, SortKey};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiState {
    /// `name`, `modified`, `created`, `size`, `ext`.
    pub sort: String,
    pub sort_reverse: bool,
}

impl UiState {
    pub fn from_sort(s: DirSort) -> UiState {
        UiState {
            sort: s.key.word().to_string(),
            sort_reverse: s.reverse,
        }
    }

    /// The folder order; anything unknown (or relevance, results only) is by name.
    pub fn sort(&self) -> DirSort {
        let key = SortKey::parse(&self.sort)
            .filter(|k| *k != SortKey::Relevance)
            .unwrap_or_default();
        DirSort {
            key,
            reverse: self.sort_reverse,
        }
    }
}

pub fn path() -> Option<PathBuf> {
    if cfg!(test) {
        return Some(
            std::env::temp_dir()
                .join(format!("sentier-test-ui-{}", std::process::id()))
                .join("ui.toml"),
        );
    }
    Some(
        PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
            .join("sentier")
            .join("ui.toml"),
    )
}

pub fn load(path: &Path) -> UiState {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| toml::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save(path: &Path, state: &UiState) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let text = toml::to_string_pretty(state).map_err(|e| e.to_string())?;
    crate::fs::write_atomic(path, &text).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_defaults() {
        let s = DirSort {
            key: SortKey::Modified,
            reverse: true,
        };
        let st = UiState::from_sort(s);
        assert_eq!(st.sort(), s);
        let p = path().unwrap_or_default();
        assert!(save(&p, &st).is_ok());
        assert_eq!(load(&p), st);
        // Unknown or results-only: by name.
        let odd = UiState {
            sort: "relevance".into(),
            sort_reverse: false,
        };
        assert_eq!(odd.sort(), DirSort::default());
        assert_eq!(UiState::default().sort(), DirSort::default());
    }
}
