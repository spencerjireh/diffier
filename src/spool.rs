//! Reading the JSONL spool: full replay scans and incremental tailing.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::event::HookEvent;
use crate::paths::rotated;

/// Events shown when no SessionStart boundary exists for the cwd.
pub const NO_BOUNDARY_TAIL: usize = 200;

/// Compare an event's `cwd` against the monitor's directory, tolerating
/// symlinks and trailing slashes. Falls back to string comparison when a
/// path cannot be canonicalized.
pub fn same_cwd(event_cwd: &str, cwd: &Path) -> bool {
    let ev = Path::new(event_cwd);
    match (fs::canonicalize(ev), fs::canonicalize(cwd)) {
        (Ok(a), Ok(b)) => a == b,
        _ => {
            let norm = |p: &Path| p.to_string_lossy().trim_end_matches('/').to_string();
            norm(ev) == norm(cwd)
        }
    }
}

/// Which event directories a monitor follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MatchMode {
    /// Any worktree or subdirectory of the monitor's git repository.
    #[default]
    Repo,
    /// The monitor's exact directory only.
    CwdOnly,
}

/// What git reports for a directory, both canonicalized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CwdInfo {
    pub common_dir: PathBuf,
    pub toplevel: PathBuf,
}

/// Why an event `cwd` was accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CwdMatch {
    /// The monitor's own directory (exact mode, or repo mode without git).
    Exact,
    /// Same repository via git; `toplevel` is the event's worktree root.
    Repo { toplevel: PathBuf },
}

/// One `git rev-parse --git-common-dir --show-toplevel` run in `dir`. None
/// when git is not on PATH, `dir` is outside a repository, or the output is
/// unusable.
pub fn git_info(dir: &Path) -> Option<CwdInfo> {
    let out = Command::new("git")
        .args(["rev-parse", "--git-common-dir", "--show-toplevel"])
        .current_dir(dir)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let mut lines = text.lines();
    // The common dir is printed relative to `dir` (`.git`, `../.git`, ...).
    let common_dir = fs::canonicalize(dir.join(lines.next()?)).ok()?;
    let toplevel = fs::canonicalize(lines.next()?).ok()?;
    Some(CwdInfo {
        common_dir,
        toplevel,
    })
}

/// `same_cwd` with the target canonicalized once and each distinct event `cwd`
/// answered from a cache: a replay scan sees the same handful of strings tens
/// of thousands of times. In repo mode a cwd that is not the monitor's is
/// asked of git once and accepted when it shares the repository.
pub struct CwdMatcher {
    cwd: PathBuf,
    canonical: Option<PathBuf>,
    /// The monitor's repository; Some only when repo mode is active via git.
    repo: Option<CwdInfo>,
    cache: HashMap<String, Option<CwdMatch>>,
}

impl CwdMatcher {
    pub fn new(cwd: PathBuf, mode: MatchMode) -> Self {
        let canonical = fs::canonicalize(&cwd).ok();
        let repo = match mode {
            MatchMode::Repo => git_info(&cwd),
            MatchMode::CwdOnly => None,
        };
        Self {
            cwd,
            canonical,
            repo,
            cache: HashMap::new(),
        }
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// The monitor's worktree root when repo mode is active via git.
    pub fn toplevel(&self) -> Option<&Path> {
        self.repo.as_ref().map(|r| r.toplevel.as_path())
    }

    pub fn repo_mode(&self) -> bool {
        self.repo.is_some()
    }

    /// Cached classification of one event `cwd`.
    pub fn resolve(&mut self, event_cwd: &str) -> Option<CwdMatch> {
        if let Some(hit) = self.cache.get(event_cwd) {
            return hit.clone();
        }
        let same = match (fs::canonicalize(Path::new(event_cwd)), &self.canonical) {
            (Ok(a), Some(b)) => a == *b,
            _ => {
                let norm = |p: &Path| p.to_string_lossy().trim_end_matches('/').to_string();
                norm(Path::new(event_cwd)) == norm(&self.cwd)
            }
        };
        let hit = match &self.repo {
            None => same.then_some(CwdMatch::Exact),
            // The monitor's own directory needs no git call.
            Some(repo) if same => Some(CwdMatch::Repo {
                toplevel: repo.toplevel.clone(),
            }),
            Some(repo) => git_info(Path::new(event_cwd))
                .filter(|info| info.common_dir == repo.common_dir)
                .map(|info| CwdMatch::Repo {
                    toplevel: info.toplevel,
                }),
        };
        self.cache.insert(event_cwd.to_string(), hit.clone());
        hit
    }

    pub fn matches(&mut self, event_cwd: &str) -> bool {
        self.resolve(event_cwd).is_some()
    }

    pub fn resolve_event(&mut self, ev: &HookEvent) -> Option<CwdMatch> {
        self.resolve(ev.cwd.as_deref()?)
    }

    pub fn accepts(&mut self, ev: &HookEvent) -> bool {
        self.resolve_event(ev).is_some()
    }
}

/// Split a byte buffer into complete lines, returning the parsed events and
/// the number of bytes consumed (everything up to and including the last
/// newline). Malformed lines are skipped.
pub fn parse_complete_lines(buf: &[u8]) -> (Vec<HookEvent>, usize) {
    let mut events = Vec::new();
    let mut consumed = 0;
    for chunk in buf.split(|b| *b == b'\n') {
        let end = consumed + chunk.len();
        if end >= buf.len() {
            // Last chunk has no trailing newline: partial line, keep it.
            break;
        }
        if let Ok(s) = std::str::from_utf8(chunk)
            && let Some(ev) = HookEvent::parse_line(s)
        {
            events.push(ev);
        }
        consumed = end + 1;
    }
    (events, consumed)
}

pub struct Replay {
    pub events: Vec<HookEvent>,
    /// Byte offset just past the last complete line of the current spool.
    pub offset: u64,
}

/// Read the spool (and the rotated `<spool>.1` that precedes it) and return
/// the events `matcher` accepts belonging to the sessions still in play.
pub fn scan_replay(path: &Path, matcher: &mut CwdMatcher) -> Replay {
    let mut all = Vec::new();
    // The hook rotates at 50 MB; without this the boundary and the history
    // before it are lost the first time the spool fills up.
    let previous = rotated(path);
    if previous != path && previous.exists() {
        let bytes = fs::read(&previous).unwrap_or_default();
        all.extend(parse_complete_lines(&bytes).0);
    }
    let bytes = fs::read(path).unwrap_or_default();
    let (current, consumed) = parse_complete_lines(&bytes);
    all.extend(current);
    let events = select_session(all, matcher);
    Replay {
        events,
        offset: consumed as u64,
    }
}

/// Key grouping events into sessions. Events without a `session_id` share one
/// bucket rather than being dropped.
fn session_key(ev: &HookEvent) -> &str {
    ev.session_id.as_deref().unwrap_or("")
}

/// Pure selection step shared by the replay scan and tests.
///
/// The primary session is the one owning the last non-compact SessionStart
/// `matcher` accepts. Any other session with activity after that boundary is
/// running concurrently in the same directory or repository, so its own
/// current segment is kept too; otherwise starting a second Claude Code
/// session here would erase the first session's feed. Original spool order is
/// preserved.
pub fn select_session(all: Vec<HookEvent>, matcher: &mut CwdMatcher) -> Vec<HookEvent> {
    let mut matching: Vec<HookEvent> = all.into_iter().filter(|e| matcher.accepts(e)).collect();

    let is_boundary =
        |e: &HookEvent| e.is_session_start() && e.source.as_deref() != Some("compact");
    let Some(primary_at) = matching.iter().rposition(is_boundary) else {
        let start = matching.len().saturating_sub(NO_BOUNDARY_TAIL);
        return matching.split_off(start);
    };

    // Each session's segment starts at its own most recent non-compact
    // SessionStart, or at its first event when it never emitted one.
    let mut segment_start: HashMap<&str, usize> = HashMap::new();
    for (i, ev) in matching.iter().enumerate() {
        let key = session_key(ev);
        if is_boundary(ev) || !segment_start.contains_key(key) {
            segment_start.insert(key, i);
        }
    }

    // Sessions still active after the primary boundary are shown alongside it;
    // older, finished sessions stay out of the feed.
    let primary = session_key(&matching[primary_at]);
    let mut active: HashSet<&str> = matching[primary_at + 1..].iter().map(session_key).collect();
    active.insert(primary);

    let keep: Vec<bool> = matching
        .iter()
        .enumerate()
        .map(|(i, ev)| {
            let key = session_key(ev);
            active.contains(key) && segment_start.get(key).is_some_and(|start| i >= *start)
        })
        .collect();

    let mut iter = keep.into_iter();
    matching.retain(|_| iter.next().unwrap_or(false));
    matching
}

/// Incremental reader that survives rotation (inode change or shrink).
pub struct SpoolTailer {
    path: PathBuf,
    offset: u64,
    ino: u64,
    buf: Vec<u8>,
}

impl SpoolTailer {
    pub fn new(path: PathBuf, offset: u64) -> Self {
        let ino = fs::metadata(&path).map(|m| m.ino()).unwrap_or(0);
        Self {
            path,
            offset,
            ino,
            buf: Vec::new(),
        }
    }

    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Read any new complete lines. A missing file yields nothing.
    pub fn poll(&mut self) -> Vec<HookEvent> {
        let meta = match fs::metadata(&self.path) {
            Ok(m) => m,
            Err(_) => return Vec::new(),
        };
        if meta.ino() != self.ino || meta.len() < self.offset {
            self.offset = 0;
            self.buf.clear();
            self.ino = meta.ino();
        }
        if meta.len() == self.offset {
            return Vec::new();
        }
        let mut file = match File::open(&self.path) {
            Ok(f) => f,
            Err(_) => return Vec::new(),
        };
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return Vec::new();
        }
        let mut chunk = Vec::new();
        if file.read_to_end(&mut chunk).is_err() {
            return Vec::new();
        }
        self.offset += chunk.len() as u64;
        self.buf.extend_from_slice(&chunk);
        let (events, consumed) = parse_complete_lines(&self.buf);
        self.buf.drain(..consumed);
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn line(event: &str, cwd: &str, extra: &str) -> String {
        format!(r#"{{"hook_event_name":"{event}","cwd":"{cwd}"{extra}}}"#)
    }

    fn parse(lines: &[String]) -> Vec<HookEvent> {
        lines
            .iter()
            .map(|l| HookEvent::parse_line(l).unwrap())
            .collect()
    }

    fn matcher(cwd: &Path) -> CwdMatcher {
        CwdMatcher::new(cwd.to_path_buf(), MatchMode::CwdOnly)
    }

    fn ids(events: &[HookEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| e.tool_use_id.clone())
            .collect()
    }

    #[test]
    fn partial_line_is_buffered_until_complete() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("events.jsonl");
        fs::write(&p, "").unwrap();
        let mut t = SpoolTailer::new(p.clone(), 0);
        let mut f = fs::OpenOptions::new().append(true).open(&p).unwrap();
        write!(f, r#"{{"hook_event_name":"Pre"#).unwrap();
        assert!(t.poll().is_empty());
        writeln!(f, r#"ToolUse","cwd":"/x"}}"#).unwrap();
        let got = t.poll();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].hook_event_name, "PreToolUse");
        assert!(t.poll().is_empty());
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let buf = format!(
            "{}\nnot json\n{}\n",
            line("A", "/x", ""),
            line("B", "/x", "")
        );
        let (events, consumed) = parse_complete_lines(buf.as_bytes());
        assert_eq!(
            events
                .iter()
                .map(|e| e.hook_event_name.as_str())
                .collect::<Vec<_>>(),
            ["A", "B"]
        );
        assert_eq!(consumed, buf.len());
    }

    #[test]
    fn shrink_resets_offset() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("events.jsonl");
        fs::write(
            &p,
            format!("{}\n{}\n", line("A", "/x", ""), line("B", "/x", "")),
        )
        .unwrap();
        let mut t = SpoolTailer::new(p.clone(), 0);
        assert_eq!(t.poll().len(), 2);
        // Rotate: replace with a shorter file (new inode too).
        fs::remove_file(&p).unwrap();
        fs::write(&p, format!("{}\n", line("C", "/x", ""))).unwrap();
        let got = t.poll();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].hook_event_name, "C");
    }

    #[test]
    fn boundary_is_last_non_compact_session_start_for_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let c = cwd.to_string_lossy().to_string();
        let all = parse(&[
            line(
                "SessionStart",
                &c,
                r#","session_id":"s1","source":"startup""#,
            ),
            line(
                "PostToolUse",
                &c,
                r#","session_id":"s1","tool_use_id":"old""#,
            ),
            line(
                "SessionStart",
                "/elsewhere",
                r#","session_id":"s2","source":"startup""#,
            ),
            line(
                "SessionStart",
                &c,
                r#","session_id":"s1","source":"startup""#,
            ),
            line("PostToolUse", &c, r#","session_id":"s1","tool_use_id":"a""#),
            line(
                "SessionStart",
                &c,
                r#","session_id":"s1","source":"compact""#,
            ),
            line(
                "PostToolUse",
                "/elsewhere",
                r#","session_id":"s2","tool_use_id":"z""#,
            ),
            line("PostToolUse", &c, r#","session_id":"s1","tool_use_id":"b""#),
        ]);
        let sel = select_session(all, &mut matcher(cwd));
        assert_eq!(ids(&sel), ["a", "b"]);
        assert!(sel[0].is_session_start());
        assert_eq!(sel.len(), 4);
    }

    #[test]
    fn concurrent_session_in_same_cwd_keeps_its_history() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let c = cwd.to_string_lossy().to_string();
        // Session a starts and edits, then session b starts in the same dir.
        // a keeps working, so its earlier cards must survive the replay.
        let all = parse(&[
            line(
                "SessionStart",
                &c,
                r#","session_id":"a","source":"startup""#,
            ),
            line("PostToolUse", &c, r#","session_id":"a","tool_use_id":"a1""#),
            line(
                "SessionStart",
                &c,
                r#","session_id":"b","source":"startup""#,
            ),
            line("PostToolUse", &c, r#","session_id":"b","tool_use_id":"b1""#),
            line("PostToolUse", &c, r#","session_id":"a","tool_use_id":"a2""#),
        ]);
        assert_eq!(
            ids(&select_session(all, &mut matcher(cwd))),
            ["a1", "b1", "a2"]
        );

        // Session a is idle after b starts: it is finished, so it drops out.
        let quiet = parse(&[
            line(
                "SessionStart",
                &c,
                r#","session_id":"a","source":"startup""#,
            ),
            line("PostToolUse", &c, r#","session_id":"a","tool_use_id":"a1""#),
            line(
                "SessionStart",
                &c,
                r#","session_id":"b","source":"startup""#,
            ),
            line("PostToolUse", &c, r#","session_id":"b","tool_use_id":"b1""#),
        ]);
        assert_eq!(ids(&select_session(quiet, &mut matcher(cwd))), ["b1"]);
    }

    #[test]
    fn restarted_session_drops_its_own_earlier_segment() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let c = cwd.to_string_lossy().to_string();
        let all = parse(&[
            line(
                "SessionStart",
                &c,
                r#","session_id":"a","source":"startup""#,
            ),
            line(
                "PostToolUse",
                &c,
                r#","session_id":"a","tool_use_id":"old""#,
            ),
            line(
                "SessionStart",
                &c,
                r#","session_id":"a","source":"startup""#,
            ),
            line(
                "PostToolUse",
                &c,
                r#","session_id":"a","tool_use_id":"new""#,
            ),
        ]);
        assert_eq!(ids(&select_session(all, &mut matcher(cwd))), ["new"]);
    }

    #[test]
    fn no_boundary_falls_back_to_tail() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let c = cwd.to_string_lossy().to_string();
        let all: Vec<HookEvent> = (0..NO_BOUNDARY_TAIL + 5)
            .map(|i| {
                HookEvent::parse_line(&line(
                    "PostToolUse",
                    &c,
                    &format!(r#","tool_use_id":"{i}""#),
                ))
                .unwrap()
            })
            .collect();
        let sel = select_session(all, &mut matcher(cwd));
        assert_eq!(sel.len(), NO_BOUNDARY_TAIL);
        assert_eq!(sel[0].tool_use_id.as_deref(), Some("5"));
    }

    #[test]
    fn scan_replay_reports_offset_of_last_complete_line() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let c = cwd.to_string_lossy().to_string();
        let p = dir.path().join("events.jsonl");
        let full = format!("{}\n", line("SessionStart", &c, r#","source":"startup""#));
        fs::write(&p, format!("{full}{{\"partial")).unwrap();
        let r = scan_replay(&p, &mut matcher(cwd));
        assert_eq!(r.events.len(), 1);
        assert_eq!(r.offset, full.len() as u64);
    }

    #[test]
    fn scan_replay_includes_the_rotated_file() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let c = cwd.to_string_lossy().to_string();
        let p = dir.path().join("events.jsonl");
        // The session started before the spool rotated; its history is in .1.
        let old = format!(
            "{}\n{}\n",
            line(
                "SessionStart",
                &c,
                r#","session_id":"s","source":"startup""#
            ),
            line(
                "PostToolUse",
                &c,
                r#","session_id":"s","tool_use_id":"before""#
            )
        );
        fs::write(rotated(&p), &old).unwrap();
        let new = format!(
            "{}\n{}\n",
            line(
                "SessionStart",
                &c,
                r#","session_id":"s","source":"compact""#
            ),
            line(
                "PostToolUse",
                &c,
                r#","session_id":"s","tool_use_id":"after""#
            )
        );
        fs::write(&p, &new).unwrap();

        let r = scan_replay(&p, &mut matcher(cwd));
        assert_eq!(ids(&r.events), ["before", "after"]);
        // The tail offset refers to the live spool only.
        assert_eq!(r.offset, new.len() as u64);
    }

    #[test]
    fn cwd_matcher_caches_and_agrees_with_same_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_path_buf();
        let c = cwd.to_string_lossy().to_string();
        let mut m = matcher(&cwd);
        assert!(m.matches(&c));
        assert!(m.matches(&format!("{c}/")));
        assert!(!m.matches("/somewhere/else"));
        assert_eq!(m.matches(&c), same_cwd(&c, &cwd));
        assert_eq!(
            m.matches("/somewhere/else"),
            same_cwd("/somewhere/else", &cwd)
        );
    }

    #[test]
    fn repo_mode_falls_back_outside_a_repo() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_path_buf();
        let c = cwd.to_string_lossy().to_string();
        let mut m = CwdMatcher::new(cwd.clone(), MatchMode::Repo);
        assert!(!m.repo_mode());
        assert!(m.toplevel().is_none());
        assert_eq!(m.resolve(&c), Some(CwdMatch::Exact));
        assert_eq!(m.resolve("/somewhere/else"), None);
    }

    #[test]
    fn cwd_only_never_consults_git() {
        let m = CwdMatcher::new(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")),
            MatchMode::CwdOnly,
        );
        assert!(!m.repo_mode());
    }
}
