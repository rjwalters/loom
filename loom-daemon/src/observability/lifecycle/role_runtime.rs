//! Where a role tick ran, on its `loom.role_attempt` span (#12342): the
//! runtime, the engine, and — when the session ran in a container — the
//! container name and image digest.
//!
//! "Which host and runtime ran this Judge?" must be answerable from the span
//! alone, after the fact. `host.name` is already a Resource attribute and
//! `loom.daemon.version` / `.revision` are stamped on every span by
//! [`crate::telemetry::trace::provenance::stamp`]; this adds the rest. The
//! launch site notes the facts here, on the same thread-local channel
//! [`super::role_draw`] uses; [`super::open_role_attempt`] reads them back, so
//! they ride the span at open and survive a tick that is killed or times out.
//!
//! Unknown is absent, never zero: a host-process run carries no container
//! keys, and a runtime that is not a known engine carries no engine.

use std::cell::RefCell;
use std::path::Path;

use crate::telemetry::trace::TraceAttributes;
use crate::tokens_pool::session_lifecycle::{container_name, is_session_managed};
use crate::tokens_pool::session_state::{self, Snapshot, LATEST_MAX_AGE};

/// `loom.runtime`: the runtime the tick was admitted onto (the existing key).
pub const RUNTIME: &str = "loom.runtime";
/// `loom.engine`: the agent engine, `codex` or `claude`.
pub const ENGINE: &str = "loom.engine";
/// `loom.session.container`: the session container the tick ran in.
pub const CONTAINER: &str = "loom.session.container";
/// `loom.session.image_digest`: the container's image (`sha256:…`), as the
/// session reconciler last observed it.
pub const IMAGE_DIGEST: &str = "loom.session.image_digest";
/// The exported span attributes this module adds beyond `loom.runtime`. Each
/// must survive the daemon allowlist and the gateway's span `keep_keys`
/// (contract-tested).
pub const KEYS: [&str; 3] = [ENGINE, CONTAINER, IMAGE_DIGEST];

/// The profile and account env `selection.apply` stamped on a launch.
const CODEX_HOME_ENV: &str = "LOOM_CODEX_HOME";
const ACCOUNT_ENV: &str = "LOOM_ACCOUNT_NAME";
/// `spawn-codex.sh`'s own opt-out: `0` forces a bare-metal run.
const SESSION_EXEC_ENV: &str = "LOOM_CODEX_SESSION_EXEC";

/// One launch's execution context.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Launch {
    pub runtime: Option<String>,
    pub container: Option<String>,
    pub image_digest: Option<String>,
}

thread_local! {
    static NOTED: RefCell<Option<Launch>> = const { RefCell::new(None) };
}

/// The agent engine a runtime name denotes, when it is one Loom knows.
#[must_use]
pub fn engine_of(runtime: &str) -> Option<&'static str> {
    match runtime {
        "codex" => Some("codex"),
        "claude" => Some("claude"),
        _ => None,
    }
}

/// The span attributes for one launch.
#[must_use]
pub fn attributes(launch: &Launch) -> TraceAttributes {
    let mut attrs = TraceAttributes::new();
    if let Some(runtime) = launch.runtime.as_deref() {
        super::insert_nonempty_bounded(&mut attrs, RUNTIME, runtime);
        if let Some(engine) = engine_of(runtime) {
            attrs.insert(ENGINE.to_owned(), engine.to_owned());
        }
    }
    if let Some(container) = launch.container.as_deref() {
        super::insert_nonempty_bounded(&mut attrs, CONTAINER, container);
        if let Some(digest) = launch.image_digest.as_deref() {
            super::insert_nonempty_bounded(&mut attrs, IMAGE_DIGEST, digest);
        }
    }
    attrs
}

/// The container `account` dispatches into, when its profile is
/// session-managed and the operator has not forced bare metal — the same
/// facts `spawn-codex.sh` decides on.
#[must_use]
pub fn session_container(
    runtime: Option<&str>,
    profile: Option<&Path>,
    account: Option<&str>,
) -> Option<String> {
    if runtime != Some("codex") {
        return None;
    }
    if std::env::var(SESSION_EXEC_ENV).is_ok_and(|v| v == "0") {
        return None;
    }
    let (profile, account) = (profile?, account.filter(|a| !a.is_empty())?);
    is_session_managed(profile).then(|| container_name(account))
}

/// The image the reconciler last observed `container` running, from its
/// published snapshot. No Docker call; `None` when there is no fresh snapshot.
#[must_use]
pub fn observed_image(container: &str) -> Option<String> {
    let snapshot = session_state::latest(LATEST_MAX_AGE)?;
    let Snapshot::Available(map) = snapshot.as_ref() else {
        return None;
    };
    let image = map.get(container)?.inspect.get("Image")?.as_str()?;
    Some(image.to_owned())
}

/// Note the launch about to open this thread's `loom.role_attempt` span, from
/// the env the launch's `Command` already carries.
pub fn note_launch(command: &std::process::Command, runtime: Option<&str>) {
    let env = |name: &str| {
        command
            .get_envs()
            .find(|(key, _)| *key == name)
            .and_then(|(_, value)| value)
            .map(std::ffi::OsStr::to_os_string)
    };
    let profile = env(CODEX_HOME_ENV).map(std::path::PathBuf::from);
    let account = env(ACCOUNT_ENV).and_then(|a| a.into_string().ok());
    let container = session_container(runtime, profile.as_deref(), account.as_deref());
    let image_digest = container.as_deref().and_then(observed_image);
    NOTED.with(|slot| {
        *slot.borrow_mut() = Some(Launch {
            runtime: runtime.map(str::to_owned),
            container,
            image_digest,
        });
    });
}

/// Drop any note left behind, so a tick starts clean.
pub(super) fn clear() {
    NOTED.with(|slot| slot.borrow_mut().take());
}

/// Take this tick's note, as span attributes (empty when none was noted).
pub(super) fn take() -> TraceAttributes {
    NOTED
        .with(|slot| slot.borrow_mut().take())
        .map_or_else(TraceAttributes::new, |launch| attributes(&launch))
}

#[cfg(test)]
#[path = "role_runtime/tests.rs"]
mod tests;
