//! Filesystem model: entries, listings, sorting. All I/O goes through [`worker`].

pub mod path;
pub mod worker;

use crate::platform::{Attrs, DriveKind};
use std::cmp::Ordering;
use std::time::{Instant, SystemTime};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Dir,
    File,
    /// A drive in the Locations root.
    Drive(DriveKind),
    /// A share: in a server node's listing (named `share`), or a saved share in the
    /// Locations root (named `\\server\share`).
    Share,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub kind: EntryKind,
    /// Symlink or junction (not followed while listing).
    pub is_link: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub created: Option<SystemTime>,
    pub attrs: Attrs,
    /// Extra text for virtual entries: a drive's volume label or mapped UNC target, a
    /// share's remark. Empty for files and directories.
    pub detail: String,
}

impl Entry {
    /// An entry for a virtual listing (drives, shares): no size, dates or attributes.
    pub fn virtual_entry(name: String, kind: EntryKind, detail: String) -> Entry {
        Entry {
            name,
            kind,
            is_link: false,
            size: 0,
            modified: None,
            created: None,
            attrs: Attrs::default(),
            detail,
        }
    }

    /// Something you can enter with `l`.
    pub fn is_container(&self) -> bool {
        !matches!(self.kind, EntryKind::File)
    }

    /// Hidden by the `.` toggle: Windows hidden or system attribute.
    pub fn is_hidden(&self) -> bool {
        self.attrs.hidden || self.attrs.system
    }
}

#[derive(Debug)]
pub struct Listing {
    /// Sorted: containers first, then natural, case-insensitive name order.
    pub entries: Vec<Entry>,
    pub loaded_at: Instant,
}

pub fn sort_entries(entries: &mut [Entry]) {
    entries.sort_by(|a, b| {
        b.is_container()
            .cmp(&a.is_container())
            .then_with(|| natural_cmp(&a.name, &b.name))
    });
}

/// Case-insensitive natural order: `file2` < `file10`, like Explorer.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let na: String = std::iter::from_fn(|| a.next_if(char::is_ascii_digit)).collect();
                let nb: String = std::iter::from_fn(|| b.next_if(char::is_ascii_digit)).collect();
                let (ta, tb) = (na.trim_start_matches('0'), nb.trim_start_matches('0'));
                // Longer digit run = bigger number; same length compares lexically.
                let ord = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if ord != Ordering::Equal {
                    return ord;
                }
                // `01` vs `1`: equal value, fewer leading zeros first, for a stable order.
                let zeros = na.len().cmp(&nb.len());
                if zeros != Ordering::Equal {
                    return zeros;
                }
            }
            (Some(x), Some(y)) => {
                let ord = x.to_lowercase().cmp(y.to_lowercase());
                if ord != Ordering::Equal {
                    return ord;
                }
                a.next();
                b.next();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_order_table() {
        let cases: &[(&str, &str, Ordering)] = &[
            ("a", "b", Ordering::Less),
            ("A", "a", Ordering::Equal),
            ("file2", "file10", Ordering::Less),
            ("file10", "file9", Ordering::Greater),
            ("x01", "x1", Ordering::Greater),
            ("x001", "x2", Ordering::Less),
            ("abc", "abcd", Ordering::Less),
            ("Zeta", "alpha", Ordering::Greater),
            ("v1.10.0", "v1.9.0", Ordering::Greater),
            (
                "99999999999999999999999",
                "100000000000000000000000",
                Ordering::Less,
            ),
        ];
        for (a, b, want) in cases {
            assert_eq!(natural_cmp(a, b), *want, "{a} vs {b}");
        }
    }

    #[test]
    fn dirs_sort_first() {
        let mk = |name: &str, kind| Entry {
            name: name.to_string(),
            kind,
            is_link: false,
            size: 0,
            modified: None,
            created: None,
            attrs: Attrs::default(),
            detail: String::new(),
        };
        let mut v = vec![
            mk("b.txt", EntryKind::File),
            mk("zdir", EntryKind::Dir),
            mk("a.txt", EntryKind::File),
            mk("Adir", EntryKind::Dir),
        ];
        sort_entries(&mut v);
        let names: Vec<_> = v.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Adir", "zdir", "a.txt", "b.txt"]);
    }
}
