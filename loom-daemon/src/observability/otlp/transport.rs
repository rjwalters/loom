//! OTLP/HTTP response policy. Receiver text is never copied into diagnostics.
use super::super::exporter::{ExportError, SignalCounts};
use serde::Serialize;

const MAX_RESPONSE_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Signal {
    Logs,
    Metrics,
    // The trace foundation wires this into the envelope mapper separately.
    #[allow(dead_code)]
    Traces,
}
impl Signal {
    pub(super) fn unit(self) -> &'static str {
        match self {
            Self::Logs => "log_records",
            Self::Metrics => "metric_data_points",
            Self::Traces => "spans",
        }
    }
    fn rejected_field(self) -> &'static str {
        match self {
            Self::Logs => "rejectedLogRecords",
            Self::Metrics => "rejectedDataPoints",
            Self::Traces => "rejectedSpans",
        }
    }
}

pub(super) struct ResponseOutcome {
    pub counts: SignalCounts,
    pub retry: bool,
    pub error: Option<ExportError>,
}
impl ResponseOutcome {
    fn failure(items: u64, retry: bool, detail: &str) -> Self {
        Self {
            counts: if retry {
                SignalCounts {
                    retry_scheduled: items,
                    ..Default::default()
                }
            } else {
                SignalCounts {
                    dropped: items,
                    ..Default::default()
                }
            },
            retry,
            error: Some(ExportError::Transport(detail.to_string())),
        }
    }
}

pub(super) async fn post<T: Serialize + Sync>(
    client: &reqwest::Client,
    endpoint: &str,
    key: &str,
    signal: Signal,
    items: u64,
    body: &T,
) -> ResponseOutcome {
    let result = client
        .post(endpoint)
        .bearer_auth(key)
        .json(body)
        .send()
        .await;
    let mut response = match result {
        Ok(response) => response,
        Err(error) => {
            return ResponseOutcome::failure(
                items,
                true,
                if error.is_timeout() {
                    "OTLP request timed out"
                } else {
                    "OTLP request transport failed"
                },
            )
        }
    };
    let status = response.status().as_u16();
    if status != 200 {
        // OTLP explicitly enumerates retriable HTTP responses. Other statuses,
        // including 400 and 500, must not permanently pin a poison batch.
        return ResponseOutcome::failure(
            items,
            matches!(status, 429 | 502 | 503 | 504),
            &format!("OTLP HTTP {status}"),
        );
    }
    let is_json = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"));
    if !is_json {
        return ResponseOutcome::failure(
            items,
            false,
            "OTLP response is not application/json; acceptance unknown",
        );
    }
    let mut bytes = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) if chunk.len() <= MAX_RESPONSE_BYTES.saturating_sub(bytes.len()) => {
                bytes.extend_from_slice(&chunk)
            }
            Ok(Some(_)) => {
                return ResponseOutcome::failure(
                    items,
                    false,
                    "OTLP response exceeds 4096 bytes; acceptance unknown",
                )
            }
            Ok(None) => break,
            // The receiver may already have accepted the request. We cannot
            // claim success; retry preserves the documented at-least-once policy.
            Err(_) => {
                return ResponseOutcome::failure(
                    items,
                    true,
                    "OTLP response read failed; acceptance unknown",
                )
            }
        }
    }
    parse_response(signal, items, &bytes)
}

fn parse_response(signal: Signal, items: u64, bytes: &[u8]) -> ResponseOutcome {
    let invalid =
        || ResponseOutcome::failure(items, false, "invalid OTLP response; acceptance unknown");
    let Ok(serde_json::Value::Object(body)) = serde_json::from_slice(bytes) else {
        return invalid();
    };
    // ProtoJSON null means unset, including message and scalar fields. OTLP's
    // JSON deviations do not change that rule.
    let Some(partial) = body.get("partialSuccess").filter(|value| !value.is_null()) else {
        return ResponseOutcome {
            counts: SignalCounts {
                accepted: items,
                ..Default::default()
            },
            retry: false,
            error: None,
        };
    };
    let Some(partial) = partial.as_object() else {
        return invalid();
    };
    let rejected = match partial.get(signal.rejected_field()) {
        None | Some(serde_json::Value::Null) => 0,
        Some(serde_json::Value::String(s)) => match integer_count(s) {
            Some(n) => n,
            None => return invalid(),
        },
        Some(serde_json::Value::Number(value)) => match integer_count(&value.to_string()) {
            Some(n) => n,
            None => return invalid(),
        },
        Some(_) => return invalid(),
    };
    if rejected > items {
        return invalid();
    }
    let warning = match partial.get("errorMessage") {
        None | Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::String(s)) => !s.is_empty(),
        Some(_) => return invalid(),
    };
    ResponseOutcome {
        counts: SignalCounts {
            accepted: items - rejected,
            rejected,
            warnings: u64::from(warning),
            ..Default::default()
        },
        retry: false,
        error: (rejected > 0).then(|| {
            ExportError::Transport(format!(
                "OTLP receiver rejected {rejected} {} (not retried)",
                signal.unit()
            ))
        }),
    }
}

/// ProtoJSON permits quoted/unquoted integer exponent notation. Parse decimal
/// strings exactly: rounding a quoted fractional value through f64 would invent
/// a whole rejected count. Response byte bounds also bound this parser's work.
fn integer_count(text: &str) -> Option<u64> {
    let (negative, text) = match text.strip_prefix('-') {
        Some(text) => (true, text),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let (mantissa, exponent) = match text.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (mantissa, exponent.parse::<i32>().ok()?),
        None => (text, 0),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if whole.is_empty() && fraction.is_empty()
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let digits = format!("{whole}{fraction}");
    let mut digits = digits.trim_start_matches('0').to_owned();
    if digits.is_empty() {
        return Some(0);
    }
    if negative {
        return None;
    }
    let mut scale = exponent.checked_sub(i32::try_from(fraction.len()).ok()?)?;
    if scale < 0 {
        let end = digits.len().checked_sub(scale.unsigned_abs() as usize)?;
        if !digits.as_bytes()[end..].iter().all(|b| *b == b'0') {
            return None;
        }
        digits.truncate(end);
        scale = 0;
    }
    if digits.len().checked_add(scale as usize)? > 19 {
        return None;
    }
    let value = digits
        .parse::<u64>()
        .ok()?
        .checked_mul(10_u64.checked_pow(scale as u32)?)?;
    (value <= i64::MAX as u64).then_some(value)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn parses_success_warnings_and_rejections_in_signal_units() {
        for (signal, field) in [
            (Signal::Logs, "rejectedLogRecords"),
            (Signal::Metrics, "rejectedDataPoints"),
            (Signal::Traces, "rejectedSpans"),
        ] {
            assert_eq!(parse_response(signal, 3, b"{}").counts.accepted, 3);
            let warning =
                parse_response(signal, 3, br#"{"partialSuccess":{"errorMessage":"secret echo"}}"#);
            assert_eq!(warning.counts.accepted, 3);
            assert_eq!(warning.counts.warnings, 1);
            assert!(warning.error.is_none());
            let body =
                format!(r#"{{"partialSuccess":{{"{field}":"2","errorMessage":"secret echo"}}}}"#);
            let partial = parse_response(signal, 3, body.as_bytes());
            assert_eq!(partial.counts.accepted, 1);
            assert_eq!(partial.counts.rejected, 2);
            assert!(!partial.retry);
            assert!(!partial.error.unwrap().to_string().contains("secret echo"));
        }
    }
    #[test]
    fn invalid_responses_are_bounded_drops_not_endless_retries() {
        for body in [
            "",
            "not json",
            "[]",
            r#"{"partialSuccess":[]}"#,
            r#"{"partialSuccess":{"rejectedLogRecords":"-1"}}"#,
            r#"{"partialSuccess":{"rejectedLogRecords":4}}"#,
        ] {
            let result = parse_response(Signal::Logs, 3, body.as_bytes());
            assert!(!result.retry);
            assert_eq!(result.counts.dropped, 3);
            assert_eq!(result.counts.accepted, 0);
        }
    }

    #[test]
    fn protojson_null_fields_are_unset_for_every_signal() {
        for (signal, field) in [
            (Signal::Logs, "rejectedLogRecords"),
            (Signal::Metrics, "rejectedDataPoints"),
            (Signal::Traces, "rejectedSpans"),
        ] {
            for body in [
                r#"{"partialSuccess":null}"#.to_owned(),
                format!(r#"{{"partialSuccess":{{"{field}":null,"errorMessage":null}}}}"#),
            ] {
                let result = parse_response(signal, 3, body.as_bytes());
                assert_eq!(result.counts.accepted, 3);
                assert_eq!(result.counts.dropped, 0);
                assert_eq!(result.counts.warnings, 0);
                assert!(result.error.is_none());
                assert!(!result.retry);
            }
            let body = format!(r#"{{"partialSuccess":{{"{field}":"2e0","errorMessage":null}}}}"#);
            let result = parse_response(signal, 3, body.as_bytes());
            assert_eq!(result.counts.accepted, 1);
            assert_eq!(result.counts.rejected, 2);
            assert_eq!(result.counts.warnings, 0);
        }
    }

    #[test]
    fn integer_exponents_preserve_counts_without_fractional_rounding() {
        for value in ["2", "2.0", "2e0", "20e-1", r#""2e0""#, r#""0.2e1""#] {
            let body = format!(r#"{{"partialSuccess":{{"rejectedLogRecords":{value}}}}}"#);
            assert_eq!(
                parse_response(Signal::Logs, 3, body.as_bytes())
                    .counts
                    .rejected,
                2
            );
        }
        assert_eq!(integer_count("9223372036854775807"), Some(i64::MAX as u64));
        for value in [
            "-1",
            "1.5",
            "1e100",
            "1e-100",
            "9223372036854775808",
            "1.0000000000000001",
            "NaN",
            "Infinity",
            "",
            "2e",
            "2e1e1",
        ] {
            assert_eq!(integer_count(value), None, "{value}");
        }
    }
}
