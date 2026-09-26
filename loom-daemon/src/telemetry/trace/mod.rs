//! Portable, content-free trace records. No exporter or task runs at construction.

mod context;
pub mod journal;
mod span;
pub mod store;
mod story;

pub use context::{derived_hex, SpanId, TraceContext, TraceId};
pub use span::{SpanEvent, SpanLink, SpanName, SpanRecord, SpanStatus, TraceAttributes};
pub use story::story_context;

#[cfg(test)]
mod tests;
