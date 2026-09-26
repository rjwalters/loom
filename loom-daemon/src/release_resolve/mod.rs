//! Release-artifact resolution (epic #7810, PR 5).
//!
//! Answers one question: *what is the latest release binary for this host's
//! platform, and how does it compare to what is installed?*
//!
//! # Why this moved into Rust
//!
//! `--resolve-json` (#7609) exists precisely **because** this was not in Rust.
//! Its own header says so: the daemon asks `loom-daemon-update.sh` what the
//! latest artifact is "rather than reimplementing release resolution in Rust".
//! That was the right call when the alternative was a second implementation;
//! it is the thing to undo now that the daemon can own the only one.
//!
//! `auto_update.rs` calls [`resolve`] directly. `loom-daemon-update.sh
//! --resolve-json` keeps working by delegating here, so the 27 assertions
//! already written against that mode stay as the equivalence proof.
//!
//! # Strictly read-only
//!
//! No binary download, no `git fetch`, no build, no provision, no restart. The
//! only thing fetched is the release's ~65-byte `.sha256` asset. Everything in
//! this module is a query.
//!
//! # Never fabricate
//!
//! Every optional field is `None` when it could not be determined — an older
//! `gh` that does not report `publishedAt`, a `.sha256` asset that would not
//! download, a host with no resolvable installed binary. A guessed value here
//! becomes a persisted comparison the daemon acts on.

pub mod emit;
pub mod host;
pub mod resolve;
pub mod semver;

pub use resolve::{
    asset_names, build_time_repo, explain_no_artifact, resolve, resolve_repo, Inputs, Resolution,
    Resolved,
};
