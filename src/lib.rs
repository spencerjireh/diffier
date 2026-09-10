//! diffier: a live diff feed for Claude Code edits.
//!
//! Hooks installed in Claude Code snapshot files before each edit and append
//! the hook payload to a JSONL spool. This crate tails the spool, diffs the
//! snapshot against the file on disk, and renders the result.

pub mod app;
pub mod diff;
pub mod event;
pub mod install;
pub mod paths;
pub mod render;
pub mod session;
pub mod snapshot;
pub mod spool;
pub mod ui;
