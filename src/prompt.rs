//! The line editor behind every prompt (`:` command line, `/` `?` search, `f` filter;
//! rename later). CLAUDE.md "Prompts":
//!
//! - **insert** (the starting mode): readline basics: `Ctrl-w` delete word, `Ctrl-u` delete
//!   to start, `Ctrl-a` / `Ctrl-e` start / end, `Ctrl-r Ctrl-w` insert the name under the
//!   cursor, arrows, Home/End, Backspace (on an empty line it cancels, like vim's `:`).
//! - **normal** (`Esc` from insert): `h l w b e 0 $ x`, `i a I A`, `dw cw ciw C D`, `u`.
//!   A second `Esc` cancels.
//! - `Enter` confirms in either mode.
//!
//! Pure state; no I/O, no drawing.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditMode {
    Insert,
    Normal,
}

/// What a key did to the line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edit {
    /// Cursor moved or mode changed, text is the same.
    Moved,
    Changed,
    Submit,
    Cancel,
    /// Not an editing key (the caller may use it, e.g. Tab or Up/Down).
    Ignored,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    None,
    /// `d` or `c` waiting for its motion.
    Op(char),
    /// `ci` / `di` waiting for `w`.
    OpInner(char),
    /// `Ctrl-r` waiting for `Ctrl-w`.
    Register,
}

#[derive(Debug, Clone)]
pub struct LineEditor {
    chars: Vec<char>,
    /// Insert: 0..=len. Normal: 0..len (0 when empty).
    cursor: usize,
    pub mode: EditMode,
    undo: Vec<(Vec<char>, usize)>,
    pending: Pending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Space,
    Word,
    Punct,
}

fn class(c: char) -> Class {
    if c.is_whitespace() {
        Class::Space
    } else if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else {
        Class::Punct
    }
}

impl LineEditor {
    pub fn new(text: &str) -> LineEditor {
        let chars: Vec<char> = text.chars().collect();
        LineEditor {
            cursor: chars.len(),
            chars,
            mode: EditMode::Insert,
            undo: Vec::new(),
            pending: Pending::None,
        }
    }

    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    /// Cursor position in chars.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Replaces the whole line (Tab completion), cursor at the end, insert mode.
    pub fn set_text(&mut self, text: &str) {
        self.snapshot();
        self.chars = text.chars().collect();
        self.cursor = self.chars.len();
        self.mode = EditMode::Insert;
    }

    pub fn handle(&mut self, key: &KeyEvent, word_under_cursor: &str) -> Edit {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if key.code == KeyCode::Enter {
            self.pending = Pending::None;
            return Edit::Submit;
        }
        match self.mode {
            EditMode::Insert => self.insert_key(key.code, ctrl, word_under_cursor),
            EditMode::Normal => self.normal_key(key.code, ctrl),
        }
    }

    fn snapshot(&mut self) {
        self.undo.push((self.chars.clone(), self.cursor));
        if self.undo.len() > 100 {
            self.undo.remove(0);
        }
    }

    fn insert_key(&mut self, code: KeyCode, ctrl: bool, word: &str) -> Edit {
        if std::mem::replace(&mut self.pending, Pending::None) == Pending::Register {
            if ctrl && code == KeyCode::Char('w') {
                for c in word.chars() {
                    self.chars.insert(self.cursor, c);
                    self.cursor += 1;
                }
                return Edit::Changed;
            }
            return Edit::Moved;
        }
        match (code, ctrl) {
            (KeyCode::Esc, _) => {
                self.mode = EditMode::Normal;
                self.cursor = self.cursor.saturating_sub(1);
                self.clamp();
                Edit::Moved
            }
            (KeyCode::Char('w'), true) => {
                let start = self.word_back(self.cursor);
                self.delete(start, self.cursor)
            }
            (KeyCode::Char('u'), true) => self.delete(0, self.cursor),
            (KeyCode::Char('a'), true) | (KeyCode::Home, _) => self.move_to(0),
            (KeyCode::Char('e'), true) | (KeyCode::End, _) => self.move_to(self.chars.len()),
            (KeyCode::Char('r'), true) => {
                self.pending = Pending::Register;
                Edit::Moved
            }
            (KeyCode::Char('h'), true) | (KeyCode::Backspace, _) => {
                if self.chars.is_empty() {
                    return Edit::Cancel;
                }
                if self.cursor == 0 {
                    return Edit::Moved;
                }
                self.delete(self.cursor - 1, self.cursor)
            }
            (KeyCode::Delete, _) => self.delete(self.cursor, (self.cursor + 1).min(self.len())),
            (KeyCode::Left, _) => self.move_to(self.cursor.saturating_sub(1)),
            (KeyCode::Right, _) => self.move_to((self.cursor + 1).min(self.len())),
            (KeyCode::Char(c), false) => {
                self.chars.insert(self.cursor, c);
                self.cursor += 1;
                Edit::Changed
            }
            _ => Edit::Ignored,
        }
    }

    fn normal_key(&mut self, code: KeyCode, ctrl: bool) -> Edit {
        let KeyCode::Char(c) = code else {
            return match code {
                KeyCode::Esc => Edit::Cancel,
                KeyCode::Left => self.normal_move(self.cursor.saturating_sub(1)),
                KeyCode::Right => self.normal_move(self.cursor + 1),
                KeyCode::Home => self.normal_move(0),
                KeyCode::End => self.normal_move(self.len()),
                KeyCode::Backspace => self.normal_move(self.cursor.saturating_sub(1)),
                _ => Edit::Ignored,
            };
        };
        if ctrl {
            return Edit::Ignored;
        }
        match std::mem::replace(&mut self.pending, Pending::None) {
            Pending::Op(op) => return self.operator(op, c),
            Pending::OpInner(op) => {
                return if c == 'w' {
                    let (s, e) = self.inner_word();
                    self.apply_op(op, s, e)
                } else {
                    Edit::Moved
                };
            }
            Pending::Register | Pending::None => {}
        }
        match c {
            'h' => self.normal_move(self.cursor.saturating_sub(1)),
            'l' => self.normal_move(self.cursor + 1),
            'w' => self.normal_move(self.word_fwd(self.cursor)),
            'b' => self.normal_move(self.word_back(self.cursor)),
            'e' => self.normal_move(self.word_end(self.cursor)),
            '0' => self.normal_move(0),
            '$' => self.normal_move(self.len()),
            'x' => {
                if self.chars.is_empty() {
                    return Edit::Moved;
                }
                self.snapshot();
                self.chars.remove(self.cursor);
                self.clamp();
                Edit::Changed
            }
            'i' => self.enter_insert(self.cursor),
            'a' => self.enter_insert((self.cursor + 1).min(self.len())),
            'I' => self.enter_insert(0),
            'A' => self.enter_insert(self.len()),
            'd' | 'c' => {
                self.pending = Pending::Op(c);
                Edit::Moved
            }
            'D' => self.apply_op('d', self.cursor, self.len()),
            'C' => self.apply_op('c', self.cursor, self.len()),
            'u' => match self.undo.pop() {
                Some((chars, cursor)) => {
                    self.chars = chars;
                    self.cursor = cursor;
                    self.clamp();
                    Edit::Changed
                }
                None => Edit::Moved,
            },
            _ => Edit::Ignored,
        }
    }

    /// `d`/`c` + motion.
    fn operator(&mut self, op: char, motion: char) -> Edit {
        let from = self.cursor;
        let to = match motion {
            'i' => {
                self.pending = Pending::OpInner(op);
                return Edit::Moved;
            }
            // `cw` on a word changes to its end, like `ce` (vim's special case).
            'w' if op == 'c' && self.chars.get(from).is_some_and(|c| !c.is_whitespace()) => {
                self.word_end(from) + 1
            }
            'w' => self.word_fwd(from),
            'e' => self.word_end(from) + 1,
            'b' => return self.apply_op(op, self.word_back(from), from),
            '$' => self.len(),
            '0' => return self.apply_op(op, 0, from),
            _ => return Edit::Moved,
        };
        self.apply_op(op, from, to.min(self.len()))
    }

    fn apply_op(&mut self, op: char, from: usize, to: usize) -> Edit {
        self.snapshot();
        let (from, to) = (from.min(self.len()), to.min(self.len()));
        self.chars.drain(from..to);
        self.cursor = from;
        if op == 'c' {
            self.mode = EditMode::Insert;
        } else {
            self.clamp();
        }
        Edit::Changed
    }

    fn enter_insert(&mut self, at: usize) -> Edit {
        // One undo step per insert session, as in vim.
        self.snapshot();
        self.cursor = at;
        self.mode = EditMode::Insert;
        Edit::Moved
    }

    fn normal_move(&mut self, to: usize) -> Edit {
        self.cursor = to;
        self.clamp();
        Edit::Moved
    }

    fn move_to(&mut self, to: usize) -> Edit {
        self.cursor = to.min(self.len());
        Edit::Moved
    }

    fn delete(&mut self, from: usize, to: usize) -> Edit {
        if from >= to {
            return Edit::Moved;
        }
        self.chars.drain(from..to);
        self.cursor = from;
        Edit::Changed
    }

    fn len(&self) -> usize {
        self.chars.len()
    }

    /// Normal mode keeps the cursor on a character.
    fn clamp(&mut self) {
        let max = match self.mode {
            EditMode::Insert => self.len(),
            EditMode::Normal => self.len().saturating_sub(1),
        };
        self.cursor = self.cursor.min(max);
    }

    /// Start of the next word (vim `w`).
    fn word_fwd(&self, from: usize) -> usize {
        let n = self.len();
        let mut i = from;
        if i >= n {
            return n;
        }
        let start = class(self.chars[i]);
        if start != Class::Space {
            while i < n && class(self.chars[i]) == start {
                i += 1;
            }
        }
        while i < n && class(self.chars[i]) == Class::Space {
            i += 1;
        }
        i
    }

    /// Start of this or the previous word (vim `b`, readline Ctrl-w).
    fn word_back(&self, from: usize) -> usize {
        let mut i = from.min(self.len());
        while i > 0 && class(self.chars[i - 1]) == Class::Space {
            i -= 1;
        }
        if i == 0 {
            return 0;
        }
        let c = class(self.chars[i - 1]);
        while i > 0 && class(self.chars[i - 1]) == c {
            i -= 1;
        }
        i
    }

    /// End of this or the next word (vim `e`), as a char index.
    fn word_end(&self, from: usize) -> usize {
        let n = self.len();
        if n == 0 {
            return 0;
        }
        let mut i = (from + 1).min(n - 1);
        while i < n - 1 && class(self.chars[i]) == Class::Space {
            i += 1;
        }
        let c = class(self.chars[i]);
        while i + 1 < n && class(self.chars[i + 1]) == c {
            i += 1;
        }
        i
    }

    /// The run of same-class chars under the cursor (vim `iw`), as `start..end`.
    fn inner_word(&self) -> (usize, usize) {
        let n = self.len();
        if n == 0 {
            return (0, 0);
        }
        let at = self.cursor.min(n - 1);
        let c = class(self.chars[at]);
        let mut s = at;
        while s > 0 && class(self.chars[s - 1]) == c {
            s -= 1;
        }
        let mut e = at + 1;
        while e < n && class(self.chars[e]) == c {
            e += 1;
        }
        (s, e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs keys in vim-ish notation: plain chars, `<Esc>`, `<CR>`, `<BS>`, `<C-x>`.
    fn run(start: &str, keys: &str) -> (String, usize, EditMode, Edit) {
        let mut ed = LineEditor::new(start);
        let mut last = Edit::Moved;
        let mut rest = keys;
        while !rest.is_empty() {
            let (key, len) = if let Some(end) = rest.strip_prefix('<').and_then(|r| r.find('>')) {
                let name = &rest[1..end + 1];
                let k = match name {
                    "Esc" => KeyEvent::from(KeyCode::Esc),
                    "CR" => KeyEvent::from(KeyCode::Enter),
                    "BS" => KeyEvent::from(KeyCode::Backspace),
                    "Left" => KeyEvent::from(KeyCode::Left),
                    "Home" => KeyEvent::from(KeyCode::Home),
                    n if n.starts_with("C-") => KeyEvent::new(
                        KeyCode::Char(n.chars().nth(2).unwrap_or('?')),
                        KeyModifiers::CONTROL,
                    ),
                    n => panic!("key {n}"),
                };
                (k, end + 2)
            } else {
                let c = rest.chars().next().unwrap_or('?');
                (KeyEvent::from(KeyCode::Char(c)), c.len_utf8())
            };
            last = ed.handle(&key, "hit000");
            rest = &rest[len..];
        }
        (ed.text(), ed.cursor(), ed.mode, last)
    }

    fn text(start: &str, keys: &str) -> String {
        run(start, keys).0
    }

    #[test]
    fn insert_mode_table() {
        let cases: &[(&str, &str, &str)] = &[
            ("", "abc", "abc"),
            ("cd C:\\work", "<C-w>", "cd C:\\"),
            ("cd foo bar", "<C-w>", "cd foo "),
            ("cd foo bar", "<C-w><C-w>", "cd "),
            ("cd C:\\a b", "<C-u>", ""),
            ("abc", "<C-a>X", "Xabc"),
            ("abc", "<C-a><C-e>Y", "abcY"),
            ("abc", "<BS>", "ab"),
            ("abc", "<Left>Z", "abZc"),
            ("cd ", "<C-r><C-w>", "cd hit000"),
            ("cd ", "<C-r>x", "cd "),
        ];
        for (start, keys, want) in cases {
            assert_eq!(text(start, keys), *want, "{start:?} + {keys}");
        }
        assert_eq!(
            run("", "<BS>").3,
            Edit::Cancel,
            "Backspace on empty cancels"
        );
        assert_eq!(run("x", "<CR>").3, Edit::Submit);
    }

    #[test]
    fn normal_mode_table() {
        // Cursor after `<Esc>` sits on the last char.
        let cases: &[(&str, &str, &str)] = &[
            ("abc", "<Esc>x", "ab"),
            ("abc", "<Esc>0x", "bc"),
            ("abc", "<Esc>0iX", "Xabc"),
            ("abc", "<Esc>0aX", "aXbc"),
            ("abc", "<Esc>IX", "Xabc"),
            ("abc", "<Esc>0AX", "abcX"),
            ("cd foo bar", "<Esc>0dw", "foo bar"),
            ("cd foo bar", "<Esc>0wdw", "cd bar"),
            ("cd foo bar", "<Esc>0wcwX", "cd X bar"),
            ("cd foo bar", "<Esc>0wwciwY", "cd foo Y"),
            ("cd foo bar", "<Esc>0wC", "cd "),
            ("cd foo bar", "<Esc>0wD", "cd "),
            ("cd foo bar", "<Esc>bdiw", "cd foo "),
            ("cd foo bar", "<Esc>0wwde", "cd foo "),
            ("cd foo bar", "<Esc>$db", "cd foo r"),
            ("one", "<Esc>xxu", "on"),
            ("one", "<Esc>xxuu", "one"),
            ("one", "<Esc>0iAB<Esc>u", "one"),
            (r"cd C:\work\proj", "<Esc>bdiw", r"cd C:\work\"),
        ];
        for (start, keys, want) in cases {
            assert_eq!(text(start, keys), *want, "{start:?} + {keys}");
        }
    }

    #[test]
    fn modes_and_cancel() {
        let (_, cursor, mode, last) = run("abc", "<Esc>");
        assert_eq!((cursor, mode, last), (2, EditMode::Normal, Edit::Moved));
        assert_eq!(
            run("abc", "<Esc><Esc>").3,
            Edit::Cancel,
            "second Esc cancels"
        );
        assert_eq!(run("abc", "<Esc><CR>").3, Edit::Submit, "Enter in normal");
        let (_, cursor, ..) = run("abc", "<Esc>$l");
        assert_eq!(cursor, 2, "normal cursor stays on a char");
        let (_, cursor, ..) = run("cd foo", "<Esc>0e");
        assert_eq!(cursor, 1, "e to end of word");
        let (_, cursor, ..) = run("cd foo", "<Esc>0w");
        assert_eq!(cursor, 3, "w to next word");
    }
}
