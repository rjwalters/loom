//! The pinned ClickHouse image and the `clickhouse local` runner shared by the
//! SigNoz replay tests (#13781).
//!
//! The pin must equal the `clickhouse/clickhouse-server` line of
//! `.github/ci-images.txt` (asserted by `hub_image_refs.rs`), so a digest bump
//! is one edit here plus that list.
//!
//! Included per test binary with `#[path = "common/clickhouse_local.rs"]`
//! beside `hub_image`, not through `common/mod.rs`, which pulls in the daemon
//! harness these proofs do not use. Requires `mod hub_image;` at the same level.

// Each test binary uses its own subset of this module.
#![allow(dead_code)]

use std::io::Write;
use std::process::{Command, Stdio};

pub const CLICKHOUSE_IMAGE: &str = "clickhouse/clickhouse-server:25.12.5@sha256:cacf32d6884291dc2ff5e0156a97f46fc53ff7c929a7906d114e268a929dfd3a";

/// Runs `script` through `clickhouse local --multiquery` in the pinned image,
/// with `args` appended (`--format=...`, `--param_*=...`), and returns stdout.
/// Panics with the engine's own stderr on failure: a query that does not parse
/// must fail the test, not be silently skipped.
pub fn clickhouse_with(script: &str, args: &[String]) -> String {
    let mut child = Command::new("docker")
        .args([
            "run",
            "--rm",
            "-i",
            "--entrypoint",
            "clickhouse",
            &crate::hub_image::resolve(CLICKHOUSE_IMAGE),
            "local",
            "--multiquery",
        ])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("docker is required for this test");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "clickhouse rejected the script:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// [`clickhouse_with`] with only `--format=<format>`.
pub fn clickhouse(script: &str, format: &str) -> String {
    clickhouse_with(script, &[format!("--format={format}")])
}

/// [`clickhouse`] with `JSONEachRow`.
pub fn clickhouse_json(script: &str) -> String {
    clickhouse(script, "JSONEachRow")
}
