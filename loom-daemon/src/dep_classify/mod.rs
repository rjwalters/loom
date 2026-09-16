//! Dependency classification — the Rust port of `classify-dependency-block.sh`
//! and its two sourced helpers (epic #7810, PR 3).
//!
//! The three scripts form one unit: `classify-dependency-block.sh` *sources*
//! `detect-dependency-cycle.sh` and `detect-startable-subset.sh`, so they could
//! not be ported separately.
//!
//! # The contract this must not move
//!
//! Role prompts invoke the entry point **by path** and parse its stdout
//! line-wise, so the shell entry points keep their names and become thin stubs.
//! `tests/test-classify-dependency-block.sh` (252 assertions) drives them
//! through that CLI and is kept, not translated: if assertions written against
//! the shell implementation still pass against this one, the port preserved
//! behaviour. Translating them into Rust unit tests would replace that evidence
//! with an assertion.

pub mod refs;
pub mod subset;
