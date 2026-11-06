//! Bulk rename, PowerRename-style (CLAUDE.md section 16).
//!
//! - [`tokens`]: the Replace field's `${…}` / `$…` tokens (PowerRename's set + extras).
//! - [`engine`]: search / replace for one name with every option.
//! - [`plan`]: the preview for the whole batch, with the safety checks.
//! - [`apply`]: the two-phase apply (deepest first), the journal and undo. Blocking.
//! - [`presets`]: saved presets (`rename.toml`), the last options and the field history.
//! - [`meta`]: photo (EXIF) and PDF-title values read from the files. Blocking.
//!
//! The panel (`app::rename`) runs the plan and applies it through the worker.

pub mod apply;
pub mod engine;
pub mod meta;
pub mod plan;
pub mod presets;
pub mod tokens;
