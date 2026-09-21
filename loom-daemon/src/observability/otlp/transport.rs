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
    let Some(partial) = body.get("partialSuccess") else {
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
        None => 0,
        Some(serde_json::Value::String(s)) => match s.parse::<u64>() {
            Ok(n) => n,
            Err(_) => return invalid(),
        },
        Some(value) => match value.as_u64() {
            Some(n) => n,
            None => return invalid(),
        },
    };
    if rejected > items {
        return invalid();
    }
    let warning = match partial.get("errorMessage") {
        None => false,
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
            r#"{"partialSuccess":null}"#,
            r#"{"partialSuccess":{"rejectedLogRecords":"-1"}}"#,
            r#"{"partialSuccess":{"rejectedLogRecords":4}}"#,
        ] {
            let result = parse_response(Signal::Logs, 3, body.as_bytes());
            assert!(!result.retry);
            assert_eq!(result.counts.dropped, 3);
            assert_eq!(result.counts.accepted, 0);
        }
    }
}
