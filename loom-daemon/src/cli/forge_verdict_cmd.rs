//! `loom-daemon forge verdict-gate | verdict-labels` (#10581): the I/O half of
//! [`loom_daemon::verdict_gate`], called by `post-verdict.sh` before and after
//! it posts a verdict comment. Reads and writes go through REST (`gh api`),
//! never GraphQL, so they survive an exhausted GraphQL pool.

use std::path::Path;

use anyhow::Result;
use loom_daemon::claim_reconciliation::VerdictKind;
use loom_daemon::verdict_gate::{self as gate, Decision, GateInput};
use serde_json::Value;

fn verdict_or_exit(cmd: &str, verdict: &str) -> VerdictKind {
    gate::parse_verdict(verdict).unwrap_or_else(|| {
        eprintln!("forge {cmd}: --verdict must be approved or changes-requested, got {verdict:?}");
        std::process::exit(2)
    })
}

/// The PR's label names, live (uncached). `None` when the read failed.
fn read_labels(repo: &str, pr: u64, root: &Path) -> Option<Vec<String>> {
    let path = format!("repos/{repo}/issues/{pr}/labels?per_page=100");
    let out = loom_daemon::script_helpers::run_gh(&["api", &path], root, false);
    let doc = serde_json::from_slice::<Value>(&out.ok_output()?.stdout).ok()?;
    gate::label_names(&doc)
}

/// `forge verdict-gate`.
pub(crate) fn verdict_gate(
    pr: u64,
    repo: &str,
    verdict: &str,
    sha: &str,
    overrule: &str,
    window_secs: i64,
) -> Result<()> {
    let kind = verdict_or_exit("verdict-gate", verdict);
    let root = super::forge_identity_cmd::workspace();
    let comments = loom_daemon::comment_trust::records::fetch_trusted_comments(
        repo,
        &pr.to_string(),
        &root,
        false,
    );
    let labels = read_labels(repo, pr, &root);
    let decision = gate::decide(&GateInput {
        verdict: kind,
        sha,
        comments: comments.as_deref(),
        labels: labels.as_deref(),
        overrule,
        now: chrono::Utc::now(),
        window_secs,
    });
    if matches!(decision, Decision::Proceed(_)) && (comments.is_none() || labels.is_none()) {
        eprintln!(
            "forge verdict-gate: WARNING — PR #{pr}'s comments or labels could not be read; \
             a changes-requested verdict posts anyway (it cannot merge anything)"
        );
    }
    let (line, code) = decision.render();
    // What this caller's gate saw, for `verdict-reconcile` to compare against.
    let seen = comments
        .as_deref()
        .map_or(0, |c| gate::count_markers(c, sha, opposite_of(kind)));
    let seen_same = comments
        .as_deref()
        .map_or(0, |c| gate::max_marker_id(c, sha, kind));
    println!("{line} seen-opposite={seen} seen-same-max-id={seen_same}");
    std::process::exit(code)
}

fn opposite_of(kind: VerdictKind) -> VerdictKind {
    match kind {
        VerdictKind::Approved => VerdictKind::ChangesRequested,
        VerdictKind::ChangesRequested => VerdictKind::Approved,
    }
}

/// `forge verdict-reconcile` (read-only): after the post and label transition,
/// re-read the PR and say whether a rival verdict landed at the same head
/// after this caller's gate read (a cross-host race the host lock cannot see).
pub(crate) fn verdict_reconcile(
    pr: u64,
    repo: &str,
    verdict: &str,
    sha: &str,
    seen_opposite: usize,
    seen_same_max_id: u64,
    nonce: &str,
) -> Result<()> {
    let kind = verdict_or_exit("verdict-reconcile", verdict);
    let root = super::forge_identity_cmd::workspace();
    let comments = loom_daemon::comment_trust::records::fetch_trusted_comments(
        repo,
        &pr.to_string(),
        &root,
        false,
    );
    let outcome =
        gate::reconcile(kind, sha, seen_opposite, seen_same_max_id, nonce, comments.as_deref());
    // The loser of an identical-verdict race withdraws its own comment, so
    // exactly one stands. A failed delete is an unconfirmed state, not a pass.
    if let gate::Reconciled::Duplicate(id) = &outcome {
        let path = format!("repos/{repo}/issues/comments/{id}");
        let deleted =
            loom_daemon::script_helpers::run_gh(&["api", "-X", "DELETE", &path], &root, false)
                .ok_output()
                .is_some();
        if !deleted {
            println!(
                "{} UNREAD duplicate comment {id} could not be withdrawn",
                gate::RECONCILE_SENTINEL
            );
            std::process::exit(1)
        }
    }
    let (line, code) = outcome.render();
    println!("{line}");
    std::process::exit(code)
}

/// `forge verdict-labels`.
pub(crate) fn verdict_labels(pr: u64, repo: &str, verdict: &str) -> Result<()> {
    let kind = verdict_or_exit("verdict-labels", verdict);
    let root = super::forge_identity_cmd::workspace();
    let (add, remove) = gate::transition(kind);
    let base = format!("repos/{repo}/issues/{pr}/labels");
    // Add first: a failure after this point leaves the verdict label on,
    // never a verdict with no label (#10605).
    let mut args: Vec<String> = vec!["api".into(), "-X".into(), "POST".into(), base.clone()];
    for l in add {
        args.push("-f".into());
        args.push(format!("labels[]={l}"));
    }
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    // Never delete the source/claim labels unless the add landed: a failed
    // POST followed by the DELETEs would drop the PR out of its queue with
    // no verdict label to replace it.
    if loom_daemon::script_helpers::run_gh(&argv, &root, false)
        .ok_output()
        .is_none()
    {
        eprintln!(
            "forge verdict-labels: PR #{pr}'s {} label could not be added; the existing queue/claim \
             labels were left in place.\n  Repair: {}",
            kind.marker_token(),
            gate::repair_command(pr, repo, kind)
        );
        std::process::exit(1)
    }
    // A DELETE of a label the PR does not carry is a 404; the re-read below
    // is what decides success, so the individual results are not consulted.
    for l in remove {
        let path = format!("{base}/{}", l.replace(':', "%3A"));
        let _ = loom_daemon::script_helpers::run_gh(&["api", "-X", "DELETE", &path], &root, false);
    }
    let problems = match read_labels(repo, pr, &root) {
        Some(labels) => gate::label_problems(kind, &labels),
        None => vec!["the labels could not be re-read to verify the transition".to_string()],
    };
    if problems.is_empty() {
        println!("{} OK {}", gate::LABELS_SENTINEL, add.join(","));
        return Ok(());
    }
    eprintln!(
        "forge verdict-labels: PR #{pr}'s {} label transition did not hold: {}.\n  Repair: {}",
        kind.marker_token(),
        problems.join("; "),
        gate::repair_command(pr, repo, kind)
    );
    std::process::exit(1)
}

/// The lock directory for one PR (`owner/name` flattened, so it is one path
/// component).
fn lock_path(pr: u64, repo: &str) -> std::path::PathBuf {
    let base = std::env::var_os("LOOM_VERDICT_LOCK_DIR").map_or_else(
        || {
            Path::new(&std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into()))
                .join(".loom/locks/post-verdict")
        },
        Into::into,
    );
    base.join(format!("{}-{pr}", repo.replace('/', "_")))
}

/// Take the lock: `mkdir` is the atomic step. A holder that never released
/// (crashed) is reaped once its directory is older than [`LOCK_STALE`].
fn acquire_lock(dir: &Path, wait: std::time::Duration) -> bool {
    const LOCK_STALE: std::time::Duration = std::time::Duration::from_secs(300);
    if let Some(parent) = dir.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let deadline = std::time::Instant::now() + wait;
    loop {
        if std::fs::create_dir(dir).is_ok() {
            return true;
        }
        let stale = std::fs::metadata(dir)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > LOCK_STALE);
        if stale {
            let _ = std::fs::remove_dir(dir);
            continue;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

/// `forge verdict-lock`.
pub(crate) fn verdict_lock(action: &str, pr: u64, repo: &str) -> Result<()> {
    let dir = lock_path(pr, repo);
    match action {
        "acquire" => {
            let wait = std::env::var("LOOM_VERDICT_LOCK_WAIT_SECS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(90);
            if acquire_lock(&dir, std::time::Duration::from_secs(wait)) {
                return Ok(());
            }
            eprintln!(
                "forge verdict-lock: could not take {} within {wait}s; another verdict on PR #{pr} is in flight",
                dir.display()
            );
            std::process::exit(9)
        }
        "release" => {
            let _ = std::fs::remove_dir(&dir);
            Ok(())
        }
        other => {
            eprintln!("forge verdict-lock: action must be acquire or release, got {other:?}");
            std::process::exit(2)
        }
    }
}

#[cfg(test)]
mod lock_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_held_lock_is_refused_until_released() {
        let dir = std::env::temp_dir().join(format!("loom-vlock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let lock = dir.join("owner_repo-1");
        assert!(acquire_lock(&lock, Duration::from_millis(0)));
        assert!(
            !acquire_lock(&lock, Duration::from_millis(300)),
            "held: second caller times out"
        );
        std::fs::remove_dir(&lock).unwrap();
        assert!(acquire_lock(&lock, Duration::from_millis(0)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
