//! In-flight model fallback — a step killed by a model limit is re-dispatched
//! once on the next rung of `autonomous.modelFallback.chain` (#13905).
//!
//! #13873's selection-time fallback ([`super::model_fallback`]) answers a
//! launch that *starts* against a class-exhausted pool. This module covers a
//! child that was already running when the per-model limit hit
//! (`You've reached your Fable 5 limit`, `hit your Fable limit`): the step
//! dies mid-run, and before #13905 the death was charged to the item (the
//! Builder fast-fail ladder, the repeat cap) and the redraw re-launched the
//! same pinned model into the same wall.
//!
//! # The three pieces
//!
//! 1. **Classify** ([`killed_model_class`]): the tail of the dead step's
//!    current dispatch names a model class in a limit banner. A banner that
//!    names no model (`hit your weekly limit`) is an account-wide fault no
//!    other rung escapes, so it is not this module's and stays as it is today.
//! 2. **Charge nothing** ([`observe_step_end`] → the step's `provider_exhausted`
//!    end, #13966): the death is the provider's, not the item's, so the ledger
//!    records the uncharged refusal and the next draw re-picks the item at
//!    once. Mirrors the quarantine-tally exemption an issue sweep's insta-crash
//!    already has (`SweepRegistry::insta_crash_is_account_exhaustion`).
//! 3. **Step down once** ([`step_down`]): a memo `(item, role) → killed class`
//!    makes the redraw's launch take the first chain rung strictly below the
//!    killed class. A kill at that rung records a new memo (one rung per kill,
//!    never a loop at the same model); a kill at the last rung records none,
//!    and the death is handled as today; a step that ends any other way clears
//!    the memo, so nothing outlives the re-dispatch it was made for.
//!
//! The memo also lapses after [`MEMO_TTL`]: limits reset, so a redraw that
//! comes long after the kill goes back to the configured model.
//!
//! Default OFF with `modelFallback`: an absent or disabled block records
//! nothing and changes nothing (the issue's AC).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use regex::Regex;

use super::model_fallback::{read_model_fallback_config, ModelFallbackDecision};
use crate::tokens_pool::bad_tokens::model_class_of;

/// Why a launch ran below its configured model after its predecessor was
/// killed by a model limit (#13905). Rides the step span's
/// `loom.step.model_fallback_reason`, next to #13873's
/// `no-account-for-class`.
pub(crate) const REASON_MODEL_LIMIT_KILL: &str = "model-limit-kill";

/// The memo's `role` for a legacy `SweepKind::Issue` sweep (#14443). One
/// process runs the whole lifecycle, so there is no per-role key: the sweep is
/// one pseudo-role, distinct from every [`StepRole`](crate::step_role::StepRole)
/// name so a step and a sweep on the same item never share a memo.
pub(crate) const ISSUE_SWEEP_ROLE: &str = "issue-sweep";

/// How long a kill's step-down stays pending. Past it the redraw launches the
/// configured model again: per-model limits are windows, not permanent.
const MEMO_TTL: Duration = Duration::from_secs(3600);

/// How many trailing lines of the dead step's current dispatch are read for
/// the banner. A CLI that dies on its limit prints the banner last; a
/// transcript that merely *quotes* the phrase (a step reading this very
/// issue's text) is far from the end, so a narrow window keeps a healthy
/// step's own output from reading as a kill.
pub(crate) const BANNER_TAIL_LINES: usize = 15;

/// `reached|hit your <up to three words> limit`, capturing the words. The
/// filler is bounded like [`super::crash_signals::model_limit_signature`] so a
/// sentence that merely ends in "limit" cannot match.
fn banner() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(?:reached|hit) your ((?:\S+\s+){1,3}?)limit")
            .expect("valid static model-limit banner regex")
    })
}

/// The model class a limit banner in `log_tail` names (`fable`, `opus`, …),
/// or `None` when no line names one: no banner, a banner naming no model
/// (account-wide), or a concurrent-session capacity line (#3947), which is a
/// capacity fault no rung fixes. Pure.
#[must_use]
pub(crate) fn killed_model_class(log_tail: &str) -> Option<String> {
    log_tail
        .lines()
        .filter(|line| !super::crash_signals::session_capacity_exclusion().is_match(line))
        .filter_map(|line| banner().captures(line))
        .find_map(|caps| {
            caps[1].split_whitespace().find_map(|word| {
                model_class_of(word.trim_matches(|c: char| !c.is_ascii_alphanumeric()))
            })
        })
}

/// The first chain entry strictly below `class`'s position in `chain`: the
/// entry's raw name. `None` when `class` is not in the chain (no provable
/// "below") or nothing recognized follows it (the last rung). Pure.
#[must_use]
pub(crate) fn next_rung(chain: &[String], class: &str) -> Option<String> {
    let classes: Vec<Option<String>> = chain.iter().map(|e| model_class_of(e)).collect();
    let at = position_of(&classes, class)?;
    chain
        .iter()
        .zip(classes.iter())
        .skip(at + 1)
        .find(|(_, c)| c.as_deref().is_some_and(|c| !c.eq_ignore_ascii_case(class)))
        .map(|(entry, _)| entry.trim().to_string())
}

fn position_of(classes: &[Option<String>], class: &str) -> Option<usize> {
    classes
        .iter()
        .position(|c| c.as_deref().is_some_and(|c| c.eq_ignore_ascii_case(class)))
}

/// `(workspace, item, role)` → the class a step on that pair was last killed
/// at, with when. Process-global rather than a registry field for the reasons
/// `capture::STEP_PROVIDER_EXHAUSTIONS` is: the writer is the step end, the
/// reader is the spawn-time launch resolve, and neither holds the other's
/// state.
type Key = (PathBuf, u32, String);
static MEMOS: Mutex<BTreeMap<Key, (String, Instant)>> = Mutex::new(BTreeMap::new());

fn key(root: &Path, item: u32, role: &str) -> Key {
    (root.to_path_buf(), item, role.to_string())
}

fn lock() -> std::sync::MutexGuard<'static, BTreeMap<Key, (String, Instant)>> {
    MEMOS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What a step's end tells this module, resolved by the caller from its own
/// facts. `clean` is "the step ended in a way a limit kill cannot explain": a
/// zero exit, a timeout, a cancel, or a session that wrote a verdict line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EndKind {
    /// Exited non-zero or with no status, and wrote no verdict: a kill is
    /// possible.
    Died,
    /// Anything else.
    Clean,
}

/// A step of `role` on `item` ended; read its log's tail at `log_path` and
/// update the memo. Returns `true` when the death is a model-limit kill that
/// has a rung to step down to, so the caller records it as the provider's
/// fault (uncharged), `false` otherwise (handled as today).
///
/// Disabled `modelFallback` records nothing and returns `false`. Every other
/// non-kill end clears the item's memo, since the re-dispatch it was made for
/// has run.
pub(crate) fn observe_step_end(
    root: &Path,
    item: u32,
    role: &str,
    log_path: &Path,
    end: EndKind,
) -> bool {
    let config = read_model_fallback_config(root);
    if !config.enabled {
        return false;
    }
    let killed = (end == EndKind::Died)
        .then(|| {
            super::crash_signals::dispatch_scoped_tail(log_path, BANNER_TAIL_LINES)
                .ok()
                .and_then(|tail| killed_model_class(&tail))
        })
        .flatten();
    let rung = killed
        .as_deref()
        .and_then(|class| next_rung(&config.chain, class).map(|r| (class, r)));
    let Some((class, rung)) = rung else {
        // Not a kill, or the last rung (or off-chain): handled as today, and
        // whatever step-down was pending has been spent.
        lock().remove(&key(root, item, role));
        return false;
    };
    log::warn!(
        "sweep_registry: {role} step on #{item} was killed by its {class} limit — its next \
         launch steps down to {rung} (#13905)"
    );
    lock().insert(key(root, item, role), (class.to_string(), Instant::now()));
    true
}

/// Whether a step-down is pending (and unexpired) for `(item, role)`. Read-only
/// and ignores the config, so it only answers "was this pair killed lately";
/// [`step_down`] decides what the launch becomes. The issue-sweep resume
/// re-dispatch names no model (#14443), so it asks this before resolving one.
pub(crate) fn step_down_pending(root: &Path, item: u32, role: &str) -> bool {
    lock()
        .get(&key(root, item, role))
        .is_some_and(|(_, at)| at.elapsed() < MEMO_TTL)
}

/// The launch the memo asks for: given the model a launch would run
/// (`model`, after selection-time fallback) and its `configured` origin, the
/// replacement `(model, decision)` when a kill is pending for `(item, role)`,
/// else `None` (launch as resolved).
///
/// Never climbs and never relaunches the killed class: when `model` already
/// sits strictly below the killed class in the chain, or is not on the chain
/// at all, it stands. Pure apart from reading the memo and the config.
pub(crate) fn step_down(
    root: &Path,
    item: u32,
    role: &str,
    model: &str,
    configured: &str,
) -> Option<(String, ModelFallbackDecision)> {
    let killed = {
        let mut memos = lock();
        let k = key(root, item, role);
        match memos.get(&k) {
            Some((_, at)) if at.elapsed() >= MEMO_TTL => {
                memos.remove(&k);
                return None;
            }
            Some((class, _)) => class.clone(),
            None => return None,
        }
    };
    let config = read_model_fallback_config(root);
    if !config.enabled {
        return None;
    }
    let classes: Vec<Option<String>> = config.chain.iter().map(|e| model_class_of(e)).collect();
    let killed_at = position_of(&classes, &killed)?;
    let launch_at = position_of(&classes, &model_class_of(model)?)?;
    if launch_at > killed_at {
        return None;
    }
    let rung = next_rung(&config.chain, &killed)?;
    Some((
        rung.clone(),
        ModelFallbackDecision {
            model: rung,
            from: configured.to_string(),
            reason: REASON_MODEL_LIMIT_KILL,
        },
    ))
}

#[cfg(test)]
#[path = "model_limit_kill_tests.rs"]
mod tests;
