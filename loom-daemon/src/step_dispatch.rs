//! Item-first step dispatch (#11894): the inputs the step engine reads,
//! built ahead of the engine itself.
//!
//! - [`listing`]: every open issue and pull request of a repo, from the GitHub
//!   state mirror when it can vouch for the repo, else the shared ETag-cached
//!   REST walk (#11894 §4.2 step 1). Since #12306 this is the Direct backend
//!   of [`crate::discovery`]; the gather reads the tick's snapshot, not this.
//! - [`stage_entry`]: when an item entered its current stage, from the
//!   mirror's `labels_at`, then the issue's events, then its creation time
//!   (#11894 §4.3).
//!
//! - [`item_draw`]: the seeded weighted item draw over the one pool of every workspace's
//!   classified items, `actionable × priority × age` (#11894 §4.2 step 3,
//!   #11896).
//!
//! - [`outcome_ledger`]: what a finished step's outcome class does next, per
//!   `(item, step)`, and the cooldown query the draw reads (#11900).
//! - [`reeval_ladder`]: the Champion re-evaluation ladder, 6 h → 7 d, kept
//!   in the item's body on the forge (#11900).
//!
//! - [`gather`]: one workspace's items, joined, classified and ready to
//!   weigh, with the forge facts the classifier reads (#11909).
//! - [`tick`]: the step tick: workspace draw, item draw, gates, claim and
//!   spawn, redrawing on a pass (#11909).
//! - [`step_end`]: a finished step's forge post-state, classified and
//!   recorded in the outcome ledger (#11909).
//! - [`gather_budget`]: which repos a tick gathers before its wall-clock
//!   deadline, and where the next tick resumes (#12074).
//! - [`driver`]: the production tick the work-finder loop calls, over the
//!   live registries (#11909).
//!
//! This is the work finder's only item dispatch path (#11909): the
//! `loom:issue`-only sweep finder is no longer called.

pub mod driver;
pub mod gather;
pub mod gather_budget;
pub mod item_draw;
pub mod listing;
pub mod outcome_ledger;
pub mod reeval_ladder;
pub mod stage_entry;
pub mod step_end;
pub mod tick;
