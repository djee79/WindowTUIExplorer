//! The picture preview (CLAUDE.md section 11) in the app: which picture the preview wants,
//! asking the worker for it, the in-memory cache, preloading, and the full-screen view.
//!
//! - A single step asks at once. While keys stream in (holding `j`), only after the
//!   cursor has been still for [`IMAGE_STILL`], so not every file passed is decoded.
//! - The box comes from the UI's last frame ([`App::image_area`]); when it changes (window
//!   resized, full screen) the picture is fitted again.
//! - Answers for a picture the cursor has left are dropped (the key differs), but kept in
//!   the cache, so coming back is instant. "No thumbnail" answers are remembered too, so
//!   the shell isn't asked again.
//! - Preloading (Yazi's idea): once the current picture is shown, the previous and next
//!   previewable files, and the next page of a PDF, are prepared into the cache. A request
//!   already on its way is never sent twice.
//! - `zp` full screen: the preview takes the whole body. `j`/`k` move to the next file,
//!   `]p`/`[p` page, `+`/`-` zoom, `=` resets, `H J K L` pan, `zp`/`Esc`/`q` go back.

use super::{App, Mode};
use crate::fs::Entry;
use crate::fs::path::Loc;
use crate::preview::image::ImagePreview;
use crate::preview::{ImageKey, PreviewKind, View};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Cursor stillness before a picture is read, while keys stream in.
pub const IMAGE_STILL: Duration = Duration::from_millis(60);
/// How far the preloader looks for the previous / next previewable file.
const PRELOAD_SCAN: usize = 20;

/// The picture the preview column shows (or is waiting for).
#[derive(Debug, Clone)]
pub struct ImageState {
    pub key: ImageKey,
    /// None while the worker is on it.
    pub result: Option<Result<Arc<ImagePreview>, String>>,
}

/// Keys pending in the full-screen preview (`zp`, `]p`, `[p`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FullPending {
    Z,
    Next,
    Prev,
}

impl App {
    /// The key for previewing `e` (in the cwd listing) in the UI's box at `view`.
    fn key_for(&self, e: &Entry, view: View) -> Option<ImageKey> {
        if e.is_container() {
            return None;
        }
        let kind = PreviewKind::of(&e.name)?;
        let area = self.image_area?;
        if area.width == 0 || area.height == 0 {
            return None;
        }
        let loc = self.cwd.join(&e.name).ok()?;
        let page = match kind {
            PreviewKind::Pdf => self.pdf_pages.get(&loc).copied().unwrap_or(0),
            PreviewKind::Image | PreviewKind::Thumbnail => 0,
        };
        Some(ImageKey {
            loc,
            size: e.size,
            modified: e.modified,
            page,
            cols: area.width,
            rows: area.height,
            view,
        })
    }

    /// The picture the preview should show now.
    fn image_target(&self) -> Option<ImageKey> {
        if self.mode == Mode::Picker || self.at_root() {
            return None;
        }
        self.key_for(self.selected()?, self.pview)
    }

    /// Sends `key` to the worker unless it's cached, remembered as missing, or already
    /// on its way.
    fn request_image(&mut self, key: ImageKey) {
        if self.image_cache.get(&key).is_some()
            || self.image_misses.get(&key).is_some()
            || self.image_inflight.contains(&key)
        {
            return;
        }
        self.image_inflight.insert(key.clone());
        self.worker.image_preview(key, self.graphics.clone());
    }

    /// From `tick`: once the cursor has been still long enough, make sure the picture the
    /// preview wants is shown or on its way.
    pub(super) fn image_tick(&mut self, now: Instant) {
        if now < self.image_due {
            return;
        }
        let Some(key) = self.image_target() else {
            return;
        };
        if self.image.as_ref().is_some_and(|s| s.key == key) {
            return;
        }
        let known = match (self.image_cache.get(&key), self.image_misses.get(&key)) {
            (Some(hit), _) => Some(Ok(hit)),
            (None, Some(miss)) => Some(Err(miss)),
            (None, None) => None,
        };
        let hit = known.is_some();
        self.image = Some(ImageState {
            key: key.clone(),
            result: known,
        });
        self.dirty = true;
        if hit {
            self.preload();
        } else {
            // A preload may already be fetching it: then its answer fills this in.
            self.request_image(key);
        }
    }

    pub(super) fn on_image_preview(
        &mut self,
        key: ImageKey,
        result: Result<Arc<ImagePreview>, String>,
    ) {
        self.image_inflight.remove(&key);
        match &result {
            Ok(p) => self.image_cache.put(key.clone(), Arc::clone(p)),
            // A timeout may pass (a slow share); anything else won't change by asking again.
            Err(e) if !e.contains("timed out") => self.image_misses.put(key.clone(), e.clone()),
            Err(_) => {}
        }
        if let Some(s) = self.image.as_mut().filter(|s| s.key == key) {
            s.result = Some(result);
            self.dirty = true;
            self.preload();
        }
    }

    /// Prepares the previous and next previewable files, and the next PDF page, into the
    /// cache. Not while zoomed: those pictures are cropped to this file.
    fn preload(&mut self) {
        if self.pview.zoomed() {
            return;
        }
        let mut keys = Vec::new();
        // The next page of the PDF shown.
        if let Some(s) = &self.image
            && let Some(Ok(p)) = &s.result
            && let Some(info) = &p.pdf
            && info.page + 1 < info.count
        {
            keys.push(ImageKey {
                page: info.page + 1,
                ..s.key.clone()
            });
        }
        // The nearest previewable file on each side.
        if let Some(listing) = self.view_listing.clone() {
            for forward in [true, false] {
                let rows = (1..=PRELOAD_SCAN).filter_map(|d| {
                    if forward {
                        self.cursor.checked_add(d)
                    } else {
                        self.cursor.checked_sub(d)
                    }
                });
                let next = rows
                    .filter_map(|row| self.view.get(row))
                    .filter_map(|&i| listing.entries.get(i))
                    .find_map(|e| self.key_for(e, View::default()));
                keys.extend(next);
            }
        }
        for key in keys {
            self.request_image(key);
        }
    }

    /// The picture state for the file at `loc`, if it's the one held.
    pub fn image_of(&self, loc: &Loc) -> Option<&ImageState> {
        self.image.as_ref().filter(|s| &s.key.loc == loc)
    }

    /// Something is being read for the preview: the spinner keeps ticking.
    pub(super) fn image_busy(&self) -> bool {
        self.image.as_ref().is_some_and(|s| s.result.is_none())
    }

    // ------------------------------------------------------------------ PDF pages

    /// `]p` / `[p` (with a count): page through the PDF under the cursor. Stops at the
    /// first and last page.
    pub(super) fn step_page(&mut self, n: usize, forward: bool) {
        let Some((loc, count)) = self.pdf_under_cursor() else {
            return;
        };
        let now = self.pdf_pages.get(&loc).copied().unwrap_or(0);
        let n = u32::try_from(n).unwrap_or(u32::MAX);
        let last = count.map_or(u32::MAX, |c| c.saturating_sub(1));
        let page = if forward {
            now.saturating_add(n).min(last)
        } else {
            now.saturating_sub(n)
        };
        if page == now {
            let edge = if forward { "last" } else { "first" };
            self.set_status(format!("already on the {edge} page"), false);
            return;
        }
        self.pdf_pages.insert(loc, page);
        self.dirty = true;
    }

    /// `gP` / `{n}gP`: go to page `n` (1-based) of the PDF under the cursor.
    pub(super) fn goto_page(&mut self, n: usize) {
        let Some((loc, count)) = self.pdf_under_cursor() else {
            return;
        };
        let page = u32::try_from(n.max(1) - 1).unwrap_or(u32::MAX);
        let page = count.map_or(page, |c| page.min(c.saturating_sub(1)));
        self.pdf_pages.insert(loc, page);
        self.dirty = true;
    }

    /// The PDF under the cursor, and its page count once a page has been shown.
    fn pdf_under_cursor(&mut self) -> Option<(Loc, Option<u32>)> {
        let e = self.selected().filter(|e| !e.is_container());
        let Some(e) = e.filter(|e| PreviewKind::of(&e.name) == Some(PreviewKind::Pdf)) else {
            self.set_status("]p [p gP page through a PDF under the cursor".into(), false);
            return None;
        };
        let loc = self.cwd.join(&e.name).ok()?;
        let count = self
            .image_of(&loc)
            .and_then(|s| s.result.as_ref())
            .and_then(|r| r.as_ref().ok())
            .and_then(|p| p.pdf.as_ref())
            .map(|i| i.count);
        Some((loc, count))
    }

    // ---------------------------------------------------------------- full screen

    /// `zp`: the preview takes the whole body (and back).
    pub(super) fn toggle_full_preview(&mut self) {
        if self.mode == Mode::Preview {
            return self.leave_full_preview();
        }
        let previewable = self
            .selected()
            .is_some_and(|e| !e.is_container() && PreviewKind::of(&e.name).is_some())
            || self.cursor_text_lines().is_some();
        if !previewable {
            self.set_status(
                "zp: no picture, PDF, text or thumbnail under the cursor".into(),
                false,
            );
            return;
        }
        self.mode = Mode::Preview;
        self.pview = View::default();
        self.full_count = None;
        self.full_pending = None;
        self.dirty = true;
    }

    fn leave_full_preview(&mut self) {
        self.mode = Mode::Normal;
        self.pview = View::default();
        self.dirty = true;
    }

    /// Keys in the full-screen preview.
    pub(super) fn on_preview_key(&mut self, ev: KeyEvent) {
        let text = self.cursor_text_lines().is_some();
        let half = (self.page.max(2) / 2) as isize;
        if ev.modifiers.contains(KeyModifiers::CONTROL) {
            // A text file scrolls half a page; pictures have no use for these.
            match ev.code {
                KeyCode::Char('d') if text => self.scroll_text(half),
                KeyCode::Char('u') if text => self.scroll_text(-half),
                _ => {}
            }
            return;
        }
        if text && self.full_pending.is_none() {
            match ev.code {
                KeyCode::Char('J') => return self.scroll_text(half),
                KeyCode::Char('K') => return self.scroll_text(-half),
                _ => {}
            }
        }
        let pending = self.full_pending.take();
        let count = self.full_count.take();
        let n = count.unwrap_or(1).max(1);
        let ni = isize::try_from(n).unwrap_or(isize::MAX);
        match (pending, ev.code) {
            (Some(FullPending::Z), KeyCode::Char('p')) => self.leave_full_preview(),
            (Some(FullPending::Next), KeyCode::Char('p')) => self.step_page(n, true),
            (Some(FullPending::Prev), KeyCode::Char('p')) => self.step_page(n, false),
            (Some(_), _) => {}
            (None, KeyCode::Char(c)) if c.is_ascii_digit() && (c != '0' || count.is_some()) => {
                let d = c as usize - '0' as usize;
                self.full_count = Some((count.unwrap_or(0) * 10 + d).min(99_999));
            }
            (None, KeyCode::Esc | KeyCode::Char('q')) => self.leave_full_preview(),
            (None, KeyCode::Char('z')) => self.full_pending = Some(FullPending::Z),
            (None, KeyCode::Char(']')) => {
                self.full_pending = Some(FullPending::Next);
                self.full_count = count;
            }
            (None, KeyCode::Char('[')) => {
                self.full_pending = Some(FullPending::Prev);
                self.full_count = count;
            }
            (None, KeyCode::Char('j') | KeyCode::Down) => self.move_cursor(ni),
            (None, KeyCode::Char('k') | KeyCode::Up) => self.move_cursor(-ni),
            (None, KeyCode::Char('+')) => self.set_view(self.pview.step_zoom(true)),
            (None, KeyCode::Char('-')) => self.set_view(self.pview.step_zoom(false)),
            (None, KeyCode::Char('=')) => self.set_view(View::default()),
            (None, KeyCode::Char('H')) => self.set_view(self.pview.pan(-1, 0)),
            (None, KeyCode::Char('L')) => self.set_view(self.pview.pan(1, 0)),
            (None, KeyCode::Char('K')) => self.set_view(self.pview.pan(0, -1)),
            (None, KeyCode::Char('J')) => self.set_view(self.pview.pan(0, 1)),
            _ => {}
        }
        self.dirty = true;
    }

    /// The full-screen zoom in percent (100 when not zoomed).
    pub fn zoom(&self) -> u16 {
        self.pview.zoom
    }

    fn set_view(&mut self, view: View) {
        if view != self.pview {
            self.pview = view;
            self.dirty = true;
        }
    }
}

// ------------------------------------------------------------------ text

/// The text preview of the file under the cursor: its first lines once read.
pub struct TextState {
    pub loc: Loc,
    /// None while reading; Ok(None): not text after all.
    pub result: Option<Result<Option<Box<crate::preview::text::TextPreview>>, String>>,
    /// First line shown (the full-screen preview scrolls).
    pub scroll: usize,
}

impl App {
    /// When the cursor settles on a file that may be text: read its first lines.
    pub(super) fn request_text_preview(&mut self) {
        use crate::preview::text::{TextKind, kind};
        let Some(e) = self.selected() else { return };
        if e.is_container() || self.search_state(&self.cwd).is_some_and(|s| s.content) {
            return;
        }
        let k = kind(&e.name);
        if k == TextKind::Binary {
            return;
        }
        let Ok(loc) = self.cwd.join(&e.name) else {
            return;
        };
        if loc.as_fs().is_none() || self.text_preview.as_ref().is_some_and(|t| t.loc == loc) {
            return;
        }
        self.text_preview = Some(TextState {
            loc: loc.clone(),
            result: None,
            scroll: 0,
        });
        self.worker.text_preview(loc, k);
    }

    pub(super) fn on_text_preview(
        &mut self,
        loc: Loc,
        result: Result<Option<Box<crate::preview::text::TextPreview>>, String>,
    ) {
        // A late answer for a file the cursor has left is dropped.
        if let Some(t) = self.text_preview.as_mut().filter(|t| t.loc == loc) {
            t.result = Some(result);
            self.dirty = true;
        }
    }

    /// The text preview of `loc`, if it's text and read.
    pub fn text_of(&self, loc: &Loc) -> Option<(&crate::preview::text::TextPreview, usize)> {
        let t = self.text_preview.as_ref().filter(|t| &t.loc == loc)?;
        match &t.result {
            Some(Ok(Some(tp))) => Some((tp, t.scroll)),
            _ => None,
        }
    }

    /// The text under the cursor, if it's shown as text.
    fn cursor_text_lines(&self) -> Option<usize> {
        let e = self.selected()?;
        let loc = self.cwd.join(&e.name).ok()?;
        self.text_of(&loc).map(|(tp, _)| tp.lines.len())
    }

    /// `J` / `K` / `^d` / `^u` in the full-screen text preview.
    fn scroll_text(&mut self, by: isize) {
        let lines = self.cursor_text_lines().unwrap_or(0);
        let page = (self.page.max(2)) as isize;
        if let Some(t) = self.text_preview.as_mut() {
            let max = lines.saturating_sub(page as usize / 2);
            t.scroll = t.scroll.saturating_add_signed(by).min(max);
            self.dirty = true;
        }
    }
}

impl App {
    /// Whether the file under the cursor shows as text (the hint row).
    pub fn cursor_is_text(&self) -> bool {
        self.cursor_text_lines().is_some()
    }
}
