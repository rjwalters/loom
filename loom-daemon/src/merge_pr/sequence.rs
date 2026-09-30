//! The durable "approved, but not yet" merge-sequencing gate (#9378).
//!
//! # The gap this fills
//!
//! Two PRs needed to land in a specific order; both agents agreed; a third
//! merged the follower first because it held a verdict and nothing in the repo
//! said otherwise. There was no label meaning *approved, and must not merge
//! yet*. `loom:operator` means "a human must rule" — an escalation, false for
//! a pure ordering constraint — and `loom:blocked` is an issue-status label
//! that `merge-pr.sh` never reads as a merge gate. This module supplies the
//! missing primitive as two halves:
//!
//! 1. **The gate is a label.** `loom:sequenced` joins
//!    [`crate::merge_pr::labels::BLOCKING`], so a PR carrying it beside
//!    `loom:pr` is refused by the existing verdict-contradiction guard on
//!    every merge path, with the same no-override treatment. The merge path
//!    reads ONLY the label — it never parses markers, never fetches
//!    predecessor state, and grows no shell. A sequencing hold that cleared
//!    itself inside the merge script would need live forge reads at merge
//!    time; instead the hold is *durable* precisely because it moves by a
//!    separate, explicit evaluation step.
//! 2. **The condition is a trusted marker.** The applier (the #9686
//!    sequencing pass, or an agent encoding a human-agreed order) writes:
//!
//!    ```text
//!    <!-- loom:sequence after=111 pred_head=<40-hex> follower_head=<40-hex> plan=<id> -->
//!    ```
//!
//!    on the follower PR. `after` names the predecessor (same repo; cross-repo
//!    ordering is refused by construction — there is nowhere to record one).
//!    `pred_head` pins the predecessor tree the order was computed against.
//!    `follower_head` pins the held PR's own head, so a stale marker is
//!    detectable instead of silently binding a tree it never judged. `plan=`
//!    groups a multi-PR ordering so repeated passes are idempotent, and is
//!    the vocabulary #9687/#9688 reuse for combined-PR reservations — pin it
//!    here, do not fork it there.
//!
//! [`evaluate`] is the half that moves the label: given the parsed marker and
//! the live predecessor state, it returns the release decision. The CLI verb
//! (`merge-pr sequence-eval`) fetches the inputs and prints one sentinel;
//! callers release on a positive signal only, the same asymmetry as every
//! other guard here.
//!
//! # Parsing rules (each one is a reproduced incident shape)
//!
//! - **Single-line HTML comments only.** The input is many comment bodies;
//!   the `hold_state` lesson applies unchanged — a `<!--` with no `-->` on
//!   the same line yields nothing, so one malformed comment cannot change how
//!   a later one is read.
//! - **A malformed span is not a marker.** Documentation lines quoting the
//!   format, prose field lists, and truncated writes all fail validation and
//!   are skipped — the same disposition `hold_state` reached for its quoted
//!   example (`<sha>` fails the hex capture, so the doc line yields no match).
//!   This is safe here because [`evaluate`] never trusts the marker's claims
//!   on their own: both pinned SHAs are re-checked against live forge state,
//!   so the worst case of honoring a stale-but-valid marker is a hold that
//!   stands (or replans), never a wrongful release.
//! - **Newest marker wins.** Bodies arrive oldest-first, as the forge renders
//!   them; a REPLAN rewrites the marker rather than amending it, so the
//!   newest valid marker is by construction the current plan.
//! - **Full 40-hex SHAs required.** The producer copies the SHA from the
//!   forge; a short SHA is an ambiguous pin, and an ambiguous pin defeats the
//!   head-freshness check that makes replanning safe. Fail closed.
//! - **Trust is applied upstream.** By #9548 an outsider's well-formed marker
//!   is prose; the CLI filters through `comment_trust` before this parser
//!   runs, exactly as the verdict-marker scan does.

use std::path::Path;
use std::process::{Command, Stdio};

/// The only stdout a caller may treat as "the recorded predecessor landed —
/// releasing the sequencing hold is authorized".
pub const CLEAR: &str = "LOOM-SEQUENCE-CLEAR";

/// The predecessor closed without merging: the recorded condition can never
/// fire. Distinct from [`CLEAR`] so the caller's policy stays explicit — the
/// #9686 pass releases with a comment, and a human's semantic "after" marker
/// gets the same factual answer without the two release reasons becoming
/// indistinguishable in the transcript.
pub const DISSOLVED: &str = "LOOM-SEQUENCE-DISSOLVED";

/// Still ordered behind a live predecessor at the recorded head. The hold
/// stands; nothing to do.
pub const KEEP: &str = "LOOM-SEQUENCE-KEEP";

/// The plan's pinned state no longer matches reality (follower head moved or
/// predecessor head moved). The marker must be re-derived; honoring it would
/// bind a tree it never judged.
pub const REPLAN: &str = "LOOM-SEQUENCE-REPLAN";

/// No trusted marker on the PR: nothing here owns a hold. A PR may still be
/// unmergeable for a dozen other reasons; this verb is simply not one of them.
pub const NONE: &str = "LOOM-SEQUENCE-NONE";

/// The prefix that identifies a sequencing marker, before field parsing.
pub const MARKER_PREFIX: &str = "loom:sequence";

/// A parsed, validated `<!-- loom:sequence … -->` marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequenceMarker {
    /// The predecessor PR number (`after=`).
    pub after: u32,
    /// The predecessor head the order was computed against (`pred_head=`).
    pub pred_head: String,
    /// The follower head at apply time (`follower_head=`).
    pub follower_head: String,
    /// The plan identity grouping this ordering (`plan=`).
    pub plan: String,
    /// Who authored the hold (`source=`): `Some("pass")` marks a planner-authored,
    /// SOFT ordering preference — the #9686 pass may expiry-release it when the
    /// predecessor stalls (starvation bound). `None` (or any other value) is a
    /// HARD hold: a human's or agent's semantic "after" never expires into merge
    /// permission (#9063's non-negotiable).
    pub source: Option<String>,
}

/// The live predecessor state the evaluation needs, as the forge reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PredecessorState {
    /// PR is open (not closed).
    pub open: bool,
    /// PR has been merged.
    pub merged: bool,
    /// The current head-ref SHA. `None` when the forge withheld it (a deleted
    /// head ref can still report the SHA on the pull object; a failed parse
    /// must not be read as a match).
    pub head_sha: Option<String>,
    /// RFC3339 last-activity timestamp, when the forge supplied one. Not part
    /// of the release decision itself — the #9686 pass reads it for the soft
    /// hold's starvation bound (any predecessor activity keeps a soft order
    /// fresh). `None` never expires anything.
    pub updated_at: Option<String>,
}

/// Why [`Verdict::Keep`] holds the sequencing label in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepReason {
    /// The ordinary waiting state: predecessor open at the recorded head.
    InFlight,
    /// The predecessor's head moved since the plan was recorded. The ORDER
    /// stands; the pinned SHA must be re-derived (replan), because the
    /// recorded tree may never land.
    PredecessorMoved,
    /// The held PR's own head moved. Same consequence, different side: the
    /// marker no longer describes the PR it holds.
    FollowerMoved,
    /// The predecessor merged, but not at the recorded head — either it
    /// landed from a different tree, or something pushed to its head branch
    /// after the merge. The recorded tree's fate is unknown; never release
    /// on an unknown.
    MergedAtUnknownHead,
}

/// The release decision for one sequencing hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The recorded predecessor tree landed; release authorized.
    Clear,
    /// The predecessor closed unmerged; the condition can never fire.
    Dissolved,
    /// The hold stands, for the stated reason.
    Keep(KeepReason),
}

/// The canonical marker line the applier writes.
///
/// Single producer, single format: parse and render live in this module so
/// they cannot drift — the pass (#9686) writes what this parser reads.
#[must_use]
pub fn marker_text(marker: &SequenceMarker) -> String {
    let source = marker
        .source
        .as_deref()
        .map(|s| format!(" source={s}"))
        .unwrap_or_default();
    format!(
        "<!-- {} after={} pred_head={} follower_head={} plan={}{} -->",
        MARKER_PREFIX, marker.after, marker.pred_head, marker.follower_head, marker.plan, source
    )
}

/// The inner text of every HTML comment that both opens and closes on `line`.
///
/// Line-local by construction — the same rule as `hold_state::html_comment_spans`,
/// for the same reason: many bodies are scanned together, and a multi-line
/// scan lets one malformed comment change how a later one is read.
pub(crate) fn html_comment_spans(line: &str) -> Vec<&str> {
    let mut spans = Vec::new();
    let mut rest = line;
    while let Some(open) = rest.find("<!--") {
        let after = &rest[open + 4..];
        let Some(close) = after.find("-->") else {
            break;
        };
        spans.push(&after[..close]);
        rest = &after[close + 3..];
    }
    spans
}

/// `pred_head=`/`follower_head=` value: exactly 40 lowercase hex chars.
///
/// Full SHA or nothing — see the module docs. The 40-char width is the forge's
/// own rendering; anything else is a hand-typed approximation.
fn is_full_sha(s: &str) -> bool {
    s.len() == 40
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `plan=` value: a short opaque token. The tokenizer already excludes
/// whitespace and the `-->` closer; this rejects only the empty string and
/// implausibly long ids.
fn is_plan_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64
}

/// Parse one comment span into a marker, or `None` if the span is not a
/// sequencing marker (including a marker-shaped span with invalid fields —
/// see the module docs for why malformed does not mean "match with prejudice").
fn parse_span(span: &str) -> Option<SequenceMarker> {
    let text = span.trim();
    let rest = text.strip_prefix(MARKER_PREFIX)?;
    if !rest.starts_with(char::is_whitespace) {
        // `loom:sequence-x` is a different marker namespace, not a malformed
        // field list. Only a real `loom:sequence` prefix enters validation.
        return None;
    }
    let mut after = None;
    let mut pred_head = None;
    let mut follower_head = None;
    let mut plan = None;
    let mut source = None;
    for field in rest.split_whitespace() {
        let (key, value) = field.split_once('=')?;
        match key {
            "after" => after = value.parse::<u32>().ok().filter(|n| *n > 0),
            "pred_head" => pred_head = is_full_sha(value).then(|| value.to_string()),
            "follower_head" => follower_head = is_full_sha(value).then(|| value.to_string()),
            "plan" => plan = is_plan_id(value).then(|| value.to_string()),
            "source" => source = is_plan_id(value).then(|| value.to_string()),
            _ => return None,
        }
    }
    Some(SequenceMarker {
        after: after?,
        pred_head: pred_head?,
        follower_head: follower_head?,
        plan: plan?,
        source,
    })
}

/// Scan concatenated comment bodies (oldest-first, trusted only) for the
/// newest sequencing marker.
///
/// A quoted, backticked, or prose-shaped mention does not match: the marker
/// must sit inside a single-line HTML comment AND start with
/// [`MARKER_PREFIX`] at the comment's own beginning (after surrounding
/// whitespace inside the comment).
#[must_use]
pub fn parse(bodies: &[String]) -> Option<SequenceMarker> {
    let mut newest = None;
    for body in bodies {
        for line in body.lines() {
            for span in html_comment_spans(line) {
                if let Some(marker) = parse_span(span) {
                    newest = Some(marker);
                }
            }
        }
    }
    newest
}

/// The release tombstone every hold-releasing writer emits
/// (`<!-- loom:sequence released plan=<plan> -->`): the #9686 pass on
/// clear/dissolve/expiry, `consolidate-abort` (#9688), and
/// `consolidate-reconcile` (#9689). Rendered here so the writers and
/// [`parse_live`] cannot drift.
#[must_use]
pub fn release_marker_text(plan: &str) -> String {
    format!("<!-- {MARKER_PREFIX} released plan={plan} -->")
}

/// A span that ends a hold rather than stating one.
enum Tombstone {
    /// `loom:sequence released plan=<plan>` — the hold for `plan` was released.
    Released(String),
    /// `loom:sequence replanned` — the pass voided the hold (a pin moved).
    Replanned,
}

fn parse_tombstone(span: &str) -> Option<Tombstone> {
    let rest = span.trim().strip_prefix(MARKER_PREFIX)?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let mut fields = rest.split_whitespace();
    match (fields.next()?, fields.next(), fields.next()) {
        ("replanned", None, None) => Some(Tombstone::Replanned),
        ("released", Some(plan), None) => plan
            .strip_prefix("plan=")
            .filter(|p| is_plan_id(p))
            .map(|p| Tombstone::Released(p.to_string())),
        _ => None,
    }
}

/// The hold the marker history says is STILL IN FORCE: the newest valid
/// marker, unless a later tombstone ended it.
///
/// [`parse`] answers "what did the newest marker say" and is what the gate
/// evaluation reads (the label is the gate there, so a stale marker on an
/// unlabeled PR is inert). Callers that decide from marker history alone —
/// "is this PR still reserved?", "is this reservation still ours to
/// release?" — must use this instead, or a released hold reads as live
/// forever (#9745 review: the release comment carries no pins, so [`parse`]
/// skips it and the old reservation keeps winning).
///
/// A `released` tombstone ends the hold only when its `plan=` names the
/// current marker's plan — a late release of an older plan must not void a
/// newer hold. `replanned` carries no plan and voids whatever precedes it.
#[must_use]
pub fn parse_live(bodies: &[String]) -> Option<SequenceMarker> {
    let mut newest: Option<SequenceMarker> = None;
    for body in bodies {
        for line in body.lines() {
            for span in html_comment_spans(line) {
                if let Some(marker) = parse_span(span) {
                    newest = Some(marker);
                    continue;
                }
                match parse_tombstone(span) {
                    Some(Tombstone::Replanned) => newest = None,
                    Some(Tombstone::Released(plan))
                        if newest.as_ref().is_some_and(|m| m.plan == plan) =>
                    {
                        newest = None;
                    }
                    _ => {}
                }
            }
        }
    }
    newest
}

/// Evaluate one sequencing hold against live forge state.
///
/// Order of checks is load-bearing: the follower's own head is checked first
/// (a marker that no longer describes THIS tree must not be honored no matter
/// what the predecessor did), then the predecessor's outcome.
#[must_use]
pub fn evaluate(marker: &SequenceMarker, pred: &PredecessorState, follower_head: &str) -> Verdict {
    if marker.follower_head != follower_head {
        return Verdict::Keep(KeepReason::FollowerMoved);
    }
    if pred.merged {
        return match pred.head_sha.as_deref() {
            Some(head) if head == marker.pred_head => Verdict::Clear,
            _ => Verdict::Keep(KeepReason::MergedAtUnknownHead),
        };
    }
    if !pred.open {
        return Verdict::Dissolved;
    }
    match pred.head_sha.as_deref() {
        Some(head) if head == marker.pred_head => Verdict::Keep(KeepReason::InFlight),
        _ => Verdict::Keep(KeepReason::PredecessorMoved),
    }
}

/// The stdout line for a [`Verdict`], naming the sentinel and the reason so a
/// transcript reader never has to decode a bare token.
#[must_use]
pub fn verdict_line(verdict: Verdict, marker: &SequenceMarker) -> String {
    match verdict {
        Verdict::Clear => {
            format!("{CLEAR} predecessor #{} merged at the recorded head", marker.after)
        }
        Verdict::Dissolved => {
            format!("{DISSOLVED} predecessor #{} closed without merging", marker.after)
        }
        Verdict::Keep(KeepReason::InFlight) => {
            format!("{KEEP} predecessor #{} is open at the recorded head", marker.after)
        }
        Verdict::Keep(KeepReason::PredecessorMoved) => format!(
            "{REPLAN} predecessor #{} head moved since plan {} was recorded",
            marker.after, marker.plan
        ),
        Verdict::Keep(KeepReason::FollowerMoved) => {
            format!("{REPLAN} this PR's head moved since plan {} was recorded", marker.plan)
        }
        Verdict::Keep(KeepReason::MergedAtUnknownHead) => format!(
            "{REPLAN} predecessor #{} merged but not at the recorded head of plan {}",
            marker.after, marker.plan
        ),
    }
}

// --- Forge reads --------------------------------------------------------
//
// Thin `gh api` wrappers with the same injection seam `redate` /
// `stale_checks::fetch` provide: the binary is a parameter, so tests stub it
// with a script path instead of a global env var that would race across
// parallel test threads.

/// The `gh` binary, honoring `LOOM_GH_BIN` — the same seam
/// `redate` and `stale_checks::fetch` provide.
#[must_use]
pub fn gh_bin() -> String {
    std::env::var("LOOM_GH_BIN").unwrap_or_else(|_| "gh".to_string())
}

/// Fetch every TRUSTED comment body on `pr`, oldest first (#9548: an
/// outsider's well-formed marker is prose, dropped before any parse).
/// `--paginate` is REQUIRED: the marker is always among the NEWEST comments,
/// and the default first page is the oldest 30 — the same pitfall #5455
/// documented for the fallback-queue scan. `None` on any failure: a read
/// that did not happen is never an empty comment stream.
pub fn fetch_trusted_bodies(bin: &str, root: &Path, nwo: &str, pr: u32) -> Option<Vec<String>> {
    let mut cmd = Command::new(bin);
    cmd.arg("api")
        .arg(format!("repos/{nwo}/issues/{pr}/comments"))
        .arg("--paginate");
    cmd.current_dir(root);
    // #5401: cross-owner managed repo -> its own owner's installation-token
    // GH_CONFIG_DIR (no-op for single-owner fleets / the root owner).
    crate::credential_preflight::apply_gh_config_for_root(&mut cmd, root);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    crate::comment_trust::TrustPolicy::for_root(root).trusted_bodies(&out.stdout)
}

/// Fetch the predecessor's live pull-request state. `None` on any failure —
/// same rule as [`fetch_trusted_bodies`]: an unknown predecessor state is
/// never an open predecessor.
pub fn fetch_predecessor(
    bin: &str,
    root: &Path,
    nwo: &str,
    after: u32,
) -> Option<PredecessorState> {
    let mut cmd = Command::new(bin);
    cmd.arg("api").arg(format!("repos/{nwo}/pulls/{after}"));
    cmd.current_dir(root);
    crate::credential_preflight::apply_gh_config_for_root(&mut cmd, root);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    predecessor_from_json(&out.stdout)
}

/// Parse a pulls-API body into a [`PredecessorState`].
///
/// `state: "open"|"closed"`, `merged: bool`, `head.sha`. Any missing or
/// mistyped field is a `None` (unknown), never a default: an unparseable
/// head must not compare equal to the recorded pin, and an unparseable
/// `merged` must not read as merged or unmerged.
pub fn predecessor_from_json(body: &[u8]) -> Option<PredecessorState> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let open = match v.get("state").and_then(|s| s.as_str()) {
        Some("open") => true,
        Some("closed") => false,
        _ => return None,
    };
    let merged = match v.get("merged") {
        Some(serde_json::Value::Bool(b)) => *b,
        _ => return None,
    };
    let head_sha = match v.pointer("/head/sha") {
        Some(serde_json::Value::String(s)) if is_full_sha(s) => Some(s.clone()),
        Some(_) => return None,
        None => None,
    };
    let updated_at = v
        .get("updated_at")
        .and_then(|s| s.as_str())
        .map(str::to_string);
    Some(PredecessorState {
        open,
        merged,
        head_sha,
        updated_at,
    })
}

#[cfg(test)]
mod tests;
