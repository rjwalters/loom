//! How much memory a relayed request will take once decoded, worked out from
//! its wire bytes **before** decoding it (Issue #10964).
//!
//! # Why
//!
//! A request body cap bounds wire bytes, not memory. An OTLP message element
//! can be two bytes on the wire (a tag and a zero length) and hundreds of
//! bytes decoded: an empty `Span` is 2 bytes in, 264 bytes of struct out. A
//! 4 MiB body of empty elements therefore decodes to over half a gigabyte
//! before any of the relay's own checks run. The receiver estimates first and
//! refuses (`413`) a request whose estimate exceeds its budget, so the
//! expensive decode only ever runs on input whose decoded size is already
//! known to be bounded.
//!
//! # Protobuf: exact structure, upper-bound sizes
//!
//! [`protobuf`] walks the wire format with the OTLP schema in hand — which
//! length-delimited fields are submessages, which are strings or bytes,
//! which are packed numbers — without allocating. Every element of a
//! repeated message field is charged the in-memory size of its struct
//! (`size_of`, so the charge tracks the generated types exactly); strings and
//! bytes are charged their length, plus a header when repeated; packed
//! numbers are charged eight bytes per element. Singular submessages live
//! inline in their parent's struct and cost nothing extra. Unknown fields,
//! which the decoder skips, cost nothing. Malformed wire data is an error,
//! exactly as the decoder would report.
//!
//! # JSON: a single scan, charged by context
//!
//! [`json`] tokenizes without building anything. A heap-allocated element is
//! exactly an element of a JSON array, so each array element is charged by
//! the field that array belongs to — a `spans` element the size of a `Span`,
//! an `attributes` element the size of a `KeyValue` — and a scalar or string
//! element the size of a `String`. An array under a key this table does not
//! know is charged the **largest** struct, so the bound never depends on the
//! table being complete; the decoder ignores unknown fields, so in practice
//! it costs nothing. String contents are charged once, as the body length.
//! (The OTLP JSON decoder accepts only the camelCase field names; there are
//! no aliases to evade the table with.)

use std::mem::size_of;

use opentelemetry_proto::tonic::common::v1::{AnyValue, EntityRef, KeyValue};
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use opentelemetry_proto::tonic::metrics::v1::{
    exponential_histogram_data_point::Buckets, summary_data_point::ValueAtQuantile, Exemplar,
    ExponentialHistogramDataPoint, HistogramDataPoint, Metric, NumberDataPoint, ResourceMetrics,
    ScopeMetrics, SummaryDataPoint,
};
use opentelemetry_proto::tonic::trace::v1::{span, ResourceSpans, ScopeSpans, Span};

use super::super::transport::Signal;

/// Nesting deeper than this is refused, matching `prost`'s own recursion
/// limit (the decoder would refuse it too).
const MAX_DEPTH: usize = 100;

/// The OTLP message types the walk distinguishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    LogsRequest,
    TracesRequest,
    MetricsRequest,
    ResourceLogs,
    ResourceSpans,
    ResourceMetrics,
    ScopeLogs,
    ScopeSpans,
    ScopeMetrics,
    LogRecord,
    Span,
    Event,
    Link,
    Status,
    Metric,
    /// `Gauge`, `Sum`, `Histogram`, `ExponentialHistogram`, `Summary`: each a
    /// `data_points` list at tag 1 of the given point kind.
    Points(Point),
    NumberPoint,
    HistogramPoint,
    ExponentialPoint,
    Buckets,
    SummaryPoint,
    Quantile,
    Exemplar,
    Resource,
    Scope,
    EntityRef,
    KeyValue,
    AnyValue,
    ArrayValue,
    KeyValueList,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Point {
    Number,
    Histogram,
    Exponential,
    Summary,
}

/// What a field is, for the walk.
enum Field {
    /// A submessage; `repeated` elements are heap-allocated in a `Vec`.
    Message { kind: Kind, repeated: bool },
    /// A string or bytes field; `repeated` adds a `String` header each.
    Text { repeated: bool },
    /// A repeated number, packed (one length-delimited run) or not.
    Numbers,
    /// Anything else: a scalar, or a field this schema does not know (the
    /// decoder skips unknown fields without allocating).
    Other,
}

fn message(kind: Kind) -> Field {
    Field::Message {
        kind,
        repeated: false,
    }
}

fn repeated(kind: Kind) -> Field {
    Field::Message {
        kind,
        repeated: true,
    }
}

const TEXT: Field = Field::Text { repeated: false };

/// The OTLP schema, as far as allocation is concerned.
fn field(kind: Kind, tag: u32) -> Field {
    use Kind::*;
    match (kind, tag) {
        (LogsRequest, 1) => repeated(ResourceLogs),
        (TracesRequest, 1) => repeated(ResourceSpans),
        (MetricsRequest, 1) => repeated(ResourceMetrics),
        (ResourceLogs | ResourceSpans | ResourceMetrics, 1) => message(Resource),
        (ResourceLogs, 2) => repeated(ScopeLogs),
        (ResourceSpans, 2) => repeated(ScopeSpans),
        (ResourceMetrics, 2) => repeated(ScopeMetrics),
        (ResourceLogs | ResourceSpans | ResourceMetrics, 3) => TEXT,
        (ScopeLogs | ScopeSpans | ScopeMetrics, 1) => message(Scope),
        (ScopeLogs, 2) => repeated(LogRecord),
        (ScopeSpans, 2) => repeated(Span),
        (ScopeMetrics, 2) => repeated(Metric),
        (ScopeLogs | ScopeSpans | ScopeMetrics, 3) => TEXT,
        (LogRecord, 3 | 9 | 10 | 12) => TEXT,
        (LogRecord, 5) => message(AnyValue),
        (LogRecord, 6) => repeated(KeyValue),
        (Span, 1..=5) => TEXT,
        (Span, 9) => repeated(KeyValue),
        (Span, 11) => repeated(Event),
        (Span, 13) => repeated(Link),
        (Span, 15) => message(Status),
        (Event, 2) => TEXT,
        (Event, 3) => repeated(KeyValue),
        (Link, 1..=3) => TEXT,
        (Link, 4) => repeated(KeyValue),
        (Status, 2) => TEXT,
        (Metric, 1..=3) => TEXT,
        (Metric, 5) => message(Points(Point::Number)),
        (Metric, 7) => message(Points(Point::Number)),
        (Metric, 9) => message(Points(Point::Histogram)),
        (Metric, 10) => message(Points(Point::Exponential)),
        (Metric, 11) => message(Points(Point::Summary)),
        (Metric, 12) => repeated(KeyValue),
        (Points(Point::Number), 1) => repeated(NumberPoint),
        (Points(Point::Histogram), 1) => repeated(HistogramPoint),
        (Points(Point::Exponential), 1) => repeated(ExponentialPoint),
        (Points(Point::Summary), 1) => repeated(SummaryPoint),
        (NumberPoint, 7) => repeated(KeyValue),
        (NumberPoint, 5) => repeated(Exemplar),
        (HistogramPoint, 9) => repeated(KeyValue),
        (HistogramPoint, 6 | 7) => Field::Numbers,
        (HistogramPoint, 8) => repeated(Exemplar),
        (ExponentialPoint, 1) => repeated(KeyValue),
        (ExponentialPoint, 8 | 9) => message(Buckets),
        (ExponentialPoint, 11) => repeated(Exemplar),
        (Buckets, 2) => Field::Numbers,
        (SummaryPoint, 7) => repeated(KeyValue),
        (SummaryPoint, 6) => repeated(Quantile),
        (Exemplar, 7) => repeated(KeyValue),
        (Exemplar, 4 | 5) => TEXT,
        (Resource, 1) => repeated(KeyValue),
        (Resource, 3) => repeated(EntityRef),
        (Scope, 1 | 2) => TEXT,
        (Scope, 3) => repeated(KeyValue),
        (EntityRef, 1 | 2) => TEXT,
        (EntityRef, 3 | 4) => Field::Text { repeated: true },
        (KeyValue, 1) => TEXT,
        (KeyValue, 2) => message(AnyValue),
        (AnyValue, 1 | 7) => TEXT,
        (AnyValue, 5) => message(ArrayValue),
        (AnyValue, 6) => message(KeyValueList),
        (ArrayValue, 1) => repeated(AnyValue),
        (KeyValueList, 1) => repeated(KeyValue),
        _ => Field::Other,
    }
}

/// The in-memory size of one element of `kind`.
fn struct_size(kind: Kind) -> usize {
    use Kind::*;
    match kind {
        LogsRequest | TracesRequest | MetricsRequest => size_of::<Vec<u8>>(),
        ResourceLogs => size_of::<self::ResourceLogs>(),
        ResourceSpans => size_of::<self::ResourceSpans>(),
        ResourceMetrics => size_of::<self::ResourceMetrics>(),
        ScopeLogs => size_of::<self::ScopeLogs>(),
        ScopeSpans => size_of::<self::ScopeSpans>(),
        ScopeMetrics => size_of::<self::ScopeMetrics>(),
        LogRecord => size_of::<self::LogRecord>(),
        Span => size_of::<self::Span>(),
        Event => size_of::<span::Event>(),
        Link => size_of::<span::Link>(),
        Metric => size_of::<self::Metric>(),
        NumberPoint => size_of::<NumberDataPoint>(),
        HistogramPoint => size_of::<HistogramDataPoint>(),
        ExponentialPoint => size_of::<ExponentialHistogramDataPoint>(),
        SummaryPoint => size_of::<SummaryDataPoint>(),
        Quantile => size_of::<ValueAtQuantile>(),
        Exemplar => size_of::<self::Exemplar>(),
        EntityRef => size_of::<self::EntityRef>(),
        KeyValue => size_of::<self::KeyValue>(),
        AnyValue => size_of::<self::AnyValue>(),
        // Only ever singular, so inline in a parent already charged.
        Status | Points(_) | Buckets | Resource | Scope | ArrayValue | KeyValueList => 0,
    }
}

/// The largest struct any OTLP request element can decode to: what the JSON
/// bound charges per object.
fn largest_struct() -> usize {
    [
        size_of::<ResourceLogs>(),
        size_of::<ResourceSpans>(),
        size_of::<ResourceMetrics>(),
        size_of::<ScopeLogs>(),
        size_of::<ScopeSpans>(),
        size_of::<ScopeMetrics>(),
        size_of::<LogRecord>(),
        size_of::<Span>(),
        size_of::<span::Event>(),
        size_of::<span::Link>(),
        size_of::<Metric>(),
        size_of::<NumberDataPoint>(),
        size_of::<HistogramDataPoint>(),
        size_of::<ExponentialHistogramDataPoint>(),
        size_of::<SummaryDataPoint>(),
        size_of::<Exemplar>(),
        size_of::<EntityRef>(),
        size_of::<KeyValue>(),
        size_of::<AnyValue>(),
        size_of::<Buckets>(),
    ]
    .into_iter()
    .max()
    .unwrap_or(0)
}

/// Decoded bytes estimated for a `signal` request, or `Err` when the wire
/// data is malformed (as the decoder would also find it).
pub(super) fn protobuf(signal: Signal, body: &[u8]) -> Result<usize, ()> {
    let root = match signal {
        Signal::Logs => Kind::LogsRequest,
        Signal::Traces => Kind::TracesRequest,
        Signal::Metrics => Kind::MetricsRequest,
    };
    walk(root, body, 0)
}

/// What one element of the JSON array under `key` decodes to.
fn json_element(key: &[u8]) -> usize {
    match key {
        b"resourceLogs" => size_of::<ResourceLogs>(),
        b"resourceSpans" => size_of::<ResourceSpans>(),
        b"resourceMetrics" => size_of::<ResourceMetrics>(),
        b"scopeLogs" => size_of::<ScopeLogs>(),
        b"scopeSpans" => size_of::<ScopeSpans>(),
        b"scopeMetrics" => size_of::<ScopeMetrics>(),
        b"logRecords" => size_of::<LogRecord>(),
        b"spans" => size_of::<Span>(),
        b"events" => size_of::<span::Event>(),
        b"links" => size_of::<span::Link>(),
        b"metrics" => size_of::<Metric>(),
        b"dataPoints" => size_of::<NumberDataPoint>()
            .max(size_of::<HistogramDataPoint>())
            .max(size_of::<ExponentialHistogramDataPoint>())
            .max(size_of::<SummaryDataPoint>()),
        b"exemplars" => size_of::<Exemplar>(),
        b"quantileValues" => size_of::<ValueAtQuantile>(),
        b"entityRefs" => size_of::<EntityRef>(),
        b"attributes" | b"filteredAttributes" | b"metadata" | b"values" => {
            size_of::<KeyValue>().max(size_of::<AnyValue>())
        }
        _ => largest_struct(),
    }
}

/// A JSON request's decoded-size upper bound. Tolerant of malformed input
/// (the decoder rejects that); never panics.
pub(super) fn json(body: &[u8]) -> usize {
    /// Keys longer than any OTLP field name are "unknown".
    const MAX_KEY: usize = 32;
    enum Frame {
        /// An object; `key` is the last key read, `expect_key` whether the
        /// next string is a key.
        Object { key: Vec<u8>, expect_key: bool },
        /// An array whose elements are charged `element` each; `fresh`
        /// until its current element has been charged.
        Array { element: usize, fresh: bool },
    }
    let mut stack: Vec<Frame> = Vec::new();
    let mut total = body.len();
    let mut at = 0usize;
    // Charge the element starting here, if the innermost frame is an array
    // waiting for one. `object` selects the struct charge over a scalar's.
    let charge = |stack: &mut Vec<Frame>, total: &mut usize, object: bool| {
        if let Some(Frame::Array { element, fresh }) = stack.last_mut() {
            if *fresh {
                *fresh = false;
                let cost = if object {
                    *element
                } else {
                    size_of::<String>()
                };
                *total = total.saturating_add(cost);
            }
        }
    };
    while at < body.len() {
        let byte = body[at];
        match byte {
            b'"' => {
                let start = at + 1;
                let mut end = start;
                let mut escaped = false;
                while end < body.len() {
                    let c = body[end];
                    if escaped {
                        escaped = false;
                    } else if c == b'\\' {
                        escaped = true;
                    } else if c == b'"' {
                        break;
                    }
                    end += 1;
                }
                let text = body.get(start..end).unwrap_or_default();
                match stack.last_mut() {
                    Some(Frame::Object { key, expect_key }) if *expect_key => {
                        key.clear();
                        if text.len() <= MAX_KEY {
                            key.extend_from_slice(text);
                        }
                        *expect_key = false;
                    }
                    _ => charge(&mut stack, &mut total, false),
                }
                at = end + 1;
                continue;
            }
            b'{' | b'[' if stack.len() >= MAX_DEPTH => {
                // Deeper than the decoder will go (it refuses past 128), and
                // the scan's own stack must stay bounded too.
                return usize::MAX;
            }
            b'{' => {
                charge(&mut stack, &mut total, true);
                stack.push(Frame::Object {
                    key: Vec::new(),
                    expect_key: true,
                });
            }
            b'[' => {
                charge(&mut stack, &mut total, true);
                let element = match stack.last() {
                    Some(Frame::Object { key, .. }) => json_element(key),
                    _ => largest_struct(),
                };
                stack.push(Frame::Array {
                    element,
                    fresh: true,
                });
            }
            b'}' | b']' => {
                stack.pop();
            }
            b',' => match stack.last_mut() {
                Some(Frame::Object { expect_key, .. }) => *expect_key = true,
                Some(Frame::Array { fresh, .. }) => *fresh = true,
                None => {}
            },
            b':' | b' ' | b'\t' | b'\n' | b'\r' => {}
            // The first byte of a number, `true`, `false` or `null`.
            _ => charge(&mut stack, &mut total, false),
        }
        at += 1;
    }
    total
}

fn varint(buffer: &[u8], at: &mut usize) -> Result<u64, ()> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *buffer.get(*at).ok_or(())?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(())
}

/// The bytes the fields of one `kind` message in `buffer` add.
fn walk(kind: Kind, buffer: &[u8], depth: usize) -> Result<usize, ()> {
    if depth > MAX_DEPTH {
        return Err(());
    }
    let mut total = 0usize;
    let mut at = 0usize;
    while at < buffer.len() {
        let key = varint(buffer, &mut at)?;
        let tag = u32::try_from(key >> 3).map_err(|_| ())?;
        if tag == 0 {
            return Err(());
        }
        match key & 7 {
            // Varint.
            0 => {
                varint(buffer, &mut at)?;
                if matches!(field(kind, tag), Field::Numbers) {
                    total = total.saturating_add(8);
                }
            }
            // Fixed 64 / fixed 32.
            1 | 5 => {
                let width = if key & 7 == 1 { 8 } else { 4 };
                at = at
                    .checked_add(width)
                    .filter(|end| *end <= buffer.len())
                    .ok_or(())?;
                if matches!(field(kind, tag), Field::Numbers) {
                    total = total.saturating_add(8);
                }
            }
            // Length-delimited.
            2 => {
                let length = usize::try_from(varint(buffer, &mut at)?).map_err(|_| ())?;
                let end = at
                    .checked_add(length)
                    .filter(|end| *end <= buffer.len())
                    .ok_or(())?;
                let payload = buffer.get(at..end).ok_or(())?;
                at = end;
                let added = match field(kind, tag) {
                    Field::Message {
                        kind: child,
                        repeated,
                    } => {
                        let element = if repeated { struct_size(child) } else { 0 };
                        element.saturating_add(walk(child, payload, depth + 1)?)
                    }
                    Field::Text { repeated } => {
                        let header = if repeated { size_of::<String>() } else { 0 };
                        header.saturating_add(length)
                    }
                    // Packed: at most one element per byte, eight bytes each.
                    Field::Numbers => length.saturating_mul(8),
                    Field::Other => 0,
                };
                total = total.saturating_add(added);
            }
            // Groups are not part of OTLP; the decoder refuses them too.
            _ => return Err(()),
        }
    }
    Ok(total)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "estimate_tests.rs"]
mod tests;
