//! `loom-daemon restart --pause --then-exit` (#11812): host maintenance.
//!
//! Sends `Request::PauseThenExit`, prints the ack, waits for the daemon's H4
//! to write a `phase = paused` manifest and for the daemon to exit, then prints
//! what was paused and the deadline by which the host must be back for H5 to
//! resume it rather than requeue it.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{DateTime, Utc};

use loom_daemon::auto_update::pause_manifest::{
    self, Disposition, ItemStatus, LoadOutcome, PauseManifest, Phase,
};
use loom_daemon::types::{Request, Response};

use super::super::common::query_daemon;

/// How long past H4's own deadline the CLI waits for the manifest.
const WAIT_SLACK: Duration = Duration::from_secs(30);
/// How long the CLI waits for the process to go once the manifest is written.
const EXIT_WAIT: Duration = Duration::from_secs(30);

pub(crate) async fn handle(socket_path: &Path) -> Result<()> {
    let reply = match query_daemon(socket_path, &Request::PauseThenExit).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Could not reach loom-daemon at {}: {e}", socket_path.display());
            eprintln!("Nothing was paused. A daemon that is already down has nothing to pause.");
            std::process::exit(1);
        }
    };
    let Response::HostMaintenancePause {
        accepted,
        manifest_id,
        manifest_path,
        took_over,
        escalated,
        in_flight,
        pause_budget_secs,
        h4_deadline_secs,
        max_age_secs: _,
        resume_window_secs,
        message,
    } = reply
    else {
        if let Response::Error { message } = &reply {
            eprintln!("Daemon error: {message}");
        } else {
            eprintln!("Unexpected response from daemon: {reply:?}");
        }
        eprintln!(
            "The running daemon does not support --pause --then-exit (it predates #11812). \
             Nothing was changed: any drain in progress is untouched."
        );
        std::process::exit(1);
    };
    if !accepted {
        eprintln!("loom-daemon did NOT pause: {message}");
        std::process::exit(1);
    }
    println!(
        "loom-daemon host-maintenance pause started (trigger host-maintenance, manifest {}, {} \
         sweep(s) in flight{}{}).",
        manifest_id.as_deref().unwrap_or("pending"),
        in_flight,
        took_over
            .as_deref()
            .map(|t| format!(", took over {t}"))
            .unwrap_or_default(),
        if escalated {
            ", escalated the pause roll in progress"
        } else {
            ""
        }
    );
    println!("{message}.");
    let wait = Duration::from_secs(h4_deadline_secs) + WAIT_SLACK;
    println!(
        "Waiting up to {}s for H4 (pause budget {pause_budget_secs}s) to write the manifest and \
         exit...",
        wait.as_secs()
    );
    let Some(path) = manifest_path.map(PathBuf::from) else {
        eprintln!("The daemon named no manifest path; check `loom-daemon status`.");
        std::process::exit(1);
    };
    let Some(manifest) = wait_for_paused(&path, manifest_id.as_deref(), wait).await else {
        eprintln!(
            "H4 did not report a paused manifest at {} within {}s. Check `loom-daemon status`: a \
             pause that failed before stopping anything falls back to a then-exit drain (dispatch \
             stays PAUSED, the daemon stops once nothing is in flight); it never resumes \
             dispatch on its own.",
            path.display(),
            wait.as_secs()
        );
        std::process::exit(1);
    };
    let exited = wait_for_exit(socket_path, EXIT_WAIT).await;
    println!("{}", render_summary(&manifest, &path, resume_window_secs, exited));
    if exited {
        Ok(())
    } else {
        std::process::exit(1);
    }
}

/// Poll `path` until it holds a `phase = paused` manifest (with `id`, when
/// known) or `wait` passes.
async fn wait_for_paused(path: &Path, id: Option<&str>, wait: Duration) -> Option<PauseManifest> {
    let until = Instant::now() + wait;
    loop {
        if let LoadOutcome::Loaded(m) | LoadOutcome::Stale(m) =
            pause_manifest::load(path, Utc::now())
        {
            if m.phase == Phase::Paused && id.is_none_or(|id| id == m.manifest_id) {
                return Some(m);
            }
        }
        if Instant::now() >= until {
            return None;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Whether the daemon stopped answering its socket within `wait`.
async fn wait_for_exit(socket_path: &Path, wait: Duration) -> bool {
    let until = Instant::now() + wait;
    loop {
        if tokio::net::UnixStream::connect(socket_path).await.is_err() {
            return true;
        }
        if Instant::now() >= until {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// The operator-facing summary once H4 is done. Pure, for the tests.
pub(crate) fn render_summary(
    m: &PauseManifest,
    path: &Path,
    resume_window_secs: u64,
    exited: bool,
) -> String {
    let mut paused = 0usize;
    let mut exited_items = 0usize;
    let mut requeued: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for it in &m.items {
        if it.disposition == Disposition::Requeue {
            *requeued
                .entry(it.reason.clone().unwrap_or_else(|| "unknown".to_string()))
                .or_insert(0) += 1;
        } else if it.status == ItemStatus::Paused {
            paused += 1;
        } else {
            exited_items += 1;
        }
    }
    let requeue_total: usize = requeued.values().sum();
    let reasons = if requeued.is_empty() {
        String::new()
    } else {
        format!(
            " ({})",
            requeued
                .iter()
                .map(|(r, n)| format!("{r}: {n}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let max_age = i64::try_from(m.roll.max_age_secs).unwrap_or(i64::MAX);
    let window = i64::try_from(resume_window_secs).unwrap_or(0);
    let stale_at = m.roll.pause_started_at + chrono::Duration::seconds(max_age);
    let start_by = stale_at - chrono::Duration::seconds(window);
    let exit_line = if exited {
        "loom-daemon has exited and STAYS DOWN: the operator stop is on record, so neither the \
         watchdog nor the supervisor restarts it."
    } else {
        "WARNING: the manifest is written but the daemon is still answering its socket; check \
         `loom-daemon status` before changing the host."
    };
    format!(
        "H4 complete: manifest {id} ({path}), {n} item(s): {paused} paused for resume, \
         {requeue_total} requeued{reasons}, {exited_items} ended by themselves.\n\
         {exit_line}\n\
         RESUME DEADLINE: start the daemon again by {start_by} (the manifest goes stale at \
         {stale_at}: max age {max}s from the pause start {started}, minus {window}s for H5's \
         health probation and resume). Started later, H5 requeues every paused item instead (no \
         work lost: claims released, checkpoints kept).\n\
         Workspace: H5 resumes each agent from its saved session in the SAME workspace and \
         worktree on this host. An in-place stop/modify/start keeps the volume; a rebuild that \
         replaces the instance must reattach it, or the items are requeued.\n\
         Start with an explicit start (`loom-daemon-start.sh` / `loom-daemon daemon-start`), \
         which clears the operator-stop record. A bare supervisor start comes up HELD, and H5 \
         waits under the hold until the manifest is stale.",
        id = m.manifest_id,
        path = path.display(),
        n = m.items.len(),
        start_by = ts(start_by),
        stale_at = ts(stale_at),
        max = m.roll.max_age_secs,
        started = ts(m.roll.pause_started_at),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use loom_daemon::auto_update::pause_manifest::{ItemKind, ManifestItem, Roll, WrittenBy};

    fn item(
        id: &str,
        disposition: Disposition,
        status: ItemStatus,
        reason: Option<&str>,
    ) -> ManifestItem {
        let mut it: ManifestItem = serde_json::from_value(serde_json::json!({
            "id": id, "kind": "sweep", "repo": "/r", "disposition": "resume", "status": "paused",
        }))
        .unwrap();
        it.kind = ItemKind::Sweep;
        it.disposition = disposition;
        it.status = status;
        it.reason = reason.map(str::to_string);
        it
    }

    #[test]
    fn the_summary_names_the_items_the_deadline_and_the_start_rule() {
        let started = DateTime::parse_from_rfc3339("2026-10-13T23:58:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let m = PauseManifest {
            schema_version: pause_manifest::SCHEMA_VERSION,
            manifest_id: "rp-20261013T235800Z-ab12".to_string(),
            phase: Phase::Paused,
            written_by: WrittenBy::default(),
            roll: Roll {
                from_version: Some("0.19.1155".to_string()),
                to_version: "unknown".to_string(),
                to_artifact_sha256: None,
                target_source: Some(pause_manifest::TargetSource::HostMaintenance),
                staged_at: None,
                pause_started_at: started,
                pause_completed_at: None,
                pause_duration_ms: None,
                pause_budget_secs: Some(120),
                min_resumable_age_secs: Some(300),
                max_age_secs: 900,
            },
            items: vec![
                item("a", Disposition::Resume, ItemStatus::Paused, None),
                item("b", Disposition::Resume, ItemStatus::Paused, None),
                item("c", Disposition::Requeue, ItemStatus::Requeued, Some("young-agent-reset")),
            ],
            events: Vec::new(),
        };
        let out = render_summary(&m, Path::new("/s/roll-pause-manifest.json"), 210, true);
        assert!(out.contains("3 item(s): 2 paused for resume, 1 requeued (young-agent-reset: 1)"));
        assert!(out.contains("STAYS DOWN"));
        // 23:58:00 + 900 s = 00:13:00; minus 210 s = 00:09:30.
        assert!(out.contains("start the daemon again by 2026-10-14T00:09:30Z"), "{out}");
        assert!(out.contains("goes stale at 2026-10-14T00:13:00Z"));
        assert!(out.contains("explicit start"));
        assert!(render_summary(&m, Path::new("/s/m.json"), 210, false).contains("WARNING"));
    }
}
