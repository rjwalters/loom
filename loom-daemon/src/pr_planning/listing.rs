//! Paginated conditional REST reads retain author association/type, unlike
//! the reduced issue listing. Cache keys include repo and credential scope.
use super::*;
use crate::forge_call_stats::{ops, ForgeOp};
use crate::forge_etag_store as store;
use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::path::Path;
use std::process::Command;

fn read(root: &Path, gh: &Path, op: ForgeOp, suffix: &str) -> Result<Value> {
    let repo = std::env::var("LOOM_REPO").ok();
    let target = store::resolve_target(Some(root), repo.as_deref());
    let url = format!("repos/{}/{}", target.repo.as_deref().unwrap_or("{owner}/{repo}"), suffix);
    let key = store::cache_key(Some(root), &target, &url);
    let path = store::disk_cache_path(&key);
    let sent = store::read_disk_entry(&path);
    let (status, response, stderr) = store::fetch_conditional(
        store::ConditionalRead::new("pr_planning", op),
        gh,
        Some(root),
        &target,
        &url,
        sent.as_ref().map(|e| e.etag.as_str()),
    )?;
    let body = match response {
        Some(r) if r.status == 304 => {
            sent.ok_or_else(|| anyhow!("PR queue cache missing for 304"))?
                .body
        }
        Some(r) if r.status == 200 && status.success() => {
            if let Some(etag) = r.etag {
                store::write_disk_entry(
                    &path,
                    &store::DiskEntry {
                        etag,
                        body: r.body.clone(),
                    },
                );
            }
            r.body
        }
        _ => return Err(anyhow!("PR queue read failed: {stderr}")),
    };
    serde_json::from_str(&body).context("parse PR queue response")
}

/// Every open PR, newest first: `GET pulls?state=open` (`pr.list-open`).
fn list_open(root: &Path, gh: &Path) -> Result<Vec<Value>> {
    let mut rows = Vec::new();
    for page in 1.. {
        let value = read(
            root,
            gh,
            ops::PR_LIST_OPEN,
            &format!("pulls?state=open&sort=created&direction=desc&per_page=100&page={page}"),
        )?;
        let batch = value.as_array().context("PR listing must be an array")?;
        rows.extend(batch.iter().cloned());
        if batch.len() < 100 {
            break;
        }
    }
    Ok(rows)
}

/// Whether `judge-fallback-guard.sh` admits this fallback row. The guard owns
/// bot admission, lifetime cap and head dedup; its failure is an error.
fn guard_admits(root: &Path, row: &Value) -> Result<bool> {
    let number = row["number"].as_u64().context("PR number missing")?;
    let output = Command::new(root.join(".loom/scripts/judge-fallback-guard.sh"))
        .current_dir(root)
        .arg(number.to_string())
        .output()
        .context("run Judge fallback guard")?;
    // Velocity is independent of admission, including an empty queue.
    // Keep diagnostics off stdout, which belongs to the queue JSON.
    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.lines().any(|line| line == "VELOCITY_ALERT=1") {
        let count = stdout
            .lines()
            .find_map(|line| line.strip_prefix("VELOCITY_COUNT="))
            .unwrap_or("unknown");
        eprintln!(
            "Judge fallback warning for PR #{number}: VELOCITY_ALERT=1 VELOCITY_COUNT={count}"
        );
    }
    match output.status.code() {
        Some(0) => Ok(true),
        Some(10..=12) => Ok(false),
        _ => Err(anyhow!(
            "Judge fallback guard failed for #{number}: {}",
            String::from_utf8_lossy(&output.stderr)
        )),
    }
}

/// Listing and fallback guard failures are errors, never an empty queue.
/// Fallback review only; no workflow labels or branch mutations occur here.
pub fn fetch_queue(root: &Path, gh: &Path, role: PrRole) -> Result<Vec<Value>> {
    let mut rows = list_open(root, gh)?;
    if role == PrRole::Doctor {
        for row in &mut rows {
            if has_label(row, "loom:pr")
                && !has_label(row, "loom:changes-requested")
                && !crate::work_finder::OPERATOR_HOLD_NAMES
                    .iter()
                    .chain(crate::work_finder::PR_LANE_PARK_LABELS.iter())
                    .chain(&["loom:treating"])
                    .any(|l| has_label(row, l))
            {
                let number = row["number"].as_u64().context("PR number missing")?;
                row["mergeable"] = read(root, gh, ops::PR_VIEW_STATE, &format!("pulls/{number}"))?
                    ["mergeable"]
                    .clone();
            }
        }
    }
    let trust = TrustPolicy::for_root(root);
    let mut queue = ordered_queue(rows, role, prefer_human_prs(root), &trust);
    if role == PrRole::Judge {
        let mut guarded = Vec::new();
        for row in queue {
            if row["mode"] == "fallback" {
                if guard_admits(root, &row)? {
                    guarded.push(row);
                }
            } else if !backstop_holds(root, gh, &row, &trust) {
                guarded.push(row);
            }
        }
        queue = guarded;
    }
    Ok(queue)
}

/// Every trusted comment on issue/PR `number`, oldest first, through the
/// same conditional pages as the listing (an unchanged page is a free `304`).
fn trusted_comments(
    root: &Path,
    gh: &Path,
    number: u64,
    trust: &TrustPolicy,
) -> Result<Vec<Value>> {
    let mut rows = Vec::new();
    for page in 1.. {
        let value = read(
            root,
            gh,
            ops::COMMENT_LIST,
            &format!("issues/{number}/comments?per_page=100&page={page}"),
        )?;
        let batch = value
            .as_array()
            .context("comment listing must be an array")?;
        rows.extend(batch.iter().cloned());
        if batch.len() < 100 {
            break;
        }
    }
    Ok(trust.filter(rows))
}

/// Whether the Judge no-verdict backstop holds this workflow row
/// ([`crate::judge_backstop`], #11855): its passes at this head and label set
/// already ended without a verdict often enough. Fails open — an unreadable
/// record list serves the PR — and says why on stderr (stdout is the queue).
fn backstop_holds(root: &Path, gh: &Path, row: &Value, trust: &TrustPolicy) -> bool {
    use crate::judge_backstop as backstop;
    let (Some(number), Some(head)) = (row["number"].as_u64(), row["head"]["sha"].as_str()) else {
        return false;
    };
    if !backstop::enabled() || !has_label(row, backstop::REVIEW_LABEL) {
        return false;
    }
    let labels: Vec<&str> = row["labels"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|l| l.as_str().or_else(|| l["name"].as_str()))
        .collect();
    let comments = match trusted_comments(root, gh, number, trust) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "Judge backstop: PR #{number} records unreadable ({e:#}); serving it (#11855)"
            );
            return false;
        }
    };
    match backstop::hold_from_comments(&comments, head, &labels, chrono::Utc::now()) {
        Some(until) => {
            eprintln!(
                "Judge backstop: PR #{number} held until {} — its Judge passes at head {} ended \
                 without a verdict (#11855); a push or label change releases it",
                until.to_rfc3339(),
                &head[..head.len().min(12)]
            );
            true
        }
        None => false,
    }
}

/// The Judge role-runner gate on a non-empty labeled listing (#11855): is
/// there a row `pr-queue --role judge` would actually serve — a labeled row
/// the no-verdict backstop does not hold, or a guard-admitted fallback row?
/// Stops at the first one, so a repository with servable work pays for one
/// row's record read.
pub fn has_judge_work(root: &Path, gh: &Path) -> Result<bool> {
    let (prefer, trust) = (prefer_human_prs(root), TrustPolicy::for_root(root));
    for row in ordered_queue(list_open(root, gh)?, PrRole::Judge, prefer, &trust) {
        let served = if row["mode"] == "fallback" {
            guard_admits(root, &row)?
        } else {
            !backstop_holds(root, gh, &row, &trust)
        };
        if served {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Judge's role-runner gate on an empty labeled listing (#9389): is there a
/// guard-admitted fallback PR that joins a busy queue — trusted interactive
/// (preference on) or [`workflow_shaped`] (always)? The predicate runs on the
/// one cached listing before the guard, so a repo with no such PR runs no
/// guard at all, and the first admitted row settles it.
pub fn has_judge_fallback(root: &Path, gh: &Path) -> Result<bool> {
    let (prefer, trust) = (prefer_human_prs(root), TrustPolicy::for_root(root));
    for row in ordered_queue(list_open(root, gh)?, PrRole::Judge, prefer, &trust) {
        if row["mode"] == "fallback"
            && joins_busy_queue(&row, prefer, &trust)
            && guard_admits(root, &row)?
        {
            return Ok(true);
        }
    }
    Ok(false)
}
