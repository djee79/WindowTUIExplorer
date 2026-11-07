//! Pick mode: choosing a folder by going there (a sync preset's destination, a backup's
//! zips folder, a preset's source when editing). The explorer works as usual: `h` `l`,
//! `'` pin jumps, `gl`, `Space Space`, `:cd`, tabs. `p` takes the folder you're in
//! (it's where the sync "pastes"); `Esc` cancels. A banner on the header says what's
//! being picked.

use super::App;
use super::keys::PresetField;
use super::keys::PromptKind;
use crate::fs::path::Loc;

/// What's being picked, for the banner.
#[derive(Debug, Clone)]
pub struct Pick {
    pub field: PresetField,
    /// `sync HIT → ?`, `backup HIT → ?`.
    pub title: String,
}

impl Pick {
    /// The banner's instruction.
    pub fn ask(&self) -> &'static str {
        match self.field {
            PresetField::Dest => "go to the destination (the local copy)",
            PresetField::DestDir => "go to the folder the zips go into",
            PresetField::Source => "go to the folder to copy from",
            _ => "go to the folder",
        }
    }
}

/// Fields chosen by going to a folder rather than typing.
pub fn is_pick_field(f: PresetField) -> bool {
    matches!(
        f,
        PresetField::Source | PresetField::Dest | PresetField::DestDir
    )
}

impl App {
    /// Starts picking `field` for the pin being drafted, at `start` if given.
    pub(super) fn start_pick(&mut self, field: PresetField, start: Option<Loc>) {
        let title = match &self.pin_draft {
            Some(d) => {
                let src = d
                    .pin
                    .extra
                    .get("source")
                    .and_then(|v| v.as_str())
                    .map(|s| s.trim_end_matches('\\').rsplit('\\').next().unwrap_or(s))
                    .unwrap_or("?")
                    .to_string();
                let n = d
                    .pin
                    .extra
                    .get("include")
                    .and_then(|v| v.as_array())
                    .map_or(0, Vec::len);
                let what = if n > 0 {
                    format!(" ({n} selected)")
                } else {
                    String::new()
                };
                match field {
                    PresetField::Source => format!("{} {}", d.pin.kind.label(), d.pin.name),
                    _ => format!("{} {src}{what} → ?", d.pin.kind.label()),
                }
            }
            None => return,
        };
        self.pick = Some(Pick { field, title });
        if let Some(loc) = start.filter(|l| *l != self.cwd) {
            self.navigate(loc, None);
        }
        self.set_status(
            "move around as usual; p takes the folder you're in, Esc cancels".into(),
            false,
        );
        self.dirty = true;
    }

    /// `p` while picking: this folder. A refusal (inside the source…) keeps picking.
    pub(super) fn choose_pick(&mut self) {
        let Some(pick) = self.pick.take() else {
            return;
        };
        let Some(path) = self.cwd.real().as_fs().map(|p| p.display()) else {
            self.pick = Some(pick);
            return self.set_status(
                "a real folder, please: Locations and search results can't be picked".into(),
                true,
            );
        };
        if let Err(e) = self.pin_submit(PromptKind::Preset(pick.field), &path) {
            self.pick = Some(pick);
            self.set_status(e, true);
        }
        self.dirty = true;
    }

    /// `Esc` while picking: the whole preset is dropped (or the edit left unchanged).
    pub(super) fn cancel_pick(&mut self) {
        if let Some(pick) = self.pick.take() {
            self.pin_cancel(PromptKind::Preset(pick.field));
        }
        self.dirty = true;
    }
}
