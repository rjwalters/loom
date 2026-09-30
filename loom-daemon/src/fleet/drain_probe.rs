//! Parsing a remote `loom-daemon status --json` for `fleet drain`'s
//! `wait-remote-exit` phase (#4343), moved out of `drain.rs` (frozen by
//! `.loom/docs/file-size-policy.md`) when #9588 added the timed-out-hold
//! verdict.

use super::super::CommandOutput;

/// Whether a remote `status --json` payload (given as raw stdout) reports the
/// daemon still draining — used by `wait-remote-exit` to distinguish "still
/// waiting" from "drain was refused and dispatch resumed". Thin string-level
/// wrapper over [`still_draining`]; the polling path parses once and calls
/// [`classify_remote_exit`] instead.
#[cfg(test)]
#[must_use]
pub(super) fn parse_still_draining(stdout: &str) -> Option<bool> {
    let value = serde_json::from_str::<serde_json::Value>(stdout).ok()?;
    still_draining(&value)
}

/// Read the drain flag out of an already-parsed `status --json` payload.
///
/// The real payload **nests** this under `drain` — `build_status_json_value`
/// in `main.rs` emits `"drain": { "draining": …, "deadline": …, "note": … }`
/// (#4090) — so a top-level-only read always returns `None` against a live
/// daemon and silently disables the refusal branch in `wait_remote_exit`.
/// The top-level fallback keeps a hypothetical flatter/older payload legible
/// rather than making the parse brittle.
#[must_use]
fn still_draining(value: &serde_json::Value) -> Option<bool> {
    value
        .get("drain")
        .and_then(|drain| drain.get("draining"))
        .or_else(|| value.get("draining"))
        .and_then(serde_json::Value::as_bool)
}

/// One `wait-remote-exit` poll's verdict about the remote daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RemoteExitProbe {
    /// The remote daemon is gone — the expected outcome of a `then_exit`
    /// drain, whether the *host* is still up (the normal case) or also gone.
    Exited,
    /// Still draining, or the payload is not (yet) legible — keep polling.
    StillGoing,
    /// The daemon is reachable and reports `draining: false` — the drain was
    /// refused/aborted and dispatch resumed. Fail loudly.
    Refused,
    /// The daemon is reachable, still draining, and reports the drain
    /// **timed out and held** (`drain.roll.timed_out: true`, #9588): dispatch
    /// stays paused on the host and it will stop once its stragglers finish,
    /// but this orchestrator's documented contract for a non-forced timeout
    /// is exit 2 — report it now rather than wait out its own deadline.
    TimedOutHeld,
}

/// Classify one remote `loom-daemon status --json` invocation.
///
/// Three shapes matter, and only the first used to be handled:
///
/// 1. **Empty stdout + non-zero exit** — the SSH transport itself failed
///    (connection refused, exit 255): host gone ⇒ `Exited`.
/// 2. **A payload with a top-level `error` key** — the #4069
///    unreachable-daemon payload (`print_status_unreachable_json` in
///    `main.rs`) `println!`s `{"error": "could not reach loom-daemon at …",
///    "install_state": …}` to **stdout** and exits non-zero
///    (`install_state.exit_code()`). This is the normal post-`then_exit`
///    state (host up, daemon down) and is therefore the primary success
///    signal, not a parse miss. Mirrors
///    [`super::status::classify_status_output`]'s `DaemonDown` arm.
/// 3. **A live status payload** — inspect `drain.draining` to tell "still
///    draining" from "refused and dispatching again".
#[must_use]
pub(super) fn classify_remote_exit(out: &CommandOutput) -> RemoteExitProbe {
    if out.stdout.trim().is_empty() {
        return if out.ok() {
            // Reachable but silent — nothing to conclude yet.
            RemoteExitProbe::StillGoing
        } else {
            RemoteExitProbe::Exited
        };
    }
    match serde_json::from_str::<serde_json::Value>(&out.stdout) {
        Ok(value) if value.get("error").is_some() => RemoteExitProbe::Exited,
        Ok(value) if timed_out_held(&value) => RemoteExitProbe::TimedOutHeld,
        Ok(value) => match still_draining(&value) {
            Some(false) => RemoteExitProbe::Refused,
            // `Some(true)` (still draining) or `None` (a payload without the
            // field at all) ⇒ keep polling.
            _ => RemoteExitProbe::StillGoing,
        },
        Err(_) => RemoteExitProbe::StillGoing,
    }
}

/// #9588: the remote operator drain passed its deadline without force and is
/// holding dispatch paused (`drain.roll.timed_out: true` while still
/// `draining`). A pre-#9588 daemon never emits the field ⇒ `false`.
#[must_use]
fn timed_out_held(value: &serde_json::Value) -> bool {
    still_draining(value) == Some(true)
        && value
            .get("drain")
            .and_then(|d| d.get("roll"))
            .and_then(|r| r.get("timed_out"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live(stdout: &str) -> CommandOutput {
        CommandOutput {
            code: 0,
            stdout: stdout.to_string(),
            stderr: String::new(),
        }
    }

    #[test]
    fn a_timed_out_held_remote_drain_is_its_own_verdict() {
        let held = r#"{"drain": {"draining": true, "roll": {"timed_out": true}}}"#;
        assert_eq!(classify_remote_exit(&live(held)), RemoteExitProbe::TimedOutHeld);
        let waiting = r#"{"drain": {"draining": true, "roll": {"timed_out": false}}}"#;
        assert_eq!(classify_remote_exit(&live(waiting)), RemoteExitProbe::StillGoing);
        // A pre-#9588 payload (no field) keeps polling exactly as before.
        let old = r#"{"drain": {"draining": true, "roll": null}}"#;
        assert_eq!(classify_remote_exit(&live(old)), RemoteExitProbe::StillGoing);
    }
}
