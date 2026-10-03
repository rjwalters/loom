//! `loom-daemon merge-pr consolidate-prepare` / `consolidate-abort` (#9688,
//! contract ADR-0023) — turn an eligible group of component PRs into ONE
//! candidate PR with every source reserved, or undo an attempt cleanly.
//!
//! # Contract highlights the caller must not need to re-derive
//!
//! - Eligibility is checked fresh immediately before any mutation.
//! - **Any push aborts the attempt** (ADR-0023 §3, operator ruling
//!   2026-10-01). Once the reservations are applied, preparation runs the
//!   three-part `pin_check`: candidate head equals the ledger, every source
//!   head equals its pin, every reservation is live. Any failure withdraws
//!   the candidate (close + release + branch delete) and fails the run.
//!   Nothing is ever re-pinned or repaired in place; the next attempt
//!   against the new heads has a fresh id.
//! - Adopt-first: an open candidate PR for this group's deterministic
//!   attempt id is adopted, never duplicated, but only while the attempt is
//!   live. A candidate whose head moved off its ledger, or a reservation that
//!   was applied and then lost, aborts instead of being adopted. Only a
//!   reservation that was never written is backfilled, against the
//!   ledger's recorded candidate head (never the live one).
//! - A construction conflict is a hard abort: sources untouched, scratch
//!   worktree removed, no partial candidate.
//! - Abort releases ONLY the attempt's own still-live reservations (the
//!   source's live hold names this attempt; holds the ordering pass already
//!   voided are skipped), preserves every source, records the ADR §7 abort
//!   cause, and never touches a merged candidate (landing wins;
//!   reconciliation territory). A released reservation no longer counts
//!   against eligibility (`live_marker`).
//! - `consolidate-reconcile` (#9689) finishes a MERGED candidate's
//!   bookkeeping; it never merges and never releases a reservation (the
//!   ordering pass is the one releaser on landing, ADR-0023 §4).
//! - Every `gh` call honors `--repo`/`LOOM_REPO` and the per-root credential.

use anyhow::{bail, Context, Result};
use loom_daemon::claim_reconciliation::merge_sequence::SEQUENCE_LABEL;
use loom_daemon::merge_pr::consolidate::{
    self as cons, attempt_id, candidate_branch, check_eligibility, fetch_component,
    find_open_candidate, live_marker, mapping_body, parse_mapping, pin_check, push_branch,
    remove_worktree, reservation_comment_body, reservation_marker, reservation_present,
    reservation_state, AbortReason, Bounds, CandidateMapping, PrepareOutcome, ReservationState,
};
use loom_daemon::merge_pr::sequence::fetch_trusted_bodies;
use serde::Deserialize;

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
        scope_repo(self.repo.as_deref());
        let root = std::env::current_dir()?;
        let gh = std::path::PathBuf::from(loom_daemon::gh_invocation::gh_bin());
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
            let applied = adopt(&gh, &root, existing, &attempt, &components)?;
            println!(
                "AlreadyPrepared: candidate PR #{existing} already exists for attempt {attempt} \
                 — adopted, {applied} reservation(s) backfilled"
            );
            return Ok(());
        }

        // 4. Construction off the LIVE default-branch tip. `construct()` is
        // pure git mechanics — it references `base` and every pin as bare
        // local objects, which a clone behind the live tip (the ordinary
        // case: `base` just came from the API, not a local fetch) or a
        // component head never otherwise fetched will not have.
        let base = live_base(&gh, &root, &default_branch)?;
        if !has_commit(&root, &base) {
            let _ = std::process::Command::new("git")
                .args(["fetch", "--quiet", "origin", &default_branch])
                .current_dir(&root)
                .status();
            if !has_commit(&root, &base) {
                bail!("could not fetch the default branch tip {base} before construction");
            }
        }
        let worktree = root.join(format!(".loom/worktrees/consolidate-{attempt}"));
        let pin_refs: Vec<(u32, &str)> = components.iter().filter_map(|c| c.pin()).collect();
        for &(number, head) in &pin_refs {
            ensure_pr_head_fetched(&root, number, head)
                .context(format!("fetching PR #{number}'s head before construction"))?;
        }
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
            // A racing worker created the PR between our push and here: its
            // ledger, not ours, is the attempt's record, so adopt it through
            // the same live-attempt checks as step 3.
            Some(existing) => {
                remove_worktree("git", &root, &worktree);
                let applied = adopt(&gh, &root, existing, &attempt, &components)?;
                println!(
                    "AlreadyPrepared: candidate PR #{existing} was created concurrently for \
                     attempt {attempt} — adopted, {applied} reservation(s) backfilled"
                );
                return Ok(());
            }
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
        remove_worktree("git", &root, &worktree);
        let mapping = parse_mapping(&body)
            .context("the candidate body this run wrote does not parse as a mapping")?;

        // 6. Reservations, then (7) the push-abort check. The order matters:
        // re-reading the heads BEFORE reserving would leave a window where a
        // source push lands between the read and the reservation, pinning a
        // hold to a head that is already gone (ADR-0023 worked example 5).
        let applied = apply_reservations(
            &gh,
            &root,
            candidate_pr,
            &mapping.candidate_head,
            &attempt,
            &components,
        )?;
        verify_or_abort(&gh, &root, candidate_pr, &mapping)?;
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

    /// Why (ADR-0023 §7 abort cause, recorded in the close comment):
    /// `operator` (a decision) or `ci-failure` (the candidate's CI is red).
    #[arg(long, value_enum, default_value_t = RequestedCause::Operator)]
    cause: RequestedCause,

    /// OWNER/REPO. Omit to let `gh` resolve from the working directory.
    #[arg(long, value_name = "OWNER/REPO")]
    repo: Option<String>,
}

impl ConsolidateAbortArgs {
    pub(crate) fn run(self) -> Result<()> {
        scope_repo(self.repo.as_deref());
        let root = std::env::current_dir()?;
        let gh = std::path::PathBuf::from(loom_daemon::gh_invocation::gh_bin());
        let reason = match self.cause {
            RequestedCause::Operator => AbortReason::Operator,
            RequestedCause::CiFailure => AbortReason::CiFailure,
        };
        abort(&gh, &root, self.pr, &reason)
    }
}

#[derive(clap::Args)]
pub(crate) struct ConsolidateReconcileArgs {
    /// The MERGED candidate PR to reconcile.
    #[arg(long, value_name = "N")]
    pr: u32,

    /// OWNER/REPO. Omit to let `gh` resolve from the working directory.
    #[arg(long, value_name = "OWNER/REPO")]
    repo: Option<String>,
}

impl ConsolidateReconcileArgs {
    pub(crate) fn run(self) -> Result<()> {
        // Same scoping as prepare/abort: reconcile reads through
        // `{owner}/{repo}` placeholders and `fetch_trusted_bodies`, which
        // honor GH_REPO, not a `--repo` flag.
        scope_repo(self.repo.as_deref());
        let root = std::env::current_dir()?;
        let gh = std::path::PathBuf::from(cons::gh_bin_env());
        // The orchestration lives in the library so it runs under test
        // end-to-end (restart, unverified component, pushed source).
        let report = cons::reconcile::reconcile(&gh, "git", &root, self.pr)?;
        println!("{}", report.summary());
        if !report.unverified.is_empty() {
            eprintln!(
                "NOT verified as included (left open for human review — never closed on an \
                 unknown): {:?}",
                report.unverified
            );
        }
        if !report.unread.is_empty() {
            eprintln!(
                "Could not read the transcript or live head (nothing written to them) — re-run \
                 to retry: {:?}",
                report.unread
            );
        }
        if !report.untouched_open.is_empty() {
            eprintln!(
                "Pushed after landing — left open with an untouched-open status (ADR-0023 §6.2): \
                 {:?}",
                report.untouched_open
            );
        }
        if !report.complete() {
            std::process::exit(1);
        }
        Ok(())
    }
}

/// The causes a human may name on `consolidate-abort`; the push-abort
/// causes are detected, never claimed.
#[derive(Clone, Copy, clap::ValueEnum)]
pub(crate) enum RequestedCause {
    Operator,
    CiFailure,
}

// --- shared helpers (CLI-local I/O glue) --------------------------------

/// Scope every `gh` call in this process to the target repo. `LOOM_REPO`
/// feeds `consolidate::gh` (which appends `--repo`); `GH_REPO` is what `gh`
/// itself honors for the subcommands and `{owner}/{repo}` API placeholders
/// that take no `--repo` flag — including `sequence::fetch_trusted_bodies`.
fn scope_repo(flag: Option<&str>) {
    if let Some(nwo) = flag {
        std::env::set_var("LOOM_REPO", nwo);
    }
    if let Ok(nwo) = std::env::var("LOOM_REPO") {
        if !nwo.trim().is_empty() {
            std::env::set_var("GH_REPO", nwo);
        }
    }
}

/// A `gh` command in `root` with the per-root credential applied (#5401: a
/// cross-owner managed repo needs its own owner's installation token) — the
/// same routing `consolidate::gh` and the sequence reads use.
fn gh_cmd(gh: &std::path::Path, root: &std::path::Path) -> std::process::Command {
    let mut cmd = std::process::Command::new(gh);
    cmd.current_dir(root);
    loom_daemon::credential_preflight::apply_gh_config_for_root(&mut cmd, root);
    cmd
}

fn cons_default_branch(gh: &std::path::Path, root: &std::path::Path) -> Result<String> {
    // consolidate::default_branch is private to the module; this thin
    // wrapper re-reads it through the same public surface the module tests.
    let out = gh_cmd(gh, root)
        .args([
            "repo",
            "view",
            "--json",
            "defaultBranchRef",
            "--jq",
            ".defaultBranchRef.name",
        ])
        .output()
        .context("gh repo view")?;
    if !out.status.success() {
        bail!("reading the default branch: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn live_base(gh: &std::path::Path, root: &std::path::Path, default_branch: &str) -> Result<String> {
    let out = gh_cmd(gh, root)
        .args([
            "api",
            &format!("repos/{{owner}}/{{repo}}/commits/{default_branch}"),
            "--jq",
            ".sha",
        ])
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
    // Only LIVE holds count (label present, not released by a newer release
    // marker): a reservation from an aborted attempt must not reject this one.
    for c in components.iter().filter(|c| c.has(SEQUENCE_LABEL)) {
        if let Some(bodies) = fetch_trusted_bodies(&bin, root, "{owner}/{repo}", c.number) {
            if let Some(m) = live_marker(c, &bodies) {
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

/// The candidate PR as the push-abort checks need it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CandidatePr {
    body: String,
    state: String,
    head_ref_name: String,
    head_ref_oid: String,
}

fn view_candidate(
    gh: &std::path::Path,
    root: &std::path::Path,
    candidate: u32,
) -> Result<CandidatePr> {
    let out = gh_cmd(gh, root)
        .args([
            "pr",
            "view",
            &candidate.to_string(),
            "--json",
            "body,state,headRefName,headRefOid",
        ])
        .output()
        .context("gh pr view candidate")?;
    if !out.status.success() {
        bail!(
            "reading candidate PR #{candidate}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    serde_json::from_slice(&out.stdout).context("parse candidate JSON")
}

/// Every mapped source's live state and oldest-first trusted bodies. An
/// unreadable transcript is an error, not an abort: the check cannot tell a
/// lost reservation from a failed read, and aborting on a read failure would
/// destroy a live attempt. The attempt stays as it is; a re-run re-checks.
fn read_sources(
    gh: &std::path::Path,
    root: &std::path::Path,
    mapping: &CandidateMapping,
) -> Result<Vec<(cons::ComponentState, Vec<String>)>> {
    let bin = gh.to_string_lossy().to_string();
    let mut sources = Vec::new();
    for (n, _) in &mapping.components {
        let c = fetch_component(gh, root, *n).context(format!("re-reading PR #{n}"))?;
        let Some(bodies) = fetch_trusted_bodies(&bin, root, "{owner}/{repo}", *n) else {
            bail!(
                "could not read PR #{n}'s comments to verify its reservation; attempt {} is left \
                 as it is, re-run to re-check it",
                mapping.attempt
            );
        };
        sources.push((c, bodies));
    }
    Ok(sources)
}

/// ADR-0023 §3: once this run's reservations are in place, the attempt must
/// still be live, or it is aborted. Nothing is re-pinned.
fn verify_or_abort(
    gh: &std::path::Path,
    root: &std::path::Path,
    candidate_pr: u32,
    mapping: &CandidateMapping,
) -> Result<()> {
    let live_head = view_candidate(gh, root, candidate_pr)?.head_ref_oid;
    let sources = read_sources(gh, root, mapping)?;
    match pin_check(mapping, candidate_pr, &live_head, &sources) {
        None => Ok(()),
        Some(reason) => Err(abort_attempt(gh, root, candidate_pr, &reason)),
    }
}

/// Abort for `reason` and turn it into the run's error. A failed withdrawal
/// is reported with the command that finishes it.
fn abort_attempt(
    gh: &std::path::Path,
    root: &std::path::Path,
    candidate_pr: u32,
    reason: &AbortReason,
) -> anyhow::Error {
    if let Err(e) = abort(gh, root, candidate_pr, reason) {
        eprintln!(
            "consolidate-prepare: withdrawing candidate #{candidate_pr} failed ({e:#}); run \
             `consolidate-abort --pr {candidate_pr}` to finish the cleanup"
        );
    }
    anyhow::anyhow!(
        "consolidation attempt {} — HARD ABORT per ADR-0023 §3 (cause {}); candidate \
         #{candidate_pr} withdrawn. Re-run against the current heads for a fresh attempt id",
        reason.describe(),
        reason.cause()
    )
}

/// Whether `sha` is present as a local git object.
fn has_commit(root: &std::path::Path, sha: &str) -> bool {
    std::process::Command::new("git")
        .args(["cat-file", "-e", &format!("{sha}^{{commit}}")])
        .current_dir(root)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Make sure `sha` (PR #`pr_number`'s head) is a local git object, fetching
/// the PR's head ref if it is not. Mirrors `stacked_children::establish_pin`:
/// only the post-fetch re-check decides, the fetch's own exit status is not
/// interesting on its own.
fn ensure_pr_head_fetched(root: &std::path::Path, pr_number: u32, sha: &str) -> Result<()> {
    if has_commit(root, sha) {
        return Ok(());
    }
    let _ = std::process::Command::new("git")
        .args([
            "fetch",
            "--quiet",
            "origin",
            &format!("refs/pull/{pr_number}/head"),
        ])
        .current_dir(root)
        .status();
    if !has_commit(root, sha) {
        bail!("could not fetch PR #{pr_number}'s head {sha} to verify ancestry");
    }
    Ok(())
}

/// Git-level check that the candidate's tree at `candidate_head` actually
/// contains every component's pinned commit as an ancestor (ADR-0023 §5
/// hardening, Judge finding on #9688: `adopt()`'s other checks are entirely
/// self-consistency over the candidate's own attacker-reachable body/branch).
/// A same-repo candidate branch needs write access to push, so convergence
/// alone is not a strong enough guarantee here — `--no-ff` construction makes
/// inclusion an ancestry fact, so this is checkable directly.
fn verify_ancestry(
    root: &std::path::Path,
    candidate_pr: u32,
    candidate_head: &str,
    components: &[cons::ComponentState],
) -> Result<()> {
    ensure_pr_head_fetched(root, candidate_pr, candidate_head)?;
    for c in components {
        let Some(head) = c.head_sha.as_deref() else {
            continue;
        };
        ensure_pr_head_fetched(root, c.number, head)?;
        let is_ancestor = std::process::Command::new("git")
            .args(["merge-base", "--is-ancestor", head, candidate_head])
            .current_dir(root)
            .status()
            .context("git merge-base --is-ancestor")?
            .success();
        if !is_ancestor {
            bail!(
                "candidate PR #{candidate_pr}'s tree at {candidate_head} does not actually \
                 contain PR #{}'s commit {head} as an ancestor — refusing to adopt it (the \
                 candidate's own body/branch can be attacker-controlled; inspect by hand)",
                c.number
            );
        }
    }
    Ok(())
}

/// Adopt an open candidate for `attempt` — only while the attempt is live.
///
/// The ledger (the candidate body) is the record: its attempt and pins must
/// be this group's, its recorded candidate head must still be the live one,
/// and no reservation it already wrote may have been lost. Any of those
/// failing is a push abort, not a repair. Reservations that were never
/// written are backfilled against the RECORDED candidate head, then the full
/// check runs again. Returns the number backfilled.
fn adopt(
    gh: &std::path::Path,
    root: &std::path::Path,
    candidate_pr: u32,
    attempt: &str,
    components: &[cons::ComponentState],
) -> Result<usize> {
    let cand = view_candidate(gh, root, candidate_pr)?;
    let Some(mapping) = parse_mapping(&cand.body) else {
        bail!(
            "open PR #{candidate_pr} on the {attempt} candidate branch carries no consolidation \
             mapping — refusing to adopt it; inspect it by hand"
        );
    };
    let ours: std::collections::BTreeSet<(u32, String)> = components
        .iter()
        .filter_map(|c| c.pin().map(|(n, h)| (n, h.to_string())))
        .collect();
    let recorded: std::collections::BTreeSet<(u32, String)> =
        mapping.components.iter().cloned().collect();
    if mapping.attempt != attempt || ours != recorded {
        bail!(
            "candidate PR #{candidate_pr}'s ledger (attempt {}) does not record this group's \
             pins — refusing to adopt it; inspect it by hand",
            mapping.attempt
        );
    }
    if cand.head_ref_oid != mapping.candidate_head {
        return Err(abort_attempt(
            gh,
            root,
            candidate_pr,
            &AbortReason::CandidatePush {
                recorded: mapping.candidate_head.clone(),
                live: cand.head_ref_oid,
            },
        ));
    }
    // The checks above are entirely self-consistency over the candidate's OWN
    // body/branch — an attacker who precomputes this attempt's deterministic
    // id can satisfy every one of them. Verify at the git level that the
    // candidate's tree actually descends from each component's pinned commit
    // before trusting the ledger any further.
    verify_ancestry(root, candidate_pr, &mapping.candidate_head, components)?;
    let sources = read_sources(gh, root, &mapping)?;
    let lost: Vec<u32> = sources
        .iter()
        .filter(|(c, bodies)| {
            let expected = reservation_marker(candidate_pr, &mapping.candidate_head, c, attempt);
            reservation_state(c, bodies, &expected) == ReservationState::Lost
        })
        .map(|(c, _)| c.number)
        .collect();
    if !lost.is_empty() {
        return Err(abort_attempt(gh, root, candidate_pr, &AbortReason::ReservationLost(lost)));
    }
    let (applied, _) = backfill_reservations_inner(
        gh,
        root,
        candidate_pr,
        &mapping.candidate_head,
        attempt,
        components,
    )?;
    verify_or_abort(gh, root, candidate_pr, &mapping)?;
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
        let out = gh_cmd(gh, root)
            .args(["pr", "edit", &n, "--add-label", SEQUENCE_LABEL])
            .output()
            .context("gh pr edit (reservation label)")?;
        if !out.status.success() {
            bail!("labeling source PR #{n}: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        let body = reservation_comment_body(&marker, attempt);
        let out = gh_cmd(gh, root)
            .args(["pr", "comment", &n, "--body", &body])
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
    let out = gh_cmd(gh, root)
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

fn abort(
    gh: &std::path::Path,
    root: &std::path::Path,
    candidate: u32,
    reason: &AbortReason,
) -> Result<()> {
    let bin = gh.to_string_lossy().to_string();
    // 1. The mapping comes from the candidate's own body (the ledger).
    let c = view_candidate(gh, root, candidate)?;
    let Some(mapping) = parse_mapping(&c.body) else {
        bail!("PR #{candidate} carries no consolidation mapping — not a candidate PR");
    };
    if c.state == "MERGED" {
        bail!("candidate #{candidate} is MERGED — landing wins; run consolidate-reconcile (#9689), never abort");
    }

    // 2. Release ONLY this attempt's STILL-LIVE reservations: the source's
    // live hold (label present, no newer `released`/`replanned` tombstone)
    // must name this attempt. A hold the ordering pass already voided, or a
    // newer hold from another plan, is not ours to touch (ADR-0023 §3).
    let mut released = 0;
    let mut skipped = 0;
    for (number, _head) in &mapping.components {
        let source = match fetch_component(gh, root, *number) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("consolidate-abort: could not read PR #{number} ({e:#}) — leaving its reservation for a re-run");
                continue;
            }
        };
        let Some(bodies) = fetch_trusted_bodies(&bin, root, "{owner}/{repo}", *number) else {
            eprintln!("consolidate-abort: could not read PR #{number}'s comments — leaving its reservation for a re-run");
            continue;
        };
        let ours = live_marker(&source, &bodies).filter(|m| m.plan == mapping.attempt);
        let Some(marker) = ours else {
            skipped += 1;
            continue;
        };
        let n = number.to_string();
        // Label first: the label IS the hold. If it cannot come off, the
        // reservation is still live — say so and post no release marker,
        // rather than write a transcript that contradicts the gate.
        let out = gh_cmd(gh, root)
            .args(["pr", "edit", &n, "--remove-label", SEQUENCE_LABEL])
            .output()
            .context("gh pr edit (release)")?;
        if !out.status.success() {
            eprintln!(
                "consolidate-abort: removing the label from #{n} failed — its reservation is \
                 still live; re-run consolidate-abort to release it: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
            continue;
        }
        let body = cons::reservation_release_body(&marker, &mapping.attempt);
        let out = gh_cmd(gh, root)
            .args(["pr", "comment", &n, "--body", &body])
            .output()
            .context("gh pr comment (release)")?;
        if !out.status.success() {
            eprintln!("consolidate-abort: release comment on #{n} failed (continuing)");
        }
        released += 1;
    }

    // 3. Close the candidate with the cause recorded (ADR-0023 §7).
    // Idempotent: a closed candidate only gets the comment again on a re-run.
    let comment = cons::abort_comment(&mapping.attempt, reason);
    let out = gh_cmd(gh, root)
        .args(["pr", "close", &candidate.to_string(), "--comment", &comment])
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
    let out = gh_cmd(gh, root)
        .args([
            "api",
            "-X",
            "DELETE",
            &format!("repos/{{owner}}/{{repo}}/git/refs/heads/{}", c.head_ref_name),
        ])
        .output()
        .context("gh api delete ref")?;
    if !out.status.success() {
        eprintln!(
            "consolidate-abort: branch cleanup skipped ({})",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    println!(
        "Aborted: attempt {} (cause {}), candidate #{candidate} closed, {released} reservation(s) \
         released, {skipped} already voided or released",
        mapping.attempt,
        reason.cause()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &std::path::Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git run");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn comp(number: u32, head: &str) -> cons::ComponentState {
        cons::ComponentState {
            number,
            state: "OPEN".to_string(),
            draft: false,
            head_sha: Some(head.to_string()),
            base_ref: "main".to_string(),
            labels: vec![],
            files: Default::default(),
            additions: 1,
            deletions: 0,
        }
    }

    /// A real candidate built by `--no-ff` merging a component branch into a
    /// base, exactly as `cons::construct` does — so the commit the component
    /// is checked against genuinely descends from it.
    fn built_candidate_repo() -> (tempfile::TempDir, String, String) {
        let tmp = tempfile::tempdir().unwrap();
        git(tmp.path(), &["init", "-q", "-b", "main"]);
        git(tmp.path(), &["config", "user.email", "t@t"]);
        git(tmp.path(), &["config", "user.name", "t"]);
        std::fs::write(tmp.path().join("base.txt"), "base\n").unwrap();
        git(tmp.path(), &["add", "."]);
        git(tmp.path(), &["commit", "-qm", "base"]);
        git(tmp.path(), &["checkout", "-qb", "component"]);
        std::fs::write(tmp.path().join("c.txt"), "c\n").unwrap();
        git(tmp.path(), &["add", "."]);
        git(tmp.path(), &["commit", "-qm", "component"]);
        let component_head = git(tmp.path(), &["rev-parse", "HEAD"]);
        git(tmp.path(), &["checkout", "-q", "main"]);
        git(tmp.path(), &["merge", "--no-ff", "--no-edit", "component"]);
        let candidate_head = git(tmp.path(), &["rev-parse", "HEAD"]);
        (tmp, candidate_head, component_head)
    }

    #[test]
    fn ancestry_holds_when_the_candidate_genuinely_merged_the_component() {
        let (tmp, candidate_head, component_head) = built_candidate_repo();
        let components = vec![comp(10, &component_head)];
        verify_ancestry(tmp.path(), 99, &candidate_head, &components)
            .expect("the candidate's --no-ff merge makes this an ancestry fact");
    }

    #[test]
    fn ancestry_fails_a_forged_candidate_that_never_merged_the_component() {
        let tmp = tempfile::tempdir().unwrap();
        git(tmp.path(), &["init", "-q", "-b", "main"]);
        git(tmp.path(), &["config", "user.email", "t@t"]);
        git(tmp.path(), &["config", "user.name", "t"]);
        std::fs::write(tmp.path().join("base.txt"), "base\n").unwrap();
        git(tmp.path(), &["add", "."]);
        git(tmp.path(), &["commit", "-qm", "base"]);
        let forged_candidate_head = git(tmp.path(), &["rev-parse", "HEAD"]);

        // A "component" commit that the forged candidate never actually
        // merged — e.g. a real PR whose number the attacker named in a
        // crafted body, but whose commit is unreachable from their branch.
        git(tmp.path(), &["checkout", "-qb", "unrelated"]);
        std::fs::write(tmp.path().join("c.txt"), "c\n").unwrap();
        git(tmp.path(), &["add", "."]);
        git(tmp.path(), &["commit", "-qm", "component"]);
        let component_head = git(tmp.path(), &["rev-parse", "HEAD"]);

        let components = vec![comp(10, &component_head)];
        let err = verify_ancestry(tmp.path(), 99, &forged_candidate_head, &components)
            .expect_err("a candidate that never merged the component must be refused");
        assert!(
            format!("{err:#}").contains("does not actually"),
            "error should name the ancestry failure: {err:#}"
        );
    }
}
