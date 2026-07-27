//! Native Rust port of the token-pool hot path (`loom_tools.tokens`, epic
//! #4081 "eliminate Python from Loom", Phase 1 = issue #4082).
//!
//! # Scope of this phase
//!
//! This is a **pure addition** — nothing in the repo calls into this module
//! yet (`spawn-claude.sh`, `probe-tokens.sh`, and the daemon's own
//! [`crate::token_ranking_refresh`] / [`crate::tokens`] remain on the Python
//! path). The caller cutover is epic #4081 Phase 2.
//!
//! Ported in this phase — the concurrency-critical "hot path" every sweep
//! dispatch exercises, plus the operator-facing bookkeeping CLI:
//!
//! | Module | Python source | Mirrors |
//! |---|---|---|
//! | [`paths`] | `tokens/paths.py` | per-repo/shared pool resolution (#3938) |
//! | [`locking`] | `tokens/_locking.py` | `mkdir`-based lock (no `flock` — absent on stock macOS) |
//! | [`rng`] | (stdlib `random`) | seedable PRNG for deterministic tests, no new crate |
//! | [`rotation`] | `tokens/rotation.py` | one-per-account round-robin cursor (#3909) |
//! | [`bad_tokens`] | `tokens/bad_tokens.py` | `.bad_tokens` mark/read/cleanup, word-boundary matching |
//! | [`allowlist`] | `tokens/allowlist.py` | `.allowlist` pin/unpin CRUD, exact-name validation |
//! | [`failure_counts`] | `tokens/failure_counts.py` | `.failure_counts` consecutive-exhaustion counter |
//! | [`select`] | `tokens/select.py` | 3-tier selection algorithm |
//!
//! **Deferred to a follow-up issue** (filed as part of this PR — see the PR
//! description): `tokens/check.py` (the HTTP rate-limit probe — needs an
//! explicit curl-vs-crate decision plus header-parsing tests), `bootstrap.py`
//! (multi-source `.env` merge + file provisioning), `monitor.py` /
//! `monitor_db.py` (claude-monitor SQLite import). None of these three are on
//! the per-dispatch hot path (they run at bootstrap time or on an operator /
//! periodic cadence), so scoping them out keeps this PR reviewable while
//! still letting a caller flip `spawn-claude.sh` onto the Rust `select`/
//! `pin`/`unpin`/`unblock` surface in Phase 2 without waiting on them.
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
pub mod failure_counts;
pub mod locking;
pub mod paths;
pub mod rng;
pub mod rotation;
pub mod select;

pub use select::{EmptyTokenPoolError, SelectedToken, EX_CONFIG};
