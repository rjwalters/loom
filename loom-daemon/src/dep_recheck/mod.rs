//! Curator's re-check fingerprints — the Rust port of
//! `dep-recheck-fingerprint.sh` (epic #7810, PR 4).
//!
//! Answers, deterministically, *what did this pass conclude* for the two
//! `curator.md` sections that must not re-post an unchanged conclusion:
//! "Re-check Idempotency" (#4986) and "Checking Operator-Only Premises"
//! (#6849).
//!
//! # Why it exists at all
//!
//! Both sections used to define their `CONCLUSION_HASH` as inline bash embedded
//! in the role prompt *text*, re-derived from natural-language instructions by
//! every Curator invocation. In production the fingerprint churned across dozens
//! of distinct values on #6335/#6805 over weeks with an unchanged blocking
//! condition, defeating the "never re-post an unchanged conclusion" guard and
//! spamming near-duplicate comments.
//!
//! # What it does NOT do
//!
//! It does not compare against a prior marker and post anything. It computes a
//! hash; `curator.md` reads that hash against the most recent marker comment.
//! [`decide`] is the one piece of that comparison extracted here, because it is
//! pure arithmetic over two strings and a number — and because it determines
//! whether a `loom:curating` claim is needed at all (#7617), which must be
//! answerable *before* claiming.
//!
//! `BLOCK_REASON` (free-text justification) and `ORTHOGONAL` (#6516) stay
//! caller-supplied pass-throughs: they are judgment calls made by reading prose,
//! not mechanical PR-state facts. They fold into the hash verbatim, exactly as
//! the old inline formula did.

pub mod cli;
pub mod decide;
pub mod extract;
pub mod forge;
pub mod named;
pub mod premise;
pub mod recheck;
