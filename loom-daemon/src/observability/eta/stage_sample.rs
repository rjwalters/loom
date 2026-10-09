//! `eta.stage_sample` emission (#10756): every stage-journal row, offered as
//! it is appended, on every host. The rows are already built, so this needs
//! no forge read.
//!
//! Unlike `eta.stage_outcome`, this is **not** behind the ETA authority gate:
//! a non-authority host's journal (its own sweeps' bus events) is exactly what
//! was missing from SigNoz.

use super::{sink, JournalEntry, QueueSink, TelemetryEnvelope, TelemetryRecord};

/// Offer `rows` to the OTLP sink. Returns how many were offered (or, in a dry
/// run, logged).
pub(super) fn emit(rows: &[JournalEntry], host_id: &str, dry_run: bool) -> usize {
    offer(rows, host_id, dry_run, sink())
}

fn offer(
    rows: &[JournalEntry],
    host_id: &str,
    dry_run: bool,
    sink: Option<&dyn QueueSink>,
) -> usize {
    let mut offered = 0;
    for row in rows {
        let line = format!(
            "eta.stage_sample event={} repo={} issue={:?} pr={:?}",
            row.event, row.repo, row.issue, row.pr_number
        );
        if !row.has_provenance() {
            log::warn!("eta: dropped {line}: invalid provenance");
            continue;
        }
        offered += 1;
        if dry_run {
            log::info!("eta: would emit {line}");
        } else if let Some(sink) = sink {
            log::debug!("eta: emit {line}");
            let record = TelemetryRecord::EtaStageSample(row.clone());
            sink.offer(TelemetryEnvelope::new(host_id, record));
        }
    }
    offered
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::Provenance;
    use super::*;
    use chrono::{TimeZone, Utc};
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

    fn row(event: &str, revision: &str) -> JournalEntry {
        let at = Utc.with_ymd_and_hms(2026, 10, 8, 12, 0, 0).unwrap();
        let loom = Provenance {
            version: "0.19.800".to_string(),
            revision: revision.to_string(),
            tree_state: "clean".to_string(),
            complete: true,
        };
        JournalEntry::new(event, "rjwalters/loom", at, &loom)
    }

    const SHA: &str = "9d8e226ce0123456789abcdef0123456789abcde";

    #[test]
    fn every_row_reaches_the_sink_verbatim_and_unproven_ones_never_do() {
        let sink = Capture::default();
        let rows = vec![
            row("label.first_seen", SHA),
            row("sweep.phase", SHA),
            row("label.transition", "not-a-sha"),
        ];
        assert_eq!(offer(&rows, "host-b", false, Some(&sink)), 2);
        let sent = sink.0.lock().unwrap();
        assert_eq!(sent.len(), 2);
        assert!(sent.iter().all(|e| e.host_id == "host-b"));
        assert!(matches!(&sent[0].record, TelemetryRecord::EtaStageSample(r) if *r == rows[0]));
        assert!(matches!(&sent[1].record, TelemetryRecord::EtaStageSample(r) if *r == rows[1]));
    }

    #[test]
    fn a_dry_run_counts_but_offers_nothing() {
        let sink = Capture::default();
        assert_eq!(offer(&[row("label.transition", SHA)], "host-a", true, Some(&sink)), 1);
        assert!(sink.0.lock().unwrap().is_empty());
    }
}
