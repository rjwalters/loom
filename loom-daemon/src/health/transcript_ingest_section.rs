//! The `transcript_ingest` health section (issue #8477): whether the
//! background transcript-token-ingest pass ([`crate::activity::transcript_ingest`])
//! is enabled, and whether it is keeping up with transcripts actually on
//! disk.
//!
//! Split into its own file because `health.rs` sits at its
//! `.loom/docs/file-size-policy.md` ratchet: assessment logic that grows goes
//! in a sibling module and the parent keeps only the `mod` line plus the
//! re-export, exactly as [`super::calibration_section`] already does.
//!
//! Unlike [`super::assess_limit_calibration`] or [`super::assess_observability`],
//! this section is **not** suppressed when the underlying feature is off —
//! that is precisely the incident #8477 exists to stop being silent: Claude
//! Code deletes a transcript `cleanupPeriodDays` (default 30) after it was
//! last touched, so a host that opted out of ingestion (deliberately or by a
//! stuck/crashed pass) is losing fleet token/cost history right now, which
//! is worth a non-green line rather than no line at all.

use super::{HealthInputs, HealthSection, Verdict};
use crate::activity::transcript_ingest::STALE_THRESHOLD_HOURS;

/// Assess the collected [`IngestHealthStatus`] into the `transcript_ingest`
/// section. Pure: the collector (`cli/health.rs`) has already done the I/O
/// ([`crate::activity::transcript_ingest::collect_health_status`]), so this
/// mapping to verdict/summary is unit-testable without a real activity
/// database or `~/.claude/projects` tree.
///
/// `None` only when the collector did not run at all (a fixture that never
/// set [`HealthInputs::transcript_ingest`]) — the real collector always
/// populates it, so a live `loom-daemon health` run always renders this
/// section.
#[must_use]
pub fn assess_transcript_ingest(inputs: &HealthInputs) -> Option<HealthSection> {
    const KEY: &str = "transcript_ingest";
    let status = inputs.transcript_ingest.as_ref()?;

    if !status.enabled {
        return Some(HealthSection {
            key: KEY,
            verdict: Verdict::Degraded,
            summary: "transcript token/cost ingestion is OFF (LOOM_TRANSCRIPT_INGEST=0 or \
                       autonomous.transcriptIngest.enabled=false) — fleet cost history on this \
                       host will not survive Claude Code's 30-day transcript retention"
                .to_string(),
            detail: serde_json::json!({ "enabled": false }),
        });
    }

    let stale = matches!(
        (status.newest_ingested_age_hours, status.newest_transcript_age_hours),
        (Some(ingested), Some(transcript))
            if ingested > STALE_THRESHOLD_HOURS && ingested > transcript
    );

    let detail = serde_json::json!({
        "enabled": true,
        "newestIngestedAgeHours": status.newest_ingested_age_hours,
        "newestTranscriptAgeHours": status.newest_transcript_age_hours,
    });

    if stale {
        return Some(HealthSection {
            key: KEY,
            verdict: Verdict::Degraded,
            summary: format!(
                "transcript ingestion looks stuck — newest ledger entry is {:.1}h old while a \
                 newer transcript exists on disk",
                status.newest_ingested_age_hours.unwrap_or_default()
            ),
            detail,
        });
    }

    Some(HealthSection {
        key: KEY,
        verdict: Verdict::Green,
        summary: status.newest_ingested_age_hours.map_or_else(
            || "transcript ingestion enabled, nothing ingested yet".to_string(),
            |age| format!("transcript ingestion healthy (newest ledger entry {age:.1}h old)"),
        ),
        detail,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::activity::transcript_ingest::IngestHealthStatus;
    use crate::health::HealthInputs;

    fn inputs_with(transcript_ingest: Option<IngestHealthStatus>) -> HealthInputs {
        HealthInputs {
            transcript_ingest,
            ..Default::default()
        }
    }

    #[test]
    fn not_collected_renders_no_section() {
        assert!(assess_transcript_ingest(&inputs_with(None)).is_none());
        let report = crate::health::assess(&inputs_with(None));
        assert!(report.sections.iter().all(|s| s.key != "transcript_ingest"));
    }

    #[test]
    fn disabled_renders_degraded_even_though_it_is_a_deliberate_opt_out() {
        let section = assess_transcript_ingest(&inputs_with(Some(IngestHealthStatus {
            enabled: false,
            newest_ingested_age_hours: None,
            newest_transcript_age_hours: None,
        })))
        .expect("a disabled host still renders a section (#8477: silence is the incident)");
        assert_eq!(section.verdict, Verdict::Degraded);
        assert!(section.summary.contains("OFF"), "{}", section.summary);
        assert!(section.summary.contains("30-day"), "{}", section.summary);
    }

    #[test]
    fn enabled_with_no_history_yet_is_green() {
        let section = assess_transcript_ingest(&inputs_with(Some(IngestHealthStatus {
            enabled: true,
            newest_ingested_age_hours: None,
            newest_transcript_age_hours: Some(0.1),
        })))
        .unwrap();
        assert_eq!(section.verdict, Verdict::Green);
        assert!(section.summary.contains("nothing ingested yet"), "{}", section.summary);
    }

    #[test]
    fn enabled_and_fresh_is_green_and_names_the_age() {
        let section = assess_transcript_ingest(&inputs_with(Some(IngestHealthStatus {
            enabled: true,
            newest_ingested_age_hours: Some(0.25),
            newest_transcript_age_hours: Some(0.1),
        })))
        .unwrap();
        assert_eq!(section.verdict, Verdict::Green);
        assert!(section.summary.contains("0.2"), "{}", section.summary);
    }

    #[test]
    fn a_stale_ledger_behind_a_newer_transcript_is_degraded() {
        let section = assess_transcript_ingest(&inputs_with(Some(IngestHealthStatus {
            enabled: true,
            newest_ingested_age_hours: Some(12.0),
            newest_transcript_age_hours: Some(0.5),
        })))
        .unwrap();
        assert_eq!(section.verdict, Verdict::Degraded);
        assert!(section.summary.contains("stuck"), "{}", section.summary);
        assert!(section.summary.contains("12.0"), "{}", section.summary);
    }

    /// A ledger entry older than the threshold is not itself evidence of a
    /// stuck pass when nothing newer has arrived since — e.g. a quiet host
    /// with no fresh transcripts at all.
    #[test]
    fn an_old_ledger_entry_with_no_newer_transcript_is_not_flagged_stale() {
        let section = assess_transcript_ingest(&inputs_with(Some(IngestHealthStatus {
            enabled: true,
            newest_ingested_age_hours: Some(48.0),
            newest_transcript_age_hours: Some(48.0),
        })))
        .unwrap();
        assert_eq!(section.verdict, Verdict::Green);
    }

    #[test]
    fn assess_renders_the_section_and_promotes_overall_when_disabled() {
        let inputs = inputs_with(Some(IngestHealthStatus {
            enabled: false,
            newest_ingested_age_hours: None,
            newest_transcript_age_hours: None,
        }));
        let report = crate::health::assess(&inputs);
        let section = report
            .sections
            .iter()
            .find(|s| s.key == "transcript_ingest")
            .expect("assess must render the transcript_ingest section");
        assert_eq!(section.verdict, Verdict::Degraded);
        assert_eq!(report.overall, Verdict::Degraded);
    }

    #[test]
    fn a_green_reading_leaves_the_roll_up_unpromoted() {
        let inputs = inputs_with(Some(IngestHealthStatus {
            enabled: true,
            newest_ingested_age_hours: Some(0.1),
            newest_transcript_age_hours: Some(0.1),
        }));
        let report = crate::health::assess(&inputs);
        let section = report
            .sections
            .iter()
            .find(|s| s.key == "transcript_ingest")
            .expect("assess must render the transcript_ingest section");
        assert_eq!(section.verdict, Verdict::Green);
        assert_ne!(report.overall, Verdict::Degraded);
    }
}
