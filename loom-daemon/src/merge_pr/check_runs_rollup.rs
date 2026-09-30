//! The check-runs rollup PARSE that feeds every branch of `--auto`'s settle
//! wait (#8191 slice of `_wait_for_checks_then_sync_merge`, `merge-pr.sh`
//! lines ~2005-2013 before this port).
//!
//! # What it decides
//!
//! Every poll of the wait loop fetches the head SHA's check-runs rollup and
//! derives exactly three things from it, which then route the whole
//! iteration:
//!
//! - **failing** — the names of checks whose conclusion is TERMINAL and
//!   non-success. That set is exactly
//!   `failure` / `timed_out` / `cancelled` / `action_required` and nothing
//!   else: `skipped`, `neutral` and `stale` are completed-but-not-failing and
//!   must NOT refuse a merge, and `success` obviously must not. Non-empty
//!   hands the iteration to [`super::checks_failure`], whose required-context
//!   overlap test can refuse the merge outright.
//! - **pending** — the names of checks that are not `completed`. Anything
//!   other than the literal string `completed` counts, including a status the
//!   forge has not invented yet and a check-run carrying no `status` at all:
//!   the retired filter was `select(.status != "completed")`, which is
//!   deliberately a denylist of one value rather than an allowlist of the
//!   statuses known in 2026. Non-empty keeps the loop waiting.
//! - **total_count** — the rollup's own row count, which is what #6169's
//!   zero-row ambiguity guard ([`super::zero_checks`]) keys on. A zero here
//!   is never trusted on one read.
//!
//! Getting any of the three wrong is a wrong merge or a wrong refusal on
//! unattended Champion runs, and all three came from `jq` filters whose
//! failure mode is SILENT: each was written `2>/dev/null || true`, so a
//! rollup shape `jq` could not walk produced an empty answer that reads
//! exactly like "nothing is failing and nothing is running".
//!
//! # Fidelity to the retired `jq`, deliberately preserved
//!
//! [`parse`] reproduces the three filters' observable behaviour rather than
//! tidying it, because the wait loop's downstream branches were tuned against
//! it:
//!
//! - **A rollup `jq` cannot walk yields EMPTY lists and `total_count` 0**,
//!   not an error. `{"check_runs": null}`, a missing `check_runs`, a
//!   `check_runs` holding a non-object element (`.name` on a number is a
//!   `jq` type error that aborts the whole filter, taking the names it had
//!   already collected with it), a top-level array, malformed bytes, empty
//!   input — all of them came back empty from `|| true`. That routes the poll
//!   into the zero-row guard, which is the fail-CLOSED direction: it keeps
//!   waiting rather than declaring settlement. Reproducing it means a
//!   degraded forge response cannot become "settled" here any more than it
//!   could before.
//! - **`.check_runs` as an OBJECT iterates its VALUES.** `jq`'s `.[]` does,
//!   so `{"check_runs":{"a":{...}}}` was never the empty answer a naive
//!   "arrays only" port would produce.
//! - **`unique` SORTS and de-duplicates**, so both lists arrive in `jq`'s
//!   total order (null < false < true < numbers < strings < arrays <
//!   objects; strings by codepoint), not in rollup order. The refusal
//!   message [`super::checks_failure`] builds is operator-visible text, so
//!   that order is observable.
//! - **A check-run with no `name` contributes the literal text `null`.**
//!   `jq -r` only unquotes STRINGS; `null`, numbers and booleans print as
//!   their JSON form. A name that is not a string is therefore a name-shaped
//!   line, not a dropped row — and dropping it would silently shrink the
//!   pending set, which is the one direction that can end a wait early.
//! - **`total_count` is validated as ASCII digits only.** The retired shell
//!   read `.total_count // 0` — `//` fires on `null` AND on `false`, so
//!   `false` became `0` while a literal `0` stayed `0` — then gated the
//!   result on `[[ =~ ^[0-9]+$ ]]`, which rejects `7.0`, `-1`, `1e3` and
//!   `"abc"` alike and substituted `0`. A string `"7"` passed, because
//!   `jq -r` had already unquoted it. That gate is here, not in the shell,
//!   so the caller can no longer be handed a value its `-gt` arithmetic
//!   would choke on.
//!
//! One bound is deliberate rather than reproduced: an INTEGER `total_count`
//! larger than `u64::MAX` renders here in exponent form and is therefore
//! gated to `0`, which is what jq ≤ 1.6 did (it round-tripped every number
//! through a double) but not what jq ≥ 1.7 does (it preserves the literal
//! digits, which the `^[0-9]+$` gate then accepts). Preserving the literal
//! would mean carrying the payload's raw bytes alongside the parsed value
//! for a field that counts CHECK RUNS ON ONE COMMIT — a few hundred at the
//! very most — so the bound is taken instead, and it fails toward `0`, the
//! direction that keeps #6169's zero-row guard engaged rather than skipping
//! it.
//!
//! # What stays in the shell
//!
//! The forge read itself (`forge_get_check_runs`, its retry-once, and the
//! `FORGE_CHECK_RUNS_RC_*` fetch-failure codes classified by
//! [`super::check_runs_streak`]) and every consequence of the parse: the
//! `observed_checks` latch, the deadline/sleep loop, and the three routing
//! branches. This module is handed bytes and answers what is in them —
//! naming which checks are failing is not deciding what to do about it.

use serde_json::Value;
use std::cmp::Ordering;

/// `jq` yields `null` for an absent key rather than erroring, so the absent
/// case needs a borrowable `null` of its own.
const NULL: Value = Value::Null;

/// One poll's parsed rollup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rollup {
    /// `.total_count`, after the retired `// 0` alternative and the
    /// `^[0-9]+$` gate — kept as the digit STRING the shell's arithmetic
    /// consumed, so a count wider than any integer type this port picked
    /// cannot be silently re-rendered.
    pub total_count: String,
    /// Terminal-failing check names, in `unique` order.
    pub failing: Vec<String>,
    /// Not-`completed` check names, in `unique` order.
    pub pending: Vec<String>,
}

impl Rollup {
    /// The newline-joined string the retired `$(jq …)` produced for a name
    /// list — which is what the shell's `[[ -n … ]]` tests measured, and is
    /// NOT the same question as "is the list empty": a single check-run named
    /// `""` yielded one empty line, which command substitution stripped to
    /// the empty string, so the shell read it as "nothing failing/pending".
    #[must_use]
    pub fn joined(names: &[String]) -> String {
        names.join("\n")
    }

    /// `printf '%s\n' "$pending" | wc -l` over the joined string — the count
    /// the shell reported as "N check(s) still running". One for an empty
    /// list, because `printf '%s\n' ""` still emits a line.
    #[must_use]
    pub fn line_count(names: &[String]) -> usize {
        Self::joined(names).split('\n').count()
    }
}

/// The three retired `jq` filters over one rollup payload.
///
/// Never fails: any input `jq` would have errored on returns the all-empty
/// [`Rollup`] (`total_count` `"0"`), exactly as `2>/dev/null || true` did.
#[must_use]
pub fn parse(raw: &str) -> Rollup {
    let Ok(root) = serde_json::from_str::<Value>(raw.trim()) else {
        // Malformed, or empty input: `jq` printed nothing and the `|| true` /
        // `|| echo 0` fallbacks supplied the empty answer.
        return Rollup {
            total_count: "0".to_string(),
            ..Rollup::default()
        };
    };

    Rollup {
        total_count: total_count(&root),
        failing: failing(&root),
        pending: pending(&root),
    }
}

/// `jq -r '.total_count // 0'`, then bash's `[[ =~ ^[0-9]+$ ]] || 0`.
fn total_count(root: &Value) -> String {
    // `.total_count` on anything but an object or null is a `jq` type error
    // ("Cannot index array with \"total_count\""), which the retired
    // `|| echo 0` answered with 0.
    let field = match root {
        Value::Object(map) => map.get("total_count").unwrap_or(&NULL),
        Value::Null => &NULL,
        _ => return "0".to_string(),
    };
    // `//` is jq's alternative operator: it fires on `null` and on `false`,
    // and on nothing else — a literal `0` is truthy to `//` and stays `0`.
    let rendered = match field {
        Value::Null | Value::Bool(false) => "0".to_string(),
        other => render_raw(other),
    };
    // Bash's own gate, with ASCII digits only. `^[0-9]+$` is anchored at both
    // ends and `$` is end-of-STRING in `[[ =~ ]]` (there is no multiline
    // mode), so a value with an embedded newline never passed either.
    if !rendered.is_empty() && rendered.bytes().all(|b| b.is_ascii_digit()) {
        rendered
    } else {
        "0".to_string()
    }
}

/// `jq -r '[.check_runs[] | select(.conclusion == "failure" or … ) | .name] | unique | .[]'`.
fn failing(root: &Value) -> Vec<String> {
    const TERMINAL: [&str; 4] = ["failure", "timed_out", "cancelled", "action_required"];
    collect(root, |run| {
        // `.conclusion` on a non-object aborts the filter; `field` returns
        // `None` for that, which `collect` turns into the empty answer.
        let c = field(run, "conclusion")?;
        Some(c.as_str().is_some_and(|s| TERMINAL.contains(&s)))
    })
}

/// `jq -r '[.check_runs[] | select(.status != "completed") | .name] | unique | .[]'`.
fn pending(root: &Value) -> Vec<String> {
    collect(root, |run| {
        let s = field(run, "status")?;
        // `!=` against a string: a missing/null/non-string status is NOT
        // equal to "completed", so it counts as pending. The retired filter
        // was a denylist of exactly one value.
        Some(!matches!(s, Value::String(text) if text == "completed"))
    })
}

/// Iterate `.check_runs[]`, keep the runs `select` admits, project `.name`,
/// then `unique`. `None` from `select` is a `jq` type error, which aborted
/// the whole filter and produced NO names at all — not a skipped row.
fn collect(root: &Value, select: impl Fn(&Value) -> Option<bool>) -> Vec<String> {
    let Some(runs) = members(root) else {
        return Vec::new();
    };
    let mut names: Vec<Value> = Vec::new();
    for run in runs {
        match select(run) {
            Some(true) => match field(run, "name") {
                // `.name` on a non-object is the same aborting type error.
                None => return Vec::new(),
                Some(name) => names.push(name.clone()),
            },
            Some(false) => {}
            None => return Vec::new(),
        }
    }
    unique(names)
}

/// `.check_runs[]`'s iterable, or `None` when `jq` would have errored
/// ("Cannot iterate over null" / "over number" …). An OBJECT iterates its
/// VALUES, which is `jq`'s `.[]`, not an error.
fn members(root: &Value) -> Option<Vec<&Value>> {
    let Value::Object(map) = root else {
        return None;
    };
    match map.get("check_runs") {
        Some(Value::Array(items)) => Some(items.iter().collect()),
        Some(Value::Object(inner)) => Some(inner.values().collect()),
        _ => None,
    }
}

/// `.<key>` on one check-run. `None` when the run is not indexable by a
/// string key (`jq` errors, aborting the filter); `Value::Null` when the key
/// is simply absent, which `jq` yields rather than erroring.
fn field<'a>(run: &'a Value, key: &str) -> Option<&'a Value> {
    match run {
        Value::Object(map) => Some(map.get(key).unwrap_or(&NULL)),
        // `jq` indexes `null` with any key and gets `null`.
        Value::Null => Some(&NULL),
        _ => None,
    }
}

/// `unique`: sort in jq's total order, de-duplicate, then render each value
/// as `jq -r` would have printed it.
fn unique(mut names: Vec<Value>) -> Vec<String> {
    names.sort_by(jq_cmp);
    names.dedup_by(|a, b| jq_cmp(a, b) == Ordering::Equal);
    names.iter().map(render_raw).collect()
}

/// `jq`'s total order across types: null < false < true < numbers < strings <
/// arrays < objects.
fn jq_cmp(a: &Value, b: &Value) -> Ordering {
    fn rank(v: &Value) -> u8 {
        match v {
            Value::Null => 0,
            Value::Bool(false) => 1,
            Value::Bool(true) => 2,
            Value::Number(_) => 3,
            Value::String(_) => 4,
            Value::Array(_) => 5,
            Value::Object(_) => 6,
        }
    }
    let by_rank = rank(a).cmp(&rank(b));
    if by_rank != Ordering::Equal {
        return by_rank;
    }
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x
            .as_f64()
            .unwrap_or(f64::NAN)
            .partial_cmp(&y.as_f64().unwrap_or(f64::NAN))
            .unwrap_or(Ordering::Equal),
        // Rust's `str` ordering is UTF-8 byte order, which for well-formed
        // UTF-8 is codepoint order — what `jq` compares strings by.
        (Value::String(x), Value::String(y)) => x.cmp(y),
        (Value::Array(x), Value::Array(y)) => {
            for (i, j) in x.iter().zip(y.iter()) {
                let c = jq_cmp(i, j);
                if c != Ordering::Equal {
                    return c;
                }
            }
            x.len().cmp(&y.len())
        }
        // `jq` compares objects by their sorted key sets first, then by the
        // values at those keys. Vanishingly rare for a check-run name, but
        // ordering it arbitrarily would make `unique` unstable.
        (Value::Object(x), Value::Object(y)) => {
            // Sorted, because `preserve_order` keeps this map in INSERTION
            // order while jq compares objects by their sorted key sets.
            let mut kx: Vec<&String> = x.keys().collect();
            let mut ky: Vec<&String> = y.keys().collect();
            kx.sort_unstable();
            ky.sort_unstable();
            let by_keys = kx.cmp(&ky);
            if by_keys != Ordering::Equal {
                return by_keys;
            }
            for k in kx {
                let c = jq_cmp(&x[k], &y[k]);
                if c != Ordering::Equal {
                    return c;
                }
            }
            Ordering::Equal
        }
        _ => Ordering::Equal,
    }
}

/// `jq -r`: strings lose their quotes, everything else prints as compact
/// JSON — so an absent `.name` is the four characters `null`, not a dropped
/// row.
fn render_raw(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests;
