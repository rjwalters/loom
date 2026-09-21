//! Portable, content-free trace records. No exporter or task runs at construction.

mod context;
mod span;
pub mod store;

pub use context::{SpanId, TraceContext, TraceId};
pub use span::{SpanEvent, SpanLink, SpanName, SpanRecord, SpanStatus, TraceAttributes};

#[cfg(test)]
mod tests;
