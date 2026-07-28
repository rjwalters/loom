//! Native Rust port of the token-pool hot path (`loom_tools.tokens`, epic
//! #4081 "eliminate Python from Loom", Phase 1 = issue #4082).
//!
//! # Scope of this phase
//!
//! Phase 1 (issue #4082) was a **pure addition** with no caller cutover. Phase
//! 2 landed in two increments: issue #4080 cut the check/probe path over
//! (`loom-daemon tokens check`, issue #4108, this module's [`check`]) —
//! invoked by `defaults/scripts/probe-tokens.sh` (native-binary resolution,
//! `python3 -m` fallback removed), the daemon's own
//! [`crate::token_ranking_refresh::ScriptRankingRefreshRunner`] (via
//! `std::env::current_exe()`), and `loom-daemon status`'s
//! `collect_token_usage()` (in-process). Issue #4228 finished Phase 2: the
//! token-*selection* path (`spawn-claude.sh` / `claude-wrapper.sh`, this
//! module's [`select`]) and the bad-token-marking path
//! (`claude-wrapper.sh`'s account rotation, this module's [`bad_tokens`], now
//! exposed as `loom-daemon tokens mark-bad`) both cut over to the native CLI
//! too — zero Python left on the token hot path, and the `LOOM_PACKAGE_PATH`
//! bridge that used to locate the Python package for those two callers was
//! retired end-to-end (scripts + `sweep_registry`/`role_runner` forwarding).
//!
//! Ported in this phase — the concurrency-critical "hot path" every sweep
//! dispatch exercises, plus the operator-facing bookkeeping CLI:
//!
//! | Module | Python source | Mirrors |
//! |---|---|---|
//! | [`bootstrap`] | `tokens/bootstrap.py` | multi-source `.env` merge + `.token`/`index.json` provisioning |
//! | [`paths`] | `tokens/paths.py` | per-repo/shared pool resolution (#3938) |
//! | [`locking`] | `tokens/_locking.py` | `mkdir`-based lock (no `flock` — absent on stock macOS) |
//! | [`rng`] | (stdlib `random`) | seedable PRNG for deterministic tests, no new crate |
//! | [`rotation`] | `tokens/rotation.py` | one-per-account round-robin cursor (#3909) |
//! | [`bad_tokens`] | `tokens/bad_tokens.py` | `.bad_tokens` mark/read/cleanup, word-boundary matching |
//! | [`allowlist`] | `tokens/allowlist.py` | `.allowlist` pin/unpin CRUD, exact-name validation |
//! | [`failure_counts`] | `tokens/failure_counts.py` | `.failure_counts` consecutive-exhaustion counter |
//! | [`select`] | `tokens/select.py` | 3-tier selection algorithm |
//! | [`check`] | `tokens/check.py` | HTTP rate-limit probe (curl transport) + `.ranking` writer |
//! | [`monitor`] | `tokens/monitor.py` | claude-monitor `ranking.json` consumer (`--source auto\|monitor`) |
//! | [`monitor_db`] | `tokens/monitor_db.py` | claude-monitor live SQLite (`usage.db`) import, `import-from-monitor` |
//!
//! The probe path ([`check`] + [`monitor`], issue #4094) shells to `curl`
//! rather than take an HTTP-client crate — following the [`rng`] precedent of
//! avoiding new dependencies, and matching `token_ranking_refresh.rs`, which
//! already shells out via `Command::new`. The transport is behind the
//! [`check::ProbeTransport`] trait so tests never touch the network.
//!
//! `bootstrap.py` (multi-source `.env` merge + file provisioning) was ported in
//! issue #4105 as [`bootstrap`]. `monitor_db.py` (claude-monitor SQLite import,
//! consumed by `import-from-monitor`) was ported in issue #4106 as
//! [`monitor_db`], reusing [`bootstrap::materialize_accounts`] /
//! [`bootstrap::write_index`] so the writer stays identical by construction.
//! [`bootstrap`] still leaves the read-only `_check_monitor_divergence` warning
//! (which reads the live `usage.db`) to a future follow-up, since it is a
//! bootstrap-time advisory rather than the import path itself.
//!
//! # Byte-compatible state (hard requirement)
//!
//! `.ranking`, `.bad_tokens`, `.allowlist`, `.failure_counts` must parse
//! identically whether written by Python or Rust, since both implementations
//! coexist until epic #4081 Phase 4. Every writer here uses the same file
//! formats as its Python counterpart (see each submodule's doc comment) and
//! the conformance suite under `loom-tools/tests/tokens/test_rust_conformance.py`
//! diffs fixture pools driven through both CLIs.

pub mod allowlist;
pub mod bad_tokens;
pub mod bootstrap;
pub mod check;
pub mod failure_counts;
pub mod locking;
pub mod monitor;
pub mod monitor_db;
pub mod paths;
pub mod rng;
pub mod rotation;
pub mod select;

pub use select::{EmptyTokenPoolError, SelectedToken, EX_CONFIG};

/// Auto-unpin pre-flight (issue #4228, epic #4081 Phase 2): if the operator
/// has pinned specific accounts (`.allowlist`) and EVERY pinned account has
/// reached the consecutive-failure threshold ([`failure_counts::DEFAULT_THRESHOLD`]),
/// clear the pin and reset the failure counters rather than trap the spawner
/// on a set of exhausted pinned accounts forever. Mirrors the inline Python
/// heredoc `spawn-claude.sh` historically ran ahead of every selection
/// (never a standalone Python module — this is the first time the behavior
/// gets a name in either language).
///
/// Empty-pool guard preserved: this NEVER touches `.bad_tokens` — if that
/// file blocks every account, the operator must intervene (e.g. `loom-tokens
/// unblock <name>`).
///
/// Returns `Some(message)` — already formatted like the historical Python
/// advisory line — when the auto-unpin fired; `None` when there was nothing
/// to do (no active pin, or not every pinned account is over threshold).
#[must_use]
pub fn maybe_auto_unpin(workspace: &std::path::Path) -> Option<String> {
    let pinned = allowlist::read_allowlist(workspace);
    if pinned.is_empty() {
        return None;
    }
    let threshold = failure_counts::DEFAULT_THRESHOLD;
    let all_over_threshold = pinned
        .iter()
        .all(|name| failure_counts::threshold_reached(workspace, name, threshold));
    if !all_over_threshold {
        return None;
    }
    let _ = allowlist::clear_allowlist(workspace);
    let _ = failure_counts::reset_all(workspace);
    Some(format!(
        "[auto-unpin] All {} pinned account(s) hit {threshold} consecutive failures; cleared \
         .allowlist.",
        pinned.len()
    ))
}

#[cfg(test)]
mod auto_unpin_tests {
    use super::*;
    use std::fs;

    fn make_pool(names: &[&str]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".loom").join("tokens");
        fs::create_dir_all(&dir).unwrap();
        for n in names {
            fs::write(dir.join(format!("{n}.token")), format!("key-{n}")).unwrap();
        }
        tmp
    }

    #[test]
    fn no_allowlist_is_a_noop() {
        let tmp = make_pool(&["a"]);
        assert!(maybe_auto_unpin(tmp.path()).is_none());
    }

    #[test]
    fn allowlist_below_threshold_is_a_noop() {
        let tmp = make_pool(&["a", "b"]);
        allowlist::write_allowlist(tmp.path(), &["a".to_string()]).unwrap();
        for _ in 0..failure_counts::DEFAULT_THRESHOLD - 1 {
            failure_counts::record_failure(tmp.path(), "a", failure_counts::DEFAULT_THRESHOLD)
                .unwrap();
        }
        assert!(maybe_auto_unpin(tmp.path()).is_none());
        assert_eq!(allowlist::read_allowlist(tmp.path()), vec!["a".to_string()]);
    }

    #[test]
    fn all_pinned_over_threshold_clears_allowlist_and_counters() {
        let tmp = make_pool(&["a", "b", "c"]);
        allowlist::write_allowlist(tmp.path(), &["a".to_string(), "b".to_string()]).unwrap();
        for name in ["a", "b"] {
            for _ in 0..failure_counts::DEFAULT_THRESHOLD {
                failure_counts::record_failure(tmp.path(), name, failure_counts::DEFAULT_THRESHOLD)
                    .unwrap();
            }
        }
        let msg = maybe_auto_unpin(tmp.path()).expect("expected auto-unpin to fire");
        assert!(msg.contains("[auto-unpin]"));
        assert!(msg.contains("2 pinned account"));
        assert!(allowlist::read_allowlist(tmp.path()).is_empty());
        assert_eq!(failure_counts::get_count(tmp.path(), "a"), 0);
        assert_eq!(failure_counts::get_count(tmp.path(), "b"), 0);
    }

    #[test]
    fn one_pinned_account_still_healthy_blocks_auto_unpin() {
        let tmp = make_pool(&["a", "b"]);
        allowlist::write_allowlist(tmp.path(), &["a".to_string(), "b".to_string()]).unwrap();
        for _ in 0..failure_counts::DEFAULT_THRESHOLD {
            failure_counts::record_failure(tmp.path(), "a", failure_counts::DEFAULT_THRESHOLD)
                .unwrap();
        }
        // "b" never failed — the pin must survive.
        assert!(maybe_auto_unpin(tmp.path()).is_none());
        assert_eq!(allowlist::read_allowlist(tmp.path()), vec!["a".to_string(), "b".to_string()]);
    }
}
