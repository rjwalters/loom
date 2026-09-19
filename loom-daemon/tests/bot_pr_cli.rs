//! End-to-end CLI coverage for `loom-daemon bot-pr` (issue #4765).
//!
//! Exercised as a subprocess rather than in-process because the handler uses
//! `std::process::exit` to carry the verdict in its exit code — the same
//! reason `tests/tokens_unblock_cli.rs` and `tests/accounts_cli.rs` do.
//!
//! What only this layer can prove, and the unit tests cannot:
//!
//! - the **exit-code contract** (`0` qualifies / `1` does not / `2` could not
//!   run) that `champion-bot-pr.md` branches on;
//! - that stdout is genuinely `eval`-safe — asserted by feeding it to a real
//!   `sh -c` and reading the variables back out, including for a PR title
//!   containing shell metacharacters;
//! - that config resolution reaches a real `.loom/config.json` on disk.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// A minimal Loom repo: `resolve_effective_config` requires both `.git` and
/// `.loom` to resolve a root, and `repo_root::find_repo_root` requires them
/// too.
fn make_repo(champion_block: Option<&str>) -> tempfile::TempDir {
    let td = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(td.path().join(".git")).unwrap();
    std::fs::create_dir_all(td.path().join(".loom")).unwrap();
    let body = match champion_block {
        Some(b) => format!("{{\n  \"champion\": {b}\n}}\n"),
        None => "{}\n".to_string(),
    };
    std::fs::write(td.path().join(".loom").join("config.json"), body).unwrap();
    td
}

fn run(repo: &Path, args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .arg("bot-pr")
        .args(args)
        .arg("--repo-root")
        .arg(repo.to_str().unwrap())
        // Keep a real host's machine-level defaults file out of the assertion
        // (the established convention across this crate).
        .env("LOOM_CONFIG_DEFAULTS_FILE", "")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn write_files(repo: &Path, names: &[&str]) -> PathBuf {
    let p = repo.join("files.txt");
    std::fs::write(&p, names.join("\n")).unwrap();
    p
}

const LOCKFILE_DIFF: &str = "\
diff --git a/Cargo.lock b/Cargo.lock
--- a/Cargo.lock
+++ b/Cargo.lock
@@ -1,3 +1,3 @@
-version = \"1.0.1\"
+version = \"1.0.2\"
";

/// `eval` the subcommand's stdout in a real shell and echo one variable back,
/// which is the only honest test of "is this output safe to eval?".
fn eval_var(stdout: &str, var: &str) -> String {
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!("{stdout}\nprintf '%s' \"${var}\""))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "eval of classifier output failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn config_exits_1_and_reports_disabled_when_the_repo_has_not_opted_in() {
    let repo = make_repo(None);
    let out = run(repo.path(), &["config"], "");
    assert_eq!(out.status.code(), Some(1), "default is OFF");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("BOT_PR_ENABLED='false'"), "{stdout}");
    assert_eq!(eval_var(&stdout, "BOT_PR_MAX_SEMVER"), "all");
}

#[test]
fn config_exits_0_and_echoes_the_knobs_when_enabled() {
    let repo = make_repo(Some(
        r#"{ "autoMergeDependabot": true, "trustedBotAuthors": ["dependabot[bot]", "renovate[bot]"], "dependabotMaxSemver": "minor" }"#,
    ));
    let out = run(repo.path(), &["config"], "");
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(eval_var(&stdout, "BOT_PR_ENABLED"), "true");
    assert_eq!(eval_var(&stdout, "BOT_PR_TRUSTED_AUTHORS"), "dependabot[bot],renovate[bot]");
    assert_eq!(eval_var(&stdout, "BOT_PR_MAX_SEMVER"), "minor");
}

#[test]
fn classify_exits_1_when_the_flag_is_off_whatever_the_pr_looks_like() {
    let repo = make_repo(None);
    let files = write_files(repo.path(), &["Cargo.lock"]);
    let out = run(
        repo.path(),
        &[
            "classify",
            "--author",
            "dependabot[bot]",
            "--title",
            "Bump serde from 1.0.1 to 1.0.2",
            "--files-from",
            files.to_str().unwrap(),
        ],
        LOCKFILE_DIFF,
    );
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(eval_var(&stdout, "BOT_PR_QUALIFIES"), "false");
    assert_eq!(eval_var(&stdout, "BOT_PR_REASON"), "disabled");
}

#[test]
fn classify_exits_0_for_a_lockfile_only_dependabot_pr_when_enabled() {
    let repo = make_repo(Some(r#"{ "autoMergeDependabot": true }"#));
    let files = write_files(repo.path(), &["Cargo.lock"]);
    let out = run(
        repo.path(),
        &[
            "classify",
            "--author",
            "dependabot[bot]",
            "--title",
            "Bump serde from 1.0.1 to 1.0.2",
            "--files-from",
            files.to_str().unwrap(),
        ],
        LOCKFILE_DIFF,
    );
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(eval_var(&stdout, "BOT_PR_QUALIFIES"), "true");
    assert_eq!(eval_var(&stdout, "BOT_PR_WAIVED"), "critical-file,recency-doctor-route");
}

#[test]
fn classify_falls_back_when_the_bot_pr_also_touches_code() {
    let repo = make_repo(Some(r#"{ "autoMergeDependabot": true }"#));
    let files = write_files(repo.path(), &["Cargo.lock", "src/main.rs"]);
    let out = run(
        repo.path(),
        &[
            "classify",
            "--author",
            "dependabot[bot]",
            "--files-from",
            files.to_str().unwrap(),
        ],
        LOCKFILE_DIFF,
    );
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(eval_var(&stdout, "BOT_PR_REASON"), "non-manifest-file");
    assert_eq!(eval_var(&stdout, "BOT_PR_OFFENDING_FILE"), "src/main.rs");
}

#[test]
fn a_hostile_pr_title_cannot_escape_the_eval() {
    // A PR title is untrusted external content. This title would execute in a
    // naively-quoted `KEY=value` render.
    let repo = make_repo(Some(r#"{ "autoMergeDependabot": true }"#));
    let files = write_files(repo.path(), &["Cargo.lock"]);
    let hostile = "'; touch /tmp/loom-bot-pr-pwned; echo '";
    let out = run(
        repo.path(),
        &[
            "classify",
            "--author",
            hostile,
            "--title",
            hostile,
            "--files-from",
            files.to_str().unwrap(),
        ],
        LOCKFILE_DIFF,
    );
    assert_eq!(out.status.code(), Some(1), "hostile author is not trusted");
    let stdout = String::from_utf8_lossy(&out.stdout);
    // The detail carries the hostile string verbatim, and eval returns it as
    // DATA rather than running it.
    let detail = eval_var(&stdout, "BOT_PR_DETAIL");
    assert!(detail.contains(hostile), "detail was {detail}");
    assert!(
        !Path::new("/tmp/loom-bot-pr-pwned").exists(),
        "eval of the classifier's output executed embedded shell"
    );
}

#[test]
fn the_file_list_defaults_to_the_diffs_own_paths_when_not_supplied() {
    let repo = make_repo(Some(r#"{ "autoMergeDependabot": true }"#));
    let out = run(repo.path(), &["classify", "--author", "dependabot[bot]"], LOCKFILE_DIFF);
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn an_unreadable_files_from_path_is_exit_2_not_a_verdict() {
    // The distinction that matters: "the classifier could not run" must never
    // be mistaken for "this PR does not qualify".
    let repo = make_repo(Some(r#"{ "autoMergeDependabot": true }"#));
    let out = run(
        repo.path(),
        &[
            "classify",
            "--author",
            "dependabot[bot]",
            "--files-from",
            "/nonexistent/loom/files.txt",
        ],
        LOCKFILE_DIFF,
    );
    assert_ne!(out.status.code(), Some(0));
    assert_ne!(
        out.status.code(),
        Some(1),
        "an I/O failure must not render as the 'does not qualify' verdict"
    );
}
