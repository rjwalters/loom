//! Paginated conditional REST reads retain author association/type, unlike
//! the reduced issue listing. Cache keys include repo and credential scope.
use super::*;
use crate::forge_call_stats::{ops, ForgeOp};
use crate::forge_etag_store as store;
use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::collections::HashSet;
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

/// Every page of the conditional listing at `suffix` (`&page=N` appended),
/// attributed to `op` in the forge call stats.
fn read_all(root: &Path, gh: &Path, op: ForgeOp, suffix: &str) -> Result<Vec<Value>> {
    let mut rows = Vec::new();
    for page in 1.. {
        let value = read(root, gh, op, &format!("{suffix}&page={page}"))?;
        let batch = value.as_array().context("forge listing must be an array")?;
        rows.extend(batch.iter().cloned());
        if batch.len() < 100 {
            break;
        }
    }
    Ok(rows)
}

/// Listing and fallback guard failures are errors, never an empty queue.
/// Fallback guard owns bot admission, lifetime cap and head dedup. It grants
/// review only; no workflow labels or branch mutations occur here.
pub fn fetch_queue(root: &Path, gh: &Path, role: PrRole) -> Result<Vec<Value>> {
    fetch(root, gh, role, true)
}

/// [`fetch_queue`], with star-time stamping optional: a caller that only asks
/// *whether* a row exists ([`has_interactive_fallback`]) never needs the order.
fn fetch(root: &Path, gh: &Path, role: PrRole, stamp_stars: bool) -> Result<Vec<Value>> {
    // `GET pulls?state=open`: the open-PR queue. The inventory's PR
    // discovery row is by-head only (`pr.list-by-head`), so the full
    // queue listing has no row yet (#9831).
    let queue_op = ForgeOp::uninventoried("open-PR queue listing has no inventory row");
    let mut rows =
        read_all(root, gh, queue_op, "pulls?state=open&sort=created&direction=desc&per_page=100")?;
    if role == PrRole::Doctor {
        for row in &mut rows {
            if has_label(row, "loom:pr")
                && !has_label(row, "loom:changes-requested")
                && ![
                    "loom:operator",
                    "loom:operator-only",
                    "loom:blocked",
                    "loom:treating",
                ]
                .iter()
                .any(|l| has_label(row, l))
            {
                let number = row["number"].as_u64().context("PR number missing")?;
                row["mergeable"] = read(root, gh, ops::PR_VIEW_STATE, &format!("pulls/{number}"))?
                    ["mergeable"]
                    .clone();
            }
        }
    }
    let (prefer, trust) = (prefer_human_prs(root), TrustPolicy::for_root(root));
    let mut admitted = admit(rows, role, prefer, &trust);
    if stamp_stars {
        stamp_star_times(root, gh, &mut admitted);
    }
    let mut queue = order(admitted, role, prefer, &trust);
    if role == PrRole::Judge {
        let mut guarded = Vec::new();
        for row in queue {
            if row["mode"] == "fallback" {
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
                    eprintln!("Judge fallback warning for PR #{number}: VELOCITY_ALERT=1 VELOCITY_COUNT={count}");
                }
                match output.status.code() {
                    Some(0) => {}
                    Some(10..=12) => continue,
                    _ => {
                        return Err(anyhow!(
                            "Judge fallback guard failed for #{number}: {}",
                            String::from_utf8_lossy(&output.stderr)
                        ))
                    }
                }
            }
            guarded.push(row);
        }
        queue = guarded;
    }
    Ok(queue)
}

/// Stamp each starred row with its effective star time (#9974) so
/// [`order`] takes the earliest star first, a PR inheriting its linked
/// issue's earlier star. `rows` are the role's ADMITTED rows, and nothing is
/// read unless two or more of them are starred: only then can star time
/// change the order (#9975 review). The starred listing is a conditional
/// read (a `304` costs no quota) and timelines go through the
/// `updated_at`-validated [`crate::forge_starred::StarTimeCache`], so a
/// repeat call with no new star activity makes no timeline read. A failed
/// read degrades to the `created_at` fallback with a warning: star time is
/// an ordering refinement among stars, never an admission decision.
fn stamp_star_times(root: &Path, gh: &Path, rows: &mut [Value]) {
    use crate::forge_starred::{self as fs, Kind, Listed};
    let wanted: HashSet<u64> = rows
        .iter()
        .filter(|r| has_label(r, "loom:operator-priority"))
        .filter_map(|r| r["number"].as_u64())
        .collect();
    if wanted.len() < 2 {
        return;
    }
    let listing = match read_all(
        root,
        gh,
        ops::ISSUE_LIST,
        "issues?labels=loom:operator-priority&state=open&per_page=100",
    ) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("pr-queue: star-time read failed ({e:#}); ordering stars by created_at");
            return;
        }
    };
    let starred: HashSet<u32> = listing
        .iter()
        .filter_map(|v| v["number"].as_u64().and_then(|n| u32::try_from(n).ok()))
        .collect();
    // Admitted starred PRs (from the pulls rows) plus every starred issue
    // (inheritance candidates); `resolve` reads only what ordering needs.
    let mut listed: Vec<Listed> = rows
        .iter()
        .filter(|r| r["number"].as_u64().is_some_and(|n| wanted.contains(&n)))
        .filter_map(|r| Listed::from_rest(r, true))
        .collect();
    listed.extend(
        listing
            .iter()
            .filter(|v| v["pull_request"].is_null())
            .filter_map(|v| Listed::from_rest(v, false)),
    );
    let env_repo = std::env::var("LOOM_REPO").ok();
    let target = store::resolve_target(Some(root), env_repo.as_deref());
    let mut times = fs::StarTimes::for_target(root, gh, &target);
    let ordered = fs::resolve(listed, Kind::Pr, None, &[], false, &mut |n, u| times.get(n, u));
    times.finish(&starred);
    stamp(rows, &ordered);
}

/// Copy each starred PR's effective star time from `starred` onto its row.
/// Pure; rows absent from `starred` keep the `created_at` fallback.
pub(super) fn stamp(rows: &mut [Value], starred: &[crate::forge_starred::StarredRow]) {
    for row in rows.iter_mut() {
        let at = row["number"]
            .as_u64()
            .and_then(|n| starred.iter().find(|s| u64::from(s.number) == n))
            .and_then(|s| s.starred_at.clone());
        if let Some(at) = at {
            row[STAR_AT_FIELD] = Value::String(at);
        }
    }
}

/// Whether the Judge queue holds an interactive fallback row. Only `.any()`
/// is asked, so the order (and its star-time reads) is skipped: this probe
/// runs every Judge role-runner tick and must stay one conditional listing.
pub fn has_interactive_fallback(root: &Path, gh: &Path) -> Result<bool> {
    if !prefer_human_prs(root) {
        return Ok(false);
    }
    Ok(fetch(root, gh, PrRole::Judge, false)?
        .iter()
        .any(|r| r["mode"] == "fallback" && r["origin"] == "interactive"))
}
