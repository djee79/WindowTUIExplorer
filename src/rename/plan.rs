//! The preview plan (CLAUDE.md section 16): every item's new name and status, with the
//! safety checks. Apply is refused while anything is blocked.
//!
//! - unchanged (`·`), excluded (Space, or a type the toggles leave out);
//! - blocked (`✕`): an invalid name (`< > : " / \ | ? *`, reserved, trailing dot or space,
//!   empty), two items ending on one name, or a name already taken in the folder by
//!   something outside the batch;
//! - warned (`⚠`): a path over 260 characters (allowed), a token with no value;
//! - case-only renames and swaps are fine: the apply goes through temporary names.
//!
//! Pure: the folder contents (`existing`) are passed in.

use super::engine::{self, DateSource, Matcher, Options};
use super::tokens::{Context, Names, Notes, Rng};
use std::collections::{HashMap, HashSet};
use std::time::SystemTime;

/// One item the panel works on.
#[derive(Debug, Clone, Default)]
pub struct Source {
    /// Its folder, full path.
    pub dir: String,
    pub name: String,
    pub is_dir: bool,
    pub created: Option<SystemTime>,
    pub modified: Option<SystemTime>,
    /// EXIF date taken.
    pub taken: Option<SystemTime>,
    /// Photo metadata (`CAMERA_MODEL` → …), once read.
    pub meta: Option<HashMap<String, String>>,
    pub pdf_title: Option<String>,
    /// Space in the panel: left out.
    pub excluded: bool,
}

impl Source {
    pub fn path(&self) -> String {
        format!("{}\\{}", self.dir.trim_end_matches('\\'), self.name)
    }

    /// The folder's own name, for `${parent}`.
    fn parent(&self) -> &str {
        self.dir
            .trim_end_matches('\\')
            .rsplit('\\')
            .next()
            .unwrap_or(&self.dir)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Rename,
    /// Renamed, with something to know (`long path`, `no $CAMERA_MODEL`).
    Warn(String),
    Unchanged,
    /// Space, or a type left out by the files / folders toggles.
    Excluded,
    /// Can't be renamed like this; Apply waits until it's fixed or excluded.
    Blocked(String),
}

impl Status {
    pub fn renames(&self) -> bool {
        matches!(self, Status::Rename | Status::Warn(_))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Index into the sources.
    pub source: usize,
    pub new_name: String,
    pub status: Status,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub rows: Vec<Row>,
    pub renames: usize,
    pub unchanged: usize,
    pub excluded: usize,
    pub blocked: usize,
}

impl Plan {
    /// (old full path, new full path) for every item that is renamed.
    pub fn pairs(&self, sources: &[Source]) -> Vec<(String, String)> {
        self.rows
            .iter()
            .filter(|r| r.status.renames())
            .filter_map(|r| {
                let s = sources.get(r.source)?;
                Some((
                    s.path(),
                    format!("{}\\{}", s.dir.trim_end_matches('\\'), r.new_name),
                ))
            })
            .collect()
    }
}

/// Lowercased names already in each folder (lowercased path), outside sentier's control.
pub type Existing = HashMap<String, HashSet<String>>;

/// Windows' classic limit; longer paths work with `\\?\` but many apps choke on them.
const LONG_PATH: usize = 260;

pub fn plan(
    sources: &[Source],
    search: &str,
    replace: &str,
    o: &Options,
    names: &Names,
    existing: &Existing,
    seed: u64,
) -> Result<Plan, String> {
    let matcher = Matcher::new(search, o)?;
    let rules = engine::Rules {
        matcher: &matcher,
        replace,
        options: o,
        names,
    };
    let mut rng = Rng::new(seed);
    let mut index = 0u64;
    let mut per_folder: HashMap<String, u64> = HashMap::new();
    let mut rows: Vec<Row> = Vec::with_capacity(sources.len());
    for (i, s) in sources.iter().enumerate() {
        let wanted = if s.is_dir { o.folders } else { o.files };
        if s.excluded || !wanted {
            rows.push(Row {
                source: i,
                new_name: s.name.clone(),
                status: Status::Excluded,
            });
            continue;
        }
        let in_folder = per_folder.entry(s.dir.to_lowercase()).or_insert(0);
        let (stem, ext) = engine::split_name(&s.name, s.is_dir);
        let when = match o.dates {
            DateSource::Created => s.created,
            DateSource::Modified => s.modified,
            DateSource::Taken => s.taken,
        };
        let cx = Context {
            index,
            index_in_folder: *in_folder,
            date: when.map(chrono::DateTime::<chrono::Local>::from),
            meta: s.meta.as_ref(),
            parent: s.parent(),
            stem,
            ext,
            pdf_title: s.pdf_title.as_deref(),
            enumerate: o.enumerate,
            random: o.random,
        };
        index += 1;
        *in_folder += 1;
        let mut notes = Notes::default();
        let new_name = engine::rename(&s.name, s.is_dir, &rules, &cx, &mut rng, &mut notes);
        let status = if new_name == s.name {
            Status::Unchanged
        } else if let Err(e) = crate::fs::ops::check_name(&new_name, false) {
            Status::Blocked(e)
        } else if s.dir.trim_end_matches('\\').len() + 1 + new_name.len() > LONG_PATH {
            Status::Warn("long path".into())
        } else if let Some(m) = notes.missing.first() {
            Status::Warn(format!("no {m} value"))
        } else {
            Status::Rename
        };
        rows.push(Row {
            source: i,
            new_name,
            status,
        });
    }
    check_collisions(sources, &mut rows, existing);
    let mut p = Plan {
        rows,
        ..Plan::default()
    };
    for r in &p.rows {
        match r.status {
            Status::Rename | Status::Warn(_) => p.renames += 1,
            Status::Unchanged => p.unchanged += 1,
            Status::Excluded => p.excluded += 1,
            Status::Blocked(_) => p.blocked += 1,
        }
    }
    Ok(p)
}

/// Two items ending on one name, or a renamed item landing on a name the folder already
/// has outside the batch: blocked.
fn check_collisions(sources: &[Source], rows: &mut [Row], existing: &Existing) {
    // Where every batch item ends up (its new name if renamed, else where it is).
    let mut finals: HashMap<(String, String), Vec<usize>> = HashMap::new();
    let mut originals: HashMap<String, HashSet<String>> = HashMap::new();
    for (k, r) in rows.iter().enumerate() {
        let s = &sources[r.source];
        let dir = s.dir.to_lowercase();
        originals
            .entry(dir.clone())
            .or_default()
            .insert(s.name.to_lowercase());
        let name = if r.status.renames() {
            &r.new_name
        } else {
            &s.name
        };
        finals
            .entry((dir, name.to_lowercase()))
            .or_default()
            .push(k);
    }
    let mut blocked: Vec<(usize, String)> = Vec::new();
    for ks in finals.values() {
        if ks.len() > 1 {
            for &k in ks {
                if rows[k].status.renames() {
                    let other = ks
                        .iter()
                        .find(|&&o| o != k)
                        .map(|&o| sources[rows[o].source].name.clone())
                        .unwrap_or_default();
                    blocked.push((k, format!("collides with {other}")));
                }
            }
        }
    }
    for (k, r) in rows.iter().enumerate() {
        if !r.status.renames() {
            continue;
        }
        let s = &sources[r.source];
        let dir = s.dir.to_lowercase();
        let new = r.new_name.to_lowercase();
        let ours = originals.get(&dir).is_some_and(|o| o.contains(&new));
        if !ours && existing.get(&dir).is_some_and(|e| e.contains(&new)) {
            blocked.push((k, format!("collides with existing {}", r.new_name)));
        }
    }
    for (k, why) in blocked {
        if !matches!(rows[k].status, Status::Blocked(_)) {
            rows[k].status = Status::Blocked(why);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(names: &[&str]) -> Vec<Source> {
        names
            .iter()
            .map(|n| Source {
                dir: r"\\srv01\project\HIT\hitiso".into(),
                name: n.to_string(),
                is_dir: !n.contains('.'),
                ..Source::default()
            })
            .collect()
    }

    fn existing(names: &[&str]) -> Existing {
        let mut m = Existing::new();
        m.insert(
            r"\\srv01\project\hit\hitiso".into(),
            names.iter().map(|n| n.to_lowercase()).collect(),
        );
        m
    }

    fn re() -> Options {
        Options {
            regex: true,
            ..Options::default()
        }
    }

    #[test]
    fn mockup_12_plan() {
        let s = src(&[
            "B-1001_rev3.pdf",
            "B-1003_rev1.pdf",
            "site_photo_0412.jpg",
            "iso_index.xlsx",
        ]);
        // HIT_B-1003_R1.pdf is already there, not part of the batch.
        let ex = existing(&[
            "B-1001_rev3.pdf",
            "B-1003_rev1.pdf",
            "site_photo_0412.jpg",
            "iso_index.xlsx",
            "HIT_B-1003_R1.pdf",
        ]);
        let p = plan(
            &s,
            r"(B-\d{4})_rev(\d+)",
            "HIT_$1_R$2",
            &re(),
            &Names::default(),
            &ex,
            1,
        );
        let Ok(p) = p else { panic!("{p:?}") };
        assert_eq!(p.rows[0].new_name, "HIT_B-1001_R3.pdf");
        assert_eq!(p.rows[0].status, Status::Rename);
        assert_eq!(
            p.rows[1].status,
            Status::Blocked("collides with existing HIT_B-1003_R1.pdf".into())
        );
        assert_eq!(p.rows[2].status, Status::Unchanged);
        assert_eq!((p.renames, p.unchanged, p.blocked), (1, 2, 1));
        assert_eq!(
            p.pairs(&s),
            [(
                r"\\srv01\project\HIT\hitiso\B-1001_rev3.pdf".to_string(),
                r"\\srv01\project\HIT\hitiso\HIT_B-1001_R3.pdf".to_string()
            )]
        );
    }

    #[test]
    fn collisions_swaps_and_case_only() {
        // Two items onto one name: both blocked.
        let s = src(&["a1.txt", "a2.txt"]);
        let ex = existing(&["a1.txt", "a2.txt"]);
        let p = plan(&s, r"\d", "", &re(), &Names::default(), &ex, 1).unwrap_or_default();
        assert_eq!(p.blocked, 2);
        assert!(matches!(&p.rows[0].status, Status::Blocked(w) if w == "collides with a2.txt"));
        // A chain onto names the batch frees (1→2 while 2→3) and a case-only rename are
        // fine: the two-phase apply goes through temporary names.
        let s = src(&["1.txt", "2.txt"]);
        let ex = existing(&["1.txt", "2.txt"]);
        let chain = Options {
            regex: true,
            enumerate: true,
            ..Options::default()
        };
        let p =
            plan(&s, "^.*$", "${start=2}", &chain, &Names::default(), &ex, 1).unwrap_or_default();
        let names: Vec<&str> = p.rows.iter().map(|r| r.new_name.as_str()).collect();
        assert_eq!(names, ["2.txt", "3.txt"]);
        assert_eq!((p.renames, p.blocked), (2, 0));
        let s = src(&["b-1001.pdf"]);
        let up = Options {
            format: engine::Format::Upper,
            ..Options::default()
        };
        let p = plan(
            &s,
            "",
            "",
            &up,
            &Names::default(),
            &existing(&["b-1001.pdf"]),
            1,
        )
        .unwrap_or_default();
        assert_eq!(p.rows[0].new_name, "B-1001.pdf");
        assert_eq!(p.rows[0].status, Status::Rename, "case-only is allowed");
    }

    #[test]
    fn invalid_excluded_filters_and_enumeration_order() {
        let mut s = src(&["a.txt", "b.txt", "folder", "c.txt"]);
        s[1].excluded = true;
        let o = Options {
            regex: true,
            enumerate: true,
            folders: false,
            ..Options::default()
        };
        let p = plan(
            &s,
            "^.*$",
            "f${start=1}",
            &o,
            &Names::default(),
            &Existing::new(),
            1,
        )
        .unwrap_or_default();
        let names: Vec<&str> = p.rows.iter().map(|r| r.new_name.as_str()).collect();
        // b excluded and the folder left out: not numbered.
        assert_eq!(names, ["f1.txt", "b.txt", "folder", "f2.txt"]);
        assert_eq!(p.excluded, 2);
        // Invalid names are blocked with the reason.
        let s = src(&["a.txt", "b.txt"]);
        let p = plan(
            &s,
            "a",
            "CON",
            &Options::default(),
            &Names::default(),
            &Existing::new(),
            1,
        )
        .unwrap_or_default();
        assert!(
            matches!(&p.rows[0].status, Status::Blocked(_)),
            "{:?}",
            p.rows[0]
        );
        let p = plan(
            &s,
            "b",
            "x?",
            &Options::default(),
            &Names::default(),
            &Existing::new(),
            1,
        )
        .unwrap_or_default();
        assert!(matches!(&p.rows[1].status, Status::Blocked(_)));
        // A bad regex is an error, not a plan.
        assert!(plan(&s, "(", "", &re(), &Names::default(), &Existing::new(), 1).is_err());
    }

    #[test]
    fn dates_and_missing_tokens_warn() {
        use chrono::TimeZone;
        let mut s = src(&["photo.jpg"]);
        s[0].created = chrono::Local
            .with_ymd_and_hms(2026, 5, 1, 9, 0, 0)
            .single()
            .map(SystemTime::from);
        let p = plan(
            &s,
            "photo",
            "$YYYY-$MM-$DD_$CAMERA_MODEL",
            &Options::default(),
            &Names::default(),
            &Existing::new(),
            1,
        )
        .unwrap_or_default();
        assert_eq!(p.rows[0].new_name, "2026-05-01_.jpg");
        assert_eq!(
            p.rows[0].status,
            Status::Warn("no $CAMERA_MODEL value".into())
        );
        // The parent folder token.
        let p = plan(
            &s,
            "photo",
            "${parent}",
            &Options::default(),
            &Names::default(),
            &Existing::new(),
            1,
        )
        .unwrap_or_default();
        assert_eq!(p.rows[0].new_name, "hitiso.jpg");
    }
}
