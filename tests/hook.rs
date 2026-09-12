//! Behavioral tests for `diffier hook` as Claude Code runs it: it must never
//! exit non-zero, never treat a non-string payload field as a path, and never
//! corrupt the spool.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::Value;

fn hook_command() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_diffier"));
    c.arg("hook");
    c
}

/// A tempdir standing in for HOME/XDG, with the paths the hook derives.
struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn state(&self) -> PathBuf {
        self.dir.path().join("state")
    }

    fn cache(&self) -> PathBuf {
        self.dir.path().join("cache")
    }

    fn spool(&self) -> PathBuf {
        self.state().join("diffier/events.jsonl")
    }

    fn snapshots(&self) -> PathBuf {
        self.cache().join("diffier")
    }

    fn command(&self) -> Command {
        let mut c = hook_command();
        c.env_remove("HOME")
            .env("XDG_STATE_HOME", self.state())
            .env("XDG_CACHE_HOME", self.cache())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        c
    }

    /// Feed one payload to the hook; returns its exit code.
    fn run(&self, payload: &str) -> i32 {
        let mut child = self.command().stdin(Stdio::piped()).spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.as_bytes())
            .unwrap();
        child.wait().unwrap().code().unwrap_or(-1)
    }

    fn spool_lines(&self) -> Vec<String> {
        fs::read_to_string(self.spool())
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

#[test]
fn array_valued_file_path_is_not_a_path() {
    let env = Env::new();
    // The shell hook this replaced could have expanded an array into extra
    // words for `eval`; here it must simply count as no path at all.
    let payload = r#"{"hook_event_name":"PreToolUse","session_id":"s","tool_use_id":"t","tool_name":"Edit","cwd":"/p","tool_input":{"file_path":["x","touch","/tmp/pwned"]}}"#;
    assert_eq!(env.run(payload), 0);
    assert!(!env.snapshots().join("s").exists());
    // The event is still spooled, and exactly once.
    assert_eq!(env.spool_lines().len(), 1);
}

#[test]
fn array_valued_tool_name_is_not_a_file_tool() {
    let env = Env::new();
    let file = env.dir.path().join("c.txt");
    fs::write(&file, "x\n").unwrap();
    let payload = format!(
        r#"{{"hook_event_name":"PreToolUse","session_id":"s","tool_use_id":"t","tool_name":["Edit"],"cwd":"/p","tool_input":{{"file_path":"{}"}}}}"#,
        file.display()
    );
    assert_eq!(env.run(&payload), 0);
    assert!(!env.snapshots().join("s/t").exists());
    assert_eq!(env.spool_lines().len(), 1);
}

#[test]
fn unset_home_and_xdg_exits_zero_without_writing() {
    let env = Env::new();
    let mut child = hook_command()
        .env_remove("HOME")
        .env_remove("XDG_STATE_HOME")
        .env_remove("XDG_CACHE_HOME")
        .current_dir(env.dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"hook_event_name":"PreToolUse","tool_name":"Edit","tool_input":{"file_path":"/tmp/x"}}"#)
        .unwrap();
    // Exit 2 from a PreToolUse hook blocks Claude's tool call.
    assert_eq!(child.wait().unwrap().code(), Some(0));
    // Nothing relative to the cwd either.
    assert!(!env.dir.path().join("diffier").exists());
    assert!(!env.dir.path().join(".local").exists());
}

#[test]
fn hook_writes_nothing_to_stdout() {
    let env = Env::new();
    let out =
        env.command()
            .stdout(Stdio::piped())
            .stdin(Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                child.stdin.take().unwrap().write_all(
                    br#"{"hook_event_name":"SessionStart","session_id":"s","cwd":"/p"}"#,
                )?;
                child.wait_with_output()
            })
            .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty(), "stdout: {:?}", out.stdout);
    assert_eq!(env.spool_lines().len(), 1);
}

#[test]
fn pre_and_post_snapshot_the_file_on_both_sides() {
    let env = Env::new();
    let file = env.dir.path().join("a.txt");
    fs::write(&file, "before\n").unwrap();
    let f = file.display();
    assert_eq!(
        env.run(&format!(
            r#"{{"hook_event_name":"PreToolUse","session_id":"s","tool_use_id":"t","tool_name":"Edit","cwd":"/p","tool_input":{{"file_path":"{f}"}}}}"#
        )),
        0
    );
    fs::write(&file, "after\n").unwrap();
    assert_eq!(
        env.run(&format!(
            r#"{{"hook_event_name":"PostToolUse","session_id":"s","tool_use_id":"t","tool_name":"Edit","cwd":"/p","tool_input":{{"file_path":"{f}","content":"after\n"}},"tool_response":{{"originalFile":"before\n","content":"junk"}}}}"#
        )),
        0
    );
    let snaps = env.snapshots().join("s");
    assert_eq!(fs::read_to_string(snaps.join("t")).unwrap(), "before\n");
    assert_eq!(
        fs::read_to_string(snaps.join("t.after")).unwrap(),
        "after\n"
    );

    // The large fields are dropped, and originalFile too since a snapshot exists.
    let post: Value = serde_json::from_str(&env.spool_lines()[1]).unwrap();
    assert!(post["tool_response"].get("content").is_none());
    assert!(post["tool_response"].get("originalFile").is_none());
    assert!(post["tool_input"].get("content").is_none());
    assert_eq!(
        post["tool_input"]["file_path"].as_str().unwrap(),
        file.to_str().unwrap()
    );
    assert!(post["ts"].as_u64().unwrap() > 0);
}

#[test]
fn original_file_is_kept_when_no_snapshot_exists() {
    let env = Env::new();
    // PostToolUse with no preceding PreToolUse: originalFile is the only
    // pre-edit content the monitor can fall back to.
    assert_eq!(
        env.run(
            r#"{"hook_event_name":"PostToolUse","session_id":"s","tool_use_id":"t","tool_name":"Write","cwd":"/p","tool_input":{"file_path":"/tmp/gone"},"tool_response":{"originalFile":"kept"}}"#
        ),
        0
    );
    let post: Value = serde_json::from_str(&env.spool_lines()[0]).unwrap();
    assert_eq!(post["tool_response"]["originalFile"], "kept");
}

#[test]
fn post_for_a_removed_file_writes_an_absent_marker() {
    let env = Env::new();
    assert_eq!(
        env.run(
            r#"{"hook_event_name":"PostToolUse","session_id":"s","tool_use_id":"t","tool_name":"Edit","cwd":"/p","tool_input":{"file_path":"/nonexistent/gone.txt"}}"#
        ),
        0
    );
    assert!(env.snapshots().join("s/t.after.absent").exists());
}

#[test]
fn non_file_tools_are_spooled_but_not_snapshotted() {
    let env = Env::new();
    let file = env.dir.path().join("b.txt");
    fs::write(&file, "x\n").unwrap();
    let f = file.display();
    // An MCP tool whose name merely contains "Edit" must not drive snapshots.
    assert_eq!(
        env.run(&format!(
            r#"{{"hook_event_name":"PreToolUse","session_id":"s","tool_use_id":"t","tool_name":"mcp__x__EditThing","cwd":"/p","tool_input":{{"file_path":"{f}"}}}}"#
        )),
        0
    );
    assert!(!env.snapshots().join("s/t").exists());
    assert_eq!(env.spool_lines().len(), 1);
}

#[test]
fn concurrent_writers_do_not_interleave() {
    let env = Env::new();
    const WRITERS: usize = 12;
    // Large enough that the append needs several write() calls, which is what
    // interleaves under O_APPEND without a lock.
    let filler = "z".repeat(200 * 1024);

    let mut children = Vec::new();
    for i in 0..WRITERS {
        let payload = format!(
            r#"{{"hook_event_name":"PostToolUse","session_id":"s","tool_use_id":"t{i}","tool_name":"Write","cwd":"/p","tool_input":{{"file_path":"/tmp/f{i}"}},"tool_response":{{"originalFile":"{filler}"}}}}"#
        );
        let mut child = env.command().stdin(Stdio::piped()).spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        // Feed each child from its own thread so none of them blocks the others.
        let writer = std::thread::spawn(move || {
            let _ = stdin.write_all(payload.as_bytes());
        });
        children.push((child, writer));
    }
    for (mut child, writer) in children {
        assert_eq!(child.wait().unwrap().code(), Some(0));
        writer.join().unwrap();
    }

    let lines = env.spool_lines();
    assert_eq!(lines.len(), WRITERS, "expected one line per writer");
    let mut ids: Vec<String> = Vec::new();
    for l in &lines {
        let v: Value = serde_json::from_str(l).expect("every spool line must be valid JSON");
        ids.push(v["tool_use_id"].as_str().unwrap().to_string());
    }
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), WRITERS, "an event was lost or duplicated");
}

#[test]
fn spool_rotates_on_any_event_not_just_session_start() {
    let env = Env::new();
    fs::create_dir_all(env.spool().parent().unwrap()).unwrap();
    // Just over the hook's 50 MB threshold.
    let mut big = vec![b'x'; 52 * 1024 * 1024];
    *big.last_mut().unwrap() = b'\n';
    fs::write(env.spool(), &big).unwrap();

    assert_eq!(
        env.run(r#"{"hook_event_name":"PostToolUse","session_id":"s","tool_use_id":"t","tool_name":"Edit","cwd":"/p","tool_input":{"file_path":"/tmp/x"}}"#),
        0
    );
    let rotated = env.spool().with_file_name("events.jsonl.1");
    assert!(rotated.exists(), "oversized spool was not rotated");
    assert_eq!(fs::metadata(&rotated).unwrap().len(), big.len() as u64);
    assert_eq!(env.spool_lines().len(), 1);
}

#[test]
fn malformed_payload_still_produces_one_line() {
    let env = Env::new();
    // Not JSON, so the raw payload is spooled with its newlines stripped.
    assert_eq!(env.run("{not json\nat all"), 0);
    assert_eq!(env.spool_lines(), ["{not jsonat all"]);
}
