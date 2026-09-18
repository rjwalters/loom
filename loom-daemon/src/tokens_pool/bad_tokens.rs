//! Bad-token tracking, ported from `loom_tools.tokens.bad_tokens`.
//!
//! Tokens that fail with `TOKEN_EXPIRED`, `TOKEN_EXHAUSTED`, or otherwise
//! prove unusable are appended to `.loom/tokens/.bad_tokens`. Subsequent
//! selection calls skip these tokens.
//!
//! File format (one entry per line):
//! `<ISO8601 UTC timestamp> <token_name> <reason words...>`
//!
//! Reads use a word-boundary regex so `agent-1` and `agent-10` do not
//! collide — the exact behavior the Python word-boundary regex provides.
//!
//! # Model-class scoping (#8058)
//!
//! An exhaustion that is known to be scoped to a single **model class**
//! (`MODEL_CREDITS_EXHAUSTED`, or the #4501 per-model ceiling) appends a
//! [`MODEL_CLASS_MARKER_PREFIX`] marker to the free-form reason:
//!
//! ```text
//! 2026-09-17T04:05:06Z agent-1 exhausted: out of usage credits [model-class:opus]
//! ```
//!
//! The line format is **unchanged** — the marker lives inside the existing
//! free-form reason field, so every pre-#8058 reader (and every reader that
//! never asks about a class) treats it as ordinary reason text and keeps
//! blocking the account account-wide.
//!
//! The rule, in both directions:
//!
//! * A **class-less** entry — every line written before #8058, every
//!   `TOKEN_EXPIRED`/auth line, and every account-wide weekly/plan exhaustion —
//!   blocks **all** classes. Nothing here ever narrows an existing mark.
//! * A **class-scoped** entry blocks only the class it names, and only for a
//!   caller that asked about a class ([`is_bad_for_class`] /
//!   [`blocking_entry_for_class`]). [`is_bad`] and [`blocking_entry`] keep
//!   their account-wide meaning verbatim, because their callers outside this
//!   module (`sweep_registry::quarantine`, `sweep_registry::crash_signals`)
//!   mean "is this account unusable at all" and silently narrowing them would
//!   readmit genuinely dead accounts.
//! * Each class-scoped mark is its own line with its own timestamp, so it ages
//!   out on its own schedule, independently of any account-wide line for the
//!   same account.

use std::path::{Path, PathBuf};

use chrono::Utc;
use regex::Regex;

use super::locking::MkdirLock;
use super::paths::resolve_tokens_dir;

/// Default cooldown (seconds) after which a non-auth (exhaustion) bad-token
/// entry stops blocking selection (#4122). Weekly/session-limit exhaustion is
/// transient — the account recovers on its own — so those entries expire while
/// auth-reason entries (a broken credential) remain permanent. Mirrors the
/// Python reference default (`bad_tokens.py::cleanup_bad_tokens`, 6h).
pub const DEFAULT_EXHAUSTION_COOLDOWN_SECS: i64 = 6 * 3600;

/// Env override (whole seconds, must parse to `> 0`) for
/// [`DEFAULT_EXHAUSTION_COOLDOWN_SECS`].
pub const EXHAUSTION_COOLDOWN_ENV: &str = "LOOM_TOKEN_EXHAUSTION_COOLDOWN_SECS";

/// Opening delimiter of the model-class marker embedded in a `.bad_tokens`
/// reason (#8058). The full marker is `[model-class:<class>]`.
///
/// Deliberately a **suffix inside the existing free-form reason field** rather
/// than a new column: `.bad_tokens` lines are parsed as
/// `<ts> <name> <reason...>` by this module, by `tokens check`'s reader and by
/// several shell call sites, so adding a column would break every one of them.
/// A marker inside the reason is invisible to all of them and — critically —
/// keeps a class-scoped line blocking account-wide for any reader that has not
/// been taught about classes.
pub const MODEL_CLASS_MARKER_PREFIX: &str = "[model-class:";

/// Resolve a model alias or pinned ID to the `.bad_tokens` model class
/// (`haiku` / `sonnet` / `opus` / `fable`), or `None` when the value is empty
/// or unrecognized (#8058).
///
/// Delegates to [`crate::script_helpers::model_tiers::task_alias_of`] — the
/// classifier the sweep orchestrator's cost ladder already uses — so there is
/// exactly one definition of "which class is this model" in the tree.
///
/// **Unrecognized is `None`, never an error.** Both the mark path and the
/// select path treat `None` as "no class information", which degrades to
/// today's account-wide behaviour. Selection must never fail closed on a model
/// name it does not know.
#[must_use]
pub fn model_class_of(model: &str) -> Option<String> {
    let alias = crate::script_helpers::model_tiers::task_alias_of(model);
    if alias.is_empty() {
        None
    } else {
        Some(alias)
    }
}

/// Append the [`MODEL_CLASS_MARKER_PREFIX`] marker to `reason` for
/// `model_class`, or return `reason` unchanged when there is no class (#8058).
///
/// `model_class` is expected to already be a normalized class from
/// [`model_class_of`]; an empty/blank class is treated as no class.
#[must_use]
pub fn scoped_reason(reason: &str, model_class: Option<&str>) -> String {
    match model_class.map(str::trim).filter(|c| !c.is_empty()) {
        Some(class) => {
            let base = reason.trim();
            if base.is_empty() {
                format!("{MODEL_CLASS_MARKER_PREFIX}{class}]")
            } else {
                format!("{base} {MODEL_CLASS_MARKER_PREFIX}{class}]")
            }
        }
        None => reason.to_string(),
    }
}

/// The model class a `.bad_tokens` reason is scoped to, or `None` for a
/// class-less (account-wide) reason (#8058).
///
/// Tolerant by construction: an unterminated or empty marker reads as `None`,
/// i.e. account-wide — the fail-safe direction, since a malformed marker must
/// never narrow an entry that was meant to block everything.
#[must_use]
pub fn reason_model_class(reason: &str) -> Option<&str> {
    let start = reason.find(MODEL_CLASS_MARKER_PREFIX)? + MODEL_CLASS_MARKER_PREFIX.len();
    let rest = &reason[start..];
    let end = rest.find(']')?;
    let class = rest[..end].trim();
    if class.is_empty() {
        None
    } else {
        Some(class)
    }
}

/// Whether a `.bad_tokens` entry whose reason is `reason` blocks selection for
/// `queried_class` (#8058).
///
/// * A class-less entry blocks every class (and the class-less query).
/// * A class-scoped entry blocks the class-less query (so [`is_bad`]'s
///   account-wide meaning is unchanged) and its own class only.
///
/// The entry's marker is normalized through [`model_class_of`] before the
/// comparison, so a writer with no classifier of its own — `claude-wrapper.sh`
/// appends the raw `$LOOM_MODEL` — is read correctly
/// (`[model-class:claude-opus-5]` blocks `opus`). A marker that normalizes to
/// nothing is treated as **class-less**, i.e. it blocks everything: an
/// unreadable marker must never be able to narrow an entry to nothing.
#[must_use]
fn reason_blocks_class(reason: &str, queried_class: Option<&str>) -> bool {
    let Some(queried) = queried_class.map(str::trim).filter(|c| !c.is_empty()) else {
        // No class asked about: the account-wide question. Every entry counts.
        return true;
    };
    match reason_model_class(reason).and_then(model_class_of) {
        None => true,
        Some(entry_class) => entry_class.eq_ignore_ascii_case(queried),
    }
}

/// On-disk retention for **non-auth** (exhaustion) entries on the routine
/// cleanup path (#4643).
///
/// 24h = 4× the default 6h read-time cooldown: an exhaustion line stays on
/// disk long after it stopped blocking selection (so an operator debugging the
/// previous night's incident still sees it), but `.bad_tokens` can no longer
/// grow without bound — the live shared pool had accumulated entries for days
/// because [`cleanup_bad_tokens`] had **zero callers** before #4643.
pub const DEFAULT_CLEANUP_MAX_AGE_SECS: i64 = 24 * 3600;

/// Floor retention for **auth** entries on the cleanup path (#4643).
///
/// Auth entries are permanent *at read time* ([`is_bad`] never expires them),
/// so pruning one on the routine 24h schedule would silently readmit a broken
/// credential into rotation — exactly the failure mode the auth/exhaustion
/// split exists to prevent. Cleanup therefore never drops an auth entry that
/// is younger than 30 days, regardless of the `max_age_seconds` it was called
/// with; only genuinely ancient auth lines (a credential retired a month ago)
/// are reclaimed. The permanence contract is: **exhaustion entries clear
/// themselves, auth entries clear only via `loom-daemon tokens unblock`** —
/// the 30d floor is garbage collection, not expiry.
pub const AUTH_ENTRY_MIN_RETENTION_SECS: i64 = 30 * 24 * 3600;

/// Resolve the exhaustion-entry cooldown: the [`EXHAUSTION_COOLDOWN_ENV`]
/// override (whole seconds, `> 0`) or [`DEFAULT_EXHAUSTION_COOLDOWN_SECS`].
#[must_use]
pub fn exhaustion_cooldown_secs() -> i64 {
    std::env::var(EXHAUSTION_COOLDOWN_ENV)
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_EXHAUSTION_COOLDOWN_SECS)
}

fn tokens_dir(workspace: &Path) -> PathBuf {
    resolve_tokens_dir(workspace)
}

fn bad_tokens_path(tokens_dir: &Path) -> PathBuf {
    tokens_dir.join(".bad_tokens")
}

fn lock_path(tokens_dir: &Path) -> PathBuf {
    tokens_dir.join(".bad_tokens.lock")
}

fn name_pattern(token_name: &str) -> Regex {
    let escaped = regex::escape(token_name);
    // (?m) matches Python's re.MULTILINE for ^/$ anchors.
    Regex::new(&format!(r"(?m)(^|\s){escaped}(\s|$)")).expect("valid generated regex")
}

/// Reasons treated as "auth" for `unblock` (ported from `cli.py`'s
/// `_AUTH_REASON_RE`). Matched case-insensitively against the free-form
/// reason field. Deliberately does **not** match `TOKEN_EXHAUSTED` — those
/// expire on their own.
fn auth_reason_regex() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)\b(401|oauth|auth(entication)?|unauthorized|token[_\s]?expired|expired|blocked)\b",
        )
        .expect("valid generated regex")
    })
}

/// Whether `reason` names Claude's **5h session-limit** window specifically
/// (issue #7522) — the product wording for the rolling 5h quota ("You've hit
/// your session limit"), written verbatim into the `.bad_tokens` reason field
/// by `claude-wrapper.sh`'s `rotate_exhausted_account` / the daemon's own
/// insta-crash classifier. Deliberately excludes a CONCURRENT-session
/// capacity fault ("reached your concurrent session limit" / "too many
/// concurrent sessions") — that is never marked bad in the first place (see
/// `sweep_registry::crash_signals::session_capacity_exclusion` and
/// `claude-wrapper.sh`'s own capacity-vs-quota gate ahead of
/// `rotate_exhausted_account`), so this is belt-and-suspenders: a reason
/// string mentioning "concurrent" is never treated as a session-window signal
/// even if one somehow reached this function.
///
/// Only entries matching this are eligible for the [`SESSION_WINDOW_SECS`] cap
/// and the early-expiry check in [`session_window_has_reset`] — every other
/// exhaustion reason (weekly, monthly, per-model, credits) keeps the
/// unmodified fixed-TTL behavior.
pub(crate) fn is_session_limit_reason(reason: &str) -> bool {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re =
        RE.get_or_init(|| Regex::new(r"(?i)\bsession limit\b").expect("valid generated regex"));
    re.is_match(reason) && !reason.to_ascii_lowercase().contains("concurrent")
}

/// Length of Claude's rolling **session** window, and therefore the hard upper
/// bound on how long a session-limit `.bad_tokens` entry can possibly remain
/// valid (issue #7522).
///
/// The window is 5h long and starts at the account's first message in it, so
/// an account that *hit* its session limit at `marked_at` was already inside a
/// window that began at some `start <= marked_at` and therefore resets at
/// `start + 5h <= marked_at + 5h`. Holding such an entry for the generic
/// [`DEFAULT_EXHAUSTION_COOLDOWN_SECS`] (6h — sized for the durable
/// weekly/monthly signals) therefore over-holds the account by up to an hour
/// *past the point where it is provably usable again*, which is exactly the
/// "roughly an hour of fleet-wide starvation at every 5h boundary" the issue
/// measures.
///
/// Used as a **cap**, never as a floor: the effective cooldown for a
/// session-limit entry is `min(exhaustion_cooldown_secs(), SESSION_WINDOW_SECS)`,
/// so an operator who deliberately configures a *shorter*
/// [`EXHAUSTION_COOLDOWN_ENV`] still gets the shorter value. It is also only a
/// backstop: whenever `.ranking` carries positive evidence the window already
/// rolled over ([`session_window_has_reset`]), the entry expires earlier still.
pub const SESSION_WINDOW_SECS: i64 = 5 * 3600;

/// 5h-utilization threshold below which a re-probed account is considered to
/// have rolled into a fresh session window (issue #7522). Mirrors
/// `select::DEFAULT_5H_LOAD_GATE` — the same threshold selection already uses
/// to decide "this account is not overloaded" — rather than inventing a
/// second, unrelated cutoff.
const SESSION_RESET_UTIL_THRESHOLD: f64 = 0.70;

/// Determine whether the account's 5h session window has already rolled over,
/// per the live `.ranking` snapshot in `tokens_dir` (issue #7522).
///
/// Returns:
/// - `Some(true)` — a `.ranking` row for `token_name` proves the window has
///   reset: either its recorded 5h reset instant (`limit_reset`, issue #4874)
///   falls **after** `marked_at` (so it describes the window that produced this
///   mark) and is now in the past, or a probe taken **after** `marked_at`
///   reports 5h utilization back under [`SESSION_RESET_UTIL_THRESHOLD`] (the
///   "next `tokens check` probe" signal the issue also asks for, used when no
///   exact reset instant was recorded).
/// - `Some(false)` — a `.ranking` row exists but proves the window has not
///   yet reset.
/// - `None` — no usable evidence either way (no `.ranking` file, no row for
///   this account, or the only row present predates the mark) — the caller
///   falls back to the fixed-TTL cooldown.
///
/// Note on `limit_reset`: the field is the row's **binding**-window reset, and
/// which window that is depends on the row's status — [`super::check::limit_reset`]
/// records the *7d* reset for an `exhausted` row and the 5h reset for every
/// other status. It is therefore not unconditionally a 5h instant, so an
/// `exhausted` row's instant is ignored here (it answers a different question)
/// and the mtime-gated utilization branch decides instead.
fn session_window_has_reset(
    tokens_dir: &Path,
    token_name: &str,
    marked_at: i64,
    now: i64,
) -> Option<bool> {
    let ranking_path = tokens_dir.join(".ranking");
    let meta = std::fs::metadata(&ranking_path).ok()?;
    let ranking_mtime: i64 = meta
        .modified()
        .ok()
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64);
    let text = std::fs::read_to_string(&ranking_path).ok()?;
    let row = text
        .lines()
        .filter_map(super::select::parse_ranking_line)
        .find(|r| r.name == token_name)?;

    // Strongest signal: the recorded 5h reset instant for the window that was
    // gating this account (issue #4874) has already passed.
    //
    // Freshness is still required, for the same reason the utilization branch
    // below requires it: the instant itself is a wall-clock fact, but what is
    // NOT established by a stale row is that it describes *the window that
    // produced this mark*. A row predating the mark records an already-elapsed
    // reset, which would make a FRESH session-limit mark an instant no-op —
    // selection hands the account back out, the child insta-crashes on the same
    // limit, the wrapper re-marks it, and the same stale row expires it again:
    // a thrash loop with no backoff. `reset > marked_at` is the cheapest proof
    // that the recorded window had not yet closed when the mark was written,
    // i.e. that it is this mark's own window.
    //
    // `exhausted` rows are skipped entirely: for those `check::limit_reset`
    // records the 7d reset, not a 5h instant, so the value answers a different
    // question and the utilization branch below is the honest reader.
    if row.status != "exhausted" {
        if let Some(reset) = row.limit_reset.as_deref() {
            if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(reset, "%Y-%m-%dT%H:%M:%SZ") {
                let reset_at = naive.and_utc().timestamp();
                if reset_at > marked_at {
                    return Some(reset_at <= now);
                }
            }
        }
    }

    // Fallback: a probe taken AFTER this entry was marked bad reports 5h
    // utilization back under the load-gate threshold — the window rolled
    // over even though we never learned its exact reset instant.
    if ranking_mtime > marked_at {
        if let Some(util) = row.util_5h {
            return Some(util < SESSION_RESET_UTIL_THRESHOLD);
        }
    }

    None
}

/// Two-signal early-release check for an **ambiguous** (non-session-limit-
/// matching) exhaustion entry (issue #7538).
///
/// [`session_window_has_reset`] is deliberately NOT reused here, and this is a
/// distinct, separate check rather than a widened version of it: releasing an
/// ambiguous entry on 5h evidence alone would risk readmitting a genuinely
/// **weekly**-exhausted account after only 5h, because a weekly-blocked
/// account's 5h utilization reads "reset" too (its 5h usage is near zero
/// regardless of the still-active 7-day block) — exactly the harm issue #4212
/// declined to risk by leaving ambiguous reasons out of the regex in the first
/// place. So an ambiguous entry is released only when the re-probed
/// `.ranking` row shows **both**:
///
/// 1. 5h utilization under the load gate ([`super::select::resolve_load_gate`],
///    the same threshold [`session_window_has_reset`]'s fallback and
///    `select`'s own tier-1 pick use — not a second, unrelated cutoff), AND
/// 2. 7d utilization under the exhausted threshold
///    ([`super::check::EXHAUSTED_THRESHOLD`]).
///
/// `.ranking` (`name|status|5h_util|limit_reset`, see
/// [`super::select::parse_ranking_line`]) does not carry a raw 7d-utilization
/// field — only the `status` string a probe already derived from it
/// (`super::check::status_from_utilization`: `status == "exhausted"` iff 7d
/// utilization cleared [`super::check::EXHAUSTED_THRESHOLD`]). So signal 2 is
/// read off that pre-computed field (`row.status != "exhausted"`) rather than
/// a second arithmetic comparison — it is the same threshold, just already
/// applied by the probe that wrote the row.
///
/// Requires **freshness**, exactly like [`session_window_has_reset`]'s
/// fallback branch: the `.ranking` file's mtime must be after `marked_at`, so
/// a stale pre-mark row (e.g. left over from before this entry was ever
/// probed) can never satisfy the check. This is what makes the widened
/// probe-eligibility in `check::discover_tokens` (#7538) load-bearing — an
/// ambiguous entry that has never been re-probed since it was marked bad has
/// no fresh row to read, and correctly falls through to `None` (full
/// fixed-TTL cooldown, unchanged).
///
/// Returns:
/// - `Some(true)` — a fresh `.ranking` row proves both signals hold; safe to
///   release early.
/// - `Some(false)` — a fresh row exists but at least one signal does not hold.
/// - `None` — no usable evidence (no `.ranking` file, no row for this
///   account, the only row present predates the mark, or the row has no 5h
///   utilization) — the caller falls back to the fixed-TTL cooldown.
fn ambiguous_exhaustion_has_reset(
    tokens_dir: &Path,
    token_name: &str,
    marked_at: i64,
) -> Option<bool> {
    let ranking_path = tokens_dir.join(".ranking");
    let meta = std::fs::metadata(&ranking_path).ok()?;
    let ranking_mtime: i64 = meta
        .modified()
        .ok()
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64);
    if ranking_mtime <= marked_at {
        // No probe evidence taken since this entry was marked bad.
        return None;
    }
    let text = std::fs::read_to_string(&ranking_path).ok()?;
    let row = text
        .lines()
        .filter_map(super::select::parse_ranking_line)
        .find(|r| r.name == token_name)?;
    let util_5h = row.util_5h?;
    let five_h_under_load_gate = util_5h < super::select::resolve_load_gate();
    let seven_d_under_exhausted_threshold = row.status != "exhausted";
    Some(five_h_under_load_gate && seven_d_under_exhausted_threshold)
}

/// Append a bad-token entry atomically.
///
/// # Errors
/// Returns an error if the tokens dir does not exist or the lock cannot be
/// acquired.
pub fn mark_bad(workspace: &Path, token_name: &str, reason: &str) -> Result<(), String> {
    mark_bad_in_dir(&tokens_dir(workspace), token_name, reason)
}

/// [`mark_bad`], scoping the entry to one model class when `model` names a
/// model [`model_class_of`] recognizes (#8058).
///
/// `model` is the **resolved model in flight** (`$LOOM_MODEL`), not a model
/// parsed out of the error text: a per-model ceiling / credit exhaustion means
/// "the class we were running ran out", so the class is a property of the
/// dispatch, not of the message.
///
/// `None`, an empty value, or a model the classifier does not recognize all
/// produce an ordinary **class-less** (account-wide) entry — the fail-safe
/// direction. This function can therefore never write a mark narrower than
/// what pre-#8058 code would have written for the same failure.
///
/// # Errors
/// Same as [`mark_bad`].
pub fn mark_bad_for_model(
    workspace: &Path,
    token_name: &str,
    reason: &str,
    model: Option<&str>,
) -> Result<(), String> {
    let class = model.and_then(|m| model_class_of(m));
    mark_bad_in_dir(&tokens_dir(workspace), token_name, &scoped_reason(reason, class.as_deref()))
}

/// [`mark_bad`], operating directly on an already-resolved tokens directory
/// rather than re-deriving one from a workspace root.
///
/// Mirrors the `_in_dir` / workspace-anchored split already used by
/// [`blocking_entry_in_dir`] / [`blocking_entry`] and
/// [`cleanup_bad_tokens_in_dir`] / [`cleanup_bad_tokens`]: a caller that
/// already holds the resolved `.loom/tokens` directory (e.g.
/// `tokens_pool::check`, which resolves it once via
/// `resolve_tokens_pool_dir_for_cli`) must not pass it back through
/// [`mark_bad`] as if it were a *workspace* — [`tokens_dir`] would then look
/// for `<already-resolved-dir>/.loom/tokens`, one level too deep.
///
/// # Errors
/// Returns an error if `dir` does not exist or the lock cannot be acquired.
pub fn mark_bad_in_dir(dir: &Path, token_name: &str, reason: &str) -> Result<(), String> {
    if !dir.is_dir() {
        return Err(format!("Tokens dir does not exist: {}", dir.display()));
    }

    let timestamp = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let safe_reason = reason.replace(['\n', '\r'], " ");
    let safe_reason = safe_reason.trim();
    let line = format!("{timestamp} {token_name} {safe_reason}\n");

    let _lock = MkdirLock::acquire(&lock_path(dir))?;
    use std::io::Write as _;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(bad_tokens_path(dir))
        .map_err(|e| e.to_string())?;
    f.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
    Ok(())
}

/// Why a `.bad_tokens` entry blocks selection — the reason class an operator
/// needs in order to know whether the block clears itself (#4643).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadReasonClass {
    /// Auth reason (401 / OAuth / expired / blocked) — permanent at read time,
    /// clears only via `loom-daemon tokens unblock <name>`.
    Auth,
    /// Non-auth ("exhausted" / "rate-limited") — expires by itself once the
    /// entry ages past [`exhaustion_cooldown_secs`] (#4122).
    Exhaustion,
    /// The entry's timestamp did not parse. Fail-closed: treated as permanent,
    /// because we never silently un-block a token on a malformed line.
    MalformedTimestamp,
}

impl BadReasonClass {
    /// Short class label used in operator-facing output.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Exhaustion => "exhaustion",
            Self::MalformedTimestamp => "malformed-timestamp",
        }
    }

    /// Whether entries of this class ever clear without operator action.
    #[must_use]
    pub fn permanence(self) -> &'static str {
        match self {
            Self::Auth => "permanent",
            Self::Exhaustion => "TTL",
            Self::MalformedTimestamp => "permanent (fail-closed)",
        }
    }
}

/// The `.bad_tokens` entry that is currently keeping a token out of rotation,
/// as returned by [`blocking_entry`] (#4643). Everything an operator needs to
/// tell "this clears itself in 12 minutes" apart from "this needs `unblock`".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockingEntry {
    /// Raw first field of the line (the ISO-8601 UTC timestamp, or whatever
    /// unparseable text stood in its place).
    pub timestamp: String,
    /// Free-form reason text as recorded by [`mark_bad`].
    pub reason: String,
    /// Auth (permanent) vs exhaustion (TTL) vs malformed (fail-closed).
    pub class: BadReasonClass,
    /// Seconds until this entry stops blocking selection. `Some` only for
    /// [`BadReasonClass::Exhaustion`]; `None` means "never, without operator
    /// action".
    pub cooldown_remaining_secs: Option<i64>,
}

/// Return the `.bad_tokens` entry currently blocking `token_name`, or `None`
/// when the token is selectable.
///
/// This is the single scan that decides bad-ness: [`is_bad`] is defined as
/// `blocking_entry(..).is_some()`, so the boolean the selector acts on and the
/// detail the operator reads can never de-sync (#4643 — diagnosing the
/// 2026-07-30 incident required reading Rust source precisely because the
/// selector's decision was unexplained).
///
/// Auth-reason entries (a broken credential — matched by [`auth_reason_regex`])
/// block permanently. Non-auth ("exhaustion") entries block only until they age
/// past [`exhaustion_cooldown_secs`] (#4122): weekly/session-limit exhaustion is
/// transient, so a stale exhaustion line no longer keeps an otherwise-healthy
/// account out of rotation even before [`cleanup_bad_tokens`] prunes it from
/// disk. A line whose timestamp cannot be parsed is treated as permanent
/// (fail-closed). An *expired* entry does not short-circuit the scan — a later
/// line for the same account may still block (this is the common live shape:
/// every retry appends a fresh line, so the newest entry, not the oldest
/// visible one, is the one that matters).
///
/// Reads are unsynchronized — readers see a consistent file because writers
/// only ever append whole lines.
#[must_use]
pub fn blocking_entry(workspace: &Path, token_name: &str) -> Option<BlockingEntry> {
    blocking_entry_in_dir(&tokens_dir(workspace), token_name)
}

/// [`blocking_entry`], restricted to entries that block `model_class` (#8058).
///
/// `None` for `model_class` is exactly [`blocking_entry`] — the account-wide
/// question. `Some(class)` additionally skips entries scoped to a *different*
/// class (see the module docs): a class-less entry, and an auth entry of any
/// shape, still block every class.
#[must_use]
pub fn blocking_entry_for_class(
    workspace: &Path,
    token_name: &str,
    model_class: Option<&str>,
) -> Option<BlockingEntry> {
    blocking_entry_in_dir_for_class(&tokens_dir(workspace), token_name, model_class)
}

/// [`blocking_entry`], operating directly on an already-resolved tokens
/// directory rather than re-deriving one from a workspace root (issue #6030).
///
/// Callers that already hold the resolved `.loom/tokens` directory (e.g.
/// `tokens_pool::check`, which resolves it once via
/// `resolve_tokens_pool_dir_for_cli` before probing) must not pass that
/// directory back through [`blocking_entry`] as if it were a *workspace* —
/// [`tokens_dir`] would try to resolve `<already-resolved-dir>/.loom/tokens`,
/// one level too deep. This mirrors the existing `_in_dir` / workspace-anchored
/// split already used by [`cleanup_bad_tokens_in_dir`] / [`cleanup_bad_tokens`].
#[must_use]
pub fn blocking_entry_in_dir(tokens_dir: &Path, token_name: &str) -> Option<BlockingEntry> {
    blocking_entry_in_dir_for_class(tokens_dir, token_name, None)
}

/// [`blocking_entry_in_dir`], restricted to entries that block `model_class`
/// (#8058). See [`blocking_entry_for_class`] for the semantics; this is the
/// single scan both the class-aware and account-wide readers share, so the two
/// can never drift apart.
#[must_use]
pub fn blocking_entry_in_dir_for_class(
    tokens_dir: &Path,
    token_name: &str,
    model_class: Option<&str>,
) -> Option<BlockingEntry> {
    let bad_file = bad_tokens_path(tokens_dir);
    let text = std::fs::read_to_string(&bad_file).ok()?;
    let pattern = name_pattern(token_name);
    let cooldown = exhaustion_cooldown_secs();
    let now = Utc::now().timestamp();
    for line in text.lines() {
        let stripped = line.trim();
        if stripped.is_empty() || !pattern.is_match(stripped) {
            continue;
        }
        // Parse `<ts> <name> <reason...>` from this matching line.
        let mut parts = stripped.splitn(3, ' ');
        let ts_str = parts.next().unwrap_or("");
        let _name = parts.next();
        let reason = parts.next().unwrap_or("");
        // Auth entries never expire — and are never class-scoped, whatever
        // marker the reason happens to carry (#8058 edge case): a broken
        // credential is broken for every model class, so auth wins over the
        // class filter below and blocks everything.
        if auth_reason_regex().is_match(reason) {
            return Some(BlockingEntry {
                timestamp: ts_str.to_string(),
                reason: reason.to_string(),
                class: BadReasonClass::Auth,
                cooldown_remaining_secs: None,
            });
        }
        // #8058: an entry scoped to a different model class does not block
        // this query. A class-less entry, or a class-less query, always does.
        if !reason_blocks_class(reason, model_class) {
            continue;
        }
        // Non-auth (exhaustion) entries expire after the cooldown. A
        // malformed/missing timestamp fails closed (permanent).
        match chrono::NaiveDateTime::parse_from_str(ts_str, "%Y-%m-%dT%H:%M:%SZ") {
            Ok(naive) => {
                let marked_at = naive.and_utc().timestamp();
                let age = now - marked_at;
                // #7522: a session-limit (5h window) entry must not outlive
                // its own window. Two independent relaxations apply, and ONLY
                // to a reason that names the session window specifically —
                // every other exhaustion reason (weekly, monthly, per-model,
                // credits) keeps the unmodified fixed-TTL behavior:
                //
                //  1. the TTL is capped at `SESSION_WINDOW_SECS` (the provable
                //     upper bound on the window's reset), so the entry clears
                //     on its own even on a host with no usable `.ranking`
                //     evidence at all; and
                //  2. positive evidence that the window ALREADY rolled over
                //     (`session_window_has_reset`) expires it earlier still.
                let session_limit = is_session_limit_reason(reason);
                let effective_cooldown = if session_limit {
                    cooldown.min(SESSION_WINDOW_SECS)
                } else {
                    cooldown
                };
                let mut blocked = age < effective_cooldown;
                if blocked
                    && session_limit
                    && session_window_has_reset(tokens_dir, token_name, marked_at, now)
                        == Some(true)
                {
                    blocked = false;
                }
                // #7538: an AMBIGUOUS (non-session-limit-matching) exhaustion
                // reason gets its own, distinct two-signal release check —
                // never the single-signal `session_window_has_reset` above,
                // and never the `SESSION_WINDOW_SECS` cap baked into
                // `effective_cooldown` (that stays scoped to `session_limit`
                // only). See `ambiguous_exhaustion_has_reset` for why a
                // second signal is required.
                if blocked
                    && !session_limit
                    && ambiguous_exhaustion_has_reset(tokens_dir, token_name, marked_at)
                        == Some(true)
                {
                    blocked = false;
                }
                if blocked {
                    return Some(BlockingEntry {
                        timestamp: ts_str.to_string(),
                        reason: reason.to_string(),
                        class: BadReasonClass::Exhaustion,
                        // Reported against the EFFECTIVE cooldown so the
                        // operator-facing "clears in <N>" line (claude-wrapper's
                        // rotation log, `tokens check`'s table) never promises a
                        // longer hold than the code will actually honor.
                        cooldown_remaining_secs: Some(effective_cooldown - age),
                    });
                }
            }
            Err(_) => {
                return Some(BlockingEntry {
                    timestamp: ts_str.to_string(),
                    reason: reason.to_string(),
                    class: BadReasonClass::MalformedTimestamp,
                    cooldown_remaining_secs: None,
                })
            }
        }
    }
    None
}

/// Return `true` if `token_name` is currently bad-marked.
///
/// Thin boolean projection of [`blocking_entry`] — see there for the
/// auth-vs-exhaustion permanence rules.
#[must_use]
pub fn is_bad(workspace: &Path, token_name: &str) -> bool {
    blocking_entry(workspace, token_name).is_some()
}

/// Return `true` if `token_name` is currently bad-marked **for `model_class`**
/// (#8058).
///
/// Thin boolean projection of [`blocking_entry_for_class`]. `None` is exactly
/// [`is_bad`]; `Some(class)` is a strictly narrower question — it can only
/// ever return `false` where [`is_bad`] returns `true`, never the reverse.
#[must_use]
pub fn is_bad_for_class(workspace: &Path, token_name: &str, model_class: Option<&str>) -> bool {
    blocking_entry_for_class(workspace, token_name, model_class).is_some()
}

/// A `.bad_tokens` line for an account, reported **without** the liveness
/// rules [`blocking_entry_in_dir`] applies (issue #7536 review) — the
/// historical record of *why* the account was last marked bad, whether or not
/// that mark still blocks selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoricalEntry {
    /// Raw first field of the line (the ISO-8601 UTC timestamp, or whatever
    /// unparseable text stood in its place).
    pub timestamp: String,
    /// Free-form reason text as recorded by [`mark_bad`].
    pub reason: String,
}

/// The most recent `.bad_tokens` entry recorded for `token_name`, ignoring
/// cooldown/permanence entirely: `Some` whenever the account appears in the
/// file at all, even if every entry has long since expired.
///
/// [`blocking_entry_in_dir`] answers "is this account blocked *right now*",
/// which deliberately collapses "never marked bad" and "marked bad, entry
/// expired" into the same `None`. Some callers need those two apart — notably
/// [`latest_block_was_session_limit`], which readmits a stale `.ranking`
/// `blocked` row only on *positive* evidence the block came from a session
/// limit rather than from a probe that found a dead credential.
///
/// "Most recent" is by parsed timestamp, with a later file position breaking
/// ties (writers only append, so position is the tiebreak the file itself
/// implies); an entry whose timestamp does not parse loses to any entry whose
/// timestamp does, since it carries no position in time at all.
#[must_use]
pub fn latest_entry_in_dir(tokens_dir: &Path, token_name: &str) -> Option<HistoricalEntry> {
    let text = std::fs::read_to_string(bad_tokens_path(tokens_dir)).ok()?;
    let pattern = name_pattern(token_name);
    let mut best: Option<(Option<i64>, HistoricalEntry)> = None;
    for line in text.lines() {
        let stripped = line.trim();
        if stripped.is_empty() || !pattern.is_match(stripped) {
            continue;
        }
        let mut parts = stripped.splitn(3, ' ');
        let ts_str = parts.next().unwrap_or("");
        let _name = parts.next();
        let reason = parts.next().unwrap_or("");
        let ts = chrono::NaiveDateTime::parse_from_str(ts_str, "%Y-%m-%dT%H:%M:%SZ")
            .ok()
            .map(|naive| naive.and_utc().timestamp());
        let entry = HistoricalEntry {
            timestamp: ts_str.to_string(),
            reason: reason.to_string(),
        };
        let supersedes = match &best {
            None => true,
            Some((best_ts, _)) => match (ts, *best_ts) {
                (Some(new), Some(old)) => new >= old,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => true,
            },
        };
        if supersedes {
            best = Some((ts, entry));
        }
    }
    best.map(|(_, entry)| entry)
}

/// Whether the account's most recent `.bad_tokens` entry names the 5h session
/// window ([`is_session_limit_reason`]) — i.e. whether a `blocked` state for
/// this account is *known* to have come from a session-limit mark that expires
/// on its own (issue #7522), as opposed to something that never self-heals.
///
/// `false` when the account has **no** `.bad_tokens` history at all. That case
/// is not "not a session limit, therefore harmless": `tokens check` writes a
/// `blocked` `.ranking` status for a probe that returned **401**
/// (`super::check`'s `auth_401`) or for a credential **shape mismatch**
/// (`shape_mismatch`, #5608) *without* ever calling [`mark_bad`], so a revoked
/// or mis-bound account is exactly a `blocked` row with an empty history. Those
/// must keep #5629's unconditional hard exclusion — a 401 never self-heals, so
/// readmitting it lets the fail-safe retry hand out a permanently dead
/// credential forever and costs the #4643 empty-pool diagnostic on an all-dead
/// pool. Hence the predicate is phrased as positive evidence, and every
/// no-evidence case answers `false`.
#[must_use]
pub fn latest_block_was_session_limit(tokens_dir: &Path, token_name: &str) -> bool {
    latest_entry_in_dir(tokens_dir, token_name)
        .is_some_and(|entry| is_session_limit_reason(&entry.reason))
}

/// Outcome of a [`cleanup_bad_tokens_in_dir`] pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CleanupOutcome {
    /// Entries still on disk after the pass.
    pub kept: usize,
    /// Entries dropped by this pass (`0` ⇒ the file was not rewritten).
    pub removed: usize,
}

/// Split `text` into (retained lines, removed count) under the reason-aware
/// max-age policy. Pure — no I/O — so the lock-free pre-check and the
/// under-lock rewrite in [`cleanup_bad_tokens_in_dir`] apply identical rules.
fn partition_by_age(text: &str, max_age_seconds: i64, now: i64) -> (Vec<String>, usize) {
    let cutoff = now - max_age_seconds;
    // Auth entries are permanent at read time, so they get a much longer floor
    // (see [`AUTH_ENTRY_MIN_RETENTION_SECS`]): never dropped before 30 days,
    // no matter how aggressive the requested max age is.
    let auth_cutoff = now - max_age_seconds.max(AUTH_ENTRY_MIN_RETENTION_SECS);
    let mut kept: Vec<String> = Vec::new();
    let mut removed = 0usize;
    for line in text.lines() {
        let stripped = line.trim();
        if stripped.is_empty() {
            continue;
        }
        let mut parts = stripped.splitn(3, ' ');
        let ts_str = parts.next().unwrap_or("");
        let _name = parts.next();
        let reason = parts.next().unwrap_or("");
        let entry_cutoff = if auth_reason_regex().is_match(reason) {
            auth_cutoff
        } else {
            cutoff
        };
        match chrono::NaiveDateTime::parse_from_str(ts_str, "%Y-%m-%dT%H:%M:%SZ") {
            Ok(naive) => {
                if naive.and_utc().timestamp() >= entry_cutoff {
                    kept.push(line.to_string());
                } else {
                    removed += 1;
                }
            }
            Err(_) => {
                // Malformed line — keep it so we don't silently lose data.
                kept.push(line.to_string());
            }
        }
    }
    (kept, removed)
}

/// Drop `.bad_tokens` entries in `dir` older than `max_age_seconds`, with
/// **auth** entries held for at least [`AUTH_ENTRY_MIN_RETENTION_SECS`]
/// (#4643). Malformed lines are always retained.
///
/// The common case (nothing prunable) costs one unsynchronized read and takes
/// **no lock at all** — this runs on the `tokens select` hot path, where a
/// burst of concurrent spawns must not serialize on the `.bad_tokens` lock.
/// The file is re-read and re-partitioned under the lock before any rewrite,
/// so a line appended between the pre-check and the lock is never lost.
///
/// # Errors
/// Returns an error if the lock cannot be acquired or the file cannot be
/// read/written.
pub fn cleanup_bad_tokens_in_dir(
    dir: &Path,
    max_age_seconds: i64,
) -> Result<CleanupOutcome, String> {
    let bad_file = bad_tokens_path(dir);
    if !bad_file.is_file() {
        return Ok(CleanupOutcome {
            kept: 0,
            removed: 0,
        });
    }

    // Lock-free pre-check: bail out without touching the lock when there is
    // nothing to prune (the steady state on a healthy pool).
    let text = std::fs::read_to_string(&bad_file).map_err(|e| e.to_string())?;
    let (kept, removed) = partition_by_age(&text, max_age_seconds, Utc::now().timestamp());
    if removed == 0 {
        return Ok(CleanupOutcome {
            kept: kept.len(),
            removed: 0,
        });
    }

    let _lock = MkdirLock::acquire(&lock_path(dir))?;
    let text = std::fs::read_to_string(&bad_file).map_err(|e| e.to_string())?;
    let (kept, removed) = partition_by_age(&text, max_age_seconds, Utc::now().timestamp());
    if removed == 0 {
        return Ok(CleanupOutcome {
            kept: kept.len(),
            removed: 0,
        });
    }

    let tmp = bad_file.with_extension("tmp");
    let body = if kept.is_empty() {
        String::new()
    } else {
        format!("{}\n", kept.join("\n"))
    };
    std::fs::write(&tmp, body).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &bad_file).map_err(|e| e.to_string())?;

    Ok(CleanupOutcome {
        kept: kept.len(),
        removed,
    })
}

/// Workspace-anchored [`cleanup_bad_tokens_in_dir`]. Returns the number of
/// entries retained after pruning.
///
/// # Errors
/// Returns an error if the lock cannot be acquired.
pub fn cleanup_bad_tokens(workspace: &Path, max_age_seconds: i64) -> Result<usize, String> {
    cleanup_bad_tokens_in_dir(&tokens_dir(workspace), max_age_seconds).map(|o| o.kept)
}

/// Outcome of [`unblock`]. `excluded` names the accounts that still have a
/// non-auth ("exhausted") entry left in place because the default scope (no
/// `all_reasons`) does not drop them (#4212). First-seen file order,
/// de-duplicated, so the daemon CLI and the Python `cli.py::_cmd_unblock`
/// emit a byte-identical `excluded` list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnblockOutcome {
    pub removed: usize,
    pub kept: usize,
    pub excluded: Vec<String>,
}

/// Remove entries for `names` from `.bad_tokens` (operator `unblock`, ported
/// from `cli.py::_cmd_unblock`). By default only entries whose reason field
/// matches [`auth_reason_regex`] are dropped ("TTL entries clear
/// themselves" — non-auth reasons are left for [`cleanup_bad_tokens`] /
/// natural expiry); `all_reasons` also drops non-auth entries for the given
/// names. Malformed lines (fewer than 2 whitespace-separated fields) are
/// always kept so we never silently lose data.
///
/// Returns an [`UnblockOutcome`]. `excluded` lists the named accounts whose
/// non-auth entries the default scope left in place — the caller surfaces
/// these and exits non-zero (#4212) so a no-op `unblock` can no longer look
/// like success on a still-poisoned pool.
///
/// # Errors
/// Returns an error if the lock cannot be acquired or the file cannot be
/// read/written.
pub fn unblock(
    workspace: &Path,
    names: &[String],
    all_reasons: bool,
) -> Result<UnblockOutcome, String> {
    unblock_in_dir(&tokens_dir(workspace), names, all_reasons)
}

/// [`unblock`], operating directly on an already-resolved tokens directory
/// rather than re-deriving one from a workspace root (issue #6759). Mirrors
/// the `_in_dir` / workspace-anchored split already used by
/// [`blocking_entry_in_dir`] / [`blocking_entry`] and
/// [`cleanup_bad_tokens_in_dir`] / [`cleanup_bad_tokens`] — callers that
/// already hold the resolved directory (e.g. the shared machine-level pool
/// selected via `--shared`) must not pass it back through [`unblock`] as if
/// it were a *workspace*, or [`tokens_dir`] would append `.loom/tokens` a
/// second time.
///
/// # Errors
/// Returns an error if the lock cannot be acquired or the file cannot be
/// read/written.
pub fn unblock_in_dir(
    dir: &Path,
    names: &[String],
    all_reasons: bool,
) -> Result<UnblockOutcome, String> {
    let bad_file = bad_tokens_path(dir);
    if !bad_file.is_file() {
        return Ok(UnblockOutcome {
            removed: 0,
            kept: 0,
            excluded: Vec::new(),
        });
    }

    let target: std::collections::HashSet<&str> = names.iter().map(String::as_str).collect();

    let _lock = MkdirLock::acquire(&lock_path(dir))?;
    let text = std::fs::read_to_string(&bad_file).map_err(|e| e.to_string())?;
    let mut removed = 0usize;
    let mut kept: Vec<String> = Vec::new();
    let mut excluded: Vec<String> = Vec::new();
    let mut excluded_seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for line in text.lines() {
        let stripped = line.trim();
        if stripped.is_empty() {
            continue;
        }
        let mut parts = stripped.splitn(3, ' ');
        let _timestamp = parts.next();
        let entry_name = parts.next();
        let reason = parts.next().unwrap_or("");
        if let Some(name) = entry_name {
            if target.contains(name) {
                if all_reasons || auth_reason_regex().is_match(reason) {
                    removed += 1;
                    continue;
                }
                // A named account with a non-auth entry that the DEFAULT scope
                // leaves behind — record it so the caller can name it and fail.
                if excluded_seen.insert(name.to_string()) {
                    excluded.push(name.to_string());
                }
            }
        }
        kept.push(line.to_string());
    }

    let tmp = bad_file.with_extension("tmp");
    let body = if kept.is_empty() {
        String::new()
    } else {
        format!("{}\n", kept.join("\n"))
    };
    std::fs::write(&tmp, body).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &bad_file).map_err(|e| e.to_string())?;

    Ok(UnblockOutcome {
        removed,
        kept: kept.len(),
        excluded,
    })
}

#[cfg(test)]
#[path = "bad_tokens_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "bad_tokens_model_class_tests.rs"]
mod model_class_tests;
