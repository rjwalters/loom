//! End-to-end contract for `loom-daemon footprint` (#9784), driven through
//! the real binary over a #9783 cache built with the fake adapter: build →
//! show → pair overlap, with the classification boundaries the issue pins.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::PathBuf;
use std::process::Command;

fn daemon() -> Command {
    Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
}

const REV: &str = "0123456789abcdef0123456789abcdef01234567";

#[test]
fn footprint_build_show_overlap_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");

    // Two issues in the same area: A intends the renderer; B only READS the
    // renderer for its own work (context-only) but intends the same helper.
    let fetch = |title: &str, body: &str, repo: &str| {
        let body_path = dir
            .path()
            .join(format!("body-{}.md", repo.replace('/', "-")));
        std::fs::write(&body_path, body).unwrap();
        let out = daemon()
            .args(["context", "fetch", "--title", title])
            .arg("--body-file")
            .arg(&body_path)
            .args([
                "--repo",
                "o/r",
                "--source-rev",
                REV,
                "--index-id",
                "i",
                "--query-policy",
                "q",
                "--adapter",
                "fake",
                "--store",
            ])
            .arg(&store)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .nth(1)
            .unwrap()
            .to_string()
    };
    let _key_a = fetch("Renderer work", "fix the widget renderer", "a");
    let key_b = fetch("Helper extraction", "extract shared renderer helper", "b");

    // Build footprints. The unseeded fake yields empty retrieval, so every
    // coverage count is zero but the pipeline (build→persist→show→overlap)
    // must hold — the classifier boundaries are pinned by the unit suites.
    let build = |key: &str, curator: &str| {
        daemon()
            .args(["footprint", "build", "--key", key])
            .arg("--store")
            .arg(&store)
            .args(["--curator-files", curator, "--classifier-version", "v1"])
            .output()
            .unwrap()
    };
    let out = build(&_key_a, "src/renderer.rs,src/widget.rs");
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let out = build(&key_b, "src/helper.rs");
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));

    // show returns the persisted artifact with provenance + classifier.
    let out = daemon()
        .args(["footprint", "show", "--key", &_key_a])
        .arg("--store")
        .arg(&store)
        .args(["--classifier-version", "v1"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let f: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(f["context_key"], serde_json::json!(_key_a));
    assert_eq!(f["source_revision"], serde_json::json!(REV));
    assert_eq!(f["classifier"]["version"], "v1");
    assert_eq!(
        f["curator_affected_files"],
        serde_json::json!(["src/renderer.rs", "src/widget.rs"])
    );
    assert!(f["coverage"]["notes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n.as_str().unwrap().contains("contract")));

    // overlap: two empty-retrieval footprints have no overlap, and the verb
    // refuses malformed key pairs.
    let out = daemon()
        .args([
            "footprint",
            "overlap",
            "--keys",
            &format!("{_key_a},{key_b}"),
        ])
        .arg("--store")
        .arg(&store)
        .args(["--classifier-version", "v1"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let ov: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(ov["intended_edit_overlap"], serde_json::json!([]));
    assert_eq!(ov["context_key_a"], serde_json::json!(_key_a));

    let out = daemon()
        .args(["footprint", "overlap", "--keys", "solo"])
        .arg("--store")
        .arg(&store)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));

    // Classifier bump: a different version has no persisted footprint yet
    // (derived state is per-classifier; retrieval stays valid in the cache).
    let out = daemon()
        .args(["footprint", "show", "--key", &_key_a])
        .arg("--store")
        .arg(&store)
        .args(["--classifier-version", "v2"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));

    let _: PathBuf = dir.path().to_path_buf();
}
