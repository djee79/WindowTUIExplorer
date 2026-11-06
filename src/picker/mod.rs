//! The Telescope-style picker (CLAUDE.md section 7, rule 8).
//!
//! Items go into a `nucleo` matcher, which matches on its own thread pool; the UI asks
//! for a snapshot each frame and renders only the visible rows, so typing never waits.
//! Filter words (`#tag`, `is:`, `on:`, see [`query`]) narrow the set before matching:
//! when they change, the matcher restarts with only the items that pass. The rest of
//! the query is the fuzzy pattern.
//!
//! Ranking keeps wtm's tiers on top of nucleo's score: name prefix › tag prefix ›
//! substring › fuzzy (CLAUDE.md section 6). Frecency joins with the launcher (M5).

pub mod query;

use crate::pins::PinKind;
use crate::prompt::LineEditor;
use nucleo::pattern::{CaseMatching, Normalization};
use nucleo::{Config, Matcher, Nucleo, Utf32Str};
use query::{Completion, Facts, PinQuery};
use std::sync::Arc;

/// Most matches the tier sort looks at; beyond that, nucleo's order stands.
const RANK_CAP: u32 = 5_000;

/// What a row is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Pin(PinKind),
    /// A saved view (`[[view]]`): opening it opens the pin picker with its query.
    View,
    /// A tag, in the tag picker.
    Tag,
    /// A machine that isn't pinned (recent, inventory CSV, `source_cmd`), in the machine
    /// picker; the name is its host.
    Machine,
    /// A file or folder found by the files / recent folders / grep pickers; `id` indexes
    /// the picker's paths (`App::picker_paths`).
    File,
    Dir,
    /// A key binding (`id` into the keymap) or a `:` command (`id` into `COMMANDS`).
    Binding,
    Command,
    /// One of the launcher's "+ …" rows (pin a URL, Start Menu, import wtm, new presets).
    Action,
    /// A Start Menu shortcut in the launcher (`^s`): the name, the `.lnk` path.
    StartMenu,
}

impl ItemKind {
    pub fn label(self) -> &'static str {
        match self {
            ItemKind::Pin(k) => k.label(),
            ItemKind::View => "view",
            ItemKind::Tag => "tag",
            ItemKind::Machine => "machine",
            ItemKind::StartMenu => "start",
            ItemKind::Action => "+",
            ItemKind::File => "file",
            ItemKind::Dir => "dir",
            ItemKind::Binding => "key",
            ItemKind::Command => ":",
        }
    }

    pub fn is_machine(self) -> bool {
        matches!(self, ItemKind::Machine | ItemKind::Pin(PinKind::Machine))
    }
}

/// One row of a picker.
#[derive(Debug, Clone)]
pub struct PickItem {
    /// Where it came from: the index of the pin / view in the store, or of the tag.
    pub id: usize,
    pub name: String,
    /// Path, command, URL, or a view's query.
    pub detail: String,
    pub kind: ItemKind,
    pub key: Option<char>,
    pub tags: Vec<String>,
    pub unc: bool,
    pub local: bool,
    /// The last probe: Some(true) reachable, Some(false) failed, None not checked.
    pub reachable: Option<bool>,
    pub server: Option<String>,
    /// Frecency (launcher ranking, "frequent"); 0 when never used or not tracked.
    pub score: u64,
}

impl PickItem {
    fn facts(&self) -> Facts<'_> {
        Facts {
            kind: match self.kind {
                ItemKind::Pin(k) => Some(k),
                _ => None,
            },
            view: self.kind == ItemKind::View,
            machine: self.kind.is_machine(),
            online: self.kind.is_machine().then_some(self.reachable).flatten(),
            tags: &self.tags,
            unc: self.unc,
            local: self.local,
            unreachable: self.reachable == Some(false),
            server: self.server.clone(),
        }
    }

    /// What the fuzzy pattern runs over: the name, the path, then the tags (so typing a
    /// tag's start finds it: the "tag prefix" tier).
    pub fn haystack(&self) -> String {
        format!("{}  {}  {}", self.name, self.detail, self.tags.join(" "))
    }

    /// wtm's tiers: 0 name prefix, 1 tag prefix, 2 substring, 3 fuzzy.
    fn tier(&self, text: &str) -> u8 {
        let name = self.name.to_lowercase();
        if name.starts_with(text) {
            0
        } else if self.tags.iter().any(|t| t.starts_with(text)) {
            1
        } else if name.contains(text) || self.detail.to_lowercase().contains(text) {
            2
        } else {
            3
        }
    }
}

/// The launcher with no query (wtm's browse view): the five most used ("frequent", when
/// at least two have been used and there are groups), then the untagged, then each
/// first tag A→Z under its header. Within a group: most used, then A→Z. A frequent item
/// also stays in its group, as in wtm.
pub fn browse_order(items: Vec<Arc<PickItem>>) -> (Vec<Arc<PickItem>>, Vec<(usize, String)>) {
    // The "+ …" rows go last, under their own header, in the order given.
    let (actions, items): (Vec<_>, Vec<_>) =
        items.into_iter().partition(|i| i.kind == ItemKind::Action);
    let (mut out, mut headers) = browse_items(items);
    if !actions.is_empty() {
        headers.push((out.len(), "actions".to_string()));
        out.extend(actions);
    }
    (out, headers)
}

fn browse_items(mut items: Vec<Arc<PickItem>>) -> (Vec<Arc<PickItem>>, Vec<(usize, String)>) {
    let group = |i: &PickItem| i.tags.first().cloned().unwrap_or_default();
    items.sort_by_cached_key(|i| (group(i), std::cmp::Reverse(i.score), i.name.to_lowercase()));
    let has_groups = items.iter().any(|i| !i.tags.is_empty());
    let mut out: Vec<Arc<PickItem>> = Vec::new();
    let mut headers: Vec<(usize, String)> = Vec::new();
    let mut top: Vec<Arc<PickItem>> = items.iter().filter(|i| i.score > 0).cloned().collect();
    top.sort_by_cached_key(|i| (std::cmp::Reverse(i.score), i.name.to_lowercase()));
    top.truncate(5);
    if has_groups && top.len() >= 2 {
        headers.push((0, "frequent".to_string()));
        out.extend(top);
    }
    let mut last: Option<String> = None;
    for it in items {
        let g = group(&it);
        if has_groups && last.as_deref() != Some(g.as_str()) {
            let title = if g.is_empty() {
                "untagged".to_string()
            } else {
                format!("#{g}")
            };
            headers.push((out.len(), title));
            last = Some(g);
        }
        out.push(it);
    }
    (out, headers)
}

/// What a picker lists, which decides what its keys do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Pins and saved views (`<leader>p`).
    Pins,
    /// Tags with their counts (`<leader>t`, `:tags`): Tab chooses, Enter filters.
    Tags,
    /// Machines (`<leader>m`): pinned, recent, inventory CSV, `source_cmd`.
    Machines,
    /// The launcher (`<leader><leader>`): every pin, ranked with frecency; with no
    /// query, the most used on top, then groups by first tag.
    Launcher,
    /// `<leader>ff`: files below the cwd, streamed in by the walker.
    Files,
    /// `<leader>fr`: folders by frecency (every visit counts), and the cwd's subfolders.
    Dirs,
    /// `<leader>/`: file contents; the text is the search (re-walked as you type).
    Grep,
    /// `<leader>:` every action and `:` command; `<leader>sk` every binding.
    Commands,
    Keymaps,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerMode {
    Insert,
    Normal,
}

pub struct Picker {
    pub title: &'static str,
    pub source: Source,
    /// Rows chosen with Tab (their ids), in the order chosen.
    pub chosen: Vec<usize>,
    nucleo: Nucleo<Arc<PickItem>>,
    /// Everything the source gave, before filters.
    items: Vec<Arc<PickItem>>,
    pub editor: LineEditor,
    pub mode: PickerMode,
    pub query: PinQuery,
    /// The matches in display order (capped at [`RANK_CAP`], then nucleo's order).
    pub view: Vec<Arc<PickItem>>,
    pub matched: usize,
    pub running: bool,
    /// The source is still producing (a walk): spinner and `matched / scanned`.
    pub walking: bool,
    pub scanned: u64,
    /// A line about the source: `network walk, depth 4 · ^l lifts it`.
    pub note: Option<String>,
    pub cursor: usize,
    pub offset: usize,
    /// Result rows at the last draw (for half-page moves).
    pub page: usize,
    /// Completion for the filter word being typed (`#av` → `#aveva`).
    pub completion: Option<Completion>,
    /// The most used tags with their counts, for the tag bar and `]t`.
    pub tag_bar: Vec<(String, usize)>,
    /// Section headers before rows (row index, title): the launcher's empty-query view.
    pub headers: Vec<(usize, String)>,
    is_counts: Vec<(String, usize)>,
    servers: Vec<(String, usize)>,
    /// The filter part of the query the matcher was last filled for.
    applied: Option<PinQuery>,
    /// The fuzzy text the pattern was last parsed from.
    last_text: String,
    dirty_view: bool,
    matcher: Matcher,
}

impl Picker {
    /// `notify` is called from the matcher threads when results change, to wake the UI.
    pub fn new(
        title: &'static str,
        source: Source,
        items: Vec<PickItem>,
        notify: Arc<dyn Fn() + Send + Sync>,
        query: &str,
    ) -> Picker {
        let items: Vec<Arc<PickItem>> = items.into_iter().map(Arc::new).collect();
        let mut tag_bar: Vec<(String, usize)> = Vec::new();
        let mut servers: Vec<(String, usize)> = Vec::new();
        for it in &items {
            for t in &it.tags {
                bump(&mut tag_bar, t);
            }
            if let Some(s) = &it.server {
                bump(&mut servers, s);
            }
        }
        tag_bar.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        servers.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let is_counts = query::IS_WORDS
            .iter()
            .map(|w| {
                let q = PinQuery::parse(&format!("is:{w}"));
                let n = items.iter().filter(|i| q.matches(&i.facts())).count();
                (w.to_string(), n)
            })
            .filter(|(_, n)| *n > 0)
            .collect();
        let mut p = Picker {
            title,
            source,
            chosen: Vec::new(),
            nucleo: Nucleo::new(Config::DEFAULT, notify, None, 1),
            items,
            editor: LineEditor::new(query),
            mode: PickerMode::Insert,
            query: PinQuery::default(),
            view: Vec::new(),
            matched: 0,
            running: false,
            walking: false,
            scanned: 0,
            note: None,
            cursor: 0,
            offset: 0,
            page: 10,
            completion: None,
            tag_bar,
            headers: Vec::new(),
            is_counts,
            servers,
            applied: None,
            last_text: String::new(),
            dirty_view: true,
            matcher: Matcher::new(Config::DEFAULT),
        };
        p.text_changed();
        p
    }

    pub fn total(&self) -> usize {
        self.items.len()
    }

    /// After every edit of the query: refill the matcher if the filters changed, then
    /// reparse the fuzzy pattern.
    pub fn text_changed(&mut self) {
        let text = self.editor.text();
        self.query = PinQuery::parse(&text);
        self.completion = query::complete(&text, &self.tag_bar, &self.is_counts, &self.servers);
        let filters = PinQuery {
            text: String::new(),
            problems: Vec::new(),
            ..self.query.clone()
        };
        let refill = self.applied.as_ref() != Some(&filters);
        if refill {
            self.nucleo.restart(true);
            let injector = self.nucleo.injector();
            for it in self.items.iter().filter(|i| filters.matches(&i.facts())) {
                injector.push(it.clone(), |it, cols| cols[0] = it.haystack().into());
            }
            self.applied = Some(filters);
        }
        // Grep: the text is the search itself (the walk re-runs); nothing to filter by.
        let grep_text = String::new();
        let new = if self.source == Source::Grep {
            &grep_text
        } else {
            &self.query.text
        };
        let append = !refill && new.starts_with(self.last_text.as_str());
        self.nucleo
            .pattern
            .reparse(0, new, CaseMatching::Smart, Normalization::Smart, append);
        self.last_text = new.clone();
        self.cursor = 0;
        self.offset = 0;
        self.dirty_view = true;
    }

    /// Drops every row (grep starts a new walk); the text stays.
    pub fn reset_items(&mut self) {
        self.items.clear();
        self.nucleo.restart(true);
        self.applied = None;
        self.scanned = 0;
        self.cursor = 0;
        self.offset = 0;
        self.dirty_view = true;
        self.text_changed();
    }

    /// Takes the matcher's latest results. True when the view changed.
    pub fn tick(&mut self) -> bool {
        let status = self.nucleo.tick(10);
        self.running = status.running;
        if !(status.changed || self.dirty_view) {
            return false;
        }
        self.dirty_view = false;
        let snap = self.nucleo.snapshot();
        self.matched = snap.matched_item_count() as usize;
        let n = snap.matched_item_count().min(RANK_CAP);
        let mut view: Vec<Arc<PickItem>> =
            snap.matched_items(..n).map(|i| i.data.clone()).collect();
        let text = self.query.text.to_lowercase();
        self.headers.clear();
        if !text.is_empty() {
            // Within a tier: frecency, then (launcher) A→Z; otherwise nucleo's order
            // stands (the sort is stable).
            if self.source == Source::Launcher {
                // "+ …" rows only for a real match (`pin`, `wtm`), never a loose fuzzy
                // one: typed text that matches nothing else still runs on Enter.
                view.retain(|i| i.kind != ItemKind::Action || i.tier(&text) < 3);
                view.sort_by_cached_key(|i| {
                    (
                        i.tier(&text),
                        std::cmp::Reverse(i.score),
                        i.name.to_lowercase(),
                    )
                });
            } else {
                view.sort_by_key(|i| (i.tier(&text), std::cmp::Reverse(i.score)));
            }
        } else if self.source == Source::Launcher {
            (view, self.headers) = browse_order(view);
        }
        self.view = view;
        self.cursor = self.cursor.min(self.view.len().saturating_sub(1));
        true
    }

    /// Rows found after the picker opened (a slow source): kept, and matched when they
    /// pass the current filters.
    pub fn add(&mut self, items: Vec<PickItem>) {
        let injector = self.nucleo.injector();
        for it in items {
            let it = Arc::new(it);
            if self.applied.as_ref().is_none_or(|f| f.matches(&it.facts())) {
                injector.push(it.clone(), |it, cols| cols[0] = it.haystack().into());
            }
            self.items.push(it);
        }
        self.dirty_view = true;
    }

    pub fn selected(&self) -> Option<&Arc<PickItem>> {
        self.view.get(self.cursor)
    }

    /// Tab (multi-select): choose or unchoose the row under the cursor, then move down.
    pub fn toggle_chosen(&mut self) {
        let Some(id) = self.selected().map(|i| i.id) else {
            return;
        };
        match self.chosen.iter().position(|c| *c == id) {
            Some(i) => {
                self.chosen.remove(i);
            }
            None => self.chosen.push(id),
        }
        self.move_cursor(1);
    }

    pub fn is_chosen(&self, id: usize) -> bool {
        self.chosen.contains(&id)
    }

    pub fn move_cursor(&mut self, delta: isize) {
        if self.view.is_empty() {
            return;
        }
        let last = self.view.len() - 1;
        self.cursor = self.cursor.saturating_add_signed(delta).min(last);
    }

    pub fn set_cursor(&mut self, idx: usize) {
        self.cursor = idx.min(self.view.len().saturating_sub(1));
    }

    /// Tab: take the first completion candidate.
    pub fn accept_completion(&mut self) -> bool {
        let Some(c) = self.completion.take() else {
            return false;
        };
        let Some((word, _)) = c.candidates.first() else {
            return false;
        };
        self.editor = LineEditor::new(&format!("{}{word} ", c.head));
        self.text_changed();
        true
    }

    /// `]t` / `[t`: replace the tag filters with the next / previous tag of the bar.
    pub fn cycle_tag(&mut self, forward: bool) {
        if self.tag_bar.is_empty() {
            return;
        }
        let len = self.tag_bar.len();
        let next = match self
            .query
            .single_tag()
            .and_then(|t| self.tag_bar.iter().position(|(n, _)| n == t))
        {
            Some(i) if forward => (i + 1) % len,
            Some(i) => (i + len - 1) % len,
            None if forward => 0,
            None => len - 1,
        };
        let text = self.editor.text();
        let mut words: Vec<&str> = text
            .split_whitespace()
            .filter(|w| !w.starts_with('#'))
            .collect();
        let tag = format!("#{}", self.tag_bar[next].0);
        words.push(&tag);
        self.editor = LineEditor::new(&words.join(" "));
        self.text_changed();
    }

    /// Positions in `item.haystack()` (in chars) the fuzzy pattern matched, for
    /// highlighting the visible rows.
    pub fn match_indices(&mut self, item: &PickItem) -> Vec<u32> {
        let hay = item.haystack();
        let mut buf = Vec::new();
        let mut indices = Vec::new();
        let pattern = self.nucleo.pattern.column_pattern(0);
        pattern.indices(
            Utf32Str::new(&hay, &mut buf),
            &mut self.matcher,
            &mut indices,
        );
        indices.sort_unstable();
        indices.dedup();
        indices
    }
}

fn bump(counts: &mut Vec<(String, usize)>, name: &str) {
    match counts.iter_mut().find(|(n, _)| n == name) {
        Some((_, c)) => *c += 1,
        None => counts.push((name.to_string(), 1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn item(id: usize, name: &str, detail: &str, kind: PinKind, tags: &[&str]) -> PickItem {
        PickItem {
            id,
            name: name.to_string(),
            detail: detail.to_string(),
            kind: ItemKind::Pin(kind),
            key: None,
            tags: tags.iter().map(|t| t.to_string()).collect(),
            unc: detail.starts_with(r"\\"),
            local: !detail.starts_with(r"\\"),
            reachable: None,
            server: detail
                .strip_prefix(r"\\")
                .and_then(|r| r.split('\\').next())
                .map(str::to_lowercase),
            score: 0,
        }
    }

    fn scored(name: &str, tags: &[&str], score: u64) -> PickItem {
        PickItem {
            score,
            ..item(0, name, "", PinKind::App, tags)
        }
    }

    fn row_names(v: &[Arc<PickItem>]) -> Vec<&str> {
        v.iter().map(|i| i.name.as_str()).collect()
    }

    #[test]
    fn launcher_ranks_tier_then_frecency_then_name() {
        let items = vec![
            scored("grafana staging", &[], 0),
            scored("grafana", &[], 50),
            scored("my grafana", &[], 900),
            scored("graphs", &[], 10),
            scored("zebra", &["grafana"], 0),
        ];
        let mut p = Picker::new("Launcher", Source::Launcher, items, Arc::new(|| {}), "gra");
        let end = Instant::now() + Duration::from_secs(5);
        while (p.view.len() < 5 || p.running) && Instant::now() < end {
            p.tick();
        }
        // Name prefix (by frecency, then A→Z) › tag prefix › substring (even if used most).
        assert_eq!(
            row_names(&p.view),
            [
                "grafana",
                "graphs",
                "grafana staging",
                "zebra",
                "my grafana"
            ]
        );
        assert!(p.headers.is_empty(), "no headers while searching");
    }

    #[test]
    fn launcher_browse_view_has_frequent_then_groups() {
        let items: Vec<Arc<PickItem>> = vec![
            scored("E3D HIT", &["aveva", "acme"], 40),
            scored("Admin", &["aveva"], 0),
            scored("Grafana", &["monitoring"], 300),
            scored("notes", &[], 0),
            scored("Teams", &[], 5),
        ]
        .into_iter()
        .map(Arc::new)
        .collect();
        let (view, headers) = browse_order(items);
        assert_eq!(
            row_names(&view),
            [
                "Grafana", "E3D HIT", "Teams", "Teams", "notes", "E3D HIT", "Admin", "Grafana"
            ]
        );
        assert_eq!(
            headers,
            [
                (0, "frequent".to_string()),
                (3, "untagged".to_string()),
                (5, "#aveva".to_string()),
                (7, "#monitoring".to_string())
            ]
        );
        // No tags at all: one flat list, no headers.
        let flat: Vec<Arc<PickItem>> = vec![scored("b", &[], 0), scored("a", &[], 3)]
            .into_iter()
            .map(Arc::new)
            .collect();
        let (view, headers) = browse_order(flat);
        assert_eq!(row_names(&view), ["a", "b"]);
        assert!(headers.is_empty());
    }

    fn picker(query: &str) -> Picker {
        let items = vec![
            item(
                0,
                "HIT (server)",
                r"\\srv01\project\HIT",
                PinKind::Folder,
                &["aveva", "servers"],
            ),
            item(
                1,
                "HIT (local)",
                r"D:\aveva\local\HIT",
                PinKind::Folder,
                &["aveva"],
            ),
            item(
                2,
                "AutoCAD",
                r"C:\acad\acad.exe",
                PinKind::App,
                &["autodesk"],
            ),
            item(
                3,
                "grafana",
                "https://grafana",
                PinKind::Web,
                &["acme", "monitoring"],
            ),
            item(
                4,
                "Archive",
                r"\\nas02\archive",
                PinKind::Folder,
                &["aveva", "archive"],
            ),
            item(5, "Specs with hit", r"C:\docs", PinKind::Folder, &["docs"]),
        ];
        let mut p = Picker::new("Pins", Source::Pins, items, Arc::new(|| {}), query);
        settle(&mut p);
        p
    }

    /// Ticks until the matcher has finished.
    fn settle(p: &mut Picker) {
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            p.tick();
            if !p.running || Instant::now() > end {
                break;
            }
        }
    }

    fn names(p: &Picker) -> Vec<&str> {
        p.view.iter().map(|i| i.name.as_str()).collect()
    }

    fn typed(p: &mut Picker, text: &str) {
        p.editor = LineEditor::new(text);
        p.text_changed();
        settle(p);
    }

    #[test]
    fn empty_query_lists_everything_in_order() {
        let p = picker("");
        assert_eq!(p.matched, 6);
        assert_eq!(p.total(), 6);
        assert_eq!(names(&p)[0], "HIT (server)");
    }

    #[test]
    fn tiers_name_prefix_first_then_substring() {
        let mut p = picker("");
        typed(&mut p, "hit");
        let n = names(&p);
        assert!(n[0].starts_with("HIT") && n[1].starts_with("HIT"), "{n:?}");
        assert_eq!(n[2], "Specs with hit", "substring after prefix");
        assert_eq!(n[3..], ["grafana"], "scattered fuzzy hit comes last");
        typed(&mut p, "monit");
        assert_eq!(names(&p), ["grafana"], "tag prefix finds it");
    }

    #[test]
    fn filters_narrow_before_matching() {
        let mut p = picker("");
        typed(&mut p, "#aveva");
        assert_eq!(names(&p).len(), 3);
        typed(&mut p, "#aveva is:unc");
        assert_eq!(names(&p), ["HIT (server)", "Archive"]);
        typed(&mut p, "#aveva !#archive hit");
        assert_eq!(names(&p).len(), 2);
        typed(&mut p, "on:srv0");
        assert_eq!(names(&p), ["HIT (server)"]);
        typed(&mut p, "is:app,web");
        assert!(p.query.problems.len() == 1, "is: takes one word");
        typed(&mut p, "#aveva,acme is:local");
        assert_eq!(names(&p), ["HIT (local)", "grafana"]);
    }

    #[test]
    fn tag_bar_completion_and_cycling() {
        let mut p = picker("");
        assert_eq!(p.tag_bar[0], ("aveva".to_string(), 3), "most used first");
        typed(&mut p, "x #ar");
        assert!(p.accept_completion());
        assert_eq!(p.editor.text(), "x #archive ");
        settle(&mut p);
        assert_eq!(names(&p).len(), 0, "x matches nothing tagged archive");

        typed(&mut p, "hit");
        p.cycle_tag(true);
        assert_eq!(p.editor.text(), "hit #aveva");
        p.cycle_tag(true);
        assert_eq!(p.query.single_tag(), Some(p.tag_bar[1].0.as_str()));
        p.cycle_tag(false);
        p.cycle_tag(false);
        assert_eq!(
            p.query.single_tag(),
            p.tag_bar.last().map(|t| t.0.as_str()),
            "wraps"
        );
    }

    #[test]
    fn match_indices_for_highlighting() {
        let mut p = picker("");
        typed(&mut p, "hit");
        let Some(first) = p.view.first().cloned() else {
            panic!()
        };
        let idx = p.match_indices(&first);
        assert_eq!(idx.len(), 3, "{idx:?}");
    }
}
