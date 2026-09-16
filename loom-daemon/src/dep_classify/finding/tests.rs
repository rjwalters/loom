//! Tests for dependency-finding classification (epic #7810, PR 3).

use super::*;

#[test]
fn a_phrase_immediately_followed_by_a_reference_is_a_dependency() {
    assert!(is_dependency_finding("Blocked by #3"));
    assert!(is_dependency_finding("- Depends on acme/widgets#7"));
}

#[test]
fn a_reference_shortly_before_a_bare_phrase_is_a_dependency() {
    // The lead window: the bare forms follow their subject.
    assert!(is_dependency_finding("#3 is blocking this work"));
    assert!(is_dependency_finding("acme/w#12 blocks the rollout"));
}

#[test]
fn a_phrase_with_no_reference_anywhere_is_not_a_dependency() {
    assert!(!is_dependency_finding("Blocked by a pending design decision"));
}

#[test]
fn a_reference_with_no_phrase_anywhere_is_not_a_dependency() {
    assert!(!is_dependency_finding("See #3 for background"));
}

#[test]
fn co_occurrence_alone_is_not_enough() {
    // #7756: the case that motivated proximity at all. "prerequisite" explains
    // why a soak has not started; it does not cite #7430 as a blocker. Without
    // a window this reads as a dependency and defers the issue forever.
    let bullet = "#7430 (which was a prerequisite for any meaningful soak) merged only \
                  minutes before this evaluation, so no soak observation window has \
                  started yet and the result is therefore not yet observable";
    assert!(
        !is_dependency_finding(bullet),
        "a phrase far from the reference must not read as a dependency"
    );
}

#[test]
fn the_phrase_match_is_case_insensitive() {
    // Unlike parse_dependency_refs, this one IS case-insensitive (grep -qiE).
    // The two functions genuinely differ; a rewrite that harmonised them would
    // change behaviour in one of them.
    assert!(is_dependency_finding("blocked by #3"));
    assert!(is_dependency_finding("BLOCKED BY #3"));
}

#[test]
fn a_url_reference_counts() {
    assert!(is_dependency_finding("Blocked by https://github.com/acme/widgets/issues/42"));
    assert!(is_dependency_finding("Requires https://github.com/acme/widgets/pull/9"));
}

#[test]
fn findings_are_dependency_only_requires_every_line_to_qualify() {
    let all_deps = "Blocked by #3\nDepends on #4\n";
    assert!(findings_are_dependency_only(all_deps));

    let mixed = "Blocked by #3\nThe approach is wrong on the merits\n";
    assert!(
        !findings_are_dependency_only(mixed),
        "one merits finding must disqualify the whole set"
    );
}

#[test]
fn blank_lines_are_skipped_but_an_all_blank_set_does_not_qualify() {
    assert!(findings_are_dependency_only("Blocked by #3\n\n   \nDepends on #4\n"));
    // "No findings" is NOT "only dependency findings" — treating it as such
    // would un-escalate a proposal nobody actually re-evaluated.
    assert!(!findings_are_dependency_only(""));
    assert!(!findings_are_dependency_only("\n   \n\n"));
}

/// #7877, recorded as a KNOWN FAILURE rather than fixed here.
///
/// This is a real, reproduced false negative: a genuine dependency finding that
/// the 60-character window misses. It is deliberately asserted in its CURRENT
/// (wrong) form so the port is provably behaviour-identical to the shell. When
/// #7877 is fixed, this assertion flips — and that flip is the visible, intended
/// diff, not an ambiguous port regression.
#[test]
fn issue_7877_the_window_is_too_narrow_current_behaviour_is_wrong() {
    let bullet = "**Technical feasibility**: this issue's own Dependencies section states it \
                  is \"Blocked by the sibling Phase 4 issue (run-job seam contract + host \
                  executor)\" — that issue is #7853, which is currently OPEN";
    assert!(
        !is_dependency_finding(bullet),
        "if this now passes, #7877 has been fixed — update this test and the shell suite \
         together, deliberately"
    );
}

// ---------------------------------------------------------------------------
// Differential tests against the shell original
// ---------------------------------------------------------------------------

fn shell_script() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("defaults/scripts/classify-dependency-block.sh")
}

/// Call the shell's `is_dependency_finding` directly by sourcing the script.
fn shell_is_dependency_finding(bullet: &str) -> bool {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("loom-finding-diff-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("tmp dir");
    let bullet_file = dir.join("bullet.txt");
    std::fs::write(&bullet_file, bullet).expect("write bullet");

    let driver = format!(
        r#"set -uo pipefail
source "{script}" --help >/dev/null 2>&1 || true
bullet="$(cat "{bullet_file}")"
if is_dependency_finding "$bullet"; then echo YES; else echo NO; fi
"#,
        script = shell_script().display(),
        bullet_file = bullet_file.display(),
    );
    let driver_path = dir.join("driver.sh");
    std::fs::write(&driver_path, driver).expect("write driver");

    let out = std::process::Command::new("bash")
        .arg(&driver_path)
        .output()
        .expect("the shell implementation must be runnable");
    let _ = std::fs::remove_dir_all(&dir);

    let stdout = String::from_utf8_lossy(&out.stdout);
    match stdout.trim() {
        "YES" => true,
        "NO" => false,
        other => panic!("unrecognised answer from the shell predicate: {other:?}"),
    }
}

#[test]
fn the_shell_predicate_is_reachable() {
    // Anti-vacuity guard, asserting BOTH answers: a driver that always printed
    // NO would otherwise satisfy most of the corpus below by accident.
    assert!(
        shell_is_dependency_finding("Blocked by #3"),
        "the shell predicate did not return its known-good YES; \
         every differential assertion below would be unreliable"
    );
    assert!(
        !shell_is_dependency_finding("nothing relevant here"),
        "the shell predicate did not return its known-good NO"
    );
}

#[test]
fn rust_and_shell_agree_on_dependency_classification() {
    let corpus: &[(&str, &str)] = &[
        ("immediate phrase+ref", "Blocked by #3"),
        ("bullet form", "- Depends on acme/widgets#7"),
        ("lead window", "#3 is blocking this work"),
        ("lead window qualified", "acme/w#12 blocks the rollout"),
        ("phrase, no ref", "Blocked by a pending design decision"),
        ("ref, no phrase", "See #3 for background"),
        ("upper case phrase", "BLOCKED BY #3"),
        ("url issue", "Blocked by https://github.com/acme/widgets/issues/42"),
        ("url pull", "Requires https://github.com/acme/widgets/pull/9"),
        ("waiting on", "Waiting on #11 before this can start"),
        ("cannot start until", "cannot start until #12 lands"),
        ("must wait for", "must wait for #13"),
        ("dependency on", "has a dependency on #14"),
        (
            "#7756 far co-occurrence",
            "#7430 (which was a prerequisite for any meaningful soak) merged only minutes \
             before this evaluation, so no soak observation window has started yet and the \
             result is therefore not yet observable",
        ),
        (
            "#7877 far ref after phrase",
            "**Technical feasibility**: this issue's own Dependencies section states it is \
             \"Blocked by the sibling Phase 4 issue (run-job seam contract + host executor)\" \
             — that issue is #7853, which is currently OPEN",
        ),
        ("empty", ""),
        ("just prose", "The approach is wrong on the merits"),
    ];

    for (name, bullet) in corpus {
        let shell = shell_is_dependency_finding(bullet);
        let rust = is_dependency_finding(bullet);
        assert_eq!(
            rust, shell,
            "port diverges from the shell on {name:?}\n  rust:  {rust}\n  shell: {shell}"
        );
    }
}
