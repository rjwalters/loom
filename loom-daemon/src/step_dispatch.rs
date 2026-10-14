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
//! - [`item_draw`]: the seeded weighted item draw over one workspace's
//!   classified items, `actionable × priority × age` (#11894 §4.2 step 3,
//!   #11896).
//!
//! Nothing dispatches from here yet: the finder cutover (#11909) calls the
//! draw.

pub mod item_draw;
pub mod listing;
pub mod stage_entry;
