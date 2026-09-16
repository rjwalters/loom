//! Issue #6969 AC2 — append the expected relaunch path + detached-verifier
//! bound to an auto-update roll's own "drain-and-restart triggered" outcome
//! note, split out of `auto_update.rs` to keep that file under the file-size
//! ratchet (`.loom/docs/file-size-policy.md`).

/// Append the expected relaunch path + detached-verifier bound (Issue #6969
/// AC2) to a roll's outcome note when this tick actually triggered a
/// drain-and-restart — so the auto-update roll's OWN "drain-and-restart
/// triggered" log line states which relaunch mechanism is expected and by
/// when a future gap (like the ~4-minute launchd observation that motivated
/// this) becomes attributable from the log alone, without having to
/// cross-reference the later `run_drain_supervisor` drain-complete line.
///
/// A no-op when `drain_accepted` is `false` (nothing was triggered — the note
/// already says so) or the host has no recognized supervisor (nothing to
/// verify against, mirroring every other best-effort branch in this module).
pub(super) fn with_relaunch_verify_note(note: String, drain_accepted: bool) -> String {
    if !drain_accepted {
        return note;
    }
    let Some(supervisor) = crate::ipc::detect_supervisor() else {
        return note;
    };
    let verify_poll_secs = crate::restart_verify::resolve_configured_poll_secs();
    format!(
        "{note} {}",
        crate::restart_verify::relaunch_verify_note(&supervisor, verify_poll_secs)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    /// AC2: when a roll actually triggered a drain-and-restart on a recognized
    /// supervisor, the note names the expected relaunch mechanism AND the
    /// detached verifier's bound — the two pieces of information an operator
    /// needs to tell "still within the expected window" apart from "this is
    /// the ~4-minute-gap shape the issue observed".
    #[test]
    #[serial(loom_daemon_supervisor)]
    fn test_relaunch_verify_note_appended_when_triggered_on_recognized_supervisor() {
        std::env::set_var("LOOM_DAEMON_SUPERVISOR", "launchd");
        let note = with_relaunch_verify_note(
            "rebuilt + provisioned; drain-and-restart triggered".to_string(),
            true,
        );
        std::env::remove_var("LOOM_DAEMON_SUPERVISOR");
        assert!(note.contains("drain-and-restart triggered"));
        assert!(note.contains("launchd"));
        assert!(note.contains("KeepAlive"));
        assert!(note.contains("verify-only"));
        assert!(note.contains("30s"), "default poll bound must be named: {note}");
    }

    /// AC3-adjacent: a fake supervisor that never confirms a relaunch is the
    /// scenario the module's decision logic must not silently paper over — the
    /// note is still produced (nothing here blocks on the eventual poll), but
    /// it must name the SAME bound `restart_verify::verify_and_heal` itself
    /// polls against, so the two can never disagree about "how long is too
    /// long".
    #[test]
    #[serial(loom_daemon_supervisor)]
    fn test_relaunch_verify_note_bound_matches_restart_verify_default() {
        std::env::set_var("LOOM_DAEMON_SUPERVISOR", "systemd");
        let note = with_relaunch_verify_note(
            "fetched release artifact + provisioned; drain-and-restart triggered".to_string(),
            true,
        );
        std::env::remove_var("LOOM_DAEMON_SUPERVISOR");
        assert!(note.contains(&format!("{}s", crate::restart_verify::DEFAULT_POLL_SECS)));
        assert!(note.contains("Restart=on-success"));
    }

    /// Nothing was triggered (`drain_accepted == false`) ⇒ nothing is
    /// appended, regardless of supervisor — the note already says the drain
    /// was refused/not attempted, and appending a relaunch-verify sentence to
    /// that would be actively misleading.
    #[test]
    #[serial(loom_daemon_supervisor)]
    fn test_relaunch_verify_note_untouched_when_not_triggered() {
        std::env::set_var("LOOM_DAEMON_SUPERVISOR", "launchd");
        let original = "rebuilt + provisioned, but drain-and-restart was refused (no supervisor?) \
                         — restart manually to run the fresh binary"
            .to_string();
        let note = with_relaunch_verify_note(original.clone(), false);
        std::env::remove_var("LOOM_DAEMON_SUPERVISOR");
        assert_eq!(note, original);
    }

    /// An unsupervised host has nothing to verify against — the note is
    /// unchanged even when `drain_accepted` is (degenerately) true.
    #[test]
    #[serial(loom_daemon_supervisor)]
    fn test_relaunch_verify_note_untouched_when_unsupervised() {
        std::env::remove_var("LOOM_DAEMON_SUPERVISOR");
        let original = "rebuilt + provisioned; drain-and-restart triggered".to_string();
        let note = with_relaunch_verify_note(original.clone(), true);
        assert_eq!(note, original);
    }
}
