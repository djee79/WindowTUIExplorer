//! The Replace field's tokens (CLAUDE.md section 16), PowerRename's syntax:
//!
//! - enumeration (with "enumerate" on): `${}`, `${start=X}`, `${increment=X}`,
//!   `${padding=X}`, combined with `;`. Several counters may appear in one string.
//! - random (with "random" on): `${rstringalnum=X}`, `${rstringalpha=X}`,
//!   `${rstringdigit=X}`, `${ruuidv4}`.
//! - date/time of the date source: `$YYYY $YY $Y $MMMM $MMM $MM $M $DDDD $DDD $DD $D $hh $h
//!   $mm $m $ss $s $fff $ff $f` (month/day names from the Windows user locale).
//! - photo metadata: `$CAMERA_MAKE`, `$DATE_TAKEN_YYYY`… (empty when the file has none).
//! - sentier extras: `${parent}`, `${name}`, `${ext}`, `${pdf_title}`, `${n;reset=folder}`.
//!
//! `$$` is a literal `$`; a `$` that isn't a token stays as typed. Tokens are expanded
//! before the regex replace, their values escaped, so `$1` / `${2}` stay regex captures.
//!
//! Pure.

use chrono::{Datelike, Timelike};
use std::collections::HashMap;

/// Month and weekday names, from the Windows user locale (English in tests).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Names {
    /// January … December.
    pub months: [String; 12],
    pub months_short: [String; 12],
    /// Monday … Sunday.
    pub days: [String; 7],
    pub days_short: [String; 7],
}

impl Default for Names {
    fn default() -> Self {
        let s = |v: [&str; 12]| v.map(String::from);
        let d = |v: [&str; 7]| v.map(String::from);
        Names {
            months: s([
                "January",
                "February",
                "March",
                "April",
                "May",
                "June",
                "July",
                "August",
                "September",
                "October",
                "November",
                "December",
            ]),
            months_short: s([
                "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
            ]),
            days: d([
                "Monday",
                "Tuesday",
                "Wednesday",
                "Thursday",
                "Friday",
                "Saturday",
                "Sunday",
            ]),
            days_short: d(["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"]),
        }
    }
}

/// Everything one item's tokens may use.
#[derive(Debug, Clone, Default)]
pub struct Context<'a> {
    /// Position among the included items, in the order shown (0-based).
    pub index: u64,
    /// Position within its own folder (for `${n;reset=folder}`).
    pub index_in_folder: u64,
    /// The date source's time (created / modified / EXIF taken), local.
    pub date: Option<chrono::DateTime<chrono::Local>>,
    /// `CAMERA_MAKE` → `Canon`… (without the `$`); None: no metadata read.
    pub meta: Option<&'a HashMap<String, String>>,
    pub parent: &'a str,
    pub stem: &'a str,
    pub ext: &'a str,
    pub pdf_title: Option<&'a str>,
    pub enumerate: bool,
    pub random: bool,
}

/// Per-item notes from the expansion: a token had no value (no EXIF…).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Notes {
    pub missing: Vec<String>,
}

/// The photo tokens (without `$`), longest first so `$DATE_TAKEN_YYYY` wins over shorter.
pub const META_TOKENS: &[&str] = &[
    "DATE_TAKEN_YYYY",
    "DATE_TAKEN_YY",
    "DATE_TAKEN_MM",
    "DATE_TAKEN_DD",
    "DATE_TAKEN_HH",
    "DATE_TAKEN_mm",
    "DATE_TAKEN_SS",
    "EXPOSURE_BIAS",
    "CAMERA_MODEL",
    "CAMERA_MAKE",
    "ORIENTATION",
    "COLOR_SPACE",
    "COPYRIGHT",
    "LONGITUDE",
    "APERTURE",
    "LATITUDE",
    "ALTITUDE",
    "SHUTTER",
    "AUTHOR",
    "HEIGHT",
    "FOCAL",
    "FLASH",
    "WIDTH",
    "LENS",
    "ISO",
];

/// The date tokens, longest first (they're prefixes of each other).
const DATE_TOKENS: &[&str] = &[
    "YYYY", "MMMM", "DDDD", "fff", "YY", "MMM", "MM", "DDD", "DD", "hh", "mm", "ss", "ff", "Y",
    "M", "D", "h", "m", "s", "f",
];

fn date_value(tok: &str, t: &chrono::DateTime<chrono::Local>, names: &Names) -> String {
    let month = t.month0() as usize;
    let day = t.weekday().num_days_from_monday() as usize;
    let ms = t.timestamp_subsec_millis();
    match tok {
        "YYYY" => format!("{:04}", t.year()),
        "YY" => format!("{:02}", t.year().rem_euclid(100)),
        "Y" => format!("{}", t.year().rem_euclid(10)),
        "MMMM" => names.months[month].clone(),
        "MMM" => names.months_short[month].clone(),
        "MM" => format!("{:02}", t.month()),
        "M" => t.month().to_string(),
        "DDDD" => names.days[day].clone(),
        "DDD" => names.days_short[day].clone(),
        "DD" => format!("{:02}", t.day()),
        "D" => t.day().to_string(),
        "hh" => format!("{:02}", t.hour()),
        "h" => t.hour().to_string(),
        "mm" => format!("{:02}", t.minute()),
        "m" => t.minute().to_string(),
        "ss" => format!("{:02}", t.second()),
        "s" => t.second().to_string(),
        "fff" => format!("{ms:03}"),
        "ff" => format!("{:02}", ms / 10),
        "f" => format!("{}", ms / 100),
        _ => String::new(),
    }
}

/// A small, fast generator for `${rstring…}` (no crate needed; not for secrets).
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    /// SplitMix64.
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn pick(&mut self, set: &[u8], n: usize) -> String {
        (0..n)
            .map(|_| set[(self.next() % set.len() as u64) as usize] as char)
            .collect()
    }

    fn uuid_v4(&mut self) -> String {
        let (a, b) = (self.next(), self.next());
        let bytes: Vec<u8> = a.to_le_bytes().into_iter().chain(b.to_le_bytes()).collect();
        let mut b = bytes;
        b[6] = (b[6] & 0x0f) | 0x40;
        b[8] = (b[8] & 0x3f) | 0x80;
        let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
        format!(
            "{}-{}-{}-{}-{}",
            &hex[0..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..32]
        )
    }
}

/// `start=10;increment=2;padding=4` → the counter's text for `index`.
fn counter(params: &str, index: u64) -> Option<String> {
    let (mut start, mut inc, mut pad) = (0i64, 1i64, 0usize);
    for part in params.split(';').map(str::trim).filter(|p| !p.is_empty()) {
        let (k, v) = part.split_once('=')?;
        let v = v.trim();
        match k.trim() {
            "start" => start = v.parse().ok()?,
            "increment" => inc = v.parse().ok()?,
            "padding" => pad = v.parse::<usize>().ok()?.min(32),
            _ => return None,
        }
    }
    let n = start + inc * index as i64;
    Some(if n < 0 {
        format!("-{:0pad$}", -n)
    } else {
        format!("{n:0pad$}")
    })
}

/// Expands the tokens of `replace` for one item. `regex`: the result goes on to a regex
/// replace, so a `$` in a value (or `$$`) comes out as `$$`, and `$1`-style captures and
/// unknown `${…}` are left for the regex.
pub fn expand(
    replace: &str,
    cx: &Context,
    names: &Names,
    rng: &mut Rng,
    regex: bool,
    notes: &mut Notes,
) -> String {
    let lit = |s: &str| {
        if regex {
            s.replace('$', "$$")
        } else {
            s.to_string()
        }
    };
    let mut out = String::new();
    let mut rest = replace;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        rest = &rest[i + 1..];
        if let Some(r) = rest.strip_prefix('$') {
            out.push_str(if regex { "$$" } else { "$" });
            rest = r;
            continue;
        }
        if let Some(body_end) = rest.strip_prefix('{').and_then(|r| r.find('}')) {
            let inner = &rest[1..1 + body_end];
            if let Some(v) = brace_token(inner, cx, rng, notes) {
                out.push_str(&lit(&v));
                rest = &rest[body_end + 2..];
                continue;
            }
            // Not ours (`${1}`, a named capture): leave it for the regex.
            out.push('$');
            continue;
        }
        if let Some(tok) = META_TOKENS.iter().find(|t| rest.starts_with(**t)) {
            let v = cx.meta.and_then(|m| m.get(*tok)).cloned();
            if v.is_none() {
                notes.missing.push(format!("${tok}"));
            }
            out.push_str(&lit(&v.unwrap_or_default()));
            rest = &rest[tok.len()..];
            continue;
        }
        if let Some(tok) = DATE_TOKENS.iter().find(|t| rest.starts_with(**t)) {
            match &cx.date {
                Some(t) => out.push_str(&lit(&date_value(tok, t, names))),
                None => notes.missing.push(format!("${tok}")),
            }
            rest = &rest[tok.len()..];
            continue;
        }
        // `$1`: a capture, written `${1}` so the regex crate doesn't read `$1_R` as a group
        // named `1_R` (PowerRename, like ECMAScript, means group 1).
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if regex && digits > 0 {
            out.push_str(&format!("${{{}}}", &rest[..digits]));
            rest = &rest[digits..];
            continue;
        }
        // A lone `$`: as typed.
        out.push('$');
    }
    out.push_str(rest);
    out
}

/// The `${…}` tokens sentier knows; None leaves it to the regex.
fn brace_token(inner: &str, cx: &Context, rng: &mut Rng, notes: &mut Notes) -> Option<String> {
    let inner = inner.trim();
    match inner {
        "parent" => return Some(cx.parent.to_string()),
        "name" => return Some(cx.stem.to_string()),
        "ext" => return Some(cx.ext.to_string()),
        "pdf_title" => {
            if cx.pdf_title.is_none() {
                notes.missing.push("${pdf_title}".into());
            }
            return Some(cx.pdf_title.unwrap_or_default().to_string());
        }
        "ruuidv4" if cx.random => return Some(rng.uuid_v4()),
        _ => {}
    }
    if cx.random {
        for (key, set) in [
            (
                "rstringalnum=",
                &b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"[..],
            ),
            (
                "rstringalpha=",
                &b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ"[..],
            ),
            ("rstringdigit=", &b"0123456789"[..]),
        ] {
            if let Some(n) = inner.strip_prefix(key) {
                let n: usize = n.trim().parse().ok()?;
                return Some(rng.pick(set, n.min(256)));
            }
        }
    }
    if !cx.enumerate {
        return None;
    }
    // `${n}` / `${n;reset=folder;start=…}`: a counter, per folder with reset=folder.
    if inner == "n" || inner.starts_with("n;") {
        let params: Vec<&str> = inner.split(';').skip(1).collect();
        let per_folder = params.iter().any(|p| p.trim() == "reset=folder");
        let rest: Vec<&str> = params
            .into_iter()
            .filter(|p| p.trim() != "reset=folder")
            .collect();
        let idx = if per_folder {
            cx.index_in_folder
        } else {
            cx.index
        };
        return counter(&rest.join(";"), idx);
    }
    counter(inner, cx.index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn cx<'a>(meta: Option<&'a HashMap<String, String>>) -> Context<'a> {
        Context {
            index: 2,
            index_in_folder: 0,
            date: chrono::Local
                .with_ymd_and_hms(2026, 3, 4, 7, 5, 9)
                .single()
                .map(|t| t + chrono::Duration::milliseconds(456)),
            meta,
            parent: "hitiso",
            stem: "B-1001_rev3",
            ext: "pdf",
            pdf_title: None,
            enumerate: true,
            random: true,
        }
    }

    fn go(replace: &str, regex: bool) -> (String, Notes) {
        let mut notes = Notes::default();
        let mut rng = Rng::new(1);
        let s = expand(
            replace,
            &cx(None),
            &Names::default(),
            &mut rng,
            regex,
            &mut notes,
        );
        (s, notes)
    }

    #[test]
    fn enumeration() {
        assert_eq!(go("x_${}", false).0, "x_2");
        assert_eq!(go("${padding=4;increment=2;start=10}", false).0, "0014");
        assert_eq!(
            go("${start=1}-${padding=3}", false).0,
            "3-002",
            "two counters"
        );
        assert_eq!(go("${n;reset=folder;start=1}", false).0, "1");
        // Enumerate off: left as typed (for a regex, a capture name).
        let mut c = cx(None);
        c.enumerate = false;
        let mut n = Notes::default();
        let s = expand(
            "${}",
            &c,
            &Names::default(),
            &mut Rng::new(1),
            false,
            &mut n,
        );
        assert_eq!(s, "${}");
    }

    #[test]
    fn dates_are_case_sensitive_and_longest_first() {
        assert_eq!(
            go("$YYYY-$MM-$DD $hh$mm$ss.$fff", false).0,
            "2026-03-04 070509.456"
        );
        assert_eq!(
            go("$YY $Y $M $D $h $m $s $ff $f", false).0,
            "26 6 3 4 7 5 9 45 4"
        );
        assert_eq!(
            go("$MMMM $MMM $DDDD $DDD", false).0,
            "March Mar Wednesday Wed"
        );
    }

    #[test]
    fn extras_random_dollars_and_captures() {
        assert_eq!(
            go("${parent}_${name}.${ext}", false).0,
            "hitiso_B-1001_rev3.pdf"
        );
        let (s, _) = go("${rstringdigit=6}", false);
        assert!(s.len() == 6 && s.chars().all(|c| c.is_ascii_digit()), "{s}");
        let (u, _) = go("${ruuidv4}", false);
        assert_eq!(u.len(), 36);
        assert_eq!(u.as_bytes()[14], b'4');
        // Literal mode: $$ → $, a lone $ stays.
        assert_eq!(go("cost $$5 $x", false).0, "cost $5 $x");
        // Regex mode: captures untouched, our values and $$ escaped for the regex.
        assert_eq!(go("$1_${2}_$$_${parent}", true).0, "${1}_${2}_$$_hitiso");
        // Missing metadata: empty, noted.
        let (s, n) = go("$CAMERA_MODEL-x", false);
        assert_eq!(s, "-x");
        assert_eq!(n.missing, ["$CAMERA_MODEL"]);
        let mut meta = HashMap::new();
        meta.insert("CAMERA_MODEL".to_string(), "X-T5".to_string());
        meta.insert("DATE_TAKEN_YYYY".to_string(), "2025".to_string());
        let mut n = Notes::default();
        let s = expand(
            "$DATE_TAKEN_YYYY $CAMERA_MODEL",
            &cx(Some(&meta)),
            &Names::default(),
            &mut Rng::new(1),
            false,
            &mut n,
        );
        assert_eq!(s, "2025 X-T5");
        assert!(n.missing.is_empty());
    }
}
