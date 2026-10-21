//! The `reaper.pass` span (#12461): one per worktree-reaper pass over one repo
//! and one worktree class, so the outcomes #12453 measured from host logs and
//! `du` reach the OTLP exporter.
//!
//! A pass is one of four, named by `loom.reaper.class` (`issue` / `pr`) and
//! `loom.reaper.phase`:
//!
//! - `reap`: the removal pass. `removed` / `failed` / `preserved` count
//!   worktrees, `bytes` is what the removed worktrees held.
//! - `reclaim`: the artifact-reclaim pass over the worktrees the removal pass
//!   kept. `removed` / `failed` count artifact directories, `preserved` the
//!   worktrees left untouched as a whole, `bytes` what was reclaimed.
//!
//! `loom.reaper.preserved.<reason>` splits `preserved` by the gate's fixed
//! category ([`PRESERVE_REASONS`]), never its free-text wording, so the series
//! count is bounded by that list. The repo is the workspace directory's name,
//! never a path.
//!
//! The span is an instant (the log seam has no start time), its own root, keyed
//! like every derived ID on the facts that name it, and carries the daemon's
//! version and full SHA through [`provenance::stamp`](crate::telemetry::trace::provenance::stamp).

use std::path::Path;

use chrono::{DateTime, Utc};

use crate::telemetry::trace::{SpanName, SpanRecord, SpanStatus, TraceAttributes, TraceContext};

/// The removal pass.
pub const PHASE_REAP: &str = "reap";
/// The artifact-reclaim pass.
pub const PHASE_RECLAIM: &str = "reclaim";

/// Every `loom.reaper.preserved.<reason>` category, plus `other` for a wording
/// no category claims. This list is the span-attribute allowlist's source.
pub const PRESERVE_REASONS: &[&str] = &[
    "in_use",
    "unmanaged",
    "editable_install",
    "issue_open",
    "pr_open",
    "pr_unknown",
    "not_merged",
    "grace",
    "uncommitted",
    "private_workspace",
    "other",
];

/// The fixed category for the reaper's free-text preserve wording.
#[must_use]
pub fn preserve_reason(wording: &str) -> &'static str {
    let w = wording.to_ascii_lowercase();
    // Grace first: "no PR found ..., grace period not passed" also names a PR.
    if w.contains("grace period") {
        "grace"
    } else if w.starts_with("uncommitted") {
        "uncommitted"
    } else if w.starts_with("editable") {
        "editable_install"
    } else if w.starts_with("no .loom-managed") {
        "unmanaged"
    } else if w.starts_with("private workspace") {
        "private_workspace"
    } else if w.starts_with("pr still open") {
        "pr_open"
    } else if w.starts_with("pr status unknown") {
        "pr_unknown"
    } else if w.starts_with("issue is ") {
        "issue_open"
    } else if w.contains("in use") || w.contains("in-use") || w.contains("claim") {
        "in_use"
    } else if w.contains("merged") || w.contains("unpushed") || w.contains("pushed") {
        "not_merged"
    } else {
        "other"
    }
}

/// One pass's outcome, as the span reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pass<'a> {
    /// `issue` / `pr`.
    pub class: &'a str,
    /// [`PHASE_REAP`] / [`PHASE_RECLAIM`].
    pub phase: &'a str,
    pub scanned: usize,
    pub removed: usize,
    pub failed: usize,
    /// The wording of each worktree the pass left alone.
    pub preserved: Vec<&'a str>,
    /// Bytes reclaimed (worktrees for `reap`, artifact directories for `reclaim`).
    pub bytes: u64,
}

/// The workspace directory's name, lowercased: the repo identity a span can
/// carry without a path or a `gh` lookup.
fn repo_name(repo_root: &Path) -> String {
    repo_root
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().to_ascii_lowercase())
}

/// The span for `p` over `repo_root`, at `at`.
#[must_use]
pub fn pass_span(repo_root: &Path, p: &Pass<'_>, at: DateTime<Utc>) -> SpanRecord {
    let name = SpanName::ReaperPass;
    let repo = repo_name(repo_root);
    let instant = crate::telemetry::trace::instant(at);
    let pid = std::process::id().to_string();
    let context = TraceContext::derived(
        name.as_str(),
        &[
            repo.as_str(),
            p.class,
            p.phase,
            instant.as_str(),
            pid.as_str(),
        ],
    );
    let mut attributes = TraceAttributes::new();
    attributes.insert("loom.repo".into(), repo);
    attributes.insert("loom.reaper.class".into(), p.class.to_string());
    attributes.insert("loom.reaper.phase".into(), p.phase.to_string());
    attributes.insert("loom.reaper.scanned".into(), p.scanned.to_string());
    attributes.insert("loom.reaper.removed".into(), p.removed.to_string());
    attributes.insert("loom.reaper.failed".into(), p.failed.to_string());
    attributes.insert("loom.reaper.preserved".into(), p.preserved.len().to_string());
    attributes.insert("loom.reaper.bytes".into(), p.bytes.to_string());
    for reason in PRESERVE_REASONS {
        let n = p
            .preserved
            .iter()
            .filter(|w| preserve_reason(w) == *reason)
            .count();
        if n > 0 {
            attributes.insert(format!("loom.reaper.preserved.{reason}"), n.to_string());
        }
    }
    crate::telemetry::trace::provenance::stamp(&mut attributes);
    SpanRecord {
        context,
        parent_span_id: None,
        name,
        started_at: at,
        ended_at: at,
        status: if p.failed == 0 {
            SpanStatus::Ok
        } else {
            SpanStatus::Error
        },
        attributes,
        events: Vec::new(),
        links: Vec::new(),
    }
}

/// Export one pass through the daemon's ops sink. Never fails; nothing is
/// built without a sink.
pub fn publish(repo_root: &Path, p: &Pass<'_>) {
    if super::spans_exported() {
        super::emit_span(pass_span(repo_root, p, Utc::now()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-16T14:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn pass<'a>(preserved: Vec<&'a str>, failed: usize) -> Pass<'a> {
        Pass {
            class: "issue",
            phase: PHASE_REAP,
            scanned: 7,
            removed: 2,
            failed,
            preserved,
            bytes: 4096,
        }
    }

    #[test]
    fn every_wording_the_reaper_writes_maps_to_a_fixed_category() {
        for (wording, want) in [
            ("grace period not passed (30s remaining)", "grace"),
            ("PR closed without merge, grace period not passed (9s remaining)", "grace"),
            ("no PR found for closed issue, grace period not passed (9s remaining)", "grace"),
            ("uncommitted changes", "uncommitted"),
            (
                "uncommitted changes (quarantine-stash failed or nothing to stash)",
                "uncommitted",
            ),
            ("editable pip install(s): foo", "editable_install"),
            ("no .loom-managed sentinel (user-provisioned)", "unmanaged"),
            ("private workspace ownership; host worktree is unrelated", "private_workspace"),
            ("PR still open", "pr_open"),
            ("PR status unknown", "pr_unknown"),
            ("issue is OPEN", "issue_open"),
            ("something nobody categorised", "other"),
        ] {
            assert_eq!(preserve_reason(wording), want, "{wording}");
            assert!(PRESERVE_REASONS.contains(&preserve_reason(wording)));
        }
    }

    #[test]
    fn the_span_carries_counts_reasons_bytes_and_provenance() {
        let preserved = vec!["PR still open", "PR still open", "uncommitted changes"];
        let span = pass_span(Path::new("/srv/Work/Loom"), &pass(preserved, 0), at());
        assert_eq!(span.name.as_str(), "reaper.pass");
        assert_eq!(span.status, SpanStatus::Ok);
        assert!(span.parent_span_id.is_none());
        assert!(span.validate().is_ok());
        let bounded = span.clone().bounded();
        for (key, want) in [
            ("loom.repo", "loom"),
            ("loom.reaper.class", "issue"),
            ("loom.reaper.phase", "reap"),
            ("loom.reaper.scanned", "7"),
            ("loom.reaper.removed", "2"),
            ("loom.reaper.failed", "0"),
            ("loom.reaper.preserved", "3"),
            ("loom.reaper.bytes", "4096"),
            ("loom.reaper.preserved.pr_open", "2"),
            ("loom.reaper.preserved.uncommitted", "1"),
        ] {
            assert_eq!(bounded.attributes.get(key).map(String::as_str), Some(want), "{key}");
            // `loom.repo` is admitted by `bounded_attributes` itself.
            assert!(
                key == "loom.repo" || crate::telemetry::ops::OPS_SPAN_ATTRIBUTE_KEYS.contains(&key),
                "{key}"
            );
        }
        assert!(!bounded
            .attributes
            .contains_key("loom.reaper.preserved.grace"));
        assert!(
            span.attributes
                .contains_key(crate::telemetry::trace::provenance::DAEMON_VERSION)
                && span
                    .attributes
                    .contains_key(crate::telemetry::trace::provenance::DAEMON_REVISION),
            "version and full SHA"
        );
        assert!(!span.attributes.values().any(|v| v.contains("/srv")), "no path");
        assert_eq!(
            span.context,
            pass_span(Path::new("/srv/Work/Loom"), &pass(vec![], 0), at()).context,
            "deterministic"
        );
    }

    #[test]
    fn a_failed_removal_marks_the_span_an_error() {
        let span = pass_span(Path::new("/r/x"), &pass(vec![], 1), at());
        assert_eq!(span.status, SpanStatus::Error);
    }

    #[test]
    fn publish_goes_to_the_ops_sink_when_one_is_registered() {
        let ((), captured) = super::super::capture::capture(|| {
            publish(Path::new("/r/x"), &pass(vec![], 0));
        });
        assert_eq!(captured.spans.len(), 1);
        assert_eq!(captured.spans[0].name.as_str(), "reaper.pass");
    }
}
