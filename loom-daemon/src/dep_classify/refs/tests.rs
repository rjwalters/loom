//! Tests for dependency-reference parsing (epic #7810, PR 3).
//!
//! As with `subset`, the unit tests pin the rules and the differential tests
//! prove the port matches `detect-dependency-cycle.sh`'s `parse_dependency_refs`.

use super::*;

const REPO: &str = "o/r";

#[test]
fn a_bare_reference_takes_the_default_repo() {
    assert_eq!(parse_dependency_refs("Blocked by #3", REPO), vec!["o/r#3"]);
}

#[test]
fn a_qualified_reference_keeps_its_own_repo() {
    assert_eq!(parse_dependency_refs("Depends on other/repo#7", REPO), vec!["other/repo#7"]);
}

#[test]
fn a_url_is_reduced_to_owner_repo_and_number() {
    assert_eq!(
        parse_dependency_refs("Requires https://github.com/acme/widgets/issues/42", REPO),
        vec!["acme/widgets#42"]
    );
}

#[test]
fn all_three_phrases_are_recognised() {
    for phrase in ["Blocked by", "Depends on", "Requires"] {
        let body = format!("{phrase} #5");
        assert_eq!(
            parse_dependency_refs(&body, REPO),
            vec!["o/r#5"],
            "phrase should have matched: {phrase}"
        );
    }
}

#[test]
fn the_phrase_match_is_case_sensitive() {
    // `grep -E` without `-i`. Preserved deliberately: changing it would silently
    // widen what counts as a declared dependency across every existing body.
    assert!(parse_dependency_refs("blocked by #3", REPO).is_empty());
    assert!(parse_dependency_refs("BLOCKED BY #3", REPO).is_empty());
}

#[test]
fn markdown_emphasis_between_phrase_and_reference_is_tolerated() {
    // The `[*_:\s]*` class exists for `**Blocked by**: #3`.
    assert_eq!(parse_dependency_refs("**Blocked by**: #3", REPO), vec!["o/r#3"]);
    assert_eq!(parse_dependency_refs("_Requires_ #4", REPO), vec!["o/r#4"]);
}

#[test]
fn every_reference_on_a_matching_line_is_captured_not_just_the_first() {
    // The shell's second grep scans the WHOLE line. `#99` here is arguably
    // over-capture, but it is the established behaviour and fixtures depend on
    // it. Pinned so a future "tidy-up" cannot quietly narrow it.
    assert_eq!(
        parse_dependency_refs("Blocked by #3 (see also #99)", REPO),
        vec!["o/r#3", "o/r#99"]
    );
}

#[test]
fn a_line_without_a_phrase_contributes_nothing() {
    assert!(parse_dependency_refs("Mentions #3 in passing", REPO).is_empty());
}

#[test]
fn results_are_sorted_and_deduplicated() {
    let body = "Blocked by #9\nDepends on #2\nRequires #9\n";
    assert_eq!(parse_dependency_refs(body, REPO), vec!["o/r#2", "o/r#9"]);
}

#[test]
fn a_url_too_short_to_carry_owner_and_repo_is_dropped() {
    // The shell's `[[ "$rest" == */* ]] &&` emits nothing rather than erroring.
    let got = parse_dependency_refs("Blocked by https://example.com/issues/42", REPO);
    assert!(got.is_empty(), "expected no refs, got {got:?}");
}

#[test]
fn an_empty_body_yields_nothing() {
    assert!(parse_dependency_refs("", REPO).is_empty());
}

// ---------------------------------------------------------------------------
// Differential tests against the shell original
// ---------------------------------------------------------------------------

fn shell_script() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("defaults/scripts/detect-dependency-cycle.sh")
}

/// Call the shell's `parse_dependency_refs` directly by sourcing the script.
///
/// Sourcing rather than driving the CLI: the CLI walks the dependency graph and
/// needs a live forge, while this function is pure. `--help` short-circuits the
/// script's `main` so sourcing does not run it.
fn shell_parse(body: &str, default_repo: &str) -> Vec<String> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("loom-refs-diff-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("tmp dir");
    let body_file = dir.join("body.md");
    std::fs::write(&body_file, body).expect("write body");

    let driver = format!(
        r#"set -uo pipefail
# The script runs main() only when executed, not sourced — but it also parses
# argv at load. Guard by feeding it nothing and ignoring a non-zero exit.
source "{script}" --help >/dev/null 2>&1 || true
body="$(cat "{body_file}")"
parse_dependency_refs "$body" "{repo}"
"#,
        script = shell_script().display(),
        body_file = body_file.display(),
        repo = default_repo,
    );
    let script_path = dir.join("driver.sh");
    std::fs::write(&script_path, driver).expect("write driver");

    let out = std::process::Command::new("bash")
        .arg(&script_path)
        .output()
        .expect("the shell implementation must be runnable");
    let _ = std::fs::remove_dir_all(&dir);

    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect()
}

#[test]
fn the_shell_parser_is_reachable() {
    // Anti-vacuity guard: without this, an unsourceable script would make every
    // comparison below empty-vs-empty and pass while testing nothing.
    let probe = shell_parse("Blocked by #3", REPO);
    assert_eq!(
        probe,
        vec!["o/r#3"],
        "the shell parse_dependency_refs did not return its known-good answer; \
         every differential assertion below would be vacuous"
    );
}

#[test]
fn rust_and_shell_agree_on_reference_parsing() {
    let corpus: &[(&str, &str)] = &[
        ("bare ref", "Blocked by #3"),
        ("qualified ref", "Depends on other/repo#7"),
        ("url", "Requires https://github.com/acme/widgets/issues/42"),
        ("bold phrase", "**Blocked by**: #3"),
        ("underscore phrase", "_Requires_ #4"),
        ("lower case is ignored", "blocked by #3"),
        ("two refs on one line", "Blocked by #3 (see also #99)"),
        ("no phrase", "Mentions #3 in passing"),
        ("dedup and sort", "Blocked by #9\nDepends on #2\nRequires #9\n"),
        ("short url dropped", "Blocked by https://example.com/issues/42"),
        ("mixed lines", "intro\nBlocked by #1\nnoise #55\nRequires acme/w#2\n"),
        ("trailing punctuation", "Blocked by #3, and Requires #4."),
        ("empty", ""),
        ("phrase with no ref", "Blocked by a design decision"),
    ];

    for (name, body) in corpus {
        let shell = shell_parse(body, REPO);
        let rust = parse_dependency_refs(body, REPO);
        assert_eq!(
            rust, shell,
            "port diverges from the shell on {name:?}\n  rust:  {rust:?}\n  shell: {shell:?}"
        );
    }
}
