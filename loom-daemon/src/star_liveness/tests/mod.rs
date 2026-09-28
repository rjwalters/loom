//! Tests for the starred-issue liveness contract (#9244 C).
//!
//! `fake` is an in-memory forge shared by every host in a test, so "another
//! host already posted it" is literally the same comment list.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod fake;
mod intents_tests;
mod landing_tests;
mod pass_tests;
mod replay;
mod review_fix_tests;
mod second_review_tests;
