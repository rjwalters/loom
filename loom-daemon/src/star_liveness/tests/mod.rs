//! Tests for the starred-issue liveness contract (#9244 C).
//!
//! `fake` is an in-memory forge shared by every host in a test, so "another
//! host already posted it" is literally the same comment list.

#![allow(clippy::unwrap_used, clippy::expect_used)]

/// `pub(crate)` so `safehouse::tests` can compose the **real** publisher with
/// the **real** Safehouse relay in one end-to-end test (#9321) instead of
/// hand-rolling an escalation event that could drift from what a pass emits.
pub(crate) mod fake;
mod intents_tests;
mod landing_tests;
mod notice_tests;
mod pass_tests;
mod replay;
mod review_fix_tests;
mod second_review_tests;
