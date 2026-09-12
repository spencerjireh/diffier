//! Repo-wide matching against a real git repository with a second worktree.
//! Skipped when git is not installed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use diffier::event::HookEvent;
use diffier::session::Pipeline;
use diffier::spool::{CwdMatch, CwdMatcher, MatchMode};

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// A repo at `<root>/main` with a worktree at `<root>/wt`, both canonical.
fn setup(root: &Path) -> Option<(PathBuf, PathBuf)> {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("git not installed; skipping");
        return None;
    }
    let main = root.join("main");
    fs::create_dir_all(main.join("src")).unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["commit", "-q", "--allow-empty", "-m", "init"]);
    let wt = root.join("wt");
    git(
        &main,
        &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "feat"],
    );
    Some((
        fs::canonicalize(main).unwrap(),
        fs::canonicalize(wt).unwrap(),
    ))
}

#[test]
fn repo_mode_matches_every_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let Some((main, wt)) = setup(tmp.path()) else {
        return;
    };
    let mut m = CwdMatcher::new(main.clone(), MatchMode::Repo);
    assert!(m.repo_mode());
    assert_eq!(m.toplevel(), Some(main.as_path()));
    assert_eq!(
        m.resolve(wt.to_str().unwrap()),
        Some(CwdMatch::Repo {
            toplevel: wt.clone()
        })
    );
    assert_eq!(
        m.resolve(main.join("src").to_str().unwrap()),
        Some(CwdMatch::Repo {
            toplevel: main.clone()
        })
    );
    assert_eq!(m.resolve(tmp.path().to_str().unwrap()), None);

    let mut exact = CwdMatcher::new(main.clone(), MatchMode::CwdOnly);
    assert!(!exact.matches(wt.to_str().unwrap()));
    assert!(exact.matches(main.to_str().unwrap()));
}

fn post(cwd: &Path, file: &Path) -> HookEvent {
    HookEvent::parse_line(&format!(
        r#"{{"hook_event_name":"PostToolUse","session_id":"s","cwd":"{}","tool_name":"Write","tool_use_id":"{}","tool_input":{{"file_path":"{}"}},"tool_response":{{"originalFile":""}}}}"#,
        cwd.display(),
        file.display(),
        file.display()
    ))
    .unwrap()
}

#[test]
fn worktree_cards_carry_the_basename_and_relative_path() {
    let tmp = tempfile::tempdir().unwrap();
    let Some((main, wt)) = setup(tmp.path()) else {
        return;
    };
    let mut p = Pipeline::new(main.clone(), tmp.path().join("snaps"), MatchMode::Repo);

    let in_wt = wt.join("a.rs");
    fs::write(&in_wt, "x\n").unwrap();
    let card = p.handle(&post(&wt, &in_wt)).expect("worktree card");
    assert_eq!(card.worktree.as_deref(), Some("wt"));
    assert_eq!(card.path, "a.rs");
    assert_eq!(card.root, wt);

    let in_main = main.join("src/a.rs");
    fs::write(&in_main, "x\n").unwrap();
    let card = p
        .handle(&post(&main.join("src"), &in_main))
        .expect("subdir card");
    assert_eq!(card.worktree, None);
    assert_eq!(card.path, "src/a.rs");
    assert_eq!(card.root, main);
}
