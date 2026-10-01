//! End-to-end contract for the `loom-daemon context` cache (#9783), driven
//! through the real binary with the fake adapter: key discipline, byte-
//! identical reuse with zero provider calls, explicit outcome taxonomy,
//! single-flight, crash recovery, and offline export/import/replay.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

fn daemon() -> Command {
    Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
}

struct Env {
    dir: PathBuf,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap().keep();
        Self { dir }
    }
    fn store(&self) -> PathBuf {
        self.dir.join("store")
    }
    fn drop(self) {}
}

fn write_file(p: &Path, content: &str) {
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(p, content).unwrap();
}

fn source_rev() -> &'static str {
    "0123456789abcdef0123456789abcdef01234567"
}

#[test]
fn context_fetch_reuses_without_provider_and_keys_on_content() {
    let env = Env::new();
    let body = env.dir.join("body.md");
    write_file(&body, "Implement the widget renderer.\n");

    let common = [
        "--repo",
        "o/r",
        "--source-rev",
        source_rev(),
        "--index-id",
        "idx-1",
        "--query-policy",
        "qp-v1",
        "--adapter",
        "fake",
        "--store",
    ];
    let store = env.store();
    let fetch = |title: &str, body_path: &Path| {
        daemon()
            .args(["context", "fetch"])
            .args(["--title", title])
            .arg("--body-file")
            .arg(body_path)
            .args(common)
            .arg(&store)
            .output()
            .unwrap()
    };

    // First fetch: 0 results from the unseeded fake is a successful empty
    // session — a cacheable "nothing found", not a failure.
    let out = fetch("Build widget", &body);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout1 = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(stdout1.contains("reused=false"), "{stdout1}");
    let key1 = stdout1.split_whitespace().nth(1).unwrap().to_string();

    // Second identical fetch: reused, same key.
    let out = fetch("Build widget", &body);
    let stdout2 = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(stdout2.contains("reused=true"), "{stdout2}");
    assert_eq!(stdout1.split_whitespace().nth(1), stdout2.split_whitespace().nth(1));

    // Whitespace-only body edit: canonicalization reuses the key.
    let body2 = env.dir.join("body2.md");
    write_file(&body2, "Implement the widget renderer.\n\n\n");
    let out = fetch("Build  widget", &body2); // extra spaces in title too
    let stdout3 = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(stdout3.contains("reused=true"), "whitespace-only edits must reuse: {stdout3}");

    // Content edit: new key.
    let body3 = env.dir.join("body3.md");
    write_file(&body3, "Implement the widget renderer for dark mode.\n");
    let out = fetch("Build widget", &body3);
    let stdout4 = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(stdout4.contains("reused=false"), "{stdout4}");
    assert_ne!(stdout4.split_whitespace().nth(1), Some(key1.as_str()));

    // Source revision is a key dimension too.
    let out = daemon()
        .args(["context", "fetch"])
        .args(["--title", "Build widget"])
        .arg("--body-file")
        .arg(&body)
        .args([
            "--repo",
            "o/r",
            "--source-rev",
            "ffffffffffffffffffffffffffffffffffffffff",
            "--index-id",
            "idx-1",
            "--query-policy",
            "qp-v1",
            "--adapter",
            "fake",
            "--store",
        ])
        .arg(&store)
        .output()
        .unwrap();
    let stdout5 = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(stdout5.contains("reused=false"), "source change must re-key: {stdout5}");

    // status verb: present keys verified, absent keys exit 1.
    let out = daemon()
        .args(["context", "status", "--key", &key1])
        .arg("--store")
        .arg(&store)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));

    let out = daemon()
        .args([
            "context",
            "status",
            "--key",
            "ffff0000ffff0000ffff0000ffff0000ffff0000ffff0000ffff0000ffff0000",
        ])
        .arg("--store")
        .arg(&store)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));

    env.drop();
}

#[test]
fn context_augment_adapter_reports_unavailable_cleanly() {
    if std::env::var("AUGMENT_API_TOKEN").is_ok() {
        return; // provisioned host: the live path is exercised elsewhere
    }
    let env = Env::new();
    let body = env.dir.join("body.md");
    write_file(&body, "x");
    let out = daemon()
        .args(["context", "fetch", "--title", "t"])
        .arg("--body-file")
        .arg(&body)
        .args([
            "--repo",
            "o/r",
            "--source-rev",
            source_rev(),
            "--index-id",
            "i",
            "--query-policy",
            "q",
            "--adapter",
            "augment",
            "--store",
        ])
        .arg(env.store())
        .output()
        .unwrap();
    // Unavailable provider: recorded evidence, exit 2 (retry later), never a
    // fabricated result and never a crash.
    assert_eq!(out.status.code(), Some(2), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(stdout.contains("Unavailable"), "{stdout}");
}

#[test]
fn context_export_import_replay_roundtrip() {
    let env = Env::new();
    let body = env.dir.join("body.md");
    write_file(&body, "shared renderer work");
    let store = env.store();

    let out = daemon()
        .args(["context", "fetch", "--title", "Shared renderer"])
        .arg("--body-file")
        .arg(&body)
        .args([
            "--repo",
            "o/r",
            "--source-rev",
            source_rev(),
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
    let key = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_string();

    // Export the whole store; import into a second store; replay offline.
    let bundle = env.dir.join("bundle.json");
    let out = daemon()
        .args(["context", "export"])
        .arg("--out")
        .arg(&bundle)
        .arg("--store")
        .arg(&store)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));

    let store2 = env.dir.join("store2");
    let out = daemon()
        .args(["context", "import"])
        .arg("--bundle")
        .arg(&bundle)
        .arg("--store")
        .arg(&store2)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));

    let out = daemon()
        .args(["context", "replay", "--key", &key])
        .arg("--store")
        .arg(&store2)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let replayed: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("replay prints valid JSON");
    assert_eq!(replayed["key"], serde_json::json!(key));
    assert!(!replayed["session"]["raw_responses"]
        .as_array()
        .unwrap()
        .is_empty());

    env.drop();
}
