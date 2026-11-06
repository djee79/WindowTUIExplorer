//! Open with (CLAUDE.md section 13): `o` runs the first opener a file's rule offers, `O`
//! shows them all as a menu. [`rules`] reads open.toml; [`run`] starts programs; this
//! module builds the menu (pure).

pub mod rules;
pub mod run;

use rules::{BUILTINS, OpenConfig, RunMode};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    /// From the rule (open.toml or the defaults).
    Configured,
    /// Always there: the Windows "Open with" dialog, Properties.
    System,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuItem {
    /// `default`, `nvim`, `pin:E3D`.
    pub id: String,
    pub label: String,
    /// `this terminal (inline)`, `explorer /select`, `not found`.
    pub detail: String,
    /// The direct key for it, shown on the right: `o`, `gR`, `yP`.
    pub hint: String,
    /// Its program wasn't found: shown greyed, can't run.
    pub missing: bool,
    /// The opener used last time for this extension.
    pub last: bool,
    pub section: Section,
}

fn builtin_label(id: &str) -> Option<&'static str> {
    BUILTINS.iter().find(|(n, _)| *n == id).map(|(_, l)| *l)
}

/// What `o` runs for a file: the rule's first opener, or (with `o_uses_last`) the last
/// one used for its extension, when the rule still offers it.
pub fn first_opener(cfg: &OpenConfig, name: &str, is_dir: bool, last: Option<&str>) -> String {
    let offered = cfg.openers_for(name, is_dir);
    if cfg.o_uses_last
        && let Some(l) = last.filter(|l| offered.iter().any(|o| o == l))
    {
        return l.to_string();
    }
    offered
        .into_iter()
        .next()
        .unwrap_or_else(|| "default".into())
}

/// The `O` menu for a file or folder: the rule's openers (the last-used one first when
/// `remember` is on), then Open with… and Properties.
pub fn build_menu(
    cfg: &OpenConfig,
    name: &str,
    is_dir: bool,
    last: Option<&str>,
    resolved: &BTreeMap<String, Option<String>>,
) -> Vec<MenuItem> {
    let o_runs = first_opener(cfg, name, is_dir, last);
    let item = |id: &str, section: Section| {
        let (label, detail, missing) = match cfg.openers.get(id) {
            Some(o) => {
                let missing = resolved.get(id).is_some_and(|r| r.is_none());
                let detail = if missing {
                    format!("not found: {}", o.run.first().cloned().unwrap_or_default())
                } else {
                    match o.mode {
                        RunMode::Inline => "this terminal (inline)".to_string(),
                        RunMode::Detached => "detached".to_string(),
                    }
                };
                (o.desc.clone(), detail, missing)
            }
            None => match id.strip_prefix("pin:") {
                Some(pin) => (pin.to_string(), "app pin".to_string(), false),
                None => (
                    builtin_label(id).unwrap_or(id).to_string(),
                    match id {
                        "reveal" => "explorer /select",
                        "openwith" => "Windows dialog",
                        "terminal-here" => "wezterm cli spawn",
                        _ => "",
                    }
                    .to_string(),
                    false,
                ),
            },
        };
        let hint = match id {
            _ if id == o_runs => "o",
            "reveal" => "gR",
            "copy-path" => "yp",
            "copy-unc" => "yP",
            "properties" => "Alt-Enter",
            _ => "",
        };
        MenuItem {
            id: id.to_string(),
            label,
            detail,
            hint: hint.to_string(),
            missing,
            last: last == Some(id),
            section,
        }
    };
    let mut items: Vec<MenuItem> = cfg
        .openers_for(name, is_dir)
        .iter()
        .filter(|id| !matches!(id.as_str(), "openwith" | "properties"))
        .map(|id| item(id, Section::Configured))
        .collect();
    if cfg.remember
        && let Some(i) = items.iter().position(|m| m.last)
    {
        let m = items.remove(i);
        items.insert(0, m);
    }
    if !is_dir {
        items.push(item("openwith", Section::System));
    }
    items.push(item("properties", Section::System));
    items
}

/// Menu rows matching the typed filter (case-insensitive, in label or id).
pub fn filter(items: &[MenuItem], text: &str) -> Vec<usize> {
    let t = text.to_lowercase();
    (0..items.len())
        .filter(|&i| {
            t.is_empty()
                || items[i].label.to_lowercase().contains(&t)
                || items[i].id.to_lowercase().contains(&t)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> OpenConfig {
        rules::parse(
            r#"
            [opener.nvim]
            desc = "Neovim"
            run = ["nvim", "{paths}"]
            mode = "inline"
            [opener.revu]
            desc = "Bluebeam Revu"
            run = ['C:\nope\Revu.exe', "{path}"]
            [[open.rule]]
            match = "*.pdf"
            use = ["default", "revu", "reveal", "copy-unc"]
            [[open.rule]]
            match = "*.txt"
            use = ["nvim", "default"]
            "#,
        )
        .0
    }

    #[test]
    fn menu_for_a_pdf() {
        let c = cfg();
        let mut resolved = BTreeMap::new();
        resolved.insert("revu".to_string(), None);
        resolved.insert("nvim".to_string(), Some("nvim.exe".to_string()));
        let m = build_menu(&c, "B-1001.pdf", false, Some("revu"), &resolved);
        let ids: Vec<&str> = m.iter().map(|i| i.id.as_str()).collect();
        // The last-used one first (remember), the system ones at the end.
        assert_eq!(
            ids,
            [
                "revu",
                "default",
                "reveal",
                "copy-unc",
                "openwith",
                "properties"
            ]
        );
        assert!(m[0].last && m[0].missing && m[0].detail.contains("not found"));
        assert_eq!(m[1].hint, "o", "o still runs the rule's first");
        assert_eq!(m[2].hint, "gR");
        assert_eq!(m[5].section, Section::System);
        assert_eq!(filter(&m, "REV"), [0, 2]);
        assert_eq!(filter(&m, "").len(), 6);

        let m = build_menu(&c, "notes.txt", false, None, &resolved);
        assert_eq!(m[0].detail, "this terminal (inline)");
        assert_eq!(m[0].hint, "o");
        // Folders: no Open with… dialog.
        let m = build_menu(&c, "HIT", true, None, &resolved);
        assert_eq!(m[0].id, "enter");
        assert!(!m.iter().any(|i| i.id == "openwith"));
    }

    #[test]
    fn o_uses_last_only_when_asked() {
        let mut c = cfg();
        assert_eq!(first_opener(&c, "a.pdf", false, Some("revu")), "default");
        c.o_uses_last = true;
        assert_eq!(first_opener(&c, "a.pdf", false, Some("revu")), "revu");
        // Not offered for this file any more: the rule's first.
        assert_eq!(first_opener(&c, "a.pdf", false, Some("nvim")), "default");
    }
}
