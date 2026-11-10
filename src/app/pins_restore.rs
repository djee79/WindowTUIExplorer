//! `:pins restore`: bring pins.toml back from one of its backups (`pin_backups`).
//!
//! The worker lists the backups, each compared with the current pins; they open in a
//! picker ("Pin backups") whose preview says what restoring would bring back, remove or
//! change. Enter asks first, then the worker backs up the current file and puts the
//! chosen one in place; the app takes the restored pins at once.

use super::App;
use super::tags::Confirm;
use crate::picker::{ItemKind, PickItem, Picker, Source};

/// `2026-11-09 20:15 · 12 min ago`.
pub fn when(taken: chrono::NaiveDateTime, now: chrono::NaiveDateTime) -> String {
    let mins = now.signed_duration_since(taken).num_minutes().max(0);
    let ago = match mins {
        0 => "just now".to_string(),
        1..=59 => format!("{mins} min ago"),
        60..=2879 => format!("{} h ago", mins / 60),
        _ => format!("{} days ago", mins / 1440),
    };
    format!("{} · {ago}", taken.format("%Y-%m-%d %H:%M"))
}

impl App {
    /// A backup shown in the restore picker (its preview).
    pub fn pin_backup(&self, idx: usize) -> Option<&crate::pin_backups::Listed> {
        self.pin_backups.get(idx)
    }

    /// `:pins restore`: ask the worker for the backups.
    pub(super) fn open_pin_backups(&mut self) {
        self.worker.list_pin_backups();
        self.set_status("reading the pin backups…".into(), false);
    }

    /// The backups arrived: the picker, or why there's nothing to show.
    pub(super) fn on_pin_backups(&mut self, found: Vec<crate::pin_backups::Listed>) {
        if found.is_empty() {
            return self.set_status(
                r"no pin backups yet: one is kept before each editing session (newest 5), in %APPDATA%\sentier\backups"
                    .into(),
                false,
            );
        }
        let now = chrono::Local::now().naive_local();
        let items = found
            .iter()
            .enumerate()
            .map(|(id, l)| PickItem {
                id,
                name: when(l.backup.taken, now),
                detail: match &l.diff {
                    Ok(d) => d.summary(),
                    Err(e) => format!("doesn't load: {e}"),
                },
                kind: ItemKind::Backup,
                key: None,
                tags: Vec::new(),
                unc: false,
                local: false,
                reachable: None,
                server: None,
                score: 0,
            })
            .collect();
        self.pin_backups = found;
        self.set_status(String::new(), false);
        let picker = Picker::new(
            "Pin backups",
            Source::PinBackups,
            items,
            self.worker.waker(),
            "",
        );
        self.show_picker(picker);
    }

    /// Enter on a backup: ask, saying what changes.
    pub(super) fn ask_restore_pins(&mut self, idx: usize) {
        let Some(l) = self.pin_backups.get(idx) else {
            return;
        };
        let d = match &l.diff {
            Ok(d) => d.clone(),
            Err(e) => {
                return self.set_status(format!("that backup doesn't load: {e}"), true);
            }
        };
        let now = chrono::Local::now().naive_local();
        let why = format!(
            "Restore the pins from {}? ({}; your current pins are backed up first)",
            when(l.backup.taken, now),
            d.summary()
        );
        self.close_picker();
        self.confirm = Some(Confirm::RestorePins { idx, why });
        self.open_prompt_with(super::keys::PromptKind::Confirm, "");
    }

    /// Yes: the worker swaps the files.
    pub(super) fn restore_pin_backup(&mut self, idx: usize) {
        let Some(l) = self.pin_backups.get(idx) else {
            return;
        };
        let label = l.backup.taken.format("%Y-%m-%d %H:%M").to_string();
        self.worker.restore_pins(l.backup.path.clone(), label);
        self.set_status("restoring pins…".into(), false);
    }

    pub(super) fn on_pins_restored(
        &mut self,
        result: Result<(crate::pins::PinStore, String), String>,
    ) {
        match result {
            Ok((store, label)) => {
                let n = store.pins.len();
                self.pins_pending = Some(store);
                self.apply_pending_pins();
                self.set_status(
                    format!(
                        "restored {} from {label} (the pins it replaced are a backup too: :pins restore)",
                        crate::pins::pins_word(n)
                    ),
                    false,
                );
            }
            Err(e) => self.set_status(format!("couldn't restore pins: {e}"), true),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::when;

    #[test]
    fn backup_times_read_naturally() {
        let at = |s: &str| {
            chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap_or_default()
        };
        let now = at("2026-11-09 20:30");
        assert_eq!(
            when(at("2026-11-09 20:30"), now),
            "2026-11-09 20:30 · just now"
        );
        assert_eq!(
            when(at("2026-11-09 20:18"), now),
            "2026-11-09 20:18 · 12 min ago"
        );
        assert_eq!(
            when(at("2026-11-09 08:30"), now),
            "2026-11-09 08:30 · 12 h ago"
        );
        assert_eq!(
            when(at("2026-11-02 20:30"), now),
            "2026-11-02 20:30 · 7 days ago"
        );
    }
}
