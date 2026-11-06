//! Previews (CLAUDE.md section 11): pictures in the preview column.
//!
//! What the probe (`examples/image_probe.rs`) found under WezTerm on Windows, with ConPTY
//! in between:
//! - The terminal query never answers (ConPTY doesn't pass the replies back), so the
//!   protocol is chosen from the environment, and the cell size is a setting (10×20 px).
//! - iTerm2 inline images work; sixel and kitty don't show. Halfblocks always work.
//! - An iTerm2 picture stays on screen until its cells are written again: ratatui skips
//!   those cells (the image widget marks them "skip"), and a full-screen clear doesn't
//!   remove it either. So when a picture moves or goes away, the UI loop rewrites the
//!   old picture's cells ([`repaint_cells`]), which erases it without a flicker.
//! - An iTerm2 picture lands wherever the real terminal's cursor is. ratatui hides the
//!   cursor, and ConPTY then doesn't send cursor moves at all, so a picture drawn inside
//!   ratatui's frame lands wherever ConPTY last painted (`examples/image_place.rs`: wrong
//!   half the time). So the UI only marks where the picture goes, and the UI loop writes
//!   it after the frame with the cursor shown ([`place_iterm2`]): right every time.
//!
//! Decoding and encoding run on the blocking pool ([`image::load`]); the UI only renders
//! an already-encoded picture.

pub mod image;

use crate::fs::path::Loc;
use ratatui::layout::Rect;

use ratatui_image::picker::{Picker, ProtocolType};
use std::collections::VecDeque;
use std::time::SystemTime;

/// The cell size in pixels when the terminal can't tell us (it never can through ConPTY).
pub const DEFAULT_CELL_PX: (u16, u16) = (10, 20);
/// Network files bigger than this aren't previewed automatically.
pub const MAX_NETWORK_BYTES: u64 = 50 * 1024 * 1024;
/// Rendered pictures kept in memory.
pub const CACHE_ENTRIES: usize = 64;

/// What the preview column can show for a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewKind {
    Image,
}

impl PreviewKind {
    pub fn of(name: &str) -> Option<PreviewKind> {
        let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
        match ext.as_str() {
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tif" | "tiff" | "ico" => {
                Some(PreviewKind::Image)
            }
            _ => None,
        }
    }
}

/// The graphics protocol to draw with. `override_` is a protocol name (`halfblocks`,
/// `iterm2`, `sixel`, `kitty`); otherwise WezTerm gets iTerm2 and everything else gets
/// halfblocks, which work everywhere.
pub fn choose_protocol(term_program: Option<&str>, override_: Option<&str>) -> ProtocolType {
    match override_.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("halfblocks") => return ProtocolType::Halfblocks,
        Some("iterm2") => return ProtocolType::Iterm2,
        Some("sixel") => return ProtocolType::Sixel,
        Some("kitty") => return ProtocolType::Kitty,
        _ => {}
    }
    match term_program {
        Some(t) if t.eq_ignore_ascii_case("wezterm") => ProtocolType::Iterm2,
        _ => ProtocolType::Halfblocks,
    }
}

/// The picker the worker encodes with. Reads two environment variables, no I/O:
/// `TERM_PROGRAM`, and `SENTIER_PREVIEW_PROTOCOL` to force one until config.toml's
/// `preview.protocol` exists.
pub fn graphics() -> Picker {
    // `halfblocks()` assumes 10×20 px cells, our default ([`DEFAULT_CELL_PX`]). Another
    // size (config `preview.cell_px`) needs the deprecated `from_fontsize`; the terminal
    // query that replaces it doesn't work through ConPTY.
    let mut picker = Picker::halfblocks();
    let cell = picker.font_size();
    debug_assert_eq!((cell.width, cell.height), DEFAULT_CELL_PX);
    let term = std::env::var("TERM_PROGRAM").ok();
    let forced = std::env::var("SENTIER_PREVIEW_PROTOCOL").ok();
    picker.set_protocol_type(choose_protocol(term.as_deref(), forced.as_deref()));
    picker
}

/// What a rendered picture depends on: the file (by size and time, so a changed file is
/// read again) and the box it was fitted to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ImageKey {
    pub loc: Loc,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub cols: u16,
    pub rows: u16,
}

/// A small least-recently-used cache: the most recent at the front.
pub struct Lru<K, V> {
    cap: usize,
    items: VecDeque<(K, V)>,
}

impl<K: PartialEq, V: Clone> Lru<K, V> {
    pub fn new(cap: usize) -> Self {
        Lru {
            cap: cap.max(1),
            items: VecDeque::new(),
        }
    }

    pub fn get(&mut self, key: &K) -> Option<V> {
        let i = self.items.iter().position(|(k, _)| k == key)?;
        let item = self.items.remove(i)?;
        let v = item.1.clone();
        self.items.push_front(item);
        Some(v)
    }

    pub fn put(&mut self, key: K, value: V) {
        if let Some(i) = self.items.iter().position(|(k, _)| *k == key) {
            self.items.remove(i);
        }
        self.items.push_front((key, value));
        self.items.truncate(self.cap);
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.items.len()
    }
}

/// The cells of `old` (where a picture was) that aren't inside `new` (where the current
/// picture is): rewriting them erases the old picture. Row by row.
pub fn repaint_cells(old: Rect, new: Option<Rect>) -> Vec<(u16, u16)> {
    let mut out = Vec::new();
    for y in old.top()..old.bottom() {
        for x in old.left()..old.right() {
            let covered = new
                .is_some_and(|n| x >= n.left() && x < n.right() && y >= n.top() && y < n.bottom());
            if !covered {
                out.push((x, y));
            }
        }
    }
    out
}

/// The escape sequence of an iTerm2 picture; None for the other protocols, which draw
/// through ratatui's buffer.
pub fn iterm2_data(p: &image::Picture) -> Option<&str> {
    match p {
        image::Picture::Iterm2 { data, .. } => Some(data),
        image::Picture::Cells(_) => None,
    }
}

/// Writes an iTerm2 picture at `at`, after the frame. The probe (`examples/image_place.rs`)
/// showed this is the only reliable way through ConPTY: with the cursor hidden, ConPTY
/// doesn't send cursor moves to the terminal, so the picture would land wherever ConPTY
/// last painted. Showing the cursor for the write makes it move for real.
pub fn place_iterm2(out: &mut impl std::io::Write, at: Rect, data: &str) -> std::io::Result<()> {
    use ratatui::crossterm::cursor::{Hide, MoveTo, Show};
    use ratatui::crossterm::queue;
    use ratatui::crossterm::style::Print;
    queue!(out, Show, MoveTo(at.x, at.y), Print(data), Hide)?;
    out.flush()
}

/// Too big to read over the network just because the cursor passed by.
pub fn too_big_for_network(size: u64, network: bool) -> bool {
    network && size > MAX_NETWORK_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds() {
        for name in [
            "a.png",
            "B.JPG",
            "c.jpeg",
            "x.webp",
            "scan.TIFF",
            "i.ico",
            "g.gif",
        ] {
            assert_eq!(PreviewKind::of(name), Some(PreviewKind::Image), "{name}");
        }
        for name in ["a.pdf", "png", "a.png.txt", "dwg", ""] {
            assert_eq!(PreviewKind::of(name), None, "{name}");
        }
    }

    #[test]
    fn protocol_choice() {
        assert_eq!(choose_protocol(Some("WezTerm"), None), ProtocolType::Iterm2);
        assert_eq!(choose_protocol(None, None), ProtocolType::Halfblocks);
        assert_eq!(
            choose_protocol(Some("vscode"), None),
            ProtocolType::Halfblocks
        );
        assert_eq!(
            choose_protocol(Some("WezTerm"), Some(" Halfblocks ")),
            ProtocolType::Halfblocks,
            "the override wins"
        );
        assert_eq!(choose_protocol(None, Some("sixel")), ProtocolType::Sixel);
        assert_eq!(
            choose_protocol(Some("WezTerm"), Some("nonsense")),
            ProtocolType::Iterm2,
            "an unknown override is ignored"
        );
    }

    #[test]
    fn lru_keeps_the_most_recent() {
        let mut c: Lru<u32, &str> = Lru::new(2);
        c.put(1, "a");
        c.put(2, "b");
        assert_eq!(c.get(&1), Some("a"), "1 is now the most recent");
        c.put(3, "c");
        assert_eq!(c.get(&2), None, "2 was the least recent");
        assert_eq!(c.get(&1), Some("a"));
        assert_eq!(c.get(&3), Some("c"));
        c.put(3, "c2");
        assert_eq!((c.get(&3), c.len()), (Some("c2"), 2));
    }

    #[test]
    fn repaint_skips_the_new_picture() {
        let old = Rect::new(0, 0, 3, 2);
        assert_eq!(repaint_cells(old, None).len(), 6);
        // The new picture covers the left column.
        let cells = repaint_cells(old, Some(Rect::new(0, 0, 1, 5)));
        assert_eq!(cells, [(1, 0), (2, 0), (1, 1), (2, 1)]);
        assert!(repaint_cells(old, Some(old)).is_empty());
    }

    #[test]
    fn iterm2_is_placed_with_the_cursor_shown() {
        let mut out: Vec<u8> = Vec::new();
        assert!(place_iterm2(&mut out, Rect::new(69, 2, 10, 5), "PIC").is_ok());
        let s = String::from_utf8_lossy(&out);
        // Show cursor, move (1-based row;col), the picture, hide cursor.
        assert_eq!(s, "\x1b[?25h\x1b[3;70HPIC\x1b[?25l");
    }

    #[test]
    fn network_guard() {
        assert!(too_big_for_network(MAX_NETWORK_BYTES + 1, true));
        assert!(!too_big_for_network(MAX_NETWORK_BYTES + 1, false));
        assert!(!too_big_for_network(1024, true));
    }
}
