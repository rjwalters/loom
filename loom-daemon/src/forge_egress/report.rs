//! Findings, the exit taxonomy, redaction, and the `doctor` report shape.
//!
//! Every type here mirrors 2am's `scripts/lib/github_egress.py` field for
//! field, so a dashboard or alert rule (loom-ui#1015) reads a Loom report and
//! a 2am report identically. Loom-only additions are *additive* JSON keys;
//! no 2am key is renamed or re-typed.

use std::collections::BTreeSet;

use regex::Regex;
use serde_json::{json, Value};

/// Report schema version. Matches the policy schema major version this
/// validator understands (`policy.schema.json` `schemaVersion: const 1`).
pub const SCHEMA_VERSION: u64 = 1;

/// How bad one finding is. Decides the exit class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    /// Visible but non-fatal: never changes the exit code (e.g. the
    /// `policy.unconfigured` notice on a host that is not declared managed).
    Notice,
    /// Something is definitely wrong — exit 1.
    Finding,
    /// Something that must be true could not be established — exit 2. Not a
    /// softer 1: both fail managed routing admission.
    Incomplete,
}

impl Severity {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Notice => "notice",
            Self::Finding => "finding",
            Self::Incomplete => "incomplete",
        }
    }
}

/// Which report section a finding belongs to. Only [`Section::Routing`]
/// contributes to the process exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Section {
    Routing,
    Git,
    Runtime,
    Telemetry,
}

impl Section {
    pub const ALL: [Section; 4] = [Self::Routing, Self::Git, Self::Runtime, Self::Telemetry];

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Routing => "routing",
            Self::Git => "git",
            Self::Runtime => "runtime",
            Self::Telemetry => "telemetry",
        }
    }
}

/// One failed (or unverifiable) invariant, in non-secret terms only: a host,
/// profile class, path or version plus a remedy. Never file contents, helper
/// output, arbitrary git config or the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub code: &'static str,
    pub invariant: String,
    pub expected: String,
    pub observed: String,
    pub source: String,
    pub remedy: String,
    pub severity: Severity,
    pub section: Section,
}

impl Finding {
    /// A routing-section finding with severity [`Severity::Finding`].
    #[must_use]
    pub fn new(code: &'static str, invariant: impl Into<String>) -> Self {
        Self {
            code,
            invariant: invariant.into(),
            expected: String::new(),
            observed: String::new(),
            source: String::new(),
            remedy: String::new(),
            severity: Severity::Finding,
            section: Section::Routing,
        }
    }

    #[must_use]
    pub fn expected(mut self, v: impl Into<String>) -> Self {
        self.expected = v.into();
        self
    }

    #[must_use]
    pub fn observed(mut self, v: impl Into<String>) -> Self {
        self.observed = v.into();
        self
    }

    #[must_use]
    pub fn source(mut self, v: impl Into<String>) -> Self {
        self.source = v.into();
        self
    }

    #[must_use]
    pub fn remedy(mut self, v: impl Into<String>) -> Self {
        self.remedy = v.into();
        self
    }

    #[must_use]
    pub fn incomplete(mut self) -> Self {
        self.severity = Severity::Incomplete;
        self
    }

    #[must_use]
    pub fn notice(mut self) -> Self {
        self.severity = Severity::Notice;
        self
    }

    #[must_use]
    pub fn section(mut self, s: Section) -> Self {
        self.section = s;
        self
    }

    /// The 2am `Finding.as_dict()` shape, every free-text field redacted.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "code": self.code,
            "invariant": self.invariant,
            "expected": redact(&self.expected),
            "observed": redact(&self.observed),
            "source": redact(&self.source),
            "remedy": redact(&self.remedy),
            "severity": self.severity.as_str(),
            "section": self.section.as_str(),
        })
    }
}

/// `0` aligned, `1` findings, `2` verification incomplete. A real finding
/// outranks an incomplete check, so "definitely wrong" never hides behind
/// "could not verify".
#[must_use]
pub fn exit_code(findings: &[Finding]) -> i32 {
    if findings.iter().any(|f| f.severity == Severity::Finding) {
        1
    } else if findings.iter().any(|f| f.severity == Severity::Incomplete) {
        2
    } else {
        0
    }
}

/// Collapse repeats by `(code, observed)` — readability, never suppression.
#[must_use]
pub fn dedupe(findings: Vec<Finding>) -> Vec<Finding> {
    let mut seen = BTreeSet::new();
    findings
        .into_iter()
        .filter(|f| seen.insert((f.code, f.observed.clone())))
        .collect()
}

fn token_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        #[allow(clippy::expect_used)]
        Regex::new(r"\b(gh[pousr]_[A-Za-z0-9]{16,}|github_pat_[A-Za-z0-9_]{20,})")
            .expect("static token regex compiles")
    })
}

fn bearer_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        #[allow(clippy::expect_used)]
        Regex::new(r"(?i)\b(bearer|token)\s+\S{12,}").expect("static bearer regex compiles")
    })
}

/// True when `value` looks like a credential rather than a reference. Narrow
/// on purpose (GitHub token prefixes, Authorization shapes) — a guard against
/// an obvious mistake, never a substitute for the field allowlist.
#[must_use]
pub fn looks_like_secret(value: &str) -> bool {
    token_re().is_match(value) || bearer_re().is_match(value)
}

/// Replace anything token-shaped with a fixed marker.
#[must_use]
pub fn redact(value: &str) -> String {
    let once = token_re().replace_all(value, "<redacted-token>");
    bearer_re()
        .replace_all(&once, "<redacted-authorization>")
        .into_owned()
}

/// Recursively [`redact`] every string in a JSON value (for `policy` output).
#[must_use]
pub fn redact_value(value: &Value) -> Value {
    match value {
        Value::String(s) => Value::String(redact(s)),
        Value::Array(items) => Value::Array(items.iter().map(redact_value).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), redact_value(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// One report section: its findings plus its own exit code.
#[must_use]
pub fn section_json(findings: &[Finding]) -> Value {
    json!({
        "findings": findings.iter().map(Finding::to_json).collect::<Vec<_>>(),
        "exit_code": exit_code(findings),
    })
}

/// The finding codes in `findings`, in order, deduplicated.
#[must_use]
pub fn codes(findings: &[Finding]) -> Vec<&'static str> {
    let mut seen = BTreeSet::new();
    findings
        .iter()
        .map(|f| f.code)
        .filter(|c| seen.insert(*c))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_taxonomy_finding_outranks_incomplete() {
        let finding = Finding::new("a", "i");
        let incomplete = Finding::new("b", "i").incomplete();
        assert_eq!(exit_code(&[]), 0);
        assert_eq!(exit_code(std::slice::from_ref(&finding)), 1);
        assert_eq!(exit_code(std::slice::from_ref(&incomplete)), 2);
        assert_eq!(exit_code(&[incomplete.clone(), finding.clone()]), 1);
        assert_eq!(dedupe(vec![finding.clone(), finding, incomplete]).len(), 2);
    }

    #[test]
    fn redaction_recognises_tokens_but_not_references() {
        let token = format!("ghp_{}", "A".repeat(36));
        assert!(looks_like_secret(&token));
        assert!(looks_like_secret(&format!("github_pat_{}", "B".repeat(30))));
        assert!(looks_like_secret("Bearer abcdefghijklmnop"));
        assert!(!looks_like_secret("file:/etc/2am/cred.ref"));
        assert!(!looks_like_secret("github-proxy.2amlogic.com"));
        let rendered = Finding::new("c", "i")
            .observed(format!("token {token}"))
            .to_json()
            .to_string();
        assert!(!rendered.contains(&token), "{rendered}");
    }

    #[test]
    fn finding_json_has_the_2am_shape() {
        let v = Finding::new("x.y", "inv").remedy("fix").to_json();
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "code",
                "invariant",
                "expected",
                "observed",
                "source",
                "remedy",
                "severity",
                "section"
            ]
        );
    }
}
