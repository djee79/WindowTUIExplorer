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
}
