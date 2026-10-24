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

    /// A PR is interactive (human) when its author is a present, trusted
    /// account that is not a bot or GitHub App login. No body marker is read:
    /// anything else is unknown, never priority.
    pub fn trusted_pr(pr: &Value, trust: &TrustPolicy) -> Self {
        let author = crate::comment_trust::Author::from_json(pr);
        if author.login.as_deref().is_none_or(|s| s.trim().is_empty())
            || author.app
            || !trust.trusts(&author)
        {
            return Self::Unknown;
        }
        Self::Interactive
    }
}
