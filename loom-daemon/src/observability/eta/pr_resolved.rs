//! `pr.resolved` emission (#10519): one record per PR the pass saw resolved,
//! built from the journal rows the pass already wrote. No forge read.

use chrono::{DateTime, Utc};

use super::{sink, JournalEntry, Provenance, QueueSink, TelemetryEnvelope, TelemetryRecord};
use crate::telemetry::kinds::pr_resolved::{from_journal, PrResolvedRecord};

/// Offer this pass's `pr.resolved` records to the OTLP sink. Returns how many
/// were offered (or, in a dry run, logged).
pub(super) fn emit(
    rows: &[JournalEntry],
    host_id: &str,
    dry_run: bool,
    now: DateTime<Utc>,
    resolution_sec: i64,
) -> usize {
    let records = from_journal(rows, now, resolution_sec, &Provenance::current());
    offer(records, host_id, dry_run, sink())
}

fn offer(
    records: Vec<PrResolvedRecord>,
    host_id: &str,
    dry_run: bool,
    sink: Option<&dyn QueueSink>,
) -> usize {
    let mut offered = 0;
    for record in records {
        let line = format!(
            "pr.resolved repo={} pr={} state={}",
            record.repo,
            record.pr_number,
            record.state.as_str()
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
            sink.offer(TelemetryEnvelope::new(host_id, TelemetryRecord::PrResolved(record)));
        }
    }
    offered
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::telemetry::kinds::pr_resolved::PrResolution;
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

    fn record(pr_number: u32, revision: &str) -> PrResolvedRecord {
        let at = Utc.with_ymd_and_hms(2026, 10, 6, 12, 0, 0).unwrap();
        PrResolvedRecord {
            repo: "rjwalters/loom".to_string(),
            pr_number,
            issue: None,
            state: PrResolution::Merged,
            resolved_at: at,
            observed_at: at,
            resolution_sec: 0,
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
    fn valid_records_reach_the_sink_and_unproven_ones_never_do() {
        let sink = Capture::default();
        let records = vec![record(1, SHA), record(2, "not-a-sha")];
        assert_eq!(offer(records, "host-a", false, Some(&sink)), 1);
        let sent = sink.0.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert!(matches!(&sent[0].record, TelemetryRecord::PrResolved(r) if r.pr_number == 1));
    }

    #[test]
    fn a_dry_run_counts_but_offers_nothing() {
        let sink = Capture::default();
        assert_eq!(offer(vec![record(1, SHA)], "host-a", true, Some(&sink)), 1);
        assert!(sink.0.lock().unwrap().is_empty());
    }
}
