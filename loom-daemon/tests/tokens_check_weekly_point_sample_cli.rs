//! End-to-end CLI coverage for the weekly-limit-point sampling wired into
//! `loom-daemon tokens check` (issue #8347, part of #8063).
//!
//! Uses `--source monitor` with `LOOM_CLAUDE_MONITOR_DIR` pointed at a seeded
//! `ranking.json` so a report carrying real `7d_utilization` values is
//! produced without a probe — deterministic and network-free (the probe
//! transport itself is already covered by `tokens_pool::check`'s unit tests).
//! `LOOM_ACTIVITY_DB` redirects the sample write to a temp database so no
//! real `~/.loom/activity.db` is touched.

use std::path::Path;
use std::process::Command;

fn seed_pool(workspace: &Path, accounts: &[(&str, &str)]) {
    let dir = workspace.join(".loom").join("tokens");
    std::fs::create_dir_all(&dir).unwrap();
    let rows: Vec<serde_json::Value> = accounts
        .iter()
        .enumerate()
        .map(|(i, (name, email))| {
            std::fs::write(dir.join(format!("{name}.token")), "sk-ant-oat01-fake").unwrap();
            serde_json::json!({
                "env_index": i + 1,
                "name": name,
                "email": email,
                "file": format!("{name}.token"),
                "source": "repo",
                "provider": "claude",
            })
        })
        .collect();
    std::fs::write(
        dir.join("index.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "version": 3,
            "generated_at": "2026-01-01T00:00:00Z",
            "accounts": rows,
        }))
        .unwrap(),
    )
    .unwrap();
}

/// Write a `ranking.json` whose `generated_at` is now, so it passes the
/// monitor backend's 10-minute freshness window.
fn seed_monitor_ranking(monitor_dir: &Path, rows: &[(&str, f64)]) {
    std::fs::create_dir_all(monitor_dir).unwrap();
    let accounts: Vec<serde_json::Value> = rows
        .iter()
        .map(|(email, util_7d)| {
            serde_json::json!({
                "email": email,
                "status": "available",
                "utilization": { "7d": util_7d, "5h": 0.1 },
                "resets": { "7d": "2099-01-01T00:00:00Z", "5h": "2099-01-01T00:00:00Z" },
            })
        })
        .collect();
    std::fs::write(
        monitor_dir.join("ranking.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "schema": 1,
            "generated_at": chrono::Utc::now().to_rfc3339(),
            "accounts": accounts,
        }))
        .unwrap(),
    )
    .unwrap();
}

fn read_sample(db_path: &Path) -> Option<(String, f64, i64)> {
    if !db_path.is_file() {
        return None;
    }
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.query_row("SELECT day, points, account_count FROM weekly_point_samples", [], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
    })
    .ok()
}

fn run_check(workspace: &Path, monitor_dir: &Path, db_path: &Path, extra: &[&str]) -> bool {
    let mut args = vec!["tokens", "check", "--source", "monitor", "--workspace"];
    args.push(workspace.to_str().unwrap());
    args.extend_from_slice(extra);

    let out = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args(&args)
        .env("LOOM_CLAUDE_MONITOR_DIR", monitor_dir)
        .env("LOOM_ACTIVITY_DB", db_path)
        .env("LOOM_SHARED_TOKENS_DIR", "")
        .output()
        .unwrap();
    out.status.success()
}

/// `tokens check --ranking` — the exact invocation the daemon's ~10-minute
/// `token_ranking_refresh` tick makes — persists today's sample: the summed
/// 7d utilization across the pool, in percentage points.
#[test]
fn check_with_ranking_records_todays_weekly_point_sample() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("repo");
    seed_pool(&ws, &[("acct-a", "a@example.com"), ("acct-b", "b@example.com")]);

    let monitor_dir = tmp.path().join("claude-monitor");
    seed_monitor_ranking(&monitor_dir, &[("a@example.com", 0.5), ("b@example.com", 0.25)]);

    let db_path = tmp.path().join("activity.db");
    assert!(run_check(&ws, &monitor_dir, &db_path, &["--ranking"]));

    let (day, points, account_count) =
        read_sample(&db_path).expect("a row for today's UTC date must exist");
    assert_eq!(day, chrono::Utc::now().date_naive().to_string());
    assert!((points - 75.0).abs() < 1e-6, "0.5 + 0.25 windows == 75 points, got {points}");
    assert_eq!(account_count, 2);
}

/// A second run the same day keeps the maximum, not the latest reading — and
/// a lower reading (a weekly window that reset mid-day) cannot pull the day's
/// high-water mark back down.
#[test]
fn a_second_run_the_same_day_keeps_the_days_maximum() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("repo");
    seed_pool(&ws, &[("acct-a", "a@example.com")]);

    let monitor_dir = tmp.path().join("claude-monitor");
    let db_path = tmp.path().join("activity.db");

    seed_monitor_ranking(&monitor_dir, &[("a@example.com", 0.9)]);
    assert!(run_check(&ws, &monitor_dir, &db_path, &["--ranking"]));

    seed_monitor_ranking(&monitor_dir, &[("a@example.com", 0.05)]);
    assert!(run_check(&ws, &monitor_dir, &db_path, &["--ranking"]));

    let (_, points, _) = read_sample(&db_path).expect("row");
    assert!((points - 90.0).abs() < 1e-6, "expected the day's max (90), got {points}");
}

/// A bare `tokens check` (no `--ranking`) is a read-only diagnostic: it
/// reports the pool but never persists a sample, matching `run_check`'s own
/// rule that only authoritative invocations mutate pool state.
#[test]
fn check_without_ranking_persists_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("repo");
    seed_pool(&ws, &[("acct-a", "a@example.com")]);

    let monitor_dir = tmp.path().join("claude-monitor");
    seed_monitor_ranking(&monitor_dir, &[("a@example.com", 0.5)]);

    let db_path = tmp.path().join("activity.db");
    assert!(run_check(&ws, &monitor_dir, &db_path, &[]));
    assert!(read_sample(&db_path).is_none(), "a read-only check must not write a sample");
}

/// An unwritable activity database is best-effort: `tokens check --ranking`
/// still succeeds and still writes `.ranking`.
#[test]
fn an_unwritable_activity_db_does_not_break_the_check() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("repo");
    seed_pool(&ws, &[("acct-a", "a@example.com")]);

    let monitor_dir = tmp.path().join("claude-monitor");
    seed_monitor_ranking(&monitor_dir, &[("a@example.com", 0.5)]);

    // Parent directory does not exist -> the database cannot be opened.
    let db_path = tmp.path().join("no-such-dir").join("activity.db");
    assert!(
        run_check(&ws, &monitor_dir, &db_path, &["--ranking"]),
        "a DB failure must not change tokens check's exit code"
    );
    assert!(!db_path.exists());
    assert!(
        ws.join(".loom").join("tokens").join(".ranking").is_file(),
        "the .ranking write must be unaffected by the sampling failure"
    );
}
