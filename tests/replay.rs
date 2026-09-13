//! Fixture-driven replay: a spool plus snapshots and a post-edit file tree
//! must produce a stable set of cards.

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use diffier::render::{EditCard, LayoutOpts, ViewMode};
use diffier::session::{CardInput, Pipeline, session_order};
use diffier::spool::{self, MatchMode};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn copy_dir(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let target = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// Materialize the fixture: returns (project dir, spool path).
fn setup(root: &Path) -> (PathBuf, PathBuf) {
    let cwd = root.join("proj");
    copy_dir(&fixtures().join("tree"), &cwd);
    let cwd = fs::canonicalize(&cwd).unwrap();
    let template = fs::read_to_string(fixtures().join("spool.jsonl")).unwrap();
    let spool_path = root.join("events.jsonl");
    fs::write(
        &spool_path,
        template.replace("{{CWD}}", &cwd.to_string_lossy()),
    )
    .unwrap();
    (cwd, spool_path)
}

/// Cards as summary plus body, blank-separated.
fn render_cards(inputs: &[CardInput], mode: ViewMode, width: u16, tags: bool) -> String {
    let opts = LayoutOpts {
        mode,
        width,
        wrap: true,
    };
    let mut out = String::new();
    for input in inputs {
        let card = EditCard::new(input.clone(), &opts);
        out.push_str("== ");
        out.push_str(&card.summary(tags));
        out.push('\n');
        for line in card.body_plain() {
            out.push_str(&line);
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

#[test]
fn replay_renders_expected_cards() {
    let tmp = tempfile::tempdir().unwrap();
    let (cwd, spool_path) = setup(tmp.path());
    let mut pipeline = Pipeline::new(
        cwd.clone(),
        fixtures().join("snapshots"),
        MatchMode::CwdOnly,
    );
    let replay = spool::scan_replay(&spool_path, pipeline.matcher_mut());
    let inputs = pipeline.replay(&replay.events);
    // s0 is stale, so the feed holds one session and headers carry no tag.
    let tags = session_order(&inputs).len() > 1;
    assert!(!tags);
    // The orphan Pre stays pending; nothing from s0, /somewhere/else, or Bash.
    assert_eq!(pipeline.pending_len(), 1);
    assert_eq!(pipeline.session_id.as_deref(), Some("s1"));
    assert_eq!(replay.offset, fs::metadata(&spool_path).unwrap().len());
    insta::assert_snapshot!(
        "replay_cards",
        render_cards(&inputs, ViewMode::Unified, 80, tags)
    );
    insta::assert_snapshot!(
        "replay_cards_split",
        render_cards(&inputs, ViewMode::SideBySide, 120, tags)
    );
}

/// Run `diffier dump` over the fixture with `extra` args and return stdout.
/// `--cwd-only` keeps the result independent of whether the temp dir happens
/// to sit inside a git repository.
fn dump(cwd: &Path, spool_path: &Path, extra: &[&str]) -> String {
    let assert = Command::cargo_bin("diffier")
        .unwrap()
        .args(["dump", "--cwd-only", "--spool"])
        .arg(spool_path)
        .arg("--snapshots")
        .arg(fixtures().join("snapshots"))
        .arg("--cwd")
        .arg(cwd)
        .args(extra)
        .assert()
        .success();
    String::from_utf8(assert.get_output().stdout.clone()).unwrap()
}

#[test]
fn dump_subcommand_prints_cards() {
    let tmp = tempfile::tempdir().unwrap();
    let (cwd, spool_path) = setup(tmp.path());
    let stdout = dump(&cwd, &spool_path, &[]);
    assert!(stdout.contains("== src/main.rs · Edit · "), "{stdout}");
    assert!(stdout.contains("-    println!(\"hi\");"), "{stdout}");
    assert!(stdout.contains("== notes.txt · Write · "), "{stdout}");
    assert!(stdout.contains("[Explore]"), "{stdout}");
    assert!(!stdout.contains("orphan"), "{stdout}");
    assert!(!stdout.contains("somewhere/else"), "{stdout}");
}

#[test]
fn dump_side_by_side_flag_splits() {
    let tmp = tempfile::tempdir().unwrap();
    let (cwd, spool_path) = setup(tmp.path());
    let stdout = dump(&cwd, &spool_path, &["--side-by-side", "--width", "120"]);
    let row = stdout
        .lines()
        .find(|l| l.contains("-    println!(\"hi\");"))
        .unwrap_or_else(|| panic!("{stdout}"));
    assert!(
        row.contains("│") && row.contains("+    println!(\"hello\");"),
        "{row}"
    );
    // Below the threshold the same flag prints unified.
    let narrow = dump(&cwd, &spool_path, &["--side-by-side", "--width", "80"]);
    assert!(!narrow.contains('│'), "{narrow}");
    assert!(narrow.contains("── @@ -1,3 +1,3 @@ ─"), "{narrow}");
}

#[test]
fn dump_no_wrap_flag_cuts_long_lines() {
    let tmp = tempfile::tempdir().unwrap();
    let (cwd, spool_path) = setup(tmp.path());
    // The notebook line is longer than a 40-column unified layout allows.
    let wrapped = dump(&cwd, &spool_path, &["--width", "40"]);
    assert!(!wrapped.contains('…'), "{wrapped}");
    let cut = dump(&cwd, &spool_path, &["--width", "40", "--no-wrap"]);
    assert!(cut.contains('…'), "{cut}");
    assert!(cut.lines().count() < wrapped.lines().count());
}

#[test]
fn dump_session_filter_matches_prefix_or_suffix() {
    let tmp = tempfile::tempdir().unwrap();
    let (cwd, spool_path) = setup(tmp.path());
    for needle in ["s1", "1"] {
        let stdout = dump(&cwd, &spool_path, &["--session", needle]);
        assert!(stdout.contains("== src/main.rs"), "{needle}: {stdout}");
    }
    for needle in ["s0", "zz"] {
        let stdout = dump(&cwd, &spool_path, &["--session", needle]);
        assert!(stdout.trim().is_empty(), "{needle}: {stdout}");
    }
}

#[test]
fn dump_hides_temp_dir_edits_without_all_files() {
    let tmp = tempfile::tempdir().unwrap();
    let (cwd, spool_path) = setup(tmp.path());
    let mut spool = fs::read_to_string(&spool_path).unwrap();
    spool.push_str(&format!(
        "{{\"session_id\":\"s1\",\"cwd\":\"{}\",\"hook_event_name\":\"PostToolUse\",\"tool_name\":\"Write\",\"tool_use_id\":\"sp1\",\"tool_input\":{{\"file_path\":\"/tmp/claude-1/proj/s1/scratchpad/note.txt\"}},\"ts\":9001}}\n",
        cwd.to_string_lossy()
    ));
    fs::write(&spool_path, spool).unwrap();
    let stdout = dump(&cwd, &spool_path, &[]);
    assert!(!stdout.contains("scratchpad"), "{stdout}");
    assert!(stdout.contains("== notes.txt"), "{stdout}");
    let stdout = dump(&cwd, &spool_path, &["--all-files"]);
    assert!(
        stdout.contains("== /tmp/claude-1/proj/s1/scratchpad/note.txt · Write · "),
        "{stdout}"
    );
}

#[test]
fn dump_without_spool_fails_cleanly() {
    let tmp = tempfile::tempdir().unwrap();
    Command::cargo_bin("diffier")
        .unwrap()
        .args(["dump", "--spool"])
        .arg(tmp.path().join("missing.jsonl"))
        .assert()
        .failure()
        .stderr(predicates::str::contains("spool not found"));
}
