//! The exported `pick.decision` is a **bounded summary** (#12974, parent
//! #12973): the full record (every candidate and draw) goes to the payload
//! archive ([`crate::observability::pick_payload_archive`]) and the log keeps
//! only the header, the totals and counts by reason, under
//! [`SUMMARY_MAX_BYTES`]. A summary is flagged by `payload_ref`, the `tick_id`
//! the archived payload is keyed by.
//!
//! The wire change is additive: `schema_version` stays
//! [`super::pick_decision::PICK_DECISION_SCHEMA_VERSION`], the lists a summary
//! empties are already optional or empty-able, and the new fields
//! (`payload_ref`, `skipped_counts`, `exclusion_counts`) are skipped when unset.

use super::pick_decision::{PickDecisionRecord, PickItemDraw};

/// The ceiling on a summary's serialized body (the ~16 KB ingest cap of the
/// operator ruling in #12973).
pub const SUMMARY_MAX_BYTES: usize = 16 * 1024;

/// Most `acted[]` entries a summary keeps; the rest are in the payload.
const MAX_ACTED: usize = 20;

fn json_len(record: &PickDecisionRecord) -> usize {
    serde_json::to_vec(record).map_or(usize::MAX, |v| v.len())
}

impl PickDecisionRecord {
    /// The bounded summary of this (unchunked) record, flagged with
    /// `payload_ref`. It never carries `candidates[]`, `skipped[]`,
    /// `item_draw.pools[]` / `draws[]` or `workspace_draw.draws[]`; totals
    /// (`candidates_total`, `draws_total`) stay, and the skipped and excluded
    /// entries are counted by reason. Serialized size is at most
    /// [`SUMMARY_MAX_BYTES`]: if the free-form remainder (`acted`, `lanes`, ...)
    /// would not fit it is dropped, last-resort fields first.
    #[must_use]
    pub fn summarize(&self, payload_ref: &str) -> PickDecisionRecord {
        let mut s = self.clone();
        s.chunk_index = 0;
        s.chunk_count = 1;
        s.payload_ref = Some(payload_ref.to_string());
        s.candidates = Vec::new();
        for skip in std::mem::take(&mut s.skipped) {
            *s.skipped_counts
                .entry(skip.reason.as_str().to_string())
                .or_default() += 1;
        }
        if let Some(draw) = s.workspace_draw.as_mut() {
            draw.draws = Vec::new();
        }
        if let Some(draw) = s.item_draw.as_mut() {
            for e in std::mem::take(&mut draw.excluded) {
                *s.exclusion_counts
                    .entry(e.skip_reason.as_str().to_string())
                    .or_default() += e.count;
            }
            *draw = bare_item_draw(draw);
        }
        s.acted.truncate(MAX_ACTED);
        // Shed the unbounded remainder only if the cap is still exceeded.
        if json_len(&s) > SUMMARY_MAX_BYTES {
            s.very_important_demoted = Vec::new();
            s.lanes = Vec::new();
        }
        if json_len(&s) > SUMMARY_MAX_BYTES {
            s.acted = Vec::new();
        }
        s
    }
}

/// `draw` without its lists: the seed inputs, exponents and `draws_total`.
fn bare_item_draw(draw: &PickItemDraw) -> PickItemDraw {
    PickItemDraw {
        seed_scheme: draw.seed_scheme.clone(),
        seed_minute: draw.seed_minute.clone(),
        seed_host: draw.seed_host.clone(),
        tick_counter: draw.tick_counter.clone(),
        draws_total: draw.draws_total,
        backlog_exponent: draw.backlog_exponent.clone(),
        busy_exponent: draw.busy_exponent.clone(),
        repos: Vec::new(),
        closures: Vec::new(),
        excluded: Vec::new(),
        pools: Vec::new(),
        draws: Vec::new(),
        weight_overflow: Vec::new(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "pick_summary_tests.rs"]
pub(crate) mod tests;
