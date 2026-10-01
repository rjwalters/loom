//! Advisory for a **scoped-socket** daemon starting alongside a live
//! machine-level daemon (#9815).
//!
//! # Why the #3806 singleton guard is not the whole answer
//!
//! `loom-daemon` is a **one-per-machine** process: the token pool
//! (`~/.loom/tokens/`), the per-machine sweep-admission budget
//! (`autonomous.workFinder.maxConcurrent`, #4512) and the singleton IPC socket
//! (`~/.loom/loom-daemon.sock`) are all machine-level, and multi-repo coverage
//! is the workspace registry (#3926), not a second daemon. The #3806 guard in
//! [`crate::ipc::IpcServer::run`] enforces this **per socket**: a start whose
//! socket is already served by a live incumbent refuses. Because the default
//! socket path is machine-level, a naive second start from a different repo or
//! worktree is correctly refused.
//!
//! A start with a custom `LOOM_SOCKET_PATH` never reaches that guard's refusal:
//! it binds a *different* socket and comes up as a fully independent daemon
//! with **zero feedback** that an incumbent exists elsewhere on the machine.
//! Two live daemons on one host then:
//!
//! - **fragment the shared token pool** — each claims accounts with no
//!   cross-process coordination;
//! - **double-claim the per-machine admission budget**;
//! - **duplicate the per-process in-memory brakes** — quarantine tallies
//!   (#3939) and dispatch backoff (#4485) are per-process, the documented
//!   amplifier of the label-flapping incident (daemon-reference §quarantine);
//! - **stomp shared liveness state** — the machine-level pid file, heartbeat
//!   and autonomy marker each name ONE daemon (the #4774 mismatch class).
//!
//! # Why warn, not refuse
//!
//! A scratch-isolated daemon — a tempdir socket plus a scoped supervisor
//! identity — is exactly what the hermetic suites deliberately run alongside
//! production, and [`crate::daemon_start::guards`] already establishes that a
//! refusal there "would be wrong" (`warn_scratch_workdir_drift`'s rationale).
//! So the decision is by *directory character*, not by daemon count: a scoped
//! start rooted at a **scratch-style** directory is silent (test isolation
//! stays friction-free), while one rooted at a **real** directory alongside a
//! live machine-level incumbent logs a loud, actionable warning. The advisory
//! can never fail a start.
//!
//! # Where it runs
//!
//! [`warn_if_scoped_alongside_machine_daemon`] is called from
//! [`crate::ipc::IpcServer::run`] immediately after the #3806 singleton-guard
//! check — the same choke-point discipline as #4774's pid claim: every
//! supervised relaunch passes through it, including the ones that never re-run
//! `loom-daemon-start.sh`, so a parallel daemon surfaces in `daemon.log` the
//! day it starts rather than two incidents later.
//!
//! # Deliberately out of scope
//!
//! The reverse direction (the default-socket daemon learning about scoped
//! siblings) needs a machine-level ownership registry, not a probe — #9816.

use std::path::{Path, PathBuf};

/// The machine-level default socket: `$HOME/.loom/loom-daemon.sock` — what a
/// daemon binds when `LOOM_SOCKET_PATH` is unset.
///
/// Deliberately NOT [`crate::daemon_service::resolve_loom_dir()`] (that
/// *follows* the override): the whole point of this module is to see past this
/// process's own scoping to the socket an unscoped daemon would have bound.
#[must_use]
pub(crate) fn machine_default_socket() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".loom").join("loom-daemon.sock"))
}

/// What a start should do about a live machine-level incumbent — one variant
/// per branch so tests assert on the decision, not the filesystem, and the
/// caller's silence/warn split is exact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ScopedStartDecision {
    /// This start is unscoped (no `LOOM_SOCKET_PATH`, an empty one, or a path
    /// equal to the machine default): the #3806 singleton guard already
    /// refuses a live incumbent, so there is nothing to add.
    CoveredBySingletonGuard,
    /// No home directory — the machine default socket cannot be resolved, so
    /// there is nothing to probe and nothing to say.
    NoMachineDefault,
    /// The machine default socket has no live listener — this daemon, scoped
    /// or not, is the only one on the host.
    NoIncumbent,
    /// A live daemon answers the machine-level default socket while this one
    /// binds elsewhere. `scratch_isolated` marks a test daemon rooted at a
    /// scratch-style directory — silent by design (see the module docs).
    LiveIncumbent { scratch_isolated: bool },
}

/// The pure decision core: every input is a parameter, so tests drive each
/// branch without mutating process-global env vars or binding real sockets.
#[must_use]
pub(crate) fn decide_scoped_start(
    override_env: Option<&str>,
    own_socket: &Path,
    loom_dir: Option<&Path>,
    machine_default: Option<&Path>,
    machine_default_live: bool,
) -> ScopedStartDecision {
    // An exported-but-empty override is unset (the `daemon_pidfile`
    // `non_empty` convention).
    let Some(_over) = override_env.filter(|v| !v.is_empty()) else {
        return ScopedStartDecision::CoveredBySingletonGuard;
    };
    let Some(default) = machine_default else {
        return ScopedStartDecision::NoMachineDefault;
    };
    if own_socket == default {
        return ScopedStartDecision::CoveredBySingletonGuard;
    }
    if !machine_default_live {
        return ScopedStartDecision::NoIncumbent;
    }
    ScopedStartDecision::LiveIncumbent {
        scratch_isolated: loom_dir.is_some_and(|dir| {
            crate::daemon_start::guards::is_scratch_style_path(&dir.display().to_string())
        }),
    }
}

/// The operator-facing warning for a [`ScopedStartDecision`] — `None` for
/// every branch that stays silent, so the caller is an exact
/// `if let Some(warning)`.
#[must_use]
pub(crate) fn scoped_start_warning(
    decision: &ScopedStartDecision,
    own_socket: &Path,
    machine_default: &Path,
) -> Option<String> {
    if !matches!(
        decision,
        ScopedStartDecision::LiveIncumbent {
            scratch_isolated: false
        }
    ) {
        return None;
    }
    Some(format!(
        "this daemon binds a NON-default socket {}, while another loom-daemon is already \
         listening on the machine-level socket {} — loom-daemon is ONE daemon per machine \
         (#3926): two live daemons each claim the shared token pool and the per-machine \
         concurrency budget, and their in-memory quarantine/backoff brakes cannot see each \
         other (label-flapping risk). Multi-repo coverage is `loom-daemon workspace add`, not \
         a second daemon (#9815). Stop one of the two unless this is a deliberate isolated \
         test instance.",
        own_socket.display(),
        machine_default.display()
    ))
}

/// The startup choke point: resolve this process's env and delegate to
/// [`warn_scoped_start`]. Never fatal, never refuses — every error branch is a
/// silent return (see [`ScopedStartDecision`]).
pub(crate) async fn warn_if_scoped_alongside_machine_daemon(own_socket: &Path) {
    let Some(machine_default) = machine_default_socket() else {
        return;
    };
    let override_env = std::env::var("LOOM_SOCKET_PATH").ok();
    let loom_dir = crate::autonomy_marker::resolve_loom_dir();
    warn_scoped_start(own_socket, override_env.as_deref(), loom_dir.as_deref(), &machine_default)
        .await;
}

/// The parameterised advisory body: probe the machine-level default socket
/// only when a real-directory scoped start could plausibly warn, then decide
/// and log. Parameterised so tests drive every branch without mutating
/// process-global env vars or depending on the checkout's own directory
/// character (review fix: the wrapper test used to build its warn fixture
/// under `CARGO_MANIFEST_DIR`, which classifies scratch in a
/// `.loom/worktrees/` or `*-checkout` checkout — exactly where builders run
/// scoped tests).
async fn warn_scoped_start(
    own_socket: &Path,
    override_env: Option<&str>,
    loom_dir: Option<&Path>,
    machine_default: &Path,
) {
    // Probe lazily: an unscoped start is the #3806 guard's business and a
    // scratch-isolated daemon stays silent regardless, so neither pays a
    // connect to the default socket on every startup. `true` asks the
    // hypothetical "would a live incumbent warn?" — if even that is no, the
    // probe cannot change the outcome.
    let could_warn = matches!(
        decide_scoped_start(override_env, own_socket, loom_dir, Some(machine_default), true,),
        ScopedStartDecision::LiveIncumbent {
            scratch_isolated: false
        }
    );
    if !could_warn {
        return;
    }
    let machine_default_live = crate::ipc::socket_has_live_listener(machine_default).await;
    let decision = decide_scoped_start(
        override_env,
        own_socket,
        loom_dir,
        Some(machine_default),
        machine_default_live,
    );
    if let Some(warning) = scoped_start_warning(&decision, own_socket, machine_default) {
        log::warn!("daemon_parallel_advisory: {warning}");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    // ---------------- decision matrix ----------------

    fn decided(
        override_env: Option<&str>,
        own: &str,
        loom_dir: Option<&str>,
        default: Option<&str>,
        default_live: bool,
    ) -> ScopedStartDecision {
        let path = |s: Option<&str>| s.map(PathBuf::from);
        decide_scoped_start(
            override_env,
            Path::new(own),
            path(loom_dir).as_deref(),
            path(default).as_deref(),
            default_live,
        )
    }

    #[test]
    fn an_unscoped_start_is_the_singleton_guards_business() {
        // No override at all: the #3806 guard refuses a live incumbent on the
        // very socket this daemon would bind.
        assert_eq!(
            decided(None, "/h/.loom/loom-daemon.sock", Some("/h/.loom"), Some("/h/.loom/x"), true),
            ScopedStartDecision::CoveredBySingletonGuard
        );
    }

    #[test]
    fn an_empty_override_is_unset_not_scoped() {
        // The daemon_pidfile `non_empty` convention.
        assert_eq!(
            decided(
                Some(""),
                "/h/.loom/loom-daemon.sock",
                Some("/h/.loom"),
                Some("/h/.loom/loom-daemon.sock"),
                true
            ),
            ScopedStartDecision::CoveredBySingletonGuard
        );
    }

    #[test]
    fn an_override_equal_to_the_default_is_unscoped() {
        assert_eq!(
            decided(
                Some("/h/.loom/loom-daemon.sock"),
                "/h/.loom/loom-daemon.sock",
                Some("/h/.loom"),
                Some("/h/.loom/loom-daemon.sock"),
                true
            ),
            ScopedStartDecision::CoveredBySingletonGuard
        );
    }

    #[test]
    fn no_home_means_no_machine_default_to_probe() {
        assert_eq!(
            decided(Some("/elsewhere/daemon.sock"), "/elsewhere/daemon.sock", None, None, true),
            ScopedStartDecision::NoMachineDefault
        );
    }

    #[test]
    fn a_quiet_default_socket_means_this_daemon_is_the_only_one() {
        assert_eq!(
            decided(
                Some("/elsewhere/daemon.sock"),
                "/elsewhere/daemon.sock",
                Some("/elsewhere"),
                Some("/h/.loom/loom-daemon.sock"),
                false
            ),
            ScopedStartDecision::NoIncumbent
        );
    }

    #[test]
    fn a_live_incumbent_with_a_real_loom_dir_warns() {
        // The target scenario: a scoped start rooted at a REAL directory while
        // the machine-level daemon is live.
        assert_eq!(
            decided(
                Some("/home/u/.loom-alt/daemon.sock"),
                "/home/u/.loom-alt/daemon.sock",
                Some("/home/u/.loom-alt"),
                Some("/home/u/.loom/loom-daemon.sock"),
                true
            ),
            ScopedStartDecision::LiveIncumbent {
                scratch_isolated: false
            }
        );
    }

    #[test]
    fn a_scratch_rooted_scoped_daemon_stays_silent() {
        // The hermetic suites' setup: a tempdir socket alongside production.
        // `/var/folders/...` is macOS `$TMPDIR`, matched by
        // `is_scratch_style_path`.
        assert_eq!(
            decided(
                Some("/var/folders/nd/t/loom-test/daemon.sock"),
                "/var/folders/nd/t/loom-test/daemon.sock",
                Some("/var/folders/nd/t/loom-test"),
                Some("/home/u/.loom/loom-daemon.sock"),
                true
            ),
            ScopedStartDecision::LiveIncumbent {
                scratch_isolated: true
            }
        );
    }

    #[test]
    fn an_unresolvable_loom_dir_is_not_treated_as_scratch() {
        // `resolve_loom_dir` cannot fail when `LOOM_SOCKET_PATH` is set (the
        // parent of the path), but the pure core must not silently classify a
        // missing value as test isolation.
        assert_eq!(
            decided(
                Some("/home/u/.loom-alt/daemon.sock"),
                "/home/u/.loom-alt/daemon.sock",
                None,
                Some("/home/u/.loom/loom-daemon.sock"),
                true
            ),
            ScopedStartDecision::LiveIncumbent {
                scratch_isolated: false
            }
        );
    }

    // ---------------- warning rendering ----------------

    #[test]
    fn the_warning_names_both_sockets_and_the_multi_repo_path() {
        let warning = scoped_start_warning(
            &ScopedStartDecision::LiveIncumbent {
                scratch_isolated: false,
            },
            Path::new("/home/u/.loom-alt/daemon.sock"),
            Path::new("/home/u/.loom/loom-daemon.sock"),
        )
        .expect("a live non-scratch incumbent warns");
        assert!(warning.contains("/home/u/.loom-alt/daemon.sock"), "warning: {warning}");
        assert!(warning.contains("/home/u/.loom/loom-daemon.sock"), "warning: {warning}");
        assert!(warning.contains("workspace add"), "warning: {warning}");
        assert!(warning.contains("#9815"), "warning: {warning}");
    }

    #[test]
    fn every_silent_branch_renders_no_warning() {
        let silent = [
            ScopedStartDecision::CoveredBySingletonGuard,
            ScopedStartDecision::NoMachineDefault,
            ScopedStartDecision::NoIncumbent,
            ScopedStartDecision::LiveIncumbent {
                scratch_isolated: true,
            },
        ];
        for decision in &silent {
            assert_eq!(
                scoped_start_warning(decision, Path::new("/x"), Path::new("/y")),
                None,
                "{decision:?} must stay silent"
            );
        }
    }

    // ---------------- env resolution ----------------

    #[test]
    #[serial_test::serial(env_home_path)]
    fn machine_default_is_home_loom_loom_daemon_sock() {
        let fake_home = tempfile::tempdir().unwrap();
        let old_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", fake_home.path());

        let resolved = machine_default_socket();

        match old_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        assert_eq!(resolved, Some(fake_home.path().join(".loom").join("loom-daemon.sock")));
    }

    // ---------------- the startup wrapper ----------------

    /// A minimal live daemon on `socket_path`: accepts connections in a loop
    /// and answers every line with the adjacently-tagged `Response::Pong`
    /// wire shape, so `socket_has_live_listener`'s Ping/Pong probe succeeds.
    /// Runs on the caller's runtime; the task dies with the temporary runtime
    /// when `block_on` returns (the listener's socket file lives in the
    /// caller's tempdir, which is dropped right after).
    async fn spawn_fake_pong_daemon(socket_path: &Path) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let listener = tokio::net::UnixListener::bind(socket_path).unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let (reader, mut writer) = stream.into_split();
                tokio::spawn(async move {
                    let mut lines = BufReader::new(reader).lines();
                    while let Ok(Some(_line)) = lines.next_line().await {
                        let _ = writer.write_all(b"{\"type\":\"Pong\"}\n").await;
                        let _ = writer.flush().await;
                    }
                });
            }
        });
    }

    /// Drive one `warn_scoped_start` scenario under `capture_logs`, on a
    /// current-thread runtime built inside the sync closure (a plain `#[test]`
    /// — the runtime must not already be running). The fake Pong daemon is
    /// spawned first so the probe (when it runs) sees a live incumbent.
    fn captured_scenario(own_socket: &Path, loom_dir: Option<&str>) -> Vec<(log::Level, String)> {
        let machine_dir = tempfile::tempdir().unwrap();
        let machine_socket = machine_dir.path().join("machine.sock");
        crate::test_log_capture::capture_logs(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    spawn_fake_pong_daemon(&machine_socket).await;
                    warn_scoped_start(
                        own_socket,
                        Some(own_socket.to_str().unwrap()),
                        loom_dir.map(Path::new),
                        &machine_socket,
                    )
                    .await;
                })
        })
    }

    fn advisory_warns(records: &[(log::Level, String)]) -> Vec<&str> {
        records
            .iter()
            .filter(|(level, msg)| {
                *level == log::Level::Warn && msg.contains("daemon_parallel_advisory")
            })
            .map(|(_, msg)| msg.as_str())
            .collect()
    }

    /// The warn half, end to end through the wrapper body: a real-directory
    /// scoped start alongside a live machine-level daemon produces exactly one
    /// advisory warn naming both sockets. The loom dir is a plain string, so
    /// the scenario is independent of the checkout's own directory character —
    /// under `CARGO_MANIFEST_DIR` the fixture used to classify scratch in a
    /// `.loom/worktrees/` checkout and fail deterministically (review fix).
    #[test]
    fn wrapper_body_warns_for_a_real_scoped_start() {
        let own = tempfile::tempdir().unwrap().path().join("daemon.sock");
        let records = captured_scenario(&own, Some("/home/u/.loom-alt"));
        let warns = advisory_warns(&records);
        assert_eq!(
            warns.len(),
            1,
            "exactly one advisory warn expected; got {warns:?} — full capture: {records:?}"
        );
        assert!(warns[0].contains(own.display().to_string().as_str()), "warn: {}", warns[0]);
        assert!(
            warns[0].contains("machine.sock"),
            "the warning must name the machine-level socket: {}",
            warns[0]
        );
    }

    /// The silent half: the same live incumbent, but the scoped start is
    /// rooted at a scratch-style directory — the hermetic suites' setup — so
    /// nothing is logged and the probe is not even paid.
    #[test]
    fn wrapper_body_stays_silent_for_a_scratch_rooted_scoped_start() {
        let scratch_root = std::env::temp_dir().join("loom-parallel-advisory-test");
        let own = scratch_root.join("daemon.sock");
        let records = captured_scenario(&own, Some(scratch_root.to_str().unwrap()));
        assert!(
            advisory_warns(&records).is_empty(),
            "a scratch-isolated scoped daemon must stay silent; got {records:?}"
        );
    }

    /// An unscoped start is the #3806 guard's business — the advisory must
    /// stay silent for it (and, structurally, never even probe: the early
    /// return in `warn_scoped_start` precedes the connect).
    #[test]
    fn wrapper_body_stays_silent_for_an_unscoped_start() {
        let own = tempfile::tempdir().unwrap().path().join("daemon.sock");
        let records = crate::test_log_capture::capture_logs(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    // No fake daemon needed: the early return precedes the
                    // probe, so nothing should ever connect.
                    warn_scoped_start(
                        &own,
                        None,
                        Some(Path::new("/home/u/.loom")),
                        Path::new("/nonexistent/.loom/loom-daemon.sock"),
                    )
                    .await;
                });
        });
        assert!(
            advisory_warns(&records).is_empty(),
            "an unscoped start must stay silent; got {records:?}"
        );
    }
}
