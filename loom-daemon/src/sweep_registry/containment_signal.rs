//! Containment observability (issue #7430, epic #6896 Phase 3: per-sweep
//! resource limits and containment observability).
//!
//! `spawn-claude.sh`'s containerized dispatch mode (issue #7429) writes a
//! canonical `# LOOM_DISPATCH_MODE mode=<container|bare-metal> ...` marker to
//! the per-sweep log — one line for a `docker run`-wrapped dispatch (naming
//! the image and the applied `--cpus`/`--memory` limits, or `none` for an
//! intentionally-unbounded axis) and a symmetric one-word line for bare-metal
//! dispatch. This module parses that marker so `loom-daemon status` can
//! surface which sweeps are running containerized and their configured
//! resource limits (the #7430 acceptance criterion).
//!
//! Deliberately independent of `crash_signals.rs`'s polling machinery
//! (`poll_observability`/`token_name`/`runtime`): those fields need to
//! tolerate a slow-starting child that has not yet logged its selection, so
//! they poll with a bounded timeout. The dispatch-mode marker is written
//! once, synchronously, at the very top of `spawn-claude.sh` — well before
//! token selection even begins — so a single best-effort read is sufficient
//! and no caller of this module needs to block waiting for it. This also
//! keeps the change scoped to observability/rendering: no `SweepInfo` schema
//! change, no dispatch-path plumbing, no IPC wire-format bump.

use std::path::Path;

/// The canonical marker `spawn-claude.sh` writes for a containerized
/// dispatch (see the containment block in `defaults/scripts/spawn-claude.sh`).
pub const DISPATCH_MODE_LOG_MARKER: &str = "# LOOM_DISPATCH_MODE mode=";

/// Parsed containment shape for one sweep.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContainmentSignal {
    /// `true` when the marker's mode token was `container`; `false` for
    /// `bare-metal`, an absent marker (bare-metal dispatch predating this
    /// marker, or a read that failed), or any unrecognized token.
    pub containerized: bool,
    /// The `--cpus` value applied to the container, when present and not
    /// the marker's explicit `none` sentinel (an intentionally-unbounded
    /// axis, e.g. the host CPU-quota mechanism was disabled at dispatch
    /// time). Always `None` when `containerized` is `false`.
    pub cpus: Option<String>,
    /// The `--memory` value applied to the container, mirroring `cpus`.
    pub memory: Option<String>,
}

/// Extract the containment signal from log `contents`, scanning only the
/// region at/after this dispatch's header (`header_anchor`, e.g.
/// `sweep_id=<id>`) — mirrors `crash_signals::parse_runtime_after`'s
/// anchoring so a rerun's fresh header is never confused with a stale
/// marker line left by a previous dispatch appended to the same reused
/// per-issue log. Within that region, the FIRST occurrence of the marker is
/// used deliberately (not the last): a containerized dispatch's outer,
/// host-side `docker run` decision is logged before the recursed child (which
/// runs with `LOOM_SPAWN_CONTAINERIZED=1` already set) logs its own,
/// necessarily bare-metal-relative-to-itself marker later in the same log
/// stream — the OUTER decision is what "is this sweep containerized" means.
///
/// Returns [`ContainmentSignal::default`] (bare-metal / unknown) when the
/// header, the marker, or a recognizable mode token is absent — never
/// panics.
pub fn parse_containment_after(contents: &str, header_anchor: &str) -> ContainmentSignal {
    let Some(region_start) = contents.rfind(header_anchor) else {
        return ContainmentSignal::default();
    };
    let region = &contents[region_start..];
    let Some(marker_at) = region.find(DISPATCH_MODE_LOG_MARKER) else {
        return ContainmentSignal::default();
    };
    let after = &region[marker_at + DISPATCH_MODE_LOG_MARKER.len()..];
    let Some(line) = after.lines().next() else {
        return ContainmentSignal::default();
    };

    let mut parts = line.split_whitespace();
    let containerized = parts.next() == Some("container");
    if !containerized {
        return ContainmentSignal::default();
    }

    let mut signal = ContainmentSignal {
        containerized: true,
        cpus: None,
        memory: None,
    };
    for kv in parts {
        if let Some(v) = kv.strip_prefix("cpus=") {
            if v != "none" {
                signal.cpus = Some(v.to_string());
            }
        } else if let Some(v) = kv.strip_prefix("memory=") {
            if v != "none" {
                signal.memory = Some(v.to_string());
            }
        }
    }
    signal
}

/// Best-effort, single-read detection from `log_path` — never blocks, never
/// fails the caller, and never panics on a missing/unreadable file.
/// `header_anchor` should be `format!("sweep_id={sweep_id}")`, matching the
/// anchor `crash_signals`'s parsers already use for the same log.
pub fn detect_containment(log_path: &Path, header_anchor: &str) -> ContainmentSignal {
    std::fs::read_to_string(log_path)
        .ok()
        .map(|contents| parse_containment_after(&contents, header_anchor))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_marker_defaults_to_bare_metal() {
        let log =
            "==== loom-daemon dispatch: sweep_id=sweep-issue-1-0 issue=1 ====\nno marker here\n";
        let sig = parse_containment_after(log, "sweep_id=sweep-issue-1-0");
        assert_eq!(sig, ContainmentSignal::default());
        assert!(!sig.containerized);
    }

    #[test]
    fn bare_metal_marker_parses_as_not_containerized() {
        let log = "==== loom-daemon dispatch: sweep_id=sweep-issue-2-0 issue=2 ====\n\
                   # LOOM_DISPATCH_MODE mode=bare-metal\n";
        let sig = parse_containment_after(log, "sweep_id=sweep-issue-2-0");
        assert!(!sig.containerized);
        assert_eq!(sig.cpus, None);
        assert_eq!(sig.memory, None);
    }

    #[test]
    fn container_marker_with_limits_parses_both_axes() {
        let log = "==== loom-daemon dispatch: sweep_id=sweep-issue-3-0 issue=3 ====\n\
                   # LOOM_DISPATCH_MODE mode=container image=ghcr.io/rjwalters/loom-worker:latest cpus=6 memory=4096m\n";
        let sig = parse_containment_after(log, "sweep_id=sweep-issue-3-0");
        assert!(sig.containerized);
        assert_eq!(sig.cpus.as_deref(), Some("6"));
        assert_eq!(sig.memory.as_deref(), Some("4096m"));
    }

    #[test]
    fn container_marker_with_none_axes_reports_unbounded() {
        let log = "==== loom-daemon dispatch: sweep_id=sweep-issue-4-0 issue=4 ====\n\
                   # LOOM_DISPATCH_MODE mode=container image=custom cpus=none memory=none\n";
        let sig = parse_containment_after(log, "sweep_id=sweep-issue-4-0");
        assert!(sig.containerized);
        assert_eq!(sig.cpus, None);
        assert_eq!(sig.memory, None);
    }

    #[test]
    fn anchors_to_the_last_header_not_a_stale_earlier_one() {
        // A rerun appends a fresh header; a stale marker before the CURRENT
        // header (belonging to a previous dispatch attempt) must not leak
        // into this dispatch's containment signal.
        let log = "==== loom-daemon dispatch: sweep_id=old ====\n\
                   # LOOM_DISPATCH_MODE mode=container image=old cpus=1 memory=512m\n\
                   ==== loom-daemon dispatch: sweep_id=new issue=5 ====\n\
                   # LOOM_DISPATCH_MODE mode=bare-metal\n";
        let sig = parse_containment_after(log, "sweep_id=new");
        assert!(!sig.containerized);
    }

    #[test]
    fn first_marker_after_header_wins_over_a_recursed_childs_later_one() {
        // The outer (host-side) `docker run` decision is logged before the
        // recursed child's own bare-metal-relative-to-itself marker later in
        // the same log stream. The FIRST marker after the header is the
        // dispatch's real containment status.
        let log = "==== loom-daemon dispatch: sweep_id=sweep-issue-6-0 issue=6 ====\n\
                   # LOOM_DISPATCH_MODE mode=container image=w cpus=2 memory=1024m\n\
                   # LOOM_DISPATCH_MODE mode=bare-metal\n";
        let sig = parse_containment_after(log, "sweep_id=sweep-issue-6-0");
        assert!(sig.containerized);
        assert_eq!(sig.cpus.as_deref(), Some("2"));
        assert_eq!(sig.memory.as_deref(), Some("1024m"));
    }

    #[test]
    fn detect_containment_returns_default_for_missing_file() {
        let sig = detect_containment(Path::new("/nonexistent/path/for/test.log"), "sweep_id=x");
        assert_eq!(sig, ContainmentSignal::default());
    }

    #[test]
    fn detect_containment_reads_a_real_file() {
        let dir = std::env::temp_dir().join(format!(
            "loom-containment-signal-test-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let log_path = dir.join("sweep-issue-7.log");
        std::fs::write(
            &log_path,
            "==== loom-daemon dispatch: sweep_id=sweep-issue-7-0 issue=7 ====\n\
             # LOOM_DISPATCH_MODE mode=container image=w cpus=4 memory=2048m\n",
        )
        .unwrap();
        let sig = detect_containment(&log_path, "sweep_id=sweep-issue-7-0");
        assert!(sig.containerized);
        assert_eq!(sig.cpus.as_deref(), Some("4"));
        assert_eq!(sig.memory.as_deref(), Some("2048m"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
