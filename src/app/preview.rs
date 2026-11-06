//! The picture preview (CLAUDE.md section 11) in the app: which picture the preview column
//! wants, asking the worker for it, and the in-memory cache.
//!
//! - Asked only after the cursor has been still for [`IMAGE_STILL`], so holding `j` doesn't
//!   decode every file it passes.
//! - The box comes from the UI's last frame ([`App::image_area`]); when it changes (window
//!   resized) the picture is fitted again.
//! - Answers for a picture the cursor has left are dropped (the key differs), but kept in
//!   the cache, so coming back is instant.

use super::{App, Mode};
use crate::fs::path::Loc;
use crate::preview::image::ImagePreview;
use crate::preview::{ImageKey, PreviewKind};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Cursor stillness before a picture is read.
pub const IMAGE_STILL: Duration = Duration::from_millis(60);

/// The picture the preview column shows (or is waiting for).
#[derive(Debug, Clone)]
pub struct ImageState {
    pub key: ImageKey,
    /// None while the worker is on it.
    pub result: Option<Result<Arc<ImagePreview>, String>>,
}

impl App {
    /// The picture the preview column should show now, fitted to the UI's box.
    fn image_target(&self) -> Option<ImageKey> {
        if self.mode == Mode::Picker || self.at_root() {
            return None;
        }
        let e = self.selected().filter(|e| !e.is_container())?;
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
        })
    }

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

    /// From `tick`: once the cursor has been still long enough, make sure the picture the
    /// preview wants is shown or on its way.
    pub(super) fn image_tick(&mut self, now: Instant) {
        if now.duration_since(self.moved_at) < IMAGE_STILL {
            return;
        }
        let Some(key) = self.image_target() else {
            return;
        };
        if self.image.as_ref().is_some_and(|s| s.key == key) {
            return;
        }
        if let Some(hit) = self.image_cache.get(&key) {
            self.image = Some(ImageState {
                key,
                result: Some(Ok(hit)),
            });
            self.dirty = true;
            return;
        }
        self.image = Some(ImageState {
            key: key.clone(),
            result: None,
        });
        self.dirty = true;
        self.worker.image_preview(key, self.graphics.clone());
    }

    pub(super) fn on_image_preview(
        &mut self,
        key: ImageKey,
        result: Result<Arc<ImagePreview>, String>,
    ) {
        if let Ok(p) = &result {
            self.image_cache.put(key.clone(), Arc::clone(p));
        }
        if let Some(s) = self.image.as_mut().filter(|s| s.key == key) {
            s.result = Some(result);
            self.dirty = true;
        }
    }

    /// The picture state for the file at `loc`, if it's the one held.
    pub fn image_of(&self, loc: &Loc) -> Option<&ImageState> {
        self.image.as_ref().filter(|s| &s.key.loc == loc)
    }

    /// Something is being read: the spinner keeps ticking.
    pub(super) fn image_busy(&self) -> bool {
        self.image.as_ref().is_some_and(|s| s.result.is_none())
    }
}
