//! Text previews: the first lines of a text or code file (`.bat`, `.ps1`, `.vb`,
//! `.pmlmac`, logs…). Pure: the worker reads the first [`READ_BYTES`] and hands them here.
//!
//! - Which files: known text extensions; known binary ones never; anything else (no or
//!   unknown extension) is looked at: no NUL bytes means text.
//! - Encodings: UTF-8 (with or without BOM), UTF-16 LE / BE (BOM, or ASCII-looking with
//!   zero bytes in between: PowerShell 5 and `.reg` write those), else Windows-1252 (old
//!   `.bat` / `.ini` files).
//! - Tabs become 4 spaces; other control characters are dropped.

/// How much of a file is read for its preview (enough for hundreds of lines; quick on a
/// share).
pub const READ_BYTES: usize = 64 * 1024;
/// Lines kept (the full-screen preview scrolls through them).
pub const MAX_LINES: usize = 2_000;

/// Whether a file is worth reading as text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextKind {
    /// A known text / code extension.
    Text,
    /// No extension, or one we don't know: read it and see.
    Maybe,
    /// A known binary format (pictures, PDFs, Office, archives, programs…).
    Binary,
}

/// Text and code: the editor list (`open::rules::TEXT_EXTS`) plus a few more.
const MORE_TEXT: &[&str] = &[
    "vb",
    "bas",
    "cls",
    "frm",
    "vba",
    "vbproj",
    "sln",
    "csproj",
    "props",
    "targets",
    "resx",
    "xaml",
    "config",
    "manifest",
    "psd1",
    "ps1xml",
    "lst",
    "out",
    "err",
    "nfo",
    "diz",
    "sql",
    "properties",
    "gradle",
    "kts",
    "dockerfile",
    "makefile",
    "cmake",
    "tf",
    "hcl",
    "rtf",
    "srt",
    "sub",
    "tex",
    "bib",
    "plist",
    "svg",
    "pmlobj",
    "pmlcmd",
    "dat",
];

/// Never read as text.
const BINARY: &[&str] = &[
    "exe", "dll", "sys", "msi", "msp", "cab", "zip", "7z", "rar", "gz", "tgz", "bz2", "xz", "iso",
    "img", "vhd", "vhdx", "png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff", "ico", "heic",
    "psd", "pdf", "doc", "docx", "xls", "xlsx", "xlsm", "ppt", "pptx", "one", "msg", "pst", "ost",
    "mdb", "accdb", "mp3", "mp4", "mkv", "avi", "mov", "wmv", "wav", "flac", "dwg", "dxf", "dgn",
    "rvt", "nwd", "nwc", "ifc", "step", "stp", "igs", "sat", "db", "sqlite", "lnk", "url", "ttf",
    "otf", "woff", "woff2", "class", "jar", "pyc", "o", "obj", "lib", "pdb", "bin", "dmp",
];

pub fn kind(name: &str) -> TextKind {
    let ext = match name.rsplit_once('.') {
        Some((stem, e)) if !stem.is_empty() => e.to_ascii_lowercase(),
        _ => return TextKind::Maybe,
    };
    if BINARY.contains(&ext.as_str()) {
        return TextKind::Binary;
    }
    let known = crate::open::rules::TEXT_EXTS
        .split(',')
        .map(str::trim)
        .chain(MORE_TEXT.iter().copied())
        .any(|e| e == ext);
    if known {
        TextKind::Text
    } else {
        TextKind::Maybe
    }
}

/// The first lines of a file, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextPreview {
    pub lines: Vec<String>,
    /// `UTF-8`, `UTF-16 LE`, `Windows-1252`…
    pub encoding: &'static str,
    /// The file goes on past what was read.
    pub truncated: bool,
}

/// Decodes the first bytes of a file (`whole`: they're all of it). None: it isn't text
/// (`kind` was `Maybe` and the bytes look binary).
pub fn decode(bytes: &[u8], whole: bool, kind: TextKind) -> Option<TextPreview> {
    let (text, encoding) = if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        (String::from_utf8_lossy(rest).into_owned(), "UTF-8 (BOM)")
    } else if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        (utf16(rest, false), "UTF-16 LE")
    } else if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        (utf16(rest, true), "UTF-16 BE")
    } else if looks_utf16le(bytes) {
        (utf16(bytes, false), "UTF-16 LE")
    } else {
        // Zero bytes in what claims to be text: a binary file (unless it's known text).
        let sample = &bytes[..bytes.len().min(8 * 1024)];
        if kind != TextKind::Text && sample.contains(&0) {
            return None;
        }
        match std::str::from_utf8(bytes) {
            Ok(s) => (s.to_string(), "UTF-8"),
            // Cut mid-character at the end of what was read: still UTF-8.
            Err(e) if !whole && e.error_len().is_none() && e.valid_up_to() + 4 > bytes.len() => (
                String::from_utf8_lossy(&bytes[..e.valid_up_to()]).into_owned(),
                "UTF-8",
            ),
            Err(_) => (bytes.iter().map(|&b| cp1252(b)).collect(), "Windows-1252"),
        }
    };
    let mut lines: Vec<String> = text.split('\n').map(clean_line).collect();
    // A file ending in a newline has no empty last line; a partial read drops the cut one.
    if lines.last().is_some_and(String::is_empty) || (!whole && lines.len() > 1) {
        lines.pop();
    }
    let truncated = !whole || lines.len() > MAX_LINES;
    lines.truncate(MAX_LINES);
    Some(TextPreview {
        lines,
        encoding,
        truncated,
    })
}

/// ASCII text saved as UTF-16 LE without a BOM: zero every other byte.
fn looks_utf16le(b: &[u8]) -> bool {
    let n = b.len().min(512) & !1;
    n >= 8 && b[..n].chunks(2).filter(|c| c[1] == 0 && c[0] != 0).count() * 10 >= (n / 2) * 9
}

fn utf16(b: &[u8], big_endian: bool) -> String {
    let units: Vec<u16> = b
        .chunks_exact(2)
        .map(|c| {
            if big_endian {
                u16::from_be_bytes([c[0], c[1]])
            } else {
                u16::from_le_bytes([c[0], c[1]])
            }
        })
        .collect();
    String::from_utf16_lossy(&units)
}

/// Windows-1252: Latin-1 plus the 0x80–0x9F punctuation (€ ‚ ƒ „ … † ‡ ˆ ‰ Š ‹ Œ Ž ‘ ’ “ ” • – —
/// ˜ ™ š › œ ž Ÿ).
fn cp1252(b: u8) -> char {
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8D}', 'Ž',
        '\u{8F}', '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9D}',
        'ž', 'Ÿ',
    ];
    match b {
        0x80..=0x9F => HIGH[usize::from(b - 0x80)],
        _ => char::from(b),
    }
}

/// `\r` gone, tabs as 4 spaces, other control characters dropped.
fn clean_line(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    for c in line.chars() {
        match c {
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_by_name() {
        for (name, want) in [
            ("deploy.ps1", TextKind::Text),
            ("Module1.VB", TextKind::Text),
            ("run.bat", TextKind::Text),
            ("pipe.pmlmac", TextKind::Text),
            ("app.log", TextKind::Text),
            ("README", TextKind::Maybe),
            ("hosts", TextKind::Maybe),
            ("notes.xyz", TextKind::Maybe),
            (".gitignore", TextKind::Maybe),
            ("setup.exe", TextKind::Binary),
            ("plan.DWG", TextKind::Binary),
            ("photo.jpg", TextKind::Binary),
            ("report.pdf", TextKind::Binary),
        ] {
            assert_eq!(kind(name), want, "{name}");
        }
    }

    #[test]
    fn encodings_and_lines() {
        let t = decode(b"@echo off\r\n\techo hi\r\n", true, TextKind::Text);
        assert_eq!(
            t,
            Some(TextPreview {
                lines: vec!["@echo off".into(), "    echo hi".into()],
                encoding: "UTF-8",
                truncated: false,
            })
        );
        let bom = decode(&[0xEF, 0xBB, 0xBF, b'h', b'i'], true, TextKind::Text);
        assert_eq!(
            bom.map(|t| (t.lines, t.encoding)),
            Some((vec!["hi".into()], "UTF-8 (BOM)"))
        );
        // PowerShell 5's UTF-16 LE, with and without its BOM.
        let mut le = vec![0xFF, 0xFE];
        for c in "Get-Item x\r\n".encode_utf16() {
            le.extend(c.to_le_bytes());
        }
        let t = decode(&le, true, TextKind::Text);
        assert_eq!(t.as_ref().map(|t| t.encoding), Some("UTF-16 LE"));
        assert_eq!(t.map(|t| t.lines), Some(vec!["Get-Item x".to_string()]));
        let t = decode(&le[2..], true, TextKind::Maybe);
        assert_eq!(
            t.map(|t| t.encoding),
            Some("UTF-16 LE"),
            "no BOM, still found"
        );
        // An old ANSI file: é and € in Windows-1252.
        let t = decode(&[b'c', b'a', b'f', 0xE9, b' ', 0x80], true, TextKind::Text);
        assert_eq!(
            t.map(|t| (t.lines, t.encoding)),
            Some((vec!["café €".to_string()], "Windows-1252"))
        );
        // Binary: zero bytes, unknown extension → not text.
        assert_eq!(
            decode(&[b'M', b'Z', 0, 0, 3, 0], true, TextKind::Maybe),
            None
        );
        // Read only partly: the cut last line is dropped, and it says so.
        let t = decode(b"one\ntwo\nthr", false, TextKind::Text);
        assert_eq!(
            t.map(|t| (t.lines, t.truncated)),
            Some((vec!["one".into(), "two".into()], true))
        );
        // A UTF-8 character cut at the end of the read is still UTF-8.
        let mut cut = "é\nx".as_bytes().to_vec();
        cut.extend_from_slice(&"é".as_bytes()[..1]);
        assert_eq!(
            decode(&cut, false, TextKind::Text).map(|t| t.encoding),
            Some("UTF-8")
        );
    }
}
