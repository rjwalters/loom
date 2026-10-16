//! Work-finder tick facts exported beside [`super::dispatch`] (#11858).
//!
//! Each closes a question SigNoz could not answer from the tick span alone:
//!
//! - **Which repo a hold names.** `loom.dispatch.halt_causes` lists the
//!   distinct causes and no repo, so a long fleet-wide `main_red` hold could
//!   not be attributed. `loom.dispatch.halted_repos` names each held repo
//!   with its cause, from the work finder's halt clock
//!   ([`crate::work_finder::halt_cause::since::current`]).
//! - **Why the cap is what it is.** `loom.dispatch.max_concurrent` is
//!   `min(configured, disk headroom)` (the RAM term was removed in #12330);
//!   when it dropped, nothing said which term did it. `loom.dispatch.cap_limiter`
//!   names the binding term and `loom.dispatch.cap_{configured,disk}` carry the terms, also
//!   exported as the `loom.dispatch.cap_term{kind}` gauge.
//! - **Pre-filter skips apart from refusals.**
//!   `loom.dispatch.decisions{reason=pr_open}` counts both the open-PR
//!   pre-filter's skips and `dispatch()`'s own refusals. The pre-filter's share
//!   is `loom.dispatch.pr_open_prefiltered` (a span attribute and a delta
//!   counter).
//! - **An admission's repo before the collector resolved it.** The admission
//!   spans read the collector's repo cache, which is empty until its first
//!   pass, so early admissions carried no `loom.repo`. [`fallback_repo_ref`]
//!   names the repo from the checkout's `origin` remote (the memoised read the
//!   tick's listings already make), never a local path.
//!
//! - **How long the step gather took, and what it covered** (#12074).
//!   `loom.dispatch.gather_ms`, `loom.dispatch.repos_gathered`,
//!   `loom.dispatch.repos_skipped` and `loom.dispatch.gather_truncated`, so a
//!   gather the tick deadline cut short is visible rather than silent.
//!
//! Everything here reads what the tick already computed; it adds no forge
//! call.

use std::path::Path;

use crate::telemetry::ops::{MetricName, MetricPoint};
use crate::telemetry::queue_snapshot::QueueRepoRef;
use crate::telemetry::RepoVisibility;
use crate::types::CapView;
use crate::work_finder::halt_cause::since::RootHalt;
use crate::work_finder::TickReport;

/// Span attribute values are bounded to this many characters at export
/// (`trace::span::bounded_attributes`); `loom.dispatch.halted_repos` stops
/// adding entries before it.
const MAX_ATTRIBUTE_CHARS: usize = 256;

fn int(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// `owner/repo=cause` for each held root, sorted, comma-joined and cut at a
/// whole entry within [`MAX_ATTRIBUTE_CHARS`]. A root whose repo is unknown
/// is skipped (never a local path); a hold no cause names reads `unknown`.
/// `None` when no root is held.
#[must_use]
pub fn halted_repos(
    halts: &[RootHalt],
    repo_of: impl Fn(&Path) -> Option<String>,
) -> Option<String> {
    let mut entries: Vec<String> = halts
        .iter()
        .filter_map(|h| {
            let repo = repo_of(&h.root)?;
            let cause = h.cause.map_or("unknown", |c| c.as_str());
            Some(format!("{repo}={cause}"))
        })
        .collect();
    entries.sort();
    entries.dedup();
    let mut out = String::new();
    for entry in entries {
        let extra = entry.len() + usize::from(!out.is_empty());
        if out.len() + extra > MAX_ATTRIBUTE_CHARS {
            break;
        }
        if !out.is_empty() {
            out.push(',');
        }
        out.push_str(&entry);
    }
    (!out.is_empty()).then_some(out)
}

/// The tick span's extra attributes: the cap terms and limiter (when the tick
/// recorded them), the pre-filter's open-PR skips, and the held repos.
#[must_use]
pub fn tick_attributes(
    report: &TickReport,
    cap: Option<CapView>,
    halted_repos: Option<String>,
) -> Vec<(String, String)> {
    let mut out = vec![(
        "loom.dispatch.pr_open_prefiltered".to_string(),
        report.pr_open_prefiltered.to_string(),
    )];
    if let Some(cap) = cap {
        out.push(("loom.dispatch.cap_limiter".into(), cap.limiter().as_str().into()));
        out.push(("loom.dispatch.cap_configured".into(), cap.configured.to_string()));
        if let Some(disk) = cap.disk {
            out.push(("loom.dispatch.cap_disk".into(), disk.to_string()));
        }
    }
    if let Some(repos) = halted_repos {
        out.push(("loom.dispatch.halted_repos".into(), repos));
    }
    if let Some(g) = report.gather {
        out.push(("loom.dispatch.gather_ms".into(), g.gather_ms.to_string()));
        out.push(("loom.dispatch.repos_gathered".into(), g.repos_gathered.to_string()));
        out.push(("loom.dispatch.repos_skipped".into(), g.repos_skipped.to_string()));
        out.push(("loom.dispatch.gather_truncated".into(), g.truncated.to_string()));
    }
    if let Some(p) = report.phases {
        out.extend(phase_attributes(&p));
    }
    if let Some(d) = &report.discovery {
        out.extend(discovery_attributes(d));
    }
    if let Some(hold) = &report.spawn_hold {
        out.push(("loom.dispatch.spawn_hold".into(), hold.clone()));
    }
    out
}

/// The tick span's discovery facts (#12306, #12311). The worklist keys are
/// absent when the chain has no worklist tier, never a 0. Pure.
#[must_use]
pub fn discovery_attributes(d: &crate::discovery::SnapshotStats) -> Vec<(String, String)> {
    let mut out = vec![
        ("loom.discovery.backend".to_string(), d.backend.to_string()),
        ("loom.discovery.tier".into(), d.tier.to_string()),
        ("loom.discovery.items".into(), d.items.to_string()),
        ("loom.discovery.forge_reads".into(), d.discovery_forge_reads.to_string()),
    ];
    if let Some(reason) = &d.fallback_reason {
        out.push(("loom.discovery.fallback_reason".into(), reason.clone()));
    }
    if !d.fallback_repos.is_empty() {
        out.push(("loom.discovery.fallback_repos".into(), d.fallback_repos.join(",")));
    }
    if let Some(outcome) = d.worklist_outcome {
        out.push(("loom.discovery.worklist.outcome".into(), outcome.to_string()));
        if let Some(class) = d.worklist_error_class {
            out.push(("loom.discovery.worklist.error_class".into(), class.to_string()));
        }
        if let Some(ms) = d.worklist_latency_ms {
            out.push(("loom.discovery.worklist.latency_ms".into(), ms.to_string()));
        }
        out.push(("loom.discovery.repos.requested".into(), d.repos.to_string()));
        out.push(("loom.discovery.repos.ok".into(), d.repos_ok.to_string()));
        out.push(("loom.discovery.repos.unknown".into(), d.repos_unknown.to_string()));
        out.push(("loom.discovery.repos.backfilling".into(), d.repos_backfilling.to_string()));
        out.push(("loom.discovery.repos.error".into(), d.repos_error.to_string()));
        out.push(("loom.discovery.stale_repos".into(), d.stale_repos.to_string()));
    }
    if let Some(age) = d.oldest_watermark_age_secs {
        out.push(("loom.discovery.oldest_watermark_age_secs".into(), age.to_string()));
    }
    out
}

/// The tick span's per-phase timing (#12256): where a tick's wall time went.
/// Pure.
#[must_use]
pub fn phase_attributes(
    p: &crate::step_dispatch::gather_budget::TickPhases,
) -> Vec<(String, String)> {
    vec![
        ("loom.dispatch.phase.listing_ms".into(), p.listing_ms.to_string()),
        ("loom.dispatch.phase.filter_ms".into(), p.filter_ms.to_string()),
        ("loom.dispatch.phase.draw_ms".into(), p.draw_ms.to_string()),
        ("loom.dispatch.phase.spawn_ms".into(), p.spawn_ms.to_string()),
        ("loom.dispatch.spawn_attempts".into(), p.spawn_attempts.to_string()),
        ("loom.dispatch.gather_concurrency".into(), p.gather_concurrency.to_string()),
        ("loom.dispatch.spawn_concurrency".into(), p.spawn_concurrency.to_string()),
        (
            "loom.dispatch.phase.spawn_resolve_ms".into(),
            p.spawn_parts.resolve_ms.to_string(),
        ),
        ("loom.dispatch.phase.spawn_claim_ms".into(), p.spawn_parts.claim_ms.to_string()),
        (
            "loom.dispatch.phase.spawn_launch_ms".into(),
            p.spawn_parts.launch_ms.to_string(),
        ),
        (
            "loom.dispatch.phase.spawn_account_ms".into(),
            p.spawn_parts.account_ms.to_string(),
        ),
    ]
}

/// The tick's extra metric points: one `loom.dispatch.cap_term{kind}` gauge
/// per measured term, and `loom.dispatch.pr_open_prefiltered` when non-zero.
#[must_use]
pub fn tick_points(report: &TickReport, cap: Option<CapView>) -> Vec<MetricPoint> {
    let mut points = Vec::new();
    if let Some(cap) = cap {
        let terms = [("configured", Some(cap.configured)), ("disk", cap.disk)];
        for (kind, value) in terms {
            if let Some(value) = value {
                points.push(
                    MetricPoint::int(MetricName::DispatchCapTerm, int(value)).label("kind", kind),
                );
            }
        }
    }
    if report.pr_open_prefiltered > 0 {
        points.push(MetricPoint::int(
            MetricName::DispatchPrOpenPrefiltered,
            int(report.pr_open_prefiltered),
        ));
    }
    points
}

/// The cap terms of the tick just published, when it recorded them.
#[must_use]
pub fn published_cap() -> Option<CapView> {
    crate::work_finder::last_tick_summary().and_then(|s| s.cap)
}

/// `root`'s repo from its `origin` remote (memoised; no forge call), with the
/// cached visibility, else `private` (fail closed). `None` when the remote is
/// not a GitHub `owner/repo`.
#[must_use]
pub fn fallback_repo_ref(root: &Path) -> Option<QueueRepoRef> {
    let (host, repo) = crate::forge_etag_store::remote_identity(root)?;
    if host != "github.com" || !crate::agent_gh::classify::is_slug(&repo) {
        return None;
    }
    let visibility =
        crate::telemetry::visibility::cached_visibility(&repo).unwrap_or(RepoVisibility::Private);
    Some(QueueRepoRef { repo, visibility })
}

/// The held repos of the work finder's last tick, each named through the
/// collector's cache, else [`fallback_repo_ref`].
#[must_use]
pub fn current_halted_repos() -> Option<String> {
    let halts = crate::work_finder::halt_cause::since::current()?;
    halted_repos(&halts, |root: &Path| {
        let key = root.display().to_string();
        super::super::repo_ref::cached_repo_ref(&key)
            .or_else(|| fallback_repo_ref(root))
            .map(|r| r.repo)
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::telemetry::ops::MetricValue;

    #[test]
    fn discovery_facts_are_attributes_the_export_keeps() {
        let mirror = crate::discovery::SnapshotStats {
            backend: "mirror",
            tier: "gateway",
            fallback_reason: Some("transport".into()),
            fallback_repos: vec!["acme/a".into(), "acme/b".into()],
            worklist_outcome: Some("error"),
            worklist_error_class: Some("transport"),
            worklist_latency_ms: Some(40),
            not_modified: Some(false),
            repos: 31,
            repos_ok: 28,
            repos_unknown: 1,
            repos_backfilling: 1,
            repos_error: 1,
            items: 1_555,
            oldest_watermark_age_secs: Some(90),
            stale_repos: 2,
            discovery_forge_reads: 4,
        };
        let report = TickReport {
            discovery: Some(mirror),
            ..TickReport::default()
        };
        let attrs = tick_attributes(&report, None, None);
        let get = |k: &str| attrs.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str());
        assert_eq!(get("loom.discovery.backend"), Some("mirror"));
        assert_eq!(get("loom.discovery.tier"), Some("gateway"));
        assert_eq!(get("loom.discovery.fallback_reason"), Some("transport"));
        assert_eq!(get("loom.discovery.fallback_repos"), Some("acme/a,acme/b"));
        assert_eq!(get("loom.discovery.worklist.outcome"), Some("error"));
        assert_eq!(get("loom.discovery.worklist.error_class"), Some("transport"));
        assert_eq!(get("loom.discovery.worklist.latency_ms"), Some("40"));
        assert_eq!(get("loom.discovery.repos.requested"), Some("31"));
        assert_eq!(get("loom.discovery.repos.ok"), Some("28"));
        assert_eq!(get("loom.discovery.repos.unknown"), Some("1"));
        assert_eq!(get("loom.discovery.repos.backfilling"), Some("1"));
        assert_eq!(get("loom.discovery.repos.error"), Some("1"));
        assert_eq!(get("loom.discovery.items"), Some("1555"));
        assert_eq!(get("loom.discovery.oldest_watermark_age_secs"), Some("90"));
        assert_eq!(get("loom.discovery.forge_reads"), Some("4"));
        for (key, _) in &attrs {
            assert!(
                crate::telemetry::ops::OPS_SPAN_ATTRIBUTE_KEYS.contains(&key.as_str()),
                "{key} is not on the export allowlist"
            );
        }
        // Direct mode has no worklist: those keys are absent, never 0.
        let direct = TickReport {
            discovery: Some(crate::discovery::SnapshotStats {
                backend: "direct",
                repos: 3,
                discovery_forge_reads: 6,
                ..crate::discovery::SnapshotStats::default()
            }),
            ..TickReport::default()
        };
        let attrs = tick_attributes(&direct, None, None);
        let get = |k: &str| attrs.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str());
        assert_eq!(get("loom.discovery.forge_reads"), Some("6"));
        assert_eq!(get("loom.discovery.worklist.outcome"), None);
        assert_eq!(get("loom.discovery.worklist.latency_ms"), None);
        assert_eq!(get("loom.discovery.repos.requested"), None);
    }

    #[test]
    fn tick_phases_are_attributes_the_export_keeps() {
        let report = TickReport {
            phases: Some(crate::step_dispatch::gather_budget::TickPhases {
                listing_ms: 12_000,
                filter_ms: 300,
                draw_ms: 40,
                spawn_ms: 9_000,
                spawn_attempts: 5,
                gather_concurrency: 8,
                spawn_concurrency: 4,
                spawn_parts: crate::step_dispatch::gather_budget::SpawnParts {
                    resolve_ms: 700,
                    claim_ms: 6_000,
                    launch_ms: 200,
                    account_ms: 11_000,
                },
            }),
            gather: Some(crate::step_dispatch::gather_budget::GatherStats::default()),
            spawn_hold: Some("rate_limit_breaker".to_string()),
            ..TickReport::default()
        };
        let attrs = tick_attributes(&report, None, None);
        let get = |k: &str| attrs.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str());
        assert_eq!(get("loom.dispatch.phase.listing_ms"), Some("12000"));
        assert_eq!(get("loom.dispatch.phase.spawn_ms"), Some("9000"));
        assert_eq!(get("loom.dispatch.spawn_attempts"), Some("5"));
        assert_eq!(get("loom.dispatch.gather_concurrency"), Some("8"));
        assert_eq!(get("loom.dispatch.spawn_concurrency"), Some("4"));
        assert_eq!(get("loom.dispatch.spawn_hold"), Some("rate_limit_breaker"));
        assert_eq!(get("loom.dispatch.phase.spawn_claim_ms"), Some("6000"));
        assert_eq!(get("loom.dispatch.phase.spawn_account_ms"), Some("11000"));
        // Every tick attribute survives export (#12256: the #12074 gather
        // keys were emitted but missing from the allowlist).
        for (key, _) in &attrs {
            assert!(
                crate::telemetry::ops::OPS_SPAN_ATTRIBUTE_KEYS.contains(&key.as_str()),
                "{key} is not on the export allowlist"
            );
        }
        // No phases, no phase attributes.
        let bare = tick_attributes(&TickReport::default(), None, None);
        assert!(!bare
            .iter()
            .any(|(k, _)| k.starts_with("loom.dispatch.phase.")));
    }
    use crate::work_finder::halt_cause::HaltCause;

    fn halt(root: &str, cause: Option<HaltCause>) -> RootHalt {
        RootHalt {
            root: std::path::PathBuf::from(root),
            cause,
            since: chrono::Utc::now(),
            since_lower_bound: false,
        }
    }

    fn repo_of(root: &Path) -> Option<String> {
        match root.to_str()? {
            "/w/a" => Some("o/a".into()),
            "/w/b" => Some("o/b".into()),
            _ => None,
        }
    }

    #[test]
    fn halted_repos_name_each_held_repo_with_its_cause() {
        let halts = [
            halt("/w/b", Some(HaltCause::MaintainOnly)),
            halt("/w/a", Some(HaltCause::MainRed)),
            halt("/w/unresolved", Some(HaltCause::MainRed)),
        ];
        assert_eq!(
            halted_repos(&halts, repo_of).as_deref(),
            Some("o/a=main_red,o/b=maintain_only")
        );
        assert_eq!(halted_repos(&[halt("/w/a", None)], repo_of).as_deref(), Some("o/a=unknown"));
        assert_eq!(halted_repos(&[], repo_of), None);
    }

    #[test]
    fn halted_repos_stay_within_the_attribute_bound() {
        let halts: Vec<RootHalt> = (0..40)
            .map(|i| halt(&format!("/w/{i}"), Some(HaltCause::Drain)))
            .collect();
        let named = halted_repos(&halts, |root: &Path| {
            Some(format!("owner/repo-{}", root.file_name()?.to_str()?))
        })
        .unwrap();
        assert!(named.len() <= MAX_ATTRIBUTE_CHARS, "{}", named.len());
        assert!(named.split(',').all(|e| e.ends_with("=drain")), "{named}");
    }

    #[test]
    fn tick_facts_name_the_binding_cap_term_and_prefiltered_skips() {
        let report = TickReport {
            skipped_pr_open: 3,
            pr_open_prefiltered: 2,
            ..TickReport::default()
        };
        let cap = CapView::from_terms(12, 4);
        let attrs = tick_attributes(&report, Some(cap), Some("o/a=main_red".into()));
        let get = |k: &str| {
            attrs
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("loom.dispatch.cap_limiter"), Some("disk"));
        assert_eq!(get("loom.dispatch.cap_configured"), Some("12"));
        assert_eq!(get("loom.dispatch.cap_disk"), Some("4"));
        assert_eq!(get("loom.dispatch.cap_ram"), None, "no RAM term since #12330");
        assert_eq!(get("loom.dispatch.pr_open_prefiltered"), Some("2"));
        assert_eq!(get("loom.dispatch.halted_repos"), Some("o/a=main_red"));
        for (key, _) in &attrs {
            assert!(
                crate::telemetry::ops::OPS_SPAN_ATTRIBUTE_KEYS.contains(&key.as_str()),
                "{key}"
            );
        }

        let points = tick_points(&report, Some(cap));
        let term = |kind: &str| {
            points
                .iter()
                .find(|p| {
                    p.name == MetricName::DispatchCapTerm
                        && p.labels.get("kind").map(String::as_str) == Some(kind)
                })
                .map(|p| p.value)
        };
        assert_eq!(term("disk"), Some(MetricValue::Int(4)));
        assert_eq!(term("ram"), None);
        assert_eq!(term("configured"), Some(MetricValue::Int(12)));
        let prefiltered = points
            .iter()
            .find(|p| p.name == MetricName::DispatchPrOpenPrefiltered)
            .unwrap();
        assert_eq!(prefiltered.value, MetricValue::Int(2));
    }

    /// #12330: the binding axis is `disk` or `configured`; there is no RAM
    /// term, so `loom.dispatch.cap_ram` is never exported.
    #[test]
    fn tick_facts_name_the_binding_axis_without_a_ram_term() {
        let limiter = |c, d| CapView::from_terms(c, d).limiter().as_str();
        assert_eq!(limiter(20, 3), "disk");
        assert_eq!(limiter(20, 30), "configured");
        assert_eq!(limiter(20, 20), "configured", "a tie is the operator's");
        assert_eq!(limiter(20, usize::MAX), "configured", "unmeasured disk never binds");
        let attrs = tick_attributes(&TickReport::default(), Some(CapView::from_terms(20, 3)), None);
        assert!(attrs.iter().all(|(k, _)| !k.contains("ram")), "{attrs:?}");
        assert!(attrs
            .iter()
            .any(|(k, v)| k == "loom.dispatch.cap_limiter" && v == "disk"));
    }

    #[test]
    fn an_unmeasured_term_is_absent_not_zero() {
        let report = TickReport::default();
        let cap = CapView::from_terms(8, usize::MAX);
        let attrs = tick_attributes(&report, Some(cap), None);
        assert!(attrs.iter().all(|(k, _)| k != "loom.dispatch.cap_disk"));
        assert!(attrs
            .iter()
            .any(|(k, v)| k == "loom.dispatch.cap_limiter" && v == "configured"));
        let points = tick_points(&report, Some(cap));
        assert_eq!(points.len(), 1, "configured only; no prefiltered counter at zero");
        assert!(tick_attributes(&report, None, None).len() == 1);
    }

    /// #12074: the step gather's figures ride the tick span.
    #[test]
    fn the_gather_figures_are_tick_attributes() {
        let report = TickReport {
            gather: Some(crate::step_dispatch::gather_budget::GatherStats {
                gather_ms: 91_000,
                repos_gathered: 40,
                repos_skipped: 22,
                truncated: true,
            }),
            ..TickReport::default()
        };
        let attrs = tick_attributes(&report, None, None);
        let get = |k: &str| attrs.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str());
        assert_eq!(get("loom.dispatch.gather_ms"), Some("91000"));
        assert_eq!(get("loom.dispatch.repos_gathered"), Some("40"));
        assert_eq!(get("loom.dispatch.repos_skipped"), Some("22"));
        assert_eq!(get("loom.dispatch.gather_truncated"), Some("true"));
    }
}
