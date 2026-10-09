//! `eta.stage_outcome` emission (#10929). Each stage boundary the tracker
//! journals becomes one record, linked to the estimates open for the item at
//! that moment. The records are built from rows already written, so this
//! needs no forge read.
//!
//! Only the ETA authority emits (#10498). Both callers already sit behind the
//! authority gate: the bus path returns at `authority::journal_only`, and the
//! pass runs only on the authority. [`build`] checks the flag again, so a
//! future caller cannot bypass it.

use chrono::{DateTime, Utc};

use super::{authority, sink, JournalEntry, Provenance, QueueSink, Resolved, TelemetryRecord};
use crate::eta::score::EstimateSummary;
use crate::telemetry::kinds::eta_stage_outcome::{from_journal, EtaStageOutcomeRecord};

/// This batch's records: `pending` is the tracker's open set after the batch,
/// and `resolved` holds the estimates the same batch scored, which were open
/// until it ran. Empty on a non-authority host.
pub(super) fn build(
    rows: &[JournalEntry],
    pending: &[EstimateSummary],
    resolved: &[Resolved],
    now: DateTime<Utc>,
) -> Vec<EtaStageOutcomeRecord> {
    if !authority::active() {
        return Vec::new();
    }
    let open = pending.iter().chain(resolved.iter().map(|r| &r.estimate));
    from_journal(rows, open, now, &Provenance::current())
}

/// Offer `records` to the OTLP sink, in each item's story trace. Returns how
/// many were offered (or, in a dry run, logged).
pub(super) fn emit(records: Vec<EtaStageOutcomeRecord>, host_id: &str, dry_run: bool) -> usize {
    offer(records, host_id, dry_run, sink())
}

fn offer(
    records: Vec<EtaStageOutcomeRecord>,
    host_id: &str,
    dry_run: bool,
    sink: Option<&dyn QueueSink>,
) -> usize {
    let mut offered = 0;
    for record in records {
        let line = format!(
            "eta.stage_outcome repo={} issue={} stage={} exit={} dwell_sec={:?}",
            record.repo,
            record.issue,
            record.stage,
            record.exit.as_str(),
            record.dwell_sec
        );
        if !record.has_provenance() {
            log::warn!("eta: dropped {line}: invalid provenance");
            continue;
        }
        offered += 1;
        if dry_run {
            log::info!("eta: would emit {line}");
        } else if let Some(sink) = sink {
            log::debug!("eta: emit {line}");
            let (repo_id, issue) = (record.repo_id, record.issue);
            let record = TelemetryRecord::EtaStageOutcome(record);
            sink.offer(super::envelope(host_id, record, repo_id, issue));
        }
    }
    offered
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::TelemetryEnvelope;
    use super::*;
    use crate::telemetry::kinds::eta_stage_outcome::StageExit;
    use chrono::TimeZone;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Capture(Mutex<Vec<TelemetryEnvelope>>);

    impl QueueSink for Capture {
        fn offer(&self, envelope: TelemetryEnvelope) {
            self.0.lock().unwrap().push(envelope);
        }

        fn offer_durable(&self, envelope: TelemetryEnvelope) -> std::io::Result<()> {
            self.offer(envelope);
            Ok(())
        }
    }

    fn record(issue: u32, revision: &str) -> EtaStageOutcomeRecord {
        let at = Utc.with_ymd_and_hms(2026, 10, 8, 12, 0, 0).unwrap();
        EtaStageOutcomeRecord {
            repo: "rjwalters/loom".to_string(),
            repo_id: Some(1),
            issue,
            pr_number: None,
            stage: crate::eta::Stage::ReviewWait,
            entered_at: None,
            left_at: at,
            dwell_sec: None,
            exit: StageExit::Pass,
            next_stage: Some(crate::eta::Stage::MergeWait),
            event: "label.transition".to_string(),
            observed_at: at,
            resolution_sec: Some(300),
            open_estimates: 0,
            estimate_ids: Vec::new(),
            loom: Provenance {
                version: "0.19.800".to_string(),
                revision: revision.to_string(),
                tree_state: "clean".to_string(),
                complete: true,
            },
        }
    }

    const SHA: &str = "9d8e226ce0123456789abcdef0123456789abcde";

    #[test]
    fn valid_records_reach_the_sink_in_the_story_trace() {
        let sink = Capture::default();
        let records = vec![record(1, SHA), record(2, "not-a-sha")];
        assert_eq!(offer(records, "host-a", false, Some(&sink)), 1);
        let sent = sink.0.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert!(matches!(&sent[0].record, TelemetryRecord::EtaStageOutcome(r) if r.issue == 1));
        assert!(sent[0].trace_context.is_some(), "a known repo id joins the story trace");
    }

    #[test]
    fn a_dry_run_counts_but_offers_nothing() {
        let sink = Capture::default();
        assert_eq!(offer(vec![record(1, SHA)], "host-a", true, Some(&sink)), 1);
        assert!(sink.0.lock().unwrap().is_empty());
    }
}
