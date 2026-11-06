//! Bulk rename, PowerRename-style (CLAUDE.md section 16).
//!
//! - [`tokens`]: the Replace field's `${…}` / `$…` tokens (PowerRename's set + extras).
//! - [`engine`]: search / replace for one name with every option.
//! - [`plan`]: the preview for the whole batch, with the safety checks.
//!
//! All pure; the panel (`app::rename`) runs the plan and applies it through the worker.

pub mod engine;
pub mod plan;
pub mod tokens;
