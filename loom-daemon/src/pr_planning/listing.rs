//! Paginated conditional REST reads retain author association/type, unlike
//! the reduced issue listing. Cache keys include repo and credential scope.
use super::*;
use crate::forge_etag_store as store;
use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::path::Path;
use std::process::Command;

fn read(root: &Path, gh: &Path, suffix: &str) -> Result<Value> {
    let repo = std::env::var("LOOM_REPO").ok();
    let target = store::resolve_target(Some(root), repo.as_deref());
    let url = format!("repos/{}/{}", target.repo.as_deref().unwrap_or("{owner}/{repo}"), suffix);
    let key = store::cache_key(Some(root), &target, &url);
    let path = store::disk_cache_path(&key);
    let sent = store::read_disk_entry(&path);
    let (status, response, stderr) = store::fetch_conditional(
        "pr_planning",
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

/// Listing and fallback guard failures are errors, never an empty queue.
/// Fallback guard owns bot admission, lifetime cap and head dedup. It grants
/// review only; no workflow labels or branch mutations occur here.
pub fn fetch_queue(root: &Path, gh: &Path, role: PrRole) -> Result<Vec<Value>> {
    let mut rows = Vec::new();
    for page in 1.. {
        let value = read(
            root,
            gh,
            &format!("pulls?state=open&sort=created&direction=desc&per_page=100&page={page}"),
        )?;
        let batch = value.as_array().context("PR listing must be an array")?;
        rows.extend(batch.iter().cloned());
        if batch.len() < 100 {
            break;
        }
    }
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
                row["mergeable"] = read(root, gh, &format!("pulls/{number}"))?["mergeable"].clone();
            }
        }
    }
    let mut queue = ordered_queue(rows, role, prefer_human_prs(root), &TrustPolicy::for_root(root));
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

pub fn has_interactive_fallback(root: &Path, gh: &Path) -> Result<bool> {
    if !prefer_human_prs(root) {
        return Ok(false);
    }
    Ok(fetch_queue(root, gh, PrRole::Judge)?
        .iter()
        .any(|r| r["mode"] == "fallback" && r["origin"] == "interactive"))
}
