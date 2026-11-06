//! `%APPDATA%\sentier\config.toml` (CLAUDE.md "Config"). Every field optional.
//!
//! So far only `[auth.<name>]` profiles are read (M2c); the other settings in the spec
//! (`show_hidden`, `numbers`, timeouts, preview…) arrive with the config milestone and are
//! ignored until then, so a file that already has them still loads.

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
    let base = std::env::var_os("APPDATA")?;
    Some(PathBuf::from(base).join("sentier").join(FILE_NAME))
}

#[cfg(test)]
mod tests {
    use super::*;

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
