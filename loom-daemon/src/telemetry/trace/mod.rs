//! Portable, content-free trace records. No exporter or task runs at construction.

mod context;
pub mod journal;
mod span;
pub mod store;

pub use context::{derived_hex, instant, SpanId, TraceContext, TraceId};
pub mod provenance;
pub use span::{
    SpanEvent, SpanLink, SpanName, SpanRecord, SpanStatus, TraceAttributes, STATUS_MESSAGE,
};

#[cfg(test)]
mod tests;
