//! Exhaustion / rate-limit bad-marking for the API-key pool (issue #8401).
//!
//! A minimal, provider-neutral analogue of [`crate::tokens_pool::bad_tokens`]:
//! one JSON file per provider (`<pool>/<provider>/.bad_accounts.json`)
//! holding marks with an optional reset horizon. [`super::classify`]
//! recognises a harness's own quota/rate-limit error text and produces the
//! [`super::classify::Classification`] a caller passes to [`mark_bad`];
//! nothing in this module parses provider HTTP responses itself.
//!
//! # What this slice covers, and what does not follow from it
//!
//! The mechanism end to end — mark an account bad, have it drop out of
//! [`super::select::select_api_key`] until its reset horizon, and become
//! selectable again once that horizon passes — is implemented and tested
//! here with a *simulated* classification (acceptance criterion: "Bad-marking
//! a simulated exhausted account (classifier fixture) removes it from
//! selection until its reset horizon").
//!
//! Automatically calling [`mark_bad`] from a live, failed spawn is **not**
//! wired up in this slice. Native harness spawns run under Unix `exec`
//! (`worker_spawn::exec`) to preserve PID/signal parity for the role runner,
//! so this process's own lifetime ends at the point of exec — there is no
//! in-process hook left to observe the child's stdout/stderr and call
//! [`mark_bad`] automatically. Wiring real captured Z.ai/OpenCode error text
//! into that automatic call is tracked as a follow-up (#8424); until then,
//! `mark-bad` is an operator/tooling-invoked CLI verb, not a self-healing loop.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::tokens_pool::locking::MkdirLock;

use super::paths::{list_account_files, provider_dir, validate_account, validate_provider};
use super::registry::{restrict_dir, write_secret};

/// Per-provider file holding every recorded mark (expired ones included; the
/// selection ladder filters on [`active_mark`] at read time).
pub const BAD_MARKS_FILE: &str = ".bad_accounts.json";

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

/// Record (or replace) a bad mark for `name`.
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
    validate_provider(provider)?;
    validate_account(name)?;
    if cooldown_secs == Some(0) {
        return Err(
            "cooldown_secs must be > 0 (use `api-keys unblock` to clear a mark immediately)"
                .to_string(),
        );
    }
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
    marks.retain(|m| m.name != name);
    let mark = BadMark {
        name: name.to_string(),
        reason: reason.replace(['\n', '\r'], " "),
        marked_at: now,
        resets_at: cooldown_secs.map(|secs| now + secs),
    };
    marks.push(mark.clone());
    marks.sort_by(|a, b| a.name.cmp(&b.name));
    write_marks(root, provider, &marks)?;
    Ok(mark)
}

/// Clear a mark early (an operator override, or a successful re-probe).
///
/// # Errors
/// Returns an error when no mark is currently recorded for `provider/name`.
pub fn unmark(root: &Path, provider: &str, name: &str) -> Result<(), String> {
    validate_provider(provider)?;
    validate_account(name)?;
    let dir = provider_dir(root, provider);
    if !dir.exists() {
        return Err(format!("no bad mark recorded for {provider}/{name}"));
    }
    let _lock = MkdirLock::acquire(&lock_path(root, provider))
        .map_err(|e| format!("cannot lock bad-marks file: {e}"))?;
    let mut marks = read_marks(root, provider)?;
    let before = marks.len();
    marks.retain(|m| m.name != name);
    if marks.len() == before {
        return Err(format!("no bad mark recorded for {provider}/{name}"));
    }
    write_marks(root, provider, &marks)
}

/// [`unmark`] for `registry::remove`: drop `name`'s mark if there is one, and
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

/// The active (not-yet-reset) mark for `name`, if any, at `now`.
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
    Ok(read_marks(root, provider)?
        .into_iter()
        .find(|m| m.name == name && m.resets_at.is_none_or(|resets_at| resets_at > now)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pool root with `names` registered under `zai` (obviously fake keys).
    fn pool(names: &[&str]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        for name in names {
            crate::api_keys_pool::registry::add(
                tmp.path(),
                "zai",
                name,
                "ZAI_API_KEY",
                "fake-key-not-a-real-credential",
                false,
            )
            .unwrap();
        }
        tmp
    }

    fn active(root: &Path, provider: &str, name: &str, now: u64) -> Option<BadMark> {
        active_mark(root, provider, name, now).unwrap()
    }

    #[test]
    fn mark_bad_then_active_mark_reports_it_until_the_horizon() {
        let tmp = pool(&["alpha"]);
        let mark = mark_bad(tmp.path(), "zai", "alpha", "quota exceeded\nretry later", Some(3600))
            .unwrap();
        assert_eq!(mark.reason, "quota exceeded retry later");
        let now = mark.marked_at;
        assert!(active(tmp.path(), "zai", "alpha", now).is_some());
        assert!(active(tmp.path(), "zai", "alpha", now + 3599).is_some());
        assert!(active(tmp.path(), "zai", "alpha", now + 3601).is_none());
    }

    #[test]
    fn a_permanent_mark_never_expires_until_unmark() {
        let tmp = pool(&["alpha"]);
        mark_bad(tmp.path(), "zai", "alpha", "permanent", None).unwrap();
        assert!(active(tmp.path(), "zai", "alpha", epoch_now() + 10_000_000).is_some());
        unmark(tmp.path(), "zai", "alpha").unwrap();
        assert!(active(tmp.path(), "zai", "alpha", epoch_now()).is_none());
    }

    #[test]
    fn zero_cooldown_is_rejected() {
        let tmp = pool(&["alpha"]);
        let err = mark_bad(tmp.path(), "zai", "alpha", "x", Some(0)).unwrap_err();
        assert!(err.contains("unblock"), "{err}");
    }

    #[test]
    fn unmark_on_an_unmarked_account_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let err = unmark(tmp.path(), "zai", "ghost").unwrap_err();
        assert!(err.contains("no bad mark recorded"), "{err}");
    }

    #[test]
    fn re_marking_replaces_the_previous_entry_rather_than_appending() {
        let tmp = pool(&["alpha"]);
        mark_bad(tmp.path(), "zai", "alpha", "first", Some(10)).unwrap();
        mark_bad(tmp.path(), "zai", "alpha", "second", Some(20)).unwrap();
        let marks = read_marks(tmp.path(), "zai").unwrap();
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].reason, "second");
    }

    #[test]
    fn marks_are_scoped_per_provider() {
        let tmp = pool(&["alpha"]);
        mark_bad(tmp.path(), "zai", "alpha", "x", Some(10)).unwrap();
        assert!(active(tmp.path(), "openai", "alpha", epoch_now()).is_none());
    }

    /// Judge nit (#8428): marking a name that is not registered used to
    /// succeed and `mkdir` the provider directory as a side effect.
    #[test]
    fn marking_an_unregistered_account_is_refused_and_creates_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let err = mark_bad(tmp.path(), "zai", "ghost", "x", Some(10)).unwrap_err();
        assert!(err.contains("no such account zai/ghost"), "{err}");
        assert!(!provider_dir(tmp.path(), "zai").exists());
    }

    /// Judge finding 2 (#8428). Each body is a state the Judge reproduced:
    /// zero-length (what a reader saw between `O_TRUNC` and `write_all`, or
    /// what `ENOSPC` leaves) and a torn JSON body. Before the fix both read as
    /// "no marks" (`unwrap_or_default`), and the follow-up `mark_bad` rewrote
    /// the file holding only `beta` — `alpha`'s mark was gone for good.
    #[test]
    fn an_unparsable_marks_file_fails_closed_and_is_never_clobbered() {
        for torn in ["", "[{\"name\":\"alpha\",\"reason\":\"quota", "{}"] {
            let tmp = pool(&["alpha", "beta"]);
            mark_bad(tmp.path(), "zai", "alpha", "secret-ish reason text", Some(3600)).unwrap();
            let path = marks_path(tmp.path(), "zai");
            std::fs::write(&path, torn).unwrap();

            let err = read_marks(tmp.path(), "zai").unwrap_err();
            assert!(err.contains(&path.display().to_string()), "{err}");
            assert!(!err.contains("secret-ish"), "parse error echoed contents: {err}");
            assert!(active_mark(tmp.path(), "zai", "alpha", epoch_now()).is_err());

            // Neither writer may rewrite it from an empty read.
            assert!(mark_bad(tmp.path(), "zai", "beta", "x", Some(10)).is_err());
            assert!(unmark(tmp.path(), "zai", "alpha").is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), torn, "file was clobbered");
        }
    }

    #[test]
    fn an_absent_marks_file_is_simply_no_marks() {
        let tmp = pool(&["alpha"]);
        assert_eq!(read_marks(tmp.path(), "zai").unwrap(), Vec::new());
        assert_eq!(active_mark(tmp.path(), "zai", "alpha", epoch_now()), Ok(None));
    }

    /// The write itself: replaced by `rename`, never truncated in place, and no
    /// staging file is left behind.
    #[test]
    fn marks_are_written_atomically_and_leave_no_temp_file() {
        let tmp = pool(&["alpha", "beta"]);
        mark_bad(tmp.path(), "zai", "alpha", "first", Some(3600)).unwrap();
        let path = marks_path(tmp.path(), "zai");
        #[cfg(unix)]
        let inode_before = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(&path).unwrap().ino()
        };
        mark_bad(tmp.path(), "zai", "beta", "second", Some(3600)).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let metadata = std::fs::metadata(&path).unwrap();
            assert_ne!(metadata.ino(), inode_before, "marks file was rewritten in place");
            assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        }
        assert_eq!(read_marks(tmp.path(), "zai").unwrap().len(), 2);
        let stranded: Vec<_> = std::fs::read_dir(provider_dir(tmp.path(), "zai"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(stranded.is_empty(), "{stranded:?}");
    }
}
