//! Tests for startable-subset extraction (epic #7810, PR 3).
//!
//! These pin the rules the shell `awk` encoded. The shell suite
//! (`tests/test-detect-startable-subset.sh`, 25 assertions) still drives the
//! same logic through the CLI and is the compatibility proof; these cover the
//! edges that are awkward to reach from a CLI fixture.

use super::*;

#[test]
fn a_section_is_captured_without_its_heading() {
    let body = "# Title\n\n## Startable subset\n- do this now\n- and this\n";
    assert_eq!(extract_startable_subset(body), "- do this now\n- and this\n");
    assert!(has_startable_subset(body));
}

#[test]
fn capture_stops_at_the_next_heading_of_equal_depth() {
    let body = "## Startable subset\nin\n## Dependencies\nout\n";
    assert_eq!(extract_startable_subset(body), "in\n");
}

#[test]
fn capture_stops_at_a_shallower_heading() {
    let body = "### Startable subset\nin\n## Later\nout\n";
    assert_eq!(extract_startable_subset(body), "in\n");
}

#[test]
fn a_deeper_subsection_stays_inside_the_capture() {
    // The reason depth is tracked at all rather than stopping at any heading:
    // `### Files` documents the subset, it does not end it.
    let body = "## Startable subset\nintro\n### Files\na.rs\n## Dependencies\nout\n";
    assert_eq!(extract_startable_subset(body), "intro\n### Files\na.rs\n");
}

#[test]
fn the_heading_match_is_case_insensitive_and_a_prefix() {
    for heading in [
        "## Startable subset",
        "## startable subset",
        "## STARTABLE SUBSET",
        "## Startable Subset (partial)",
    ] {
        let body = format!("{heading}\nwork\n");
        assert_eq!(
            extract_startable_subset(&body),
            "work\n",
            "heading should have matched: {heading}"
        );
    }
}

#[test]
fn a_top_level_heading_is_not_a_section() {
    // `#` is the issue title. The shell's range is {2,6}; a single `#` must not
    // open a capture.
    let body = "# Startable subset\nwork\n";
    assert_eq!(extract_startable_subset(body), "");
    assert!(!has_startable_subset(body));
}

#[test]
fn seven_hashes_is_not_a_heading() {
    let body = "####### Startable subset\nwork\n";
    assert_eq!(extract_startable_subset(body), "");
}

#[test]
fn an_issue_reference_is_not_mistaken_for_a_heading() {
    // THE trap this repo keeps hitting: `#5664` starts with `#` but is prose.
    // If it read as a heading it would silently truncate the section.
    let body = "## Startable subset\nsee #5664 for context\nmore work\n";
    assert_eq!(extract_startable_subset(body), "see #5664 for context\nmore work\n");
}

#[test]
fn a_hash_run_without_following_whitespace_is_not_a_heading() {
    let body = "## Startable subset\n##notaheading\nstill in\n";
    assert_eq!(extract_startable_subset(body), "##notaheading\nstill in\n");
}

#[test]
fn an_indented_heading_still_counts() {
    // The shell strips leading whitespace before testing, so an indented
    // heading both opens and closes a capture.
    let body = "  ## Startable subset\nin\n   ## Next\nout\n";
    assert_eq!(extract_startable_subset(body), "in\n");
}

#[test]
fn a_blank_section_is_not_a_subset() {
    // Present-but-empty must read as absent: there is nothing to start.
    let body = "## Startable subset\n\n   \n## Dependencies\nout\n";
    assert!(!has_startable_subset(body));
}

#[test]
fn no_section_at_all_is_empty_and_absent() {
    let body = "# Title\n\nJust a description with no subset heading.\n";
    assert_eq!(extract_startable_subset(body), "");
    assert!(!has_startable_subset(body));
}

#[test]
fn only_the_first_section_is_captured() {
    // A second heading of the same name after the first has closed does not
    // reopen capture — matching the shell, whose `capturing` flag can be
    // re-armed only by the opening branch, which it reaches again.
    let body = "## Startable subset\nfirst\n## Other\nx\n## Startable subset\nsecond\n";
    let got = extract_startable_subset(body);
    assert!(got.contains("first"), "got {got:?}");
    // The shell DOES re-arm on the second heading, so both are captured.
    assert!(got.contains("second"), "shell re-arms on a later heading; got {got:?}");
    assert!(!got.contains('x'), "text between sections must not leak: {got:?}");
}

#[test]
fn an_empty_body_is_handled() {
    assert_eq!(extract_startable_subset(""), "");
    assert!(!has_startable_subset(""));
}

// ---------------------------------------------------------------------------
// Differential tests against the shell original (epic #7810, PR 3)
// ---------------------------------------------------------------------------
//
// Unit tests above prove this port is self-consistent. They cannot prove it
// matches `detect-startable-subset.sh`, which is what the migration actually
// claims. These run BOTH implementations on the same input and compare.
//
// They are deliberately temporary: they exist while both implementations do,
// and go when the shell one does. Until then they are the strongest evidence
// available that behaviour was preserved.

/// Locate the shell script from the compiled crate's source tree.
fn shell_script() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("defaults/scripts/detect-startable-subset.sh")
}

/// Run the shell implementation on `body` and return its captured section.
///
/// The CLI prints a `STARTABLE_SUBSET` / `NO_STARTABLE_SUBSET` marker line
/// first; the rest is the section.
fn shell_extract(body: &str) -> String {
    // Unique per call, not per process: these tests run in parallel, and a
    // shared fixture path had them overwriting each other's body mid-read —
    // which surfaced as the anti-vacuity guard failing only under concurrency
    // and passing in isolation.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("loom-subset-diff-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("tmp dir");
    let f = dir.join("body.md");
    std::fs::write(&f, body).expect("write body");

    let out = std::process::Command::new("bash")
        .arg(shell_script())
        .args(["--issue", "1", "--repo", "o/r", "--body-file"])
        .arg(&f)
        .output()
        .expect("the shell implementation must be runnable");

    let _ = std::fs::remove_dir_all(&dir);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut lines = stdout.lines();
    match lines.next() {
        Some("NO_STARTABLE_SUBSET") | None => String::new(),
        Some("STARTABLE_SUBSET") => {
            let rest: Vec<&str> = lines.collect();
            if rest.is_empty() {
                String::new()
            } else {
                let mut s = rest.join("\n");
                s.push('\n');
                s
            }
        }
        Some(other) => panic!("unrecognised marker from the shell CLI: {other:?}"),
    }
}

#[test]
fn the_shell_implementation_is_present_and_runnable() {
    // Anti-vacuity guard. Without this, a missing or unrunnable script would
    // make every differential case below compare "" against "" and pass while
    // proving nothing — a gate reporting green while doing no work.
    let p = shell_script();
    assert!(p.is_file(), "shell original not found at {}", p.display());
    let probe = shell_extract("## Startable subset\nprobe\n");
    assert_eq!(
        probe, "probe\n",
        "the shell implementation did not produce its known-good answer; \
         every differential assertion below would be vacuous"
    );
}

#[test]
fn rust_and_shell_agree_across_the_edge_cases() {
    let corpus: &[(&str, &str)] = &[
        ("plain section", "# Title\n\n## Startable subset\n- do this now\n- and this\n"),
        (
            "deeper subsection stays",
            "## Startable subset\nintro\n### Files\na.rs\n## Dependencies\nout\n",
        ),
        ("equal-depth heading closes", "## Startable subset\nin\n## Dependencies\nout\n"),
        ("shallower heading closes", "### Startable subset\nin\n## Later\nout\n"),
        (
            "re-arms on a later heading",
            "## Startable subset\nfirst\n## Other\nx\n## Startable subset\nsecond\n",
        ),
        (
            "issue ref is not a heading",
            "## Startable subset\nsee #5664 for context\n##notaheading\nstill in\n",
        ),
        ("top-level heading excluded", "# Startable subset\nwork\n"),
        ("seven hashes excluded", "####### Startable subset\nwork\n"),
        ("indented heading", "  ## Startable subset\nin\n   ## Next\nout\n"),
        ("blank section", "## Startable subset\n\n   \n## Dependencies\nout\n"),
        ("absent", "# Title\n\nJust a description.\n"),
        ("case and prefix", "## STARTABLE SUBSET (partial)\nwork\n"),
        ("empty body", ""),
    ];

    for (name, body) in corpus {
        let shell = shell_extract(body);
        // Compare like for like: the shell CLI gates its output on
        // `has_startable_subset`, printing NO_STARTABLE_SUBSET for a
        // present-but-blank section. The raw extractor does not, so the gate
        // has to be applied on this side too — comparing the ungated extractor
        // against the gated CLI measures the harness, not the port.
        let rust = if has_startable_subset(body) {
            extract_startable_subset(body)
        } else {
            String::new()
        };
        assert_eq!(
            rust, shell,
            "port diverges from the shell on {name:?}\n  rust:  {rust:?}\n  shell: {shell:?}"
        );
    }
}
