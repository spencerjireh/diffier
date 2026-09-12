//! diffier: a live diff feed for Claude Code edits.
//!
//! `diffier hook`, registered in Claude Code's settings, snapshots files
//! before each edit and appends the hook payload to a JSONL spool. The TUI
//! tails the spool, diffs the snapshot against the file on disk, and renders
//! the result.

pub mod app;
pub mod diff;
pub mod event;
pub mod hook;
pub mod install;
pub mod paths;
pub mod render;
pub mod session;
pub mod snapshot;
pub mod spool;
pub mod ui;
