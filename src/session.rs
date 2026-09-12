//! Turn a stream of hook events for one directory into edit cards.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::diff::{self, DiffResult};
use crate::event::HookEvent;
use crate::snapshot;
use crate::spool::{CwdMatch, CwdMatcher, MatchMode};

/// Pending PreToolUse entries older than this are dropped.
pub const PENDING_TTL_MS: u64 = 10 * 60 * 1000;

#[derive(Debug, Clone)]
pub struct PendingPre {
    pub ts: u64,
    pub tool: String,
    pub file: Option<PathBuf>,
    pub agent: Option<String>,
    pub session_id: Option<String>,
}

/// Everything needed to render a card; rendering is a separate step.
#[derive(Debug, Clone)]
pub struct CardInput {
    pub path: String,
    pub file: PathBuf,
    pub tool: String,
    pub ts: u64,
    pub agent: Option<String>,
    pub diff: DiffResult,
    pub user_modified: bool,
    pub session_id: Option<String>,
    /// Basename of the edit's worktree, only when it is not the monitor's.
    pub worktree: Option<String>,
    /// Directory `path` is relative to and delta runs in: the edit's worktree
    /// root in repo mode, otherwise the monitor's cwd.
    pub root: PathBuf,
}

/// Distinct session ids in order of first card.
pub fn session_order(inputs: &[CardInput]) -> Vec<String> {
    let mut seen = Vec::new();
    for id in inputs.iter().filter_map(|i| i.session_id.as_deref()) {
        if !seen.iter().any(|s| s == id) {
            seen.push(id.to_string());
        }
    }
    seen
}

/// `--session` matching: a prefix of the id, or its tail so a header tag works.
pub fn session_matches(id: &str, needle: &str) -> bool {
    id.starts_with(needle) || id.ends_with(needle)
}

pub struct Pipeline {
    cwd: PathBuf,
    matcher: CwdMatcher,
    snapshot_root: PathBuf,
    pending: HashMap<String, PendingPre>,
    pub session_id: Option<String>,
}

impl Pipeline {
    pub fn new(cwd: PathBuf, snapshot_root: PathBuf, mode: MatchMode) -> Self {
        Self {
            matcher: CwdMatcher::new(cwd.clone(), mode),
            cwd,
            snapshot_root,
            pending: HashMap::new(),
            session_id: None,
        }
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn matcher_mut(&mut self) -> &mut CwdMatcher {
        &mut self.matcher
    }

    /// The monitor's worktree root when repo matching is active via git.
    pub fn toplevel(&self) -> Option<&Path> {
        self.matcher.toplevel()
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Whether the event belongs to this monitor's directory.
    pub fn accepts(&mut self, ev: &HookEvent) -> bool {
        self.matcher.accepts(ev)
    }

    /// Feed one event. Returns a card only for a PostToolUse of a file tool.
    pub fn handle(&mut self, ev: &HookEvent) -> Option<CardInput> {
        let matched = self.matcher.resolve_event(ev)?;
        if ev.is_session_start() {
            // Only this session's in-flight edits are stale; another session
            // in the same repo may have one between its Pre and Post.
            if ev.source.as_deref() != Some("compact") {
                self.pending.retain(|_, p| p.session_id != ev.session_id);
            }
            self.session_id = ev.session_id.clone();
            return None;
        }
        if !ev.is_file_tool() {
            return None;
        }
        if ev.is_pre() {
            if let Some(id) = ev.tool_use_id.clone() {
                self.pending.insert(
                    id,
                    PendingPre {
                        ts: ev.ts_ms(),
                        tool: ev.tool_name.clone().unwrap_or_default(),
                        file: ev.file_path(),
                        agent: ev.agent_label(),
                        session_id: ev.session_id.clone(),
                    },
                );
            }
            return None;
        }
        if !ev.is_post() {
            return None;
        }
        let pre = ev
            .tool_use_id
            .as_deref()
            .and_then(|id| self.pending.remove(id));
        let file = ev
            .file_path()
            .or_else(|| pre.as_ref().and_then(|p| p.file.clone()))
            .or_else(|| ev.response_str("filePath").map(PathBuf::from))?;
        let tool = ev
            .tool_name
            .clone()
            .or_else(|| pre.as_ref().map(|p| p.tool.clone()))
            .unwrap_or_default();
        let agent = ev
            .agent_label()
            .or_else(|| pre.as_ref().and_then(|p| p.agent.clone()));
        let (root, worktree) = match matched {
            CwdMatch::Repo { toplevel } => {
                let worktree = (Some(toplevel.as_path()) != self.matcher.toplevel())
                    .then(|| {
                        toplevel
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                    })
                    .flatten();
                (toplevel, worktree)
            }
            CwdMatch::Exact => (self.cwd.clone(), None),
        };
        let path = diff::display_path(&file, &root);
        let before = snapshot::resolve(&self.snapshot_root, ev);
        // Prefer the hook's post-edit snapshot: reading the file here would pick
        // up any edit that landed between the tool finishing and this event
        // being processed, which is every event during a replay.
        let after = snapshot::resolve_after(&self.snapshot_root, ev)
            .unwrap_or_else(|| fs::read(&file).ok());
        let diff = diff::compute(&before, after.as_deref(), &path);
        Some(CardInput {
            path,
            file,
            tool,
            ts: ev.ts_ms(),
            agent,
            diff,
            user_modified: ev.response_bool("userModified").unwrap_or(false),
            session_id: ev.session_id.clone(),
            worktree,
            root,
        })
    }

    /// Drop stale PreToolUse entries (tool denied or failed, so no Post).
    pub fn prune_pending(&mut self, now_ms: u64) {
        self.pending
            .retain(|_, p| p.ts == 0 || now_ms.saturating_sub(p.ts) < PENDING_TTL_MS);
    }

    /// Run a batch of events (e.g. from a replay scan) and collect cards.
    pub fn replay(&mut self, events: &[HookEvent]) -> Vec<CardInput> {
        events.iter().filter_map(|e| self.handle(e)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::DiffKind;

    fn ev(json: String) -> HookEvent {
        HookEvent::parse_line(&json).unwrap()
    }

    #[test]
    fn pre_then_post_builds_card_from_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        let snaps = dir.path().join("snaps");
        fs::create_dir_all(cwd.join("src")).unwrap();
        fs::create_dir_all(snaps.join("s1")).unwrap();
        fs::write(snaps.join("s1/t1"), "old\n").unwrap();
        fs::write(cwd.join("src/a.rs"), "new\n").unwrap();
        let c = cwd.to_string_lossy();
        let f = cwd.join("src/a.rs").to_string_lossy().to_string();
        let mut p = Pipeline::new(cwd.clone(), snaps, MatchMode::CwdOnly);
        let pre = ev(format!(
            r#"{{"hook_event_name":"PreToolUse","session_id":"s1","cwd":"{c}","tool_name":"Edit","tool_use_id":"t1","tool_input":{{"file_path":"{f}"}},"ts":1000}}"#
        ));
        assert!(p.handle(&pre).is_none());
        assert_eq!(p.pending_len(), 1);
        let post = ev(format!(
            r#"{{"hook_event_name":"PostToolUse","session_id":"s1","cwd":"{c}","tool_name":"Edit","tool_use_id":"t1","tool_input":{{"file_path":"{f}"}},"tool_response":{{"userModified":true}},"agent_type":"Explore","ts":2000}}"#
        ));
        let card = p.handle(&post).expect("card");
        assert_eq!(p.pending_len(), 0);
        assert_eq!(card.path, "src/a.rs");
        assert_eq!(card.tool, "Edit");
        assert_eq!(card.ts, 2000);
        assert_eq!(card.agent.as_deref(), Some("Explore"));
        assert!(card.user_modified);
        assert_eq!(card.session_id.as_deref(), Some("s1"));
        assert!(card.worktree.is_none());
        assert_eq!(card.root, cwd);
        assert_eq!(card.diff.kind, DiffKind::Modified);
        assert!(card.diff.unified.contains("-old\n+new\n"));
    }

    #[test]
    fn after_snapshot_wins_over_the_file_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        let snaps = dir.path().join("snaps");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(snaps.join("s1")).unwrap();
        fs::write(snaps.join("s1/t1"), "one\n").unwrap();
        // What the file looked like right after this edit...
        fs::write(snaps.join("s1/t1.after"), "two\n").unwrap();
        // ...and what a later edit has since made of it.
        fs::write(cwd.join("a.rs"), "three\n").unwrap();
        let c = cwd.to_string_lossy();
        let f = cwd.join("a.rs").to_string_lossy().to_string();
        let mut p = Pipeline::new(cwd.clone(), snaps, MatchMode::CwdOnly);
        let post = ev(format!(
            r#"{{"hook_event_name":"PostToolUse","session_id":"s1","cwd":"{c}","tool_name":"Edit","tool_use_id":"t1","tool_input":{{"file_path":"{f}"}}}}"#
        ));
        let card = p.handle(&post).unwrap();
        assert!(
            card.diff.unified.contains("-one\n+two\n"),
            "{}",
            card.diff.unified
        );
        assert!(!card.diff.unified.contains("three"));
    }

    #[test]
    fn after_absent_marker_reports_deletion_despite_a_file_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        let snaps = dir.path().join("snaps");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(snaps.join("s1")).unwrap();
        fs::write(snaps.join("s1/t1"), "gone\n").unwrap();
        fs::write(snaps.join("s1/t1.after.absent"), "").unwrap();
        fs::write(cwd.join("a.rs"), "recreated\n").unwrap();
        let c = cwd.to_string_lossy();
        let f = cwd.join("a.rs").to_string_lossy().to_string();
        let mut p = Pipeline::new(cwd.clone(), snaps, MatchMode::CwdOnly);
        let post = ev(format!(
            r#"{{"hook_event_name":"PostToolUse","session_id":"s1","cwd":"{c}","tool_name":"Edit","tool_use_id":"t1","tool_input":{{"file_path":"{f}"}}}}"#
        ));
        assert_eq!(p.handle(&post).unwrap().diff.kind, DiffKind::Deleted);
    }

    #[test]
    fn other_cwd_and_non_file_tools_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_path_buf();
        let mut p = Pipeline::new(cwd.clone(), dir.path().join("snaps"), MatchMode::CwdOnly);
        let other = ev(r#"{"hook_event_name":"PostToolUse","cwd":"/elsewhere","tool_name":"Edit","tool_use_id":"x","tool_input":{"file_path":"/elsewhere/f"}}"#.to_string());
        assert!(p.handle(&other).is_none());
        let c = cwd.to_string_lossy();
        let bash = ev(format!(
            r#"{{"hook_event_name":"PostToolUse","cwd":"{c}","tool_name":"Bash","tool_use_id":"b","tool_input":{{"command":"ls"}}}}"#
        ));
        assert!(p.handle(&bash).is_none());
    }

    #[test]
    fn session_start_clears_pending_except_compact() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_path_buf();
        let c = cwd.to_string_lossy().to_string();
        let mut p = Pipeline::new(cwd.clone(), dir.path().join("snaps"), MatchMode::CwdOnly);
        let pre = ev(format!(
            r#"{{"hook_event_name":"PreToolUse","cwd":"{c}","tool_name":"Edit","tool_use_id":"t","tool_input":{{"file_path":"/x"}}}}"#
        ));
        p.handle(&pre);
        p.handle(&ev(format!(r#"{{"hook_event_name":"SessionStart","cwd":"{c}","source":"compact","session_id":"s9"}}"#)));
        assert_eq!(p.pending_len(), 1);
        assert_eq!(p.session_id.as_deref(), Some("s9"));
        p.handle(&ev(format!(
            r#"{{"hook_event_name":"SessionStart","cwd":"{c}","source":"startup"}}"#
        )));
        assert_eq!(p.pending_len(), 0);
    }

    #[test]
    fn prune_drops_old_pending() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_path_buf();
        let c = cwd.to_string_lossy().to_string();
        let mut p = Pipeline::new(cwd, dir.path().join("snaps"), MatchMode::CwdOnly);
        p.handle(&ev(format!(r#"{{"hook_event_name":"PreToolUse","cwd":"{c}","tool_name":"Edit","tool_use_id":"t","tool_input":{{"file_path":"/x"}},"ts":1000}}"#)));
        p.prune_pending(1000 + PENDING_TTL_MS - 1);
        assert_eq!(p.pending_len(), 1);
        p.prune_pending(1000 + PENDING_TTL_MS);
        assert_eq!(p.pending_len(), 0);
    }

    #[test]
    fn post_without_pre_still_renders() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_path_buf();
        let c = cwd.to_string_lossy().to_string();
        let f = cwd.join("n.txt");
        fs::write(&f, "hello\n").unwrap();
        let fs_ = f.to_string_lossy().to_string();
        let mut p = Pipeline::new(cwd, dir.path().join("snaps"), MatchMode::CwdOnly);
        let post = ev(format!(
            r#"{{"hook_event_name":"PostToolUse","session_id":"s","cwd":"{c}","tool_name":"Write","tool_use_id":"w","tool_input":{{"file_path":"{fs_}","content":"hello\n"}},"tool_response":{{"type":"create","originalFile":""}}}}"#
        ));
        let card = p.handle(&post).unwrap();
        assert_eq!(card.path, "n.txt");
        assert_eq!(card.diff.kind, DiffKind::Modified);
        assert!(card.diff.unified.contains("+hello\n"));
    }

    #[test]
    fn session_start_only_clears_its_own_pending() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_path_buf();
        let c = cwd.to_string_lossy().to_string();
        let mut p = Pipeline::new(cwd, dir.path().join("snaps"), MatchMode::CwdOnly);
        for sid in ["s1", "s2"] {
            p.handle(&ev(format!(
                r#"{{"hook_event_name":"PreToolUse","session_id":"{sid}","cwd":"{c}","tool_name":"Edit","tool_use_id":"t-{sid}","tool_input":{{"file_path":"/x"}}}}"#
            )));
        }
        assert_eq!(p.pending_len(), 2);
        p.handle(&ev(format!(
            r#"{{"hook_event_name":"SessionStart","session_id":"s2","cwd":"{c}","source":"startup"}}"#
        )));
        assert_eq!(p.pending_len(), 1);
        p.handle(&ev(format!(
            r#"{{"hook_event_name":"SessionStart","session_id":"s1","cwd":"{c}","source":"startup"}}"#
        )));
        assert_eq!(p.pending_len(), 0);
    }

    fn card(session: Option<&str>) -> CardInput {
        CardInput {
            path: "f".into(),
            file: PathBuf::from("/p/f"),
            tool: "Edit".into(),
            ts: 0,
            agent: None,
            diff: diff::compute(&crate::snapshot::Before::Absent, None, "f"),
            user_modified: false,
            session_id: session.map(str::to_string),
            worktree: None,
            root: PathBuf::from("/p"),
        }
    }

    #[test]
    fn session_order_is_first_appearance() {
        let inputs = [
            card(Some("b")),
            card(None),
            card(Some("a")),
            card(Some("b")),
        ];
        assert_eq!(session_order(&inputs), ["b", "a"]);
    }

    #[test]
    fn session_matches_prefix_or_suffix() {
        assert!(session_matches("abc-123456", "abc"));
        assert!(session_matches("abc-123456", "123456"));
        assert!(!session_matches("abc-123456", "c-1"));
    }
}
