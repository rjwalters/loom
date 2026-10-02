//! `loom-daemon forge check-claim <issue>` — the aggregated pre-flight
//! claim-CAS probe (issue #9453 Phase 1).
//!
//! # Why this exists
//!
//! `forge check-open-pr` (#8551) answered exactly one of the four questions a
//! claimant must ask before taking an issue. Between 2026-09-29 and
//! 2026-09-30, four cross-lane races (#9430, #9432, #9638, #9586 — a
//! hand-claim lane and the daemon fleet building the same epic at the same
//! time) each burned a duplicate PR because *some* signal was consulted and
//! *another* was not: an open PR, a `loom:building` label, a fresh
//! `loom:lease` comment, or a remote `feature/issue-N` branch. This
//! subcommand aggregates all four into one "may I claim issue N **right
//! now**?" probe, cheapest-first, short-circuiting on the first blocker, so
//! every lane — hand-claim, in-session sweep, spawn script — asks the same
//! four questions through the same code.
//!
//! # One code path per leg, not a second query
//!
//! - Leg 1 (open linked PR) is [`crate::worktree_ops::gh::probe_open_linked_pr`]
//!   — the same closes-graph ∪ timeline union `forge check-open-pr`, the
//!   registry dispatch guard, and orphan recovery use (#4123/#8551/#8940).
//! - Leg 2 (claim label) is the same `gh issue view --json labels` read the
//!   dispatch-side collision guard takes, classified by
//!   [`crate::sweep_registry::preflip_labels`]'s `CLAIM_LABELS` (#4085/#7873)
//!   — with `url` requested alongside `labels`, which costs no extra call and
//!   is what answers the target-kind question below (#9929).
//! - Leg 3 (fresh lease) reads the same `<!-- loom:lease … -->` comments
//!   [`crate::claim_reconciliation::forge::fetch_freshest_lease_updated_at`]
//!   reads, against the same `LOOM_LEASE_TTL_MINUTES` TTL (#6179/#6286), with
//!   the same trust filter (#9548) and the same yield exclusion the publish
//!   path applies (#5331/#6485). It re-walks the raw rows rather than calling
//!   that function because the `LEASE_ALREADY_HELD` token must name the
//!   holder's `<host> <sweep-id>` and skip already-yielded claimants —
//!   facts that function's `LeaseProbe` timestamp-only answer does not carry.
//! - Leg 4 (remote branch) is one `git ls-remote --heads origin
//!   feature/issue-N` — zero forge calls (#9447's `-install-merge` suffix
//!   improvisation happened precisely because nothing probed this).
//!
//! # Exit-code contract (the whole public surface)
//!
//! | Exit | Meaning | stdout | What the caller must do |
//! |---|---|---|---|
//! | `0` | BLOCKED — reason token: `OPEN_PR #X` / `TARGET_IS_PR #N` / `BUILDING` / `LEASE_ALREADY_HELD <host> <sweep-id>` / `BRANCH_EXISTS feature/issue-N` | the token | **Hard-abort the claim.** |
//! | [`EX_SAFE_TO_CLAIM`] (1) | verified safe to claim | empty | Claim. |
//! | [`EX_PROBE_FAILED`] (5) | no verdict — any leg's read failed | empty | **Fail closed.** Treat as blocked; check by hand. |
//! | [`EX_FORGE_DECLINED`] (3) | Gitea — legs 1–3 are GitHub-only | empty | Check by hand. |
//!
//! `0` is the blocked state on purpose (mirroring `check-open-pr`'s
//! 0-is-captured convention): the *unsafe* state is the one a bare `if` fires
//! on. Every non-zero code other than exactly `1` means **the question was
//! not answered**, which a caller must treat as blocked — never as an
//! all-clear.
//!
//! `--force-claim` overrides legs 2–4 ONLY. It must **never** override the
//! `OPEN_PR` leg: an open linked PR is someone's *submitted work*, not a
//! claim race — and a leg-1 read failure under `--force-claim` still exits
//! [`EX_PROBE_FAILED`], because the one leg that cannot be overridden was
//! never verified. Overridden blockers (and overridden read failures) are
//! reported on stderr as warnings while the probe still answers "safe".
//!
//! `TARGET_IS_PR` is the second never-overridable blocker (Issue #9929). It
//! is not a claim race at all but a **category error**: the target number is
//! a pull request, so there is no issue here to claim and `--force-claim`
//! cannot make one exist. It falls out of leg 2 for free — `gh issue view
//! <n> --json labels,url` serves a PR as happily as an issue (issues and PRs
//! share one number namespace and the `/issues/{n}` resource), and the `url`
//! it returns is the only thing in the whole claim walk that says which kind
//! the target actually is. Without this leg, every downstream step accepts a
//! PR number silently: `gh issue edit <pr> --add-label loom:building`
//! succeeds, and the issue lifecycle proceeds on something that can never
//! close. That is the #9929 incident's signature — a pull request carrying
//! `loom:curating` → `loom:curated`, stranded outside both pipelines.
//!
//! # Cost
//!
//! ≤3 forge reads + 1 `git ls-remote` per invocation, short-circuited on the
//! first blocker — the budget issue #9453's design fixes for this probe.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

use crate::comment_trust::{records::AUTHOR_JQ, TrustPolicy};
use crate::forge_cmd::{detect_forge, ForgeType, EX_FORGE_DECLINED};
use crate::sweep_registry::preflip_labels::claim_labels_in;
use crate::sweep_registry::{SweepRegistry, LEASE_MARKER_PREFIX};
use crate::worktree_ops::gh::{probe_open_linked_pr, OpenPrProbe};

/// Exit code for a **verified** "safe to claim" — the only all-clear.
/// Deliberately the same value `forge check-open-pr` uses for its verified
/// absence, so a caller migrating between the two keeps its `if` arms.
pub const EX_SAFE_TO_CLAIM: i32 = 1;

/// Exit code for "some leg's read could not be answered" — fail CLOSED.
/// Same value and meaning as `crate::forge_check_open_pr::EX_PROBE_FAILED`.
pub const EX_PROBE_FAILED: i32 = 5;

/// First-line prefix of a `loom:lease-yield` stand-down record (#6287) —
/// a claimant whose `(host, sweep)` posted one has already stood down, so
/// its still-"fresh" lease must not block leg 3 (#5331/#6485, mirroring
/// `sweep-lease-publish.sh`'s publish-side exclusion).
const YIELD_MARKER_PREFIX: &str = "<!-- loom:lease-yield host=";

/// Leg 2's reading: the issue's current label snapshot, classified by the
/// same `CLAIM_LABELS` predicate (`loom:building`/`loom:reviewing`/
/// `loom:treating`) the dispatch collision guard uses (#4085/#7873) — plus
/// the one answer only this leg's read can give: whether the target number
/// is an **issue at all** ([`Self::IsPullRequest`], #9929).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LabelLeg {
    /// The target number is a **pull request**, not an issue — the `url` the
    /// same read returns points at `/pull/<n>`. A category error, not a
    /// claim race: never overridable by `--force-claim` (#9929).
    IsPullRequest,
    /// A claim label (`loom:building`/`loom:reviewing`/`loom:treating`) is
    /// present — someone already took this issue. Carries the observed
    /// claim label(s) for the refusal text.
    Claimed(Vec<String>),
    /// Read succeeded; no claim label present.
    Clean,
    /// The label read failed (gh missing/failed/unparseable) — NOT evidence
    /// of a clean snapshot.
    Unknown,
}

/// Leg 3's reading: the freshest live (fresh, non-yielded, trusted) lease
/// record on the issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LeaseLeg {
    /// A fresh, non-yielded lease exists — a live worker holds this claim.
    Held { host: String, sweep_id: String },
    /// Read succeeded; no fresh non-yielded lease (absent, stale, or all
    /// yielded).
    NoneFresh,
    /// The comments read failed — NOT evidence of an absent lease (#7591's
    /// distinction, mirrored from `LeaseProbe::ReadFailed`).
    ReadFailed,
}

/// Leg 4's reading: does `feature/issue-N` exist on `origin`?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BranchLeg {
    /// `git ls-remote` answered and printed the ref.
    Exists,
    /// `git ls-remote` answered with no matching ref.
    Absent,
    /// The `git ls-remote` call itself failed (no network, no remote) — NOT
    /// a verified absence.
    ProbeFailed,
}

/// The four legs' evidence. Legs are collected cheapest-first and only up to
/// the first leg that ends the walk (a blocker, or an unreadable leg when
/// `--force-claim` was not passed), so `None` means "not consulted" —
/// [`decide`] never reads past the leg that already decided the verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClaimEvidence {
    pub(crate) open_pr: OpenPrProbe,
    pub(crate) labels: Option<LabelLeg>,
    pub(crate) lease: Option<LeaseLeg>,
    pub(crate) branch: Option<BranchLeg>,
}

/// What the CLI prints and exits with for one probe verdict. Split out from
/// [`handle`] so the contract is unit-testable without a live `gh` — the
/// same seam `crate::forge_check_open_pr::Verdict` provides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// Process exit status.
    pub code: i32,
    /// Machine-readable stdout (the reason token, or empty).
    pub stdout: String,
    /// Human-readable explanation, always non-empty.
    pub stderr: String,
}

/// The Gitea decline (exit [`EX_FORGE_DECLINED`]) as a [`Verdict`], so the
/// "check by hand" wording is testable without a Gitea checkout. Legs 1–3
/// are GitHub APIs; leg 4 alone cannot answer the claim question, so the
/// whole probe declines rather than report a partial all-clear.
#[must_use]
pub fn gitea_declined(issue: u32) -> Verdict {
    Verdict {
        code: EX_FORGE_DECLINED,
        stdout: String::new(),
        stderr: format!(
            "loom-daemon forge check-claim: legs 1-3 of the claim probe are GitHub-only; \
             on Gitea, check issue #{issue} for an open PR, claim labels, and a fresh lease \
             by hand before claiming it."
        ),
    }
}

/// A blocked verdict's helper: exit `0`, the reason token on stdout, the
/// human explanation on stderr.
fn blocked(issue: u32, token: &str, detail: String) -> Verdict {
    Verdict {
        code: 0,
        stdout: token.to_string(),
        stderr: format!("issue #{issue} is NOT safe to claim: {detail} Hard-abort the claim."),
    }
}

/// A fail-closed verdict's helper: exit [`EX_PROBE_FAILED`], nothing on
/// stdout. The wording must never read as an absence — the failure mode this
/// command exists to prevent is exactly an agent concluding "nothing in
/// flight" from a question that was never answered.
fn unanswered(issue: u32, detail: &str) -> Verdict {
    Verdict {
        code: EX_PROBE_FAILED,
        stdout: String::new(),
        stderr: format!(
            "could not answer the claim question for issue #{issue}: {detail}. This is NOT \
             an all-clear — fail closed and treat the issue as possibly already claimed; \
             check it by hand."
        ),
    }
}

/// Render the aggregated verdict. Pure: every forge read stays in the leg
/// readers below, and the walk order (open PR → label → lease → branch,
/// first blocker wins) is the order the design fixes.
///
/// `--force-claim` overrides legs 2–4 only: their blockers and read
/// failures become stderr warnings and the walk continues, but the `OPEN_PR`
/// leg blocks unconditionally, and an unreadable leg 1 fails closed even
/// when forced (the never-overridable leg was never verified).
#[must_use]
pub(crate) fn decide(issue: u32, evidence: &ClaimEvidence, force_claim: bool) -> Verdict {
    // Leg 1 — open linked PR. Never overridable (#9453: an open PR is
    // someone's submitted work, not a claim race).
    match evidence.open_pr {
        OpenPrProbe::Open(pr) => {
            return blocked(
                issue,
                &format!("OPEN_PR #{pr}"),
                format!(
                    "an open linked PR (#{pr}) already exists — the work is already in \
                     flight (#4123), and --force-claim never overrides this leg."
                ),
            );
        }
        OpenPrProbe::ProbeFailed => {
            return unanswered(
                issue,
                "the open-linked-PR leg could not read the forge (gh missing, failed, \
                 rate-limited, or the repository could not be resolved) — and --force-claim \
                 cannot override a leg it is forbidden to override",
            );
        }
        OpenPrProbe::NoneOpen => {}
    }

    // Legs 2–4 — overridable by --force-claim, each blocker/unreadable leg
    // recorded as a warning when overridden so the caller sees exactly what
    // it forced past.
    let mut forced_past: Vec<String> = Vec::new();
    let mut stop: Option<Verdict> = None;
    let overridable = |detail: String, token: Option<&str>, forced_past: &mut Vec<String>| {
        if force_claim {
            forced_past.push(match token {
                Some(t) => t.to_string(),
                None => detail.clone(),
            });
            None
        } else if let Some(token) = token {
            Some(blocked(issue, token, detail))
        } else {
            Some(unanswered(issue, &detail))
        }
    };

    if stop.is_none() {
        if let Some(labels) = &evidence.labels {
            match labels {
                // Never overridable (#9929): a PR number is not a claimable
                // issue under any flag. Returned directly rather than through
                // `overridable`, which is reserved for legs --force-claim may
                // buy past.
                LabelLeg::IsPullRequest => {
                    return blocked(
                        issue,
                        &format!("TARGET_IS_PR #{issue}"),
                        format!(
                            "that number is a pull request, not an issue — issues and PRs share one \
                             number namespace, so `gh issue view`/`gh issue edit` accept it \
                             silently and the issue lifecycle would proceed on something that \
                             can never close (#9929). --force-claim never overrides this leg: \
                             there is no issue here to claim."
                        ),
                    );
                }
                LabelLeg::Claimed(claims) => {
                    stop = overridable(
                        format!(
                            "claim label(s) [{}] already on the issue — another agent took it \
                             (the same labels the dispatch collision guard refuses on)",
                            claims.join(", ")
                        ),
                        Some("BUILDING"),
                        &mut forced_past,
                    );
                }
                LabelLeg::Unknown => {
                    stop = overridable(
                        "the label read failed (gh missing, failed, or unparseable)".to_string(),
                        None,
                        &mut forced_past,
                    );
                }
                LabelLeg::Clean => {}
            }
        }
    }

    if stop.is_none() {
        if let Some(lease) = &evidence.lease {
            match lease {
                LeaseLeg::Held { host, sweep_id } => {
                    stop = overridable(
                        format!(
                            "a fresh lease record from host={host} sweep={sweep_id} is being \
                             renewed within the TTL — a live worker holds this claim \
                             (#6179/#6286)"
                        ),
                        Some(&format!("LEASE_ALREADY_HELD {host} {sweep_id}")),
                        &mut forced_past,
                    );
                }
                LeaseLeg::ReadFailed => {
                    stop = overridable(
                        "the lease-comment read failed (gh missing, failed, or unparseable) — \
                         unverifiable is not absent (#7591)"
                            .to_string(),
                        None,
                        &mut forced_past,
                    );
                }
                LeaseLeg::NoneFresh => {}
            }
        }
    }

    if stop.is_none() {
        if let Some(branch) = evidence.branch {
            match branch {
                BranchLeg::Exists => {
                    let branch_name = format!("feature/issue-{issue}");
                    stop = overridable(
                        format!(
                            "the remote branch {branch_name} already exists on origin — a \
                             prior or racing claimant pushed it; never create a suffix branch \
                             past it (#9447)"
                        ),
                        Some(&format!("BRANCH_EXISTS {branch_name}")),
                        &mut forced_past,
                    );
                }
                BranchLeg::ProbeFailed => {
                    stop = overridable(
                        "git ls-remote could not answer whether the remote branch exists \
                         (network, credentials, or no origin remote)"
                            .to_string(),
                        None,
                        &mut forced_past,
                    );
                }
                BranchLeg::Absent => {}
            }
        }
    }

    if let Some(v) = stop {
        return v;
    }

    let mut stderr = if forced_past.is_empty() {
        String::new()
    } else {
        format!("--force-claim overriding: {}; ", forced_past.join("; "))
    };
    stderr.push_str(&format!(
        "issue #{issue} is safe to claim: no open linked PR, no claim label, no fresh lease, \
         and no remote feature/issue-{issue} branch."
    ));
    Verdict {
        code: EX_SAFE_TO_CLAIM,
        stdout: String::new(),
        stderr,
    }
}

/// Does a forge item URL name a **pull request** rather than an issue (#9929)?
///
/// Matches GitHub web (`…/owner/repo/pull/306`), GitHub REST
/// (`…/repos/owner/repo/pulls/306`) and Gitea (`…/owner/repo/pulls/306`), by
/// requiring the *path segment immediately before the number* to be
/// `pull`/`pulls` — not a bare substring search, which would misread a
/// repository literally named `pull` (`…/me/pull/issues/5`) as a PR.
#[must_use]
pub(crate) fn url_is_pull_request(url: &str) -> bool {
    let path = url.split_once("://").map_or(url, |(_, rest)| rest);
    let mut segments = path.split('/').filter(|s| !s.is_empty()).rev();
    let Some(number) = segments.next() else {
        return false;
    };
    if number.is_empty() || !number.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    matches!(segments.next(), Some("pull" | "pulls"))
}

// ---------------------------------------------------------------------------
// Leg readers (all I/O; every decision above them is pure)
// ---------------------------------------------------------------------------

/// Leg 2: read the issue's current labels and classify them by the same
/// [`CLAIM_LABELS`] predicate the dispatch collision guard uses
/// (`SweepRegistry::classify_preflip_labels` — same `gh issue view --json
/// labels` call, same fail-closed `Unknown` on any unreadable answer).
///
/// Also answers leg 2's second question (#9929): `url` is requested alongside
/// `labels` — one field on the read that was already being made, zero extra
/// forge calls — and a `/pull/` path means the target is a pull request, not
/// an issue ([`LabelLeg::IsPullRequest`]). `gh issue view` serves a PR number
/// without complaint, so this is the only point in the claim walk where the
/// target's *kind* is observable at all.
pub(crate) fn read_claim_labels(gh_bin: &Path, root: &Path, issue: u32) -> LabelLeg {
    // #10089: counted via the facade (`claim.labels`); it supplies the #5401
    // cross-owner GH_CONFIG_DIR from `root`. `gh issue view` takes --repo
    // (unlike `gh api`, #8263).
    let inv = crate::claim_reconciliation::gh_call::read("claim.labels", gh_bin, root)
        .args(["issue", "view", &issue.to_string(), "--json", "labels,url"])
        .args(crate::claim_reconciliation::gh_call::loom_repo_flag());
    let Ok(out) = crate::claim_reconciliation::gh_call::output(inv) else {
        return LabelLeg::Unknown;
    };
    if !out.status.success() {
        return LabelLeg::Unknown;
    }
    // `gh issue view --json labels,url` emits
    // `{"labels":[{"name":"..."},…],"url":"https://…/issues/42"}`.
    let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
        return LabelLeg::Unknown;
    };
    // Kind before claim state (#9929): on a PR, the label snapshot is a true
    // read of the wrong object, so there is nothing to classify.
    if let Some(url) = parsed.get("url").and_then(|u| u.as_str()) {
        if url_is_pull_request(url) {
            return LabelLeg::IsPullRequest;
        }
    }
    // An absent/non-string `url` is NOT evidence of a PR, and does not make
    // the *label* question unanswerable either — `gh` always returns a
    // requested field, so this only arises against a stub, where the
    // pre-#9929 label-only behavior is the right fallback.
    let Some(arr) = parsed.get("labels").and_then(|l| l.as_array()) else {
        return LabelLeg::Unknown;
    };
    let labels: Vec<String> = arr
        .iter()
        .filter_map(|l| l.get("name").and_then(|n| n.as_str()).map(String::from))
        .collect();
    let claims = claim_labels_in(&labels);
    if claims.is_empty() {
        LabelLeg::Clean
    } else {
        LabelLeg::Claimed(claims)
    }
}

/// One lease or lease-yield comment row as read off the forge (NDJSON).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LeaseRow {
    pub(crate) updated_at: DateTime<Utc>,
    pub(crate) body: String,
}

/// Parse the `gh api --paginate --jq` NDJSON of leg 3's read (one
/// `{updated_at, body}` object per line — never a `[...]` array, which
/// `--paginate` would corrupt, #4637). Malformed lines are dropped rather
/// than failing the batch, mirroring `SweepRegistry::parse_lease_comments_json`.
#[must_use]
pub(crate) fn parse_lease_rows(ndjson: &str) -> Vec<LeaseRow> {
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
/// Same shape as the publish/fence scripts' parser and
/// `cli/lease_co_occupancy`'s — only the first line is inspected, and never
/// the free-form prose after the closing `-->` (`lease-record.md`).
fn parse_yield_marker_line(body: &str) -> Option<(String, String)> {
    let rest = body.lines().next()?.strip_prefix(YIELD_MARKER_PREFIX)?;
    let (host, rest) = rest.split_once(" sweep=")?;
    let sweep = rest.split_whitespace().next()?;
    if host.is_empty() || sweep.is_empty() || sweep == "-->" {
        return None;
    }
    Some((host.to_string(), sweep.to_string()))
}

/// The freshest live lease among `rows`: within `ttl_minutes` of `now`
/// ([`crate::claim_reconciliation::lease_is_fresh`]'s TTL semantics —
/// `LOOM_LEASE_TTL_MINUTES`, default 15), trusted (the caller already
/// filtered), and not excluded by a matching `loom:lease-yield` (#5331/
/// #6485: a yielded claimant already stood down). Pure.
#[must_use]
pub(crate) fn freshest_live_lease(
    rows: &[LeaseRow],
    now: DateTime<Utc>,
    ttl_minutes: f64,
) -> Option<(String, String)> {
    let yielded: std::collections::BTreeSet<(String, String)> = rows
        .iter()
        .filter_map(|r| parse_yield_marker_line(&r.body))
        .collect();
    rows.iter()
        .filter(|r| crate::claim_reconciliation::lease_is_fresh(r.updated_at, now, ttl_minutes))
        .filter_map(|r| {
            SweepRegistry::parse_lease_marker_line(&r.body).map(|pair| (r.updated_at, pair))
        })
        .filter(|(_, pair)| !yielded.contains(pair))
        .max_by_key(|(updated_at, _)| *updated_at)
        .map(|(_, pair)| pair)
}

/// Leg 3: read the issue's lease and lease-yield comments (one round trip,
/// both marker shapes — the same combined read `sweep-lease-publish.sh`
/// makes), keep only trusted authors' rows (#9548/#9631), and report the
/// freshest live lease. TTL comes from
/// [`crate::claim_reconciliation::resolve_lease_ttl_minutes`] at the caller.
pub(crate) fn read_freshest_live_lease(
    gh_bin: &Path,
    root: &Path,
    issue: u32,
    now: DateTime<Utc>,
    ttl_minutes: f64,
) -> LeaseLeg {
    // #10089: counted via the facade (`claim.lease_comments`); it applies the
    // #5401 GH_CONFIG_DIR and the #8263 `LOOM_REPO` -> GH_REPO env contract.
    let jq = format!(
        r#".[] | select(.body != null and ((.body | startswith("{LEASE_MARKER_PREFIX}")) or (.body | startswith("{YIELD_MARKER_PREFIX}")))) | {{updated_at: .updated_at, body: .body, {AUTHOR_JQ}}}"#
    );
    let inv = crate::claim_reconciliation::gh_call::read("claim.lease_comments", gh_bin, root)
        .args([
            "api",
            &format!("repos/{{owner}}/{{repo}}/issues/{issue}/comments"),
            "--paginate",
            "--jq",
            &jq,
        ]);
    let Ok(out) = crate::claim_reconciliation::gh_call::output(inv) else {
        return LeaseLeg::ReadFailed;
    };
    if !out.status.success() {
        return LeaseLeg::ReadFailed;
    }
    // #9548/#9631: an untrusted (or unattributed) row is prose — it can
    // neither block a claim here nor excuse a peer's live lease.
    let policy = TrustPolicy::for_root(root);
    let rows = parse_lease_rows(&String::from_utf8_lossy(&policy.trusted_ndjson(&out.stdout)));
    match freshest_live_lease(&rows, now, ttl_minutes) {
        Some((host, sweep_id)) => LeaseLeg::Held { host, sweep_id },
        None => LeaseLeg::NoneFresh,
    }
}

/// Classify one `git ls-remote --heads origin feature/issue-N` result. Pure.
#[must_use]
pub(crate) fn classify_ls_remote(success: bool, stdout: &str) -> BranchLeg {
    if !success {
        return BranchLeg::ProbeFailed;
    }
    if stdout.trim().is_empty() {
        BranchLeg::Absent
    } else {
        BranchLeg::Exists
    }
}

/// Leg 4: does `feature/issue-N` exist on `origin`? Zero forge calls (git
/// wire protocol). An empty answer is a verified absence; a failed call is
/// [`BranchLeg::ProbeFailed`], never an absence.
pub(crate) fn remote_branch_leg(git_bin: &Path, root: &Path, issue: u32) -> BranchLeg {
    let mut cmd = std::process::Command::new(git_bin);
    cmd.arg("ls-remote")
        .arg("--heads")
        .arg("origin")
        .arg(format!("feature/issue-{issue}"))
        .current_dir(root);
    match cmd.output() {
        Ok(out) => classify_ls_remote(out.status.success(), &String::from_utf8_lossy(&out.stdout)),
        Err(_) => BranchLeg::ProbeFailed,
    }
}

/// Handle `loom-daemon forge check-claim <issue>`. Never returns (exits the
/// process with the code from [`decide`]); returns `Err` only when the
/// current directory cannot be resolved.
///
/// Resolution is cwd-scoped, like `forge check-open-pr`: run it from inside
/// the repository (a managed worktree included) and it answers about that
/// repository. `LOOM_REPO` is honored per leg exactly as each leg's
/// precedent reader honors it.
pub fn handle(issue: u32, force_claim: bool) -> Result<()> {
    let root: PathBuf = std::env::current_dir().context(
        "loom-daemon forge check-claim: could not resolve the current directory; \
         run it from inside the repository whose issue you are about to claim",
    )?;

    // GitHub-only by construction: legs 1-3 are GitHub APIs. Declining is
    // honest; a partial probe (leg 4 alone) reporting "safe to claim" would
    // be the exact false all-clear this command exists to prevent.
    if detect_forge(Some(&root)) == ForgeType::Gitea {
        let v = gitea_declined(issue);
        eprintln!("{}", v.stderr);
        std::process::exit(v.code);
    }

    let gh_bin = PathBuf::from(std::env::var("LOOM_GH_BIN").unwrap_or_else(|_| "gh".into()));
    let now = Utc::now();
    let ttl_minutes = crate::claim_reconciliation::resolve_lease_ttl_minutes();

    // Cheapest-first with short-circuit: each later leg is consulted only
    // when every earlier leg is Clear (or, under --force-claim, when its
    // blocker/unreadable answer cannot change the verdict anyway — the walk
    // keeps collecting so the stderr warning names everything overridden).
    let open_pr = probe_open_linked_pr(&root, issue);
    let mut evidence = ClaimEvidence {
        open_pr,
        labels: None,
        lease: None,
        branch: None,
    };
    if matches!(evidence.open_pr, OpenPrProbe::NoneOpen) {
        evidence.labels = Some(read_claim_labels(&gh_bin, &root, issue));
        let labels_clear = matches!(evidence.labels, Some(LabelLeg::Clean));
        if labels_clear || force_claim {
            evidence.lease =
                Some(read_freshest_live_lease(&gh_bin, &root, issue, now, ttl_minutes));
            let lease_clear = matches!(evidence.lease, Some(LeaseLeg::NoneFresh));
            if lease_clear || force_claim {
                evidence.branch = Some(remote_branch_leg(Path::new("git"), &root, issue));
            }
        }
    }

    let v = decide(issue, &evidence, force_claim);
    if !v.stdout.is_empty() {
        println!("{}", v.stdout);
    }
    eprintln!("{}", v.stderr);
    std::process::exit(v.code);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn evidence(
        open_pr: OpenPrProbe,
        labels: Option<LabelLeg>,
        lease: Option<LeaseLeg>,
        branch: Option<BranchLeg>,
    ) -> ClaimEvidence {
        ClaimEvidence {
            open_pr,
            labels,
            lease,
            branch,
        }
    }

    // --- Leg 1: OPEN_PR -----------------------------------------------------

    #[test]
    fn open_pr_blocker_is_exit_zero_with_the_open_pr_token() {
        let v = decide(8413, &evidence(OpenPrProbe::Open(8462), None, None, None), false);
        assert_eq!(v.code, 0);
        assert_eq!(v.stdout, "OPEN_PR #8462");
        assert!(v.stderr.contains("Hard-abort"), "{}", v.stderr);
        assert!(v.stderr.contains("#8462"), "{}", v.stderr);
    }

    /// The never-override rule: --force-claim must NEVER buy past an open
    /// linked PR — it is someone's submitted work, not a claim race (#9453).
    #[test]
    fn open_pr_blocker_is_never_overridable_by_force_claim() {
        let v = decide(8413, &evidence(OpenPrProbe::Open(8462), None, None, None), true);
        assert_eq!(v.code, 0, "--force-claim may not override the OPEN_PR leg");
        assert_eq!(v.stdout, "OPEN_PR #8462");
        assert!(v.stderr.contains("never overrides"), "{}", v.stderr);
    }

    /// An unanswerable leg 1 fails closed EVEN under --force-claim: the one
    /// leg the flag is forbidden to override was never verified.
    #[test]
    fn open_pr_read_failure_fails_closed_even_when_forced() {
        let v = decide(8413, &evidence(OpenPrProbe::ProbeFailed, None, None, None), true);
        assert_eq!(v.code, EX_PROBE_FAILED);
        assert!(v.stdout.is_empty(), "{:?}", v.stdout);
        assert!(v.stderr.contains("NOT an all-clear"), "{}", v.stderr);
        assert!(
            !v.stderr.contains("safe to claim"),
            "a failed leg must never read as an all-clear: {}",
            v.stderr
        );
    }

    // --- Leg 2: BUILDING ----------------------------------------------------

    #[test]
    fn claim_label_blocker_is_exit_zero_with_the_building_token() {
        for label in ["loom:building", "loom:reviewing", "loom:treating"] {
            let v = decide(
                42,
                &evidence(
                    OpenPrProbe::NoneOpen,
                    Some(LabelLeg::Claimed(vec![label.to_string()])),
                    None,
                    None,
                ),
                false,
            );
            assert_eq!(v.code, 0, "{label}");
            assert_eq!(v.stdout, "BUILDING", "{label}");
            assert!(v.stderr.contains(label), "{}", v.stderr);
        }
    }

    #[test]
    fn label_read_failure_fails_closed() {
        let v = decide(
            42,
            &evidence(OpenPrProbe::NoneOpen, Some(LabelLeg::Unknown), None, None),
            false,
        );
        assert_eq!(v.code, EX_PROBE_FAILED);
        assert!(v.stdout.is_empty());
        assert!(v.stderr.contains("NOT an all-clear"), "{}", v.stderr);
    }

    // --- Leg 3: LEASE_ALREADY_HELD ------------------------------------------

    #[test]
    fn fresh_lease_blocker_is_exit_zero_with_the_lease_already_held_token() {
        let v = decide(
            9447,
            &evidence(
                OpenPrProbe::NoneOpen,
                Some(LabelLeg::Clean),
                Some(LeaseLeg::Held {
                    host: "host-d9142cf3".to_string(),
                    sweep_id: "sweep-insession-20260929T034321Z-84090".to_string(),
                }),
                None,
            ),
            false,
        );
        assert_eq!(v.code, 0);
        assert_eq!(
            v.stdout,
            "LEASE_ALREADY_HELD host-d9142cf3 sweep-insession-20260929T034321Z-84090"
        );
        assert!(v.stderr.contains("host-d9142cf3"), "{}", v.stderr);
    }

    #[test]
    fn lease_read_failure_fails_closed() {
        let v = decide(
            42,
            &evidence(
                OpenPrProbe::NoneOpen,
                Some(LabelLeg::Clean),
                Some(LeaseLeg::ReadFailed),
                None,
            ),
            false,
        );
        assert_eq!(v.code, EX_PROBE_FAILED);
        assert!(v.stdout.is_empty());
        assert!(v.stderr.contains("unverifiable is not absent"), "{}", v.stderr);
    }

    // --- Leg 4: BRANCH_EXISTS ------------------------------------------------

    #[test]
    fn branch_blocker_is_exit_zero_with_the_branch_exists_token() {
        let v = decide(
            9447,
            &evidence(
                OpenPrProbe::NoneOpen,
                Some(LabelLeg::Clean),
                Some(LeaseLeg::NoneFresh),
                Some(BranchLeg::Exists),
            ),
            false,
        );
        assert_eq!(v.code, 0);
        assert_eq!(v.stdout, "BRANCH_EXISTS feature/issue-9447");
        assert!(v.stderr.contains("feature/issue-9447"), "{}", v.stderr);
    }

    #[test]
    fn branch_probe_failure_fails_closed() {
        let v = decide(
            42,
            &evidence(
                OpenPrProbe::NoneOpen,
                Some(LabelLeg::Clean),
                Some(LeaseLeg::NoneFresh),
                Some(BranchLeg::ProbeFailed),
            ),
            false,
        );
        assert_eq!(v.code, EX_PROBE_FAILED);
        assert!(v.stdout.is_empty());
    }

    // --- The clean path ------------------------------------------------------

    #[test]
    fn clean_issue_is_exit_one_with_empty_stdout() {
        let v = decide(
            42,
            &evidence(
                OpenPrProbe::NoneOpen,
                Some(LabelLeg::Clean),
                Some(LeaseLeg::NoneFresh),
                Some(BranchLeg::Absent),
            ),
            false,
        );
        assert_eq!(v.code, EX_SAFE_TO_CLAIM);
        assert!(v.stdout.is_empty(), "{:?}", v.stdout);
        assert!(v.stderr.contains("safe to claim"), "{}", v.stderr);
    }

    // --- --force-claim -------------------------------------------------------

    #[test]
    fn force_claim_overrides_legs_two_through_four_but_reports_them() {
        for evidence in [
            evidence(
                OpenPrProbe::NoneOpen,
                Some(LabelLeg::Claimed(vec!["loom:building".to_string()])),
                None,
                None,
            ),
            evidence(
                OpenPrProbe::NoneOpen,
                Some(LabelLeg::Clean),
                Some(LeaseLeg::Held {
                    host: "peer-host".to_string(),
                    sweep_id: "sweep-x".to_string(),
                }),
                None,
            ),
            evidence(
                OpenPrProbe::NoneOpen,
                Some(LabelLeg::Clean),
                Some(LeaseLeg::NoneFresh),
                Some(BranchLeg::Exists),
            ),
            // Unreadable legs 2-4 are equally overridden, not fatal.
            evidence(OpenPrProbe::NoneOpen, Some(LabelLeg::Unknown), None, None),
        ] {
            let v = decide(42, &evidence, true);
            assert_eq!(v.code, EX_SAFE_TO_CLAIM, "{evidence:?}");
            assert!(v.stdout.is_empty(), "{:?} -> {:?}", evidence, v.stdout);
            assert!(
                v.stderr.contains("--force-claim overriding"),
                "the override must be visible on stderr: {}",
                v.stderr
            );
            assert!(v.stderr.contains("safe to claim"), "{}", v.stderr);
        }
    }

    // --- The code surface ----------------------------------------------------

    #[test]
    fn exit_codes_are_mutually_distinct() {
        let codes = [
            decide(1, &evidence(OpenPrProbe::Open(2), None, None, None), false).code,
            decide(
                1,
                &evidence(
                    OpenPrProbe::NoneOpen,
                    Some(LabelLeg::Clean),
                    Some(LeaseLeg::NoneFresh),
                    Some(BranchLeg::Absent),
                ),
                false,
            )
            .code,
            decide(1, &evidence(OpenPrProbe::ProbeFailed, None, None, None), false).code,
            gitea_declined(1).code,
        ];
        let mut sorted = codes.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 4, "{codes:?}");
        assert!(!codes.contains(&crate::forge_cmd::EX_FORGE_HEAD_MISMATCH), "{codes:?}");
    }

    #[test]
    fn gitea_decline_is_exit_three_with_empty_stdout() {
        let v = gitea_declined(42);
        assert_eq!(v.code, EX_FORGE_DECLINED);
        assert!(v.stdout.is_empty(), "{:?}", v.stdout);
        assert!(v.stderr.contains("by hand"), "{}", v.stderr);
        assert!(
            !v.stderr.contains("safe to claim"),
            "a decline must never read as an all-clear: {}",
            v.stderr
        );
    }

    // --- Leg-reader plumbing (pure halves) -----------------------------------

    #[test]
    fn parse_lease_rows_drops_malformed_lines_and_keeps_good_ones() {
        let fleet = crate::comment_trust::records::TEST_FLEET_AUTHOR;
        let ndjson = format!(
            "{{\"updated_at\":\"2026-09-30T03:43:22Z\",\"body\":\"<!-- loom:lease host=h sweep=s -->\",{fleet}}}\n\
             not-json\n\
             \n\
             {{\"updated_at\":\"garbage\",\"body\":\"x\",{fleet}}}\n"
        );
        let rows = parse_lease_rows(&ndjson);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(rows[0].body.starts_with(LEASE_MARKER_PREFIX));
    }

    #[test]
    fn freshest_live_lease_picks_the_freshest_non_yielded_fresh_lease() {
        let now = DateTime::parse_from_rfc3339("2026-09-30T04:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let ttl = 15.0;
        let mk = |mins_ago, host, sweep, id, yield_rec| {
            let ts = now - chrono::Duration::minutes(mins_ago);
            let marker = if yield_rec {
                format!("<!-- loom:lease-yield host={host} sweep={sweep} earliest_host=other -->")
            } else {
                format!("<!-- loom:lease host={host} sweep={sweep} -->")
            };
            LeaseRow {
                updated_at: ts,
                body: format!("{marker}\nrenewal record (comment id {id})"),
            }
        };
        // Stale (past TTL), fresh yielded, fresh live, freshest live.
        let rows = vec![
            mk(20, "old-host", "old-sweep", 1, false),
            mk(3, "yielded-host", "yielded-sweep", 2, true),
            mk(10, "live-host", "live-sweep", 3, false),
            mk(2, "freshest-host", "freshest-sweep", 4, false),
        ];
        assert_eq!(
            freshest_live_lease(&rows, now, ttl),
            Some(("freshest-host".to_string(), "freshest-sweep".to_string()))
        );

        // The yielded row alone: not a live lease even though fresh.
        let yielded_only = vec![mk(1, "y-host", "y-sweep", 5, true)];
        assert_eq!(freshest_live_lease(&yielded_only, now, ttl), None);

        // Exactly at the TTL boundary is NOT fresh (age < ttl is the rule).
        let boundary = vec![mk(15, "b-host", "b-sweep", 6, false)];
        assert_eq!(freshest_live_lease(&boundary, now, ttl), None);
    }

    #[test]
    fn classify_ls_remote_distinguishes_exists_absent_and_failed() {
        assert_eq!(
            classify_ls_remote(true, "aef1c2…\trefs/heads/feature/issue-42\n"),
            BranchLeg::Exists
        );
        assert_eq!(classify_ls_remote(true, "\n"), BranchLeg::Absent);
        assert_eq!(classify_ls_remote(false, ""), BranchLeg::ProbeFailed);
    }

    // --- Leg readers against a fake gh/git (no network) ----------------------

    use tempfile::tempdir;

    fn write_exec(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/usr/bin/env bash\n{body}")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }
        path
    }

    // --- Target kind: a PR number is not a claimable issue (#9929) ----------

    #[test]
    fn url_is_pull_request_reads_the_segment_before_the_number() {
        for pr in [
            "https://github.com/rjwalters/loom/pull/9932",
            "https://api.github.com/repos/rjwalters/loom/pulls/9932",
            "https://gitea.example.com/owner/repo/pulls/306",
            "github.com/o/r/pull/1",
        ] {
            assert!(url_is_pull_request(pr), "{pr} is a PR URL");
        }
        for issue in [
            "https://github.com/rjwalters/loom/issues/9929",
            "https://api.github.com/repos/rjwalters/loom/issues/9929",
            // A repository literally named `pull` must not read as a PR —
            // the reason this is a segment test, not a substring search.
            "https://github.com/me/pull/issues/5",
            "https://github.com/me/pulls/issues/5",
            // No trailing number at all.
            "https://github.com/rjwalters/loom/pull",
            "",
        ] {
            assert!(!url_is_pull_request(issue), "{issue} is not a PR URL");
        }
    }

    /// The #9929 refusal: a pull-request target is blocked with its own token
    /// and, like `OPEN_PR`, cannot be bought past — there is no issue to
    /// claim, so forcing is meaningless rather than merely risky.
    #[test]
    fn pull_request_target_is_blocked_and_never_overridable() {
        for force in [false, true] {
            let v = decide(
                306,
                &evidence(OpenPrProbe::NoneOpen, Some(LabelLeg::IsPullRequest), None, None),
                force,
            );
            assert_eq!(v.code, 0, "force={force}: a PR target must block");
            assert_eq!(v.stdout, "TARGET_IS_PR #306");
            assert!(v.stderr.contains("a pull request, not an issue"), "{}", v.stderr);
            assert!(v.stderr.contains("Hard-abort"), "{}", v.stderr);
        }
    }

    /// The whole point of leg 2 carrying the kind question: the read that
    /// already happens is the one that can answer it. A PR payload resolves
    /// to `IsPullRequest` even when its labels look perfectly claimable —
    /// which is exactly the #9929 shape (a PR wearing issue-lifecycle
    /// labels), and `gh issue view` serves it without complaint.
    #[test]
    fn read_claim_labels_detects_a_pull_request_target() {
        let dir = tempdir().unwrap();

        let pr = write_exec(
            dir.path(),
            "gh-pr.sh",
            r#"echo '{"labels":[{"name":"loom:curated"}],"url":"https://github.com/o/r/pull/306"}'"#,
        );
        assert_eq!(read_claim_labels(&pr, dir.path(), 306), LabelLeg::IsPullRequest);

        // A PR whose labels would otherwise read as a claim: still the kind
        // refusal, not BUILDING — the claim question does not apply.
        let claimed_pr = write_exec(
            dir.path(),
            "gh-claimed-pr.sh",
            r#"echo '{"labels":[{"name":"loom:building"}],"url":"https://github.com/o/r/pull/306"}'"#,
        );
        assert_eq!(read_claim_labels(&claimed_pr, dir.path(), 306), LabelLeg::IsPullRequest);

        // A real issue URL is unaffected.
        let real_issue = write_exec(
            dir.path(),
            "gh-issue.sh",
            r#"echo '{"labels":[{"name":"loom:issue"}],"url":"https://github.com/o/r/issues/42"}'"#,
        );
        assert_eq!(read_claim_labels(&real_issue, dir.path(), 42), LabelLeg::Clean);
    }

    #[test]
    fn read_claim_labels_distinguishes_claimed_clean_and_unknown() {
        let dir = tempdir().unwrap();

        let claimed = write_exec(
            dir.path(),
            "gh-claimed.sh",
            r#"echo '{"labels":[{"name":"loom:issue"},{"name":"loom:building"}]}'"#,
        );
        assert_eq!(
            read_claim_labels(&claimed, dir.path(), 42),
            LabelLeg::Claimed(vec!["loom:building".to_string()])
        );

        let clean = write_exec(
            dir.path(),
            "gh-clean.sh",
            r#"echo '{"labels":[{"name":"loom:issue"},{"name":"loom:curated"}]}'"#,
        );
        assert_eq!(read_claim_labels(&clean, dir.path(), 42), LabelLeg::Clean);

        let failing = write_exec(dir.path(), "gh-fail.sh", "echo 'gh blew up' >&2\nexit 1");
        assert_eq!(read_claim_labels(&failing, dir.path(), 42), LabelLeg::Unknown);

        let garbage = write_exec(dir.path(), "gh-garbage.sh", "echo 'not json'");
        assert_eq!(read_claim_labels(&garbage, dir.path(), 42), LabelLeg::Unknown);
    }

    #[test]
    fn read_freshest_live_lease_distinguishes_held_none_and_read_failed() {
        let dir = tempdir().unwrap();
        let now = DateTime::parse_from_rfc3339("2026-09-30T04:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let fleet = crate::comment_trust::records::TEST_FLEET_AUTHOR;
        let ts = "2026-09-30T03:58:00Z"; // 2 minutes old — fresh at the 15m TTL

        let held = write_exec(
            dir.path(),
            "gh-held.sh",
            &format!(
                r#"echo '{{"updated_at":"{ts}","body":"<!-- loom:lease host=h1 sweep=s1 -->",{fleet}}}'"#
            ),
        );
        assert_eq!(
            read_freshest_live_lease(&held, dir.path(), 1, now, 15.0),
            LeaseLeg::Held {
                host: "h1".to_string(),
                sweep_id: "s1".to_string()
            }
        );

        // An UNTRUSTED author's lease reads as absent (#9548): the row
        // carries no author fields, so the trust filter drops it.
        let untrusted = write_exec(
            dir.path(),
            "gh-untrusted.sh",
            &format!(
                r#"echo '{{"updated_at":"{ts}","body":"<!-- loom:lease host=h2 sweep=s2 -->"}}'"#
            ),
        );
        assert_eq!(
            read_freshest_live_lease(&untrusted, dir.path(), 1, now, 15.0),
            LeaseLeg::NoneFresh
        );

        let empty = write_exec(dir.path(), "gh-empty.sh", "exit 0");
        assert_eq!(read_freshest_live_lease(&empty, dir.path(), 1, now, 15.0), LeaseLeg::NoneFresh);

        let failing = write_exec(dir.path(), "gh-fail.sh", "echo 'transient' >&2\nexit 1");
        assert_eq!(
            read_freshest_live_lease(&failing, dir.path(), 1, now, 15.0),
            LeaseLeg::ReadFailed
        );
    }

    #[test]
    fn remote_branch_leg_uses_ls_remote_heads_origin() {
        let dir = tempdir().unwrap();

        let exists =
            write_exec(dir.path(), "git-exists.sh", r#"echo "aef1c2 refs/heads/feature/issue-42""#);
        assert_eq!(remote_branch_leg(&exists, dir.path(), 42), BranchLeg::Exists);

        let absent = write_exec(dir.path(), "git-absent.sh", "true");
        assert_eq!(remote_branch_leg(&absent, dir.path(), 42), BranchLeg::Absent);

        let failing = write_exec(dir.path(), "git-fail.sh", "echo 'no network' >&2\nexit 128");
        assert_eq!(remote_branch_leg(&failing, dir.path(), 42), BranchLeg::ProbeFailed);
    }
}
