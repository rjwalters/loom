//! `loom-daemon merge-pr consolidate-prepare` / `consolidate-abort` (#9688,
//! contract ADR-0023) — turn an eligible group of component PRs into ONE
//! candidate PR with every source reserved, or undo an attempt cleanly.
//!
//! # Contract highlights the caller must not need to re-derive
//!
//! - Eligibility is checked fresh immediately before any mutation, and the
//!   head pins are re-read AFTER the candidate exists (a source push during
//!   preparation aborts the attempt instead of silently re-pinning).
//! - Adopt-first: an open candidate PR for this group's deterministic
//!   attempt id is adopted, never duplicated; missing reservations are
//!   backfilled, present ones left alone.
//! - A construction conflict is a hard abort: sources untouched, scratch
//!   worktree removed, no partial candidate.
//! - Abort releases ONLY the attempt's own reservations (markers whose
//!   `plan=` names this attempt), preserves every source, and never touches
//!   a merged candidate (landing wins; reconciliation territory).

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use loom_daemon::merge_pr::consolidate::{
    self as cons, attempt_id, candidate_branch, check_eligibility, fetch_component,
    find_open_candidate, mapping_body, parse_mapping, push_branch, remove_worktree,
    reservation_comment_body, reservation_marker, reservation_present, Bounds, PrepareOutcome,
};
use loom_daemon::claim_reconciliation::merge_sequence::SEQUENCE_LABEL;
use loom_daemon::merge_pr::sequence::fetch_trusted_bodies;

#[derive(clap::Args)]
pub(crate) struct ConsolidatePrepareArgs {
    /// The component PR numbers, in landing order (oldest first). The order
    /// is advisory — eligibility and construction re-derive what they need —
    /// but it is the recorded intent.
    #[arg(long = "pr", value_name = "N")]
    prs: Vec<u32>,

    /// The recorded compatibility rationale (ADR-0023 E8). Required: a
    /// consolidation without a stated reason is exactly the silent
    /// scope-widening this contract exists to prevent.
    #[arg(long, value_name = "TEXT")]
    reason: String,

    /// OWNER/REPO. Omit to let `gh` resolve from the working directory.
    #[arg(long, value_name = "OWNER/REPO")]
    repo: Option<String>,
}

impl ConsolidatePrepareArgs {
    pub(crate) fn run(self) -> Result<()> {
        if self.prs.len() < 2 {
            bail!("consolidation needs at least two component PRs");
        }
        if let Some(nwo) = self.repo.as_deref() {
            std::env::set_var("LOOM_REPO", nwo);
        }
        let root = std::env::current_dir()?;
        let gh = std::path::PathBuf::from(cons::gh_bin_env());
        let bounds = Bounds::from_env();

        // 1-2. Fetch components + default branch, then eligibility — all
        // before any mutation.
        let mut components = Vec::new();
        for n in &self.prs {
            components.push(fetch_component(&gh, &root, *n).context(format!("reading PR #{n}"))?);
        }
        let default_branch = cons_default_branch(&gh, &root)?;
        let markers = fetch_markers(&gh, &root, &components);
        let failures =
            check_eligibility(&components, &markers, &default_branch, &self.reason, &bounds);
        if !failures.is_empty() {
            eprintln!("consolidate-prepare: group is NOT eligible (nothing was written):");
            for f in &failures {
                eprintln!("  - {f:?}");
            }
            std::process::exit(1);
        }

        // 3. Identity + adopt-first.
        let pins: Vec<(u32, &str)> = components.iter().filter_map(|c| c.pin()).collect();
        let attempt = attempt_id(&pins);
        let branch = candidate_branch(&attempt);
        if let Some(existing) = find_open_candidate(&gh, &root, &branch)? {
            let applied = backfill_reservations(&gh, &root, existing, &attempt, &components)?;
            println!(
                "AlreadyPrepared: candidate PR #{existing} already exists for attempt {attempt} \
                 — adopted, {applied} reservation(s) backfilled"
            );
            return Ok(());
        }

        // 4. Construction off the LIVE default-branch tip.
        let base = live_base(&gh, &root, &default_branch)?;
        let worktree = root.join(format!(".loom/worktrees/consolidate-{attempt}"));
        let pin_refs: Vec<(u32, &str)> = components.iter().filter_map(|c| c.pin()).collect();
        let candidate_head = match cons::construct("git", &root, &worktree, &base, &pin_refs) {
            Ok(head) => head,
            Err(conflict) => {
                eprintln!(
                    "consolidate-prepare: HARD ABORT — construction conflict on component #{}: \
                     {}\nSources are untouched; v1 does not retry or evict (ADR-0023 §2 E10). \
                     Fix or drop the conflicting component and re-run.",
                    conflict.number, conflict.detail
                );
                std::process::exit(1);
            }
        };

        // 5. Push + create the candidate PR (adopt if a racing worker won).
        if let Err(e) = push_branch("git", &root, &worktree, &branch) {
            remove_worktree("git", &root, &worktree);
            bail!("{e}");
        }
        let body =
            mapping_body(&attempt, &default_branch, &candidate_head, &pin_refs, &self.reason);
        let candidate_pr = match find_open_candidate(&gh, &root, &branch)? {
            Some(existing) => existing,
            None => {
                match create_candidate_pr(&gh, &root, &branch, &default_branch, &attempt, &body) {
                    Ok(pr) => pr,
                    Err(e) => {
                        remove_worktree("git", &root, &worktree);
                        return Err(e.context(
                            "creating the candidate PR — the pushed branch remains \
                         for adopt-or-abort on the next run (ADR-0023 §5)",
                        ));
                    }
                }
            }
        };

        // 6. Re-read the source heads: a push during preparation aborts
        // instead of silently re-pinning (ADR-0023 worked example 5).
        let mut moved = Vec::new();
        for c in &components {
            let fresh = fetch_component(&gh, &root, c.number)?;
            if fresh.pin() != c.pin() {
                moved.push(c.number);
            }
        }
        if !moved.is_empty() {
            remove_worktree("git", &root, &worktree);
            bail!(
                "component heads moved during preparation: {:?} — HARD ABORT per ADR-0023; the \
                 candidate branch remains for a fresh attempt against the new heads",
                moved
            );
        }

        // 7. Reservations.
        let applied =
            apply_reservations(&gh, &root, candidate_pr, &candidate_head, &attempt, &components)?;
        remove_worktree("git", &root, &worktree);
        println!(
            "Prepared: attempt {attempt}, candidate PR #{candidate_pr} (branch {branch}), \
             {applied} reservation(s) applied, components {:?}",
            self.prs
        );
        let _ = PrepareOutcome::Prepared {
            attempt,
            candidate_pr,
            branch,
            reservations: applied,
        };
        Ok(())
    }
}

#[derive(clap::Args)]
pub(crate) struct ConsolidateAbortArgs {
    /// The candidate PR to abort.
    #[arg(long, value_name = "N")]
    pr: u32,

    /// OWNER/REPO. Omit to let `gh` resolve from the working directory.
    #[arg(long, value_name = "OWNER/REPO")]
    repo: Option<String>,
}

impl ConsolidateAbortArgs {
    pub(crate) fn run(self) -> Result<()> {
        if let Some(nwo) = self.repo.as_deref() {
            std::env::set_var("LOOM_REPO", nwo);
        }
        let root = std::env::current_dir()?;
        let gh = std::path::PathBuf::from(cons::gh_bin_env());
        abort(&gh, &root, self.pr)
    }
}

// --- shared helpers (CLI-local I/O glue) --------------------------------

fn cons_default_branch(gh: &std::path::Path, root: &std::path::Path) -> Result<String> {
    // consolidate::default_branch is private to the module; this thin
    // wrapper re-reads it through the same public surface the module tests.
    let out = std::process::Command::new(gh)
        .args([
            "repo",
            "view",
            "--json",
            "defaultBranchRef",
            "--jq",
            ".defaultBranchRef.name",
        ])
        .current_dir(root)
        .env("GH_CONFIG_DIR", std::env::var("GH_CONFIG_DIR").unwrap_or_default())
        .output()
        .context("gh repo view")?;
    if !out.status.success() {
        bail!("reading the default branch: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn live_base(gh: &std::path::Path, root: &std::path::Path, default_branch: &str) -> Result<String> {
    let out = std::process::Command::new(gh)
        .args([
            "api",
            &format!("repos/{{owner}}/{{repo}}/commits/{default_branch}"),
            "--jq",
            ".sha",
        ])
        .current_dir(root)
        .output()
        .context("gh api commits")?;
    if !out.status.success() {
        bail!(
            "reading the default-branch tip: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn fetch_markers(
    gh: &std::path::Path,
    root: &std::path::Path,
    components: &[loom_daemon::merge_pr::consolidate::ComponentState],
) -> std::collections::BTreeMap<u32, loom_daemon::merge_pr::sequence::SequenceMarker> {
    let bin = gh.to_string_lossy().to_string();
    let mut out = std::collections::BTreeMap::new();
    for c in components {
        if let Some(bodies) = fetch_trusted_bodies(&bin, root, "{owner}/{repo}", c.number) {
            if let Some(m) = loom_daemon::merge_pr::sequence::parse(&bodies) {
                out.insert(c.number, m);
            }
        }
    }
    out
}

/// Apply any missing reservations; skip ones whose exact marker already
/// exists. Returns (applied, already-present) counts on the adopted path.
fn apply_reservations(
    gh: &std::path::Path,
    root: &std::path::Path,
    candidate_pr: u32,
    candidate_head: &str,
    attempt: &str,
    components: &[loom_daemon::merge_pr::consolidate::ComponentState],
) -> Result<usize> {
    let (applied, _) =
        backfill_reservations_inner(gh, root, candidate_pr, candidate_head, attempt, components)?;
    Ok(applied)
}

fn backfill_reservations(
    gh: &std::path::Path,
    root: &std::path::Path,
    candidate_pr: u32,
    attempt: &str,
    components: &[loom_daemon::merge_pr::consolidate::ComponentState],
) -> Result<usize> {
    // The adopted candidate's head: from the PR itself, not re-derived.
    let out = std::process::Command::new(gh)
        .args([
            "pr",
            "view",
            &candidate_pr.to_string(),
            "--json",
            "headRefOid",
            "--jq",
            ".headRefOid",
        ])
        .current_dir(root)
        .output()
        .context("gh pr view candidate")?;
    if !out.status.success() {
        bail!("reading the candidate head: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let head = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let (applied, _) =
        backfill_reservations_inner(gh, root, candidate_pr, &head, attempt, components)?;
    Ok(applied)
}

fn backfill_reservations_inner(
    gh: &std::path::Path,
    root: &std::path::Path,
    candidate_pr: u32,
    candidate_head: &str,
    attempt: &str,
    components: &[loom_daemon::merge_pr::consolidate::ComponentState],
) -> Result<(usize, usize)> {
    let bin = gh.to_string_lossy().to_string();
    let mut applied = 0;
    let mut present = 0;
    for c in components {
        let marker = reservation_marker(candidate_pr, candidate_head, c, attempt);
        let bodies =
            fetch_trusted_bodies(&bin, root, "{owner}/{repo}", c.number).unwrap_or_default();
        if reservation_present(&bodies, &marker) {
            present += 1;
            continue;
        }
        let n = c.number.to_string();
        let out = std::process::Command::new(gh)
            .args(["pr", "edit", &n, "--add-label", SEQUENCE_LABEL])
            .current_dir(root)
            .output()
            .context("gh pr edit (reservation label)")?;
        if !out.status.success() {
            bail!("labeling source PR #{n}: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        let body = reservation_comment_body(&marker, attempt);
        let out = std::process::Command::new(gh)
            .args(["pr", "comment", &n, "--body", &body])
            .current_dir(root)
            .output()
            .context("gh pr comment (reservation)")?;
        if !out.status.success() {
            bail!("commenting source PR #{n}: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        applied += 1;
    }
    Ok((applied, present))
}

fn create_candidate_pr(
    gh: &std::path::Path,
    root: &std::path::Path,
    branch: &str,
    default_branch: &str,
    attempt: &str,
    body: &str,
) -> Result<u32> {
    // The body goes through a temp file: forge-controlled text belongs on a
    // stream, never an argument vector.
    let path = root.join(format!(".loom-consolidate-body-{attempt}.md"));
    std::fs::write(&path, body)?;
    let title = format!("consolidated candidate ({attempt})");
    let out = std::process::Command::new(gh)
        .args([
            "pr",
            "create",
            "--head",
            branch,
            "--base",
            default_branch,
            "--title",
            &title,
            "--body-file",
            path.to_str().unwrap_or_default(),
        ])
        .current_dir(root)
        .output()
        .context("gh pr create")?;
    let _ = std::fs::remove_file(&path);
    if !out.status.success() {
        bail!("gh pr create: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let url = stdout
        .lines()
        .rev()
        .find(|l| l.starts_with("http"))
        .unwrap_or_default();
    let number = url
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .parse::<u32>()
        .context(format!("parsing the candidate PR number from {url:?}"))?;
    Ok(number)
}

fn abort(gh: &std::path::Path, root: &std::path::Path, candidate: u32) -> Result<()> {
    let bin = gh.to_string_lossy().to_string();
    // 1. The mapping comes from the candidate's own body (the ledger).
    let out = std::process::Command::new(gh)
        .args([
            "pr",
            "view",
            &candidate.to_string(),
            "--json",
            "body,state,headRefName",
        ])
        .current_dir(root)
        .output()
        .context("gh pr view candidate")?;
    if !out.status.success() {
        bail!(
            "reading candidate PR #{candidate}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct CandidatePr {
        body: String,
        state: String,
        head_ref_name: String,
    }
    let c: CandidatePr = serde_json::from_slice(&out.stdout).context("parse candidate JSON")?;
    let Some(mapping) = parse_mapping(&c.body) else {
        bail!("PR #{candidate} carries no consolidation mapping — not a candidate PR");
    };
    if c.state == "MERGED" {
        bail!("candidate #{candidate} is MERGED — landing wins; run consolidate-reconcile (#9689), never abort");
    }

    // 2. Release ONLY this attempt's reservations: a source's NEWEST marker
    // must name this attempt, or it is not ours to touch.
    let mut released = 0;
    for (number, _head) in &mapping.components {
        let Some(bodies) = fetch_trusted_bodies(&bin, root, "{owner}/{repo}", *number) else {
            eprintln!("consolidate-abort: could not read PR #{number}'s comments — leaving its reservation for a re-run");
            continue;
        };
        let ours =
            loom_daemon::merge_pr::sequence::parse(&bodies).filter(|m| m.plan == mapping.attempt);
        let Some(marker) = ours else { continue };
        let n = number.to_string();
        let out = std::process::Command::new(gh)
            .args(["pr", "edit", &n, "--remove-label", SEQUENCE_LABEL])
            .current_dir(root)
            .output()
            .context("gh pr edit (release)")?;
        if !out.status.success() {
            eprintln!(
                "consolidate-abort: removing the label from #{n} failed (continuing): {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let body = cons::reservation_release_body(&marker, &mapping.attempt);
        let out = std::process::Command::new(gh)
            .args(["pr", "comment", &n, "--body", &body])
            .current_dir(root)
            .output()
            .context("gh pr comment (release)")?;
        if !out.status.success() {
            eprintln!("consolidate-abort: release comment on #{n} failed (continuing)");
        }
        released += 1;
    }

    // 3. Close the candidate (idempotent — closing a closed PR is a no-op).
    let comment = format!(
        "Consolidation attempt `{}` aborted: the candidate is withdrawn and every component \
         reservation is released. The component PRs are untouched and actionable. (ADR-0023 §3, \
         #9688)",
        mapping.attempt
    );
    let out = std::process::Command::new(gh)
        .args(["pr", "close", &candidate.to_string(), "--comment", &comment])
        .current_dir(root)
        .output()
        .context("gh pr close")?;
    if !out.status.success() {
        eprintln!(
            "consolidate-abort: closing the candidate failed (it may already be closed): {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    // 4. Branch cleanup, best-effort (a leftover is cleaned by the reapers;
    // #9372 guards do not apply — consolidated branches have no children).
    let out = std::process::Command::new(gh)
        .args([
            "api",
            "-X",
            "DELETE",
            &format!("repos/{{owner}}/{{repo}}/git/refs/heads/{}", c.head_ref_name),
        ])
        .current_dir(root)
        .output()
        .context("gh api delete ref")?;
    if !out.status.success() {
        eprintln!(
            "consolidate-abort: branch cleanup skipped ({})",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    println!(
        "Aborted: attempt {}, candidate #{candidate} closed, {released} reservation(s) released",
        mapping.attempt
    );
    Ok(())
}
