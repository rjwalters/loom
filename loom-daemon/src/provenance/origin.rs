//! Explicit work provenance. A username, bot flag or missing fleet marker
//! never implies a human drove a session.
use crate::comment_trust::TrustPolicy;
use serde_json::Value;

pub const ENV: &str = "LOOM_WORK_ORIGIN";

#[derive(
    Debug,
    Default,
    Clone,
    Copy,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub enum WorkOrigin {
    Interactive,
    Autonomous,
    #[default]
    Unknown,
}

impl WorkOrigin {
    pub fn from_env() -> Self {
        if std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true") {
            return Self::Autonomous;
        }
        std::env::var(ENV)
            .ok()
            .as_deref()
            .and_then(Self::parse)
            .unwrap_or_default()
    }
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "interactive" => Some(Self::Interactive),
            "autonomous" => Some(Self::Autonomous),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Autonomous => "autonomous",
            Self::Unknown => "unknown",
        }
    }

    /// Require exactly one valid standalone provenance record, from a
    /// trusted PR author. Duplicate records (even identical ones), unknown
    /// versions and malformed origin values are unknown, never priority.
    pub fn trusted_pr(pr: &Value, trust: &TrustPolicy) -> Self {
        if crate::comment_trust::Author::from_json(pr)
            .login
            .as_deref()
            .is_none_or(|s| s.trim().is_empty())
            || !trust.trusts_json(pr)
        {
            return Self::Unknown;
        }
        let body = pr["body"].as_str().unwrap_or("");
        if body.matches("<!-- loom:provenance").count() != 1 {
            return Self::Unknown;
        }
        crate::merge_pr::refs::strip_fenced_code_blocks(body)
            .lines()
            .filter(|line| line.starts_with(super::marker::PREFIX))
            .find_map(super::marker::Marker::parse)
            .and_then(|m| m.origin)
            .unwrap_or_default()
    }
}
