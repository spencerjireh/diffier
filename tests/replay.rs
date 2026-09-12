//! Fixture-driven replay: a spool plus snapshots and a post-edit file tree
//! must produce a stable set of cards.

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use diffier::render::{EditCard, Renderer};
use diffier::session::{Pipeline, session_order};
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
    let cards: Vec<EditCard> = inputs
        .into_iter()
        .map(|input| EditCard::new(input, &Renderer::Plain, 80))
        .collect();

    let mut out = String::new();
    for card in &cards {
        out.push_str("== ");
        out.push_str(&card.summary(tags));
        out.push('\n');
        for line in card.body_plain() {
            out.push_str(&line);
            out.push('\n');
        }
        out.push('\n');
    }
    // The orphan Pre stays pending; nothing from s0, /somewhere/else, or Bash.
    assert_eq!(pipeline.pending_len(), 1);
    assert_eq!(pipeline.session_id.as_deref(), Some("s1"));
    assert_eq!(replay.offset, fs::metadata(&spool_path).unwrap().len());
    insta::assert_snapshot!("replay_cards", out);
}

/// Run `diffier dump` over the fixture with `extra` args and return stdout.
/// `--cwd-only` keeps the result independent of whether the temp dir happens
/// to sit inside a git repository.
fn dump(cwd: &Path, spool_path: &Path, extra: &[&str]) -> String {
    let assert = Command::cargo_bin("diffier")
        .unwrap()
        .args(["dump", "--no-delta", "--cwd-only", "--spool"])
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
    assert!(stdout.contains("+++ b/notes.txt"), "{stdout}");
    assert!(stdout.contains("[Explore]"), "{stdout}");
    assert!(!stdout.contains("orphan"), "{stdout}");
    assert!(!stdout.contains("somewhere/else"), "{stdout}");
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
