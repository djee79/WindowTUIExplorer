//! Frecency (CLAUDE.md section 6), ported from wtm's `usage_score`: how often and how
//! recently something was opened. One store, `%LOCALAPPDATA%\sentier\usage.tsv`, one
//! line per key: `count<TAB>last-epoch-secs<TAB>key` (wtm's format).
//!
//! Keys are what a pin opens, lowercased, so renaming a pin keeps its history: the
//! command and args of an app, the URL of a web app, the path of a folder or file.
//!
//! Score = min(count, 100) × recency weight (used < 1 h ago: 8, < 1 day: 4, < 7 days: 2,
//! else 1). Pure except [`load`] and [`save`], which block: the worker calls them.

use crate::pins::{Pin, PinKind};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Usage {
    /// key → (count, last use, epoch seconds).
    map: HashMap<String, (u32, u64)>,
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Usage {
    pub fn score(&self, key: &str, now: u64) -> u64 {
        let Some(&(count, last)) = self.map.get(key) else {
            return 0;
        };
        let age = now.saturating_sub(last);
        let recency = match age {
            0..3_600 => 8,
            3_600..86_400 => 4,
            86_400..604_800 => 2,
            _ => 1,
        };
        u64::from(count.min(100)) * recency
    }

    pub fn record(&mut self, key: &str, now: u64) {
        let e = self.map.entry(key.to_string()).or_insert((0, 0));
        e.0 = e.0.saturating_add(1);
        e.1 = now;
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Takes in what another copy recorded (the file read at startup after launches
    /// were already recorded here): the higher count and the later time win.
    pub fn merge(&mut self, other: Usage) {
        for (k, (c, t)) in other.map {
            let e = self.map.entry(k).or_insert((0, 0));
            e.0 = e.0.max(c);
            e.1 = e.1.max(t);
        }
    }

    pub fn parse(text: &str) -> Usage {
        let mut map = HashMap::new();
        for line in text.lines() {
            let mut it = line.splitn(3, '\t');
            if let (Some(c), Some(t), Some(k)) = (it.next(), it.next(), it.next())
                && let (Ok(c), Ok(t)) = (c.parse(), t.parse())
            {
                map.insert(k.to_string(), (c, t));
            }
        }
        Usage { map }
    }

    pub fn to_text(&self) -> String {
        let mut keys: Vec<&String> = self.map.keys().collect();
        keys.sort();
        keys.into_iter()
            .map(|k| {
                let (c, t) = self.map[k];
                format!("{c}\t{t}\t{k}\n")
            })
            .collect()
    }
}

/// What a pin's frecency is kept under (None: nothing to key it by).
pub fn pin_key(p: &Pin) -> Option<String> {
    let s = |k: &str| p.extra.get(k).and_then(|v| v.as_str()).map(str::to_string);
    let key = match p.kind {
        // wtm's key: `command args`, so imported entries keep their history.
        PinKind::App => format!(
            "{} {}",
            p.command.as_deref()?,
            p.args.as_deref().unwrap_or("")
        )
        .trim()
        .to_string(),
        PinKind::Web => p.url.clone()?,
        PinKind::Folder | PinKind::File => match p.target() {
            Some(t) => t.display(),
            None => p.path.clone()?,
        },
        PinKind::Sync => format!("sync {} {}", s("source")?, s("dest")?),
        PinKind::Backup => format!("backup {} {}", s("source")?, s("dest_dir")?),
        PinKind::Machine => format!("machine {}", s("host")?),
    };
    Some(key.to_lowercase())
}

/// `%LOCALAPPDATA%\sentier\usage.tsv` (a temp file in tests).
pub fn store_path() -> Option<PathBuf> {
    if cfg!(test) {
        return Some(
            std::env::temp_dir().join(format!("sentier-test-usage-{}.tsv", std::process::id())),
        );
    }
    Some(
        PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
            .join("sentier")
            .join("usage.tsv"),
    )
}

pub fn load(path: &Path) -> Usage {
    std::fs::read_to_string(path)
        .map(|t| Usage::parse(&t))
        .unwrap_or_default()
}

/// Atomic: written next to it, then renamed over it.
pub fn save(path: &Path, usage: &Usage) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("tsv.tmp");
    std::fs::write(&tmp, usage.to_text()).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn score_weights_recency() {
        let now = 1_000_000;
        let mut u = Usage::default();
        assert_eq!(u.score("a", now), 0);
        u.record("a", now - 10);
        u.record("a", now - 10);
        assert_eq!(u.score("a", now), 16, "2 uses × 8 (within the hour)");
        assert_eq!(u.score("a", now + 7_200), 8, "× 4 within the day");
        assert_eq!(u.score("a", now + 3 * 86_400), 4, "× 2 within the week");
        assert_eq!(u.score("a", now + 30 * 86_400), 2, "× 1 after");
        for _ in 0..200 {
            u.record("b", now);
        }
        assert_eq!(u.score("b", now), 800, "count capped at 100");
    }

    #[test]
    fn wtm_format_round_trips_and_merges() {
        let text = "3\t1700000000\tc:\\tools\\e3d.bat -p hit\n1\t5\thttps://grafana\nbad line\n";
        let u = Usage::parse(text);
        assert_eq!(u.len(), 2);
        assert_eq!(Usage::parse(&u.to_text()), u);
        let mut mine = Usage::default();
        mine.record("https://grafana", 9);
        mine.merge(u);
        assert_eq!(mine.map["https://grafana"], (1, 9));
        assert_eq!(mine.map["c:\\tools\\e3d.bat -p hit"], (3, 1_700_000_000));

        let path = store_path().unwrap_or_default();
        assert_eq!(save(&path, &mine), Ok(()));
        assert_eq!(load(&path), mine);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn keys_are_what_pins_open() {
        let pin = |t: &str| match toml::from_str::<Pin>(t) {
            Ok(p) => p,
            Err(e) => panic!("{e}"),
        };
        let app = pin("kind='app'\nname='E3D'\ncommand='C:\\Tools\\E3D.bat'\nargs='-p HIT'");
        assert_eq!(pin_key(&app).as_deref(), Some(r"c:\tools\e3d.bat -p hit"));
        let web = pin("kind='web'\nname='g'\nurl='https://Grafana.example'");
        assert_eq!(pin_key(&web).as_deref(), Some("https://grafana.example"));
        let dir = pin("kind='folder'\nname='x'\npath='\\\\SRV01\\p\\HIT\\'");
        assert_eq!(pin_key(&dir).as_deref(), Some(r"\\srv01\p\hit"));
    }
}
