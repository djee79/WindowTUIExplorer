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
        PreviewKind::of(&e.name)?;
        let area = self.image_area?;
        if area.width == 0 || area.height == 0 {
            return None;
        }
        Some(ImageKey {
            loc: self.cwd.join(&e.name).ok()?,
            size: e.size,
            modified: e.modified,
            cols: area.width,
            rows: area.height,
        })
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
