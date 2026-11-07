//! The vim-first keymap (CLAUDE.md "Keymap (vim-first)"): key notation, actions, the
//! default normal-mode bindings, and `%APPDATA%\sentier\keymap.toml` overrides.
//!
//! Bindings are data with a `desc`, so the which-key popup and the help overlay are built
//! from the same table the dispatcher uses and can't drift from it. Everything here is
//! pure; the worker reads the file.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::Deserialize;
use std::time::Duration;

pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(300);
const FILE_NAME: &str = "keymap.toml";

// ------------------------------------------------------------------------------- keys

/// One key press, normalized: for characters the case carries Shift, so `G` is
/// `Char('G')` without a shift flag, and `Ctrl-D` is `Char('d')` + ctrl.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key {
    pub code: KeyCode,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Key {
    pub const fn plain(code: KeyCode) -> Key {
        Key {
            code,
            ctrl: false,
            alt: false,
            shift: false,
        }
    }

    pub fn from_event(ev: &KeyEvent) -> Key {
        let m = ev.modifiers;
        let ctrl = m.contains(KeyModifiers::CONTROL);
        let alt = m.contains(KeyModifiers::ALT);
        let shift = m.contains(KeyModifiers::SHIFT);
        match ev.code {
            KeyCode::Char(c) => Key {
                // Ctrl-letters arrive in either case depending on the terminal.
                code: KeyCode::Char(if ctrl { c.to_ascii_lowercase() } else { c }),
                ctrl,
                alt,
                shift: false,
            },
            // Shift-Tab arrives as its own code; don't double-count the shift.
            KeyCode::BackTab => Key {
                code: KeyCode::BackTab,
                ctrl,
                alt,
                shift: false,
            },
            code => Key {
                code,
                ctrl,
                alt,
                shift,
            },
        }
    }

    pub fn is_digit(&self) -> Option<u32> {
        match self.code {
            KeyCode::Char(c) if !self.ctrl && !self.alt => c.to_digit(10),
            _ => None,
        }
    }

    /// Vim notation: `g`, `<C-d>`, `<CR>`, `<Space>`, `<S-Tab>`.
    pub fn notation(&self) -> String {
        let name = match self.code {
            KeyCode::Char(' ') => "Space".to_string(),
            KeyCode::Char('<') => "lt".to_string(),
            KeyCode::Char(c) => c.to_string(),
            KeyCode::Enter => "CR".to_string(),
            KeyCode::Esc => "Esc".to_string(),
            KeyCode::Tab => "Tab".to_string(),
            KeyCode::BackTab => "S-Tab".to_string(),
            KeyCode::Backspace => "BS".to_string(),
            KeyCode::Delete => "Del".to_string(),
            KeyCode::Up => "Up".to_string(),
            KeyCode::Down => "Down".to_string(),
            KeyCode::Left => "Left".to_string(),
            KeyCode::Right => "Right".to_string(),
            KeyCode::Home => "Home".to_string(),
            KeyCode::End => "End".to_string(),
            KeyCode::PageUp => "PageUp".to_string(),
            KeyCode::PageDown => "PageDown".to_string(),
            KeyCode::Insert => "Insert".to_string(),
            KeyCode::F(n) => format!("F{n}"),
            KeyCode::Null => "char".to_string(),
            other => format!("{other:?}"),
        };
        let mods = format!(
            "{}{}{}",
            if self.ctrl { "C-" } else { "" },
            if self.alt { "A-" } else { "" },
            if self.shift { "S-" } else { "" },
        );
        let bare = mods.is_empty() && matches!(self.code, KeyCode::Char(c) if c != ' ' && c != '<');
        if bare {
            name
        } else {
            format!("<{mods}{name}>")
        }
    }
}

/// `<char>` in a binding: any plain character (no Ctrl/Alt), handed to the action.
pub const ANY_CHAR: Key = Key::plain(KeyCode::Null);

impl Key {
    /// The character a `<char>` slot takes: a plain printable key.
    pub fn plain_char(&self) -> Option<char> {
        match self.code {
            KeyCode::Char(c) if !self.ctrl && !self.alt && c != ' ' => Some(c),
            _ => None,
        }
    }
}

/// Binding keys against pressed keys, `<char>` matching any plain character.
fn keys_match(binding: &[Key], pressed: &[Key]) -> bool {
    binding.len() == pressed.len()
        && binding
            .iter()
            .zip(pressed)
            .all(|(b, p)| b == p || (*b == ANY_CHAR && p.plain_char().is_some()))
}

impl Action {
    /// Fills a `<char>` action's character from the key that completed it.
    pub fn with_char(self, last: &Key) -> Action {
        match (self, last.plain_char()) {
            (Action::SetMark(_), Some(c)) => Action::SetMark(c),
            (Action::JumpMark(_), Some(c)) => Action::JumpMark(c),
            (other, _) => other,
        }
    }
}

/// `gg`, `<C-d>`, `<leader>ff`, `<S-CR>` → keys. `<leader>` becomes `leader`.
pub fn parse_keys(s: &str, leader: Key) -> Result<Vec<Key>, String> {
    let mut keys = Vec::new();
    let mut rest = s;
    while let Some(c) = rest.chars().next() {
        if c == '<'
            && let Some(end) = rest.find('>')
            && end > 1
        {
            keys.push(parse_special(&rest[1..end], leader)?);
            rest = &rest[end + 1..];
            continue;
        }
        keys.push(Key::plain(KeyCode::Char(c)));
        rest = &rest[c.len_utf8()..];
    }
    if keys.is_empty() {
        return Err("empty key sequence".to_string());
    }
    Ok(keys)
}

fn parse_special(inner: &str, leader: Key) -> Result<Key, String> {
    if inner.eq_ignore_ascii_case("leader") {
        return Ok(leader);
    }
    let (mut ctrl, mut alt, mut shift) = (false, false, false);
    let mut name = inner;
    // Modifier prefixes: C- A- M- S-, in any order. A bare `-` key is `<->`? Not needed.
    while name.len() > 2 && name.as_bytes()[1] == b'-' {
        match name.as_bytes()[0].to_ascii_uppercase() {
            b'C' => ctrl = true,
            b'A' | b'M' => alt = true,
            b'S' => shift = true,
            _ => return Err(format!("unknown modifier in <{inner}>")),
        }
        name = &name[2..];
    }
    let code = match name.to_ascii_lowercase().as_str() {
        "cr" | "enter" | "return" => KeyCode::Enter,
        "esc" => KeyCode::Esc,
        "tab" if shift => {
            shift = false;
            KeyCode::BackTab
        }
        "tab" => KeyCode::Tab,
        "bs" | "backspace" => KeyCode::Backspace,
        "del" | "delete" => KeyCode::Delete,
        "space" => KeyCode::Char(' '),
        "lt" => KeyCode::Char('<'),
        "bslash" => KeyCode::Char('\\'),
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        "insert" => KeyCode::Insert,
        // `m<char>`: any plain character, passed to the action (vim's `m{a-zA-Z}`).
        "char" => KeyCode::Null,
        f if f.len() > 1 && f.starts_with('f') && f[1..].parse::<u8>().is_ok() => {
            KeyCode::F(f[1..].parse().unwrap_or(1))
        }
        _ => {
            let mut chars = name.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => {
                    // `<C-D>` and `<C-d>` are the same; `<S-a>` is just `A`.
                    let c = if ctrl {
                        c.to_ascii_lowercase()
                    } else if shift {
                        shift = false;
                        c.to_ascii_uppercase()
                    } else {
                        c
                    };
                    KeyCode::Char(c)
                }
                _ => return Err(format!("unknown key <{inner}>")),
            }
        }
    };
    Ok(Key {
        code,
        ctrl,
        alt,
        shift,
    })
}

/// A key sequence in notation, with a leading leader shown as `<leader>`.
pub fn fmt_keys(keys: &[Key], leader: Key) -> String {
    keys.iter()
        .enumerate()
        .map(|(i, k)| {
            if i == 0 && *k == leader && keys.len() > 1 {
                "<leader>".to_string()
            } else {
                k.notation()
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------- actions

/// Everything a binding (or a `:` command) can do. `run` strings in keymap.toml and the
/// `:` command line parse into this through [`Action::parse`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Down,
    Up,
    Parent,
    Open,
    Top,
    Bottom,
    HalfPageDown,
    HalfPageUp,
    PageDown,
    PageUp,
    ScrollMiddle,
    ScrollTop,
    ScrollBottom,
    SearchForward,
    SearchBackward,
    SearchNext,
    SearchPrev,
    Filter,
    Escape,
    Back,
    Forward,
    PreviousDir,
    Locations,
    Downloads,
    VolumeRoot,
    ToggleHidden,
    Refresh,
    Delete,
    CommandLine,
    /// `m<char>`: pin the current folder under that key (a vim mark).
    SetMark(char),
    /// `'<char>`: jump to / open the pin with that key.
    JumpMark(char),
    /// `M`: pin the item under the cursor.
    PinItem,
    /// `T` on a pin in Locations: edit its tags. `r` there renames it.
    EditPinTags,
    /// `r` / `cw`: rename the item under the cursor (in Locations: a pin).
    Rename,
    /// `a`: new file (a trailing `\` makes a folder); `A`: new folder.
    NewFile,
    NewDir,
    /// `D`: delete permanently (asks, red popup).
    DeletePermanent,
    /// `:rename`: the bulk rename panel; `:rename-edit`: the names in `$EDITOR`.
    RenamePanel,
    /// `u` / `:undo-rename [n]`: rename the n newest bulk-rename batches back (asks).
    UndoRename(usize),
    /// `u`: the newest bulk rename in this folder (or below it), back (asks).
    UndoRenameHere,
    BulkRename,
    /// `<leader>p`: the pin picker.
    PinPicker,
    /// `<leader>t` / `:tags`: the tag picker.
    TagPicker,
    /// `<leader>m`: the machine picker (pinned, recent, inventory, `source_cmd`).
    MachinePicker,
    /// `Tab`: toggle the item under the cursor and move down.
    ToggleMark,
    /// `v` / `V`: visual mode (range select with motions).
    Visual,
    /// `yy`: yank (copy) the selection or `n` items.
    Yank,
    /// `x`: cut the selection or `n` items.
    Cut,
    /// `p`: paste here, never replacing; `P`: paste and replace (asks).
    Paste,
    PasteOverwrite,
    /// `J` / `<leader>j`: the jobs panel.
    Jobs,
    /// `.`: repeat the last paste here.
    RepeatPaste,
    /// `:pin`: pin the current folder (name, tags, key).
    PinCwd,
    /// `:tag rename <old> <new>` / `:tag delete <tag>`, across all pins.
    TagRename(String, String),
    TagDelete(String),
    /// `:view save <name>` (the last pin-picker query) / `:view delete <name>`.
    ViewSave(String),
    ViewDelete(String),
    /// `:sync <preset>` (dry-run preview first) / `:sync! <preset>` (update mode skips
    /// it; mirror never does). No name: the sync preset picker.
    Sync(String, bool),
    /// `:backup <preset>` (asks once). No name: the backup preset picker.
    Backup(String),
    /// `O`: the open-with menu.
    OpenWith,
    /// `gR`: reveal in Explorer (the item selected in an Explorer window).
    Reveal,
    /// `Alt-Enter`: Windows Properties.
    Properties,
    /// `yp` / `yP` / `yn`: copy the path / the UNC-resolved path / the name.
    CopyPath,
    CopyUnc,
    CopyName,
    /// `gx` / `gw`: the current folder in Explorer / a new WezTerm tab.
    ExplorerHere,
    TerminalHere,
    /// `:import-wtm`: wtm's launcher entries as pins.
    ImportWtm,
    /// `:pin url [URL]`: a web app pin.
    PinUrl(String),
    /// The finders: `<leader>ff` files, `<leader>fr` recent folders, `<leader>/` grep,
    /// `<leader>:` commands, `<leader>sk` keymaps (`:picker files` etc.).
    FilesPicker,
    DirsPicker,
    GrepPicker,
    CommandsPicker,
    KeymapsPicker,
    /// `go` / `<leader>e`: this folder in the editor.
    EditHere,
    /// `<leader><leader>` / `<C-Space>`: the launcher.
    Launcher,
    /// `<leader>rs`: the pins picker filtered to sync presets.
    SyncPicker,
    /// `g<Space>`: the `:cd ` prompt, ready for a path (Yazi's `g<Space>`).
    GotoPath,
    Help,
    Quit,
    QuitWriteCwd,
    QuitNoWrite,
    /// `<C-l>` / `:redraw`: clear the terminal and draw everything again (vim's redraw).
    Redraw,
    /// `t` / `:tabnew [path]`: a new tab on this folder (or `path`).
    NewTab(String),
    /// `:tabclose`: close this tab (`q` does too, and quits on the last one).
    CloseTab,
    /// `gt` (`{n}gt`: tab n) / `gT`: next / previous tab.
    NextTab,
    PrevTab,
    /// `Alt-1…9`: tab n.
    GotoTab(usize),
    /// `cd <path>` / `e <path>`: any path form, relative to the cwd, `~` for home.
    Cd(String),
    /// `s`: recursive name search below the current folder.
    NameSearch,
    /// `S`: recursive content search below the current folder.
    ContentSearch,
    /// `]p` / `[p` / `gP`: page through the PDF under the cursor (counts work).
    NextPage,
    /// `:net list` / `:net drop <server>` / `:net drop-all`.
    Net(String),
    /// `gm`: the Machines list.
    Machines,
    /// `:connect [path]`: the credential prompt for a share: the path given, the item under
    /// the cursor (Locations: a saved share, a pin, a mapped drive), or the cwd's share.
    Connect(String),
    /// `zp`: full-screen preview (zoom, pan, next file).
    FullPreview,
    PrevPage,
    GotoPage,
    /// `]m` / `[m`: next / previous match in the preview of a content-search result.
    NextMatch,
    PrevMatch,
    /// `gr`: in search results, the real folder of the result under the cursor.
    GotoResultFolder,
    /// `,s`: cycle the results sort.
    CycleSort,
    /// `:{n}`: go to row n.
    GotoLine(usize),
    /// `:set nu` / `:set nonu`.
    SetNumber(bool),
    /// `:set rnu` / `:set nornu`.
    SetRelativeNumber(bool),
}

/// (name, action) for every argument-less action, for keymap.toml and `:`.
pub const NAMED_ACTIONS: &[(&str, Action)] = NAMED;

const NAMED: &[(&str, Action)] = &[
    ("down", Action::Down),
    ("up", Action::Up),
    ("parent", Action::Parent),
    ("open", Action::Open),
    ("top", Action::Top),
    ("bottom", Action::Bottom),
    ("half-page-down", Action::HalfPageDown),
    ("half-page-up", Action::HalfPageUp),
    ("page-down", Action::PageDown),
    ("page-up", Action::PageUp),
    ("scroll-middle", Action::ScrollMiddle),
    ("scroll-top", Action::ScrollTop),
    ("scroll-bottom", Action::ScrollBottom),
    ("search", Action::SearchForward),
    ("search-backward", Action::SearchBackward),
    ("search-next", Action::SearchNext),
    ("search-prev", Action::SearchPrev),
    ("filter", Action::Filter),
    ("escape", Action::Escape),
    ("back", Action::Back),
    ("forward", Action::Forward),
    ("previous-dir", Action::PreviousDir),
    ("locations", Action::Locations),
    ("downloads", Action::Downloads),
    ("volume-root", Action::VolumeRoot),
    ("toggle-hidden", Action::ToggleHidden),
    ("refresh", Action::Refresh),
    ("delete", Action::Delete),
    ("command-line", Action::CommandLine),
    ("set-mark", Action::SetMark('\0')),
    ("jump-mark", Action::JumpMark('\0')),
    ("pin-item", Action::PinItem),
    ("edit-pin-tags", Action::EditPinTags),
    ("rename-pin", Action::Rename),
    ("rename-item", Action::Rename),
    ("new-file", Action::NewFile),
    ("new-dir", Action::NewDir),
    ("delete-permanent", Action::DeletePermanent),
    ("rename", Action::RenamePanel),
    ("undo-rename", Action::UndoRename(1)),
    ("undo-rename-here", Action::UndoRenameHere),
    ("rename-edit", Action::BulkRename),
    ("bulk-rename", Action::BulkRename),
    ("pin-picker", Action::PinPicker),
    ("tag-picker", Action::TagPicker),
    ("machine-picker", Action::MachinePicker),
    ("toggle-select", Action::ToggleMark),
    ("visual", Action::Visual),
    ("yank", Action::Yank),
    ("cut", Action::Cut),
    ("paste", Action::Paste),
    ("paste-overwrite", Action::PasteOverwrite),
    ("jobs", Action::Jobs),
    ("sync-picker", Action::SyncPicker),
    ("launcher", Action::Launcher),
    ("edit-here", Action::EditHere),
    ("import-wtm", Action::ImportWtm),
    ("open-with", Action::OpenWith),
    ("reveal", Action::Reveal),
    ("properties", Action::Properties),
    ("copy-path", Action::CopyPath),
    ("copy-unc", Action::CopyUnc),
    ("copy-name", Action::CopyName),
    ("explorer-here", Action::ExplorerHere),
    ("terminal-here", Action::TerminalHere),
    ("repeat-paste", Action::RepeatPaste),
    ("goto-path", Action::GotoPath),
    ("help", Action::Help),
    ("quit", Action::Quit),
    ("quit-write-cwd", Action::QuitWriteCwd),
    ("quit-no-write", Action::QuitNoWrite),
    ("redraw", Action::Redraw),
    ("tab-new", Action::NewTab(String::new())),
    ("tab-close", Action::CloseTab),
    ("tab-next", Action::NextTab),
    ("tab-prev", Action::PrevTab),
    ("tab-goto", Action::GotoTab(1)),
    ("name-search", Action::NameSearch),
    ("content-search", Action::ContentSearch),
    ("next-page", Action::NextPage),
    ("full-preview", Action::FullPreview),
    ("connect", Action::Connect(String::new())),
    ("machines", Action::Machines),
    ("prev-page", Action::PrevPage),
    ("goto-page", Action::GotoPage),
    ("next-match", Action::NextMatch),
    ("prev-match", Action::PrevMatch),
    ("goto-result-folder", Action::GotoResultFolder),
    ("cycle-sort", Action::CycleSort),
    // `:` command aliases (vim spellings)
    ("q", Action::Quit),
    ("q!", Action::QuitNoWrite),
    ("qa", Action::QuitNoWrite),
    ("tabclose", Action::CloseTab),
    ("tabc", Action::CloseTab),
    ("tabprevious", Action::PrevTab),
    ("tabp", Action::PrevTab),
    ("tabN", Action::PrevTab),
    ("wq", Action::QuitWriteCwd),
    ("x", Action::QuitWriteCwd),
    ("h", Action::Help),
];

/// Commands from the spec that later milestones bring; named so the message says "not
/// yet" instead of "unknown".
const LATER: &[(&str, &str)] = &[];

/// Where a `:` command is listed in `g?` (the fourth column).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CmdGroup {
    GoConnect,
    PinsTags,
    Pickers,
    SyncBackup,
    RenameOptions,
}

impl CmdGroup {
    pub const ALL: [CmdGroup; 5] = [
        CmdGroup::GoConnect,
        CmdGroup::PinsTags,
        CmdGroup::Pickers,
        CmdGroup::SyncBackup,
        CmdGroup::RenameOptions,
    ];

    pub fn title(self) -> &'static str {
        match self {
            CmdGroup::GoConnect => ": GO & CONNECT",
            CmdGroup::PinsTags => ": PINS & TAGS",
            CmdGroup::Pickers => ": PICKERS",
            CmdGroup::SyncBackup => ": SYNC & BACKUP",
            CmdGroup::RenameOptions => ": RENAME & OPTIONS",
        }
    }
}

/// Which key does the same as a command (shown beside it in `g?`, looked up in the live
/// keymap, so remaps show).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyHint {
    /// The action the command itself parses to (`picker files` → `Space f f`).
    Same,
    /// Another action, by its `run` name (`cd <path>` → `goto-path`, `g Space`).
    Via(&'static str),
    None,
}

/// One `:` command: for the hint under the command line, `g?` and the commands picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Command {
    pub name: &'static str,
    /// `<path>` required, `[name]` optional.
    pub args: &'static str,
    pub desc: &'static str,
    pub group: CmdGroup,
    pub key: KeyHint,
}

impl Command {
    /// `cd <path>`, `pin`.
    pub fn line(&self) -> String {
        if self.args.is_empty() {
            self.name.to_string()
        } else {
            format!("{} {}", self.name, self.args)
        }
    }

    /// The action whose key is shown beside it.
    pub fn hint_action(&self) -> Option<Action> {
        match self.key {
            KeyHint::Same => Action::parse(self.name).ok(),
            KeyHint::Via(id) => NAMED.iter().find(|(n, _)| *n == id).map(|(_, a)| a.clone()),
            KeyHint::None => None,
        }
    }
}

const fn cmd(
    name: &'static str,
    args: &'static str,
    desc: &'static str,
    group: CmdGroup,
    key: KeyHint,
) -> Command {
    Command {
        name,
        args,
        desc,
        group,
        key,
    }
}

/// The `:` commands worth knowing, in `g?` order per group. Keep in step with
/// [`Action::parse`] (a test parses every one).
pub const COMMANDS: &[Command] = {
    use CmdGroup::*;
    use KeyHint::{None as NoKey, Same, Via};
    &[
        cmd(
            "cd",
            "<path>",
            "go to a folder (any form, UNC too; Tab completes)",
            GoConnect,
            Via("goto-path"),
        ),
        cmd(
            "connect",
            "[path]",
            "log in to a share (the one under the cursor)",
            GoConnect,
            NoKey,
        ),
        cmd("net list", "", "live network connections", GoConnect, NoKey),
        cmd(
            "net drop",
            "<server>",
            "disconnect from a server",
            GoConnect,
            NoKey,
        ),
        cmd("machines", "", "user PCs", GoConnect, Same),
        cmd(
            "tabnew",
            "[path]",
            "a new tab, here or at path",
            GoConnect,
            Via("tab-new"),
        ),
        cmd(
            "tabnext",
            "[n]",
            "next tab, or tab n",
            GoConnect,
            Via("tab-next"),
        ),
        cmd("tabprevious", "", "previous tab", GoConnect, Same),
        cmd(
            "tabclose",
            "",
            "close this tab (q too, with several tabs)",
            GoConnect,
            NoKey,
        ),
        cmd(
            "pin",
            "",
            "pin this folder (name, tags, key)",
            PinsTags,
            NoKey,
        ),
        cmd(
            "pin url",
            "[URL]",
            "pin a web app (opens as its own window)",
            PinsTags,
            Same,
        ),
        cmd(
            "import-wtm",
            "",
            "import wtm's launcher entries as pins",
            PinsTags,
            Same,
        ),
        cmd("tags", "", "the tag picker", PinsTags, Same),
        cmd(
            "tag rename",
            "<old> <new>",
            "rename a tag on every pin",
            PinsTags,
            NoKey,
        ),
        cmd(
            "tag delete",
            "<tag>",
            "remove a tag from every pin",
            PinsTags,
            NoKey,
        ),
        cmd(
            "view save",
            "<name>",
            "save the last pin-picker query as a view",
            PinsTags,
            NoKey,
        ),
        cmd("launcher", "", "the launcher", Pickers, Same),
        cmd("picker files", "", "find files below here", Pickers, Same),
        cmd("picker dirs", "", "recent folders", Pickers, Same),
        cmd(
            "picker grep",
            "",
            "search file contents below here",
            Pickers,
            Same,
        ),
        cmd(
            "picker commands",
            "",
            "every action, searchable",
            Pickers,
            Same,
        ),
        cmd(
            "picker keymaps",
            "",
            "search the key bindings",
            Pickers,
            Same,
        ),
        cmd(
            "sync",
            "[name]",
            "run a sync preset (dry run first)",
            SyncBackup,
            Via("sync-picker"),
        ),
        cmd(
            "sync!",
            "[name]",
            "run an update sync without the preview",
            SyncBackup,
            NoKey,
        ),
        cmd(
            "sync new",
            "",
            "new sync preset from this folder",
            SyncBackup,
            Same,
        ),
        cmd("backup", "[name]", "run a backup preset", SyncBackup, Same),
        cmd(
            "backup new",
            "",
            "new backup preset from this folder",
            SyncBackup,
            Same,
        ),
        cmd(
            "rename",
            "",
            "bulk rename panel, PowerRename-style (r on a selection)",
            RenameOptions,
            Same,
        ),
        cmd(
            "rename-edit",
            "",
            "bulk rename the names in $EDITOR (Neovim)",
            RenameOptions,
            NoKey,
        ),
        cmd(
            "undo-rename",
            "[n]",
            "rename the last n bulk renames back",
            RenameOptions,
            Via("undo-rename-here"),
        ),
        cmd(
            "redraw",
            "",
            "clear and redraw the screen",
            RenameOptions,
            Same,
        ),
        cmd(
            "set",
            "rnu nornu nu nonu",
            "line numbers",
            RenameOptions,
            NoKey,
        ),
        cmd(
            "q",
            "",
            "quit (wq: and cd the shell here)",
            RenameOptions,
            Via("quit"),
        ),
    ]
};

/// The commands that start with what's typed (else with its first word, so `sync HIT`
/// still shows the sync ones), as `name args — what it does`.
pub fn command_hints(typed: &str) -> Vec<String> {
    let t = typed.trim_start().to_lowercase();
    let word = t.split_whitespace().next().unwrap_or("");
    let pick = |prefix: &str| -> Vec<String> {
        COMMANDS
            .iter()
            .filter(|c| format!("{} {}", c.name, c.args).starts_with(prefix))
            .map(|c| format!("{} — {}", c.line(), c.desc))
            .collect()
    };
    let exact = pick(&t);
    if exact.is_empty() { pick(word) } else { exact }
}

/// Where a binding is listed in `g?`. It follows the action, not the key, so remaps and
/// keymap.toml additions land in the right section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Group {
    Move,
    History,
    Tabs,
    SearchFilter,
    Preview,
    SelectEdit,
    PathsWindows,
    PinsTags,
    GoTo,
    Pickers,
    SyncBackup,
    Session,
}

impl Group {
    /// The order of the sections in `g?`.
    pub const ALL: [Group; 12] = [
        Group::Move,
        Group::History,
        Group::Tabs,
        Group::SearchFilter,
        Group::Preview,
        Group::SelectEdit,
        Group::PathsWindows,
        Group::PinsTags,
        Group::GoTo,
        Group::Pickers,
        Group::SyncBackup,
        Group::Session,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Group::Move => "MOVE",
            Group::History => "HISTORY",
            Group::Tabs => "TABS",
            Group::SearchFilter => "SEARCH & FILTER",
            Group::Preview => "PREVIEW",
            Group::SelectEdit => "SELECT & EDIT",
            Group::PathsWindows => "PATHS & WINDOWS",
            Group::PinsTags => "PINS & TAGS",
            Group::GoTo => "GO TO",
            Group::Pickers => "PICKERS",
            Group::SyncBackup => "SYNC & BACKUP",
            Group::Session => "SESSION",
        }
    }
}

/// Keys that aren't remappable (handled before the keymap), listed in `g?` all the same:
/// (keys as shown, what they do, section).
pub const FIXED: &[(&str, &str, Group)] = &[
    ("C-c", "quit at once", Group::Session),
    ("Esc", "in a prompt: back to normal", Group::Session),
    ("Esc Esc", "in a prompt: cancel", Group::Session),
];

impl Action {
    /// Its `g?` section. No catch-all: a new action has to be given one.
    pub fn group(&self) -> Group {
        use Action::*;
        match self {
            Down | Up | Parent | Open | Top | Bottom | HalfPageDown | HalfPageUp | PageDown
            | PageUp | ScrollMiddle | ScrollTop | ScrollBottom | GotoLine(_) => Group::Move,
            Back | Forward | PreviousDir => Group::History,
            NewTab(_) | CloseTab | NextTab | PrevTab | GotoTab(_) => Group::Tabs,
            SearchForward | SearchBackward | SearchNext | SearchPrev | Filter | NameSearch
            | ContentSearch | GotoResultFolder | CycleSort | Escape => Group::SearchFilter,
            FullPreview | NextPage | PrevPage | GotoPage | NextMatch | PrevMatch => Group::Preview,
            ToggleMark | Visual | Yank | Cut | Paste | PasteOverwrite | RepeatPaste | NewFile
            | NewDir | Rename | UndoRenameHere | UndoRename(_) | RenamePanel | BulkRename
            | Delete | DeletePermanent => Group::SelectEdit,
            CopyPath | CopyUnc | CopyName | OpenWith | Reveal | ExplorerHere | Properties
            | TerminalHere | EditHere => Group::PathsWindows,
            JumpMark(_) | SetMark(_) | PinItem | EditPinTags | PinPicker | TagPicker
            | PinUrl(_) | PinCwd | TagRename(..) | TagDelete(_) | ViewSave(_) | ViewDelete(_) => {
                Group::PinsTags
            }
            Locations | Machines | Downloads | Cd(_) | VolumeRoot | GotoPath | ToggleHidden
            | Help | Net(_) | Connect(_) => Group::GoTo,
            Launcher | FilesPicker | DirsPicker | GrepPicker | CommandsPicker | KeymapsPicker
            | MachinePicker => Group::Pickers,
            Jobs | Sync(..) | Backup(_) | SyncPicker | ImportWtm => Group::SyncBackup,
            Refresh | CommandLine | Quit | QuitWriteCwd | QuitNoWrite | Redraw | SetNumber(_)
            | SetRelativeNumber(_) => Group::Session,
        }
    }
}

impl Action {
    /// Parses a `run` string or a `:` command line (without the `:`).
    pub fn parse(text: &str) -> Result<Action, String> {
        let text = text.trim();
        let (head, arg) = match text.split_once(char::is_whitespace) {
            Some((h, a)) => (h, a.trim()),
            None => (text, ""),
        };
        match head {
            "" => Err("empty command".to_string()),
            "cd" | "e" | "edit" => Ok(Action::Cd(if arg.is_empty() {
                "~".to_string()
            } else {
                arg.to_string()
            })),
            "net" => Ok(Action::Net(arg.to_string())),
            "connect" => Ok(Action::Connect(arg.to_string())),
            "tabnew" | "tabe" | "tabedit" => Ok(Action::NewTab(arg.to_string())),
            "tabnext" | "tabn" if arg.is_empty() => Ok(Action::NextTab),
            "tabnext" | "tabn" => match arg.parse::<usize>() {
                Ok(n) if n > 0 => Ok(Action::GotoTab(n)),
                _ => Err("tabnext [n]: the tab number".to_string()),
            },
            "undo-rename" if arg.is_empty() => Ok(Action::UndoRename(1)),
            "undo-rename" => match arg.parse::<usize>() {
                Ok(n) if n > 0 => Ok(Action::UndoRename(n)),
                _ => Err("undo-rename [n]: how many batches back".to_string()),
            },
            "picker" => match arg {
                "files" => Ok(Action::FilesPicker),
                "dirs" | "recent" => Ok(Action::DirsPicker),
                "grep" => Ok(Action::GrepPicker),
                "commands" => Ok(Action::CommandsPicker),
                "keymaps" => Ok(Action::KeymapsPicker),
                "pins" => Ok(Action::PinPicker),
                "launcher" => Ok(Action::Launcher),
                "tags" => Ok(Action::TagPicker),
                "machines" => Ok(Action::MachinePicker),
                _ => Err("picker files | dirs | grep | commands | keymaps | pins | launcher | tags | machines".to_string()),
            },
            "sync" => Ok(Action::Sync(arg.to_string(), false)),
            "sync!" => Ok(Action::Sync(arg.to_string(), true)),
            "backup" => Ok(Action::Backup(arg.to_string())),
            "pin" if arg.is_empty() => Ok(Action::PinCwd),
            "pin" if arg == "url" || arg.starts_with("url ") => {
                Ok(Action::PinUrl(arg[3..].trim().to_string()))
            }
            "tags" if arg.is_empty() => Ok(Action::TagPicker),
            "tag" => {
                let words: Vec<&str> = arg.split_whitespace().collect();
                match words.as_slice() {
                    ["rename", old, new] => Ok(Action::TagRename(old.to_string(), new.to_string())),
                    ["delete", tag] => Ok(Action::TagDelete(tag.to_string())),
                    _ => Err("tag rename <old> <new> | tag delete <tag>".to_string()),
                }
            }
            "view" => match arg.split_once(char::is_whitespace) {
                Some(("save", name)) if !name.trim().is_empty() => {
                    Ok(Action::ViewSave(name.trim().to_string()))
                }
                Some(("delete", name)) if !name.trim().is_empty() => {
                    Ok(Action::ViewDelete(name.trim().to_string()))
                }
                _ => Err("view save <name> | view delete <name>".to_string()),
            },
            "set" | "se" => match arg {
                "nu" | "number" => Ok(Action::SetNumber(true)),
                "nonu" | "nonumber" => Ok(Action::SetNumber(false)),
                "rnu" | "relativenumber" => Ok(Action::SetRelativeNumber(true)),
                "nornu" | "norelativenumber" => Ok(Action::SetRelativeNumber(false)),
                "" => Err("set what? (nu, nonu, rnu, nornu)".to_string()),
                other => Err(format!("unknown option: {other}")),
            },
            n if n.chars().all(|c| c.is_ascii_digit()) && arg.is_empty() => n
                .parse()
                .map(Action::GotoLine)
                .map_err(|_| format!("bad line number: {n}")),
            _ => {
                if let Some((_, m)) = LATER.iter().find(|(n, _)| *n == head) {
                    return Err(format!("{head} isn't available yet (arrives in {m})"));
                }
                match NAMED.iter().find(|(n, _)| *n == head) {
                    Some(_) if !arg.is_empty() => Err(format!("{head} takes no argument")),
                    Some((_, a)) => Ok(a.clone()),
                    None => Err(format!("unknown command: {head}")),
                }
            }
        }
    }
}

// --------------------------------------------------------------------------- bindings

#[derive(Debug, Clone)]
pub struct Binding {
    pub keys: Vec<Key>,
    pub action: Action,
    pub desc: String,
}

/// What the keys pressed so far add up to.
#[derive(Debug, PartialEq, Eq)]
pub enum Lookup<'a> {
    /// Not the start of any binding.
    None,
    /// The start of one or more longer bindings.
    Prefix,
    /// A whole binding. `longer` says other bindings continue past it (wait for the
    /// timeout before running, as vim does).
    Exact { action: &'a Action, longer: bool },
}

/// One row of the which-key popup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Continuation {
    pub key: Key,
    pub desc: String,
    /// Opens another level instead of running something.
    pub group: bool,
}

#[derive(Debug, Clone)]
pub struct Keymap {
    pub leader: Key,
    pub timeout: Duration,
    /// Earlier bindings win when two share the same keys.
    pub normal: Vec<Binding>,
}

/// The default normal-mode table: (keys, action, desc). An empty desc keeps an alias out
/// of the which-key popup and help (the primary key is listed instead).
fn defaults() -> Vec<(&'static str, Action, &'static str)> {
    use Action::*;
    vec![
        ("j", Down, "down"),
        ("<Down>", Down, ""),
        ("k", Up, "up"),
        ("<Up>", Up, ""),
        ("h", Parent, "parent directory"),
        ("<Left>", Parent, ""),
        ("<BS>", Parent, ""),
        ("-", Parent, "parent directory (vinegar)"),
        ("l", Open, "open: enter dir / run file"),
        ("<Right>", Open, ""),
        ("<CR>", Open, ""),
        // Until open.toml (M5), the first opener is ShellExecute / enter.
        ("o", Open, "open with the first opener"),
        ("O", OpenWith, "open with… (menu)"),
        ("gR", Reveal, "reveal in Explorer"),
        ("<A-CR>", Properties, "Windows Properties"),
        ("yp", CopyPath, "copy path"),
        ("yP", CopyUnc, "copy UNC path (mapped drives resolved)"),
        ("yn", CopyName, "copy name"),
        ("gx", ExplorerHere, "this folder in Explorer"),
        ("gw", TerminalHere, "new WezTerm tab here"),
        ("gg", Top, "top (or line n)"),
        ("<Home>", Top, ""),
        ("G", Bottom, "bottom (or line n)"),
        ("<End>", Bottom, ""),
        ("<C-d>", HalfPageDown, "half page down"),
        ("<C-u>", HalfPageUp, "half page up"),
        ("<C-f>", PageDown, "page down"),
        ("<PageDown>", PageDown, ""),
        ("<C-b>", PageUp, "page up"),
        ("<PageUp>", PageUp, ""),
        ("zz", ScrollMiddle, "cursor line to middle"),
        ("zt", ScrollTop, "cursor line to top"),
        ("zb", ScrollBottom, "cursor line to bottom"),
        ("/", SearchForward, "search forward"),
        ("?", SearchBackward, "search backward"),
        ("n", SearchNext, "next match"),
        ("N", SearchPrev, "previous match"),
        ("f", Filter, "filter this directory"),
        ("s", NameSearch, "search names below here"),
        ("S", ContentSearch, "search contents below here"),
        ("]p", NextPage, "next PDF page"),
        ("zp", FullPreview, "full-screen preview"),
        ("[p", PrevPage, "previous PDF page"),
        ("gP", GotoPage, "go to PDF page"),
        ("]m", NextMatch, "next match in the preview"),
        ("[m", PrevMatch, "previous match in the preview"),
        ("gr", GotoResultFolder, "go to the result's folder"),
        (",s", CycleSort, "cycle results sort"),
        ("<Esc>", Escape, "clear filter / search"),
        ("H", Back, "back in history"),
        ("<C-o>", Back, ""),
        ("L", Forward, "forward in history"),
        ("''", PreviousDir, "previous directory"),
        ("'<char>", JumpMark('\0'), "jump to a pin"),
        ("m<char>", SetMark('\0'), "pin this folder under a key"),
        ("M", PinItem, "pin the item under the cursor"),
        ("T", EditPinTags, "edit a pin's tags (Locations)"),
        ("r", Rename, "rename (Locations: a pin)"),
        ("cw", Rename, "rename"),
        ("u", UndoRenameHere, "undo the last bulk rename here"),
        ("a", NewFile, "new file (end in \\ for a folder)"),
        ("A", NewDir, "new folder"),
        ("D", DeletePermanent, "delete permanently (asks)"),
        ("~", Cd("~".to_string()), "home"),
        ("gl", Locations, "Locations"),
        ("gm", Machines, "Machines (user PCs)"),
        ("gd", Downloads, "Downloads"),
        ("g\\", VolumeRoot, "root of this drive or share"),
        ("g<Space>", GotoPath, "go to a path (any form, UNC too)"),
        ("g.", ToggleHidden, "toggle hidden/system files"),
        ("g?", Help, "help"),
        ("<leader>l", Locations, "Locations"),
        (
            "<leader><leader>",
            Launcher,
            "launcher (apps, web apps, pins)",
        ),
        ("<C-Space>", Launcher, ""),
        ("<leader>p", PinPicker, "pins picker"),
        ("<leader>ff", FilesPicker, "find files below here"),
        ("<leader>e", EditHere, "this folder in the editor (Neovim)"),
        ("go", EditHere, "this folder in the editor"),
        ("<leader>fr", DirsPicker, "recent folders"),
        ("<leader>/", GrepPicker, "grep file contents below here"),
        (
            "<leader>:",
            CommandsPicker,
            "commands (every action, searchable)",
        ),
        ("<leader>sk", KeymapsPicker, "search the key bindings"),
        (
            "<leader>nu",
            PinUrl(String::new()),
            "new web app pin (a URL)",
        ),
        (
            "<leader>ns",
            Sync("new".to_string(), false),
            "new sync preset from this folder",
        ),
        (
            "<leader>nb",
            Backup("new".to_string()),
            "new backup preset from this folder",
        ),
        ("<leader>ni", ImportWtm, "import wtm's launcher entries"),
        ("<leader>t", TagPicker, "tags: pick, filter, rename"),
        ("<leader>m", MachinePicker, "machines picker"),
        ("<leader>j", Jobs, "jobs panel"),
        ("<leader>rs", SyncPicker, "run a sync preset"),
        ("<leader>rb", Backup(String::new()), "run a backup preset"),
        ("J", Jobs, "jobs panel"),
        ("<Tab>", ToggleMark, "select + down"),
        ("v", Visual, "visual (range select)"),
        ("V", Visual, ""),
        ("yy", Yank, "yank (copy)"),
        ("x", Cut, "cut"),
        ("p", Paste, "paste here"),
        ("P", PasteOverwrite, "paste, replacing (asks)"),
        (".", RepeatPaste, "repeat the last paste here"),
        ("R", Refresh, "retry / refresh / re-probe"),
        (
            "dd",
            Delete,
            "delete to the Recycle Bin (Locations: remove)",
        ),
        (":", CommandLine, "command line"),
        ("q", Quit, "quit (with several tabs: close this one)"),
        ("t", NewTab(String::new()), "new tab here"),
        ("gt", NextTab, "next tab ({n}gt: tab n)"),
        ("gT", PrevTab, "previous tab"),
        ("<A-1>", GotoTab(1), "tab 1"),
        ("<A-2>", GotoTab(2), "tab 2"),
        ("<A-3>", GotoTab(3), "tab 3"),
        ("<A-4>", GotoTab(4), "tab 4"),
        ("<A-5>", GotoTab(5), "tab 5"),
        ("<A-6>", GotoTab(6), "tab 6"),
        ("<A-7>", GotoTab(7), "tab 7"),
        ("<A-8>", GotoTab(8), "tab 8"),
        ("<A-9>", GotoTab(9), "tab 9"),
        ("ZZ", QuitWriteCwd, "quit and cd the shell here"),
        ("Q", QuitWriteCwd, ""),
        ("ZQ", QuitNoWrite, "quit without writing the cwd"),
        (
            "<C-l>",
            Redraw,
            "redraw the screen (if a program scribbled on it)",
        ),
    ]
}

/// Labels for prefix keys in the which-key popup.
fn group_label(prefix: &[Key], leader: Key) -> &'static str {
    match prefix {
        [k] if *k == leader => "leader",
        [k] => match k.code {
            KeyCode::Char('g') => "goto",
            KeyCode::Char('z') => "scroll",
            KeyCode::Char('d') => "delete",
            KeyCode::Char('Z') => "quit",
            KeyCode::Char('\'') => "jump to a pin",
            KeyCode::Char('m') => "pin this folder as",
            KeyCode::Char('y') => "yank",
            KeyCode::Char('c') => "change",
            KeyCode::Char(',') => "results",
            _ => "more",
        },
        [l, k] if *l == leader && k.code == KeyCode::Char('r') => "run a preset",
        [l, k] if *l == leader && k.code == KeyCode::Char('n') => "new… (web app, presets)",
        [l, k] if *l == leader && k.code == KeyCode::Char('f') => "find (files, recent folders)",
        [l, k] if *l == leader && k.code == KeyCode::Char('s') => "search (keymaps)",
        _ => "more",
    }
}

impl Default for Keymap {
    fn default() -> Self {
        let leader = Key::plain(KeyCode::Char(' '));
        Keymap {
            leader,
            timeout: DEFAULT_TIMEOUT,
            normal: build_defaults(leader),
        }
    }
}

fn build_defaults(leader: Key) -> Vec<Binding> {
    defaults()
        .into_iter()
        .filter_map(|(keys, action, desc)| {
            let keys = parse_keys(keys, leader).ok()?;
            Some(Binding {
                keys,
                action,
                desc: desc.to_string(),
            })
        })
        .collect()
}

impl Keymap {
    pub fn lookup(&self, pressed: &[Key]) -> Lookup<'_> {
        // An exact binding beats a `<char>` one: `''` wins over `'<char>`.
        let (mut exact, mut wild) = (None, None);
        let mut longer = false;
        for b in &self.normal {
            if b.keys.len() < pressed.len() || !keys_match(&b.keys[..pressed.len()], pressed) {
                continue;
            }
            if b.keys.len() > pressed.len() {
                longer = true;
            } else if b.keys.contains(&ANY_CHAR) {
                wild.get_or_insert(&b.action);
            } else {
                exact.get_or_insert(&b.action);
            }
        }
        match (exact.or(wild), longer) {
            (Some(action), longer) => Lookup::Exact { action, longer },
            (None, true) => Lookup::Prefix,
            (None, false) => Lookup::None,
        }
    }

    /// The action a `<char>` binding would run after `pressed` (`'` → jump-mark), for
    /// the which-key popup to list what the character can be.
    pub fn char_action(&self, pressed: &[Key]) -> Option<&Action> {
        self.normal
            .iter()
            .find(|b| {
                b.keys.len() == pressed.len() + 1
                    && b.keys[pressed.len()] == ANY_CHAR
                    && keys_match(&b.keys[..pressed.len()], pressed)
            })
            .map(|b| &b.action)
    }

    /// The which-key rows after `pressed`, in table order, one per next key.
    pub fn continuations(&self, pressed: &[Key]) -> Vec<Continuation> {
        let mut out: Vec<Continuation> = Vec::new();
        for b in &self.normal {
            if b.keys.len() <= pressed.len() || !keys_match(&b.keys[..pressed.len()], pressed) {
                continue;
            }
            let key = b.keys[pressed.len()];
            // `<char>` continuations are listed by the popup from app state (pins).
            if key == ANY_CHAR {
                continue;
            }
            let group = b.keys.len() > pressed.len() + 1;
            if let Some(existing) = out.iter_mut().find(|c| c.key == key) {
                // A direct binding beats "+group"; a described one beats an alias.
                if !group && (existing.group || existing.desc.is_empty()) {
                    existing.group = group;
                    existing.desc = b.desc.clone();
                }
                continue;
            }
            let desc = if group {
                let mut prefix = pressed.to_vec();
                prefix.push(key);
                group_label(&prefix, self.leader).to_string()
            } else {
                b.desc.clone()
            };
            out.push(Continuation { key, desc, group });
        }
        // Aliases with no description (arrows, <CR>) stay out of the popup.
        out.retain(|c| !c.desc.is_empty());
        out
    }

    /// The key sequence to show for `action`: a described binding before an alias, then
    /// the fewest keys, then the fewest modifiers, then the first in the table. Shown
    /// beside a command in `g?`.
    pub fn shortest(&self, action: &Action) -> Option<Vec<Key>> {
        self.normal
            .iter()
            .filter(|b| b.action == *action)
            .min_by_key(|b| {
                (
                    // An undescribed alias (`C-Space`, arrows) only if nothing else.
                    b.desc.is_empty(),
                    b.keys.len(),
                    b.keys
                        .iter()
                        .filter(|k| k.ctrl || k.alt || !matches!(k.code, KeyCode::Char(_)))
                        .count(),
                )
            })
            .map(|b| b.keys.clone())
    }
}

// ------------------------------------------------------------------------ keymap.toml

#[derive(Debug, Default, Deserialize)]
struct File {
    leader: Option<String>,
    timeout_ms: Option<u64>,
    #[serde(default)]
    normal: ModeTable,
}

#[derive(Debug, Default, Deserialize)]
struct ModeTable {
    #[serde(default)]
    prepend: Vec<Entry>,
    #[serde(default)]
    append: Vec<Entry>,
    /// Replaces the defaults for this mode entirely.
    keymap: Option<Vec<Entry>>,
}

#[derive(Debug, Deserialize)]
struct Entry {
    on: String,
    run: String,
    desc: Option<String>,
}

/// `%APPDATA%\sentier\keymap.toml`.
pub fn store_path() -> Option<std::path::PathBuf> {
    if cfg!(test) {
        return Some(
            std::env::temp_dir()
                .join(format!("sentier-test-store-{}", std::process::id()))
                .join(FILE_NAME),
        );
    }
    let base = std::env::var_os("APPDATA")?;
    Some(
        std::path::PathBuf::from(base)
            .join("sentier")
            .join(FILE_NAME),
    )
}

/// Builds the keymap from keymap.toml text. Never fails: every problem (bad TOML,
/// unknown key or action, a binding that shadows a longer one) is reported and the rest
/// still applies. Tables for modes that don't exist yet (`[picker.insert]`) are ignored.
pub fn from_toml(text: &str) -> (Keymap, Vec<String>) {
    let mut problems = Vec::new();
    let file: File = match toml::from_str(text) {
        Ok(f) => f,
        Err(e) => {
            problems.push(format!("keymap.toml: {}", e.message()));
            return (Keymap::default(), problems);
        }
    };
    let mut leader = Key::plain(KeyCode::Char(' '));
    if let Some(l) = &file.leader {
        match parse_keys(l, leader) {
            Ok(k) if k.len() == 1 => leader = k[0],
            _ => problems.push(format!("keymap.toml: leader {l:?} must be one key")),
        }
    }
    let timeout = file
        .timeout_ms
        .map_or(DEFAULT_TIMEOUT, Duration::from_millis);

    let mut convert = |entries: &[Entry]| -> Vec<Binding> {
        entries
            .iter()
            .filter_map(|e| {
                let keys = parse_keys(&e.on, leader)
                    .map_err(|err| problems.push(format!("keymap.toml: {:?}: {err}", e.on)))
                    .ok()?;
                let action = Action::parse(&e.run)
                    .map_err(|err| problems.push(format!("keymap.toml: {:?}: {err}", e.on)))
                    .ok()?;
                Some(Binding {
                    keys,
                    action,
                    desc: e.desc.clone().unwrap_or_else(|| e.run.clone()),
                })
            })
            .collect()
    };
    let mut normal = convert(&file.normal.prepend);
    match &file.normal.keymap {
        Some(all) => normal.extend(convert(all)),
        None => normal.extend(build_defaults(leader)),
    }
    normal.extend(convert(&file.normal.append));

    let map = Keymap {
        leader,
        timeout,
        normal,
    };
    problems.extend(shadow_warnings(&map));
    (map, problems)
}

/// A binding that is a prefix of another only runs after the timeout. Not an error, but
/// worth saying, since it makes the shorter one feel slow.
fn shadow_warnings(map: &Keymap) -> Vec<String> {
    let mut out = Vec::new();
    for a in &map.normal {
        let shadows = map
            .normal
            .iter()
            .find(|b| b.keys.len() > a.keys.len() && b.keys[..a.keys.len()] == a.keys[..]);
        if let Some(b) = shadows {
            out.push(format!(
                "keymap.toml: {} also starts {}, so it waits {} ms before running",
                fmt_keys(&a.keys, map.leader),
                fmt_keys(&b.keys, map.leader),
                map.timeout.as_millis()
            ));
        }
    }
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPACE: Key = Key::plain(KeyCode::Char(' '));

    fn k(s: &str) -> Vec<Key> {
        match parse_keys(s, SPACE) {
            Ok(v) => v,
            Err(e) => panic!("{s}: {e}"),
        }
    }

    fn ev(code: KeyCode, m: KeyModifiers) -> Key {
        Key::from_event(&KeyEvent::new(code, m))
    }

    #[test]
    fn notation_round_trip_table() {
        let cases: &[(&str, &str)] = &[
            ("gg", "gg"),
            ("<C-d>", "<C-d>"),
            ("<C-D>", "<C-d>"),
            ("<c-o>", "<C-o>"),
            ("<CR>", "<CR>"),
            ("<Enter>", "<CR>"),
            ("<S-CR>", "<S-CR>"),
            ("<S-Tab>", "<S-Tab>"),
            ("<Esc>", "<Esc>"),
            ("<BS>", "<BS>"),
            ("<leader>ff", "<Space>ff"),
            ("<Space>", "<Space>"),
            ("<lt>", "<lt>"),
            ("<A-CR>", "<A-CR>"),
            ("<S-a>", "A"),
            ("<F5>", "<F5>"),
            ("g\\", "g\\"),
            ("''", "''"),
            ("<", "<lt>"),
            ("a<b", "a<lt>b"),
        ];
        for (input, want) in cases {
            let got: String = k(input).iter().map(Key::notation).collect();
            assert_eq!(got, *want, "{input}");
        }
        for bad in ["", "<C-xyz>", "<Q-a>", "<nope>"] {
            assert!(parse_keys(bad, SPACE).is_err(), "{bad}");
        }
    }

    #[test]
    fn events_normalize_like_notation() {
        assert_eq!(
            vec![ev(KeyCode::Char('G'), KeyModifiers::SHIFT)],
            k("G"),
            "shift folds into the case"
        );
        assert_eq!(
            vec![ev(KeyCode::Char('D'), KeyModifiers::CONTROL)],
            k("<C-d>")
        );
        assert_eq!(
            vec![ev(KeyCode::BackTab, KeyModifiers::SHIFT)],
            k("<S-Tab>")
        );
        assert_eq!(vec![ev(KeyCode::Enter, KeyModifiers::SHIFT)], k("<S-CR>"));
        assert_eq!(
            vec![ev(KeyCode::Char(' '), KeyModifiers::NONE)],
            k("<leader>")
        );
    }

    #[test]
    fn fmt_shows_leader() {
        assert_eq!(fmt_keys(&k("<leader>l"), SPACE), "<leader>l");
        assert_eq!(fmt_keys(&k("<Space>"), SPACE), "<Space>");
    }

    #[test]
    fn action_parse_table() {
        let ok: &[(&str, Action)] = &[
            ("q", Action::Quit),
            ("q!", Action::QuitNoWrite),
            ("wq", Action::QuitWriteCwd),
            ("  down  ", Action::Down),
            ("cd", Action::Cd("~".into())),
            ("cd ~", Action::Cd("~".into())),
            (r"cd \\srv\share", Action::Cd(r"\\srv\share".into())),
            (
                r"e C:\Program Files",
                Action::Cd(r"C:\Program Files".into()),
            ),
            ("locations", Action::Locations),
            ("sync", Action::Sync(String::new(), false)),
            (
                "sync HIT → local",
                Action::Sync("HIT → local".into(), false),
            ),
            ("sync! HIT", Action::Sync("HIT".into(), true)),
            ("sync-picker", Action::SyncPicker),
            ("pin url", Action::PinUrl(String::new())),
            (
                "pin url https://x.example",
                Action::PinUrl("https://x.example".into()),
            ),
            ("import-wtm", Action::ImportWtm),
            ("backup HIT", Action::Backup("HIT".into())),
            ("12", Action::GotoLine(12)),
            ("set rnu", Action::SetRelativeNumber(true)),
            ("set nornu", Action::SetRelativeNumber(false)),
            ("se nu", Action::SetNumber(true)),
            ("set nonumber", Action::SetNumber(false)),
            ("pin", Action::PinCwd),
            ("tags", Action::TagPicker),
            (
                "tag rename aveva e3d",
                Action::TagRename("aveva".into(), "e3d".into()),
            ),
            ("tag delete old", Action::TagDelete("old".into())),
            (
                "view save main server",
                Action::ViewSave("main server".into()),
            ),
            (
                "view delete main server",
                Action::ViewDelete("main server".into()),
            ),
        ];
        for (s, want) in ok {
            assert_eq!(Action::parse(s).as_ref(), Ok(want), "{s}");
        }
        let err: &[(&str, &str)] = &[
            ("", "empty"),
            ("nope", "unknown command"),
            ("q now", "no argument"),
            ("picker nope", "picker files | dirs"),
            ("set", "set what"),
            ("set list", "unknown option"),
            ("tag rename x", "tag rename <old> <new>"),
            ("tag", "tag rename"),
            ("view save", "view save <name>"),
            ("view list", "view save <name>"),
        ];
        for (s, want) in err {
            let got = Action::parse(s).err().unwrap_or_default();
            assert!(got.contains(want), "{s}: {got}");
        }
    }

    #[test]
    fn command_hints_and_the_new_group() {
        let h = command_hints("pin u");
        assert_eq!(
            h,
            ["pin url [URL] — pin a web app (opens as its own window)"]
        );
        assert_eq!(command_hints("pin").len(), 2, "pin and pin url");
        assert_eq!(command_hints("pi").len(), 7, "and the 5 pickers");
        // A name typed after the command still shows its family.
        assert!(
            command_hints("sync HIT")
                .iter()
                .all(|s| s.starts_with("sync"))
        );
        assert!(command_hints("zzz").is_empty());
        // Every listed command parses (arguments filled in where needed).
        for cmd in COMMANDS {
            let (c, a) = (cmd.name, cmd.args);
            let line = match a {
                "" => c.to_string(),
                "<path>" => format!("{c} C:\\"),
                "<old> <new>" => format!("{c} a b"),
                "rnu nornu nu nonu" => format!("{c} rnu"),
                "[n]" => format!("{c} 2"),
                _ => format!("{c} x"),
            };
            assert!(Action::parse(&line).is_ok(), "{line}");
        }
        // Space n: the "new…" group in which-key.
        let km = Keymap::default();
        let cont = km.continuations(&[km.leader]);
        assert!(
            cont.iter()
                .any(|c| c.key.plain_char() == Some('n') && c.group && c.desc.contains("new")),
            "{cont:?}"
        );
        let n = km.continuations(&[km.leader, Key::plain(KeyCode::Char('n'))]);
        assert_eq!(n.len(), 4);
    }

    #[test]
    fn lookup_and_continuations() {
        let map = Keymap::default();
        assert!(matches!(
            map.lookup(&k("j")),
            Lookup::Exact {
                action: Action::Down,
                longer: false
            }
        ));
        assert_eq!(map.lookup(&k("g")), Lookup::Prefix);
        assert_eq!(map.lookup(&k("gq")), Lookup::None);
        assert_eq!(map.lookup(&k("X")), Lookup::None);
        assert!(matches!(
            map.lookup(&k("<leader>l")),
            Lookup::Exact {
                action: Action::Locations,
                ..
            }
        ));

        let g: Vec<String> = map
            .continuations(&k("g"))
            .iter()
            .map(|c| format!("{} {}", c.key.notation(), c.desc))
            .collect();
        assert!(g.contains(&"g top (or line n)".to_string()), "{g:?}");
        assert!(g.contains(&"l Locations".to_string()), "{g:?}");
        assert!(g.contains(&"? help".to_string()), "{g:?}");

        let top: Vec<Continuation> = map.continuations(&[]);
        let g_row = top.iter().find(|c| c.key == k("g")[0]);
        assert_eq!(
            g_row.map(|c| (c.group, c.desc.as_str())),
            Some((true, "goto"))
        );
        assert!(
            !top.iter().any(|c| c.key == Key::plain(KeyCode::Down)),
            "undescribed aliases stay out"
        );
    }

    #[test]
    fn defaults_have_no_shadowing() {
        assert_eq!(shadow_warnings(&Keymap::default()), Vec::<String>::new());
        assert!(build_defaults(SPACE).len() == defaults().len(), "all parse");
    }

    #[test]
    fn toml_overrides() {
        let (map, problems) = from_toml(
            r#"
            leader = ","
            timeout_ms = 500

            [normal]
            prepend = [
              { on = "gh", run = "cd ~", desc = "Home" },
              { on = "j", run = "up" },
              { on = "<C-p>", run = "picker files", desc = "Find files" },
              { on = "<Q-x>", run = "down" },
            ]
            append = [ { on = "<leader>x", run = "quit" } ]

            [picker.insert]
            append = [ { on = "<C-l>", run = "picker lift-depth" } ]
            "#,
        );
        assert_eq!(map.leader, Key::plain(KeyCode::Char(',')));
        assert_eq!(map.timeout, Duration::from_millis(500));
        assert!(matches!(
            map.lookup(&k("gh")),
            Lookup::Exact {
                action: Action::Cd(_),
                ..
            }
        ));
        assert!(
            matches!(
                map.lookup(&k("j")),
                Lookup::Exact {
                    action: Action::Up,
                    ..
                }
            ),
            "prepend wins over the default"
        );
        let comma = Key::plain(KeyCode::Char(','));
        assert!(matches!(
            map.lookup(&[comma, Key::plain(KeyCode::Char('x'))]),
            Lookup::Exact {
                action: Action::Quit,
                ..
            }
        ));
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].contains("unknown modifier"), "{problems:?}");
        // With `,` as leader, `,s` (cycle sort) is a prefix of `,sk` (keymaps).
        assert!(
            problems[1].contains("<leader>s also starts <leader>sk"),
            "{problems:?}"
        );
    }

    #[test]
    fn toml_full_override_and_errors() {
        let (map, problems) = from_toml(
            r#"
            [normal]
            keymap = [ { on = "j", run = "down" }, { on = "g", run = "top" }, { on = "gg", run = "top" } ]
            "#,
        );
        assert_eq!(map.normal.len(), 3);
        assert!(
            problems.iter().any(|p| p.contains("g also starts gg")),
            "{problems:?}"
        );
        assert!(matches!(
            map.lookup(&k("g")),
            Lookup::Exact { longer: true, .. }
        ));

        let (map, problems) = from_toml("leader = [");
        assert_eq!(problems.len(), 1);
        assert_eq!(
            map.normal.len(),
            Keymap::default().normal.len(),
            "defaults kept"
        );
    }

    #[test]
    fn char_bindings_take_any_plain_character() {
        let map = Keymap::default();
        let run = |keys: &str| match map.lookup(&k(keys)) {
            Lookup::Exact { action, .. } => {
                let last = k(keys).last().copied().unwrap_or(ANY_CHAR);
                Some(action.clone().with_char(&last))
            }
            _ => None,
        };
        assert_eq!(run("'a"), Some(Action::JumpMark('a')));
        assert_eq!(run("'7"), Some(Action::JumpMark('7')));
        assert_eq!(run("mZ"), Some(Action::SetMark('Z')));
        assert_eq!(run("''"), Some(Action::PreviousDir), "exact beats <char>");
        assert_eq!(run("m<C-a>"), None, "ctrl keys aren't characters");
        assert_eq!(run("m<Space>"), None);
        assert_eq!(map.lookup(&k("m")), Lookup::Prefix);
        assert_eq!(map.char_action(&k("'")), Some(&Action::JumpMark('\0')));
        assert!(
            map.continuations(&k("'")).iter().all(|c| c.key != ANY_CHAR),
            "the popup lists pins itself"
        );
        assert_eq!(
            parse_keys("m<char>", SPACE).map(|v| fmt_keys(&v, SPACE)),
            Ok("m<char>".into())
        );
    }
}
