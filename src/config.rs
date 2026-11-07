//! `%APPDATA%\sentier\config.toml` (CLAUDE.md "Config"). Every field optional.
//!
//! Profiles, machines, jobs, AVEVA markers, and (M6) `show_hidden`, `numbers`, `[timeouts]`,
//! `[preview]`, `editor`. Timeouts and the picture protocol are read before the TUI starts
//! ([`load_early`]); the rest when the worker has read the file.

use crate::auth::AuthProfile;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

const FILE_NAME: &str = "config.toml";

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub auth: BTreeMap<String, AuthProfile>,
    /// `[[machine.path]]`: path templates for the machine view.
    #[serde(default)]
    pub machine: MachineSection,
    /// `[machines]`: `recent_days`, `watch`.
    #[serde(default)]
    pub machines: MachinesSection,
    /// `[jobs]`: `robocopy_threads` (`/MT:n`, default off), `native_max_bytes`.
    #[serde(default)]
    pub jobs: JobsSection,
    /// `[aveva]`: `project_markers`, the globs a project folder has at its top level.
    #[serde(default)]
    pub aveva: AvevaSection,
    /// Show hidden / system files from the start (`g.` toggles).
    pub show_hidden: Option<bool>,
    /// The line-number gutter: `hybrid` (default) | `relative` | `absolute` | `off`.
    pub numbers: Option<String>,
    /// `[timeouts]`: `local_ms` (500), `network_ms` (3000) for listings and probes.
    #[serde(default)]
    pub timeouts: TimeoutsSection,
    /// `[preview]`: `protocol` (auto | kitty | sixel | iterm2 | halfblocks),
    /// `max_network_mb` (50). `cell_px` and `cache_mb` are accepted, not used yet.
    #[serde(default)]
    pub preview: PreviewSection,
    /// The editor for `go`, `<leader>e` and `:rename` (else $VISUAL, $EDITOR, nvim).
    pub editor: Option<String>,
    /// `[theme]`: `key` and `header` colours of the `g?` overlay (`"#rrggbb"`).
    #[serde(default)]
    pub theme: ThemeSection,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct TimeoutsSection {
    pub local_ms: Option<u64>,
    pub network_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ThemeSection {
    /// Key chips in `g?` (default: the accent, amber).
    pub key: Option<String>,
    /// Section headers in `g?` (default: folder blue).
    pub header: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct PreviewSection {
    pub protocol: Option<String>,
    pub max_network_mb: Option<u64>,
    pub cell_px: Option<[u32; 2]>,
    pub cache_mb: Option<u64>,
}

/// `numbers = "…"` → (absolute number on the cursor row, relative elsewhere).
pub fn numbers(value: &str) -> Result<(bool, bool), String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "hybrid" => Ok((true, true)),
        "relative" => Ok((false, true)),
        "absolute" => Ok((true, false)),
        "off" => Ok((false, false)),
        other => Err(format!(
            "config.toml: numbers = {other:?}: hybrid, relative, absolute or off"
        )),
    }
}

impl Config {
    /// Listing / probe timeouts, the defaults where not set (a few sane limits kept).
    pub fn timeouts(&self) -> crate::fs::worker::Timeouts {
        let d = crate::fs::worker::Timeouts::default();
        let ms = |v: Option<u64>, default: std::time::Duration| {
            v.map_or(default, |m| {
                std::time::Duration::from_millis(m.clamp(100, 60_000))
            })
        };
        crate::fs::worker::Timeouts {
            local: ms(self.timeouts.local_ms, d.local),
            network: ms(self.timeouts.network_ms, d.network),
        }
    }

    /// The editor program: config, then $VISUAL, $EDITOR, else nvim.
    pub fn editor(&self) -> String {
        self.editor
            .clone()
            .filter(|e| !e.trim().is_empty())
            .or_else(|| std::env::var("VISUAL").ok().filter(|e| !e.is_empty()))
            .or_else(|| std::env::var("EDITOR").ok().filter(|e| !e.is_empty()))
            .unwrap_or_else(|| "nvim".to_string())
    }
}

/// config.toml read before the TUI starts (timeouts and the picture protocol are needed
/// then), on a helper thread with a 1 s cap: `%APPDATA%` can be a redirected network
/// folder, and startup must never hang on it. Problems come back as a message.
pub fn load_early() -> (Config, Option<String>) {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = match store_path().map(std::fs::read_to_string) {
            Some(Ok(text)) => parse(&text).map(Some),
            Some(Err(e)) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(format!("config.toml: {e}"))
            }
            _ => Ok(None),
        };
        let _ = tx.send(result);
    });
    match rx.recv_timeout(std::time::Duration::from_secs(1)) {
        Ok(Ok(Some(c))) => (c, None),
        Ok(Ok(None)) => (Config::default(), None),
        Ok(Err(e)) => (Config::default(), Some(e)),
        Err(_) => (
            Config::default(),
            Some("config.toml: reading %APPDATA% took over 1 s; defaults for now".into()),
        ),
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct MachineSection {
    #[serde(default)]
    pub path: Vec<crate::machines::PathTemplate>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct MachinesSection {
    pub recent_days: Option<u32>,
    #[serde(default)]
    pub watch: bool,
    /// An inventory export (`host,user,…` with a header row) for the machine picker.
    pub inventory_csv: Option<String>,
    /// A command that prints machines as JSON (e.g. `getad`), for the machine picker.
    pub source_cmd: Option<String>,
    /// Log changes made on admin shares to `audit.log` (default on).
    pub audit: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct JobsSection {
    pub robocopy_threads: Option<u32>,
    pub native_max_bytes: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct AvevaSection {
    pub project_markers: Option<Vec<String>>,
}

/// A Dabacon project folder holds an `evars.bat` next to its `*000` database folders.
pub const DEFAULT_AVEVA_MARKERS: &[&str] = &["evars.bat", "*000"];

impl Config {
    /// The AVEVA guard's markers: every one must match a name at the folder's top level.
    pub fn aveva_markers(&self) -> Vec<String> {
        self.aveva.project_markers.clone().unwrap_or_else(|| {
            DEFAULT_AVEVA_MARKERS
                .iter()
                .map(|s| s.to_string())
                .collect()
        })
    }

    /// The path templates: config.toml's, or the spec's defaults when it has none.
    pub fn templates(&self) -> Vec<crate::machines::PathTemplate> {
        if self.machine.path.is_empty() {
            crate::machines::default_templates()
        } else {
            self.machine.path.clone()
        }
    }

    pub fn recent_days(&self) -> u32 {
        self.machines
            .recent_days
            .unwrap_or(crate::machines::DEFAULT_RECENT_DAYS)
    }
}

/// Parses config.toml. A broken file is an error message (shown, not fatal).
pub fn parse(text: &str) -> Result<Config, String> {
    toml::from_str(text).map_err(|e| format!("config.toml: {}", e.message()))
}

/// `%APPDATA%\sentier\config.toml`, or None when APPDATA isn't set.
pub fn store_path() -> Option<PathBuf> {
    // Tests never read the user's own settings.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_timeouts_preview_and_editor() {
        let Ok(c) = parse(
            r#"
show_hidden = true
numbers = "relative"
editor = "code --wait"
[timeouts]
local_ms = 800
network_ms = 10
[preview]
protocol = "halfblocks"
max_network_mb = 200
cell_px = [9, 18]
cache_mb = 500
"#,
        ) else {
            panic!()
        };
        assert_eq!(c.show_hidden, Some(true));
        assert_eq!(
            numbers(c.numbers.as_deref().unwrap_or("")),
            Ok((false, true))
        );
        let t = c.timeouts();
        assert_eq!(t.local, std::time::Duration::from_millis(800));
        assert_eq!(t.network, std::time::Duration::from_millis(100), "clamped");
        assert_eq!(c.preview.protocol.as_deref(), Some("halfblocks"));
        assert_eq!(c.preview.max_network_mb, Some(200));
        assert_eq!(c.editor(), "code --wait");
        assert_eq!(numbers("Hybrid"), Ok((true, true)));
        assert_eq!(numbers("off"), Ok((false, false)));
        assert!(numbers("sometimes").is_err());
        // Nothing set: the defaults.
        let d = Config::default();
        assert_eq!(d.timeouts(), crate::fs::worker::Timeouts::default());
    }

    #[test]
    fn auth_profiles_and_unknown_settings() {
        let text = r#"
show_hidden = true
numbers = "hybrid"

[auth.admin]
user = 'CORP\adm-user'

[preview]
protocol = "auto"
"#;
        let Ok(c) = parse(text) else { panic!() };
        assert_eq!(c.auth.len(), 1);
        assert_eq!(c.auth["admin"].user, r"CORP\adm-user");
        assert_eq!(parse(""), Ok(Config::default()));
        assert!(parse("[auth.admin]\nuser = 3").is_err());
    }

    #[test]
    fn machine_templates_and_recent_days() {
        assert_eq!(
            Config::default().templates().len(),
            6,
            "the spec's defaults"
        );
        assert_eq!(Config::default().recent_days(), 30);
        let text = r#"
[machines]
recent_days = 7

[[machine.path]]
name = "AVEVA local"
path = 'C$\Users\{user}\AppData\Local\AVEVA'
key  = "a"
"#;
        let Ok(c) = parse(text) else { panic!() };
        let t = c.templates();
        assert_eq!(t.len(), 1, "configured ones replace the defaults");
        assert_eq!(
            (t[0].name.as_str(), t[0].key_char()),
            ("AVEVA local", Some('a'))
        );
        assert_eq!(c.recent_days(), 7);
    }
}
