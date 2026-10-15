//! `merge-pr.sh --auto`'s in-process refresh of a stale PR (#12014).
//!
//! # The gap
//!
//! The #8248/#8919 freshness guard refuses a merge when the base moved under a
//! check whose inputs the PR changes. Its remedy already existed — the
//! tree-identical re-date push of [`super::redate`], with its per-chain budget
//! and the one `update-branch` base-sync handoff — but only behind
//! `--redate-stale-checks`, which only Champion passed, and exhaustion exited
//! **1** ("Merge Failed"). Every other `--auto` caller (the sweep merge step,
//! Mode C) got exit 1 on a stale PR and fell back to a hand-rolled retry loop.
//! A PR that touches a global CI input (`ci.yml`, `.cargo/config.toml`) needed
//! several full CI rounds after approval to land on a busy `main`.
//!
//! # What this adds (one mechanism, extended)
//!
//! Nothing here refreshes a branch a second way. It decides, around the
//! existing remedy, what `merge-pr.sh` exits and whether its caller should
//! re-invoke it now:
//!
//! - **`stale`** — the guard said STALE (never an unknown): run
//!   [`redate::remedy`] exactly as `--redate-stale-checks` does and map its
//!   outcome ([`stale_decision`]). A push or a base sync is
//!   `next=reinvoke` (exit 4); a backoff deferral or a sync conflict is
//!   `next=requeue` (exit 4); exhaustion is the new exit **6**, with the
//!   existing `loom:operator` escalation unchanged (it is the only cross-pass
//!   bound).
//! - **`verdict`** — after the settle-wait, before the merge: on a head Loom
//!   itself refreshed, the standing approval carries only when
//!   [`crate::verdict_equivalence`] (the implementation
//!   `verdict-staleness-guard.sh` shells out to) answers `tree` or
//!   `clean-merge` against the TRUSTED verdict-sha marker
//!   ([`verdict_gate`]). Anything else — `rebase-patch-identical`, changed,
//!   indeterminate, no trusted marker — is `next=judge` (exit 3), never a
//!   merge. A `CLEAR` answer tells the shell to re-run the freshness guard on
//!   the head about to be merged, so a base move during the refreshed head's
//!   CI refreshes again instead of merging on stale evidence.
//! - **`timeout`** — `merge-pr.sh` is about to exit 5 (CI still running at
//!   the 420 s pin): on a Loom-refreshed head whose refresh record is younger
//!   than `merge.autoRefreshMaxWaitSecs` it prints `next=reinvoke`, else
//!   `next=requeue` ([`timeout_next`]). Rounds are therefore spread across
//!   calls, each under its caller's 600000 ms tool timeout (#9395); the
//!   wall-clock bound stops a CI run that never starts from looping forever.
//!
//! The rounds are the redate chain's own budget (`champion.redateBudget`,
//! default 3) plus its sync handoffs. There is no second round counter.
//!
//! # Opt-out and fail direction
//!
//! `merge.autoRefresh` (default `true`), env `LOOM_MERGE_AUTO_REFRESH` beating
//! it; `--redate-stale-checks` (Champion) turns it on regardless. Off prints
//! [`OFF`] and `merge-pr.sh` behaves as before. A binary without this verb
//! prints no sentinel, and `merge-pr.sh` degrades to its pre-#12014 behaviour
//! (one exit-4 redate under the flag, else exit 1): it never merges on a
//! missing answer, so the verb does not raise the `requires-daemon` floor.

use chrono::{DateTime, TimeDelta, Utc};
use serde_json::Value;
use std::path::{Path, PathBuf};

use super::redate::{self, budget, sync_handoff, BudgetConfig, RemedyOutcome};
use crate::claim_reconciliation::{extract_latest_verdict_sha, VerdictKind};
use crate::verdict_equivalence::{Equivalence, EquivalenceKind};

/// Env switch for auto-refresh (beats config).
pub const ENABLE_ENV: &str = "LOOM_MERGE_AUTO_REFRESH";
/// Config switch for auto-refresh.
pub const ENABLE_CONFIG_KEY: &str = "merge.autoRefresh";
/// Env override for the re-invoke wall-clock bound (beats config).
pub const MAX_WAIT_ENV: &str = "LOOM_MERGE_AUTO_REFRESH_MAX_WAIT_SECS";
/// Config key for the re-invoke wall-clock bound.
pub const MAX_WAIT_CONFIG_KEY: &str = "merge.autoRefreshMaxWaitSecs";
/// Default re-invoke bound: three of this repo's ~15 min CI rounds.
pub const DEFAULT_MAX_WAIT_SECS: u64 = 2700;
/// Upper clamp (one day): a typo must not make the bound meaningless.
pub const MAX_MAX_WAIT_SECS: u64 = 86_400;

/// The caller-facing line: `LOOM-AUTO-REFRESH next=… pr=… round=k/N head=…`.
pub const SENTINEL: &str = "LOOM-AUTO-REFRESH";
/// Exit 6's line: rounds and sync handoffs are spent.
pub const EXHAUSTED: &str = "LOOM-AUTO-REFRESH-EXHAUSTED";
/// Auto-refresh is switched off: the shell keeps its pre-#12014 behaviour.
pub const OFF: &str = "LOOM-AUTO-REFRESH-OFF";
/// The verdict gate passed (or the head is not a Loom refresh head).
pub const CLEAR: &str = "LOOM-AUTO-REFRESH-CLEAR";
/// The branch moved before the refresh push; `current=<sha>`.
pub const HEAD_MOVED: &str = "LOOM-AUTO-REFRESH-HEAD-MOVED";
/// The remedy or a forge read failed; the reason follows.
pub const FAILED: &str = "LOOM-AUTO-REFRESH-FAILED";

/// `merge-pr.sh`'s exit for a refresh this pass (re-queue, not a failure).
pub const EXIT_REFRESHED: i32 = 4;
/// `merge-pr.sh`'s exit when the rounds are exhausted (re-queue, held).
pub const EXIT_EXHAUSTED: i32 = 6;
/// `merge-pr.sh`'s exit when the verdict does not carry (re-queue to Judge).
pub const EXIT_JUDGE: i32 = 3;

/// The resolved auto-refresh settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    pub enabled: bool,
    pub max_wait_secs: u64,
}

impl Config {
    /// env > config > default, per key. An unparseable value at a tier falls
    /// through to the next one.
    #[must_use]
    pub fn resolve(env_enabled: Option<&str>, env_max_wait: Option<&str>, config: &Value) -> Self {
        let get = |key| crate::config_resolver::get_path(config, key);
        let enabled = env_enabled
            .and_then(parse_flag)
            .or_else(|| {
                get(ENABLE_CONFIG_KEY).and_then(|v| {
                    v.as_bool()
                        .or_else(|| v.as_str().and_then(parse_flag))
                        .or_else(|| v.as_u64().map(|n| n != 0))
                })
            })
            .unwrap_or(true);
        let max_wait_secs = env_max_wait
            .and_then(|s| s.trim().parse::<u64>().ok())
            .or_else(|| get(MAX_WAIT_CONFIG_KEY).and_then(Value::as_u64))
            .unwrap_or(DEFAULT_MAX_WAIT_SECS)
            .min(MAX_MAX_WAIT_SECS);
        Self {
            enabled,
            max_wait_secs,
        }
    }

    /// The settings for the workspace at `root` (repo, then fleet tier).
    #[must_use]
    pub fn for_root(root: &Path) -> Self {
        Self::resolve(
            std::env::var(ENABLE_ENV).ok().as_deref(),
            std::env::var(MAX_WAIT_ENV).ok().as_deref(),
            &crate::config_resolver::resolve_effective_config(root),
        )
    }
}

fn parse_flag(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// What the caller does next. Decided here; prompts only branch on the token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    /// Run the identical `merge-pr.sh` command again now (same pin).
    Reinvoke,
    /// Come back on a later pass.
    Requeue,
    /// The verdict does not carry to this head: a fresh Judge review first.
    Judge,
}

impl Next {
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::Reinvoke => "reinvoke",
            Self::Requeue => "requeue",
            Self::Judge => "judge",
        }
    }
}

/// The one sentinel line.
#[must_use]
pub fn sentinel(next: Next, pr: &str, round: u32, budget: u32, head: &str) -> String {
    format!("{SENTINEL} next={} pr={pr} round={round}/{budget} head={head}", next.token())
}

/// `merge-pr.sh`'s exit code and the stdout it prints (empty = nothing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub exit: i32,
    pub stdout: String,
}

impl Decision {
    fn new(exit: i32, stdout: String) -> Self {
        Self { exit, stdout }
    }
}

/// Map the remedy's outcome for a STALE `head` onto `merge-pr.sh`'s exit and
/// sentinel. Pure.
#[must_use]
pub fn stale_decision(outcome: &RemedyOutcome, pr: &str, head: &str, budget: u32) -> Decision {
    match outcome {
        RemedyOutcome::Pushed { new_sha, n } => Decision::new(
            EXIT_REFRESHED,
            format!(
                "{}\nPR #{pr}: the #8248 freshness guard blocked the merge, so this run pushed \
tree-identical re-date {n} of {budget} ({new_sha}); CI now re-tests the current base. The \
verdict carries only via a `tree`/`clean-merge` equivalence (#9416). Re-invoke the identical \
merge-pr.sh call (#12014).",
                sentinel(Next::Reinvoke, pr, *n, budget, new_sha)
            ),
        ),
        RemedyOutcome::Deferred { spent, retry_after } => Decision::new(
            EXIT_REFRESHED,
            format!(
                "{}\nPR #{pr}: re-date {spent} of {budget} is still inside its backoff (#9590); \
nothing was pushed. Retry after {}.",
                sentinel(Next::Requeue, pr, *spent, budget, head),
                retry_after.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            ),
        ),
        RemedyOutcome::SyncedBase { n, max, .. } => Decision::new(
            EXIT_REFRESHED,
            format!(
                "{}\nPR #{pr}: re-date budget spent, so the base was merged in via update-branch \
(sync {n} of {max}, #10388); the new head starts a fresh chain. Re-invoke the identical \
merge-pr.sh call (#12014).",
                sentinel(Next::Reinvoke, pr, 0, budget, head)
            ),
        ),
        RemedyOutcome::SyncConflict { .. } => Decision::new(
            EXIT_REFRESHED,
            format!(
                "{}\nPR #{pr}: re-date budget spent and the base sync conflicts (#10388): \
loom:merge-conflict applied for Doctor. Not merged this pass.",
                sentinel(Next::Requeue, pr, budget, budget, head)
            ),
        ),
        RemedyOutcome::Escalated {
            notice_posted,
            spent,
            budget,
        } => Decision::new(
            EXIT_EXHAUSTED,
            format!(
                "{EXHAUSTED} pr={pr} spent={spent} budget={budget}\nPR #{pr}: auto-refresh \
rounds and sync handoffs are spent and the #8248 guard still blocks: {} {} (#9590). A human \
merges it, rebases it, or pushes any commit (a fresh chain). Re-queue; not a failure (#12014).",
                redate::HOLD_LABEL,
                if *notice_posted {
                    "applied and the hold notice posted"
                } else {
                    "kept, with the existing hold notice"
                }
            ),
        ),
        RemedyOutcome::HeadMoved { current } => {
            Decision::new(3, format!("{HEAD_MOVED} current={current}"))
        }
        RemedyOutcome::Failed(why) => Decision::new(1, format!("{FAILED}\n{why}")),
    }
}

/// Where a Loom-refreshed head sits: its round and when the refresh was
/// recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshMark {
    pub round: u32,
    pub recorded_at: Option<DateTime<Utc>>,
}

/// Is `head` a head Loom's own refresh produced? From the TRUSTED listing
/// only (#9548): a re-date whose marker names `head` (round = its chain
/// position), or a base sync recorded FROM `first_parent` (the update-branch
/// merge commit's first parent; round 0, a fresh chain). Pure.
#[must_use]
pub fn refresh_mark(
    trusted: &[Value],
    head: &str,
    first_parent: Option<&str>,
) -> Option<RefreshMark> {
    let pos = budget::chain_position(trusted, head);
    if pos.spent >= 1 {
        return Some(RefreshMark {
            round: pos.spent,
            recorded_at: pos.recorded_at,
        });
    }
    let parent = first_parent.filter(|p| !p.is_empty())?;
    let marker = sync_handoff::sync_marker(parent, 0);
    let prefix = marker.strip_suffix("0 -->").unwrap_or(&marker);
    trusted
        .iter()
        .filter(|c| {
            c.get("body")
                .and_then(Value::as_str)
                .is_some_and(|b| b.contains(prefix))
        })
        .map(|c| {
            c.get("created_at")
                .and_then(Value::as_str)
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&Utc))
        })
        .max()
        .map(|recorded_at| RefreshMark {
            round: 0,
            recorded_at,
        })
}

/// Re-invoke on an exit 5, or re-queue? A refresh of unknown age re-queues:
/// never loop on a bound that cannot be evaluated. Pure.
#[must_use]
pub fn timeout_next(mark: &RefreshMark, now: DateTime<Utc>, max_wait_secs: u64) -> Next {
    let bound = TimeDelta::try_seconds(i64::try_from(max_wait_secs).unwrap_or(i64::MAX))
        .unwrap_or(TimeDelta::MAX);
    match mark.recorded_at {
        Some(at) if now.signed_duration_since(at) < bound => Next::Reinvoke,
        _ => Next::Requeue,
    }
}

/// The verdict gate's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    Clear,
    Judge(String),
}

/// Does the approval carry to a Loom-refreshed `head`? `marker` is the
/// latest TRUSTED `loom:verdict-sha` for `loom:pr`; `equivalence` is asked
/// only when the marker names another head. Only `tree` and `clean-merge`
/// carry: they are the two kinds a refresh push can produce. Pure.
pub fn verdict_gate(
    marker: Option<&str>,
    head: &str,
    equivalence: impl FnOnce(&str) -> Equivalence,
) -> Gate {
    let Some(marker) = marker.filter(|m| !m.is_empty()) else {
        return Gate::Judge(
            "no trusted loom:verdict-sha marker anchors the approval (#9258), so nothing proves \
it describes this refreshed head"
                .into(),
        );
    };
    if head.starts_with(marker) {
        return Gate::Clear;
    }
    match equivalence(marker) {
        Equivalence::Equivalent(EquivalenceKind::Tree | EquivalenceKind::CleanMerge) => Gate::Clear,
        Equivalence::Equivalent(kind) => Gate::Judge(format!(
            "the approval for {marker} carries to {head} only as `{}`, which a refresh push \
cannot produce",
            kind.token()
        )),
        Equivalence::Changed => {
            Gate::Judge(format!("{head} is a different change from the approved {marker}"))
        }
        Equivalence::Indeterminate => Gate::Judge(format!(
            "could not show {head} equivalent to the approved {marker} (fail closed)"
        )),
    }
}

/// One `merge-pr auto-refresh` call's operands.
#[derive(Debug, Clone)]
pub struct Ctx {
    pub pr: String,
    pub nwo: String,
    pub branch: String,
    pub head: String,
    /// `--redate-stale-checks` was passed: on regardless of the opt-out.
    pub explicit: bool,
}

fn repo_root() -> PathBuf {
    crate::repo_root::find_repo_root_from_cwd()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default()
}

fn enabled(ctx: &Ctx, root: &Path) -> Option<Config> {
    let cfg = Config::for_root(root);
    (cfg.enabled || ctx.explicit).then_some(cfg)
}

fn off() -> Decision {
    Decision::new(0, OFF.to_string())
}

/// `stale`: run the existing remedy and map it.
#[must_use]
pub fn run_stale(ctx: &Ctx) -> Decision {
    let root = repo_root();
    if enabled(ctx, &root).is_none() {
        return off();
    }
    let outcome = redate::remedy(&ctx.nwo, &ctx.branch, &ctx.head, &ctx.pr);
    stale_decision(&outcome, &ctx.pr, &ctx.head, BudgetConfig::for_root(&root).budget)
}

/// The PR's trusted comment listing.
fn trusted_listing(gh: &str, ctx: &Ctx, root: &Path) -> Result<Vec<Value>, String> {
    let raw = redate::gh_api_with(
        gh,
        &[
            &format!("repos/{}/issues/{}/comments", ctx.nwo, ctx.pr),
            "--paginate",
        ],
    )
    .map_err(|e| format!("could not read PR #{}'s comments: {e}", ctx.pr))?;
    crate::comment_trust::TrustPolicy::for_root(root)
        .trusted_listing(raw.as_bytes())
        .ok_or_else(|| format!("PR #{}'s comment listing did not parse", ctx.pr))
}

/// The refresh mark for `ctx.head`, reading the head's first parent only
/// when a base sync was ever recorded on the PR.
fn mark_for(gh: &str, ctx: &Ctx, trusted: &[Value]) -> Option<RefreshMark> {
    let bodies = crate::comment_trust::records::bodies(trusted).join("\n");
    let parent = (sync_handoff::handoffs_used(&bodies) > 0
        && crate::verdict_equivalence::is_sha(&ctx.head))
    .then(|| {
        redate::gh_api_with(
            gh,
            &[
                &format!("repos/{}/commits/{}", ctx.nwo, ctx.head),
                "--jq",
                ".parents[0].sha",
            ],
        )
        .ok()
    })
    .flatten();
    refresh_mark(trusted, &ctx.head, parent.as_deref())
}

/// `verdict`: the post-wait gate on a Loom-refreshed head.
#[must_use]
pub fn run_verdict(ctx: &Ctx) -> Decision {
    let root = repo_root();
    if enabled(ctx, &root).is_none() {
        return off();
    }
    let gh = crate::gh_invocation::gh_bin();
    let trusted = match trusted_listing(&gh, ctx, &root) {
        Ok(t) => t,
        Err(why) => return Decision::new(1, format!("{FAILED}\n{why}")),
    };
    let Some(mark) = mark_for(&gh, ctx, &trusted) else {
        return Decision::new(0, CLEAR.to_string());
    };
    let bodies = crate::comment_trust::records::bodies(&trusted);
    let marker = extract_latest_verdict_sha(&bodies, VerdictKind::Approved);
    let gate = verdict_gate(marker.as_deref(), &ctx.head, |reviewed| match ctx.pr.parse::<u32>() {
        Ok(pr) => {
            crate::verdict_equivalence::assess(Path::new(&gh), None, pr, reviewed, &ctx.head)
                .equivalence
        }
        Err(_) => Equivalence::Indeterminate,
    });
    match gate {
        Gate::Clear => Decision::new(0, CLEAR.to_string()),
        Gate::Judge(why) => Decision::new(
            EXIT_JUDGE,
            format!(
                "{}\nPR #{}: not merged — {why}. A fresh Judge review is needed before this \
refreshed head merges (#12014).",
                sentinel(
                    Next::Judge,
                    &ctx.pr,
                    mark.round,
                    BudgetConfig::for_root(&root).budget,
                    &ctx.head
                ),
                ctx.pr
            ),
        ),
    }
}

/// `timeout`: the sentinel for an exit 5 on a Loom-refreshed head; empty
/// stdout for any other head (the pre-#12014 exit 5).
#[must_use]
pub fn run_timeout(ctx: &Ctx, now: DateTime<Utc>) -> Decision {
    let root = repo_root();
    let Some(cfg) = enabled(ctx, &root) else {
        return Decision::new(0, String::new());
    };
    let gh = crate::gh_invocation::gh_bin();
    let trusted = match trusted_listing(&gh, ctx, &root) {
        Ok(t) => t,
        Err(why) => {
            eprintln!("merge-pr auto-refresh: {why}; no re-invoke advice (re-queue)");
            return Decision::new(1, String::new());
        }
    };
    match mark_for(&gh, ctx, &trusted) {
        None => Decision::new(0, String::new()),
        Some(mark) => Decision::new(
            0,
            sentinel(
                timeout_next(&mark, now, cfg.max_wait_secs),
                &ctx.pr,
                mark.round,
                BudgetConfig::for_root(&root).budget,
                &ctx.head,
            ),
        ),
    }
}

#[cfg(test)]
mod tests;
