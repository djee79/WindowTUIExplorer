//! Values read from the files themselves for the Replace tokens (CLAUDE.md section 16):
//! photo EXIF (`$CAMERA_MODEL`, `$DATE_TAKEN_YYYY`…, and "date taken" as a date source)
//! and a PDF's title (`${pdf_title}`).
//!
//! [`read`] blocks (it opens the file); only the worker calls it. The parsers are pure.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::SystemTime;

/// What one file gave.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Meta {
    /// `CAMERA_MODEL` → `X-T5`… (token names without `$`).
    pub values: HashMap<String, String>,
    pub taken: Option<SystemTime>,
    pub pdf_title: Option<String>,
}

const PHOTO: &[&str] = &[
    "jpg", "jpeg", "jpe", "tif", "tiff", "heic", "heif", "avif", "png", "webp", "dng", "nef",
    "cr2", "arw", "orf", "rw2",
];

/// Does reading this file give anything? (By extension.)
pub fn wanted(name: &str) -> bool {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    matches!(ext.as_deref(), Some(e) if e == "pdf" || PHOTO.contains(&e))
}

/// Reads the file's photo metadata or PDF title. None: nothing there (or unreadable).
pub fn read(path: &Path) -> Option<Meta> {
    let name = path.file_name()?.to_string_lossy().to_ascii_lowercase();
    if name.ends_with(".pdf") {
        let title = pdf_title(&pdf_bytes(path)?)?;
        return Some(Meta {
            pdf_title: Some(title),
            ..Meta::default()
        });
    }
    let file = std::fs::File::open(path).ok()?;
    let exif = exif::Reader::new()
        .read_from_container(&mut std::io::BufReader::new(file))
        .ok()?;
    let (values, taken) = photo_values(&exif);
    Some(Meta {
        values,
        taken,
        pdf_title: None,
    })
}

/// A PDF's title sits in its Info dictionary (usually near the end) or its XMP packet
/// (usually near the start): the first and last 512 KB are read, not the whole file.
fn pdf_bytes(path: &Path) -> Option<Vec<u8>> {
    const PART: u64 = 512 * 1024;
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let mut buf = Vec::new();
    if len <= 2 * PART {
        f.read_to_end(&mut buf).ok()?;
        return Some(buf);
    }
    let mut head = vec![0u8; PART as usize];
    f.read_exact(&mut head).ok()?;
    f.seek(SeekFrom::End(-(PART as i64))).ok()?;
    let mut tail = vec![0u8; PART as usize];
    f.read_exact(&mut tail).ok()?;
    head.extend_from_slice(&tail);
    Some(head)
}

/// The title from XMP (`<dc:title>…<rdf:li …>Title</rdf:li>`) or the Info dictionary
/// (`/Title (…)` or `/Title <FEFF…>`). Compressed object streams aren't opened.
pub fn pdf_title(bytes: &[u8]) -> Option<String> {
    xmp_title(bytes)
        .or_else(|| info_title(bytes))
        .map(|t| clean(&t))
        .filter(|t| !t.is_empty())
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    hay.get(from..)?
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|i| i + from)
}

fn xmp_title(b: &[u8]) -> Option<String> {
    let at = find(b, b"<dc:title", 0)?;
    let end = find(b, b"</dc:title>", at)?;
    let li = find(&b[..end], b"<rdf:li", at)?;
    let open = find(&b[..end], b">", li)? + 1;
    let close = find(&b[..end], b"</rdf:li>", open)?;
    let text = String::from_utf8_lossy(&b[open..close]).to_string();
    Some(
        text.replace("&amp;", "&")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'"),
    )
}

fn info_title(b: &[u8]) -> Option<String> {
    // The last one wins: incremental updates append a newer Info dictionary.
    let mut at = None;
    let mut from = 0;
    while let Some(i) = find(b, b"/Title", from) {
        at = Some(i);
        from = i + 6;
    }
    let mut i = at? + 6;
    while b.get(i).is_some_and(|c| c.is_ascii_whitespace()) {
        i += 1;
    }
    match b.get(i)? {
        b'(' => Some(decode_text(&literal_string(&b[i + 1..]))),
        b'<' => {
            let end = find(b, b">", i)?;
            let hex: Vec<u8> = b[i + 1..end]
                .iter()
                .copied()
                .filter(u8::is_ascii_hexdigit)
                .collect();
            let bytes: Vec<u8> = hex
                .chunks(2)
                .filter_map(|c| {
                    let s = std::str::from_utf8(c).ok()?;
                    u8::from_str_radix(&format!("{s:0<2}"), 16).ok()
                })
                .collect();
            Some(decode_text(&bytes))
        }
        _ => None,
    }
}

/// A PDF `( … )` string's bytes: nested parentheses, `\` escapes, octal.
fn literal_string(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut depth = 0;
    let mut i = 0;
    while let Some(&c) = b.get(i) {
        i += 1;
        match c {
            b'\\' => {
                let Some(&e) = b.get(i) else { break };
                i += 1;
                match e {
                    b'n' => out.push(b'\n'),
                    b'r' => out.push(b'\r'),
                    b't' => out.push(b'\t'),
                    b'b' => out.push(8),
                    b'f' => out.push(12),
                    b'0'..=b'7' => {
                        let mut v = u32::from(e - b'0');
                        for _ in 0..2 {
                            match b.get(i) {
                                Some(&d @ b'0'..=b'7') => {
                                    v = v * 8 + u32::from(d - b'0');
                                    i += 1;
                                }
                                _ => break,
                            }
                        }
                        out.push(v as u8);
                    }
                    b'\r' | b'\n' => {}
                    other => out.push(other),
                }
            }
            b'(' => {
                depth += 1;
                out.push(c);
            }
            b')' if depth == 0 => break,
            b')' => {
                depth -= 1;
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// UTF-16BE with a BOM, UTF-8 with a BOM, else PDFDocEncoding (≈ Latin-1).
fn decode_text(b: &[u8]) -> String {
    if let Some(rest) = b.strip_prefix(&[0xFE, 0xFF]) {
        let units: Vec<u16> = rest
            .chunks(2)
            .map(|c| u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    if let Some(rest) = b.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).to_string();
    }
    b.iter().map(|&c| c as char).collect()
}

/// Safe in a file name: the characters Windows refuses become `-`, runs of spaces one.
pub fn clean(s: &str) -> String {
    let s: String = s
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '-',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Every photo token there's a value for, and the date taken.
fn photo_values(exif: &exif::Exif) -> (HashMap<String, String>, Option<SystemTime>) {
    use exif::{In, Tag, Value};
    let field = |t: Tag| exif.get_field(t, In::PRIMARY);
    let text = |t: Tag| -> Option<String> {
        match &field(t)?.value {
            Value::Ascii(v) => {
                let s = String::from_utf8_lossy(v.first()?)
                    .trim_matches(['\0', ' '])
                    .to_string();
                (!s.is_empty()).then_some(s)
            }
            _ => None,
        }
    };
    let uint = |t: Tag| field(t).and_then(|f| f.value.get_uint(0));
    let ratio = |t: Tag| -> Option<f64> {
        match &field(t)?.value {
            Value::Rational(v) => v.first().map(|r| r.to_f64()),
            Value::SRational(v) => v.first().map(|r| r.to_f64()),
            _ => None,
        }
    };
    let num = |v: f64| {
        let s = format!("{v:.1}");
        s.strip_suffix(".0").map(str::to_string).unwrap_or(s)
    };
    let mut m: HashMap<String, String> = HashMap::new();
    let mut put = |k: &str, v: Option<String>| {
        if let Some(v) = v.map(|v| clean(&v)).filter(|v| !v.is_empty()) {
            m.insert(k.to_string(), v);
        }
    };
    put("CAMERA_MAKE", text(Tag::Make));
    put("CAMERA_MODEL", text(Tag::Model));
    put("LENS", text(Tag::LensModel));
    put(
        "ISO",
        uint(Tag::PhotographicSensitivity).map(|v| v.to_string()),
    );
    put(
        "APERTURE",
        ratio(Tag::FNumber).map(|v| format!("f{}", num(v))),
    );
    put(
        "SHUTTER",
        ratio(Tag::ExposureTime).map(|v| {
            if v > 0.0 && v < 1.0 {
                format!("1-{}", (1.0 / v).round())
            } else {
                format!("{}s", num(v))
            }
        }),
    );
    put(
        "FOCAL",
        ratio(Tag::FocalLength).map(|v| format!("{}mm", num(v))),
    );
    put(
        "FLASH",
        uint(Tag::Flash).map(|v| if v & 1 == 1 { "Flash" } else { "NoFlash" }.to_string()),
    );
    put(
        "EXPOSURE_BIAS",
        ratio(Tag::ExposureBiasValue).map(|v| format!("{v:+.1}")),
    );
    put("ORIENTATION", uint(Tag::Orientation).map(|v| v.to_string()));
    put(
        "COLOR_SPACE",
        uint(Tag::ColorSpace).map(|v| match v {
            1 => "sRGB".to_string(),
            0xFFFF => "Uncalibrated".to_string(),
            v => v.to_string(),
        }),
    );
    put(
        "WIDTH",
        uint(Tag::PixelXDimension)
            .or_else(|| uint(Tag::ImageWidth))
            .map(|v| v.to_string()),
    );
    put(
        "HEIGHT",
        uint(Tag::PixelYDimension)
            .or_else(|| uint(Tag::ImageLength))
            .map(|v| v.to_string()),
    );
    put("AUTHOR", text(Tag::Artist));
    put("COPYRIGHT", text(Tag::Copyright));
    let coord = |t: Tag, r: Tag, neg: &[u8]| -> Option<String> {
        let Value::Rational(v) = &field(t)?.value else {
            return None;
        };
        let deg = v.first()?.to_f64()
            + v.get(1).map_or(0.0, |x| x.to_f64()) / 60.0
            + v.get(2).map_or(0.0, |x| x.to_f64()) / 3600.0;
        let south = match &field(r).map(|f| &f.value) {
            Some(Value::Ascii(a)) => a
                .first()
                .and_then(|s| s.first())
                .is_some_and(|c| neg.contains(c)),
            _ => false,
        };
        Some(format!("{:.6}", if south { -deg } else { deg }))
    };
    put(
        "LATITUDE",
        coord(Tag::GPSLatitude, Tag::GPSLatitudeRef, b"S"),
    );
    put(
        "LONGITUDE",
        coord(Tag::GPSLongitude, Tag::GPSLongitudeRef, b"W"),
    );
    put(
        "ALTITUDE",
        ratio(Tag::GPSAltitude).map(|v| {
            let below = uint(Tag::GPSAltitudeRef) == Some(1);
            format!("{}m", num(if below { -v } else { v }))
        }),
    );
    let taken_text = text(Tag::DateTimeOriginal).or_else(|| text(Tag::DateTime));
    let taken = taken_text.as_deref().and_then(exif_time);
    if let Some(d) = taken_text.as_deref() {
        for (k, v) in date_taken_values(d) {
            m.insert(k.to_string(), v);
        }
    }
    (m, taken)
}

/// `2024:06:01 14:32:10` → the `$DATE_TAKEN_*` values.
fn date_taken_values(d: &str) -> Vec<(&'static str, String)> {
    let b = d.as_bytes();
    if b.len() < 19 || b[4] != b':' || b[7] != b':' {
        return Vec::new();
    }
    vec![
        ("DATE_TAKEN_YYYY", d[0..4].to_string()),
        ("DATE_TAKEN_YY", d[2..4].to_string()),
        ("DATE_TAKEN_MM", d[5..7].to_string()),
        ("DATE_TAKEN_DD", d[8..10].to_string()),
        ("DATE_TAKEN_HH", d[11..13].to_string()),
        ("DATE_TAKEN_mm", d[14..16].to_string()),
        ("DATE_TAKEN_SS", d[17..19].to_string()),
    ]
}

/// EXIF's local `YYYY:MM:DD HH:MM:SS` as a point in time.
fn exif_time(d: &str) -> Option<SystemTime> {
    use chrono::TimeZone;
    let t = chrono::NaiveDateTime::parse_from_str(d.get(..19)?, "%Y:%m:%d %H:%M:%S").ok()?;
    let local = chrono::Local.from_local_datetime(&t).earliest()?;
    Some(SystemTime::from(local))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdf_titles() {
        let info = b"%PDF-1.4\n1 0 obj << /Title (Piping \\(HIT\\) iso) /Author (x) >> endobj";
        assert_eq!(pdf_title(info).as_deref(), Some("Piping (HIT) iso"));
        // UTF-16 hex, and the last Info wins (incremental update).
        let hex = b"<< /Title (old) >> ... << /Title <FEFF00C9007400E9> >>";
        assert_eq!(pdf_title(hex).as_deref(), Some("\u{c9}t\u{e9}"));
        // XMP first; unsafe characters cleaned.
        let xmp = b"<x:xmpmeta><dc:title><rdf:Alt><rdf:li xml:lang=\"x-default\">A/B: plan &amp; section</rdf:li></rdf:Alt></dc:title> << /Title (info) >>";
        assert_eq!(pdf_title(xmp).as_deref(), Some("A-B- plan & section"));
        assert_eq!(pdf_title(b"%PDF-1.7 nothing here"), None);
        assert_eq!(pdf_title(b"<< /Title () >>"), None, "empty is none");
        assert_eq!(literal_string(b"a\\101\\nb) rest"), b"aA\nb".to_vec());
    }

    #[test]
    fn photo_values_from_a_real_exif_block() {
        use exif::{Field, In, Rational, Tag, Value};
        let fields = [
            Field {
                tag: Tag::Model,
                ifd_num: In::PRIMARY,
                value: Value::Ascii(vec![b"X-T5".to_vec()]),
            },
            Field {
                tag: Tag::DateTimeOriginal,
                ifd_num: In::PRIMARY,
                value: Value::Ascii(vec![b"2025:06:01 14:32:10".to_vec()]),
            },
            Field {
                tag: Tag::FNumber,
                ifd_num: In::PRIMARY,
                value: Value::Rational(vec![Rational { num: 28, denom: 10 }]),
            },
            Field {
                tag: Tag::ExposureTime,
                ifd_num: In::PRIMARY,
                value: Value::Rational(vec![Rational { num: 1, denom: 250 }]),
            },
            Field {
                tag: Tag::GPSLatitude,
                ifd_num: In::PRIMARY,
                value: Value::Rational(vec![
                    Rational { num: 45, denom: 1 },
                    Rational { num: 30, denom: 1 },
                    Rational { num: 0, denom: 1 },
                ]),
            },
            Field {
                tag: Tag::GPSLatitudeRef,
                ifd_num: In::PRIMARY,
                value: Value::Ascii(vec![b"S".to_vec()]),
            },
        ];
        let mut w = exif::experimental::Writer::new();
        for f in &fields {
            w.push_field(f);
        }
        let mut buf = std::io::Cursor::new(Vec::new());
        assert!(w.write(&mut buf, true).is_ok());
        let exif = exif::Reader::new()
            .read_from_container(&mut std::io::Cursor::new(buf.into_inner()))
            .expect("valid exif");
        let (m, taken) = photo_values(&exif);
        assert_eq!(m.get("CAMERA_MODEL").map(String::as_str), Some("X-T5"));
        assert_eq!(m.get("APERTURE").map(String::as_str), Some("f2.8"));
        assert_eq!(m.get("SHUTTER").map(String::as_str), Some("1-250"));
        assert_eq!(m.get("LATITUDE").map(String::as_str), Some("-45.500000"));
        assert_eq!(m.get("DATE_TAKEN_YYYY").map(String::as_str), Some("2025"));
        assert_eq!(m.get("DATE_TAKEN_mm").map(String::as_str), Some("32"));
        let local = chrono::DateTime::<chrono::Local>::from(taken.expect("taken"));
        assert_eq!(
            local.format("%Y-%m-%d %H:%M").to_string(),
            "2025-06-01 14:32"
        );
        assert!(wanted("IMG_001.JPG") && wanted("a.pdf") && !wanted("a.txt"));
    }
}
