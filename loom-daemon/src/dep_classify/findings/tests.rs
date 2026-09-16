//! Tests for verdict-finding extraction (epic #7810, PR 3).

use super::*;

#[test]
fn bullets_become_one_finding_per_line() {
    let c = "Verdict prose.\n- first finding\n- second finding\n";
    assert_eq!(extract_findings(c), "- first finding\n- second finding\n");
}

#[test]
fn both_bullet_markers_are_recognised() {
    assert_eq!(extract_findings("- dash\n"), "- dash\n");
    assert_eq!(extract_findings("* star\n"), "* star\n");
}

#[test]
fn a_bullet_marker_needs_following_whitespace() {
    // `*emphasis*` opening a line is prose, not a bullet.
    assert_eq!(extract_findings("*emphasised* prose\n"), "");
}

#[test]
fn an_indented_continuation_folds_onto_its_bullet() {
    let c = "- a finding that wraps\n  onto a second line\n";
    assert_eq!(extract_findings(c), "- a finding that wraps   onto a second line\n");
}

#[test]
fn prose_resuming_ends_the_list() {
    // The load-bearing rule: verdict prose after the bullets often contains
    // issue references. Reading it as findings would classify a merits verdict
    // as a dependency and un-escalate work a human parked deliberately.
    let c = "- a finding\nUnindented prose mentioning #99 as a blocker.\n- not a finding\n";
    assert_eq!(extract_findings(c), "- a finding\n");
}

#[test]
fn a_blank_line_ends_the_list() {
    // A blank line has no non-space character, so it is not a continuation.
    let c = "- a finding\n\n- after the gap\n";
    assert_eq!(extract_findings(c), "- a finding\n");
}

#[test]
fn recommended_actions_ends_the_list() {
    let c = "- a finding\n**Recommended actions**\n- do this\n";
    assert_eq!(extract_findings(c), "- a finding\n");
}

#[test]
fn recommended_actions_before_any_bullet_yields_nothing() {
    let c = "Some prose.\n**Recommended actions**\n- do this\n";
    assert_eq!(extract_findings(c), "");
}

#[test]
fn prose_before_the_first_bullet_is_skipped() {
    let c = "Intro line.\nAnother intro line.\n- the finding\n";
    assert_eq!(extract_findings(c), "- the finding\n");
}

#[test]
fn a_comment_with_no_bullets_yields_nothing() {
    assert_eq!(extract_findings("Just prose, no list.\n"), "");
    assert_eq!(extract_findings(""), "");
}

// ---------------------------------------------------------------------------
// Differential tests against the shell original
// ---------------------------------------------------------------------------

fn shell_script() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("defaults/scripts/classify-dependency-block.sh")
}

fn shell_extract_findings(comment: &str) -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("loom-findings-diff-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("tmp dir");
    let f = dir.join("comment.md");
    std::fs::write(&f, comment).expect("write comment");

    let driver = format!(
        r#"set -uo pipefail
source "{script}" --help >/dev/null 2>&1 || true
comment="$(cat "{f}")"
extract_findings "$comment"
"#,
        script = shell_script().display(),
        f = f.display(),
    );
    let driver_path = dir.join("driver.sh");
    std::fs::write(&driver_path, driver).expect("write driver");

    let out = std::process::Command::new("bash")
        .arg(&driver_path)
        .output()
        .expect("the shell implementation must be runnable");
    let _ = std::fs::remove_dir_all(&dir);
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[test]
fn the_shell_extractor_is_reachable() {
    // Anti-vacuity guard: asserts a known-good NON-EMPTY answer, since most of
    // the corpus below would be satisfied by an implementation that always
    // returned "".
    let probe = shell_extract_findings("- a finding\n");
    assert_eq!(
        probe, "- a finding\n",
        "the shell extract_findings did not return its known-good answer; \
         every differential assertion below would be unreliable"
    );
}

#[test]
fn rust_and_shell_agree_on_finding_extraction() {
    let corpus: &[(&str, &str)] = &[
        ("two bullets", "Verdict prose.\n- first finding\n- second finding\n"),
        ("star marker", "* star\n"),
        ("emphasis is not a bullet", "*emphasised* prose\n"),
        ("continuation", "- a finding that wraps\n  onto a second line\n"),
        (
            "prose ends the list",
            "- a finding\nUnindented prose mentioning #99 as a blocker.\n- not a finding\n",
        ),
        ("blank line ends the list", "- a finding\n\n- after the gap\n"),
        ("recommended actions", "- a finding\n**Recommended actions**\n- do this\n"),
        (
            "recommended actions first",
            "Some prose.\n**Recommended actions**\n- do this\n",
        ),
        ("prose before bullets", "Intro line.\nAnother intro line.\n- the finding\n"),
        ("no bullets", "Just prose, no list.\n"),
        ("empty", ""),
        (
            "realistic verdict",
            "**Verdict**: defer\n\n- **Technical feasibility**: Blocked by #3\n  which is still open\n- Scope is clear\n\n**Recommended actions**\n- wait\n",
        ),
        ("indented bullet", "  - indented bullet\n"),
        ("multiple continuations", "- one\n  two\n  three\n- next\n"),
    ];

    for (name, comment) in corpus {
        let shell = shell_extract_findings(comment);
        let rust = extract_findings(comment);
        assert_eq!(
            rust, shell,
            "port diverges from the shell on {name:?}\n  rust:  {rust:?}\n  shell: {shell:?}"
        );
    }
}
