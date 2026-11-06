//! The launcher's extras (CLAUDE.md section 6): importing wtm's entries, the Start Menu's
//! apps, pinning a URL. The launcher itself is a picker (`picker.rs`, `open_launcher`).
//!
//! - `:import-wtm` (and, at first run, an offer): wtm's `[[launcher]]` entries become
//!   pins (groups → first tag, `--app=` → web pins, folders → folder pins), skipping what
//!   is pinned already; wtm's launch history comes along.
//! - `^s` in the launcher: the Start Menu's apps in or out of the list. Enter runs one,
//!   `^a` pins it.
//! - `^a` on anything else in the launcher, or `:pin url [URL]`: a web app pin.

use super::keys::PromptKind;
use super::tags::Confirm;
use super::{App, Mode};
use crate::launcher::entry::{self, WtmEntry};
use crate::picker::{ItemKind, PickItem, Source};
use crate::pins::Pin;

impl App {
    /// `:import-wtm`.
    pub(super) fn import_wtm_command(&mut self) {
        if !self.pins_ready() {
            return;
        }
        self.set_status("reading wtm's launcher entries…".into(), false);
        self.worker.wtm_read(false);
    }

    #[allow(clippy::type_complexity)]
    pub(super) fn on_wtm_found(
        &mut self,
        result: Result<(Vec<(WtmEntry, Pin)>, crate::frecency::Usage, Vec<String>), String>,
        offer: bool,
    ) {
        let (pairs, usage, problems) = match result {
            Ok(r) => r,
            Err(e) => {
                if !offer {
                    self.set_status(format!(":import-wtm: {e}"), true);
                }
                return;
            }
        };
        for p in &problems {
            tracing::warn!(problem = %p, "wtm import");
        }
        let total = pairs.len();
        let new = entry::new_pins(pairs, &self.pins.pins);
        if new.is_empty() {
            if !offer {
                self.set_status(
                    format!("nothing new: all {total} wtm entries are pinned already"),
                    false,
                );
            }
            return;
        }
        // Only when nothing else is asking: the first-run offer waits for no one.
        if self.mode != Mode::Normal || self.confirm.is_some() {
            return;
        }
        let usage = entry::convert_usage(&usage, &new);
        let mut extra = Vec::new();
        if total > new.len() {
            extra.push(format!("{} already pinned", total - new.len()));
        }
        if !problems.is_empty() {
            extra.push(format!("{} unreadable, see the log", problems.len()));
        }
        let extra = if extra.is_empty() {
            String::new()
        } else {
            format!(" ({})", extra.join(", "))
        };
        let why = format!(
            "Import {} launcher entries from wtm{extra}? Groups become tags; wtm's hotkeys \
             are dropped.",
            new.len()
        );
        self.confirm = Some(Confirm::ImportWtm {
            pins: new.into_iter().map(|(_, p)| p).collect(),
            usage: Box::new(usage),
            offer,
            why,
        });
        self.open_prompt_with(PromptKind::Confirm, "");
    }

    pub(super) fn import_wtm(&mut self, pins: Vec<Pin>, usage: crate::frecency::Usage) {
        let n = pins.len();
        self.pins.pins.extend(pins);
        self.usage.merge(usage);
        self.save_pins();
        self.worker.save_usage(self.usage.clone());
        self.set_status(
            format!("imported {n} entries from wtm · Space Space opens the launcher"),
            false,
        );
    }

    // -------------------------------------------------------------- Start Menu

    pub(super) fn start_menu_items(&self) -> Vec<PickItem> {
        let now = crate::frecency::now_secs();
        let Some(apps) = &self.start_menu else {
            return Vec::new();
        };
        apps.iter()
            .enumerate()
            .map(|(id, (name, path))| PickItem {
                id,
                name: name.clone(),
                detail: path.clone(),
                kind: ItemKind::StartMenu,
                key: None,
                tags: Vec::new(),
                unc: false,
                local: true,
                reachable: None,
                server: None,
                score: self.usage.score(&path.to_lowercase(), now),
            })
            .collect()
    }

    /// `^s` in the launcher: the Start Menu's apps in or out.
    pub(super) fn toggle_start_menu(&mut self) {
        let Some(p) = self
            .picker
            .as_ref()
            .filter(|p| p.source == Source::Launcher)
        else {
            return;
        };
        let text = p.editor.text();
        self.launcher_start_menu = !self.launcher_start_menu;
        if self.launcher_start_menu && self.start_menu.is_none() {
            self.worker.scan_start_menu();
        }
        self.close_picker();
        self.open_launcher(&text);
        let msg = if self.launcher_start_menu {
            "Start Menu apps shown (^s hides them)"
        } else {
            "Start Menu apps hidden"
        };
        self.set_status(msg.into(), false);
    }

    pub(super) fn on_start_menu(&mut self, apps: crate::launcher::startmenu::Apps) {
        self.start_menu = Some(apps);
        if self.launcher_start_menu
            && self
                .picker
                .as_ref()
                .is_some_and(|p| p.source == Source::Launcher)
        {
            let items = self.start_menu_items();
            if let Some(p) = self.picker.as_mut() {
                p.add(items);
            }
            self.dirty = true;
        }
    }

    /// Enter on a Start Menu app: its shortcut, as the Start Menu runs it.
    pub(super) fn launch_start_menu(&mut self, id: usize) {
        let Some((name, path)) = self.start_menu.as_ref().and_then(|a| a.get(id)).cloned() else {
            return;
        };
        self.usage
            .record(&path.to_lowercase(), crate::frecency::now_secs());
        self.worker.save_usage(self.usage.clone());
        self.worker.launch(
            format!("started {name}"),
            crate::launcher::Launch {
                command: path,
                args: String::new(),
                dir: String::new(),
                find: false,
            },
        );
    }
}
