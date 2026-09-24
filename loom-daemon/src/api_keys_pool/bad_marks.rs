//! Exhaustion / rate-limit bad-marking for the API-key pool (issues #8401,
//! #8424).
//!
//! A provider-neutral analogue of [`crate::tokens_pool::bad_tokens`]: one JSON
//! file per provider (`<pool>/<provider>/.bad_accounts.json`) holding marks
//! with an optional reset horizon. [`super::classify`] recognises a harness's
//! own quota/rate-limit error text and produces the
//! [`super::classify::Classification`] a caller passes to [`mark_bad`];
//! nothing in this module parses provider HTTP responses itself.
//!
//! # Model-class scoping (#8424 item 3, reusing #8058's rule)
//!
//! A mark may name a **model class** ([`BadMark::model_class`]). The rule is
//! [`crate::tokens_pool::bad_tokens`]'s, verbatim, because the whole point is
//! that the two pools behave the same way:
//!
//! * A **class-less** mark blocks *every* class. Every mark written before
//!   #8424, and every mark an operator records without `--model-class`, is
//!   class-less, so nothing here ever narrows an existing mark.
//! * A **class-scoped** mark blocks only the class it names — and only for a
//!   caller that asked about a class ([`active_mark_for_class`] /
//!   [`is_bad_for_class`]). [`active_mark`]'s class-less question keeps its
//!   account-wide meaning: "is this account marked at all".
//! * Each class-scoped mark is its own entry with its own horizon, so it ages
//!   out independently of any account-wide mark on the same account.
//!
//! What counts as a class here is *not*
//! [`crate::tokens_pool::bad_tokens::model_class_of`]'s Claude taxonomy
//! (`haiku`/`sonnet`/`opus`/`fable`) — this pool is provider-neutral and no
//! cross-provider tier taxonomy exists. The class is the **model id** the
//! spawn asked for, normalized by [`normalize_model_class`]
//! (`glm-5.3-flash#high` -> `glm-5.3-flash`), which is exactly what #8424
//! asks for: an exhausted `glm-5.3-flash` allowance must not bad-mark the same
//! account's `glm-5` allowance. A provider whose plan pools several model ids
//! under one allowance should record the class-less (account-wide) mark
//! instead — that is what class-less *means*.
//!
//! # Automatic marking is now wired, post-hoc
//!
//! #8401 left [`mark_bad`] as an operator/tooling-invoked verb because native
//! harness spawns run under Unix `exec` (`worker_spawn::exec`), so the
//! selecting process becomes the harness and has no in-process hook left to
//! watch the child's output. #8424 resolves that by **not** trying to keep a
//! supervisor alive across the exec: [`super::ingest`] classifies the launch's
//! own retained log after the fact and calls in here. See that module for the
//! design decision and its live call sites.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::tokens_pool::locking::MkdirLock;

use super::paths::{list_account_files, provider_dir, validate_account, validate_provider};
use super::registry::{restrict_dir, write_secret};

/// Per-provider file holding every recorded mark (expired ones included; the
/// selection ladder filters on [`active_mark`] at read time).
pub const BAD_MARKS_FILE: &str = ".bad_accounts.json";

/// Longest accepted model class. Generous — provider model ids are long — but
/// bounded, so a pasted error body can never become a class.
const MAX_MODEL_CLASS_LEN: usize = 64;

/// One exhaustion/rate-limit record against a single account.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BadMark {
    pub name: String,
    /// Newline-sanitized (embedded `\n`/`\r` collapsed to spaces), the same
    /// convention `tokens_pool::bad_tokens::mark_bad` uses, so one entry is
    /// always exactly one JSON string with no embedded record separators.
    pub reason: String,
    pub marked_at: u64,
    /// Unix seconds after which the account is selectable again. `None`
    /// means "until an explicit [`unmark`]".
    pub resets_at: Option<u64>,
    /// The model class this mark is scoped to (#8424 item 3). `None` — the
    /// pre-#8424 shape, and what `serde` fills in for every mark written
    /// before this field existed — blocks the account for **every** class.
    ///
    /// Skipped when absent, so a class-less mark serialises byte-identically
    /// to the pre-#8424 file format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_class: Option<String>,
}

impl BadMark {
    /// Whether this mark blocks a caller asking about `model_class`.
    ///
    /// `None` for `model_class` is the account-wide question and is blocked by
    /// *any* mark — see the module docs for why that is not narrowed.
    #[must_use]
    pub fn blocks_class(&self, model_class: Option<&str>) -> bool {
        match (self.model_class.as_deref(), model_class) {
            // A class-less mark is account-wide: it blocks every class.
            (None, _) => true,
            // The class-less (account-wide) question: any mark answers yes.
            (Some(_), None) => true,
            (Some(marked), Some(asked)) => marked.eq_ignore_ascii_case(asked),
        }
    }

    /// `true` when this mark has not yet reached its reset horizon at `now`.
    #[must_use]
    pub fn is_active_at(&self, now: u64) -> bool {
        self.resets_at.is_none_or(|resets_at| resets_at > now)
    }

    /// Operator-facing suffix naming the mark's class, or `""` when it is
    /// account-wide. Used by `list`/`health` detail strings.
    #[must_use]
    pub fn class_suffix(&self) -> String {
        self.model_class
            .as_deref()
            .map_or_else(String::new, |class| format!(" [model-class:{class}]"))
    }
}

/// Normalize a model id into the class a mark is scoped to, or `None` when
/// there is nothing usable to scope by (#8424 item 3).
///
/// Lowercases, trims, and drops an `#effort` suffix (`glm-5.3-flash#high` and
/// `glm-5.3-flash` are one allowance). Rejects anything that is not a plain
/// model-id shape — an empty string, something over [`MAX_MODEL_CLASS_LEN`],
/// or a value with whitespace/quotes/control characters — because `None`
/// degrades safely to today's account-wide behaviour, whereas accepting junk
/// would silently split one allowance into two unreachable halves.
/// **Unrecognised is `None`, never an error**, the same contract
/// [`crate::tokens_pool::bad_tokens::model_class_of`] states.
#[must_use]
pub fn normalize_model_class(raw: &str) -> Option<String> {
    let base = raw
        .split('#')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if base.is_empty() || base.len() > MAX_MODEL_CLASS_LEN {
        return None;
    }
    let ok = base.bytes().all(|b| {
        b.is_ascii_lowercase()
            || b.is_ascii_digit()
            || matches!(b, b'-' | b'_' | b'.' | b':' | b'/')
    });
    ok.then_some(base)
}

#[must_use]
pub fn epoch_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn marks_path(root: &Path, provider: &str) -> PathBuf {
    provider_dir(root, provider).join(BAD_MARKS_FILE)
}

fn lock_path(root: &Path, provider: &str) -> PathBuf {
    provider_dir(root, provider).join(".bad_accounts.lock")
}

/// Every recorded mark for `provider`, expired or not. Callers that need only
/// the currently-active one should use [`active_mark`].
///
/// # Errors
/// *Absent* and *unusable* are different answers. No file means no marks. A
/// file that exists but cannot be read, is empty, or does not parse is an
/// error — it must never read as "no marks", because that silently returns an
/// exhausted account to selection, and a writer that started from that empty
/// read would then erase the real marks for good. Writes are atomic
/// ([`write_secret`]), so this module never produces such a file itself; one
/// on disk means a crash predating that, a full disk, or a hand edit.
///
/// The message names the path and the *shape* of the failure only — `reason`
/// is free text, and a parse error's own rendering can quote file contents.
pub fn read_marks(root: &Path, provider: &str) -> Result<Vec<BadMark>, String> {
    let path = marks_path(root, provider);
    let body = match std::fs::read_to_string(&path) {
        Ok(body) => body,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("cannot read {} ({:?})", path.display(), e.kind())),
    };
    serde_json::from_str(&body).map_err(|e| {
        format!(
            "{} is not a valid bad-marks file ({:?} error at line {}, column {}); exhaustion \
             state for provider {provider:?} is unknown. Repair the JSON, or delete the file \
             to discard every mark for this provider",
            path.display(),
            e.classify(),
            e.line(),
            e.column()
        )
    })
}

fn write_marks(root: &Path, provider: &str, marks: &[BadMark]) -> Result<(), String> {
    let dir = provider_dir(root, provider);
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    restrict_dir(&dir);
    let path = marks_path(root, provider);
    let body = serde_json::to_string_pretty(marks).map_err(|e| e.to_string())?;
    // `write_secret`, not `fs::write`: this file holds no key material by
    // construction, but `reason` is free text an operator may well paste a
    // provider error body into, and `health`/`list` only warn about loose
    // permissions on account files. Writing it `0600` keeps the whole provider
    // directory uniform rather than leaving one file at the ambient umask.
    write_secret(&path, &body)
}

/// Record (or replace) an account-wide bad mark for `name`.
///
/// `cooldown_secs = None` marks the account until an explicit [`unmark`];
/// `Some(0)` is rejected (use [`unmark`] to clear immediately instead of a
/// mark that expires the instant it is written).
pub fn mark_bad(
    root: &Path,
    provider: &str,
    name: &str,
    reason: &str,
    cooldown_secs: Option<u64>,
) -> Result<BadMark, String> {
    mark_bad_for_class(root, provider, name, reason, cooldown_secs, None)
}

/// [`mark_bad`], scoped to one model class (#8424 item 3).
///
/// `model_class = None` is exactly [`mark_bad`] — an account-wide mark. A
/// class-scoped mark replaces only the previous mark for *that* class, so an
/// account may hold one account-wide mark plus one per class, each with its
/// own horizon.
///
/// The class is normalized through [`normalize_model_class`]; a value that
/// normalizes to `None` is **rejected** rather than silently widened to an
/// account-wide mark, because quietly marking a whole account when the caller
/// asked to scope is the failure this pair exists to prevent.
pub fn mark_bad_for_class(
    root: &Path,
    provider: &str,
    name: &str,
    reason: &str,
    cooldown_secs: Option<u64>,
    model_class: Option<&str>,
) -> Result<BadMark, String> {
    validate_provider(provider)?;
    validate_account(name)?;
    if cooldown_secs == Some(0) {
        return Err(
            "cooldown_secs must be > 0 (use `api-keys unblock` to clear a mark immediately)"
                .to_string(),
        );
    }
    let model_class = match model_class {
        None => None,
        Some(raw) => Some(normalize_model_class(raw).ok_or_else(|| {
            "model class must be a model id such as glm-5.3-flash (lowercase, no spaces); omit \
             it for an account-wide mark"
                .to_string()
        })?),
    };
    // Only a registered account can be marked. Marking a name that does not
    // exist used to `mkdir` the provider directory as a side effect, leaving a
    // phantom `0/0` provider in `health` — and a typo'd name silently marked
    // nothing the operator meant. This also guarantees the lock's parent
    // directory exists (the lock is itself a `mkdir`).
    let dir = provider_dir(root, provider);
    let registered = list_account_files(&dir).map_err(|e| e.to_string())?;
    if !registered
        .iter()
        .any(|p| p.file_stem().and_then(|s| s.to_str()) == Some(name))
    {
        return Err(format!("no such account {provider}/{name}"));
    }
    let _lock = MkdirLock::acquire(&lock_path(root, provider))
        .map_err(|e| format!("cannot lock bad-marks file: {e}"))?;
    let now = epoch_now();
    // `?`: refuse to rewrite an unusable file from an empty read, which would
    // permanently drop every other account's mark.
    let mut marks = read_marks(root, provider)?;
    // Replace only the same (account, class) pair: a class-scoped mark must
    // not clear the account-wide one, nor another class's.
    marks.retain(|m| !(m.name == name && m.model_class == model_class));
    let mark = BadMark {
        name: name.to_string(),
        reason: reason.replace(['\n', '\r'], " "),
        marked_at: now,
        resets_at: cooldown_secs.map(|secs| now + secs),
        model_class,
    };
    marks.push(mark.clone());
    marks.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.model_class.cmp(&b.model_class))
    });
    write_marks(root, provider, &marks)?;
    Ok(mark)
}

/// Clear every mark for `provider/name` (an operator override, or a successful
/// re-probe) — account-wide and class-scoped alike.
///
/// # Errors
/// Returns an error when no mark is currently recorded for `provider/name`.
pub fn unmark(root: &Path, provider: &str, name: &str) -> Result<(), String> {
    unmark_for_class(root, provider, name, None)
}

/// [`unmark`], optionally narrowed to one model class (#8424 item 3).
///
/// `model_class = None` clears **every** mark for the account (the widest,
/// safest operator action — an account can never be left stuck by a class the
/// operator forgot about). `Some(class)` clears only that class's mark and
/// leaves the account-wide one, and any other class's, in place.
///
/// # Errors
/// Returns an error when the narrowing found nothing to clear.
pub fn unmark_for_class(
    root: &Path,
    provider: &str,
    name: &str,
    model_class: Option<&str>,
) -> Result<(), String> {
    validate_provider(provider)?;
    validate_account(name)?;
    let model_class = match model_class {
        None => None,
        Some(raw) => Some(
            normalize_model_class(raw)
                .ok_or_else(|| format!("{raw:?} is not a usable model class"))?,
        ),
    };
    let dir = provider_dir(root, provider);
    if !dir.exists() {
        return Err(no_such_mark(provider, name, model_class.as_deref()));
    }
    let _lock = MkdirLock::acquire(&lock_path(root, provider))
        .map_err(|e| format!("cannot lock bad-marks file: {e}"))?;
    let mut marks = read_marks(root, provider)?;
    let before = marks.len();
    marks.retain(|m| {
        m.name != name
            || model_class
                .as_deref()
                .is_some_and(|c| m.model_class.as_deref() != Some(c))
    });
    if marks.len() == before {
        return Err(no_such_mark(provider, name, model_class.as_deref()));
    }
    write_marks(root, provider, &marks)
}

fn no_such_mark(provider: &str, name: &str, model_class: Option<&str>) -> String {
    match model_class {
        Some(class) => {
            format!("no bad mark recorded for {provider}/{name} on model class {class:?}")
        }
        None => format!("no bad mark recorded for {provider}/{name}"),
    }
}

/// [`unmark`] for `registry::remove`: drop `name`'s marks if there are any, and
/// treat "nothing recorded" as success.
pub(super) fn forget(root: &Path, provider: &str, name: &str) -> Result<(), String> {
    if !marks_path(root, provider).exists() {
        return Ok(());
    }
    let _lock = MkdirLock::acquire(&lock_path(root, provider))
        .map_err(|e| format!("cannot lock bad-marks file: {e}"))?;
    let mut marks = read_marks(root, provider)?;
    let before = marks.len();
    marks.retain(|m| m.name != name);
    if marks.len() == before {
        return Ok(());
    }
    write_marks(root, provider, &marks)
}

/// The active (not-yet-reset) mark for `name`, if any, at `now` — the
/// account-wide question: *any* mark answers it.
///
/// # Errors
/// Propagates [`read_marks`]'s "exists but unusable" error; the caller must
/// treat the account as ineligible rather than as unmarked.
pub fn active_mark(
    root: &Path,
    provider: &str,
    name: &str,
    now: u64,
) -> Result<Option<BadMark>, String> {
    active_mark_for_class(root, provider, name, None, now)
}

/// [`active_mark`], narrowed to the marks that block `model_class` (#8424
/// item 3). `None` for `model_class` is exactly [`active_mark`].
///
/// The asked-about class is normalized through [`normalize_model_class`], so a
/// caller may pass the raw model it resolved (`glm-5.3-flash#high`) without
/// pre-processing. A value that does not normalize degrades to the
/// account-wide question — the *stricter* of the two readings, so an
/// unrecognisable model can only ever withhold an account, never free one.
///
/// When both an account-wide and a matching class-scoped mark are active, the
/// account-wide one is returned — it is the stronger statement, and the one an
/// operator needs to clear.
///
/// # Errors
/// As [`active_mark`]: an unreadable/unparsable marks file is an error, never
/// "not marked".
pub fn active_mark_for_class(
    root: &Path,
    provider: &str,
    name: &str,
    model_class: Option<&str>,
    now: u64,
) -> Result<Option<BadMark>, String> {
    let asked = model_class.and_then(normalize_model_class);
    let mut blocking: Vec<BadMark> = read_marks(root, provider)?
        .into_iter()
        .filter(|m| m.name == name && m.is_active_at(now) && m.blocks_class(asked.as_deref()))
        .collect();
    // Account-wide (class-less) first, then by class name for determinism.
    blocking.sort_by(|a, b| a.model_class.cmp(&b.model_class));
    Ok(blocking.into_iter().next())
}

/// `true` when `name` is currently bad-marked **for `model_class`** (#8424
/// item 3) — the boolean projection of [`active_mark_for_class`], mirroring
/// [`crate::tokens_pool::bad_tokens::is_bad_for_class`].
///
/// # Errors
/// Unlike the Claude pool's `is_bad_for_class` (which returns a bare `bool`
/// and degrades to "not bad" on an unreadable store), this pool **fails
/// closed** on an unusable marks file — the whole #8401 design rests on
/// "cannot read the state" never reading as "nothing is marked". Callers must
/// treat an error as ineligible.
pub fn is_bad_for_class(
    root: &Path,
    provider: &str,
    name: &str,
    model_class: Option<&str>,
    now: u64,
) -> Result<bool, String> {
    Ok(active_mark_for_class(root, provider, name, model_class, now)?.is_some())
}

#[cfg(test)]
#[path = "bad_marks_tests.rs"]
mod tests;
