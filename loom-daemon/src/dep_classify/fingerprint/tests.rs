//! Tests for blocker fingerprints (epic #7810, PR 3).
//!
//! These matter more than most: fingerprints are written into live issue
//! comments, so a divergence does not fail loudly — it makes Champion re-fight
//! decisions it already made.

use super::*;

#[test]
fn a_fingerprint_is_sixteen_hex_characters() {
    let f = fingerprint("o/r#3");
    assert_eq!(f.len(), 16, "got {f:?}");
    assert!(f.chars().all(|c| c.is_ascii_hexdigit()), "got {f:?}");
}

#[test]
fn order_does_not_matter() {
    // The whole point: the same SET of blockers is the same decision.
    assert_eq!(fingerprint("o/r#3 o/r#9"), fingerprint("o/r#9 o/r#3"));
}

#[test]
fn duplicates_do_not_matter() {
    assert_eq!(fingerprint("o/r#3 o/r#3 o/r#9"), fingerprint("o/r#3 o/r#9"));
}

#[test]
fn any_whitespace_run_separates_nodes() {
    // The shell's unquoted `$nodes` splits on IFS, so newlines and runs of
    // spaces behave identically to single spaces.
    let want = fingerprint("o/r#3 o/r#9");
    assert_eq!(fingerprint("o/r#3\no/r#9"), want);
    assert_eq!(fingerprint("  o/r#3   o/r#9  "), want);
    assert_eq!(fingerprint("o/r#3\t o/r#9\n"), want);
}

#[test]
fn different_sets_differ() {
    assert_ne!(fingerprint("o/r#3"), fingerprint("o/r#4"));
    assert_ne!(fingerprint("o/r#3"), fingerprint("o/r#3 o/r#4"));
}

#[test]
fn an_empty_set_is_stable() {
    // Hash of the empty string; the shell reaches the same place via an empty
    // `nodes` and the trailing-space strip.
    assert_eq!(fingerprint(""), fingerprint("   "));
    assert_eq!(fingerprint("").len(), 16);
}

#[test]
fn a_fact_fingerprint_is_prefixed_and_keyed_on_both_inputs() {
    let f = fact_fingerprint("escalation text", "abc1234");
    assert!(f.starts_with("fact-"), "got {f:?}");
    assert_eq!(f.len(), "fact-".len() + 16);

    assert_ne!(
        fact_fingerprint("escalation text", "abc1234"),
        fact_fingerprint("escalation text", "def5678"),
        "a different resolving commit must produce a different identifier"
    );
    assert_ne!(
        fact_fingerprint("a", "abc1234"),
        fact_fingerprint("b", "abc1234"),
        "different escalation text must produce a different identifier"
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

fn shell_fingerprint(nodes: &str) -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("loom-fp-diff-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("tmp dir");
    let nodes_file = dir.join("nodes.txt");
    std::fs::write(&nodes_file, nodes).expect("write nodes");

    let driver = format!(
        r#"set -uo pipefail
source "{script}" --help >/dev/null 2>&1 || true
nodes="$(cat "{nodes_file}")"
_fingerprint "$nodes"
"#,
        script = shell_script().display(),
        nodes_file = nodes_file.display(),
    );
    let driver_path = dir.join("driver.sh");
    std::fs::write(&driver_path, driver).expect("write driver");

    let out = std::process::Command::new("bash")
        .arg(&driver_path)
        .output()
        .expect("the shell implementation must be runnable");
    let _ = std::fs::remove_dir_all(&dir);
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn the_shell_fingerprint_is_reachable_and_uses_a_real_sha() {
    // Anti-vacuity guard. Also pins the environment assumption the port makes:
    // if this host fell through to `cksum`, the shell's own fingerprints would
    // be incompatible with every other host's, and comparing against them here
    // would be meaningless.
    let probe = shell_fingerprint("o/r#3");
    assert_eq!(probe.len(), 16, "shell produced {probe:?}");
    assert!(
        probe.chars().all(|c| c.is_ascii_hexdigit()),
        "shell produced {probe:?} — if this is not hex, _sha256 fell through to cksum \
         and this host's fingerprints never matched any other host's"
    );
}

#[test]
fn rust_and_shell_agree_on_fingerprints() {
    let corpus = [
        "o/r#3",
        "o/r#3 o/r#9",
        "o/r#9 o/r#3",
        "o/r#3 o/r#3 o/r#9",
        "o/r#3\no/r#9",
        "  o/r#3   o/r#9  ",
        "acme/widgets#42 other/repo#7 o/r#1",
        "",
        "   ",
    ];

    for nodes in corpus {
        let shell = shell_fingerprint(nodes);
        let rust = fingerprint(nodes);
        assert_eq!(
            rust, shell,
            "port diverges from the shell on {nodes:?}\n  rust:  {rust}\n  shell: {shell}"
        );
    }
}
