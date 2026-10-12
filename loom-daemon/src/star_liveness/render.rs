//! Human rendering of the liveness report, shared by `loom-daemon status`
//! and `loom-daemon queue`: "stage · time in stage · next actor", plus the
//! ask for anything waiting on the operator.

use crate::types::{StarLandingRow, StarLivenessReport};

/// `12m`, `3h05m`, `2d04h`.
#[must_use]
pub fn short_duration(secs: u64) -> String {
    let m = secs / 60;
    match m {
        0..=59 => format!("{m}m"),
        60..=1439 => format!("{}h{:02}m", m / 60, m % 60),
        _ => format!("{}d{:02}h", m / 1440, (m % 1440) / 60),
    }
}

/// One row: `repo#N  stage · 12m · next actor [· PR #P] [(inherited from #S)]`.
#[must_use]
pub fn row_line(r: &StarLandingRow) -> String {
    let mut line = format!(
        "{}#{}  {} · {} · {}",
        r.repo,
        r.issue,
        r.stage.as_str(),
        short_duration(r.time_in_stage_secs),
        r.next_actor
    );
    if let Some(p) = r.pr {
        line.push_str(&format!(" · PR #{p}"));
    }
    if let Some(b) = &r.blocked_by {
        line.push_str(&format!(" · blocked by {b}"));
    }
    if let Some(c) = &r.no_capacity {
        line.push_str(&format!(" · {c}"));
    }
    if let Some(from) = r.inherited_from {
        line.push_str(&format!(" (inherited from #{from})"));
    }
    if let Some(src) = &r.level_inherited_from {
        line.push_str(&format!(" (inherits {} from {src})", glyph(r.level)));
    }
    line
}

/// The level's glyph (`⭐⭐`), or `⭐` for the star and anything unknown.
#[must_use]
pub fn glyph(level: u8) -> &'static str {
    crate::operator_levels::row(crate::operator_levels::table(), level).map_or("⭐", |r| r.glyph)
}

/// The block, or no lines when nothing is starred.
#[must_use]
pub fn lines(report: Option<&StarLivenessReport>) -> Vec<String> {
    let Some(report) = report else {
        return Vec::new();
    };
    if report.rows.is_empty() && report.dropped_intents.is_empty() && report.over_cap.is_empty() {
        return Vec::new();
    }
    let asks = report.needs_operator().count();
    let mut out = vec![format!(
        "Starred (loom:operator-priority): {} tracked, {asks} waiting on the operator",
        report.rows.len()
    )];
    // #10307: an over-cap level is flagged first; it is never refused.
    for o in &report.over_cap {
        out.push(format!(
            "  ! {} over cap: {} open issues carry {} (cap {}): {}",
            glyph(o.level),
            o.count,
            o.label,
            o.cap,
            o.issues.join(", ")
        ));
    }
    for r in &report.rows {
        let marker = if r.inherited_from.is_some() {
            "  ->"
        } else if r.level >= 2 {
            "  **"
        } else {
            "  *"
        };
        out.push(format!("{marker} {}", row_line(r)));
        if let Some(ask) = &r.ask {
            out.push(format!("      ASK [{}]: {}", ask.kind.as_str(), ask.text));
        }
    }
    if !report.unfollowed_blockers.is_empty() {
        out.push(format!(
            "  (blockers in unmanaged repos, not followed: {})",
            report.unfollowed_blockers.join(", ")
        ));
    }
    if !report.failed_repos.is_empty() {
        out.push(format!(
            "  (could not read: {} — their rows may be missing)",
            report.failed_repos.join(", ")
        ));
    }
    if let Some(d) = report.dropped_intents.last() {
        out.push(format!(
            "  {} loom-ui intent(s) dropped; latest: {} #{} ({})",
            report.dropped_intents.len(),
            d.repo,
            d.number,
            d.reason
        ));
    }
    out
}
