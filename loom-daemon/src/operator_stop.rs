//! The durable **operator-stop record** (Issue #9588).
//!
//! # Why
//!
//! The `autonomy-desired` marker (#4011) is the host's durable "a daemon is
//! EXPECTED here" signal. Only `loom-daemon-stop.sh` removed it, so a daemon
//! that an operator stopped any *other* way — `restart --drain --then-exit`,
//! `fleet drain` — exited with the marker still in place. The host watchdog
//! then saw "EXPECTED but NOT running" and revived it within a minute, and the
//! revived daemon went straight back to dispatching (loom-worker-2,
//! 2026-09-30). Startup marker healing (#4331) would re-arm an absent marker
//! on any supervised relaunch for the same reason.
//!
//! # The record
//!
//! `<marker>.stopped` (a sibling of the resolved marker path, so a
//! `LOOM_AUTONOMY_MARKER` override moves both together). Recording a stop
//! **moves the marker aside into it**: the original marker lines are kept
//! verbatim, prefixed by this module's own `# [operator-stop` comment and
//! `operator_stop_*=` lines. That keeps the two states mutually exclusive on
//! disk: while the stop record exists, no `autonomy-desired` does.
//!
//! Who honours it:
//!
//! - the watchdog tick (`watchdog::tick`): record present ⇒ a deliberate stop,
//!   no recovery, exit 0 — even if a stale `autonomy-desired` also exists;
//! - startup marker healing ([`crate::autonomy_marker::heal_marker`]): never
//!   re-arms the marker while the record exists;
//! - daemon startup ([`crate::ipc::DrainState::with_default_ledger`]): a
//!   supervised relaunch (launchd `RunAtLoad`, a reboot of an enabled systemd
//!   unit, a `kickstart`) comes up with dispatch **held**.
//!
//! Who clears it:
//!
//! - `loom-daemon restart --abort-drain` ([`clear`]): restores the moved-aside
//!   marker and removes the record — the operator changed their mind;
//! - an explicit start (`loom-daemon daemon-start` / `loom-daemon-start.sh`,
//!   [`discard`]): removes the record before launching; the start writes a
//!   fresh marker of its own.

use std::path::{Path, PathBuf};

/// Comment prefix on every line this module adds, so [`clear`] can strip them.
const COMMENT_PREFIX: &str = "# [operator-stop";
/// Key prefix on every field this module adds.
const KEY_PREFIX: &str = "operator_stop_";

/// The record's path for a given resolved `autonomy-desired` marker path.
#[must_use]
pub fn record_path(marker: &Path) -> PathBuf {
    let mut s = marker.as_os_str().to_os_string();
    s.push(".stopped");
    PathBuf::from(s)
}

/// Whether an operator stop is on record for this marker.
#[must_use]
pub fn is_recorded(marker: &Path) -> bool {
    record_path(marker).exists()
}

/// A field of the record (`operator_stop_<key>=`), or `None`.
#[must_use]
pub fn field(marker: &Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(record_path(marker)).ok()?;
    let prefix = format!("{KEY_PREFIX}{key}=");
    text.lines()
        .find(|l| l.starts_with(&prefix))
        .map(|l| l[prefix.len()..].to_string())
}

/// Record an operator stop: write `<marker>.stopped` (the moved-aside marker
/// plus the stop fields), then remove `autonomy-desired`.
///
/// Idempotent: an existing record is left as is (its first `stopped_at` is the
/// one that matters), but a marker that reappeared since is still moved aside.
///
/// # Errors
///
/// A human-readable reason on any I/O failure. Callers log it and carry on — a
/// failed record must never block the drain itself.
pub fn record(marker: &Path, reason: &str) -> Result<(), String> {
    let path = record_path(marker);
    if !path.exists() {
        let original = std::fs::read_to_string(marker).unwrap_or_default();
        let body = format!(
            "{COMMENT_PREFIX} #9588] An operator stopped this daemon; the autonomy-desired\n\
             {COMMENT_PREFIX} #9588] marker was moved aside into this file so the watchdog and\n\
             {COMMENT_PREFIX} #9588] startup healing do not revive it. `loom-daemon restart\n\
             {COMMENT_PREFIX} #9588] --abort-drain` restores it; an explicit start clears it.\n\
             {KEY_PREFIX}at={at}\n\
             {KEY_PREFIX}reason={reason}\n\
             {KEY_PREFIX}pid={pid}\n\
             {original}",
            at = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
            reason = reason.replace('\n', " "),
            pid = std::process::id(),
        );
        write_private_atomic(&path, &body)?;
    }
    match std::fs::remove_file(marker) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("could not move {} aside: {e}", marker.display())),
    }
}

/// What [`clear`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cleared {
    /// No operator stop was on record.
    NotRecorded,
    /// The record was removed and the moved-aside marker restored.
    Restored,
    /// The record was removed; there was no marker to restore (the stopped
    /// daemon was unsupervised, or a marker already exists again).
    Removed,
}

/// Undo an operator stop (`--abort-drain`): restore the moved-aside marker if
/// none exists now, then remove the record.
///
/// # Errors
///
/// A human-readable reason on any I/O failure.
pub fn clear(marker: &Path) -> Result<Cleared, String> {
    let path = record_path(marker);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(Cleared::NotRecorded);
    };
    let original: String = text
        .lines()
        .filter(|l| !l.starts_with(COMMENT_PREFIX) && !l.starts_with(KEY_PREFIX))
        .map(|l| format!("{l}\n"))
        .collect();
    let restore = !marker.exists() && original.lines().any(|l| l.starts_with("started_at="));
    if restore {
        write_private_atomic(marker, &original)?;
    }
    std::fs::remove_file(&path).map_err(|e| format!("could not remove {}: {e}", path.display()))?;
    Ok(if restore {
        Cleared::Restored
    } else {
        Cleared::Removed
    })
}

/// Drop the record without restoring anything — an explicit start is about to
/// write a fresh marker. Returns `true` when a record was removed.
pub fn discard(marker: &Path) -> bool {
    std::fs::remove_file(record_path(marker)).is_ok()
}

/// Owner-only (`0o600`) temp-file + rename, so a concurrent watchdog read never
/// sees a torn file.
fn write_private_atomic(path: &Path, contents: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    }
    let mut tmp_name = path.as_os_str().to_os_string();
    tmp_name.push(format!(".tmp.{}", uuid::Uuid::new_v4()));
    let tmp = PathBuf::from(tmp_name);
    let written = {
        #[cfg(unix)]
        {
            use std::io::Write as _;
            use std::os::unix::fs::OpenOptionsExt as _;
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)
                .and_then(|mut f| f.write_all(contents.as_bytes()))
        }
        #[cfg(not(unix))]
        {
            std::fs::write(&tmp, contents.as_bytes())
        }
    };
    if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("could not write {}: {e}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const MARKER_BODY: &str = "# loom autonomy-desired marker\nstarted_at=2026-09-30T00:00:00Z\n\
                               pid_file=/p\nuse_launchd=false\n";

    #[test]
    fn record_moves_the_marker_aside_and_clear_restores_it_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("autonomy-desired");
        std::fs::write(&marker, MARKER_BODY).unwrap();

        record(&marker, "restart --drain --then-exit").unwrap();
        assert!(!marker.exists(), "the marker is moved aside");
        assert!(is_recorded(&marker));
        assert_eq!(field(&marker, "reason").as_deref(), Some("restart --drain --then-exit"));
        assert_eq!(field(&marker, "pid"), Some(std::process::id().to_string()));

        assert_eq!(clear(&marker).unwrap(), Cleared::Restored);
        assert!(!is_recorded(&marker));
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), MARKER_BODY);
    }

    #[test]
    fn record_is_idempotent_and_still_moves_a_reappeared_marker_aside() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("autonomy-desired");
        std::fs::write(&marker, MARKER_BODY).unwrap();
        record(&marker, "first").unwrap();
        std::fs::write(&marker, "started_at=later\n").unwrap();
        record(&marker, "second").unwrap();
        assert!(!marker.exists());
        assert_eq!(field(&marker, "reason").as_deref(), Some("first"));
    }

    #[test]
    fn a_stop_with_no_marker_clears_without_inventing_one() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("autonomy-desired");
        record(&marker, "unsupervised").unwrap();
        assert!(is_recorded(&marker));
        assert_eq!(clear(&marker).unwrap(), Cleared::Removed);
        assert!(!marker.exists(), "an unsupervised stop must not arm the watchdog");
        assert_eq!(clear(&marker).unwrap(), Cleared::NotRecorded);
    }

    #[test]
    fn clear_never_overwrites_a_marker_written_since() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("autonomy-desired");
        std::fs::write(&marker, MARKER_BODY).unwrap();
        record(&marker, "stop").unwrap();
        std::fs::write(&marker, "started_at=fresh\n").unwrap();
        assert_eq!(clear(&marker).unwrap(), Cleared::Removed);
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "started_at=fresh\n");
    }

    #[test]
    fn discard_drops_the_record_only() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("autonomy-desired");
        std::fs::write(&marker, MARKER_BODY).unwrap();
        record(&marker, "stop").unwrap();
        assert!(discard(&marker));
        assert!(!is_recorded(&marker));
        assert!(!marker.exists(), "discard does not restore — the start writes its own");
        assert!(!discard(&marker));
    }

    #[cfg(unix)]
    #[test]
    fn the_record_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("autonomy-desired");
        record(&marker, "stop").unwrap();
        let mode = std::fs::metadata(record_path(&marker))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
