//! The per-poll READ of a check-runs rollup inside `merge-pr.sh`'s
//! `_wait_for_checks_then_sync_merge` (#8191 slice): which checks are
//! FAILING, which are still PENDING, and the rollup's `total_count`.
//!
//! # What it replaces
//!
//! Three `jq` filters over the payload `forge_get_check_runs` returned:
//!
//! ```text
//! failing:     [.check_runs[] | select(.conclusion == "failure" or .conclusion == "timed_out"
//!                or .conclusion == "cancelled" or .conclusion == "action_required") | .name] | unique | .[]
//! pending:     [.check_runs[] | select(.status != "completed") | .name] | unique | .[]
//! total_count: .total_count // 0      (then `[[ =~ ^[0-9]+$ ]] || total_count=0`)
//! ```
//!
//! each run with `jq -r … 2>/dev/null || true` inside `$(...)`. Every verdict
//! the wait loop reaches afterwards — refuse on a failing REQUIRED check
//! (`checks_failure`), keep waiting on a pending one, settle on a non-empty
//! rollup (`observed_checks`), or take #6169/#9091's zero-row branch
//! (`zero_checks`) — is decided from these three values, so every one of them
//! is a merge-gating input.
//!
//! # The one deliberate change: an unreadable payload never looks settled
//!
//! The retired `|| true` collapsed a `jq` ERROR into the same empty string as
//! "no check matched". A payload `jq` could not walk — no `check_runs` array,
//! a non-object element, a second JSON document, trailing garbage — therefore
//! read as *nothing failing, nothing pending*, which is the exact shape the
//! loop settles on once any earlier poll saw a non-zero `total_count`. That is
//! the #3678/#6169 false-settle class again, one layer down (#3678 fixed it for
//! a failed FETCH; this was a successful fetch whose content was unreadable).
//!
//! [`classify`] instead answers ONLY for a payload inside the contract
//! `forge_get_check_runs` actually produces (see [`Refusal`]) and refuses
//! everything else. The shell reads a refusal as "still pending", so the
//! loop re-polls and — if the payload never becomes readable — ends at the
//! `LOOM_AUTO_MERGE_TIMEOUT` deadline with exit 5 (not merged, re-queue).
//! A refusal can therefore cost time, never merge an unsettled head.
//!
//! # Fidelity inside the contract
//!
//! For every payload inside the contract the three strings are the retired
//! filters' output byte for byte, AFTER `$(...)` — held by
//! `tests/merge_pr_check_runs_rollup_differential.rs` against the frozen
//! filters in `tests/fixtures/merge-pr-check-runs-rollup-retired.sh`:
//!
//! - `unique` sorts and de-duplicates the name VALUES: `null` (a check-run
//!   with no `name`) sorts before every string, strings sort by UTF-8 bytes
//!   (jq's `jvp_string_cmp` and Rust's `str` ordering agree), and `null` stays
//!   distinct from the string `"null"` even though `-r` renders both as
//!   `null` — so both lines appear.
//! - `-r` prints a string raw, embedded newlines included, one value per
//!   line; `$(...)` then strips every TRAILING newline. A name containing a
//!   newline therefore becomes two lines, exactly as it did downstream of the
//!   retired filter, and an empty-string name contributes an empty line that
//!   `$(...)` removes when it is last.
//! - `select(.conclusion == "failure")` is JSON equality, so a non-string
//!   `conclusion` never matches; `select(.status != "completed")` is JSON
//!   inequality, so a MISSING or non-string `status` counts as pending.
//!
//! One retired quirk is reproduced, not fixed, because this slice ports
//! rather than redesigns: a pending check-run whose name is the empty string
//! renders as an empty line, and if it is the ONLY pending line, `$(...)`
//! leaves `$pending` empty and the loop reads "nothing pending". Neither
//! GitHub nor Gitea produces an empty check name in practice; the quirk is
//! pinned by `empty_pending_name_is_reproduced_not_fixed` so a future fix is a
//! deliberate, visible change.

use serde_json::{Map, Value};

/// The three values the wait loop reads, exactly as the retired `$(...)`
/// captures held them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rollup {
    /// Failing check names, one per line, sorted and de-duplicated.
    pub failing: String,
    /// Still-running (not `completed`) check names, one per line.
    pub pending: String,
    /// `total_count`, as the decimal digits the shell compares with `-gt`.
    pub total_count: String,
}

/// Why a payload is outside the `forge_get_check_runs` contract. Every
/// variant is answered by the shell as "still pending" (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// Not exactly one well-formed JSON document (empty input, a parse error,
    /// trailing garbage, or a second document).
    NotOneDocument(String),
    /// The document is not a JSON object.
    NotAnObject,
    /// `total_count` is missing, or is not a JSON number written as an
    /// unsigned integer that fits in `u64`. Both forge paths build it from an
    /// integer (`.total_count // 0` / `length`), so any other shape is a
    /// broken payload — and an integral non-integer literal (`5.0`, `1e2`)
    /// is rendered differently by jq 1.6 and jq 1.7, so no answer for it
    /// could be faithful to "the retired filter" on every host.
    BadTotalCount,
    /// `check_runs` is missing or is not an array.
    CheckRunsNotAnArray,
    /// A `check_runs` element is not an object.
    ElementNotAnObject(usize),
    /// A check-run's `name` is neither a string nor absent/`null`, or is a
    /// string containing NUL (which `$(...)` would silently drop).
    BadName(usize),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::NotOneDocument(why) => {
                write!(f, "stdin is not exactly one JSON document ({why})")
            }
            Refusal::NotAnObject => write!(f, "the rollup is not a JSON object"),
            Refusal::BadTotalCount => {
                write!(f, "`total_count` is missing or is not an unsigned integer literal")
            }
            Refusal::CheckRunsNotAnArray => write!(f, "`check_runs` is missing or not an array"),
            Refusal::ElementNotAnObject(i) => write!(f, "`check_runs[{i}]` is not an object"),
            Refusal::BadName(i) => {
                write!(f, "`check_runs[{i}].name` is neither a string nor null, or contains NUL")
            }
        }
    }
}

/// The retired failing filter's terminal non-success conclusions.
const FAILING_CONCLUSIONS: [&str; 4] = ["failure", "timed_out", "cancelled", "action_required"];

/// Classify one `forge_get_check_runs` payload. See the module docs.
///
/// # Errors
///
/// A [`Refusal`] for any payload outside the contract.
pub fn classify(raw: &str) -> Result<Rollup, Refusal> {
    let doc = one_document(raw)?;
    let Value::Object(obj) = doc else {
        return Err(Refusal::NotAnObject);
    };
    let total_count = match obj.get("total_count") {
        Some(Value::Number(n)) if n.is_u64() => n.to_string(),
        _ => return Err(Refusal::BadTotalCount),
    };
    let runs = check_runs(&obj)?;

    // `None` is a check-run with no (or a null) name: jq's `null`, which sorts
    // before every string and renders as the text `null`.
    let mut failing: Vec<Option<&str>> = Vec::new();
    let mut pending: Vec<Option<&str>> = Vec::new();
    for (i, run) in runs.iter().enumerate() {
        let name = match run.get("name") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if !s.contains('\0') => Some(s.as_str()),
            Some(_) => return Err(Refusal::BadName(i)),
        };
        let conclusion = run.get("conclusion");
        if FAILING_CONCLUSIONS
            .iter()
            .any(|c| conclusion.and_then(Value::as_str) == Some(*c))
        {
            failing.push(name);
        }
        if run.get("status").and_then(Value::as_str) != Some("completed") {
            pending.push(name);
        }
    }
    Ok(Rollup {
        failing: render(failing),
        pending: render(pending),
        total_count,
    })
}

/// Exactly one JSON document, surrounded by nothing but JSON whitespace.
fn one_document(raw: &str) -> Result<Value, Refusal> {
    let mut stream = serde_json::Deserializer::from_str(raw).into_iter::<Value>();
    let doc = match stream.next() {
        Some(Ok(v)) => v,
        Some(Err(e)) => return Err(Refusal::NotOneDocument(e.to_string())),
        None => return Err(Refusal::NotOneDocument("empty input".to_string())),
    };
    match stream.next() {
        None => Ok(doc),
        Some(Ok(_)) => Err(Refusal::NotOneDocument("more than one document".to_string())),
        Some(Err(e)) => Err(Refusal::NotOneDocument(e.to_string())),
    }
}

/// `check_runs` as a slice of objects.
fn check_runs(obj: &Map<String, Value>) -> Result<Vec<&Map<String, Value>>, Refusal> {
    let Some(Value::Array(items)) = obj.get("check_runs") else {
        return Err(Refusal::CheckRunsNotAnArray);
    };
    items
        .iter()
        .enumerate()
        .map(|(i, item)| item.as_object().ok_or(Refusal::ElementNotAnObject(i)))
        .collect()
}

/// `unique | .[]` under `jq -r`, captured by `$(...)`: sort (null first,
/// then strings by bytes), de-duplicate, print each value plus a newline,
/// then strip every trailing newline.
fn render(mut names: Vec<Option<&str>>) -> String {
    // `Option`'s derived order puts `None` before every `Some`, and `&str`
    // orders by bytes — jq's null-before-string, then `jvp_string_cmp`.
    names.sort_unstable();
    names.dedup();
    let mut out = String::new();
    for name in names {
        out.push_str(name.unwrap_or("null"));
        out.push('\n');
    }
    out.truncate(out.trim_end_matches('\n').len());
    out
}

#[cfg(test)]
mod tests;
