//! `%APPDATA%\sentier\open.toml` (CLAUDE.md section 13): openers (what can run) and rules
//! (which openers a file is offered, in order; the first is what `o` does).
//!
//! ```toml
//! [opener.nvim]
//! desc = "Neovim (this terminal)"
//! run  = ["nvim", "{paths}"]
//! mode = "inline"          # inline | detached | tab (= detached)
//! for  = "all"             # all: one process for the selection · each: one per file
//!
//! [[open.rule]]
//! match = "*.{pml,txt}"    # a glob on the file name, or "dir" for folders
//! use   = ["nvim", "default", "reveal"]
//!
//! [open]
//! remember = true          # the last opener used for an extension goes to the top of O
//! o_uses_last = false      # … and `o` runs it
//! ```
//!
//! Built-in openers need no config: `default`, `reveal`, `openwith`, `properties`,
//! `explorer`, `terminal-here`, `copy-path`, `copy-unc`, `enter`. `pin:<name>` uses an
//! `app` pin. Loading is lenient: a bad opener or rule is reported and skipped, the rest
//! loads; with no file at all the defaults below apply.
//!
//! Pure: no I/O.

use serde::Deserialize;
use std::collections::BTreeMap;

pub const BUILTINS: &[(&str, &str)] = &[
    ("default", "Default app"),
    ("reveal", "Reveal in Explorer"),
    ("openwith", "Open with… (Windows dialog)"),
    ("properties", "Properties"),
    ("explorer", "Open the folder in Explorer"),
    ("terminal-here", "WezTerm tab here"),
    ("copy-path", "Copy path"),
    ("copy-unc", "Copy UNC path"),
    ("enter", "Enter"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    /// Suspend the TUI and run in this terminal (Neovim, less…).
    Inline,
    /// Start and forget (GUI apps, a new WezTerm tab).
    Detached,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum For {
    /// One process for the whole selection.
    All,
    /// One process per file.
    Each,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opener {
    pub desc: String,
    /// The program, then its arguments, with placeholders.
    pub run: Vec<String>,
    pub mode: RunMode,
    pub each: For,
}

#[derive(Debug, Clone)]
pub enum Match {
    Dir,
    Glob(globset::GlobMatcher),
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub pattern: String,
    pub matcher: Match,
    pub openers: Vec<String>,
}

impl Rule {
    pub fn matches(&self, name: &str, is_dir: bool) -> bool {
        match &self.matcher {
            Match::Dir => is_dir,
            Match::Glob(g) => !is_dir && g.is_match(name),
        }
    }
}

#[derive(Debug, Clone)]
pub struct OpenConfig {
    pub openers: BTreeMap<String, Opener>,
    pub rules: Vec<Rule>,
    pub remember: bool,
    pub o_uses_last: bool,
}

impl Default for OpenConfig {
    /// No open.toml: folders enter, files open in their Windows app.
    fn default() -> Self {
        let rules = [
            (
                "dir",
                &[
                    "enter",
                    "explorer",
                    "terminal-here",
                    "copy-path",
                    "copy-unc",
                ][..],
            ),
            ("*", &["default", "reveal", "copy-path", "copy-unc"][..]),
        ]
        .iter()
        .filter_map(|(m, u)| rule(m, u.iter().map(|s| s.to_string()).collect()).ok())
        .collect();
        OpenConfig {
            openers: BTreeMap::new(),
            rules,
            remember: true,
            o_uses_last: false,
        }
    }
}

fn rule(pattern: &str, openers: Vec<String>) -> Result<Rule, String> {
    let matcher = if pattern.eq_ignore_ascii_case("dir") {
        Match::Dir
    } else {
        let g = globset::GlobBuilder::new(pattern)
            .case_insensitive(true)
            .literal_separator(true)
            .build()
            .map_err(|e| format!("bad match {pattern:?}: {e}"))?;
        Match::Glob(g.compile_matcher())
    };
    Ok(Rule {
        pattern: pattern.to_string(),
        matcher,
        openers,
    })
}

pub fn is_builtin(name: &str) -> bool {
    BUILTINS.iter().any(|(n, _)| *n == name)
}

// ------------------------------------------------------------------ parsing

#[derive(Deserialize)]
struct RawOpener {
    #[serde(default)]
    desc: Option<String>,
    run: Vec<String>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default, rename = "for")]
    each: Option<String>,
}

#[derive(Deserialize)]
struct RawRule {
    #[serde(rename = "match")]
    pattern: String,
    #[serde(rename = "use")]
    openers: Vec<String>,
}

/// Parses open.toml. Problems are returned, never fatal: what's good loads. A file with
/// no rules keeps the default rules (after its own, if any).
pub fn parse(text: &str) -> (OpenConfig, Vec<String>) {
    let mut problems = Vec::new();
    let mut cfg = OpenConfig::default();
    let table: toml::Table = match toml::from_str(text) {
        Ok(t) => t,
        Err(e) => {
            problems.push(format!("open.toml: {e}"));
            return (cfg, problems);
        }
    };
    if let Some(openers) = table.get("opener").and_then(|v| v.as_table()) {
        for (name, value) in openers {
            if is_builtin(name) || name.starts_with("pin:") {
                problems.push(format!("open.toml: opener {name:?} is a built-in name"));
                continue;
            }
            match value.clone().try_into::<RawOpener>() {
                Ok(raw) => match opener(raw) {
                    Ok(o) => {
                        cfg.openers.insert(name.clone(), o);
                    }
                    Err(e) => problems.push(format!("open.toml: opener {name}: {e}")),
                },
                Err(e) => problems.push(format!("open.toml: opener {name}: {e}")),
            }
        }
    }
    let open = table.get("open").and_then(|v| v.as_table());
    if let Some(open) = open {
        if let Some(b) = open.get("remember").and_then(|v| v.as_bool()) {
            cfg.remember = b;
        }
        if let Some(b) = open.get("o_uses_last").and_then(|v| v.as_bool()) {
            cfg.o_uses_last = b;
        }
    }
    let raw_rules = open
        .and_then(|o| o.get("rule"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut rules = Vec::new();
    for (i, value) in raw_rules.into_iter().enumerate() {
        let raw: RawRule = match value.try_into() {
            Ok(r) => r,
            Err(e) => {
                problems.push(format!("open.toml: rule {}: {e}", i + 1));
                continue;
            }
        };
        // Unknown openers are dropped from the rule (and reported); the rest stays.
        let mut used = Vec::new();
        for name in raw.openers {
            if is_builtin(&name) || name.starts_with("pin:") || cfg.openers.contains_key(&name) {
                used.push(name);
            } else {
                problems.push(format!(
                    "open.toml: rule {:?} uses unknown opener {name:?}",
                    raw.pattern
                ));
            }
        }
        match rule(&raw.pattern, used) {
            Ok(r) => rules.push(r),
            Err(e) => problems.push(format!("open.toml: rule {}: {e}", i + 1)),
        }
    }
    // The defaults stay last, so files no rule mentions still open.
    rules.append(&mut cfg.rules);
    cfg.rules = rules;
    (cfg, problems)
}

fn opener(raw: RawOpener) -> Result<Opener, String> {
    if raw.run.first().is_none_or(|p| p.trim().is_empty()) {
        return Err("run needs a program".into());
    }
    let mode = match raw.mode.as_deref().map(str::to_ascii_lowercase).as_deref() {
        None | Some("detached") | Some("tab") => RunMode::Detached,
        Some("inline") => RunMode::Inline,
        Some(other) => return Err(format!("mode {other:?}: inline, detached or tab")),
    };
    let each = match raw.each.as_deref().map(str::to_ascii_lowercase).as_deref() {
        None | Some("all") => For::All,
        Some("each") => For::Each,
        Some(other) => return Err(format!("for {other:?}: all or each")),
    };
    let desc = raw.desc.unwrap_or_else(|| raw.run[0].clone());
    Ok(Opener {
        desc,
        run: raw.run,
        mode,
        each,
    })
}

// ------------------------------------------------------------------ using it

impl OpenConfig {
    /// The openers offered for a file or folder, in order: the first matching rule's.
    pub fn openers_for(&self, name: &str, is_dir: bool) -> Vec<String> {
        self.rules
            .iter()
            .find(|r| r.matches(name, is_dir) && !r.openers.is_empty())
            .map(|r| r.openers.clone())
            .unwrap_or_else(|| vec![if is_dir { "enter" } else { "default" }.to_string()])
    }

    /// The rule's pattern that applies (shown in the menu: `*.pdf · open.toml`).
    pub fn pattern_for(&self, name: &str, is_dir: bool) -> Option<&str> {
        self.rules
            .iter()
            .find(|r| r.matches(name, is_dir))
            .map(|r| r.pattern.as_str())
    }
}

/// `.PDF` → `pdf`; folders and files without one → "".
pub fn ext_of(name: &str, is_dir: bool) -> String {
    if is_dir {
        return String::new();
    }
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => ext.to_ascii_lowercase(),
        _ => String::new(),
    }
}

/// One target of an opener: its full path (UNC stays UNC, never re-mapped).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub path: String,
}

impl Target {
    fn name(&self) -> &str {
        self.path
            .trim_end_matches('\\')
            .rsplit('\\')
            .next()
            .unwrap_or(&self.path)
    }

    fn dir(&self) -> String {
        let p = self.path.trim_end_matches('\\');
        match p.rfind('\\') {
            Some(2) if p.as_bytes().get(1) == Some(&b':') => p[..3].to_string(),
            Some(i) => p[..i].to_string(),
            None => p.to_string(),
        }
    }

    fn stem_ext(&self) -> (&str, &str) {
        match self.name().rsplit_once('.') {
            Some((s, e)) if !s.is_empty() => (s, e),
            _ => (self.name(), ""),
        }
    }
}

/// The command lines an opener runs for `targets`: one (`for = all`) or one per target
/// (`for = each`). `{paths}` alone as an argument becomes one argument per target;
/// the others are replaced inside arguments.
pub fn command_lines(o: &Opener, targets: &[Target]) -> Vec<Vec<String>> {
    let groups: Vec<&[Target]> = match o.each {
        For::All => vec![targets],
        For::Each => targets.chunks(1).collect(),
    };
    groups
        .into_iter()
        .filter(|g| !g.is_empty())
        .map(|group| {
            let first = &group[0];
            let (stem, ext) = first.stem_ext();
            let mut out = Vec::new();
            for arg in &o.run {
                if arg == "{paths}" {
                    out.extend(group.iter().map(|t| t.path.clone()));
                    continue;
                }
                let paths = group
                    .iter()
                    .map(|t| t.path.clone())
                    .collect::<Vec<_>>()
                    .join(" ");
                out.push(
                    arg.replace("{paths}", &paths)
                        .replace("{path}", &first.path)
                        .replace("{dir}", &first.dir())
                        .replace("{name}", first.name())
                        .replace("{stem}", stem)
                        .replace("{ext}", ext),
                );
            }
            out
        })
        .collect()
}

/// Splits a pin's `args` string the way a command line would: spaces separate, double
/// quotes group.
pub fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut any = false;
    for c in s.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            c if c.is_whitespace() && !quoted => {
                if any || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                any = false;
            }
            c => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
        [opener.nvim]
        desc = "Neovim (this terminal)"
        run = ["nvim", "{paths}"]
        mode = "inline"

        [opener.revu]
        desc = "Bluebeam Revu"
        run = ['C:\Program Files\Bluebeam\Revu.exe', "{path}"]
        mode = "detached"
        for = "each"

        [opener.bad]
        run = []

        [opener.reveal]
        run = ["x"]

        [[open.rule]]
        match = "*.{pml,pmlmac,txt}"
        use = ["nvim", "default", "reveal"]

        [[open.rule]]
        match = "*.pdf"
        use = ["default", "revu", "nope", "pin:E3D"]

        [[open.rule]]
        match = "dir"
        use = ["enter", "terminal-here"]

        [open]
        o_uses_last = true
    "#;

    #[test]
    fn parse_and_match() {
        let (cfg, problems) = parse(SAMPLE);
        assert_eq!(cfg.openers.len(), 2, "{problems:?}");
        assert_eq!(cfg.openers["nvim"].mode, RunMode::Inline);
        assert_eq!(cfg.openers["revu"].each, For::Each);
        assert!(cfg.o_uses_last && cfg.remember);
        for want in [
            "opener bad",
            "\"reveal\" is a built-in",
            "unknown opener \"nope\"",
        ] {
            assert!(
                problems.iter().any(|p| p.contains(want)),
                "{want}: {problems:?}"
            );
        }
        let table = [
            ("pipe.PMLMAC", false, &["nvim", "default", "reveal"][..]),
            ("B-1001.pdf", false, &["default", "revu", "pin:E3D"][..]),
            ("HIT", true, &["enter", "terminal-here"][..]),
            // No rule of the file's: the built-in default rule.
            (
                "photo.jpg",
                false,
                &["default", "reveal", "copy-path", "copy-unc"][..],
            ),
        ];
        for (name, dir, want) in table {
            assert_eq!(cfg.openers_for(name, dir), want, "{name}");
        }
        assert_eq!(cfg.pattern_for("x.pdf", false), Some("*.pdf"));
    }

    #[test]
    fn defaults_and_broken_files() {
        let cfg = OpenConfig::default();
        assert_eq!(cfg.openers_for("a.dwg", false)[0], "default");
        assert_eq!(cfg.openers_for("HIT", true)[0], "enter");
        let (cfg, problems) = parse("not [toml");
        assert_eq!(problems.len(), 1);
        assert_eq!(cfg.openers_for("a", false)[0], "default");
        let (_, problems) =
            parse("[opener.x]\nrun=['x']\nmode='popup'\n[[open.rule]]\nmatch='['\nuse=['x']");
        assert!(
            problems.iter().any(|p| p.contains("mode \"popup\"")),
            "{problems:?}"
        );
        assert!(
            problems.iter().any(|p| p.contains("bad match")),
            "{problems:?}"
        );
    }

    #[test]
    fn placeholders() {
        let t = |p: &str| Target { path: p.into() };
        let targets = [t(r"\\srv01\p\HIT\a b.pml"), t(r"\\srv01\p\HIT\c.txt")];
        let o = |run: &[&str], each: For| Opener {
            desc: String::new(),
            run: run.iter().map(|s| s.to_string()).collect(),
            mode: RunMode::Detached,
            each,
        };
        assert_eq!(
            command_lines(&o(&["nvim", "{paths}"], For::All), &targets),
            [vec![
                "nvim".to_string(),
                r"\\srv01\p\HIT\a b.pml".into(),
                r"\\srv01\p\HIT\c.txt".into()
            ]]
        );
        let lines = command_lines(
            &o(&["x", "--cwd={dir}", "{name}", "{stem}.{ext}"], For::Each),
            &targets,
        );
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0],
            ["x", r"--cwd=\\srv01\p\HIT", "a b.pml", "a b.pml"]
        );
        assert_eq!(lines[1][3], "c.txt");
        assert_eq!(t(r"C:\a.txt").dir(), r"C:\");
        assert_eq!(ext_of("B.PDF", false), "pdf");
        assert_eq!(ext_of(".gitignore", false), "");
        assert_eq!(ext_of("x.pdf", true), "");
    }

    #[test]
    fn pin_args_split() {
        assert_eq!(
            split_args(r#"-p "C:\Program Files\x" --flag  "" end"#),
            ["-p", r"C:\Program Files\x", "--flag", "", "end"]
        );
        assert!(split_args("   ").is_empty());
    }
}
