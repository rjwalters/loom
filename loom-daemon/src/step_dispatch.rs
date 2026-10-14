//! Item-first step dispatch (#11894): the inputs the step engine reads,
//! built ahead of the engine itself.
//!
//! - [`listing`]: every open issue and pull request of a repo, from the GitHub
//!   state mirror when it can vouch for the repo, else the shared ETag-cached
//!   REST walk (#11894 §4.2 step 1).
//! - [`stage_entry`]: when an item entered its current stage, from the
//!   mirror's `labels_at`, then the issue's events, then its creation time
//!   (#11894 §4.3).
//!
//! Nothing dispatches from here yet: the item draw (#11896) is the first
//! consumer. Both are compiled and tested now so the draw lands on a measured
//! input rather than a new one.

pub mod listing;
pub mod stage_entry;
