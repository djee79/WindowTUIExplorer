//! The `s` / `S` query language (CLAUDE.md section 12). Pure parsing, no I/O.
//!
//! ```text
//! b1001 iso                 fuzzy, smart-case, over the relative path
//! *.pdf   B-10??_rev*       glob (when the text has * ? [), over the file name
//! re:^B-\d{4}_rev\d+        regex
//! ext:pdf,dwg  type:f  size:>50mb  mtime:<7d  depth:3  hidden:yes  follow:yes  !old
//! ```
//!
//! Filter tokens can appear anywhere; everything else is the pattern text.

use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pattern {
    /// No text: everything that passes the filters matches (`ext:pdf` alone).
    All,
    Fuzzy(String),
    Glob(String),
    Regex(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Dir,
    File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmp {
    Lt,
    Gt,
}

impl Cmp {
    pub fn test<T: PartialOrd>(self, value: T, bound: T) -> bool {
        match self {
            Cmp::Lt => value < bound,
            Cmp::Gt => value > bound,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub pattern: Pattern,
    /// Lowercase, without the dot.
    pub ext: Vec<String>,
    pub kind: Option<Kind>,
    pub size: Option<(Cmp, u64)>,
    /// `mtime:<7d` = modified less than 7 days ago.
    pub age: Option<(Cmp, Duration)>,
    pub depth: Option<usize>,
    pub hidden: bool,
    pub follow: bool,
    /// Path segments to leave out (lowercase); whole subtrees are pruned.
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QueryError {
    #[error("{0}: use < or >, e.g. size:>50mb")]
    NoComparison(String),
    #[error("{0}: not a size (e.g. 500kb, 50mb, 2gb)")]
    BadSize(String),
    #[error("{0}: not an age (e.g. 30m, 12h, 7d, 2w, 1y)")]
    BadAge(String),
    #[error("{0}: not a number")]
    BadNumber(String),
    #[error("{0}: use yes or no")]
    BadBool(String),
    #[error("{0}: use type:d or type:f")]
    BadType(String),
}

impl Query {
    pub fn parse(input: &str) -> Result<Query, QueryError> {
        let mut q = Query {
            pattern: Pattern::All,
            ext: Vec::new(),
            kind: None,
            size: None,
            age: None,
            depth: None,
            hidden: false,
            follow: false,
            exclude: Vec::new(),
        };
        let mut text: Vec<&str> = Vec::new();
        let mut regex: Option<Vec<&str>> = None;
        for tok in input.split_whitespace() {
            // Once `re:` starts, the rest is the regex (it may contain spaces).
            if let Some(r) = regex.as_mut() {
                r.push(tok);
                continue;
            }
            if let Some(r) = tok.strip_prefix("re:") {
                regex = Some(vec![r]);
                continue;
            }
            if let Some(seg) = tok.strip_prefix('!').filter(|s| !s.is_empty()) {
                q.exclude.push(seg.to_lowercase());
                continue;
            }
            let Some((key, value)) = tok.split_once(':') else {
                text.push(tok);
                continue;
            };
            match key.to_ascii_lowercase().as_str() {
                "ext" => q.ext.extend(
                    value
                        .split(',')
                        .map(|e| e.trim_start_matches('.').to_lowercase())
                        .filter(|e| !e.is_empty()),
                ),
                "type" => {
                    q.kind = Some(match value {
                        "d" | "dir" => Kind::Dir,
                        "f" | "file" => Kind::File,
                        _ => return Err(QueryError::BadType(tok.into())),
                    })
                }
                "size" => {
                    let (cmp, v) = comparison(tok, value)?;
                    q.size = Some((cmp, parse_size(v).ok_or(QueryError::BadSize(tok.into()))?));
                }
                "mtime" => {
                    let (cmp, v) = comparison(tok, value)?;
                    q.age = Some((cmp, parse_age(v).ok_or(QueryError::BadAge(tok.into()))?));
                }
                "depth" => {
                    q.depth = Some(
                        value
                            .parse()
                            .map_err(|_| QueryError::BadNumber(tok.into()))?,
                    )
                }
                "hidden" => q.hidden = yes_no(tok, value)?,
                "follow" => q.follow = yes_no(tok, value)?,
                // Not a filter (a drive letter, a URL-ish name): part of the text.
                _ => text.push(tok),
            }
        }
        q.pattern = match regex {
            Some(r) => Pattern::Regex(r.join(" ")),
            None if text.is_empty() => Pattern::All,
            None => {
                let t = text.join(" ");
                if t.contains(['*', '?', '[']) {
                    Pattern::Glob(t)
                } else {
                    Pattern::Fuzzy(t)
                }
            }
        };
        Ok(q)
    }
}

fn comparison<'a>(tok: &str, value: &'a str) -> Result<(Cmp, &'a str), QueryError> {
    if let Some(v) = value.strip_prefix('<') {
        Ok((Cmp::Lt, v))
    } else if let Some(v) = value.strip_prefix('>') {
        Ok((Cmp::Gt, v))
    } else {
        Err(QueryError::NoComparison(tok.to_string()))
    }
}

fn yes_no(tok: &str, v: &str) -> Result<bool, QueryError> {
    match v.to_ascii_lowercase().as_str() {
        "yes" | "y" | "true" | "on" | "1" => Ok(true),
        "no" | "n" | "false" | "off" | "0" => Ok(false),
        _ => Err(QueryError::BadBool(tok.to_string())),
    }
}

/// Splits `12.5mb` into (12.5, "mb").
fn number_unit(s: &str) -> Option<(f64, String)> {
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let n: f64 = s[..split].parse().ok()?;
    (n >= 0.0).then(|| (n, s[split..].to_ascii_lowercase()))
}

/// 1024-based like the rest of the UI: `500kb`, `50mb`, `2gb`, `1tb`, bare = bytes.
pub fn parse_size(s: &str) -> Option<u64> {
    let (n, unit) = number_unit(s)?;
    let mult: f64 = match unit.as_str() {
        "" | "b" => 1.0,
        "k" | "kb" => 1024.0,
        "m" | "mb" => 1024.0 * 1024.0,
        "g" | "gb" => 1024.0 * 1024.0 * 1024.0,
        "t" | "tb" => 1024.0_f64.powi(4),
        _ => return None,
    };
    Some((n * mult) as u64)
}

/// `30s`, `30m` (minutes), `12h`, `7d`, `2w`, `3mo`, `1y`.
pub fn parse_age(s: &str) -> Option<Duration> {
    let (n, unit) = number_unit(s)?;
    let secs: f64 = match unit.as_str() {
        "s" => 1.0,
        "m" | "min" => 60.0,
        "h" => 3600.0,
        "d" => 86_400.0,
        "w" => 7.0 * 86_400.0,
        "mo" => 30.0 * 86_400.0,
        "y" => 365.0 * 86_400.0,
        _ => return None,
    };
    Some(Duration::from_secs_f64(n * secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1024 * 1024;
    const DAY: u64 = 86_400;

    fn p(s: &str) -> Query {
        match Query::parse(s) {
            Ok(q) => q,
            Err(e) => panic!("{s}: {e}"),
        }
    }

    #[test]
    fn patterns_table() {
        let cases: &[(&str, Pattern)] = &[
            ("", Pattern::All),
            ("   ", Pattern::All),
            ("b1001 iso", Pattern::Fuzzy("b1001 iso".into())),
            ("*.pdf", Pattern::Glob("*.pdf".into())),
            ("B-10??_rev*", Pattern::Glob("B-10??_rev*".into())),
            ("[ab]*.dwg", Pattern::Glob("[ab]*.dwg".into())),
            (r"re:^B-\d{4}", Pattern::Regex(r"^B-\d{4}".into())),
            ("re:a b", Pattern::Regex("a b".into())),
            ("ext:pdf", Pattern::All),
            ("hit ext:pdf iso", Pattern::Fuzzy("hit iso".into())),
            ("C:", Pattern::Fuzzy("C:".into())),
        ];
        for (input, want) in cases {
            assert_eq!(p(input).pattern, *want, "{input:?}");
        }
    }

    #[test]
    fn filters_table() {
        let q = p(
            "hit ext:PDF,.dwg type:f size:>50mb mtime:<7d depth:3 hidden:yes follow:y !old !Archive",
        );
        assert_eq!(q.ext, ["pdf", "dwg"]);
        assert_eq!(q.kind, Some(Kind::File));
        assert_eq!(q.size, Some((Cmp::Gt, 50 * MB)));
        assert_eq!(q.age, Some((Cmp::Lt, Duration::from_secs(7 * DAY))));
        assert_eq!(q.depth, Some(3));
        assert!(q.hidden && q.follow);
        assert_eq!(q.exclude, ["old", "archive"]);

        let d = p("type:d");
        assert_eq!(d.kind, Some(Kind::Dir));
        assert!(!d.hidden && !d.follow && d.depth.is_none());
        assert_eq!(
            p("!").pattern,
            Pattern::Fuzzy("!".into()),
            "a lone ! is text"
        );
    }

    #[test]
    fn units_table() {
        let sizes: &[(&str, Option<u64>)] = &[
            ("100", Some(100)),
            ("1kb", Some(1024)),
            ("1.5mb", Some(MB + MB / 2)),
            ("2G", Some(2 * 1024 * MB)),
            ("1tb", Some(1024 * 1024 * MB)),
            ("mb", None),
            ("5xb", None),
            ("-1mb", None),
        ];
        for (s, want) in sizes {
            assert_eq!(parse_size(s), *want, "{s}");
        }
        let ages: &[(&str, Option<u64>)] = &[
            ("30m", Some(1800)),
            ("12h", Some(12 * 3600)),
            ("7d", Some(7 * DAY)),
            ("2w", Some(14 * DAY)),
            ("1y", Some(365 * DAY)),
            ("3mo", Some(90 * DAY)),
            ("7", None),
            ("7q", None),
        ];
        for (s, want) in ages {
            assert_eq!(parse_age(s).map(|d| d.as_secs()), *want, "{s}");
        }
    }

    #[test]
    fn errors_table() {
        let cases: &[(&str, &str)] = &[
            ("size:50mb", "use < or >"),
            ("size:>lots", "not a size"),
            ("mtime:<soon", "not an age"),
            ("depth:deep", "not a number"),
            ("hidden:maybe", "yes or no"),
            ("type:x", "type:d or type:f"),
        ];
        for (input, want) in cases {
            let err = Query::parse(input).err().map(|e| e.to_string());
            assert!(
                err.as_deref().is_some_and(|e| e.contains(want)),
                "{input}: {err:?}"
            );
        }
    }
}
