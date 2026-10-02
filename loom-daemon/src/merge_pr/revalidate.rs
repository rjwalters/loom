//! `--auto`'s post-wait re-validation read (#8410, an #8191 slice): what the
//! uncached re-read of the PR, taken after `_wait_for_checks_then_sync_merge`
//! settled, says about the merge that is about to fire.
//!
//! # What it replaces
//!
//! The body of `merge-pr.sh`'s `_revalidate_merge_guards`, minus the forge
//! read and the two label guards it re-runs:
//!
//! ```text
//! [[ "$(echo "$fresh" | jq -r '.merged // false')" == "true" ]] && return 0
//! fresh_sha="$(echo "$fresh" | jq -r '.head.sha // empty')"; [[ -n "$fresh_sha" ]] || error "Merge blocked: could not re-read …"
//! if [[ -n "$fresh_sha" && -n "$MERGE_PRECONDITION_SHA" && "$fresh_sha" != "$MERGE_PRECONDITION_SHA" ]]; then error_head_moved …; fi
//! PR_LABELS="$(echo "$fresh" | jq -r '.labels[]?.name // empty' 2>/dev/null || true)"
//! ```
//!
//! Four [`Verdict`]s, decided in exactly that order, because the order is the
//! contract: a PR that merged underneath the wait has nothing left to guard; a
//! re-read with no head is a forge failure and must say so (#8896) rather than
//! fall through to the `loom:pr` guard's genuine-absence wording; a head that
//! moved is the #5579 re-queue (exit 3) even when its labels ALSO went stale,
//! which is what a rebase does; and only then is the label set handed back
//! for `_check_loom_pr_label` / `_check_verdict_label_contradiction`.
//!
//! # The deliberate change: an unreadable re-read is never a verdict
//!
//! The retired reads failed in two directions, neither of them the #8896
//! refusal its own comment promised:
//!
//! - **A payload `jq` could not parse** (an HTML error page from a proxy, a
//!   truncated body, a non-object `head`) failed the `fresh_sha=$(…)`
//!   assignment under `merge-pr.sh`'s `set -euo pipefail`, which killed the
//!   script with `jq`'s own exit code — 5, the "not merged, not a failure,
//!   re-queue" code (#8896) — and no message at all beyond `jq`'s parse error.
//! - **A label set `jq` could only PARTLY walk** (a non-object element after a
//!   real one) printed the names before the error and `|| true` kept them, so
//!   the guards re-ran against a label set nobody had actually read — a
//!   `loom:changes-requested` after the bad element was simply not seen
//!   (#8112). A missing `labels` key read as "no labels", which under
//!   `--allow-unapproved` is a recorded override rather than a refusal.
//!
//! [`decide`] instead answers only for a payload inside the contract the forge
//! actually produces — one JSON object; `merged` absent, `null` or boolean;
//! `head.sha` a hex string; `labels` an array of objects whose `name` is a
//! string or absent (or `null`, which is how a Go server renders an empty
//! slice) — and returns [`Verdict::Unreadable`] for the rest. That arm is the
//! same `Merge blocked: could not re-read …` refusal (exit 1) the retired code
//! printed for a payload with no head SHA, now reached by every unreadable
//! shape rather than only that one. It can refuse a merge the retired code
//! would have let through on a half-read label set; it can never permit one
//! the retired code refused.
//!
//! # Fidelity inside the contract
//!
//! Each field is validated lazily, in the order the retired code looked at
//! it, so a payload is held to exactly the fields the retired code would
//! have read before returning: a `merged: true` re-read is [`Verdict::Merged`]
//! whatever its head and labels say, and a moved head is
//! [`Verdict::HeadMoved`] whatever its labels say. Inside the contract the
//! verdict — including the label text, which is the names joined by `\n` as
//! `jq -r` printed them and with trailing newlines stripped as `$(…)` stripped
//! them — is the retired code's, held by
//! `tests/merge_pr_revalidate_differential.rs` against the frozen function in
//! `tests/fixtures/merge-pr-revalidate-retired.sh`.

use serde_json::Value;

/// What the post-wait re-read says about the merge that is about to fire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The PR merged while the wait ran: nothing left to guard.
    Merged,
    /// The head moved past the SHA the merge is gated on (#5579 re-queue).
    /// Carries the fresh head SHA.
    HeadMoved(String),
    /// Head unchanged (or no precondition to compare against): re-run the
    /// label guards against this label set — names, one per line.
    Labels(String),
    /// The re-read is not a usable PR payload (#8896). Carries a short
    /// description of what was wrong, for stderr only; the operator-facing
    /// refusal is [`unreadable_message`].
    Unreadable(String),
}

/// Decide what the uncached post-wait re-read `payload` says, given the head
/// SHA the merge is gated on (`precondition_sha`, possibly empty when the
/// script never learned one — then the head comparison is skipped, exactly as
/// the retired `-n "$MERGE_PRECONDITION_SHA"` test skipped it).
pub fn decide(payload: &str, precondition_sha: &str) -> Verdict {
    let mut docs = serde_json::Deserializer::from_str(payload).into_iter::<Value>();
    let pr = match (docs.next(), docs.next()) {
        (Some(Ok(Value::Object(m))), None) => m,
        (Some(Ok(_)), None) => return unreadable("the re-read is not a JSON object"),
        (Some(Ok(_)), Some(_)) => {
            return unreadable("the re-read holds more than one JSON document")
        }
        (Some(Err(e)), _) => return unreadable(&format!("the re-read is not JSON: {e}")),
        (None, _) => return unreadable("the re-read is empty"),
    };

    match pr.get("merged") {
        None | Some(Value::Null) | Some(Value::Bool(false)) => {}
        Some(Value::Bool(true)) => return Verdict::Merged,
        Some(_) => return unreadable("`merged` is neither a boolean nor null"),
    }

    let fresh_sha = match pr.get("head") {
        None | Some(Value::Null) => return unreadable("the re-read carries no `head`"),
        Some(Value::Object(head)) => match head.get("sha") {
            Some(Value::String(s)) if is_hex(s) => s.clone(),
            None | Some(Value::Null) => return unreadable("the re-read carries no `head.sha`"),
            Some(Value::String(s)) if s.is_empty() => {
                return unreadable("`head.sha` is the empty string")
            }
            Some(_) => return unreadable("`head.sha` is not a hexadecimal SHA string"),
        },
        Some(_) => return unreadable("`head` is not an object"),
    };

    if !precondition_sha.is_empty() && fresh_sha != precondition_sha {
        return Verdict::HeadMoved(fresh_sha);
    }

    let labels = match pr.get("labels") {
        Some(Value::Array(items)) => items,
        Some(Value::Null) => return Verdict::Labels(String::new()),
        None => return unreadable("the re-read carries no `labels`, so the label set is unknown"),
        Some(_) => return unreadable("`labels` is not an array"),
    };
    let mut names: Vec<&str> = Vec::with_capacity(labels.len());
    for item in labels {
        let Value::Object(label) = item else {
            return unreadable("a `labels` element is not an object");
        };
        match label.get("name") {
            None | Some(Value::Null) => {}
            Some(Value::String(s)) if !s.contains('\0') => names.push(s),
            Some(_) => return unreadable("a label `name` is not a string"),
        }
    }
    // `jq -r` printed one name per line; `$(…)` stripped every trailing
    // newline, which also drops an empty-string name when it came last.
    Verdict::Labels(names.join("\n").trim_end_matches('\n').to_string())
}

/// The operator-facing refusal for [`Verdict::Unreadable`] — byte-for-byte
/// the message the retired shell passed to `error` (#8896), so the log line an
/// operator searches for is unchanged.
pub fn unreadable_message(pr: &str) -> String {
    format!(
        "Merge blocked: could not re-read PR #{pr} after --auto's settle-wait — the uncached \
         re-read returned no usable payload (no head SHA), so neither the head nor the label \
         set could be re-validated against current state. This is a forge read failure, NOT a \
         missing `loom:pr` label: refusing to merge rather than treating an unreadable response \
         as a verdict. Re-run once the forge API is healthy."
    )
}

fn unreadable(why: &str) -> Verdict {
    Verdict::Unreadable(why.to_string())
}

fn is_hex(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests;
