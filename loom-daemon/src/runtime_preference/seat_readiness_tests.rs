//! #14455: the ordered resolver draws a Codex pool only through seats that can
//! run a command — a bare-metal profile on a host of session containers is
//! passed over before the claim, and a seat whose sandbox ran nothing is held
//! on its own, leaving the rest of the pool drawable.

use super::{fixture, provision_claude_pool, write_config, ClearedRuntimeEnv};
use crate::runtime_preference::{resolve_runtime, Decision};
use crate::tokens_pool::account_registry::{AccountId, AccountProvider};
use crate::tokens_pool::sandbox_seat_hold::hold_seat;
use std::fs;
use std::path::Path;

/// A Codex account `name` under the guard's isolated profile root:
/// session-managed (adopted by `accounts session start`) or bare-metal.
fn provision_codex_account(name: &str, session_managed: bool) {
    let root = std::env::var_os("LOOM_CODEX_PROFILE_ROOT").expect("ClearedRuntimeEnv sets it");
    let dir = Path::new(&root).join(name);
    fs::create_dir_all(&dir).unwrap();
    if session_managed {
        fs::write(dir.join(crate::tokens_pool::session_lifecycle::SESSION_MARKER_FILE), "{}")
            .unwrap();
    }
}

fn codex_id(name: &str) -> AccountId {
    AccountId {
        provider: AccountProvider::Codex,
        name: name.to_string(),
    }
}

fn now() -> u64 {
    u64::try_from(chrono::Utc::now().timestamp()).unwrap()
}

/// The curator walk for `[codex, claude]`: the chosen runtime and the skip
/// summaries recorded on the way. `now` is 0 so a seat cooldown always reads
/// as live.
fn curator_walk(root: &Path) -> (String, Vec<String>) {
    let Decision::Preference { resolution, .. } =
        resolve_runtime(root, "curator", None, 0).unwrap()
    else {
        panic!("expected the preference path");
    };
    let chosen = resolution.chosen.as_ref().unwrap().admitted.runtime.clone();
    let skipped = resolution
        .skipped
        .iter()
        .map(|s| s.reason.summary())
        .collect();
    (chosen, skipped)
}

fn workspace() -> tempfile::TempDir {
    let dir = fixture();
    provision_claude_pool(dir.path(), 2);
    write_config(
        dir.path(),
        &serde_json::json!({"runtimes": {"preference": ["codex", "claude"]}}),
    );
    dir
}

/// The report: seats 1-2 run in containers, seat 3 is a bare-metal profile
/// that was never provisioned. The unprovisioned seat is never what makes the
/// pool spawnable, so a pool whose only live seat is down falls through, and
/// naming the seat the way the write-role `not-admitted(...)` line does.
#[test]
#[serial_test::serial]
fn an_unprovisioned_bare_seat_beside_container_seats_is_not_admitted() {
    use crate::tokens_pool::session_lifecycle::liveness::test_support::{running, set};
    let _env = ClearedRuntimeEnv::new();
    let dir = workspace();
    provision_codex_account("agent-1", true);
    provision_codex_account("agent-2", true);
    provision_codex_account("agent-3", false);
    let codex = ("codex".to_string(), vec![]);

    {
        let _up = set(running(&["agent-1"]));
        assert_eq!(curator_walk(dir.path()), codex, "one running seat keeps codex drawable");
    }
    {
        let _down = set(running(&[]));
        let (chosen, skipped) = curator_walk(dir.path());
        assert_eq!(chosen, "claude", "the bare seat must not keep codex alive");
        assert!(
            skipped[0].contains("1/3 not-admitted(sessionSeat") && skipped[0].contains("#14455"),
            "{skipped:?}"
        );
        assert!(skipped[0].contains("SessionDown"), "{skipped:?}");
    }
    {
        let _unobservable = set(None);
        assert_eq!(curator_walk(dir.path()), codex, "cannot observe: the live seats stay");
    }
}

/// A host with no container seat at all is a bare-metal host: nothing is
/// passed over, so a plain `CODEX_HOME` setup is unaffected.
#[test]
#[serial_test::serial]
fn a_host_with_only_bare_metal_seats_is_not_gated() {
    use crate::tokens_pool::session_lifecycle::liveness::test_support::{running, set};
    let _env = ClearedRuntimeEnv::new();
    let dir = workspace();
    provision_codex_account("bare-1", false);
    provision_codex_account("bare-2", false);
    let _down = set(running(&[]));
    assert_eq!(curator_walk(dir.path()), ("codex".to_string(), vec![]));
}

/// A seat held for its own `SANDBOX_UNAVAILABLE` leaves the other seat
/// drawable; only when every seat is held does the walk fall through, and the
/// skip names the hold. The hold is per seat, not the whole runtime.
#[test]
#[serial_test::serial]
fn a_sandbox_held_seat_is_passed_over_on_its_own() {
    use crate::tokens_pool::session_lifecycle::liveness::test_support::{running, set};
    let _env = ClearedRuntimeEnv::new();
    let dir = workspace();
    provision_codex_account("agent-1", true);
    provision_codex_account("agent-2", true);
    let _up = set(running(&["agent-1", "agent-2"]));
    let codex = ("codex".to_string(), vec![]);
    assert_eq!(curator_walk(dir.path()), codex);

    assert!(hold_seat(dir.path(), &codex_id("agent-1"), 600, now()).unwrap());
    assert_eq!(curator_walk(dir.path()), codex, "agent-2 is still drawable");

    assert!(hold_seat(dir.path(), &codex_id("agent-2"), 600, now()).unwrap());
    let (chosen, skipped) = curator_walk(dir.path());
    assert_eq!(chosen, "claude");
    assert!(skipped[0].contains("2/2 held(sandbox-unavailable"), "{skipped:?}");
}
