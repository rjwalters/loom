//! Which resolved `observability.endpoint` values are fit to export to.
//!
//! A separate module from [`super`] on purpose (file-size policy: new code
//! goes in a new sibling rather than growing `mod.rs`), and a narrow one: it
//! answers exactly one question — *is this endpoint a reserved placeholder
//! domain?* — with no config, env, or network dependencies, so the whole
//! policy is unit-testable as pure string classification.
//!
//! Issue #7815: the committed `.loom/config.json` ships a **placeholder**
//! endpoint (`https://dashboard.example.com/ingest`, #6650) and
//! `observability.ingestKeyFile` defaults to
//! `$HOME/.loom/observability/ingest.key`. A placeholder is a syntactically
//! valid `https://` URL, so before this check any host that resolved
//! `enabled: true` without also overriding the endpoint would POST its
//! **real** ingest key as an `Authorization: Bearer` header to a
//! third-party reserved domain — forever, since `sender.rs` retries
//! indefinitely. [`super::spawn_task`] therefore treats a placeholder
//! exactly like an unset endpoint: warn, register `misconfigured`, return
//! before the key file is even read.
//!
//! This is defense in depth, not the primary guard — the committed block
//! also ships `enabled: false` — so that a future placeholder or typo
//! cannot reopen the exposure.

/// Domain suffixes permanently reserved for documentation and testing, which
/// can therefore never be a real telemetry backend: the RFC 2606 §3
/// second-level names (`example.com`/`.net`/`.org`) and the RFC 2606 §2 /
/// RFC 6761 reserved TLDs (`.example`, `.invalid`, `.test`).
///
/// `localhost` is deliberately **absent** (RFC 6761 reserves it too): a
/// loopback endpoint is a legitimate local-dev / integration-test sink, and
/// an ingest key sent there never leaves the host.
const RESERVED_ENDPOINT_SUFFIXES: &[&str] = &[
    "example.com",
    "example.net",
    "example.org",
    "example",
    "invalid",
    "test",
];

/// OTLP carries authentication only in its Bearer header. Reject embedded
/// credentials and query/fragment data before a caller displays the endpoint.
#[must_use]
pub fn valid_otlp_endpoint(endpoint: &str) -> bool {
    reqwest::Url::parse(endpoint).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
    })
}

/// Extract the normalized host using the outbound HTTP client's URL parser.
/// Percent encoding, IDNA separators, and special-scheme syntax must have the
/// same interpretation here and in reqwest before any ingest key is loaded.
/// Scheme-less inputs retain their documented classification via an HTTPS base;
/// this fallback never replaces a successfully parsed URL that has a host.
fn endpoint_host(endpoint: &str) -> Option<String> {
    let url = reqwest::Url::parse(endpoint)
        .ok()
        .filter(|url| url.host_str().is_some())
        .or_else(|| {
            reqwest::Url::parse(&format!("https://{}", endpoint.trim_start_matches("//"))).ok()
        })?;
    let host = url.host_str()?.trim_end_matches('.').to_ascii_lowercase();
    (!host.is_empty()).then_some(host)
}

/// `true` when `host` **is** `suffix` or sits strictly under it as a DNS
/// child — so `example.com` and `dashboard.example.com` match `example.com`,
/// while `badexample.com` and `example.community` do not.
fn host_is_under(host: &str, suffix: &str) -> bool {
    host == suffix
        || (host.len() > suffix.len()
            && host.ends_with(suffix)
            && host.as_bytes()[host.len() - suffix.len() - 1] == b'.')
}

/// The reserved placeholder host this endpoint points at, if any — `None`
/// means "not a placeholder", i.e. safe to export to as far as this check is
/// concerned. The returned host is what the `misconfigured` detail names, so
/// an operator reading `loom-daemon status` sees exactly what was refused.
#[must_use]
pub fn reserved_placeholder_host(endpoint: &str) -> Option<String> {
    let host = endpoint_host(endpoint)?;
    RESERVED_ENDPOINT_SUFFIXES
        .iter()
        .any(|suffix| host_is_under(&host, suffix))
        .then_some(host)
}

#[cfg(test)]
mod tests {
    #[test]
    fn otlp_urls_reject_embedded_secret_locations() {
        for endpoint in [
            "http://user:secret@localhost",
            "http://localhost?key=secret",
            "http://localhost#secret",
            "not a URL",
            "ftp://localhost",
        ] {
            assert!(!super::valid_otlp_endpoint(endpoint));
        }
        assert!(super::valid_otlp_endpoint("https://localhost:4318/prefix/"));
    }
    use super::*;

    // Pure classification — no env, no runtime, no fixtures.
    // `super::super::tests::spawn_task_placeholder_endpoint_returns_none`
    // covers the wiring into `spawn_task`'s degrade-to-disabled path.

    #[test]
    fn rejects_rfc2606_documentation_domains() {
        for endpoint in [
            "https://example.com/ingest",
            "https://dashboard.example.com/ingest", // the committed placeholder (#6650)
            "https://ingest.example.net/v1/telemetry",
            "http://collector.example.org",
            "https://EXAMPLE.COM/ingest",            // case-insensitive
            "https://dashboard.example.com./ingest", // fully-qualified trailing dot
            "https://dashboard.example.com:8443/ingest", // explicit port
            "https://user:pass@dashboard.example.com/ingest", // userinfo
            "dashboard.example.com/ingest",          // no scheme at all
        ] {
            assert!(reserved_placeholder_host(endpoint).is_some(), "must be refused: {endpoint}");
        }
    }

    #[test]
    fn rejects_normalized_reserved_hosts() {
        for endpoint in [
            "https://%65xample.com/ingest",
            "https://example%2ecom/ingest",
            "https://example。com/ingest",
            "https://example．com/ingest",
            "https:example.com/ingest",
            r"https://example.com\@real.company/ingest",
            "https://%65xample.com.:8443/ingest",
            "https://collector.%74est/ingest",
            "dashboard.example.com:8443/ingest",
            "//dashboard.example.com/ingest",
        ] {
            assert!(reserved_placeholder_host(endpoint).is_some(), "must be refused: {endpoint}");
        }
        // A reserved spelling in userinfo or a path does not make the real host reserved.
        for endpoint in [
            "https://example.com@real.company/ingest",
            "https://real.company/example.com",
            "https://%62adexample.com/ingest",
            "http://[::1]:4318/ingest",
        ] {
            assert_eq!(reserved_placeholder_host(endpoint), None, "must be allowed: {endpoint}");
        }
    }

    #[test]
    fn rejects_reserved_tlds() {
        for endpoint in [
            "https://ingest.example/v1",
            "https://ingest.invalid/v1",
            "https://ingest.test/v1",
            "https://example/v1",
            "https://deeply.nested.sub.test/v1",
        ] {
            assert!(reserved_placeholder_host(endpoint).is_some(), "must be refused: {endpoint}");
        }
    }

    #[test]
    fn allows_localhost_and_real_hosts() {
        for endpoint in [
            // `localhost`/loopback is a legitimate local-dev and
            // integration-test sink — the key never leaves the host.
            "http://localhost:4318/v1/logs",
            "http://127.0.0.1:8787/ingest",
            "http://[::1]:8787/ingest",
            "https://loom-observability.workers.dev/ingest",
            "https://ingest.test-fixture.internal/v1/telemetry",
            // Near-misses that merely *contain* a reserved label: only a
            // whole-label DNS-child match counts.
            "https://badexample.com/ingest",
            "https://example.community/ingest",
            "https://latest.example.co/ingest",
            "https://testing.dev/ingest",
        ] {
            assert_eq!(reserved_placeholder_host(endpoint), None, "must be allowed: {endpoint}");
        }
    }

    #[test]
    fn names_the_offending_host() {
        assert_eq!(
            reserved_placeholder_host("https://dashboard.example.com/ingest").as_deref(),
            Some("dashboard.example.com"),
            "the misconfigured detail must be able to name what it refused"
        );
        // Unparseable garbage is never classified — the check can only ever
        // *add* a refusal, never turn a working endpoint into one.
        assert_eq!(reserved_placeholder_host(""), None);
        assert_eq!(reserved_placeholder_host("https://"), None);
    }
}
