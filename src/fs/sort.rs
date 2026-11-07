//! How a folder is ordered (Yazi's `,` keys): by name (natural, as listed), date
//! modified, date created, size or extension, each reversible. Folders always come
//! first; ties keep the name order. Pure: it reorders the view's indices, comparing
//! numbers and short strings only (a 50 000-entry folder takes milliseconds).

use super::Entry;
use std::cmp::Ordering;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortKey {
    /// Natural name order (`2` before `10`): how listings arrive.
    #[default]
    Name,
    Modified,
    Created,
    Size,
    Ext,
    /// Search results only: best match first.
    Relevance,
}

impl SortKey {
    pub fn word(self) -> &'static str {
        match self {
            SortKey::Name => "name",
            SortKey::Modified => "modified",
            SortKey::Created => "created",
            SortKey::Size => "size",
            SortKey::Ext => "ext",
            SortKey::Relevance => "relevance",
        }
    }

    pub fn parse(s: &str) -> Option<SortKey> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "name" | "n" | "natural" | "path" => SortKey::Name,
            "modified" | "m" | "mtime" | "date" => SortKey::Modified,
            "created" | "c" | "ctime" => SortKey::Created,
            "size" | "s" => SortKey::Size,
            "ext" | "e" | "extension" | "type" => SortKey::Ext,
            "relevance" | "r" | "score" => SortKey::Relevance,
            _ => return None,
        })
    }

    /// Dates and sizes read newest / biggest first; names and extensions A → Z.
    fn descending(self) -> bool {
        matches!(self, SortKey::Modified | SortKey::Created | SortKey::Size)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DirSort {
    pub key: SortKey,
    /// The other way round (`,M`, `,S`…).
    pub reverse: bool,
}

/// `report.FINAL.pdf` → `pdf`; folders and dot-files have none.
fn ext(e: &Entry) -> String {
    match e.name.rsplit_once('.') {
        Some((stem, x)) if !stem.is_empty() && !e.is_container() => x.to_lowercase(),
        _ => String::new(),
    }
}

impl DirSort {
    /// Newest / biggest / A→Z first, unless reversed.
    pub fn descending(&self) -> bool {
        self.key.descending() != self.reverse
    }

    /// `name ↑`, `modified ↓`, `size ↓`.
    pub fn label(&self) -> String {
        format!(
            "{} {}",
            self.key.word(),
            if self.descending() { "↓" } else { "↑" }
        )
    }

    /// What the order means, for the status line: `newest first`, `Z → A`.
    pub fn meaning(&self) -> &'static str {
        match (self.key, self.descending()) {
            (SortKey::Modified | SortKey::Created, true) => "newest first",
            (SortKey::Modified | SortKey::Created, false) => "oldest first",
            (SortKey::Size, true) => "biggest first",
            (SortKey::Size, false) => "smallest first",
            (SortKey::Relevance, _) => "best match first",
            (_, false) => "A → Z",
            (_, true) => "Z → A",
        }
    }

    /// Reorders `view` (indices into `entries`, which are in name order with folders
    /// first). Folders stay first; equal keys keep the name order.
    pub fn apply(&self, entries: &[Entry], view: &mut [usize]) {
        let file = |i: usize| !entries[i].is_container();
        match self.key {
            SortKey::Name | SortKey::Relevance => {
                if self.reverse {
                    // Each group backwards, folders still first.
                    let split = view.partition_point(|&i| !file(i));
                    view[..split].reverse();
                    view[split..].reverse();
                }
            }
            key => {
                let desc = self.descending();
                let exts: Vec<String> = if key == SortKey::Ext {
                    entries.iter().map(ext).collect()
                } else {
                    Vec::new()
                };
                view.sort_by(|&a, &b| {
                    let (ea, eb) = (&entries[a], &entries[b]);
                    let by_group = file(a).cmp(&file(b));
                    if by_group != Ordering::Equal {
                        return by_group;
                    }
                    let o = match key {
                        SortKey::Modified => ea.modified.cmp(&eb.modified),
                        SortKey::Created => ea.created.cmp(&eb.created),
                        // A folder's size isn't known: folders keep their name order.
                        SortKey::Size if ea.is_container() => Ordering::Equal,
                        SortKey::Size => ea.size.cmp(&eb.size),
                        SortKey::Ext => exts[a].cmp(&exts[b]),
                        SortKey::Name | SortKey::Relevance => Ordering::Equal,
                    };
                    let o = if desc { o.reverse() } else { o };
                    // Ties: the name order the listing came in.
                    o.then(a.cmp(&b))
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::EntryKind;
    use std::time::{Duration, SystemTime};

    fn entries() -> Vec<Entry> {
        let t = |d: u64| SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(d * 86_400));
        let mk = |name: &str, kind: EntryKind, size: u64, m: u64, c: u64| {
            let mut e = Entry::virtual_entry(name.to_string(), kind, String::new());
            e.size = size;
            e.modified = t(m);
            e.created = t(c);
            e
        };
        // As listed: folders first, natural name order.
        vec![
            mk("a-old", EntryKind::Dir, 0, 1, 1),
            mk("b-new", EntryKind::Dir, 0, 9, 9),
            mk("B-1001.pdf", EntryKind::File, 300, 5, 2),
            mk("notes.txt", EntryKind::File, 10, 7, 7),
            mk("plan.dwg", EntryKind::File, 900, 3, 8),
            mk("README", EntryKind::File, 50, 6, 3),
        ]
    }

    fn order(s: DirSort) -> Vec<String> {
        let e = entries();
        let mut v: Vec<usize> = (0..e.len()).collect();
        s.apply(&e, &mut v);
        v.into_iter().map(|i| e[i].name.clone()).collect()
    }

    fn by(key: SortKey, reverse: bool) -> DirSort {
        DirSort { key, reverse }
    }

    #[test]
    fn keys_directions_and_folders_first() {
        assert_eq!(
            order(DirSort::default()),
            [
                "a-old",
                "b-new",
                "B-1001.pdf",
                "notes.txt",
                "plan.dwg",
                "README"
            ]
        );
        assert_eq!(
            order(by(SortKey::Name, true)),
            [
                "b-new",
                "a-old",
                "README",
                "plan.dwg",
                "notes.txt",
                "B-1001.pdf"
            ],
            "reversed, folders still first"
        );
        assert_eq!(
            order(by(SortKey::Modified, false)),
            [
                "b-new",
                "a-old",
                "notes.txt",
                "README",
                "B-1001.pdf",
                "plan.dwg"
            ],
            "newest first"
        );
        assert_eq!(
            order(by(SortKey::Modified, true)),
            [
                "a-old",
                "b-new",
                "plan.dwg",
                "B-1001.pdf",
                "README",
                "notes.txt"
            ]
        );
        assert_eq!(
            order(by(SortKey::Created, false))[2..],
            ["plan.dwg", "notes.txt", "README", "B-1001.pdf"]
        );
        assert_eq!(
            order(by(SortKey::Size, false)),
            [
                "a-old",
                "b-new",
                "plan.dwg",
                "B-1001.pdf",
                "README",
                "notes.txt"
            ],
            "biggest first; folders keep their name order"
        );
        assert_eq!(
            order(by(SortKey::Size, true))[2..],
            ["notes.txt", "README", "B-1001.pdf", "plan.dwg"]
        );
        assert_eq!(
            order(by(SortKey::Ext, false))[2..],
            ["README", "plan.dwg", "B-1001.pdf", "notes.txt"],
            "no extension first, then dwg pdf txt"
        );
    }

    #[test]
    fn labels_and_words() {
        assert_eq!(DirSort::default().label(), "name ↑");
        assert_eq!(by(SortKey::Modified, false).label(), "modified ↓");
        assert_eq!(by(SortKey::Modified, false).meaning(), "newest first");
        assert_eq!(by(SortKey::Size, true).meaning(), "smallest first");
        assert_eq!(by(SortKey::Name, true).meaning(), "Z → A");
        for k in [
            SortKey::Name,
            SortKey::Modified,
            SortKey::Created,
            SortKey::Size,
            SortKey::Ext,
            SortKey::Relevance,
        ] {
            assert_eq!(SortKey::parse(k.word()), Some(k));
        }
        assert_eq!(SortKey::parse("date"), Some(SortKey::Modified));
        assert_eq!(SortKey::parse("colour"), None);
    }

    /// The view is rebuilt on every filter keystroke: sorting 50 000 entries must stay
    /// well under a frame even unoptimized.
    #[test]
    fn fifty_thousand_entries_sort_quickly() {
        let mut big = Vec::new();
        for i in 0..50_000u64 {
            let mut e = Entry::virtual_entry(format!("f{i}.dat"), EntryKind::File, String::new());
            e.size = (i * 7919) % 100_000;
            e.modified = SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(i * 13 % 99_991));
            big.push(e);
        }
        for key in [SortKey::Size, SortKey::Modified, SortKey::Ext] {
            let mut v: Vec<usize> = (0..big.len()).collect();
            let t = std::time::Instant::now();
            by(key, false).apply(&big, &mut v);
            let took = t.elapsed();
            assert!(took < Duration::from_millis(400), "{key:?}: {took:?}");
        }
    }
}
