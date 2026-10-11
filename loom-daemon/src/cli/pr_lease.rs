//! `loom-daemon lease pr …` — role-keyed leases for the PR-phase claims
//! (`loom:treating` for Doctor, `loom:reviewing` for Judge), #9401.
//!
//! ## The gap this closes
//!
//! An issue claim (`loom:building`) has had a liveness lease since Epic #6165:
//! a `<!-- loom:lease … -->` comment published at claim time, ordered on its
//! forge-assigned comment `id` (claim-then-verify-order, #6287), renewed while
//! the claimant works (#6180) and fenced before every push (#6309). The two
//! PR-phase claims had none of it. They were a bare label, and adding a label
//! that is already present is a silent no-op, so two Doctors can each believe
//! they "took" `loom:treating`. Observed twice: two in-session sweeps' Doctors
//! sharing one worktree (#9401's body), and two daemon-dispatched Doctors
//! working one review for ~20 minutes until one force-pushed over the other
//! (#9401's 2026-10-11 comment).
//!
//! ## What this is
//!
//! The same lease semantics, keyed by role so a Doctor's lease and a Judge's
//! lease on one PR never contend:
//!
//! ```text
//! <!-- loom:pr-lease role=<doctor|judge> host=<host-id> sweep=<sweep-id> -->
//! ```
//!
//! - **claim** publishes a lease and runs claim-then-verify-order on comment
//!   `id`: among this role's *live* leases the earliest wins. A live lease is
//!   one whose `updated_at` is inside the lease TTL (`LOOM_LEASE_TTL_MINUTES`,
//!   default 15 — the reclamation and fence TTL) and whose identity has not
//!   posted a later yield record. The TTL is the episode window: a finished
//!   claim is released or ages out, so history never outranks a live claim.
//!   A loser posts a `loom:pr-lease-yield` record and exits 4 before the
//!   caller has touched a worktree or a label.
//! - **fence** is the pre-push / pre-verdict check: still the earliest live
//!   lease of this role?
//! - **release** stops the renewer and rewrites the lease's marker to
//!   `loom:pr-lease-released`, which frees the claim immediately.
//! - **check** reports [`ClaimState`] (`free`, `held-self`, `held-peer`,
//!   `unknown`) — the PR-phase shape of #11118's proposed `claim_state(item)`.
//!
//! The label stays the caller's to write, **after** a won claim: the label is
//! a projection of the lease, not the evidence (#11118).
//!
//! ## Why a new marker family rather than `loom:lease`
//!
//! Every `loom:lease host=` reader (`sweep-lease-{publish,fence,renew}.sh`,
//! the reclaimer, dispatch ordering, check-claim, co-occupancy) assumes one
//! claim per number: a Judge's lease would read as a peer to a Doctor's.
//! Teaching all of them a role is a cross-cutting change to the issue path;
//! a separate prefix none of them matches keeps the issue path byte-for-byte
//! unchanged.
//!
//! ## Fail-open and fail-closed
//!
//! Every write (lease, yield, release, renewal) names the `owner/repo`
//! `write_scope::may_write_from` vetted (#9548); a refused target means
//! `claim` exits 5 and writes nothing.
//!
//! Before anything is written, an unreadable lease list is `unknown` and
//! claim refuses (exit 5): an unknown claim state is never "free" (#11118).
//! After this claimant's own lease is on the forge, an unverifiable ordering
//! proceeds, like the issue path (#6287): the lease is there for every peer
//! to see. `fence` fails open on an unreadable list, like
//! `sweep-lease-fence.sh` (#6309). Gitea has no `gh api` comments path, so
//! every verb is a no-op there (exit 0) and the label claim works as before.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use loom_daemon::comment_trust::{records::AUTHOR_JQ, TrustPolicy};
use loom_daemon::forge_call_stats::{ops, ForgeOp};
use loom_daemon::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};

pub(crate) mod renew;

/// First-line prefix of a live PR lease record.
pub(crate) const LEASE_PREFIX: &str = "<!-- loom:pr-lease role=";
/// First-line prefix of a tie-break loser's standdown record.
pub(crate) const YIELD_PREFIX: &str = "<!-- loom:pr-lease-yield role=";
/// First-line prefix a released lease's marker is rewritten to.
pub(crate) const RELEASED_PREFIX: &str = "<!-- loom:pr-lease-released role=";
/// The common prefix all three share — the read's `--jq` pre-filter.
const FAMILY_PREFIX: &str = "<!-- loom:pr-lease";

/// Claim held (won, or re-attached to this claimant's own live lease); fence
/// passed.
pub(crate) const EXIT_HELD: i32 = 0;
/// The lease list could not be read before anything was written. Not `2`:
/// clap exits 2 on an unknown subcommand, which is how an older
/// `loom-daemon` without `lease pr` answers, and the role prompts read that
/// as "claim by label alone, as before".
pub(crate) const EXIT_UNVERIFIED: i32 = 5;
/// fence: this claimant's own lease is no longer live (aged out or yielded).
pub(crate) const EXIT_EXPIRED: i32 = 3;
/// A peer holds the earliest live lease of this role.
pub(crate) const EXIT_PEER: i32 = 4;

/// Bounded read retries (mirrors `LEASE_ORDER_OWN_COMMENT_MAX_ATTEMPTS`).
const READ_ATTEMPTS: u32 = 3;
const READ_RETRY_DELAY: Duration = Duration::from_millis(300);
/// Confirmation re-reads once this claimant looks earliest (#6951's shape:
/// give a slower-propagating peer comment time to appear).
const CONFIRM_ATTEMPTS: u32 = 2;
const CONFIRM_DELAY: Duration = Duration::from_millis(500);
/// Per-call deadline for every `gh` call here.
const GH_TIMEOUT: Duration = Duration::from_secs(60);

/// The PR-phase role a lease is keyed by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Role {
    Doctor,
    Judge,
}

impl Role {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Doctor => "doctor",
            Self::Judge => "judge",
        }
    }

    /// The claim label this role's lease backs.
    pub(crate) fn claim_label(self) -> &'static str {
        match self {
            Self::Doctor => "loom:treating",
            Self::Judge => "loom:reviewing",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "doctor" => Some(Self::Doctor),
            "judge" => Some(Self::Judge),
            _ => None,
        }
    }
}

/// Who holds a lease: the role plus the same `(host, sweep)` pair issue
/// leases use (`host` is the published, opaque-by-default id, #6322).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Identity {
    pub(crate) role: Role,
    pub(crate) host: String,
    pub(crate) sweep: String,
}

impl Identity {
    fn fields(&self) -> String {
        format!("{} host={} sweep={}", self.role.as_str(), self.host, self.sweep)
    }

    pub(crate) fn lease_marker(&self) -> String {
        format!("{LEASE_PREFIX}{} -->", self.fields())
    }

    pub(crate) fn released_marker(&self) -> String {
        format!("{RELEASED_PREFIX}{} -->", self.fields())
    }

    pub(crate) fn yield_marker(&self, winner: &Identity) -> String {
        format!(
            "{YIELD_PREFIX}{} earliest_host={} earliest_sweep={} -->",
            self.fields(),
            winner.host,
            winner.sweep
        )
    }
}

/// `<role> host=<h> sweep=<s>[ more…] -->` after a family prefix. Only the
/// first three tokens are read; nothing past `-->` ever is.
fn parse_fields(rest: &str) -> Option<Identity> {
    let rest = rest.trim_end().strip_suffix("-->")?;
    let mut tokens = rest.split_whitespace();
    let role = Role::parse(tokens.next()?)?;
    let host = tokens.next()?.strip_prefix("host=")?;
    let sweep = tokens.next()?.strip_prefix("sweep=")?;
    if host.is_empty() || sweep.is_empty() {
        return None;
    }
    Some(Identity {
        role,
        host: host.to_string(),
        sweep: sweep.to_string(),
    })
}

/// What one comment's first line says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Record {
    Lease(Identity),
    Yield(Identity),
    Released(Identity),
}

pub(crate) fn parse_record(body: &str) -> Option<Record> {
    let first = body.lines().next()?;
    if let Some(rest) = first.strip_prefix(LEASE_PREFIX) {
        return parse_fields(rest).map(Record::Lease);
    }
    if let Some(rest) = first.strip_prefix(YIELD_PREFIX) {
        return parse_fields(rest).map(Record::Yield);
    }
    first
        .strip_prefix(RELEASED_PREFIX)
        .and_then(parse_fields)
        .map(Record::Released)
}

/// One `loom:pr-lease*` comment as read off the forge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Row {
    pub(crate) id: u64,
    pub(crate) updated_at: DateTime<Utc>,
    pub(crate) body: String,
}

/// Parse the read's NDJSON (one `{id, updated_at, body}` per line — never a
/// `[...]` array, which `--paginate` corrupts, #4637). Malformed lines drop.
pub(crate) fn parse_rows(ndjson: &str) -> Vec<Row> {
    ndjson
        .lines()
        .filter_map(|line| {
            let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
            let id = v.get("id")?.as_u64()?;
            let updated_at = DateTime::parse_from_rfc3339(v.get("updated_at")?.as_str()?)
                .ok()?
                .with_timezone(&Utc);
            let body = v.get("body")?.as_str()?.to_string();
            Some(Row {
                id,
                updated_at,
                body,
            })
        })
        .collect()
}

/// A live lease: in-TTL by its forge `updated_at`, not followed by a yield
/// record from the same identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LiveLease {
    pub(crate) id: u64,
    pub(crate) holder: Identity,
}

/// Every live lease of `role`, earliest comment `id` first. Pure.
pub(crate) fn live_leases(
    rows: &[Row],
    role: Role,
    now: DateTime<Utc>,
    ttl: chrono::Duration,
) -> Vec<LiveLease> {
    let yields: Vec<(u64, Identity)> = rows
        .iter()
        .filter_map(|r| match parse_record(&r.body)? {
            Record::Yield(who) => Some((r.id, who)),
            _ => None,
        })
        .collect();
    let mut live: Vec<LiveLease> = rows
        .iter()
        .filter(|r| now.signed_duration_since(r.updated_at) < ttl)
        .filter_map(|r| match parse_record(&r.body)? {
            Record::Lease(who) if who.role == role => Some(LiveLease {
                id: r.id,
                holder: who,
            }),
            _ => None,
        })
        .filter(|l| {
            !yields
                .iter()
                .any(|(yid, who)| *yid > l.id && *who == l.holder)
        })
        .collect();
    live.sort_by_key(|l| l.id);
    live
}

/// The PR-phase claim state for one role (#11118's `claim_state(item)`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ClaimState {
    /// No live lease of this role.
    Free,
    /// This claimant holds the earliest live lease.
    HeldBySelf { id: u64 },
    /// A peer holds the earliest live lease.
    HeldByPeer { id: u64, holder: Identity },
    /// The lease list could not be read. Never treated as `Free`.
    Unknown,
}

impl ClaimState {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Free => "free",
            Self::HeldBySelf { .. } => "held-self",
            Self::HeldByPeer { .. } => "held-peer",
            Self::Unknown => "unknown",
        }
    }
}

/// Decide [`ClaimState`] for `me` from a read (`None` = read failed). Pure.
pub(crate) fn claim_state(
    rows: Option<&[Row]>,
    me: &Identity,
    now: DateTime<Utc>,
    ttl: chrono::Duration,
) -> ClaimState {
    let Some(rows) = rows else {
        return ClaimState::Unknown;
    };
    match live_leases(rows, me.role, now, ttl).into_iter().next() {
        None => ClaimState::Free,
        Some(l) if l.holder == *me => ClaimState::HeldBySelf { id: l.id },
        Some(l) => ClaimState::HeldByPeer {
            id: l.id,
            holder: l.holder,
        },
    }
}

/// The pre-push / pre-verdict fence verdict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Fence {
    /// This claimant holds the earliest live lease.
    Pass,
    /// No evidence either way (unreadable list, or this claimant never
    /// leased): proceed, as `sweep-lease-fence.sh` does.
    NoEvidence,
    /// This claimant's lease exists but is no longer live.
    Expired,
    /// A peer holds the earliest live lease of this role.
    Superseded(Identity),
}

/// Fence decision. Pure.
pub(crate) fn fence(
    rows: Option<&[Row]>,
    me: &Identity,
    now: DateTime<Utc>,
    ttl: chrono::Duration,
) -> Fence {
    let Some(rows) = rows else {
        return Fence::NoEvidence;
    };
    match claim_state(Some(rows), me, now, ttl) {
        ClaimState::HeldBySelf { .. } => Fence::Pass,
        ClaimState::HeldByPeer { holder, .. } => Fence::Superseded(holder),
        ClaimState::Free | ClaimState::Unknown => {
            let leased = rows
                .iter()
                .any(|r| parse_record(&r.body) == Some(Record::Lease(me.clone())));
            if leased {
                Fence::Expired
            } else {
                Fence::NoEvidence
            }
        }
    }
}

/// What the post-write verification concluded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Ordering {
    Won,
    Lost(Identity),
    /// Own lease not visible or list unreadable: proceed (fail-open, #6287).
    Unverified,
}

pub(crate) fn ordering(state: &ClaimState) -> Ordering {
    match state {
        ClaimState::HeldBySelf { .. } => Ordering::Won,
        ClaimState::HeldByPeer { holder, .. } => Ordering::Lost(holder.clone()),
        ClaimState::Free | ClaimState::Unknown => Ordering::Unverified,
    }
}

// ---------------------------------------------------------------------------
// Identity resolution
// ---------------------------------------------------------------------------

/// The host id every lease-shaped comment publishes (#6322): opaque unless
/// `LOOM_LEASE_PUBLISH_HOSTNAME` opts into the raw name. Same rule as
/// `SweepRegistry::published_host_id`.
pub(crate) fn published_host() -> String {
    use loom_daemon::sweep_registry::{host_identity, opaque_host_id, LEASE_PUBLISH_HOSTNAME_ENV};
    let raw = std::env::var(LEASE_PUBLISH_HOSTNAME_ENV).is_ok_and(|v| {
        matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
    });
    let host = host_identity();
    if raw {
        host
    } else {
        opaque_host_id(&host)
    }
}

/// The sweep half of the identity: `--sweep-id`, else an in-session sweep's
/// `LOOM_SWEEP_RUN_ID`, else a daemon child's `LOOM_TERMINAL_ID`
/// (`daemon-<id>`), else a stable per-session id from `--watch-pid` (the
/// attended shape `lease ensure` uses, #10570), else `pr-lease-unknown`.
/// Every verb resolves it the same way, so a claimant's `fence` and
/// `release` find the lease its `claim` wrote.
pub(crate) fn resolve_sweep(
    explicit: Option<&str>,
    watch_pid: Option<u32>,
    env: impl Fn(&str) -> Option<String>,
) -> String {
    let non_empty = |v: Option<String>| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    if let Some(s) = non_empty(explicit.map(str::to_string)) {
        return s;
    }
    if let Some(s) = non_empty(env("LOOM_SWEEP_RUN_ID")) {
        return s;
    }
    if let Some(s) = non_empty(env("LOOM_TERMINAL_ID"))
        .and_then(|t| t.strip_prefix("daemon-").map(str::to_string))
        .filter(|s| !s.is_empty())
    {
        return s;
    }
    watch_pid
        .and_then(super::lease_ensure::session_sweep_id)
        .unwrap_or_else(|| "pr-lease-unknown".to_string())
}

// ---------------------------------------------------------------------------
// Forge I/O
// ---------------------------------------------------------------------------

/// Where and how `gh` runs; a test injects a stub `bin`.
#[derive(Clone, Debug)]
pub(crate) struct Gh {
    pub(crate) bin: PathBuf,
    pub(crate) root: PathBuf,
    /// The `owner/repo` every write names explicitly, as
    /// [`write_target`] vetted it, or why writing is refused.
    pub(crate) write_repo: Result<String, String>,
}

/// Loom writes only to repositories it manages (#9548): the vetted
/// `owner/repo` for a write from `root`, or why not. Reads are never gated.
pub(crate) fn write_target(root: &Path) -> Result<String, String> {
    match loom_daemon::write_scope::may_write_from(root, None) {
        loom_daemon::write_scope::Verdict::Allow(nwo) => Ok(nwo),
        loom_daemon::write_scope::Verdict::Deny(why) => Err(why),
    }
}

impl Gh {
    fn inv(&self, op: &'static str, intent: AccessIntent, fop: ForgeOp) -> GhInvocation {
        GhInvocation::new(Operation::new(op), intent, GhTarget::None, GH_TIMEOUT)
            .forge_op(fop)
            .program(&self.bin)
            .current_dir(&self.root)
            // W4-C: these reads verify this claimant's own writes.
            .writer_identity()
    }

    /// Every trusted `loom:pr-lease*` comment on `pr`; `None` on any failure.
    pub(crate) fn read_rows(&self, pr: u64, policy: &TrustPolicy) -> Option<Vec<Row>> {
        let jq = format!(
            r#".[] | select(.body != null and (.body | startswith("{FAMILY_PREFIX}"))) | {{id: .id, updated_at: .updated_at, body: .body, {AUTHOR_JQ}}}"#
        );
        let out = self
            .inv("pr_lease.comments", AccessIntent::Read, ops::COMMENT_LIST)
            .args([
                "api",
                &format!("repos/{{owner}}/{{repo}}/issues/{pr}/comments"),
                "--paginate",
                "--jq",
                &jq,
            ])
            .run();
        let out = out.ok_output()?;
        // #9548/#9631: an untrusted author's marker is prose.
        let trusted = policy.trusted_ndjson(&out.stdout);
        Some(parse_rows(&String::from_utf8_lossy(&trusted)))
    }

    /// [`read_rows`](Self::read_rows) with the bounded retry.
    pub(crate) fn read_rows_retrying(&self, pr: u64, policy: &TrustPolicy) -> Option<Vec<Row>> {
        for attempt in 1..=READ_ATTEMPTS {
            if let Some(rows) = self.read_rows(pr, policy) {
                return Some(rows);
            }
            if attempt < READ_ATTEMPTS {
                std::thread::sleep(READ_RETRY_DELAY);
            }
        }
        None
    }

    /// Post a comment; its forge `id` on success.
    pub(crate) fn post(&self, pr: u64, body: &str) -> Option<u64> {
        let nwo = self.write_repo.as_ref().ok()?;
        let out = self
            .inv("pr_lease.comment", AccessIntent::Write, ops::COMMENT_CREATE)
            .args([
                "api",
                "-X",
                "POST",
                &format!("repos/{nwo}/issues/{pr}/comments"),
                "-f",
                &format!("body={body}"),
                "--jq",
                ".id",
            ])
            .run();
        String::from_utf8_lossy(&out.ok_output()?.stdout)
            .trim()
            .parse()
            .ok()
    }

    /// Replace a comment's body. `Err(true)` = the comment is gone (404).
    pub(crate) fn patch(&self, comment_id: u64, body: &str) -> Result<(), bool> {
        let Ok(nwo) = self.write_repo.as_ref() else {
            return Err(false);
        };
        let out = self
            .inv("pr_lease.patch", AccessIntent::Write, ops::COMMENT_EDIT_DELETE)
            .args([
                "api",
                "-X",
                "PATCH",
                &format!("repos/{nwo}/issues/comments/{comment_id}"),
                "-f",
                &format!("body={body}"),
                "--silent",
            ])
            .run();
        match out.into_result() {
            Ok(o) if o.status.success() => Ok(()),
            Ok(o) => Err(String::from_utf8_lossy(&o.stderr).contains("HTTP 404")),
            Err(_) => Err(false),
        }
    }

    /// `(open, labels)` for `pr`; `None` when unreadable.
    pub(crate) fn pr_state(&self, pr: u64) -> Option<(bool, Vec<String>)> {
        let out = self
            .inv("pr_lease.pr_state", AccessIntent::Read, ops::ISSUE_VIEW_STATE)
            .args([
                "api",
                &format!("repos/{{owner}}/{{repo}}/issues/{pr}"),
                "--jq",
                "{state: .state, labels: [.labels[].name]}",
            ])
            .run();
        let v: serde_json::Value = serde_json::from_slice(&out.ok_output()?.stdout).ok()?;
        let open = match v.get("state")?.as_str()? {
            "open" => true,
            "closed" => false,
            _ => return None,
        };
        let labels = v
            .get("labels")?
            .as_array()?
            .iter()
            .filter_map(|l| l.as_str().map(str::to_string))
            .collect();
        Some((open, labels))
    }

    /// The single issue `pr`'s body closes (its provenance story), if any.
    fn story_issue(&self, pr: u64) -> Option<u32> {
        let out = self
            .inv("pr_lease.pr_body", AccessIntent::Read, ops::PR_VIEW_STATE)
            .args([
                "api",
                &format!("repos/{{owner}}/{{repo}}/pulls/{pr}"),
                "--jq",
                ".body // \"\"",
            ])
            .run();
        let body = String::from_utf8_lossy(&out.ok_output()?.stdout).to_string();
        match loom_daemon::merge_pr::refs::closing_refs(&body).as_slice() {
            [one] => u32::try_from(*one).ok(),
            _ => None,
        }
    }
}

/// The lease comment body. `renewed` adds the line that makes each renewal
/// PATCH change the body (a forge may not advance `updated_at` otherwise).
pub(crate) fn lease_body(
    me: &Identity,
    pr: u64,
    renewed: Option<DateTime<Utc>>,
    provenance: &str,
) -> String {
    let mut body = format!(
        "{marker}\nPR #{pr}'s `{label}` claim is held by the {role} of sweep `{sweep}` on host \
         `{host}`. This comment is a **PR lease record** (#9401): its liveness signal is its own \
         forge-assigned `updated_at`, renewed while the {role} works and released when it is \
         done. Another {role} that finds this lease live stands down before touching a worktree. \
         See `defaults/docs/lease-record.md` § \"PR-phase leases\".",
        marker = me.lease_marker(),
        label = me.role.claim_label(),
        role = me.role.as_str(),
        sweep = me.sweep,
        host = me.host,
    );
    if let Some(at) = renewed {
        body.push_str(&format!("\n<!-- loom:pr-lease-renewed at={} -->", at.to_rfc3339()));
    }
    if !provenance.is_empty() {
        body.push_str("\n\n");
        body.push_str(provenance);
    }
    body
}

/// The hidden `loom:provenance v1` line for comments this verb posts. `host=`
/// carries the same published id as the lease marker, so one comment never
/// pairs an opaque id with the raw hostname it hides (#6322).
pub(crate) fn provenance_line(root: &Path, me: &Identity, story_issue: Option<u32>) -> String {
    use loom_daemon::provenance::marker::{collect, MarkerInputs};
    let mut marker = collect(
        root,
        &MarkerInputs {
            issue: story_issue,
            sweep: Some(&me.sweep),
            ..MarkerInputs::default()
        },
    );
    marker.host = me.host.clone();
    marker.render()
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// Flags every verb shares.
#[derive(clap::Args, Debug, Clone)]
pub(crate) struct Common {
    /// The pull request.
    #[arg(value_name = "PR")]
    pub(crate) pr: u64,
    /// Which PR-phase claim: `doctor` (`loom:treating`) or `judge`
    /// (`loom:reviewing`).
    #[arg(long, value_enum)]
    pub(crate) role: Role,
    /// The durable session pid, computed by the caller as
    /// `${LOOM_AGENT_SESSION_PID:-${CLAUDE_PID:-$PPID}}` (never `$$`). It
    /// derives the lease identity, and `claim` hands it to the renewer.
    #[arg(long, value_name = "PID")]
    pub(crate) watch_pid: Option<u32>,
    /// Explicit sweep id (default: see `resolve_sweep`).
    #[arg(long)]
    pub(crate) sweep_id: Option<String>,
    /// Repo checkout to operate in.
    #[arg(long, default_value = ".")]
    pub(crate) workspace: PathBuf,
}

#[derive(clap::Subcommand)]
pub(crate) enum PrLeaseAction {
    /// Publish this role's lease on a PR and run claim-then-verify-order.
    /// Exit 0 = held (add the claim label now); 4 = a peer holds it (a yield
    /// record is posted if this call had written a lease); 5 = the lease list
    /// was unreadable, nothing written. Stand down on 4 or 5.
    Claim(Common),
    /// Pre-push / pre-verdict fence. Exit 0 = proceed; 3 = this claimant's
    /// lease is no longer live; 4 = a peer holds this role's lease.
    Fence(Common),
    /// Stop the renewer and mark this claimant's lease released. Always 0.
    Release(Common),
    /// Print `PR_LEASE_STATE=free|held-self|held-peer|unknown` (+ holder).
    /// Exit 0 free/held-self, 4 held-peer, 5 unknown.
    Check(Common),
    /// Internal: the detached renewal loop `claim` starts.
    #[command(hide = true)]
    RenewLoop(renew::RenewLoopArgs),
}

impl PrLeaseAction {
    pub(crate) fn run(self) -> Result<()> {
        let code = match self {
            Self::RenewLoop(args) => {
                renew::run_loop(&args);
                0
            }
            // Without a checkout there is no evidence: claim and check refuse,
            // fence and release fail open.
            Self::Claim(c) => with_context(&c, EXIT_UNVERIFIED, true, |ctx| ctx.claim()),
            Self::Fence(c) => with_context(&c, EXIT_HELD, false, |ctx| ctx.fence()),
            Self::Release(c) => with_context(&c, EXIT_HELD, true, |ctx| ctx.release()),
            Self::Check(c) => with_context(&c, EXIT_UNVERIFIED, false, |ctx| ctx.check()),
        };
        std::process::exit(code)
    }
}

/// Everything one verb run needs, resolved once.
pub(crate) struct Ctx {
    pub(crate) common: Common,
    pub(crate) me: Identity,
    pub(crate) gh: Gh,
    pub(crate) policy: TrustPolicy,
    pub(crate) ttl: chrono::Duration,
    pub(crate) now: fn() -> DateTime<Utc>,
    /// Whether `claim`/`release` start and stop the detached renewer (tests:
    /// no, so they never touch the host's renewer store).
    pub(crate) manage_renewer: bool,
}

fn with_context(
    common: &Common,
    no_checkout: i32,
    writes: bool,
    f: impl FnOnce(&Ctx) -> i32,
) -> i32 {
    let root = match loom_daemon::repo_root::resolve_repo_root(&common.workspace.to_string_lossy())
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("lease pr: not inside a Loom checkout ({e}); no lease evidence");
            return no_checkout;
        }
    };
    use loom_daemon::forge_cmd::{detect_forge, ForgeType};
    if detect_forge(Some(&root)) == ForgeType::Gitea {
        eprintln!("lease pr: PR leases are GitHub-only; on Gitea the label claim stands alone");
        return EXIT_HELD;
    }
    let sweep =
        resolve_sweep(common.sweep_id.as_deref(), common.watch_pid, |k| std::env::var(k).ok());
    let ttl_minutes = loom_daemon::claim_reconciliation::resolve_lease_ttl_minutes();
    let ctx = Ctx {
        me: Identity {
            role: common.role,
            host: published_host(),
            sweep,
        },
        gh: Gh {
            bin: PathBuf::from(std::env::var("LOOM_GH_BIN").unwrap_or_else(|_| "gh".into())),
            root: root.clone(),
            write_repo: if writes {
                write_target(&root)
            } else {
                Err("read-only verb".to_string())
            },
        },
        policy: TrustPolicy::for_root(&root),
        ttl: chrono::Duration::seconds((ttl_minutes * 60.0) as i64),
        now: Utc::now,
        manage_renewer: true,
        common: common.clone(),
    };
    f(&ctx)
}

impl Ctx {
    fn pr(&self) -> u64 {
        self.common.pr
    }

    fn state_of(&self, rows: Option<&[Row]>) -> ClaimState {
        claim_state(rows, &self.me, (self.now)(), self.ttl)
    }

    fn say(&self, line: &str) {
        eprintln!("lease pr {} #{}: {line}", self.me.role.as_str(), self.pr());
    }

    /// `claim`: pre-check, publish, verify order, start renewal.
    pub(crate) fn claim(&self) -> i32 {
        if let Err(why) = &self.gh.write_repo {
            self.say(&format!("refusing to write here (#9548): {why} — stand down"));
            return EXIT_UNVERIFIED;
        }
        let rows = self.gh.read_rows_retrying(self.pr(), &self.policy);
        match self.state_of(rows.as_deref()) {
            ClaimState::Unknown => {
                self.say("could not read this PR's lease records; nothing written — stand down");
                return EXIT_UNVERIFIED;
            }
            ClaimState::HeldByPeer { holder, .. } => {
                self.say(&format!(
                    "PEER_LEASE host={} sweep={} holds the live {} lease — stand down without \
                     touching the worktree",
                    holder.host,
                    holder.sweep,
                    self.me.role.as_str()
                ));
                return EXIT_PEER;
            }
            ClaimState::HeldBySelf { id } => {
                self.say(&format!("already held by this claimant (comment {id}); re-attaching"));
                self.start_renewal(id, self.gh.story_issue(self.pr()));
                println!("{} {}", self.me.host, self.me.sweep);
                return EXIT_HELD;
            }
            ClaimState::Free => {}
        }
        let story = self.gh.story_issue(self.pr());
        let provenance = provenance_line(&self.gh.root, &self.me, story);
        let body = lease_body(&self.me, self.pr(), None, &provenance);
        let Some(own_id) = self.gh.post(self.pr(), &body) else {
            self.say("could not post the lease comment; no claim taken — stand down");
            return EXIT_UNVERIFIED;
        };
        match self.verify_order() {
            Ordering::Lost(winner) => {
                let yield_body = format!(
                    "{}\nThis {role}'s PR lease (sweep `{sweep}`, host `{host}`) was not the \
                     earliest live `{role}` lease on this PR: sweep `{ws}` on host `{wh}` has an \
                     earlier forge-assigned comment id. Standing down before touching a worktree \
                     or the `{label}` label (#9401, claim-then-verify-order #6287).\n\n{provenance}",
                    self.me.yield_marker(&winner),
                    role = self.me.role.as_str(),
                    sweep = self.me.sweep,
                    host = self.me.host,
                    ws = winner.sweep,
                    wh = winner.host,
                    label = self.me.role.claim_label(),
                );
                if self.gh.post(self.pr(), &yield_body).is_none() {
                    self.say("yield record could not be posted (best-effort)");
                }
                self.say(&format!(
                    "LOST_ORDER to host={} sweep={} — yielded; stand down",
                    winner.host, winner.sweep
                ));
                EXIT_PEER
            }
            Ordering::Won | Ordering::Unverified => {
                self.start_renewal(own_id, story);
                self.say(&format!(
                    "lease {own_id} held (host={} sweep={}); add `{}` now",
                    self.me.host,
                    self.me.sweep,
                    self.me.role.claim_label()
                ));
                println!("{} {}", self.me.host, self.me.sweep);
                EXIT_HELD
            }
        }
    }

    /// Read back until this claimant's own lease is visible, then confirm.
    fn verify_order(&self) -> Ordering {
        let mut seen = Ordering::Unverified;
        for attempt in 1..=READ_ATTEMPTS {
            let rows = self.gh.read_rows(self.pr(), &self.policy);
            seen = ordering(&self.state_of(rows.as_deref()));
            if seen != Ordering::Unverified {
                break;
            }
            if attempt < READ_ATTEMPTS {
                std::thread::sleep(READ_RETRY_DELAY);
            }
        }
        if seen != Ordering::Won {
            return seen;
        }
        for _ in 0..CONFIRM_ATTEMPTS {
            std::thread::sleep(CONFIRM_DELAY);
            let rows = self.gh.read_rows(self.pr(), &self.policy);
            if let Ordering::Lost(winner) = ordering(&self.state_of(rows.as_deref())) {
                return Ordering::Lost(winner);
            }
        }
        Ordering::Won
    }

    fn start_renewal(&self, comment_id: u64, story: Option<u32>) {
        if !self.manage_renewer {
            return;
        }
        let Some(watch_pid) = self.common.watch_pid else {
            self.say("no --watch-pid: lease not renewed, it ages out at the TTL");
            return;
        };
        let line = renew::spawn(self, comment_id, watch_pid, story);
        self.say(&line);
    }

    /// `fence`.
    pub(crate) fn fence(&self) -> i32 {
        let rows = self.gh.read_rows_retrying(self.pr(), &self.policy);
        match fence(rows.as_deref(), &self.me, (self.now)(), self.ttl) {
            Fence::Pass => EXIT_HELD,
            Fence::NoEvidence => {
                self.say(
                    "PASS (no lease evidence either way; fail-open like sweep-lease-fence.sh)",
                );
                EXIT_HELD
            }
            Fence::Expired => {
                self.say("ABORT: EXPIRED — this claimant's lease is no longer live; stand down");
                EXIT_EXPIRED
            }
            Fence::Superseded(holder) => {
                self.say(&format!(
                    "ABORT: SUPERSEDED — host={} sweep={} holds the live lease; stand down",
                    holder.host, holder.sweep
                ));
                EXIT_PEER
            }
        }
    }

    /// `release`: stop the renewer, then rewrite every live lease of this
    /// identity to the released marker. Best-effort; always 0.
    pub(crate) fn release(&self) -> i32 {
        let stopped = if self.manage_renewer {
            renew::stop(self)
        } else {
            0
        };
        if stopped > 0 {
            // A renewer killed mid-PATCH could still land its write; let it.
            std::thread::sleep(Duration::from_secs(2));
        }
        let Some(rows) = self.gh.read_rows_retrying(self.pr(), &self.policy) else {
            self.say("could not read lease records; the lease will age out at the TTL");
            return EXIT_HELD;
        };
        let mut released = 0;
        for row in rows {
            if parse_record(&row.body) != Some(Record::Lease(self.me.clone())) {
                continue;
            }
            let mut lines = row.body.lines();
            lines.next();
            let rest: Vec<&str> = lines.collect();
            let body = format!("{}\n{}", self.me.released_marker(), rest.join("\n"));
            if self.gh.patch(row.id, &body).is_ok() {
                released += 1;
            }
        }
        self.say(&format!("released {released} lease comment(s); stopped {stopped} renewer(s)"));
        EXIT_HELD
    }

    /// `check`.
    pub(crate) fn check(&self) -> i32 {
        let rows = self.gh.read_rows_retrying(self.pr(), &self.policy);
        let state = self.state_of(rows.as_deref());
        println!("PR_LEASE_STATE={}", state.as_str());
        match state {
            ClaimState::HeldByPeer { holder, .. } => {
                println!("PR_LEASE_HOLDER_HOST={}", holder.host);
                println!("PR_LEASE_HOLDER_SWEEP={}", holder.sweep);
                EXIT_PEER
            }
            ClaimState::Unknown => EXIT_UNVERIFIED,
            ClaimState::Free | ClaimState::HeldBySelf { .. } => EXIT_HELD,
        }
    }
}

#[cfg(test)]
mod tests;
