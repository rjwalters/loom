//! Per-agent-scope memory telemetry (#12241): when an agent scope ends, one
//! `scope.memory` record (applied `MemoryMax`, its source, the peak, the
//! `memory.events` `max` / `oom_kill` counters, whether the peak was clipped)
//! and, when it had OOM kills, a `loom.agent_scope.oom_kills` point by `repo`
//! and `role`. Called from `crate::ram_peaks`'s tick, which owns the sampling.
//! Never shells out: the forge slug comes from the collector's cache, or the
//! checkout's memoised origin.

use std::path::Path;

use chrono::{DateTime, Utc};

use crate::ram_peaks::{classify_limit_from_env, scope_role, EndedScope, LimitSource};
use crate::telemetry::kinds::scope_memory::ScopeMemoryRecord;
use crate::telemetry::ops::{MetricName, MetricPoint};
use crate::telemetry::provenance::Provenance;
use crate::telemetry::RepoVisibility;
use crate::telemetry::TelemetryRecord;

/// The record for `e`. `repo` is the resolved `(slug, visibility)`, if any.
#[must_use]
pub fn record(
    e: &EndedScope,
    source: LimitSource,
    repo: Option<(String, RepoVisibility)>,
    at: DateTime<Utc>,
) -> ScopeMemoryRecord {
    let (repo, repo_visibility) = match repo {
        Some((slug, vis)) => (
            Some(slug),
            Some(
                match vis {
                    RepoVisibility::Public => "public",
                    RepoVisibility::Private => "private",
                }
                .to_string(),
            ),
        ),
        None => (None, None),
    };
    ScopeMemoryRecord {
        scope: e.scope.clone(),
        workspace: e.repo.clone(),
        repo,
        repo_visibility,
        role: scope_role(&e.scope, e.role.as_deref(), e.issue),
        issue: e.issue,
        limit_bytes: e.limit_bytes,
        limit_source: source,
        peak_bytes: e.peak_bytes,
        events_max: e.events.max,
        oom_kills: e.events.oom_kill,
        clipped: e.clipped(),
        observed_at: at,
        loom: Provenance::current(),
    }
}

/// The OOM-kill counter point for `r`, labelled by `repo` (the workspace key,
/// like `loom.agent_scope.peak_memory_bytes`) and `role`; `None` without kills.
#[must_use]
pub fn oom_kill_point(r: &ScopeMemoryRecord) -> Option<MetricPoint> {
    (r.oom_kills > 0).then(|| {
        MetricPoint::int(
            MetricName::AgentScopeOomKills,
            i64::try_from(r.oom_kills).unwrap_or(i64::MAX),
        )
        .label("repo", r.workspace.clone())
        .label("role", r.role.clone())
    })
}

/// Emit `e`'s record and OOM-kill point. `root` is the workspace the scope
/// ran in, when known.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn emit(e: &EndedScope, root: Option<&Path>) {
    let repo = root.and_then(|root| {
        super::super::repo_ref::cached_repo_ref(&root.display().to_string())
            .or_else(|| super::dispatch_facts::fallback_repo_ref(root))
            .map(|r| (r.repo, r.visibility))
    });
    let r = record(e, classify_limit_from_env(e.limit_bytes), repo, Utc::now());
    if let Some(point) = oom_kill_point(&r) {
        super::emit_metrics(vec![point]);
    }
    if super::records_exported() {
        super::emit_record(TelemetryRecord::ScopeMemory(r));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::ram_peaks::MemoryEvents;

    const GIB: u64 = 1024 * 1024 * 1024;

    fn ended(limit: Option<u64>, peak: u64, max: u64, oom: u64) -> EndedScope {
        EndedScope {
            scope: "loom-agent-step-builder-42-1.scope".into(),
            repo: "loom".into(),
            issue: None,
            role: None,
            peak_bytes: peak,
            limit_bytes: limit,
            events: MemoryEvents { max, oom_kill: oom },
        }
    }

    #[test]
    fn a_killed_capped_scope_is_clipped_and_counted() {
        let e = ended(Some(8 * GIB), 8 * GIB, 12, 4);
        let r = record(
            &e,
            LimitSource::History,
            Some(("rjwalters/loom".into(), RepoVisibility::Public)),
            Utc::now(),
        );
        assert!(r.clipped);
        assert_eq!(r.role, "builder");
        assert_eq!(r.repo.as_deref(), Some("rjwalters/loom"));
        assert_eq!(r.repo_visibility.as_deref(), Some("public"));
        assert_eq!((r.events_max, r.oom_kills), (12, 4));
        let p = oom_kill_point(&r).unwrap();
        assert_eq!(p.name, MetricName::AgentScopeOomKills);
        assert_eq!(p.labels["repo"], "loom");
        assert_eq!(p.labels["role"], "builder");
    }

    #[test]
    fn an_uncapped_scope_is_not_clipped_and_has_no_counter_point() {
        let r = record(&ended(None, 3 * GIB, 0, 0), LimitSource::None, None, Utc::now());
        assert!(!r.clipped);
        assert_eq!(r.limit_bytes, None);
        assert!(r.repo.is_none() && r.repo_visibility.is_none());
        assert!(oom_kill_point(&r).is_none());
    }

    #[test]
    fn emit_exports_the_record_and_the_kill_point() {
        let ((), cap) = super::super::capture::capture(|| {
            emit(&ended(Some(4 * GIB), 4 * GIB, 3, 2), None);
        });
        assert!(cap
            .records
            .iter()
            .any(|r| matches!(r, TelemetryRecord::ScopeMemory(m) if m.oom_kills == 2)));
        assert!(cap
            .metrics
            .iter()
            .any(|p| p.name == MetricName::AgentScopeOomKills));
    }
}
