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
    /// The apps Windows has registered for the extension (Explorer's "Open with").
    Windows,
    /// Always there: the Windows "Open with" dialog, Properties.
    System,
}

/// What Windows has for an extension: its default app's name, and the recommended apps
/// (id, name shown), as Explorer's "Open with" lists them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Registered {
    pub default_app: Option<String>,
    pub apps: Vec<(String, String)>,
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

/// `nvim · this terminal`, `code`: the editor and where it runs.
fn editor_detail(editor: &str) -> String {
    let parts = rules::split_args(editor);
    let program = parts.first().cloned().unwrap_or_default();
    let waits = parts.iter().any(|a| a == "--wait" || a == "-w");
    if rules::terminal_editor(&program) {
        format!("{program} · this terminal")
    } else if waits {
        format!("{program} · waits until closed")
    } else {
        program
    }
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
    registered: Option<&Registered>,
    editor: &str,
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
                        "editor" => editor_detail(editor),
                        "reveal" => "explorer /select".to_string(),
                        "openwith" => "Windows dialog".to_string(),
                        "terminal-here" => "wezterm cli spawn".to_string(),
                        _ => String::new(),
                    },
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
    // The editor is always on offer, whatever the rules say; so are Notepad++ and VS
    // Code for files when they're installed. Not installed (or not looked up yet): hidden.
    let found = |id: &str| resolved.get(id).is_some_and(|r| r.is_some());
    items.retain(|m| !cfg.optional.contains(&m.id) || found(&m.id));
    if !items.iter().any(|m| m.id == "editor") {
        items.push(item("editor", Section::Configured));
    }
    if !is_dir {
        for id in &cfg.optional {
            if found(id) && !items.iter().any(|m| &m.id == id) {
                items.push(item(id, Section::Configured));
            }
        }
    }
    if cfg.remember
        && let Some(i) = items.iter().position(|m| m.last)
    {
        let m = items.remove(i);
        items.insert(0, m);
    }
    if let Some(d) = items.iter_mut().find(|m| m.id == "default")
        && let Some(app) = registered.and_then(|r| r.default_app.clone())
    {
        d.detail = app;
    }
    if !is_dir && !rules::ext_of(name, is_dir).is_empty() {
        match registered {
            None => items.push(MenuItem {
                id: "assoc-loading".into(),
                label: "…".into(),
                detail: "looking up the apps Windows has for it".into(),
                hint: String::new(),
                missing: true,
                last: false,
                section: Section::Windows,
            }),
            Some(r) => items.extend(r.apps.iter().map(|(id, ui)| {
                let id = format!("assoc:{id}");
                MenuItem {
                    last: last == Some(id.as_str()),
                    id,
                    label: ui.clone(),
                    detail: String::new(),
                    hint: String::new(),
                    missing: false,
                    section: Section::Windows,
                }
            })),
        }
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
        let reg = Registered {
            default_app: Some("Adobe Acrobat".into()),
            apps: vec![
                (r"C:\x\msedge.exe".into(), "Microsoft Edge".into()),
                ("foxit".into(), "Foxit PDF Reader".into()),
            ],
        };
        let m = build_menu(
            &c,
            "B-1001.pdf",
            false,
            Some("revu"),
            &resolved,
            Some(&reg),
            "nvim",
        );
        let ids: Vec<&str> = m.iter().map(|i| i.id.as_str()).collect();
        // The last-used one first (remember), Windows' apps, the system ones at the end.
        assert_eq!(
            ids,
            [
                "revu",
                "default",
                "reveal",
                "copy-unc",
                "editor",
                r"assoc:C:\x\msedge.exe",
                "assoc:foxit",
                "openwith",
                "properties"
            ]
        );
        assert!(m[0].last && m[0].missing && m[0].detail.contains("not found"));
        assert_eq!(m[1].hint, "o", "o still runs the rule's first");
        assert_eq!(m[1].detail, "Adobe Acrobat", "the default app's name");
        assert_eq!(m[2].hint, "gR");
        assert_eq!(
            (m[5].section, m[5].label.as_str()),
            (Section::Windows, "Microsoft Edge")
        );
        assert_eq!(m[8].section, Section::System);
        assert_eq!(
            (m[4].label.as_str(), m[4].detail.as_str()),
            ("Editor", "nvim · this terminal"),
            "the editor is always offered"
        );
        assert_eq!(filter(&m, "REV"), [0, 2]);
        assert_eq!(filter(&m, "foxit"), [6]);
        assert_eq!(filter(&m, "").len(), 9);

        // Still looking: one greyed row in the Windows section.
        let m = build_menu(&c, "notes.txt", false, None, &resolved, None, "nvim");
        assert_eq!(m[0].detail, "this terminal (inline)");
        assert_eq!(m[0].hint, "o");
        assert!(m.iter().any(|i| i.id == "assoc-loading" && i.missing));
        // No open.toml, a .pmlmac: the editor first (o), Notepad++ because it's
        // installed, VS Code hidden because it isn't.
        let d = OpenConfig::default();
        let mut found = BTreeMap::new();
        found.insert(
            "notepadpp".to_string(),
            Some(r"C:\Program Files\Notepad++\notepad++.exe".to_string()),
        );
        found.insert("vscode".to_string(), None);
        let m = build_menu(&d, "run_test.pmlmac", false, None, &found, None, "nvim");
        let ids: Vec<&str> = m.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids[..2], ["editor", "notepadpp"], "{ids:?}");
        assert!(!ids.contains(&"vscode"));
        assert_eq!(m[0].hint, "o");
        assert_eq!(m[1].label, "Notepad++");
        // A GUI editor in config.toml: named, not "this terminal".
        let m = build_menu(&d, "a.txt", false, None, &found, None, "code");
        assert_eq!(m[0].detail, "code");

        // Folders: no Open with… dialog, no Windows apps.
        let m = build_menu(&c, "HIT", true, None, &resolved, None, "nvim");
        assert_eq!(m[0].id, "enter");
        assert!(
            !m.iter()
                .any(|i| i.id == "openwith" || i.section == Section::Windows)
        );
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
