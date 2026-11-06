//! Applying a bulk rename, the journal, and undo (CLAUDE.md section 16).
//!
//! - [`apply`]: two-phase (everything to a temporary name first, then to its final name),
//!   so swaps, cycles and case-only renames work. Items are done **deepest first**, level
//!   by level, so with subfolders included a file is renamed inside its folder before the
//!   folder itself moves. Every pair renames within its *current* parent.
//! - The journal: every applied batch is a small JSON file in
//!   `%LOCALAPPDATA%\sentier\rename-journal\`, holding where each item is now and its old
//!   name. Undo renames each one back, deepest first again, so it's the same algorithm.
//!
//! Blocking; only the worker calls it.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// How a batch went.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applied {
    /// (old full path, new full path) of every item renamed, as asked (the new path
    /// under the item's parent as it was when the batch started).
    pub done: Vec<(String, String)>,
    /// `name: why` for every item that kept its name.
    pub failed: Vec<String>,
}

fn name_of(path: &str) -> &str {
    path.rsplit('\\').next().unwrap_or(path)
}

fn parent_of(path: &str) -> &str {
    path.rsplit_once('\\').map(|(p, _)| p).unwrap_or("")
}

fn depth(path: &str) -> usize {
    path.trim_end_matches('\\').matches('\\').count()
}

/// Renames every pair, deepest first, each level in two phases. A failure leaves that
/// item under its old name; the others still go through.
pub fn apply(pairs: &[(String, String)]) -> Applied {
    let stamp = std::process::id();
    let mut order: Vec<usize> = (0..pairs.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(depth(&pairs[i].0)));
    let mut out = Applied::default();
    let mut start = 0;
    while start < order.len() {
        let d = depth(&pairs[order[start]].0);
        let end = order[start..]
            .iter()
            .position(|&i| depth(&pairs[i].0) != d)
            .map_or(order.len(), |n| start + n);
        let level = &order[start..end];
        // Phase 1: to temporary names.
        let mut moved: Vec<(usize, String)> = Vec::new();
        for (n, &i) in level.iter().enumerate() {
            let from = &pairs[i].0;
            let tmp = format!("{from}.sentier-{stamp}-{d}-{n}.tmp");
            match std::fs::rename(from, &tmp) {
                Ok(()) => moved.push((i, tmp)),
                Err(e) => out.failed.push(format!(
                    "{}: {}",
                    name_of(from),
                    crate::fs::ops::os_message(&e)
                )),
            }
        }
        // Phase 2: to the final names.
        for (i, tmp) in moved {
            let (from, to) = &pairs[i];
            let r = if Path::new(to).exists() {
                Err(format!("{} already exists", name_of(to)))
            } else {
                std::fs::rename(&tmp, to).map_err(|e| crate::fs::ops::os_message(&e))
            };
            match r {
                Ok(()) => out.done.push((from.clone(), to.clone())),
                Err(e) => {
                    let _ = std::fs::rename(&tmp, from);
                    out.failed.push(format!("{}: {e}", name_of(from)));
                }
            }
        }
        start = end;
    }
    out
}

/// One renamed item in the journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    /// Where it is now (full path, its renamed folders taken into account).
    pub now: String,
    /// Its name before the batch.
    pub was: String,
}

/// One applied batch.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Batch {
    /// Unix seconds.
    pub time: u64,
    pub items: Vec<Item>,
}

impl Batch {
    /// The journal entry for what [`apply`] did: each item's real place now, following
    /// the folders renamed in the same batch.
    pub fn from_done(done: &[(String, String)], time: u64) -> Batch {
        let by_from: HashMap<String, usize> = done
            .iter()
            .enumerate()
            .map(|(i, (from, _))| (from.to_lowercase(), i))
            .collect();
        fn now_of(
            i: usize,
            done: &[(String, String)],
            by_from: &HashMap<String, usize>,
            hops: usize,
        ) -> String {
            let to = &done[i].1;
            let parent = parent_of(to);
            match by_from.get(&parent.to_lowercase()) {
                Some(&p) if p != i && hops < 64 => {
                    format!("{}\\{}", now_of(p, done, by_from, hops + 1), name_of(to))
                }
                _ => to.clone(),
            }
        }
        Batch {
            time,
            items: (0..done.len())
                .map(|i| Item {
                    now: now_of(i, done, &by_from, 0),
                    was: name_of(&done[i].0).to_string(),
                })
                .collect(),
        }
    }

    /// Did it rename something in `dir` or below it? (For `u`, which works on the folder
    /// you're in.)
    pub fn touches(&self, dir: &str) -> bool {
        let dir = dir.trim_end_matches('\\').to_lowercase();
        let below = format!("{dir}\\");
        self.items.iter().any(|it| {
            let parent = parent_of(&it.now).to_lowercase();
            parent == dir || parent.starts_with(&below)
        })
    }

    /// The pairs that undo it: each item back to its old name, where it is now.
    pub fn undo_pairs(&self) -> Vec<(String, String)> {
        self.items
            .iter()
            .map(|it| {
                (
                    it.now.clone(),
                    format!("{}\\{}", parent_of(&it.now), it.was),
                )
            })
            .collect()
    }
}

/// A journal file's name (its id) and its batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub id: String,
    pub batch: Batch,
}

/// `%LOCALAPPDATA%\sentier\rename-journal\` (a temp folder in tests).
pub fn journal_dir() -> Option<PathBuf> {
    if cfg!(test) {
        return Some(
            std::env::temp_dir().join(format!("sentier-test-journal-{}", std::process::id())),
        );
    }
    Some(
        PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
            .join("sentier")
            .join("rename-journal"),
    )
}

/// Batches kept; older ones are dropped.
const KEEP: usize = 100;

/// Writes the batch as a new journal file (sorted names: newest last).
pub fn record(dir: &Path, batch: &Batch) -> Result<String, String> {
    if batch.items.is_empty() {
        return Ok(String::new());
    }
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let text = serde_json::to_string_pretty(batch).map_err(|e| e.to_string())?;
    let mut n = 0;
    loop {
        let id = format!("{:012}-{:06}-{n}", batch.time, std::process::id());
        let path = dir.join(format!("{id}.json"));
        if !path.exists() {
            std::fs::write(&path, text).map_err(|e| e.to_string())?;
            prune(dir);
            return Ok(id);
        }
        n += 1;
    }
}

/// Every batch, newest first. Unreadable files are skipped.
pub fn load(dir: &Path) -> Vec<Entry> {
    let mut ids: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    name.strip_suffix(".json").map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default();
    ids.sort_unstable_by(|a, b| b.cmp(a));
    ids.into_iter()
        .filter_map(|id| {
            let text = std::fs::read_to_string(dir.join(format!("{id}.json"))).ok()?;
            let batch: Batch = serde_json::from_str(&text).ok()?;
            Some(Entry { id, batch })
        })
        .collect()
}

/// After an undo: only the items that couldn't be reverted stay; an empty batch goes.
pub fn keep_only(dir: &Path, id: &str, batch: &Batch, left: &[String]) {
    let path = dir.join(format!("{id}.json"));
    let rest: Vec<Item> = batch
        .items
        .iter()
        .filter(|it| left.iter().any(|l| l.eq_ignore_ascii_case(&it.now)))
        .cloned()
        .collect();
    if rest.is_empty() {
        let _ = std::fs::remove_file(&path);
    } else if let Ok(text) = serde_json::to_string_pretty(&Batch {
        time: batch.time,
        items: rest,
    }) {
        let _ = std::fs::write(&path, text);
    }
}

fn prune(dir: &Path) {
    let all = load(dir);
    for e in all.iter().skip(KEEP) {
        let _ = std::fs::remove_file(dir.join(format!("{}.json", e.id)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(p: &Path) -> String {
        p.display().to_string()
    }

    #[test]
    fn nested_renames_and_their_undo() {
        let root = std::env::temp_dir().join(format!("sentier-apply-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("old").join("deep")).ok();
        std::fs::write(root.join("old").join("a.txt"), "a").ok();
        std::fs::write(root.join("old").join("deep").join("b.txt"), "b").ok();
        std::fs::write(root.join("x.txt"), "x").ok();
        std::fs::write(root.join("y.txt"), "y").ok();
        let j = |p: &[&str]| {
            let mut b = root.clone();
            for c in p {
                b = b.join(c);
            }
            s(&b)
        };
        // A folder, a file in it, a file two levels down, and a swap.
        let pairs = vec![
            (j(&["old"]), j(&["new"])),
            (j(&["old", "a.txt"]), j(&["old", "A1.txt"])),
            (j(&["old", "deep"]), j(&["old", "deeper"])),
            (j(&["old", "deep", "b.txt"]), j(&["old", "deep", "B1.txt"])),
            (j(&["x.txt"]), j(&["y.txt"])),
            (j(&["y.txt"]), j(&["x.txt"])),
        ];
        let r = apply(&pairs);
        assert!(r.failed.is_empty(), "{:?}", r.failed);
        assert_eq!(r.done.len(), 6);
        assert!(root.join("new").join("A1.txt").exists());
        assert!(root.join("new").join("deeper").join("B1.txt").exists());
        assert_eq!(
            std::fs::read_to_string(root.join("x.txt")).ok().as_deref(),
            Some("y")
        );
        let batch = Batch::from_done(&r.done, 7);
        let b1 = batch
            .items
            .iter()
            .find(|it| it.was == "b.txt")
            .map(|it| it.now.clone());
        assert_eq!(
            b1,
            Some(j(&["new", "deeper", "B1.txt"])),
            "follows its folders"
        );

        // The journal round-trips, newest first.
        let dir = root.join("journal");
        let first = record(&dir, &Batch::from_done(&r.done[..1], 5)).unwrap_or_default();
        let id = record(&dir, &batch).unwrap_or_default();
        let entries = load(&dir);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, id);
        assert_eq!(entries[1].id, first);

        // Undo puts everything back.
        let u = apply(&batch.undo_pairs());
        assert!(u.failed.is_empty(), "{:?}", u.failed);
        assert!(root.join("old").join("a.txt").exists());
        assert!(root.join("old").join("deep").join("b.txt").exists());
        assert_eq!(
            std::fs::read_to_string(root.join("x.txt")).ok().as_deref(),
            Some("x")
        );
        keep_only(&dir, &id, &batch, &[]);
        assert_eq!(load(&dir).len(), 1, "the undone batch is gone");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_failure_keeps_the_old_name_and_the_rest_goes_on() {
        let root = std::env::temp_dir().join(format!("sentier-apply-f-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).ok();
        std::fs::write(root.join("a.txt"), "a").ok();
        std::fs::write(root.join("taken.txt"), "t").ok();
        std::fs::write(root.join("c.txt"), "c").ok();
        let pairs = vec![
            (s(&root.join("a.txt")), s(&root.join("taken.txt"))),
            (s(&root.join("missing.txt")), s(&root.join("m.txt"))),
            (s(&root.join("c.txt")), s(&root.join("d.txt"))),
        ];
        let r = apply(&pairs);
        assert_eq!(r.done.len(), 1);
        assert_eq!(r.failed.len(), 2);
        assert!(
            r.failed
                .iter()
                .any(|f| f.contains("taken.txt already exists"))
        );
        assert!(root.join("a.txt").exists() && root.join("d.txt").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
