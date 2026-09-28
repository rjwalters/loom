//! The `Starred:` block on `loom-daemon status` (#9244 C).
//!
//! A sibling module because `status_render.rs` is a frozen over-threshold
//! ledger entry; the rendering itself is shared with `loom-daemon queue`
//! through [`loom_daemon::star_liveness::render::lines`].

use loom_daemon::types::StarLivenessReport;

/// Print the block, or nothing when no pass has run or nothing is starred.
pub fn print(report: Option<&StarLivenessReport>) {
    for line in loom_daemon::star_liveness::render::lines(report) {
        println!("{line}");
    }
}
