//! Search / replace for one name (CLAUDE.md section 16), PowerRename's semantics:
//!
//! - literal search by default, regex with `r` (Rust `regex`), "Boost" with `b`
//!   (`fancy-regex`: lookbehind, backreferences); captures `$1` `${name}` in Replace;
//! - first match only, unless "match all";
//! - case-insensitive unless "case";
//! - applied to the name, the extension, or both;
//! - then the text format (lower, UPPER, Title case, Capitalize Each Word).
//!
//! Pure.

use super::tokens::{self, Context, Names, Notes, Rng};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApplyTo {
    #[default]
    Name,
    Extension,
    Both,
}

impl ApplyTo {
    pub fn word(self) -> &'static str {
        match self {
            ApplyTo::Name => "name only",
            ApplyTo::Extension => "extension only",
            ApplyTo::Both => "name + extension",
        }
    }

    pub fn next(self) -> ApplyTo {
        match self {
            ApplyTo::Name => ApplyTo::Extension,
            ApplyTo::Extension => ApplyTo::Both,
            ApplyTo::Both => ApplyTo::Name,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Format {
    #[default]
    None,
    Lower,
    Upper,
    Title,
    Capitalize,
}

impl Format {
    pub fn word(self) -> &'static str {
        match self {
            Format::None => "none",
            Format::Lower => "lowercase",
            Format::Upper => "UPPERCASE",
            Format::Title => "Title case",
            Format::Capitalize => "Capitalize Each Word",
        }
    }

    pub fn next(self) -> Format {
        match self {
            Format::None => Format::Lower,
            Format::Lower => Format::Upper,
            Format::Upper => Format::Title,
            Format::Title => Format::Capitalize,
            Format::Capitalize => Format::None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DateSource {
    /// PowerRename's default.
    #[default]
    Created,
    Modified,
    /// EXIF date taken (pictures).
    Taken,
}

impl DateSource {
    pub fn word(self) -> &'static str {
        match self {
            DateSource::Created => "created",
            DateSource::Modified => "modified",
            DateSource::Taken => "EXIF date taken",
        }
    }

    pub fn next(self) -> DateSource {
        match self {
            DateSource::Created => DateSource::Modified,
            DateSource::Modified => DateSource::Taken,
            DateSource::Taken => DateSource::Created,
        }
    }
}

/// Every toggle of the panel (saved with presets, restored when it opens).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Options {
    pub regex: bool,
    pub boost: bool,
    pub match_all: bool,
    pub case: bool,
    pub apply_to: ApplyTo,
    pub files: bool,
    pub folders: bool,
    pub subfolders: bool,
    pub format: Format,
    pub enumerate: bool,
    pub random: bool,
    pub dates: DateSource,
    pub only_changed: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            regex: false,
            boost: false,
            match_all: false,
            case: false,
            apply_to: ApplyTo::Name,
            files: true,
            folders: true,
            subfolders: false,
            format: Format::None,
            enumerate: false,
            random: false,
            dates: DateSource::Created,
            only_changed: false,
        }
    }
}

/// The search, compiled once for every name.
pub enum Matcher {
    /// Nothing to search for: names change only by the text format.
    None,
    Literal {
        needle: String,
        case: bool,
    },
    Regex(regex::Regex),
    Fancy(fancy_regex::Regex),
}

impl Matcher {
    pub fn new(search: &str, o: &Options) -> Result<Matcher, String> {
        if search.is_empty() {
            return Ok(Matcher::None);
        }
        if !o.regex {
            return Ok(Matcher::Literal {
                needle: search.to_string(),
                case: o.case,
            });
        }
        if o.boost {
            let re = fancy_regex::RegexBuilder::new(search)
                .case_insensitive(!o.case)
                .build()
                .map_err(|e| format!("regex: {e}"))?;
            Ok(Matcher::Fancy(re))
        } else {
            let re = regex::RegexBuilder::new(search)
                .case_insensitive(!o.case)
                .build()
                .map_err(|e| format!("regex: {}", e.to_string().lines().last().unwrap_or("")))?;
            Ok(Matcher::Regex(re))
        }
    }

    /// `text` with the search replaced by `rep` (already token-expanded).
    fn replace(&self, text: &str, rep: &str, all: bool) -> String {
        match self {
            Matcher::None => text.to_string(),
            Matcher::Literal { needle, case } => replace_literal(text, needle, rep, *case, all),
            Matcher::Regex(re) => {
                if all {
                    re.replace_all(text, rep).into_owned()
                } else {
                    re.replace(text, rep).into_owned()
                }
            }
            Matcher::Fancy(re) => {
                if all {
                    re.replace_all(text, rep).into_owned()
                } else {
                    re.replace(text, rep).into_owned()
                }
            }
        }
    }
}

/// Literal, case-insensitive unless `case`: ASCII-only case folding (positions stay put).
fn replace_literal(text: &str, needle: &str, rep: &str, case: bool, all: bool) -> String {
    let (hay, pat) = if case {
        (text.to_string(), needle.to_string())
    } else {
        (text.to_ascii_lowercase(), needle.to_ascii_lowercase())
    };
    let mut out = String::new();
    let mut at = 0;
    while let Some(i) = hay[at..].find(&pat) {
        out.push_str(&text[at..at + i]);
        out.push_str(rep);
        at += i + pat.len();
        if !all || pat.is_empty() {
            break;
        }
    }
    out.push_str(&text[at..]);
    out
}

/// `report.FINAL.pdf` → (`report.FINAL`, `pdf`); folders and dot-files have no extension.
pub fn split_name(name: &str, is_dir: bool) -> (&str, &str) {
    if is_dir {
        return (name, "");
    }
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, ext),
        _ => (name, ""),
    }
}

/// Words PowerRename keeps lowercase in Title case (unless first).
const SMALL_WORDS: &[&str] = &[
    "a", "an", "and", "as", "at", "but", "by", "for", "in", "nor", "of", "on", "or", "the", "to",
    "up", "with",
];

fn format(text: &str, f: Format) -> String {
    let cap = |w: &str| {
        let mut c = w.chars();
        match c.next() {
            Some(first) => first.to_uppercase().collect::<String>() + &c.as_str().to_lowercase(),
            None => String::new(),
        }
    };
    let words = |title: bool| -> String {
        let mut out = String::new();
        let mut word = String::new();
        let mut first = true;
        let flush = |word: &mut String, out: &mut String, first: &mut bool| {
            if !word.is_empty() {
                let lower = word.to_lowercase();
                if title && !*first && SMALL_WORDS.contains(&lower.as_str()) {
                    out.push_str(&lower);
                } else {
                    out.push_str(&cap(word));
                }
                *first = false;
                word.clear();
            }
        };
        for ch in text.chars() {
            if ch.is_alphanumeric() || ch == '\'' {
                word.push(ch);
            } else {
                flush(&mut word, &mut out, &mut first);
                out.push(ch);
            }
        }
        flush(&mut word, &mut out, &mut first);
        out
    };
    match f {
        Format::None => text.to_string(),
        Format::Lower => text.to_lowercase(),
        Format::Upper => text.to_uppercase(),
        Format::Title => words(true),
        Format::Capitalize => words(false),
    }
}

/// What a whole batch is renamed with.
pub struct Rules<'a> {
    pub matcher: &'a Matcher,
    pub replace: &'a str,
    pub options: &'a Options,
    pub names: &'a Names,
}

/// The new name for one item: tokens expanded, the search replaced in the part `apply_to`
/// names, then the text format on that part.
pub fn rename(
    name: &str,
    is_dir: bool,
    rules: &Rules,
    cx: &Context,
    rng: &mut Rng,
    notes: &mut Notes,
) -> String {
    let (matcher, o) = (rules.matcher, rules.options);
    let regex = matches!(matcher, Matcher::Regex(_) | Matcher::Fancy(_));
    let rep = tokens::expand(rules.replace, cx, rules.names, rng, regex, notes);
    let edit = |part: &str| format(&matcher.replace(part, &rep, o.match_all), o.format);
    let (stem, ext) = split_name(name, is_dir);
    // A folder, or a file without an extension: the name is all there is.
    if ext.is_empty() {
        return match o.apply_to {
            ApplyTo::Extension => name.to_string(),
            ApplyTo::Name | ApplyTo::Both => edit(name),
        };
    }
    match o.apply_to {
        ApplyTo::Name => format!("{}.{ext}", edit(stem)),
        ApplyTo::Extension => format!("{stem}.{}", edit(ext)),
        ApplyTo::Both => edit(name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(name: &str, search: &str, replace: &str, o: &Options) -> String {
        let m = match Matcher::new(search, o) {
            Ok(m) => m,
            Err(e) => panic!("{e}"),
        };
        let (stem, ext) = split_name(name, false);
        let cx = Context {
            stem,
            ext,
            enumerate: o.enumerate,
            ..Context::default()
        };
        let mut notes = Notes::default();
        let names = Names::default();
        let rules = Rules {
            matcher: &m,
            replace,
            options: o,
            names: &names,
        };
        rename(name, false, &rules, &cx, &mut Rng::new(1), &mut notes)
    }

    #[test]
    fn literal_first_or_all_case_insensitive() {
        let o = Options::default();
        assert_eq!(
            run("Rev_rev_REV.pdf", "rev", "R", &o),
            "R_rev_REV.pdf",
            "first only"
        );
        let all = Options {
            match_all: true,
            ..o
        };
        assert_eq!(run("Rev_rev_REV.pdf", "rev", "R", &all), "R_R_R.pdf");
        let case = Options { case: true, ..all };
        assert_eq!(run("Rev_rev_REV.pdf", "rev", "R", &case), "Rev_R_REV.pdf");
        // Name only by default: the extension is untouched.
        assert_eq!(run("pdf.pdf", "pdf", "x", &all), "x.pdf");
        let ext = Options {
            apply_to: ApplyTo::Extension,
            ..all
        };
        assert_eq!(run("pdf.PDF", "pdf", "dwg", &ext), "pdf.dwg");
        let both = Options {
            apply_to: ApplyTo::Both,
            ..all
        };
        assert_eq!(run("a.pdf", ".pdf", "_old.pdf", &both), "a_old.pdf");
        // Nothing to search: only the format applies.
        let upper = Options {
            format: Format::Upper,
            ..o
        };
        assert_eq!(run("hit iso.pdf", "", "", &upper), "HIT ISO.pdf");
    }

    #[test]
    fn regex_captures_and_boost() {
        let re = Options {
            regex: true,
            ..Options::default()
        };
        assert_eq!(
            run("B-1001_rev3.pdf", r"(B-\d{4})_rev(\d+)", "HIT_$1_R$2", &re),
            "HIT_B-1001_R3.pdf"
        );
        assert_eq!(
            run(
                "a1b2.txt",
                r"(?<d>\d)",
                "<${d}>",
                &Options {
                    match_all: true,
                    ..re
                }
            ),
            "a<1>b<2>.txt"
        );
        assert!(
            Matcher::new("(?<=x)y", &re).is_err(),
            "lookbehind needs Boost"
        );
        let boost = Options { boost: true, ..re };
        assert_eq!(run("xy zy.txt", "(?<=x)y", "Y", &boost), "xY zy.txt");
        assert_eq!(
            run(
                "aa bb.txt",
                r"(\w)\1",
                "_",
                &Options {
                    match_all: true,
                    ..boost
                }
            ),
            "_ _.txt"
        );
        assert!(Matcher::new("(", &re).is_err_and(|e| e.starts_with("regex")));
        // Tokens with regex: the counter's value isn't read as a capture.
        let en = Options {
            enumerate: true,
            ..re
        };
        assert_eq!(run("x.txt", "^", "${padding=2}_", &en), "00_x.txt");
    }

    #[test]
    fn text_formats() {
        let f = |t: &str, fmt| format(t, fmt);
        assert_eq!(
            f("the LORD of the rings", Format::Title),
            "The Lord of the Rings"
        );
        assert_eq!(
            f("the LORD of the rings", Format::Capitalize),
            "The Lord Of The Rings"
        );
        assert_eq!(f("B-1001 rev", Format::Lower), "b-1001 rev");
        assert_eq!(
            f("piping_iso-sheet", Format::Capitalize),
            "Piping_Iso-Sheet"
        );
        assert_eq!(split_name(".gitignore", false), (".gitignore", ""));
        assert_eq!(split_name("a.b.c", false), ("a.b", "c"));
        assert_eq!(split_name("dir.v2", true), ("dir.v2", ""));
    }
}
