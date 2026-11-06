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
}

impl ItemKind {
    pub fn label(self) -> &'static str {
        match self {
            ItemKind::Pin(k) => k.label(),
            ItemKind::View => "view",
            ItemKind::Tag => "tag",
        }
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
}

impl PickItem {
    fn facts(&self) -> Facts<'_> {
        Facts {
            kind: match self.kind {
                ItemKind::Pin(k) => Some(k),
                _ => None,
            },
            view: self.kind == ItemKind::View,
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

/// What a picker lists, which decides what its keys do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Pins and saved views (`<leader>p`).
    Pins,
    /// Tags with their counts (`<leader>t`, `:tags`): Tab chooses, Enter filters.
    Tags,
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
    pub cursor: usize,
    pub offset: usize,
    /// Result rows at the last draw (for half-page moves).
    pub page: usize,
    /// Completion for the filter word being typed (`#av` → `#aveva`).
    pub completion: Option<Completion>,
    /// The most used tags with their counts, for the tag bar and `]t`.
    pub tag_bar: Vec<(String, usize)>,
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
            cursor: 0,
            offset: 0,
            page: 10,
            completion: None,
            tag_bar,
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
        let new = &self.query.text;
        let append = !refill && new.starts_with(self.last_text.as_str());
        self.nucleo
            .pattern
            .reparse(0, new, CaseMatching::Smart, Normalization::Smart, append);
        self.last_text = new.clone();
        self.cursor = 0;
        self.offset = 0;
        self.dirty_view = true;
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
        if !text.is_empty() {
            // Stable: within a tier, nucleo's score order stands.
            view.sort_by_key(|i| i.tier(&text));
        }
        self.view = view;
        self.cursor = self.cursor.min(self.view.len().saturating_sub(1));
        true
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
        }
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
