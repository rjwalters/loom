//! Self-heal a main checkout that only Loom's own stale install blocks
//! (#11425, tracker #10698). See "Self-heal" in `daemon-reference.md`.
//!
//! The fast-forward stops at `mid-operation`, `ahead`, `diverged` and `dirty`,
//! which is right when a person or an agent left work there. On fleet workers
//! it is often only a resync that never landed: `chore: resync installed Loom
//! surfaces` commits whose push a ruleset refused, modified installed files,
//! or the rebase that same script abandoned. [`heal`] runs before the rules.
//! When **every** blocker is Loom-owned ([`classify`]) it:
//!
//! 1. makes `refs/loom/rescue/<ts>` at HEAD (and `<ts>-branch` at the branch
//!    tip, when an operation moved HEAD off it);
//! 2. removes a stale, empty `index.lock`;
//! 3. stashes the dirty Loom paths as `loom-rescue-<ts>` (not during an
//!    operation: its abort discards them, and the rescue ref holds the work);
//! 4. aborts the operation;
//! 5. `reset --hard origin/<default>`.
//!
//! Any failure stops the repair and the ordinary rules report the checkout as
//! before. Untracked files are never touched. Nothing is written while
//! `fleet.autoApply` is off, the host is held, a gate run is building there or
//! the self-update is running there.
//!
//! A hold that is not repaired and lasts [`escalate_after`] becomes an
//! operator ask ([`escalate`]), once per hold. Rescue refs are pruned after
//! [`retain`].

mod classify;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, NaiveDateTime, Utc};

use self::classify::{Plan, Verdict};
use super::{git, CheckoutReport, CheckoutState, Env, Level, Memory, Net, Transition};

/// The prefix every rescue ref lives under.
pub(super) const RESCUE_PREFIX: &str = "refs/loom/rescue/";

const ESCALATE_ENV: &str = "LOOM_CHECKOUT_ESCALATE_AFTER_MINUTES";
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

/// How long a rescue ref is kept.
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
    let plan = match classify::classify(env, root, branch, &gd) {
        Verdict::Clean => None,
        Verdict::Leave(why) => {
            log::debug!("checkout_ff: {}: self-heal: left alone: {why}", root.display());
            None
        }
        Verdict::Repair(plan) => Some(plan),
    };
    let expired = expired_rescue_refs(env, root, (env.clock)(), retain());
    if plan.is_none() && expired.is_empty() {
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

fn stamp(at: DateTime<Utc>) -> String {
    at.format("%Y%m%dT%H%M%SZ").to_string()
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
        let name = format!("loom-rescue-{ts}");
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

// ============================================================================
// Escalation
// ============================================================================

/// The holds already escalated, so a standing one is asked about once.
#[derive(Debug, Default)]
pub struct Ledger {
    /// Root -> the `since` of the hold that was escalated.
    asked: HashMap<PathBuf, DateTime<Utc>>,
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
}
