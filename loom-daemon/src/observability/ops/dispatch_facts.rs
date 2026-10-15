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
//!   `min(configured, disk headroom, RAM headroom)`; when it dropped, nothing
//!   said which term did it. `loom.dispatch.cap_limiter` names the binding
//!   term and `loom.dispatch.cap_{configured,disk,ram}` carry the terms, also
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
        if let Some(ram) = cap.ram {
            out.push(("loom.dispatch.cap_ram".into(), ram.to_string()));
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
    out
}

/// The tick's extra metric points: one `loom.dispatch.cap_term{kind}` gauge
/// per measured term, and `loom.dispatch.pr_open_prefiltered` when non-zero.
#[must_use]
pub fn tick_points(report: &TickReport, cap: Option<CapView>) -> Vec<MetricPoint> {
    let mut points = Vec::new();
    if let Some(cap) = cap {
        let terms = [
            ("configured", Some(cap.configured)),
            ("disk", cap.disk),
            ("ram", cap.ram),
        ];
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
        let cap = CapView::from_terms(12, 4, 9);
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
        assert_eq!(get("loom.dispatch.cap_ram"), Some("9"));
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
        assert_eq!(term("ram"), Some(MetricValue::Int(9)));
        assert_eq!(term("configured"), Some(MetricValue::Int(12)));
        let prefiltered = points
            .iter()
            .find(|p| p.name == MetricName::DispatchPrOpenPrefiltered)
            .unwrap();
        assert_eq!(prefiltered.value, MetricValue::Int(2));
    }

    #[test]
    fn an_unmeasured_term_is_absent_not_zero() {
        let report = TickReport::default();
        let cap = CapView::from_terms(8, usize::MAX, 6);
        let attrs = tick_attributes(&report, Some(cap), None);
        assert!(attrs.iter().all(|(k, _)| k != "loom.dispatch.cap_disk"));
        assert!(attrs
            .iter()
            .any(|(k, v)| k == "loom.dispatch.cap_limiter" && v == "ram"));
        let points = tick_points(&report, Some(cap));
        assert_eq!(points.len(), 2, "configured and ram only; no prefiltered counter at zero");
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
