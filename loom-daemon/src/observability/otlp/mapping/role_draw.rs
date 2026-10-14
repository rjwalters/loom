//! `loom.role_draw.*` attributes on `role_tick.outcome` (#11726): the weighted
//! role draw that chose the tick's tap, flattened so SigNoz can group a
//! stage's results by the candidate that ran it (`loom.role` ×
//! `loom.role_draw.pick` × `loom.result`) and by attempt. The full record —
//! every roll and the seed inputs — stays on the durable JSONL record.

use opentelemetry_proto::tonic::common::v1::KeyValue;

use super::{kv_int, kv_string};
use crate::runtime_preference::role_draw::RoleDrawRecord;

/// The attributes for `draw`; empty when no draw decided the tick.
pub(super) fn attributes(draw: Option<&RoleDrawRecord>) -> Vec<KeyValue> {
    let Some(draw) = draw else {
        return Vec::new();
    };
    let table = draw
        .table
        .iter()
        .map(|w| format!("{}={}", w.candidate, w.weight))
        .collect::<Vec<_>>()
        .join(",");
    let mut out = vec![
        kv_int("loom.role_draw.attempt", i64::from(draw.attempt)),
        kv_int("loom.role_draw.table_attempt", i64::from(draw.table_attempt)),
        kv_string("loom.role_draw.layer", draw.layer.as_str()),
        kv_string("loom.role_draw.table", table),
        kv_string("loom.role_draw.seed", draw.seed.clone()),
        kv_int("loom.role_draw.steps", i64::try_from(draw.steps.len()).unwrap_or(i64::MAX)),
        kv_string("loom.role_draw.pick", draw.pick.clone().unwrap_or_else(|| "none".into())),
    ];
    if let Some(step) = draw.winning_step() {
        out.push(kv_int("loom.role_draw.sum", i64::try_from(step.total).unwrap_or(i64::MAX)));
        out.push(kv_int("loom.role_draw.roll", i64::try_from(step.roll).unwrap_or(i64::MAX)));
    }
    if let Some(model) = &draw.model {
        out.push(kv_string("loom.role_draw.model", model.clone()));
    }
    out
}
