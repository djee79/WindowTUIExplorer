//! The `g?` overlay's state and keys (docs/HELP_OVERLAY_SPEC.md). The content comes from
//! `crate::help` (built from the live keymap when it opens).
//!
//! - `/` focuses the filter (type to filter keys and commands); `Esc` / `Enter` leave it.
//! - `j` `k` / `C-n` `C-p` / arrows scroll a line, `C-d` `C-u` half a page.
//! - Any other key closes (as before).

use super::{App, Mode};
use crate::help::HelpData;
use crate::prompt::{Edit, LineEditor};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::cell::Cell;

pub struct HelpState {
    /// Everything, unfiltered.
    pub data: HelpData,
    pub filter: LineEditor,
    /// Typing in the filter.
    pub typing: bool,
    pub scroll: usize,
    /// The furthest it can scroll, as last drawn.
    pub max_scroll: Cell<usize>,
    /// Lines in a page, as last drawn.
    pub page: Cell<usize>,
}

impl HelpState {
    /// What's shown: the rows matching the filter.
    pub fn shown(&self) -> HelpData {
        self.data.filter(&self.filter.text())
    }

    fn scroll_by(&mut self, d: isize) {
        let max = self.max_scroll.get();
        self.scroll = self.scroll.saturating_add_signed(d).min(max);
    }
}

impl App {
    /// `g?`.
    pub(super) fn open_help(&mut self) {
        self.help = Some(HelpState {
            data: HelpData::build(&self.keymap),
            filter: LineEditor::new(""),
            typing: false,
            scroll: 0,
            max_scroll: Cell::new(0),
            page: Cell::new(10),
        });
        self.mode = Mode::Help;
    }

    fn close_help(&mut self) {
        self.help = None;
        self.mode = Mode::Normal;
    }

    pub(super) fn on_help_key(&mut self, ev: KeyEvent) {
        let Some(h) = self.help.as_mut() else {
            return self.close_help();
        };
        let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
        let half = (h.page.get() / 2).max(1) as isize;
        match ev.code {
            KeyCode::Char('n') if ctrl => h.scroll_by(1),
            KeyCode::Char('p') if ctrl => h.scroll_by(-1),
            KeyCode::Down => h.scroll_by(1),
            KeyCode::Up => h.scroll_by(-1),
            KeyCode::Char('d') if ctrl => h.scroll_by(half),
            KeyCode::Char('u') if ctrl && !h.typing => h.scroll_by(-half),
            KeyCode::Esc | KeyCode::Enter if h.typing => h.typing = false,
            _ if h.typing => {
                if let Edit::Changed = h.filter.handle(&ev, "") {
                    h.scroll = 0;
                }
            }
            KeyCode::Char('/') => h.typing = true,
            KeyCode::Char('j') => h.scroll_by(1),
            KeyCode::Char('k') => h.scroll_by(-1),
            _ => self.close_help(),
        }
    }
}
