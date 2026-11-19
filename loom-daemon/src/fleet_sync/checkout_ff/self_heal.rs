//! Self-heal a main checkout that only Loom's own stale install blocks
//! (#11425, tracker #10698). See "Self-heal" in `daemon-reference.md`.
//!
//! The fast-forward stops at `mid-operation`, `ahead`, `diverged` and `dirty`,
//! which is right when a person or an agent left work there. On fleet workers
//! it is often only a resync that never landed: `chore: resync installed Loom
//! surfaces` commits whose push a ruleset refused, modified installed files,
//! or the rebase that same script abandoned. [`heal`] runs before the rules.
//! When **every** blocker is Loom-owned ([`classify`]: `installed_files`, or a
//! surface the resync overwrites wholesale and `.loom/resync-ignore` does not
//! pin, [`managed`]) it:
//!
//! 1. makes `refs/loom/rescue/<ts>` at HEAD (and `<ts>-branch` at the branch
//!    tip, when an operation moved HEAD off it);
//! 2. removes a stale, empty `index.lock`;
//! 3. stashes the dirty Loom paths as `loom-rescue-<ts>` (not during an
//!    operation: its abort discards them, and the rescue ref holds the work);
//! 4. aborts the operation, or, for a HEAD left detached for
//!    [`managed::detached_after`] and behind, checks the branch out again at
//!    `origin/<default>` (#11989);
//! 5. `reset --hard origin/<default>`.
//!
//! Any failure stops the repair and the ordinary rules report the checkout as
//! before. Untracked files are never touched. Nothing is written while
//! `fleet.autoApply` is off, the host is held, a gate run is building there or
//! the self-update is running there.
//!
//! A hold that is not repaired and lasts [`escalate_after`] becomes an
//! operator ask ([`escalate`]), once per hold. Rescue refs, and the
//! `loom-rescue-` stash entries they are paired with, are pruned after
//! [`retain`] (#13977).

mod classify;
mod managed;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, NaiveDateTime, Utc};

use self::classify::{Plan, Verdict};
use super::{git, CheckoutReport, CheckoutState, Env, Level, Memory, Net, Transition};

/// The prefix every rescue ref lives under.
pub(super) const RESCUE_PREFIX: &str = "refs/loom/rescue/";

/// The stash subject the repair's step 3 writes, before its [`stamp`]: the
/// message is exactly this prefix plus the timestamp, so a stash entry's age
/// is readable from its own name.
const STASH_PREFIX: &str = "loom-rescue-";

const ESCALATE_ENV: &str = "LOOM_CHECKOUT_ESCALATE_AFTER_MINUTES";
const REREPORT_ENV: &str = "LOOM_CHECKOUT_REREPORT_AFTER_MINUTES";
const DEFAULT_REREPORT_MINUTES: u64 = 60;
const RETAIN_ENV: &str = "LOOM_RESCUE_REF_RETAIN_DAYS";
const DEFAULT_ESCALATE_MINUTES: u64 = 120;
const DEFAULT_RETAIN_DAYS: u64 = 14;

fn env_number(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(default)
}

/// How long a hold may stand before it is escalated.
pub(super) fn escalate_after() -> Duration {
    Duration::from_secs(env_number(ESCALATE_ENV, DEFAULT_ESCALATE_MINUTES) * 60)
}

/// How often a standing `wrong-branch` hold is said again (#13588).
pub(super) fn rereport_after() -> Duration {
    Duration::from_secs(env_number(REREPORT_ENV, DEFAULT_REREPORT_MINUTES) * 60)
}

/// How long a rescue ref, and the `loom-rescue-` stash beside it, is kept.
pub(super) fn retain() -> Duration {
    Duration::from_secs(env_number(RETAIN_ENV, DEFAULT_RETAIN_DAYS) * 86_400)
}

/// One repair step or outcome, turned into a [`Transition`] (one log line and
/// one event on the checkout topic) once the pass has the checkout's report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repair {
    pub kind: &'static str,
    pub level: Level,
    pub line: String,
}

fn say(net: &mut Net, root: &Path, kind: &'static str, level: Level, what: String) {
    let line = format!("{}: self-heal: {what}", root.display());
    match level {
        Level::Info => log::info!("checkout_ff: {line}"),
        Level::Warn => log::warn!("checkout_ff: {line}"),
        Level::Error => log::error!("checkout_ff: {line}"),
    }
    net.repairs.push(Repair { kind, level, line });
}

/// Hold the checkout against the self-update for a write, or `None`: nothing
/// may be written now.
fn writable(env: &Env<'_>, root: &Path) -> Option<super::MoveHold> {
    if !env.write || (env.held)().is_some() || (env.gate_in_flight)(root) {
        return None;
    }
    (env.hold)(root)
}

/// Repair `root` if everything blocking its fast-forward is Loom's.
pub(super) fn heal(env: &Env<'_>, root: &Path, branch: &str, net: &mut Net) {
    let Some(gd) = classify::git_dir(env, root) else {
        return;
    };
    rehome(env, root, branch, &gd, net);
    let plan = match classify::classify(env, root, branch, &gd) {
        Verdict::Clean => None,
        Verdict::Leave(why) => {
            log::debug!("checkout_ff: {}: self-heal: left alone: {why}", root.display());
            None
        }
        Verdict::Repair(plan) => Some(plan),
    };
    let expired = expired_rescue_refs(env, root, (env.clock)(), retain());
    let expired_stashes =
        expired_rescue_stashes(root, (env.clock)(), retain(), env.bound(git::LOCAL_TIMEOUT));
    if plan.is_none() && expired.is_empty() && expired_stashes.is_empty() {
        return;
    }
    let Some(_hold) = writable(env, root) else {
        return;
    };
    for name in &expired {
        let args = ["update-ref", "-d", name];
        match git::ok(root, &args, env.bound(git::LOCAL_TIMEOUT)) {
            Ok(_) => say(net, root, "rescue-pruned", Level::Info, format!("pruned {name}")),
            Err(e) => log::warn!("checkout_ff: {}: could not prune {name}: {e}", root.display()),
        }
    }
    prune_rescue_stashes(root, &expired_stashes, env.bound(git::LOCAL_TIMEOUT), net);
    if let Some(plan) = plan {
        if let Err(e) = repair(env, root, branch, &plan, net) {
            say(
                net,
                root,
                "repair-failed",
                Level::Warn,
                format!("stopped, checkout left as is: {e}"),
            );
        }
    }
}

/// A checkout left on another branch, or detached, whose HEAD holds nothing
/// that is not on `origin/<branch>` or a merged pull request's head (#13588)
/// goes back on `branch`; the rules after this fast-forward it. Anything in
/// doubt is left, and reported as `wrong-branch` as before.
fn rehome(env: &Env<'_>, root: &Path, branch: &str, gd: &Path, net: &mut Net) {
    let strand = match classify::stranded(env, root, branch, gd) {
        Ok(Some(strand)) => strand,
        Ok(None) => return,
        Err(why) => {
            log::debug!("checkout_ff: {}: self-heal: stays off `{branch}`: {why}", root.display());
            return;
        }
    };
    let Some(_hold) = writable(env, root) else {
        return;
    };
    let result = (|| -> Result<(), String> {
        // A rescue ref already at this HEAD is the same way back, so reuse it
        // rather than making one per pass (#13835): the checkout below can
        // fail for a reason that holds pass after pass (the default branch is
        // checked out in another worktree, an untracked file is in the way),
        // and each new ref then sat until it was pruned 14 days later.
        let rescue = match existing_rescue_ref(root, &strand.head) {
            Some(found) => found,
            None => {
                let made = rescue_ref(root, &stamp((env.clock)()), "", &strand.head)?;
                say(
                    net,
                    root,
                    "rescue-ref",
                    Level::Info,
                    format!("rescue ref {made} -> {}", strand.head),
                );
                made
            }
        };
        let own = format!("refs/heads/{branch}");
        let exists =
            git::ok(root, &["rev-parse", "--verify", "--quiet", &own], git::LOCAL_TIMEOUT).is_ok();
        let tracking = git::tracking(branch);
        let args: Vec<&str> = if exists {
            vec!["checkout", "--quiet", branch]
        } else {
            vec!["checkout", "--quiet", "-B", branch, "--track", &tracking]
        };
        let out = git::run(root, &["core.hooksPath=/dev/null"], &args, git::MERGE_TIMEOUT)?;
        if !out.status.success() {
            return Err(format!("git {} failed: {}", args.join(" "), git::stderr(&out)));
        }
        let was = strand
            .from
            .as_deref()
            .map_or_else(|| "a detached HEAD".to_string(), |b| format!("`{b}`"));
        // Nothing was carried when every commit on HEAD was already on
        // `origin/<branch>`; the clause would read "(0 commit(s) ...)" (#13835).
        let carried = if strand.unique == 0 {
            String::new()
        } else {
            format!(" ({} commit(s) only on a merged PR)", strand.unique)
        };
        say(
            net,
            root,
            "rehomed",
            Level::Warn,
            format!("put back on `{branch}` from {was}{carried}, recoverable from {rescue}"),
        );
        Ok(())
    })();
    if let Err(e) = result {
        say(
            net,
            root,
            "repair-failed",
            Level::Warn,
            format!("stopped, checkout left as is: {e}"),
        );
    }
}

fn stamp(at: DateTime<Utc>) -> String {
    at.format("%Y%m%dT%H%M%SZ").to_string()
}

/// An existing rescue ref already pointing at `sha`, if there is one (#13835).
/// Any of them is an equally good way back to that commit, so the first by
/// refname (`for-each-ref`'s own order, so the answer is stable across passes)
/// is taken. `None` on a failed read: the caller then makes a new ref, which is
/// the behaviour from before this reuse existed.
fn existing_rescue_ref(root: &Path, sha: &str) -> Option<String> {
    let args = [
        "for-each-ref",
        "--format=%(refname)",
        "--points-at",
        sha,
        RESCUE_PREFIX,
    ];
    let listed = git::ok(root, &args, git::LOCAL_TIMEOUT).ok()?;
    listed.lines().next().map(str::to_string)
}

/// A rescue ref at `sha` named `refs/loom/rescue/<ts><suffix>`, never over an
/// existing one. Returns its name.
fn rescue_ref(root: &Path, ts: &str, suffix: &str, sha: &str) -> Result<String, String> {
    for n in 0..50 {
        let tail = if n == 0 {
            String::new()
        } else {
            format!("-{n}")
        };
        let name = format!("{RESCUE_PREFIX}{ts}{tail}{suffix}");
        // An empty old value: the ref must not exist yet.
        let made = git::ok(root, &["update-ref", &name, sha, ""], git::LOCAL_TIMEOUT);
        match made {
            Ok(_) => return Ok(name),
            Err(e) if n == 49 => return Err(format!("could not make a rescue ref: {e}")),
            Err(_)
                if git::ok(
                    root,
                    &["rev-parse", "--verify", "--quiet", &name],
                    git::LOCAL_TIMEOUT,
                )
                .is_ok() => {}
            Err(e) => return Err(format!("could not make a rescue ref {name}: {e}")),
        }
    }
    Err("no free rescue ref name".to_string())
}

fn remove_lock(plan: &Plan, root: &Path, net: &mut Net) -> Result<(), String> {
    let Some(lock) = &plan.lock else {
        return Ok(());
    };
    std::fs::remove_file(lock).map_err(|e| format!("could not remove {}: {e}", lock.display()))?;
    let what = format!("removed stale empty {} ({} min old)", lock.display(), plan.lock_age_min);
    say(net, root, "lock-removed", Level::Warn, what);
    Ok(())
}

fn repair(
    env: &Env<'_>,
    root: &Path,
    branch: &str,
    plan: &Plan,
    net: &mut Net,
) -> Result<(), String> {
    let ts = stamp((env.clock)());
    let long = git::MERGE_TIMEOUT;
    let hooks_off = ["core.hooksPath=/dev/null"];
    let write = |args: &[&str]| -> Result<(), String> {
        let out = git::run(root, &hooks_off, args, long)?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!("git {} failed: {}", args.join(" "), git::stderr(&out)))
        }
    };

    if !plan.reset {
        return remove_lock(plan, root, net);
    }
    // 1. The way back, before anything changes.
    let head = git::ok(root, &["rev-parse", "HEAD"], git::LOCAL_TIMEOUT)?;
    let rescue = rescue_ref(root, &ts, "", &head)?;
    say(net, root, "rescue-ref", Level::Info, format!("rescue ref {rescue} -> {head}"));
    if plan.branch_differs {
        let own = format!("refs/heads/{branch}");
        let tip = git::ok(root, &["rev-parse", &own], git::LOCAL_TIMEOUT)?;
        let name = rescue_ref(root, &ts, "-branch", &tip)?;
        say(net, root, "rescue-ref", Level::Info, format!("rescue ref {name} -> {tip}"));
    }

    // 2. A stale lock would fail the stash.
    if let Some(lock) = &plan.lock {
        std::fs::remove_file(lock)
            .map_err(|e| format!("could not remove {}: {e}", lock.display()))?;
        say(
            net,
            root,
            "lock-removed",
            Level::Warn,
            format!("removed stale empty {} ({} min old)", lock.display(), plan.lock_age_min),
        );
    }

    // 3. Dirty Loom paths, kept.
    if plan.op.is_none() && !plan.dirty.is_empty() {
        let name = format!("{STASH_PREFIX}{ts}");
        let mut args = vec!["stash", "push", "--quiet", "-m", name.as_str(), "--"];
        args.extend(plan.dirty.iter().map(String::as_str));
        write(&args)?;
        say(
            net,
            root,
            "stash",
            Level::Info,
            format!("stashed {} path(s) as {name}", plan.dirty.len()),
        );
    }

    // 4. The operation.
    if let Some(op) = plan.op {
        write(&op.abort_args())?;
        say(
            net,
            root,
            "abort",
            Level::Info,
            format!("ran git {}", op.abort_args().join(" ")),
        );
        match git::current_branch(root, git::LOCAL_TIMEOUT)? {
            Some(on) if on == branch => {}
            other => return Err(format!("after the abort HEAD is on {other:?}, not `{branch}`")),
        }
    }

    // 4b. A HEAD left detached goes back on the branch, at origin's tip
    // (#11989). The old branch tip has its rescue ref (`branch_differs`); a
    // file git would overwrite stops the checkout, and untracked ones are kept.
    if plan.detached {
        let tracking = git::tracking(branch);
        write(&["checkout", "--quiet", "-B", branch, "--track", &tracking])?;
        say(
            net,
            root,
            "reattached",
            Level::Info,
            format!("detached HEAD put back on `{branch}` at origin/{branch}"),
        );
    }

    // 5. Only Loom's paths differ.
    write(&["reset", "--hard", "--quiet", &git::tracking(branch)])?;
    let what = format!(
        "reset `{branch}` to origin/{branch} ({} local commit(s), {} dirty path(s){}), recoverable from {rescue}",
        plan.commits,
        plan.dirty.len(),
        plan.op.map_or(String::new(), |op| format!(", {op:?} aborted")),
    );
    say(net, root, "repaired", Level::Warn, what);
    Ok(())
}

/// Rescue refs older than `keep` at `now`. A name that does not carry a
/// timestamp is kept, and so is every ref outside [`RESCUE_PREFIX`].
pub(super) fn expired_rescue_refs(
    env: &Env<'_>,
    root: &Path,
    now: DateTime<Utc>,
    keep: Duration,
) -> Vec<String> {
    let args = ["for-each-ref", "--format=%(refname)", RESCUE_PREFIX];
    let Ok(listed) = git::ok(root, &args, env.bound(git::LOCAL_TIMEOUT)) else {
        return Vec::new();
    };
    listed
        .lines()
        .filter(|name| {
            let ts = name.strip_prefix(RESCUE_PREFIX).and_then(|t| t.get(..16));
            ts.and_then(|t| NaiveDateTime::parse_from_str(t, "%Y%m%dT%H%M%SZ").ok())
                .is_some_and(|t| {
                    now.signed_duration_since(t.and_utc())
                        .to_std()
                        .is_ok_and(|age| age > keep)
                })
        })
        .map(str::to_string)
        .collect()
}

/// The timestamp a `loom-rescue-<ts>` stash subject carries, read the same way
/// [`expired_rescue_refs`] reads a ref name's: no reflog dates needed. The
/// stash step names the entry exactly [`STASH_PREFIX`] plus the 16-character
/// stamp, so anything else — a foreign subject, a missing, malformed or
/// trailing-suffixed timestamp — is `None`, and the caller keeps the entry.
fn rescue_stash_ts(subject: &str) -> Option<NaiveDateTime> {
    let ts = subject.rsplit_once(STASH_PREFIX)?.1;
    if ts.len() != 16 {
        return None;
    }
    NaiveDateTime::parse_from_str(ts, "%Y%m%dT%H%M%SZ").ok()
}

/// An expired `loom-rescue-<ts>` stash entry: its commit sha, the durable
/// handle, and the `stash@{N}` selector it had when it was listed, which is
/// not durable and is only a sort key.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RescueStash {
    sha: String,
    selector: String,
}

/// `loom-rescue-<ts>` stash entries older than `keep` at `now`, highest
/// listed index first (belt-and-braces: [`prune_rescue_stashes`] re-resolves
/// each selector from the sha, #13977). A subject that does not carry a
/// readable stamp is kept ([`rescue_stash_ts`]), and so is every entry outside
/// the prefix. An empty stack, or a failed read, lists nothing.
fn expired_rescue_stashes(
    root: &Path,
    now: DateTime<Utc>,
    keep: Duration,
    timeout: Duration,
) -> Vec<RescueStash> {
    let args = ["stash", "list", "--format=%gd%x00%H%x00%gs"];
    let Ok(listed) = git::ok(root, &args, timeout) else {
        return Vec::new();
    };
    let mut expired: Vec<RescueStash> = listed
        .lines()
        .filter_map(|line| {
            let mut f = line.splitn(3, '\x00');
            Some((f.next()?, f.next()?, f.next()?))
        })
        .filter(|(_, _, subject)| {
            rescue_stash_ts(subject).is_some_and(|ts| {
                now.signed_duration_since(ts.and_utc())
                    .to_std()
                    .is_ok_and(|age| age > keep)
            })
        })
        .map(|(selector, sha, _)| RescueStash {
            sha: sha.to_string(),
            selector: selector.to_string(),
        })
        .collect();
    expired.sort_by_key(|e| std::cmp::Reverse(stash_index(&e.selector)));
    expired
}

/// The `N` of a `stash@{N}` selector.
fn stash_index(selector: &str) -> Option<u32> {
    selector
        .strip_prefix("stash@{")?
        .strip_suffix('}')?
        .parse()
        .ok()
}

/// The entry's current selector, re-read from the live stack by commit sha: a
/// concurrent push to the shared `refs/stash` shifts every index, so a
/// selector captured at list time is not an identity (the convention in
/// `stash_retirement::current_stash_ref_for_commit`). `Ok(None)`: the entry is
/// already gone.
fn current_selector(root: &Path, sha: &str, timeout: Duration) -> Result<Option<String>, String> {
    let listed = git::ok(root, &["stash", "list", "--format=%gd%x00%H"], timeout)
        .map_err(|e| e.to_string())?;
    Ok(listed.lines().find_map(|l| {
        let (sel, h) = l.split_once('\x00')?;
        (h == sha).then(|| sel.to_string())
    }))
}

/// Drop the listed stash entries, each addressed by its sha: the selector is
/// re-resolved immediately before its drop. An entry no longer listed is
/// already gone (a no-op). A failed drop only warns: the entry outlives this
/// pass and the next pass re-lists and tries again. A push landing in the
/// instant between the re-resolve and the drop can still shift the index; git
/// offers no expected-sha precondition on `stash drop`, so that window is
/// narrowed to two spawns, not closed. The rescue ref at that timestamp stays
/// until its own expiry, so only the working-tree diff is lost.
fn prune_rescue_stashes(root: &Path, expired: &[RescueStash], timeout: Duration, net: &mut Net) {
    for entry in expired {
        let selector = match current_selector(root, &entry.sha, timeout) {
            Ok(Some(selector)) => selector,
            Ok(None) => continue,
            Err(e) => {
                log::warn!("checkout_ff: {}: could not re-list stashes: {e}", root.display());
                continue;
            }
        };
        let args = ["stash", "drop", "--quiet", selector.as_str()];
        match git::ok(root, &args, timeout) {
            Ok(_) => {
                say(net, root, "rescue-pruned", Level::Info, format!("pruned stash {selector}"))
            }
            Err(e) => {
                log::warn!("checkout_ff: {}: could not prune stash {selector}: {e}", root.display())
            }
        }
    }
}

// ============================================================================
// Escalation
// ============================================================================

/// The holds already escalated, so a standing one is asked about once.
#[derive(Debug, Default)]
pub struct Ledger {
    /// Root -> the `since` of the hold that was escalated.
    asked: HashMap<PathBuf, DateTime<Utc>>,
    /// Root -> (the `since` of a standing `wrong-branch` hold, when it was
    /// last said).
    said: HashMap<PathBuf, (DateTime<Utc>, DateTime<Utc>)>,
}

/// The holds the self-heal does not clear: a person's or an agent's work.
fn is_hold(state: CheckoutState) -> bool {
    matches!(
        state,
        CheckoutState::MidOperation
            | CheckoutState::Ahead
            | CheckoutState::Diverged
            | CheckoutState::Dirty
            | CheckoutState::WouldOverwrite
    )
}

/// The command that clears `report`'s hold, once a person has looked.
fn command(report: &CheckoutReport) -> String {
    let root = report.root.display();
    let branch = report.branch.as_deref().unwrap_or("<default>");
    let abort = ["rebase", "merge", "cherry-pick"]
        .iter()
        .find(|op| {
            let marker = match **op {
                "rebase" => ".git/rebase-merge",
                "merge" => ".git/MERGE_HEAD",
                _ => ".git/CHERRY_PICK_HEAD",
            };
            report.root.join(marker).exists()
        })
        .map_or(String::new(), |op| format!("git -C {root} {op} --abort && "));
    format!(
        "inspect with `git -C {root} status`; if nothing there is worth keeping: `{abort}git -C \
         {root} reset --hard origin/{branch}` (keep a copy first: `git -C {root} update-ref \
         {RESCUE_PREFIX}manual HEAD`)"
    )
}

/// Push one `escalated` transition for a hold that has stood for
/// `after` without being repaired, once per hold.
pub(super) fn escalate(
    ledger: &mut Ledger,
    report: &CheckoutReport,
    now: DateTime<Utc>,
    after: Duration,
    out: &mut Vec<Transition>,
) {
    if !is_hold(report.state) {
        ledger.asked.remove(&report.root);
        return;
    }
    let stood = now
        .signed_duration_since(report.since)
        .to_std()
        .unwrap_or_default();
    if stood < after || ledger.asked.get(&report.root) == Some(&report.since) {
        return;
    }
    ledger.asked.insert(report.root.clone(), report.since);
    let line = format!(
        "{}: OPERATOR ASK: held `{}` for {} min and not Loom-only, so it is not repaired automatically: {}",
        report.root.display(),
        report.state.as_str(),
        stood.as_secs() / 60,
        command(report),
    );
    out.push(Transition {
        kind: "escalated",
        level: Level::Error,
        previous: None,
        line,
        report: report.clone(),
    });
}

/// Say a standing `wrong-branch` hold again every `every` (#13588): the
/// ordinary transition is logged once, and a checkout left off its default
/// branch goes stale silently. One `still-held` transition (a log line and an
/// event on the checkout topic) per interval, so the hold can be counted.
pub(super) fn rereport(
    ledger: &mut Ledger,
    report: &CheckoutReport,
    now: DateTime<Utc>,
    every: Duration,
    out: &mut Vec<Transition>,
) {
    if report.state != CheckoutState::WrongBranch {
        ledger.said.remove(&report.root);
        return;
    }
    let last = match ledger.said.get(&report.root) {
        Some((since, last)) if *since == report.since => *last,
        _ => report.since,
    };
    if now.signed_duration_since(last).to_std().unwrap_or_default() < every {
        return;
    }
    ledger.said.insert(report.root.clone(), (report.since, now));
    let stood = now
        .signed_duration_since(report.since)
        .to_std()
        .unwrap_or_default();
    out.push(Transition {
        kind: "still-held",
        level: Level::Warn,
        previous: None,
        line: format!(
            "{}: still `wrong-branch` after {} min, not repaired: {}",
            report.root.display(),
            stood.as_secs() / 60,
            report.detail.as_deref().unwrap_or("off the default branch"),
        ),
        report: report.clone(),
    });
}

/// Turn a checkout's repair steps into transitions, and escalate its hold.
pub(super) fn after(
    env: &Env<'_>,
    memory: &mut Memory,
    net: &mut Net,
    report: &CheckoutReport,
    out: &mut Vec<Transition>,
) {
    out.extend(net.repairs.drain(..).map(|r| Transition {
        kind: r.kind,
        level: r.level,
        previous: None,
        line: r.line,
        report: report.clone(),
    }));
    escalate(&mut memory.escalated, report, (env.clock)(), escalate_after(), out);
    rereport(&mut memory.escalated, report, (env.clock)(), rereport_after(), out);
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// `now`, fixed, so test stamps are derived and never drift.
    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 17, 12, 0, 0).unwrap()
    }

    fn rescue(days_ago: i64) -> String {
        format!("loom-rescue-{}", stamp(now() - chrono::Duration::days(days_ago)))
    }

    #[test]
    fn a_rescue_stash_subject_parses_the_stamp_it_was_named_with() {
        let got = rescue_stash_ts("On master: loom-rescue-20200101T000000Z").unwrap();
        assert_eq!(got.and_utc().timestamp(), 1_577_836_800);
        // Detached HEAD spells the subject differently; the stamp is the same.
        let parsed = rescue_stash_ts(&format!("On (no branch): {}", rescue(30))).unwrap();
        assert_eq!(parsed.and_utc().timestamp(), (now() - chrono::Duration::days(30)).timestamp());
    }

    #[test]
    fn a_foreign_stash_subject_has_no_rescue_stamp() {
        assert!(rescue_stash_ts("On master: loom-quarantine: issue=1").is_none());
        assert!(rescue_stash_ts("WIP on master: abc1234 init").is_none());
        assert!(rescue_stash_ts("On master: stashed by hand").is_none());
    }

    #[test]
    fn a_malformed_or_suffixed_stamp_is_kept() {
        // No stamp at all.
        assert!(rescue_stash_ts("On master: loom-rescue-").is_none());
        // The stamp is the whole message: a suffix means it was not ours.
        assert!(rescue_stash_ts("On master: loom-rescue-20200101T000000Z-1").is_none());
        // Short and malformed stamps.
        assert!(rescue_stash_ts("On master: loom-rescue-20200101T000000").is_none());
        assert!(rescue_stash_ts("On master: loom-rescue-not-a-stamp").is_none());
        assert!(rescue_stash_ts("On master: loom-rescue-20201301T000000Z").is_none());
    }

    /// A throwaway repo with one commit and one stash per message, pushed in
    /// order — so the last message sits at `stash@{0}`.
    fn repo_with_stashes(msgs: &[String]) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let g = |args: &[&str]| git::ok(&root, args, git::LOCAL_TIMEOUT).unwrap();
        g(&["init", "--quiet"]);
        g(&["config", "user.email", "loom@example.invalid"]);
        g(&["config", "user.name", "loom"]);
        std::fs::write(root.join("f.txt"), "one\n").unwrap();
        g(&["add", "."]);
        g(&["commit", "--quiet", "-m", "init"]);
        for msg in msgs {
            let mut body = std::fs::read_to_string(root.join("f.txt")).unwrap();
            body.push_str("more\n");
            std::fs::write(root.join("f.txt"), body).unwrap();
            g(&["stash", "push", "--quiet", "-m", msg]);
        }
        (tmp, root)
    }

    #[test]
    fn the_sweep_lists_only_expired_rescue_stashes() {
        let keep = retain();
        let (tmp, root) = repo_with_stashes(&[
            rescue(30),
            rescue(1),
            "loom-quarantine: issue=1".to_string(),
            "WIP on master: abc1234 init".to_string(),
        ]);
        // Pushed first, so the old rescue sits at the highest index; every
        // other entry — young rescue, quarantine, foreign — is left.
        let selectors: Vec<String> = expired_rescue_stashes(&root, now(), keep, git::LOCAL_TIMEOUT)
            .into_iter()
            .map(|e| e.selector)
            .collect();
        assert_eq!(selectors, vec!["stash@{3}".to_string()]);
        drop(tmp);
    }

    #[test]
    fn expired_stashes_drop_newest_index_first_and_the_rest_survive() {
        let keep = retain();
        let (tmp, root) = repo_with_stashes(&[rescue(40), rescue(30), rescue(1)]);
        let mut net = Net::default();
        let expired = expired_rescue_stashes(&root, now(), keep, git::LOCAL_TIMEOUT);
        let selectors: Vec<&str> = expired.iter().map(|e| e.selector.as_str()).collect();
        assert_eq!(selectors, vec!["stash@{2}", "stash@{1}"]);
        prune_rescue_stashes(&root, &expired, git::LOCAL_TIMEOUT, &mut net);
        let left = git::ok(&root, &["stash", "list", "--format=%gs"], git::LOCAL_TIMEOUT).unwrap();
        assert_eq!(left.lines().count(), 1);
        assert!(left.contains(&rescue(1)));
        drop(tmp);
    }

    fn head_stash_sha(root: &Path) -> String {
        git::ok(root, &["rev-parse", "stash@{0}"], git::LOCAL_TIMEOUT)
            .unwrap()
            .trim()
            .to_string()
    }

    #[test]
    fn a_push_between_list_and_prune_does_not_redirect_the_drop() {
        let keep = retain();
        let (tmp, root) = repo_with_stashes(&[
            rescue(40),
            rescue(1),
            "loom-quarantine: issue=1".to_string(),
        ]);
        let mut net = Net::default();
        let expired = expired_rescue_stashes(&root, now(), keep, git::LOCAL_TIMEOUT);
        assert_eq!(expired.len(), 1);
        // Two concurrent pushes land after the list: `stash@{2}` now names
        // the young rescue, and the old selector is stale.
        let mut body = std::fs::read_to_string(root.join("f.txt")).unwrap();
        for msg in ["loom-quarantine: issue=777", "loom-quarantine: issue=778"] {
            body.push_str("x\n");
            std::fs::write(root.join("f.txt"), &body).unwrap();
            git::ok(&root, &["stash", "push", "--quiet", "-m", msg], git::LOCAL_TIMEOUT).unwrap();
        }
        prune_rescue_stashes(&root, &expired, git::LOCAL_TIMEOUT, &mut net);
        let left = git::ok(&root, &["stash", "list", "--format=%gs"], git::LOCAL_TIMEOUT).unwrap();
        assert_eq!(left.lines().count(), 4);
        assert!(!left.contains(&rescue(40)));
        assert!(left.contains(&rescue(1)));
        assert!(left.contains("loom-quarantine: issue=1"));
        // Already gone: a second prune is a silent no-op.
        let before = net.repairs.len();
        prune_rescue_stashes(&root, &expired, git::LOCAL_TIMEOUT, &mut net);
        assert_eq!(net.repairs.len(), before);
        drop(tmp);
    }

    #[test]
    fn each_prune_is_said_as_rescue_pruned_and_a_failed_drop_only_warns() {
        let (tmp, root) = repo_with_stashes(&[rescue(30)]);
        let mut net = Net::default();
        // `stash@{9}` does not exist: a failed drop is a warning, not a step.
        prune_rescue_stashes(
            &root,
            &[
                RescueStash {
                    sha: head_stash_sha(&root),
                    selector: "stash@{0}".to_string(),
                },
                RescueStash {
                    sha: "0".repeat(40),
                    selector: "stash@{9}".to_string(),
                },
            ],
            git::LOCAL_TIMEOUT,
            &mut net,
        );
        assert_eq!(net.repairs.len(), 1);
        assert_eq!(net.repairs[0].kind, "rescue-pruned");
        assert_eq!(net.repairs[0].level, Level::Info);
        assert!(net.repairs[0].line.contains("pruned stash stash@{0}"));
        drop(tmp);
    }

    #[test]
    fn an_empty_stash_stack_lists_and_prunes_nothing() {
        let (tmp, root) = repo_with_stashes(&[]);
        assert!(expired_rescue_stashes(&root, now(), Duration::from_secs(1), git::LOCAL_TIMEOUT)
            .is_empty());
        let mut net = Net::default();
        prune_rescue_stashes(&root, &[], git::LOCAL_TIMEOUT, &mut net);
        assert!(net.repairs.is_empty());
        drop(tmp);
    }
}
