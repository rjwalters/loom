//! Harness-output failure classifier for API-key providers (issues #8401,
//! #8424), the API-key analogue of the per-provider pattern tables in
//! `.loom/scripts/lib/classify-error.sh` (`spawn-codex.sh`'s classifier table,
//! built from observed CLI output).
//!
//! # Three outcomes, not two — and only two of them bad-mark
//!
//! [`Classification::CredentialFailure`] exists because an auth failure and an
//! exhaustion look equally like "the run died on the credential" from the
//! outside, and treating them the same is the one mistake this module must not
//! make (#8424 item 5, handed off from #8438): on OpenCode 2.x a `401` is also
//! what a **correct** key looks like when the adapter forgets `--standalone`,
//! so bad-marking on it would take a perfectly healthy account out of
//! selection for a launch-configuration bug. So a credential failure carries
//! **no reset horizon at all** — [`Classification::default_cooldown_secs`]
//! returns `None` and [`Classification::marks_bad`] is `false`, which is what
//! makes "do not apply an exhaustion cooldown to an auth failure" a property of
//! the type rather than of each caller's discipline.
//!
//! # Pattern provenance is recorded per pattern, on purpose
//!
//! #8401 built the first table from Zhipu's/Z.ai's **publicly documented**
//! error shapes and said so in a module-level caveat. #8424 asks for real
//! captures instead. Rather than replace one blanket caveat with another, every
//! pattern now carries its own [`Provenance`]:
//!
//! * [`Provenance::LiveCapture`] — the exact bytes were observed coming out of
//!   a real harness run, cited inline, and are asserted verbatim by a test.
//! * [`Provenance::DocumentedShape`] — transcribed from provider documentation
//!   and **not yet** seen in live output. Still worth matching (a best-effort
//!   match beats no match), but an operator reading this table can see which
//!   rows are evidence and which are inference.
//!
//! ## Honest limits of this revision
//!
//! One real capture is folded in: the `provider.auth`/HTTP 401 event below.
//! **No live Z.ai coding-plan *exhaustion* has been captured yet** — forcing
//! one requires actually running a subscription dry, which no test host here
//! can do on demand, and fabricating a plausible-looking string would be worse
//! than an honestly-labelled documented shape. Those rows therefore stay
//! [`Provenance::DocumentedShape`] and the gap stays open; fold each new
//! capture in as it is observed, replacing that row's provenance. A test keeps
//! the labelling honest rather than pretending the gap is closed.

use serde_json::Value;

/// What a harness's error text was recognised as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Classification {
    /// The provider-side allowance for this billing period is used up — no
    /// point retrying before the account's own reset instant.
    Exhausted,
    /// A transient concurrency/rate ceiling — worth a short cooldown, not a
    /// long one.
    RateLimited,
    /// The credential was rejected, or no credential reached the provider at
    /// all (`provider.auth`, HTTP 401). A configuration/credential fault, not
    /// a quota signal: it gets **no** cooldown and **no** bad mark. See the
    /// module docs.
    CredentialFailure,
}

impl Classification {
    /// A conservative default cooldown when the caller has no better
    /// provider-reported reset instant to pass to
    /// [`super::bad_marks::mark_bad`] instead.
    ///
    /// `None` means "this classification has no exhaustion reset horizon" —
    /// i.e. it must not be turned into a bad mark at all. It is deliberately
    /// **not** the same `None` [`super::bad_marks::mark_bad`] accepts (which
    /// means "mark indefinitely"): a caller must branch on it, never forward
    /// it. [`Self::marks_bad`] is the readable form of the same test.
    #[must_use]
    pub fn default_cooldown_secs(self) -> Option<u64> {
        match self {
            // 6h: long enough to stay out of a tight retry loop, short enough
            // that a stale mark self-heals inside a day even with no operator
            // follow-up.
            Self::Exhausted => Some(6 * 3600),
            Self::RateLimited => Some(60),
            Self::CredentialFailure => None,
        }
    }

    /// Whether this classification may take an account out of selection.
    /// `false` only for [`Self::CredentialFailure`] — see the module docs.
    #[must_use]
    pub fn marks_bad(self) -> bool {
        self.default_cooldown_secs().is_some()
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Exhausted => "exhausted",
            Self::RateLimited => "rate-limited",
            Self::CredentialFailure => "credential-failure",
        }
    }
}

/// Where a [`Pattern`]'s bytes came from. See the module docs for why this is
/// recorded per pattern rather than as one blanket caveat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provenance {
    /// Observed in real harness output. The string cites harness, version and
    /// date, the same way `classify-error.sh`'s table does.
    LiveCapture(&'static str),
    /// Transcribed from provider documentation; not yet seen live.
    DocumentedShape,
}

/// One substring the classifier recognises, plus its provenance.
#[derive(Clone, Copy, Debug)]
pub struct Pattern {
    /// Matched case-insensitively against the harness's combined output.
    /// Must itself be lowercase (asserted by a test).
    pub needle: &'static str,
    pub classification: Classification,
    pub provenance: Provenance,
}

/// Captured verbatim from OpenCode 2.0.10, `run --format json`, with the
/// provider key absent from the serving process — one line on stdout, exit 1
/// (observed 2026-09-20, #8438, folded in here by #8424 item 5):
///
/// ```json
/// {"type":"error","error":{"type":"provider.auth","message":"Provider request failed with HTTP 401","status":401}}
/// ```
///
/// This is the one real capture behind the table below, and it is asserted
/// byte-for-byte by a test, so an edit that breaks the shape breaks a test
/// rather than silently degrading the classifier.
pub const CAPTURED_OPENCODE_AUTH_EVENT: &str = r#"{"type":"error","error":{"type":"provider.auth","message":"Provider request failed with HTTP 401","status":401}}"#;

/// Citation used by every row taken from [`CAPTURED_OPENCODE_AUTH_EVENT`].
const OPENCODE_2_0_10: &str = "opencode 2.0.10 `run --format json`, observed 2026-09-20 (#8438)";

/// The whole pattern table, grouped for readability only — precedence is
/// decided by [`classify`], not by position (see its docs).
pub const PATTERNS: &[Pattern] = &[
    // ---- Credential / configuration faults (never bad-marked) ----
    Pattern {
        // The `error.type` of the captured OpenCode event above. Matched as
        // text too, not only structurally, so a run whose event stream is
        // wrapped in adapter prose still classifies.
        needle: "provider.auth",
        classification: Classification::CredentialFailure,
        provenance: Provenance::LiveCapture(OPENCODE_2_0_10),
    },
    Pattern {
        needle: "provider request failed with http 401",
        classification: Classification::CredentialFailure,
        provenance: Provenance::LiveCapture(OPENCODE_2_0_10),
    },
    Pattern {
        needle: "invalid api key",
        classification: Classification::CredentialFailure,
        provenance: Provenance::DocumentedShape,
    },
    Pattern {
        needle: "invalid_api_key",
        classification: Classification::CredentialFailure,
        provenance: Provenance::DocumentedShape,
    },
    Pattern {
        needle: "authentication_error",
        classification: Classification::CredentialFailure,
        provenance: Provenance::DocumentedShape,
    },
    // ---- Allowance exhausted ----
    Pattern {
        needle: "insufficient balance",
        classification: Classification::Exhausted,
        provenance: Provenance::DocumentedShape,
    },
    Pattern {
        needle: "insufficient_quota",
        classification: Classification::Exhausted,
        provenance: Provenance::DocumentedShape,
    },
    Pattern {
        needle: "quota exceeded",
        classification: Classification::Exhausted,
        provenance: Provenance::DocumentedShape,
    },
    Pattern {
        needle: "coding plan quota",
        classification: Classification::Exhausted,
        provenance: Provenance::DocumentedShape,
    },
    Pattern {
        // Zhipu API's documented error code for an exhausted balance/allowance.
        needle: "\"code\":\"1113\"",
        classification: Classification::Exhausted,
        provenance: Provenance::DocumentedShape,
    },
    // ---- Rate / concurrency ceiling ----
    Pattern {
        needle: "rate limit",
        classification: Classification::RateLimited,
        provenance: Provenance::DocumentedShape,
    },
    Pattern {
        needle: "rate_limit",
        classification: Classification::RateLimited,
        provenance: Provenance::DocumentedShape,
    },
    Pattern {
        needle: "too many requests",
        classification: Classification::RateLimited,
        provenance: Provenance::DocumentedShape,
    },
    Pattern {
        needle: "concurrency limit",
        classification: Classification::RateLimited,
        provenance: Provenance::DocumentedShape,
    },
];

/// Words that make a nearby HTTP status number a status rather than a line
/// number, a token count (`14290 tokens`) or a port. A bare `"429"` substring
/// matched all of those.
const STATUS_CONTEXT: &[&str] = &["http", "status", "code", "error"];

/// `true` when `lowered` holds `status` as a standalone number — not part of a
/// longer word or number such as `14290`, `1.429` or `429,000` — with one of
/// [`STATUS_CONTEXT`] shortly before it on the same line.
fn mentions_http_status(lowered: &str, status: &str) -> bool {
    let bytes = lowered.as_bytes();
    let byte = |i: Option<usize>| i.and_then(|i| bytes.get(i)).copied();
    let digit = |b: Option<u8>| b.is_some_and(|b| b.is_ascii_digit());
    let separator = |b: Option<u8>| matches!(b, Some(b'.' | b','));
    lowered.match_indices(status).any(|(at, _)| {
        let (prev, prev2) = (byte(at.checked_sub(1)), byte(at.checked_sub(2)));
        let (next, next2) = (byte(Some(at + status.len())), byte(Some(at + status.len() + 1)));
        let joined_before =
            prev.is_some_and(|b| b.is_ascii_alphanumeric()) || (separator(prev) && digit(prev2));
        let joined_after =
            next.is_some_and(|b| b.is_ascii_alphanumeric()) || (separator(next) && digit(next2));
        // `get`, not slicing at a computed offset: `at - 24` need not be a
        // char boundary in non-ASCII harness output.
        let window = (at.saturating_sub(24)..=at)
            .find_map(|start| lowered.get(start..at))
            .unwrap_or("");
        let same_line = window.rsplit('\n').next().unwrap_or("");
        !joined_before
            && !joined_after
            && STATUS_CONTEXT.iter().any(|word| same_line.contains(word))
    })
}

/// Read a native harness error **event** rather than prose: any line that
/// parses as a JSON object with `"type":"error"` is inspected for its
/// `error.type` / `error.status`, the shape of
/// [`CAPTURED_OPENCODE_AUTH_EVENT`].
///
/// Deliberately narrow on status codes: `401` is a credential fault, `402`
/// (payment required) and `429` are allowance/rate signals, and **everything
/// else — `403` included — is left unclassified**. A `403` is "forbidden" on
/// some providers and "quota gone" on others; guessing would either strand a
/// real exhaustion or bad-mark a healthy account, so an unrecognised status
/// falls through to the text patterns like any other prose.
fn classify_error_event(text: &str) -> Option<Classification> {
    let mut seen: Option<Classification> = None;
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if event.get("type").and_then(Value::as_str) != Some("error") {
            continue;
        }
        let error = event.get("error").unwrap_or(&Value::Null);
        let kind = error.get("type").and_then(Value::as_str).unwrap_or("");
        let status = error.get("status").and_then(Value::as_i64);
        let found = if kind.starts_with("provider.auth") || status == Some(401) {
            Some(Classification::CredentialFailure)
        } else if status == Some(402) {
            Some(Classification::Exhausted)
        } else if status == Some(429) {
            Some(Classification::RateLimited)
        } else {
            None
        };
        // A credential failure anywhere in the region wins outright — see
        // `classify`'s precedence note.
        match found {
            Some(Classification::CredentialFailure) => {
                return Some(Classification::CredentialFailure)
            }
            Some(other) => seen = seen.or(Some(other)),
            None => {}
        }
    }
    seen
}

/// Best-effort classification of a harness's combined stdout+stderr. Returns
/// `None` when nothing recognisable matched — the ordinary "this failure is
/// something else entirely" case, which must never be treated as exhaustion.
///
/// # Precedence, and why it leans away from bad-marking
///
/// Structured error **events** are read first (they are the harness's own
/// machine-readable statement of what went wrong; prose may merely quote a
/// retry banner), then the text table. Within both,
/// [`Classification::CredentialFailure`] outranks `Exhausted`/`RateLimited`,
/// so output carrying both signals produces **no bad mark**. That asymmetry is
/// deliberate: the cost of missing an exhaustion mark is one wasted retry that
/// the round-robin ladder spreads over the other accounts, while the cost of a
/// wrong exhaustion mark is a healthy account removed from the pool for hours
/// because of a launch-configuration bug (#8438's `--standalone` shape).
///
/// `_exit_code` is accepted but not consulted: a process exit status is 0-255,
/// so an HTTP status can never arrive through it (an earlier `== 429` check was
/// unreachable). The parameter stays so a future harness-specific exit code can
/// be keyed on without changing every caller.
#[must_use]
pub fn classify(output: &str, _exit_code: i32) -> Option<Classification> {
    if let Some(from_event) = classify_error_event(output) {
        return Some(from_event);
    }
    let lowered = output.to_ascii_lowercase();
    let matched = |wanted: Classification| {
        PATTERNS
            .iter()
            .any(|p| p.classification == wanted && lowered.contains(p.needle))
    };
    if matched(Classification::CredentialFailure) || mentions_http_status(&lowered, "401") {
        return Some(Classification::CredentialFailure);
    }
    if matched(Classification::Exhausted) {
        return Some(Classification::Exhausted);
    }
    if matched(Classification::RateLimited) || mentions_http_status(&lowered, "429") {
        return Some(Classification::RateLimited);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_the_documented_exhaustion_and_rate_limit_shapes() {
        assert_eq!(
            classify("Error: insufficient balance for this account", 1),
            Some(Classification::Exhausted)
        );
        assert_eq!(
            classify("{\"error\":{\"code\":\"1113\",\"message\":\"...\"}}", 1),
            Some(Classification::Exhausted)
        );
        assert_eq!(classify("HTTP 429 Too Many Requests", 1), Some(Classification::RateLimited));
        assert_eq!(classify("connection reset by peer", 1), None);
    }

    /// #8424 items 2/5: the one real captured string, asserted verbatim. The
    /// constant is what the table's `LiveCapture` rows cite, so this test is
    /// what keeps that citation true.
    #[test]
    fn the_real_captured_opencode_auth_event_classifies_as_a_credential_failure() {
        assert_eq!(
            CAPTURED_OPENCODE_AUTH_EVENT,
            r#"{"type":"error","error":{"type":"provider.auth","message":"Provider request failed with HTTP 401","status":401}}"#
        );
        assert_eq!(
            classify(CAPTURED_OPENCODE_AUTH_EVENT, 1),
            Some(Classification::CredentialFailure)
        );
        // Interleaved in a real `--log` file, among Loom's own marker lines.
        let log = format!(
            "# LOOM_LAUNCH {{\"schema\":1}}\n# LOOM_CLI_START runtime=opencode\n{CAPTURED_OPENCODE_AUTH_EVENT}\n"
        );
        assert_eq!(classify(&log, 1), Some(Classification::CredentialFailure));
    }

    /// #8424 item 5's core requirement: an auth failure carries no exhaustion
    /// reset horizon, and cannot become a bad mark.
    #[test]
    fn a_credential_failure_has_no_reset_horizon_and_never_bad_marks() {
        assert_eq!(Classification::CredentialFailure.default_cooldown_secs(), None);
        assert!(!Classification::CredentialFailure.marks_bad());
        for markable in [Classification::Exhausted, Classification::RateLimited] {
            assert!(
                markable
                    .default_cooldown_secs()
                    .is_some_and(|secs| secs > 0),
                "{markable:?}"
            );
            assert!(markable.marks_bad(), "{markable:?}");
        }
    }

    /// A launch-configuration bug (#8438's missing `--standalone`) emits a 401
    /// from a perfectly good key. Even alongside quota-ish prose, the verdict
    /// must stay "credential failure" so the account is not bad-marked.
    #[test]
    fn a_credential_failure_outranks_an_exhaustion_signal_in_the_same_output() {
        let mixed = format!("Error: insufficient balance\n{CAPTURED_OPENCODE_AUTH_EVENT}\n");
        assert_eq!(classify(&mixed, 1), Some(Classification::CredentialFailure));
        let mixed_text = "provider.auth failed; rate limit also mentioned";
        assert_eq!(classify(mixed_text, 1), Some(Classification::CredentialFailure));
    }

    #[test]
    fn structured_error_events_are_read_before_prose() {
        assert_eq!(
            classify(r#"{"type":"error","error":{"type":"provider.http","status":402}}"#, 1),
            Some(Classification::Exhausted)
        );
        assert_eq!(
            classify(r#"{"type":"error","error":{"type":"provider.http","status":429}}"#, 1),
            Some(Classification::RateLimited)
        );
        // 403 is deliberately NOT classified — see `classify_error_event`.
        assert_eq!(
            classify(r#"{"type":"error","error":{"type":"provider.http","status":403}}"#, 1),
            None
        );
        // A non-error event, and a non-JSON line, are both ignored.
        assert_eq!(classify(r#"{"type":"step_finish","tool":"loom_read"}"#, 1), None);
        assert_eq!(classify("{not json at all", 1), None);
    }

    /// Judge nit (#8428): a bare `"429"` substring matched any output that
    /// merely contained those digits. The same guard now covers `401`.
    #[test]
    fn a_429_counts_only_as_an_http_status() {
        for rate_limited in [
            "HTTP 429",
            "request failed with status code 429",
            "{\"error\":{\"code\":429}}",
            "Error: 429",
            "the provider answered with status 429.",
            "upstream returned HTTP/1.1 429\nretrying",
        ] {
            assert_eq!(
                classify(rate_limited, 1),
                Some(Classification::RateLimited),
                "{rate_limited:?}"
            );
        }
        for unrelated in [
            "panicked at src/main.rs:429:13",
            "used 14290 tokens",
            "error: listening on port 4290",
            "wrote 429 lines",
            "status ok\n429 files changed",
            "error code 1429",
            "error: latency 1.429s",
            "error: 429,000 rows",
            "ünïcödé ünïcödé ünïcödé 429",
        ] {
            assert_eq!(classify(unrelated, 1), None, "{unrelated:?}");
        }
    }

    #[test]
    fn a_401_counts_only_as_an_http_status() {
        for credential in [
            "HTTP 401",
            "error: status 401",
            "upstream returned HTTP/1.1 401",
        ] {
            assert_eq!(
                classify(credential, 1),
                Some(Classification::CredentialFailure),
                "{credential:?}"
            );
        }
        for unrelated in [
            "panicked at src/main.rs:401:2",
            "used 4010 tokens",
            "wrote 401 lines",
        ] {
            assert_eq!(classify(unrelated, 1), None, "{unrelated:?}");
        }
    }

    #[test]
    fn is_case_insensitive_and_never_panics_on_empty_output() {
        assert_eq!(classify("", 0), None);
        assert_eq!(classify("INSUFFICIENT BALANCE", 1), Some(Classification::Exhausted));
        assert_eq!(classify("PROVIDER.AUTH", 1), Some(Classification::CredentialFailure));
    }

    /// The table's own hygiene: every needle is lowercase (it is matched
    /// against lowercased output, so an upper-case needle would be dead), and
    /// the provenance labelling stays honest — at least one row is a real
    /// capture, every row *claiming* one is present in it, and the exhaustion
    /// rows are still the documented-shape gap #8424 item 2 records.
    #[test]
    fn the_pattern_table_is_lowercase_and_honestly_labelled() {
        for pattern in PATTERNS {
            assert_eq!(
                pattern.needle,
                pattern.needle.to_ascii_lowercase(),
                "needle {:?} would never match",
                pattern.needle
            );
            assert!(!pattern.needle.is_empty());
            if let Provenance::LiveCapture(citation) = pattern.provenance {
                assert!(
                    CAPTURED_OPENCODE_AUTH_EVENT
                        .to_ascii_lowercase()
                        .contains(pattern.needle),
                    "{:?} claims a live capture but does not appear in one",
                    pattern.needle
                );
                assert!(
                    citation.contains("observed"),
                    "citation {citation:?} names no observation"
                );
            }
        }
        assert!(
            PATTERNS
                .iter()
                .any(|p| matches!(p.provenance, Provenance::LiveCapture(_))),
            "the table must hold at least one real capture"
        );
        assert!(PATTERNS
            .iter()
            .filter(|p| p.classification == Classification::Exhausted)
            .all(|p| p.provenance == Provenance::DocumentedShape));
    }

    #[test]
    fn default_cooldowns_are_ordered_rate_limit_shorter_than_exhaustion() {
        assert!(
            Classification::RateLimited.default_cooldown_secs()
                < Classification::Exhausted.default_cooldown_secs()
        );
    }
}
