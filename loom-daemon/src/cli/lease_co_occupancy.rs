//! `loom-daemon lease co-occupancy <issue>` — refuse to hand back a shared
//! issue worktree that more than one live sweep may be editing at once
//! (rjwalters/kicad-tools#5783).
//!
//! ## The incident
//!
//! kicad-tools issue #5781 was claimed by four independent `/loom:sweep` runs
//! on the SAME host within ~30 minutes. Two of them dispatched Builders into
//! the shared `.loom/worktrees/issue-5781` at the same time: `worktree.sh N`
//! always resolves to the same path whichever sweep asks, and its "worktree
//! already exists and has uncommitted changes -> preserve it" fast path handed
//! the directory back unconditionally. One Builder's in-progress edit leaked
//! into the other's pushed commit (kicad-tools PR #5782).
//!
//! `sweep-lease-publish.sh` now closes most of that at the source: a fresh
//! lease from ANY other (host, sweep) pair on the same issue — same host or
//! not — makes publication exit 4, so the second sweep skips the issue before
//! it ever reaches a worktree. This subcommand is the worktree-layer backstop
//! for what that does not cover: a claim that never went through
//! `sweep-lease-publish.sh` (the daemon's own dispatch-time lease, #6179), a
//! `worktree.sh N` run outside any sweep lifecycle, or the narrow window
//! between two publishes.
//!
//! ## Why "2+ live leases" rather than "self vs. foreign"
//!
//! `worktree.sh` cannot reliably know which sweep it is running for — an
//! in-session Builder subagent's tool-call shell does not inherit the
//! orchestrator's run id, and an operator's manual `worktree.sh N` has none.
//! So this does not compare identities. It looks for the one state that is
//! never legitimate whoever "self" is: TWO OR MORE distinct (host, sweep)
//! pairs each holding a fresh, un-yielded lease on the same issue at once.
//!
//! ## Fail-open, like every other forge probe in the lease subsystem
//!
//! A missing `gh`, a failed or timed-out read, or zero/one live lease all exit
//! `0` — absence of evidence is not evidence of a peer
//! (`defaults/docs/lease-record.md`'s reader contract). Only an observed
//! 2+-live-lease state exits `1`. The read is bounded
//! (`LOOM_WORKTREE_LEASE_GUARD_TIMEOUT`, default 10s) because `worktree.sh`
//! sits on every Builder dispatch's hot path.
//!
//! ## Only trusted authors' records count (#9631)
//!
//! Every row is attributed ([`AUTHOR_JQ`]) and passed through
//! [`TrustPolicy`] before any marker is parsed, exactly like
//! `sweep_registry::guards`' lease reader (#9593/#9548): a lease or
//! lease-yield from an untrusted author reads as absent, and a row with no
//! author at all is untrusted. Otherwise an outsider could post two fresh
//! "leases" to wedge every Builder dispatch on the issue, or a fake yield to
//! excuse a real peer's live lease. Trust fails closed (nothing an
//! unresolved roster cannot vouch for is believed); the verdict itself stays
//! fail-open on a failed read, as above.
//!
//! ## Why this is Rust and not a `lib/*.sh` helper
//!
//! The kicad-tools fix shipped as a new `lib/worktree-foreign-lease-guard.sh`.
//! Upstream, `worktree.sh` is a `contract` shell-budget file whose portable
//! pool may not grow, and a new `.sh` cannot be admitted to the shell allowlist
//! as `contract` (`.loom/docs/shell-language-policy.md`). So the logic lives
//! here and `worktree.sh` keeps a one-line reach into it, the same shape as
//! `worktree-lock check-issue` (#8553) directly above it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use loom_daemon::comment_trust::{records::AUTHOR_JQ, TrustPolicy};
use loom_daemon::sweep_registry::{SweepRegistry, LEASE_MARKER_PREFIX};

/// The first-line prefix of a `loom:lease-yield` standdown record (#6287).
const YIELD_MARKER_PREFIX: &str = "<!-- loom:lease-yield host=";

/// Set to `1` to proceed despite a detected co-occupancy.
pub(crate) const OVERRIDE_ENV: &str = "WORKTREE_ALLOW_SHARED_LEASE";

/// Bound, in seconds, on the one `gh api` read this makes.
const TIMEOUT_ENV: &str = "LOOM_WORKTREE_LEASE_GUARD_TIMEOUT";
const DEFAULT_TIMEOUT_SECS: u64 = 10;

/// The same TTL knob, and default, `sweep-lease-publish.sh` uses.
const TTL_ENV: &str = "LOOM_LEASE_TTL_MINUTES";
const DEFAULT_TTL_MINUTES: f64 = 15.0;

#[derive(clap::Args)]
pub(crate) struct LeaseCoOccupancyArgs {
    /// The issue whose shared worktree is about to be handed back.
    #[arg(value_name = "ISSUE")]
    pub(crate) issue: u64,

    /// Repo checkout the `gh api` read resolves `{owner}/{repo}` from.
    #[arg(long, default_value = ".")]
    pub(crate) repo: PathBuf,

    /// On refusal, print worktree.sh's `{"success": false, ...}` document on
    /// stdout (for its `--json` contract) instead of human text on stderr.
    #[arg(long)]
    pub(crate) json: bool,
}

/// One lease or lease-yield comment, as read off the forge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LeaseRow {
    pub(crate) updated_at: DateTime<Utc>,
    pub(crate) body: String,
}

/// Parse `gh api --paginate --jq` NDJSON (one `{updated_at, body}` per line —
/// never a `[...]` array, which `--paginate` would corrupt, #4637). Malformed
/// lines are dropped rather than failing the batch.
pub(crate) fn parse_rows(ndjson: &str) -> Vec<LeaseRow> {
    ndjson
        .lines()
        .filter_map(|line| {
            let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
            let updated_at = DateTime::parse_from_rfc3339(v.get("updated_at")?.as_str()?)
                .ok()?
                .with_timezone(&Utc);
            let body = v.get("body")?.as_str()?.to_string();
            Some(LeaseRow { updated_at, body })
        })
        .collect()
}

/// `(host, sweep)` from a yield record's first line:
/// `<!-- loom:lease-yield host=H sweep=S earliest_host=EH earliest_sweep=ES -->`.
fn parse_yield_marker_line(body: &str) -> Option<(String, String)> {
    let rest = body.lines().next()?.strip_prefix(YIELD_MARKER_PREFIX)?;
    let (host, rest) = rest.split_once(" sweep=")?;
    let sweep = rest.split_whitespace().next()?;
    if host.is_empty() || sweep.is_empty() || sweep == "-->" {
        return None;
    }
    Some((host.to_string(), sweep.to_string()))
}

/// Every distinct `(host, sweep)` pair holding a lease on the issue that is
/// within `ttl` of `now` and has not posted a matching `loom:lease-yield`
/// (#5331/#6485: a yielded claimant has already stood down).
pub(crate) fn live_pairs(
    rows: &[LeaseRow],
    now: DateTime<Utc>,
    ttl: chrono::Duration,
) -> BTreeSet<(String, String)> {
    let yielded: BTreeSet<(String, String)> = rows
        .iter()
        .filter_map(|r| parse_yield_marker_line(&r.body))
        .collect();
    rows.iter()
        .filter(|r| now.signed_duration_since(r.updated_at) <= ttl)
        .filter_map(|r| SweepRegistry::parse_lease_marker_line(&r.body))
        .filter(|pair| !yielded.contains(pair))
        .collect()
}

/// Read the issue's lease + lease-yield comments written by authors `policy`
/// trusts (#9631); every other row is dropped before its marker is read.
/// `None` on ANY failure — spawn error, nonzero exit, or the deadline — which
/// the caller treats as "no evidence" (fail open), never as "no marker".
pub(crate) fn read_rows(
    gh_bin: &Path,
    repo: &Path,
    issue: u64,
    timeout: Duration,
    policy: &TrustPolicy,
) -> Option<Vec<LeaseRow>> {
    use loom_daemon::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};
    // Counted as `lease.co_occupancy_comments` (#10089). The facade supplies
    // the cross-owner `GH_CONFIG_DIR` from `repo` (#5401) and the `LOOM_REPO`
    // -> `GH_REPO` override, nulls stdin, and bounds the call by `timeout`.
    let outcome = GhInvocation::new(
        Operation::new("lease.co_occupancy_comments"),
        AccessIntent::Read,
        GhTarget::None,
        timeout,
    )
    .program(gh_bin)
    .arg("api")
    .arg(format!("repos/{{owner}}/{{repo}}/issues/{issue}/comments"))
    .arg("--paginate")
    .arg("--jq")
    .arg(format!(
        r#".[] | select(.body != null and ((.body | startswith("{LEASE_MARKER_PREFIX}")) or (.body | startswith("{YIELD_MARKER_PREFIX}")))) | {{updated_at: .updated_at, body: .body, {AUTHOR_JQ}}}"#
    ))
    .current_dir(repo)
    .run();
    match outcome.ok_output() {
        Some(out) => {
            // #9631: an untrusted (or unattributed) row is prose.
            let trusted = policy.trusted_ndjson(&out.stdout);
            Some(parse_rows(&String::from_utf8_lossy(&trusted)))
        }
        _ => None,
    }
}

/// What the caller should do, and what to tell it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Verdict {
    /// `0` = proceed, `1` = refuse.
    pub(crate) exit_code: i32,
    pub(crate) stderr: Vec<String>,
    pub(crate) stdout: Option<String>,
}

/// The whole decision, with the forge read and the environment injected.
pub(crate) fn decide(
    issue: u64,
    rows: Option<&[LeaseRow]>,
    now: DateTime<Utc>,
    ttl: chrono::Duration,
    allow_shared: bool,
    json: bool,
) -> Verdict {
    let proceed = |stderr| Verdict {
        exit_code: 0,
        stderr,
        stdout: None,
    };
    let Some(rows) = rows else {
        return proceed(Vec::new());
    };
    let live = live_pairs(rows, now, ttl);
    if live.len() < 2 {
        return proceed(Vec::new());
    }
    let mut stderr = vec![format!(
        "WARNING: issue #{issue} carries {} simultaneously FRESH sweep leases -- more than one \
         live worker has claimed this issue at once (rjwalters/kicad-tools#5783).",
        live.len()
    )];
    stderr.extend(
        live.iter()
            .map(|(host, sweep)| format!("WARNING:   live lease: host={host} sweep={sweep}")),
    );
    if allow_shared {
        stderr.push(format!(
            "WARNING: {OVERRIDE_ENV}=1 set - proceeding despite multiple live leases on issue \
             #{issue}."
        ));
        return proceed(stderr);
    }
    let error = format!(
        "Issue #{issue} carries more than one simultaneously fresh sweep lease - refusing to \
         hand back a worktree with uncommitted changes that may belong to a co-occupant. Set \
         {OVERRIDE_ENV}=1 to override."
    );
    let stdout = if json {
        Some(serde_json::json!({ "success": false, "error": error }).to_string())
    } else {
        stderr.push(format!(
            "ERROR: refusing to hand back .loom/worktrees/issue-{issue}: it has uncommitted \
             changes AND the issue carries more than one live sweep lease - it may be \
             co-occupied by another sweep right now."
        ));
        stderr.push(format!(
            "If you are certain this is safe (e.g. a dead peer whose lease has not yet aged past \
             its TTL), re-run with {OVERRIDE_ENV}=1."
        ));
        None
    };
    Verdict {
        exit_code: 1,
        stderr,
        stdout,
    }
}

fn env_parse<T: std::str::FromStr>(key: &str) -> Option<T> {
    std::env::var(key).ok()?.trim().parse().ok()
}

impl LeaseCoOccupancyArgs {
    pub(crate) fn run(self) -> Result<()> {
        let gh_bin = PathBuf::from(std::env::var("LOOM_GH_BIN").unwrap_or_else(|_| "gh".into()));
        let timeout = Duration::from_secs(env_parse(TIMEOUT_ENV).unwrap_or(DEFAULT_TIMEOUT_SECS));
        let ttl_minutes: f64 = env_parse(TTL_ENV)
            .filter(|m: &f64| m.is_finite() && *m >= 0.0)
            .unwrap_or(DEFAULT_TTL_MINUTES);
        let ttl = chrono::Duration::seconds((ttl_minutes * 60.0) as i64);
        let policy = TrustPolicy::for_root(&self.repo);
        let rows = read_rows(&gh_bin, &self.repo, self.issue, timeout, &policy);
        let allow_shared = std::env::var(OVERRIDE_ENV).is_ok_and(|v| v == "1");
        let verdict = decide(self.issue, rows.as_deref(), Utc::now(), ttl, allow_shared, self.json);
        for line in &verdict.stderr {
            eprintln!("{line}");
        }
        if let Some(doc) = &verdict.stdout {
            println!("{doc}");
        }
        std::process::exit(verdict.exit_code);
    }
}

#[cfg(test)]
mod tests;
