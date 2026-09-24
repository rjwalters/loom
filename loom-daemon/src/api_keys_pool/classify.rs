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
//!
//! ## Kimi (issue #8563), and a corrected premise
//!
//! #8563 assumed the Kimi Code CLI (`@moonshot-ai/kimi-code`) splits its exit
//! codes `0`/`1`/`75` (success/permanent/transient) the way some other
//! harnesses do. A credential-free probe of the real, pinned 2.0.2 binary
//! (`docs/experiments/kimi-harness-probe-2026-09-22.json`, observation
//! `"exit-codes-the-vendor-75-does-not-exist"`) found that premise **false**:
//! the bundle's own process-exit call sites are `0, 1, 2, 129, 143` — there is
//! no `75`. Kimi absorbs a rate limit in-process (a 10-attempt exponential
//! backoff over ~150s) and, on exhausting that ladder, exits plain `1` with a
//! prose line, the same exit code a missing credential also produces. So exit
//! code carries no signal here (unchanged from `classify`'s existing
//! `_exit_code` stance) and the only way to split "no credential" from
//! "rate-limited" is the two live-captured needles below — [`Pattern`]s,
//! exactly like every other provider in this table, not a new Kimi-specific
//! exit-code branch.
//!
//! No live Kimi/Moonshot **exhaustion** (a billing-period allowance actually
//! used up, as opposed to a transient 429) has been captured — the generic
//! `insufficient balance` / `quota exceeded` rows below are `DocumentedShape`
//! and apply here too (this table is provider-neutral), but no Kimi-specific
//! exhaustion row exists yet; fold one in once observed, same as the Z.ai gap
//! above.

use serde_json::Value;

/// Captured verbatim from Kimi Code CLI 2.0.2, `kimi -p <prompt>
/// --output-format stream-json` with `KIMI_CODE_HOME` pointed at a
/// config-free scratch directory (no credentials, no `config.toml`) —
/// observed 2026-09-22 (`docs/experiments/kimi-harness-probe-2026-09-22.json`,
/// observation `"no-credential-permanent-failure"`), folded in here for #8563:
///
/// ```text
/// error: failed to run prompt: No model configured. Run `kimi` and use /login to sign in, then retry; or set default_model in config.toml.
/// ```
pub const CAPTURED_KIMI_NO_MODEL_CONFIGURED: &str = "error: failed to run prompt: No model \
     configured. Run `kimi` and use /login to sign in, then retry; or set default_model in \
     config.toml.";

/// Citation used by every row taken from [`CAPTURED_KIMI_NO_MODEL_CONFIGURED`].
const KIMI_2_0_2_NO_CREDENTIAL: &str = "kimi 2.0.2 (@moonshot-ai/kimi-code) `-p --output-format \
     stream-json` with no credential/config, observed 2026-09-22 (#8561/#8563, \
     docs/experiments/kimi-harness-probe-2026-09-22.json)";

/// Captured verbatim from the same probe's rate-limit simulation — a local
/// HTTP stub answering `429 {"error":{"message":"rate limit
/// exceeded","type":"rate_limit_error"}}` as the `KIMI_MODEL_BASE_URL`
/// provider. Kimi 2.0.2 exhausts its own 10-attempt in-process retry ladder
/// (~150s, `APIProviderRateLimitError` on each attempt) before surfacing this
/// on stderr at exit `1`:
///
/// ```text
/// error: failed to run prompt: provider.rate_limit: 429 rate limit exceeded
/// ```
pub const CAPTURED_KIMI_RATE_LIMIT_EXHAUSTED: &str =
    "error: failed to run prompt: provider.rate_limit: 429 rate limit exceeded";

/// Citation used by every row taken from [`CAPTURED_KIMI_RATE_LIMIT_EXHAUSTED`].
const KIMI_2_0_2_RATE_LIMIT: &str = "kimi 2.0.2 (@moonshot-ai/kimi-code) rate-limit-stub probe, \
     observed 2026-09-22 (#8561/#8563, docs/experiments/kimi-harness-probe-2026-09-22.json)";

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
    // Kimi Code CLI 2.0.2's own no-credential shape (#8563) — see
    // `CAPTURED_KIMI_NO_MODEL_CONFIGURED`. Two needles from the one capture,
    // so either half of the sentence still classifies if an adapter wraps or
    // truncates the line.
    Pattern {
        needle: "no model configured",
        classification: Classification::CredentialFailure,
        provenance: Provenance::LiveCapture(KIMI_2_0_2_NO_CREDENTIAL),
    },
    Pattern {
        needle: "use /login to sign in",
        classification: Classification::CredentialFailure,
        provenance: Provenance::LiveCapture(KIMI_2_0_2_NO_CREDENTIAL),
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
    Pattern {
        // Kimi Code CLI 2.0.2's own error-code prefix for a provider rate
        // limit, surfaced only after its in-process retry ladder gives up
        // (#8563) — see `CAPTURED_KIMI_RATE_LIMIT_EXHAUSTED`. The generic
        // `rate_limit` row above already matches this text too; this entry
        // exists so the row's own provenance can honestly say `LiveCapture`
        // rather than borrowing another pattern's `DocumentedShape` one.
        needle: "provider.rate_limit",
        classification: Classification::RateLimited,
        provenance: Provenance::LiveCapture(KIMI_2_0_2_RATE_LIMIT),
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

/// `true` when `line` is one of the harness's **transcript** events — a JSON
/// object carrying a string `type` that is not `"error"`.
///
/// This is the same definition of "the harness's own error event"
/// [`classify_error_event`] already applies (`type == "error"`, exactly), read
/// the other way round: everything else in the event stream is the *model's*
/// output — assistant text, reasoning, tool calls and their results — not the
/// provider's. `tool_result`-shaped events are deliberately on the transcript
/// side of that line: a tool's output (an agent running `gh`, say) is the
/// agent's doing, not the API key's provider speaking.
///
/// Non-JSON prose and JSON objects with **no** `type` (a raw provider error
/// body echoed into the log, `{"error":{"code":"1113",…}}`) are not transcript
/// events and stay readable.
fn is_transcript_event(line: &str) -> bool {
    let line = line.trim();
    if !line.starts_with('{') {
        return false;
    }
    let Ok(event) = serde_json::from_str::<Value>(line) else {
        return false;
    };
    event
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind != "error")
}

/// `region` with the harness's transcript events removed — see
/// [`is_transcript_event`]. Cheap and line-oriented, the same shape every
/// other reader of a retained log in this tree uses.
fn without_transcript(region: &str) -> String {
    region
        .lines()
        .filter(|line| !is_transcript_event(line))
        .collect::<Vec<_>>()
        .join("\n")
}

/// [`classify`] for the **automatic** path: a region of a retained launch log,
/// which is a whole run's transcript rather than a provider's error output.
///
/// # Why this is not just [`classify`] (#8521)
///
/// `classify` assumes its input is the provider's/harness's own words — true
/// when an operator hands it captured error output, false for the region
/// [`super::ingest::classify_launch_log`] holds. That region is everything
/// after a sweep's `sweep_id=` anchor (or a role tick's header), i.e. the
/// **agent's entire transcript**, and both native harnesses are launched in a
/// JSON event mode — `pi --print --mode json`, `opencode run --format json`
/// (`super::super::worker_spawn::harness`) — so the model's prose arrives
/// inside event payloads that the substring table happily matched, because it
/// never re-parsed them.
///
/// The exhaustion needles are ordinary English an agent in this repo emits
/// routinely: `judge.md`'s own GraphQL rate-limit signature table holds
/// `quota exceeded`, `rate limit` and `too many requests` verbatim. Any failed
/// run that merely *quoted* it therefore satisfied a match and could bad-mark
/// a healthy account for 6h — the exact inversion of this subsystem's stated
/// lean ("under-marking self-corrects … over-marking idles an allowance that
/// was never exhausted").
///
/// So the prose table is shown only the lines the *provider or harness* wrote:
/// structured `{"type":"error",…}` events (read structurally first, as
/// before), raw provider error bodies, and the adapter's/CLI's own non-JSON
/// stderr prose. Nothing else changes — precedence, the pattern table and the
/// HTTP-status guards are `classify`'s, unmodified.
#[must_use]
pub fn classify_launch_region(region: &str, exit_code: i32) -> Option<Classification> {
    // `classify_error_event` ignores every non-`error` event, so running the
    // whole ladder over the filtered text is equivalent for the structured
    // pass and narrowed for the prose pass — which is the entire change.
    classify(&without_transcript(region), exit_code)
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

    /// #8563: both real Kimi Code CLI 2.0.2 captures classify correctly, and
    /// — the point of the whole exercise — exit code carries no signal: both
    /// captures were observed at exit `1`, exactly like a healthy exit-0 run
    /// exits `0`, and the vendor never emits the `75` #8563 was filed
    /// expecting (see the module docs' "corrected premise" section).
    #[test]
    fn the_real_captured_kimi_events_classify_and_exit_code_is_not_the_signal() {
        assert_eq!(
            classify(CAPTURED_KIMI_NO_MODEL_CONFIGURED, 1),
            Some(Classification::CredentialFailure)
        );
        assert_eq!(
            classify(CAPTURED_KIMI_RATE_LIMIT_EXHAUSTED, 1),
            Some(Classification::RateLimited)
        );
        // Interleaved in a real retained log, among Loom's own marker lines.
        let log = format!(
            "# LOOM_LAUNCH {{\"schema\":1}}\n# LOOM_CLI_START runtime=kimi\n\
             {CAPTURED_KIMI_NO_MODEL_CONFIGURED}\n"
        );
        assert_eq!(classify(&log, 1), Some(Classification::CredentialFailure));
        // Neither capture depends on the exit code the harness happened to
        // return: 75, which #8563 assumed existed, never occurs on the
        // pinned 2.0.2 binary (bundle exit sites are 0, 1, 2, 129, 143).
        assert_eq!(
            classify(CAPTURED_KIMI_RATE_LIMIT_EXHAUSTED, 75),
            Some(Classification::RateLimited)
        );
    }

    /// #8563's own scope: "exit 1 must be split by output into
    /// `CredentialFailure` ... vs exhaustion". Kimi returns plain `1` for
    /// both, so the split has to come entirely from the two needles above —
    /// this pins that a credential failure still outranks a rate-limit signal
    /// even though both are real Kimi output, not a fabricated mix.
    #[test]
    fn a_kimi_credential_failure_outranks_a_kimi_rate_limit_signal_at_the_same_exit_code() {
        let mixed =
            format!("{CAPTURED_KIMI_RATE_LIMIT_EXHAUSTED}\n{CAPTURED_KIMI_NO_MODEL_CONFIGURED}\n");
        assert_eq!(classify(&mixed, 1), Some(Classification::CredentialFailure));
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
        // Every capture any `LiveCapture` row may cite. A new capture is
        // added here, not by relaxing this check to skip it.
        let live_captures = [
            CAPTURED_OPENCODE_AUTH_EVENT,
            CAPTURED_KIMI_NO_MODEL_CONFIGURED,
            CAPTURED_KIMI_RATE_LIMIT_EXHAUSTED,
        ];
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
                    live_captures
                        .iter()
                        .any(|capture| capture.to_ascii_lowercase().contains(pattern.needle)),
                    "{:?} claims a live capture but does not appear in any of {live_captures:?}",
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

    /// #8521: the automatic path's prose table must not read the model's own
    /// output. Every needle here is inside a non-`error` event — exactly the
    /// shape `opencode run --format json` / `pi --print --mode json` produce.
    #[test]
    fn the_launch_region_classifier_ignores_needles_in_the_agents_own_events() {
        for transcript in [
            r#"{"type":"text","text":"the table lists quota exceeded and rate limit"}"#,
            r#"{"type":"message","content":"HTTP 429 too many requests is the signature"}"#,
            r#"{"type":"reasoning","text":"insufficient balance would mean the plan ran dry"}"#,
            r#"{"type":"tool_result","output":"gh: API rate limit exceeded"}"#,
            r#"{"type":"step_finish","tool":"loom_read","text":"concurrency limit"}"#,
            // A whole plausible transcript, plus an unclassified 403.
            "{\"type\":\"text\",\"text\":\"quota exceeded\"}\n{\"type\":\"error\",\"error\":{\"type\":\"provider.http\",\"status\":403}}",
        ] {
            assert_eq!(classify_launch_region(transcript, 1), None, "{transcript:?}");
            // The un-narrowed classifier is what made this a false positive.
            assert!(classify(transcript, 1).is_some(), "{transcript:?}");
        }
    }

    /// The narrowing must not cost a true positive: the provider and the
    /// harness both still get heard, in every shape they speak in.
    #[test]
    fn the_launch_region_classifier_still_hears_the_provider_and_the_harness() {
        // Structured events — unchanged, read before any prose.
        assert_eq!(
            classify_launch_region(CAPTURED_OPENCODE_AUTH_EVENT, 1),
            Some(Classification::CredentialFailure)
        );
        assert_eq!(
            classify_launch_region(r#"{"type":"error","error":{"status":402}}"#, 1),
            Some(Classification::Exhausted)
        );
        assert_eq!(
            classify_launch_region(r#"{"type":"error","error":{"status":429}}"#, 1),
            Some(Classification::RateLimited)
        );
        // An error event's own prose, when its status is unrecognised.
        assert_eq!(
            classify_launch_region(
                r#"{"type":"error","error":{"message":"insufficient balance","status":400}}"#,
                1
            ),
            Some(Classification::Exhausted)
        );
        // Adapter/CLI stderr prose — never part of an event stream.
        assert_eq!(
            classify_launch_region("Error: insufficient balance for this account", 1),
            Some(Classification::Exhausted)
        );
        // A raw provider error body: JSON, but no `type`, so not a transcript
        // event.
        assert_eq!(
            classify_launch_region(r#"{"error":{"code":"1113","message":"…"}}"#, 1),
            Some(Classification::Exhausted)
        );
        // Interleaved in a real retained log, among Loom's own marker lines
        // and the agent's transcript: the provider's line is still found.
        let log = "==== loom-daemon dispatch: sweep_id=sweep-issue-8521-1 ====\n\
                   # LOOM_LAUNCH {\"schema\":1}\n\
                   # LOOM_CLI_START runtime=opencode\n\
                   {\"type\":\"text\",\"text\":\"working\"}\n\
                   Error: insufficient balance\n";
        assert_eq!(classify_launch_region(log, 1), Some(Classification::Exhausted));
        // A credential failure still outranks an exhaustion in the same region.
        assert_eq!(
            classify_launch_region(
                &format!("Error: insufficient balance\n{CAPTURED_OPENCODE_AUTH_EVENT}\n"),
                1
            ),
            Some(Classification::CredentialFailure)
        );
    }

    #[test]
    fn only_a_typed_non_error_json_object_counts_as_a_transcript_event() {
        for transcript in [
            r#"{"type":"text","text":"hi"}"#,
            r#"  {"type":"step_finish"}  "#,
            r#"{"type":"tool_use","tool":"loom_read"}"#,
        ] {
            assert!(is_transcript_event(transcript), "{transcript:?}");
        }
        for kept in [
            r#"{"type":"error","error":{"status":402}}"#,
            r#"{"error":{"code":"1113"}}"#, // no `type` at all
            r#"{"type":429}"#,              // `type` is not a string
            "Error: insufficient balance",  // not JSON
            "{not json at all",
            "",
            r#"["type","error"]"#, // not an object
        ] {
            assert!(!is_transcript_event(kept), "{kept:?}");
        }
    }

    #[test]
    fn default_cooldowns_are_ordered_rate_limit_shorter_than_exhaustion() {
        assert!(
            Classification::RateLimited.default_cooldown_secs()
                < Classification::Exhausted.default_cooldown_secs()
        );
    }
}
