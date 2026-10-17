//! `loom-daemon status`'s `CTR` column (issue #7430, epic #6896 Phase 3):
//! a client-side, render-time read of each sweep's own per-sweep log for
//! the `# LOOM_DISPATCH_MODE` marker `spawn-claude.sh` writes — see
//! `sweep_registry::containment_signal`. Exercised here against REAL
//! temp log files (not just the parser's own unit tests) to prove the
//! render path actually wires `log_path`/`sweep_id` through correctly.
use super::{format_containment_column, render_in_flight_table};
use crate::cli::status::sample_report::sample_report;
use chrono::Utc;
use loom_daemon::types::{SweepInfo, SweepKind, SweepState};
use std::path::PathBuf;

fn mk(sweep_id: &str, issue: u32, log_path: PathBuf) -> SweepInfo {
    SweepInfo {
        pgid: None,
        sweep_id: sweep_id.to_string(),
        kind: SweepKind::Issue(issue),
        pid: 4242,
        token_name: "agent-1.token".to_string(),
        runtime: "claude".to_string(),
        runtime_source: None,
        log_path,
        idempotency_key: None,
        started_at: Utc::now(),
        state: SweepState::Running,
        latest_phase: Some("builder".to_string()),
        pr_number: None,
        model: None,
        effort: None,
        depends_on: None,
        repo: Some("/repos/loom".to_string()),
        overflow: false,
    }
}

struct TempLog {
    dir: PathBuf,
}
impl TempLog {
    fn new(name: &str, contents: &str) -> (Self, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "loom-status-render-containment-test-{}-{}-{}",
            std::process::id(),
            name,
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let log_path = dir.join("sweep.log");
        std::fs::write(&log_path, contents).unwrap();
        (Self { dir }, log_path)
    }
}
impl Drop for TempLog {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn bare_metal_sweep_renders_dash() {
    let (_tmp, log_path) = TempLog::new(
        "bare-metal",
        "==== loom-daemon dispatch: sweep_id=s1 issue=1 ====\n\
         # LOOM_DISPATCH_MODE mode=bare-metal\n",
    );
    let sweep = mk("s1", 1, log_path);
    assert_eq!(format_containment_column(&sweep), "-");
}

#[test]
fn missing_log_renders_dash_not_an_error() {
    let sweep = mk("s2", 2, PathBuf::from("/nonexistent/loom/sweep.log"));
    assert_eq!(format_containment_column(&sweep), "-");
}

#[test]
fn containerized_sweep_with_both_limits_renders_both() {
    let (_tmp, log_path) = TempLog::new(
        "both-limits",
        "==== loom-daemon dispatch: sweep_id=s3 issue=3 ====\n\
         # LOOM_DISPATCH_MODE mode=container image=w cpus=4 memory=2048m\n",
    );
    let sweep = mk("s3", 3, log_path);
    assert_eq!(format_containment_column(&sweep), "container(cpu=4,mem=2048m)");
}

#[test]
fn containerized_sweep_with_unbounded_limits_renders_unbounded() {
    let (_tmp, log_path) = TempLog::new(
        "unbounded",
        "==== loom-daemon dispatch: sweep_id=s4 issue=4 ====\n\
         # LOOM_DISPATCH_MODE mode=container image=w cpus=none memory=none\n",
    );
    let sweep = mk("s4", 4, log_path);
    assert_eq!(format_containment_column(&sweep), "container(unbounded)");
}

#[test]
fn table_header_advertises_ctr_column() {
    let mut report = sample_report();
    let (_tmp, log_path) = TempLog::new(
        "table-header",
        "==== loom-daemon dispatch: sweep_id=s5 issue=5 ====\n\
         # LOOM_DISPATCH_MODE mode=container image=w cpus=2 memory=1024m\n",
    );
    report.in_flight = vec![mk("s5", 5, log_path)];
    let table = render_in_flight_table(&report);
    assert!(
        table.lines().next().unwrap().contains("CTR"),
        "expected CTR in the table header, got: {table}"
    );
    assert!(
        table.contains("container(cpu=2,mem=1024m)"),
        "expected the containerized row to show its limits, got: {table}"
    );
}
