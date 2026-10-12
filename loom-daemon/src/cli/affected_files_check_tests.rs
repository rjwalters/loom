//! Tests for the `affected-files check` argument surface (issue #9400).

use std::fs;
use std::process::Command;

use loom_daemon::work_finder::affected_files::check::{
    EXIT_CLEAR, EXIT_COULD_NOT_RUN, EXIT_UNCOMMITTED, EXIT_UNKNOWN,
};

use super::*;
use crate::cli::whole_cli_parse::try_parse_cli;

/// Parse `args` through the whole binary CLI and return the `check` args.
fn parse(args: &[&str]) -> CheckArgs {
    let mut argv = vec!["loom-daemon", "affected-files", "check"];
    argv.extend_from_slice(args);
    let Ok(cli) = try_parse_cli(&argv) else {
        panic!("{argv:?} did not parse");
    };
    match cli.command {
        Some(crate::Commands::ScriptPorts(
            crate::cli::script_ports::ScriptPortCommand::AffectedFiles(
                AffectedFilesCommand::Check(args),
            ),
        )) => args,
        _ => panic!("{argv:?} parsed to another command"),
    }
}

/// A repo committing `src/kept.rs`, with `src/draft.rs` untracked.
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("src/kept.rs"), "fn kept() {}\n").unwrap();
    for args in [
        &["init", "-q"][..],
        &["add", "-A"],
        &["commit", "-q", "-m", "fixture"],
    ] {
        let status = Command::new("git")
            .arg("-C")
            .arg(root)
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }
    fs::write(root.join("src/draft.rs"), "fn draft() {}\n").unwrap();
    dir
}

/// Run the verb over `body` in `repo` against `HEAD`.
fn run(repo: &tempfile::TempDir, body: &str) -> (i32, String) {
    let body_file = repo.path().join("body.md");
    fs::write(&body_file, body).unwrap();
    parse(&[
        "--body-file",
        body_file.to_str().unwrap(),
        "--base",
        "HEAD",
        "--workspace",
        repo.path().to_str().unwrap(),
    ])
    .evaluate()
}

#[test]
fn base_defaults_to_origin_main_and_body_file_is_required() {
    let args = parse(&["--body-file", "-"]);
    assert_eq!(args.base, "origin/main");
    assert!(args.workspace.is_none());
    assert!(try_parse_cli(&["loom-daemon", "affected-files", "check"]).is_err());
}

#[test]
fn each_verdict_has_its_own_exit_code() {
    let repo = fixture();
    let clear = "## Affected Files\n\n- `src/kept.rs` - edit\n- `src/made.rs` (new)\n";
    assert_eq!(
        run(&repo, clear),
        (EXIT_CLEAR, "present\tsrc/kept.rs\nnew\tsrc/made.rs\n".into())
    );
    let unknown = "## Affected Files\n\n- `src/typo.rs` - edit\n";
    assert_eq!(run(&repo, unknown), (EXIT_UNKNOWN, "unknown\tsrc/typo.rs\n".into()));
    let parked = "## Affected Files\n\n- `src/draft.rs` - edit\n- `src/typo.rs`\n";
    assert_eq!(
        run(&repo, parked),
        (EXIT_UNCOMMITTED, "uncommitted\tsrc/draft.rs\nunknown\tsrc/typo.rs\n".into())
    );
    assert_eq!(run(&repo, "No section here.\n"), (EXIT_CLEAR, String::new()));
}

#[test]
fn a_check_that_cannot_run_is_never_a_verdict() {
    let repo = fixture();
    let ws = repo.path().to_str().unwrap();
    let body_file = repo.path().join("body.md");
    fs::write(&body_file, "## Affected Files\n\n- `src/draft.rs`\n").unwrap();
    let body = body_file.to_str().unwrap();

    // The default base (`origin/main`) does not exist in the fixture.
    let unreachable = parse(&["--body-file", body, "--workspace", ws]).evaluate();
    assert_eq!(unreachable, (EXIT_COULD_NOT_RUN, String::new()));

    let missing = repo.path().join("absent.md");
    let unreadable = parse(&[
        "--body-file",
        missing.to_str().unwrap(),
        "--base",
        "HEAD",
        "--workspace",
        ws,
    ])
    .evaluate();
    assert_eq!(unreadable, (EXIT_COULD_NOT_RUN, String::new()));
}

#[test]
fn help_documents_every_exit_code() {
    let err = match try_parse_cli(&["loom-daemon", "affected-files", "check", "--help"]) {
        Err(err) => err.to_string(),
        Ok(_) => panic!("--help must not parse to a command"),
    };
    for needle in [
        "0  clear",
        "10  unknown",
        "11  uncommitted",
        "3  could not run",
        "(new)",
    ] {
        assert!(err.contains(needle), "help lacks {needle:?}:\n{err}");
    }
}
