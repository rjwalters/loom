//! Producer-edge redaction for `session.output` (#9764) — **redaction v1**.
//!
//! # Why the producer, when `ci.job.log` redacts at the gateway
//!
//! `ci.job.log` (#8825) is scrubbed by the collector because its source is a
//! GitHub Actions log the daemon fetches wholesale: there is no earlier point
//! where the text is in Loom's hands. `session.output` is the opposite shape.
//! Its text is selected line by line out of a live transcript by code in this
//! process, so the earliest point is *here* — and an operator who exports
//! straight to their own OTLP endpoint without Loom's collector in front of it
//! would otherwise have no redaction at all.
//!
//! The collector's `transform/session_output_redaction` stage applies the same
//! classes again at the gateway. That is deliberate defence in depth, not
//! redundancy: neither layer is permitted to be the only one.
//!
//! # What this is not
//!
//! This is a **secret scrubber over already-selected content**, not a content
//! filter. It is not what keeps prompts, thinking blocks, tool arguments and
//! raw tool results out of the feed — those never reach this function, because
//! the adapter never turns them into text (see
//! `observability::session_output::claude`). Relying on a regex to exclude a
//! whole content category would be the wrong boundary and is not how this
//! works.

use std::sync::OnceLock;

use regex::Regex;

/// The policy name stamped on every record
/// ([`SessionOutputRecord::redaction`](super::SessionOutputRecord::redaction)).
/// Bump this whenever a class is added or changed, so a consumer can tell
/// which rows were produced under which policy.
pub const POLICY: &str = "producer/v1";

/// One redaction class: a pattern and the marker it is replaced with. The
/// marker text is part of the contract — a consumer may key on it, and the
/// gateway's patterns deliberately cannot match a marker (none contains `[`),
/// so a doubly-scrubbed body is stable rather than progressively mangled.
struct Class {
    pattern: Regex,
    marker: &'static str,
}

/// Every secret class the gateway's `transform/session_output_redaction`
/// stage must scrub out of a `session.output` body
/// (`defaults/observability/collector/config.yaml`), in the order the
/// collector applies them. Mirrors `classes()` above, with consecutive
/// repeats (the two `github-token` patterns) collapsed — same convention as
/// `CI_LOG_SCRUB_CLASSES` (`telemetry::ci`).
///
/// **This list is the reviewable source of truth.** A new class is added
/// here, to `classes()` above, and to the collector config in the same PR —
/// the contract test
/// (`collector_fanout::gateway_scrubs_exactly_the_declared_session_output_classes`)
/// fails if the gateway and this list ever disagree, in either direction.
pub const SESSION_OUTPUT_SCRUB_CLASSES: &[&str] = &[
    "authorization",
    "bearer-token",
    "github-token",
    "anthropic-key",
    "api-key",
    "aws-access-key-id",
    "aws-secret-access-key",
    "jwt",
    "azure-sas",
    "signed-url",
    "ssm-path",
    "credential",
    "email",
    "private-key",
];

fn classes() -> &'static [Class] {
    static CLASSES: OnceLock<Vec<Class>> = OnceLock::new();
    CLASSES.get_or_init(|| {
        // Mirrors the `ci.job.log` class set in
        // `defaults/observability/collector/config.yaml`, in the same order:
        // the most specific token shapes first, so a generic
        // `key: <value>` rule cannot swallow a shape a precise rule would have
        // labelled better. Every replacement excludes `[` from its value class
        // so an earlier marker is never re-redacted.
        let specs: &[(&str, &str)] = &[
            (r#"(?i)authorization["' ]*[:=][^\n]*"#, "[REDACTED:authorization]"),
            (r"(?i)bearer\s+[A-Za-z0-9._~+/-]{16,}=*", "[REDACTED:bearer-token]"),
            (r"(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{20,}", "[REDACTED:github-token]"),
            (r"github_pat_[A-Za-z0-9_]{20,}", "[REDACTED:github-token]"),
            (r"sk-ant-[A-Za-z0-9_-]{16,}", "[REDACTED:anthropic-key]"),
            (
                r#"(?i)(?:api[_-]?key|apikey|secret[_-]?key|access[_-]?token)["' ]*[:=]["' ]*sk-[A-Za-z0-9_-]{16,}"#,
                "[REDACTED:api-key]",
            ),
            (r"(?:AKIA|ASIA)[0-9A-Z]{16}", "[REDACTED:aws-access-key-id]"),
            (
                r#"(?i)(?:aws[_-]?secret[_-]?access[_-]?key|secret[_-]?access[_-]?key|secret[_-]?key)["']?\s*[:=]\s*["']?[A-Za-z0-9/+=]{40,}"#,
                "[REDACTED:aws-secret-access-key]",
            ),
            // The classes below were added (#5933, 2am) when `session.output`
            // gained an S3 path that bypasses the captain edge's
            // `transform/live-output`: the producer must carry that block's
            // pattern set, since nothing downstream scrubs those bytes.
            (
                r"eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{6,}",
                "[REDACTED:jwt]",
            ),
            (
                r#"(?i)(?:(?:\?|&)(?:sv|sr|sp|spr|st|se|sig|skoid|ske|skt|skv)=[^&\s"'()]*(?:&(?:sv|sr|sp|spr|st|se|sig|skoid|ske|skt|skv)=[^&\s"'()]*)*&(?:sig|skoid|ske|skt)=[^&\s"'()]*(?:&(?:sv|sr|sp|spr|st|se|sig|skoid|ske|skt|skv)=[^&\s"'()]*)*|(?:\?|&)(?:sig|skoid|ske|skt)=[^&\s"'()]*(?:&(?:sv|sr|sp|spr|st|se|sig|skoid|ske|skt|skv)=[^&\s"'()]*)+)"#,
                "[REDACTED:azure-sas]",
            ),
            (
                r#"(?i)https?://[^\s"'()]*?(?:[?&](?:x-amz-signature|signature|sig)=)[^\s"'()]*"#,
                "[REDACTED:signed-url]",
            ),
            (
                r#"(?:^|[\s"'=:;,(])(?:/harness-ops|/2am-logic)(?:/[A-Za-z0-9._*-]+)+"#,
                "[REDACTED:ssm-path]",
            ),
            (
                r#"(?i)(?:password|passwd|secret|token|api[_-]?key)["' ]*[:=]["' ]*[^\s"'&\[]{6,}"#,
                "[REDACTED:credential]",
            ),
            // Not in the CI set, and specific to this kind: an agent session
            // routinely prints the operator's own address (git author, forge
            // account). A live feed on a shared dashboard should not.
            (
                r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}",
                "[REDACTED:email]",
            ),
            // A PEM private-key header is never legitimate feed content, and
            // the body that follows it is matched by nothing else here. The
            // first pattern takes the whole block, running to the end of the
            // text when the `END` line is missing (a chunk or a clipped
            // record can cut a key short); the second takes a stray header.
            (
                r"(?i)[^\n]*-----BEGIN[\t ]+(?:[A-Z0-9]+[\t ]+)*PRIVATE KEY-----(?s:.*?)(?:(?i:-----END[\t ]+(?:[A-Z0-9]+[\t ]+)*PRIVATE KEY-----)[^\n]*|\z)",
                "[REDACTED:private-key]",
            ),
            (
                r"(?i)-----BEGIN[\t ]+(?:[A-Z0-9]+[\t ]+)*PRIVATE KEY-----|-----END[\t ]+(?:[A-Z0-9]+[\t ]+)*PRIVATE KEY-----",
                "[REDACTED:private-key]",
            ),
        ];
        specs
            .iter()
            .filter_map(|(pattern, marker)| {
                // A pattern that fails to compile is a build-time bug, but a
                // panic here would take down a daemon over a telemetry
                // nicety. Log loudly and drop the class instead — the
                // `every_class_compiles` test is what actually prevents it.
                match Regex::new(pattern) {
                    Ok(pattern) => Some(Class { pattern, marker }),
                    Err(error) => {
                        log::error!("session.output: redaction class {pattern:?} failed to compile: {error}");
                        None
                    }
                }
            })
            .collect()
    })
}

/// Apply every redaction class to `text`.
#[must_use]
pub fn scrub(text: &str) -> String {
    let mut out = text.to_string();
    for class in classes() {
        // `Cow::into_owned` only allocates when the pattern matched.
        out = class.pattern.replace_all(&out, class.marker).into_owned();
    }
    out
}

/// [`scrub`], then clip to `max_chars` **characters** (not bytes, so the
/// result is never split mid-codepoint).
///
/// Returns `(body, truncated_bytes)`, where `truncated_bytes` counts the UTF-8
/// bytes of scrubbed text that were dropped — `0` when nothing was clipped. A
/// clipped body ends with an explicit `…[+N chars truncated]` suffix so a
/// reader of the body alone, without the attribute, still knows it is partial.
///
/// Scrub-then-clip, never clip-then-scrub: clipping first can cut a secret in
/// half, leaving a prefix that no longer matches any class and is exported
/// verbatim.
#[must_use]
pub fn scrub_bounded(text: &str, max_chars: usize) -> (String, u64) {
    let scrubbed = scrub(text);
    let total = scrubbed.chars().count();
    if total <= max_chars {
        return (scrubbed, 0);
    }
    let kept: String = scrubbed.chars().take(max_chars).collect();
    let dropped = scrubbed.len().saturating_sub(kept.len()) as u64;
    let dropped_chars = total - max_chars;
    (format!("{kept}…[+{dropped_chars} chars truncated]"), dropped)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn every_class_compiles() {
        // `classes()` drops a class that fails to compile rather than
        // panicking; this is the test that makes that degradation unreachable.
        assert_eq!(classes().len(), 16, "a class silently failed to compile");
    }

    #[test]
    fn credential_shapes_never_survive() {
        for (secret, marker) in [
            ("ghp_abcdefghijklmnopqrstuvwxyz0123", "[REDACTED:github-token]"),
            ("github_pat_11ABCDEFG0abcdefghijklmnop", "[REDACTED:github-token]"),
            ("sk-ant-api03-abcdefghijklmnopqrstuvwxyz", "[REDACTED:anthropic-key]"),
            ("AKIAIOSFODNN7EXAMPLE", "[REDACTED:aws-access-key-id]"),
            (concat!("ASIA", "IOSFODNN7EXAMPLE"), "[REDACTED:aws-access-key-id]"),
            (
                "secret_key=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
                "[REDACTED:aws-secret-access-key]",
            ),
            (
                "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dBjftJeZ4CVP",
                "[REDACTED:jwt]",
            ),
            (
                "https://bucket.s3.amazonaws.com/k?X-Amz-Signature=0123abcd&X-Amz-Date=1",
                "[REDACTED:signed-url]",
            ),
            ("/harness-ops/github/token", "[REDACTED:ssm-path]"),
            ("/2am-logic/prod/db_password", "[REDACTED:ssm-path]"),
            (
                "Authorization: Bearer abcdefghijklmnopqrstuvwxyz012345",
                "[REDACTED:authorization]",
            ),
            ("password: hunter2hunter2", "[REDACTED:credential]"),
            ("someone@example.com", "[REDACTED:email]"),
        ] {
            let body = format!("I ran the command and got {secret} back");
            let scrubbed = scrub(&body);
            assert!(!scrubbed.contains(secret), "{secret} survived: {scrubbed}");
            assert!(scrubbed.contains(marker), "{secret} -> {scrubbed}");
        }
    }

    #[test]
    fn a_pem_private_key_block_is_removed_whole() {
        let body = "here it is:\n-----BEGIN RSA PRIVATE KEY-----\nMIIEow\nkey\n-----END RSA PRIVATE KEY-----\ndone";
        let scrubbed = scrub(body);
        assert!(!scrubbed.contains("MIIEow"));
        assert!(scrubbed.contains("[REDACTED:private-key]"));
        assert!(scrubbed.contains("done"));
    }

    #[test]
    fn an_unterminated_or_header_only_pem_block_is_removed_to_the_end() {
        let body = "log:\n-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXk\nmore key bytes";
        let scrubbed = scrub(body);
        assert!(!scrubbed.contains("b3Blbn"), "{scrubbed}");
        assert!(!scrubbed.contains("more key bytes"), "{scrubbed}");
        assert!(scrubbed.contains("log:"));
        let stray = scrub("tail -----END RSA PRIVATE KEY----- x");
        assert!(!stray.contains("-----END"), "{stray}");
    }

    #[test]
    fn an_azure_sas_query_string_is_removed_but_the_path_stays() {
        let scrubbed = scrub("see https://acct.blob.core.windows.net/c/b?sv=2022-11-02&sr=b&sig=abc%2Fdef%3D&se=2026-10-23 ok");
        assert!(!scrubbed.contains("sig=abc"), "{scrubbed}");
        assert!(scrubbed.contains("[REDACTED:azure-sas]"), "{scrubbed}");
        assert!(scrubbed.contains("acct.blob.core.windows.net/c/b"), "{scrubbed}");
    }

    #[test]
    fn the_stricter_union_of_the_edge_and_producer_floors_applies() {
        // The edge floors `github_pat_` at 22, the producer at 20: the
        // producer keeps the lower floor so neither's catch is lost.
        let scrubbed = scrub("x github_pat_abcdefghij0123456789 y");
        assert!(scrubbed.contains("[REDACTED:github-token]"), "{scrubbed}");
    }

    #[test]
    fn scrubbing_is_idempotent_and_never_re_redacts_a_marker() {
        let once = scrub("token: abcdefghijklmnop and ghp_abcdefghijklmnopqrstuvwxyz0123");
        let twice = scrub(&once);
        assert_eq!(once, twice);
    }

    #[test]
    fn ordinary_prose_is_untouched() {
        let body = "Reading builder.md, then running cargo check in the worktree.";
        assert_eq!(scrub(body), body);
    }

    #[test]
    fn bounding_reports_what_it_dropped_and_never_splits_a_codepoint() {
        let text = "é".repeat(50);
        let (body, dropped) = scrub_bounded(&text, 10);
        assert!(body.starts_with(&"é".repeat(10)));
        assert!(body.contains("[+40 chars truncated]"));
        // 40 dropped 2-byte chars.
        assert_eq!(dropped, 80);
        assert!(std::str::from_utf8(body.as_bytes()).is_ok());
    }

    #[test]
    fn nothing_is_reported_truncated_when_nothing_was() {
        let (body, dropped) = scrub_bounded("short", 100);
        assert_eq!(body, "short");
        assert_eq!(dropped, 0);
    }

    #[test]
    fn a_secret_straddling_the_bound_is_scrubbed_before_it_is_clipped() {
        // Clip-then-scrub would leave a live token prefix in the body. The
        // token sits past the bound, so only scrub-first removes it.
        let text = format!("{}ghp_abcdefghijklmnopqrstuvwxyz0123", "x".repeat(40));
        let (body, _) = scrub_bounded(&text, 45);
        assert!(!body.contains("ghp_"), "{body}");
    }
}
