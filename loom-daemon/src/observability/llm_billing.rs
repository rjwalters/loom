//! How an LLM run was billed (Issue #10749), exported as secret-free span
//! attributes on `loom.runtime.run` and every `loom.runtime.usage` span.
//!
//! `loom.cost.usd_estimate` is the list price either way, so a consumer
//! cannot tell cash (metered) spend from subscription use without this:
//!
//! - `llm.billing`: `subscription` | `api` | `local` | `unknown`;
//! - `llm.credential.kind`: `oauth-pool` | `chatgpt-seat` | `api-key`
//!   (omitted for `local` and for `unknown`);
//! - `llm.provider.profile`: the model-profile name (`zai-flash`,
//!   `quick-cerebras`, ...), when the launch selected one.
//!
//! Classification is data-driven. A native-harness launch reads the model
//! profile's optional `billing` declaration and the credential source it
//! resolved; a Claude/Codex launch is classified by its runtime. Anything
//! this module cannot classify is `unknown`, never a guess. Only enumerated
//! vocabulary and a profile name are ever emitted: no key value, account
//! name or token path.

use crate::telemetry::trace::TraceAttributes;

pub const BILLING_KEY: &str = "llm.billing";
pub const CREDENTIAL_KIND_KEY: &str = "llm.credential.kind";
pub const PROFILE_KEY: &str = "llm.provider.profile";

/// Span attribute keys this module owns (the export allowlist reads this).
pub const KEYS: &[&str] = &[BILLING_KEY, CREDENTIAL_KIND_KEY, PROFILE_KEY];

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LlmBilling {
    /// `subscription` | `api` | `local` | `unknown`.
    pub billing: String,
    /// `oauth-pool` | `chatgpt-seat` | `api-key`; `None` for local/unknown.
    pub credential_kind: Option<String>,
    /// Model-profile name, when one was selected.
    pub profile: Option<String>,
}

fn known_billing(value: &str) -> Option<&'static str> {
    match value {
        "subscription" => Some("subscription"),
        "api" => Some("api"),
        "local" => Some("local"),
        "unknown" => Some("unknown"),
        _ => None,
    }
}

fn known_kind(value: &str) -> Option<&'static str> {
    match value {
        "oauth-pool" => Some("oauth-pool"),
        "chatgpt-seat" => Some("chatgpt-seat"),
        "api-key" => Some("api-key"),
        _ => None,
    }
}

impl LlmBilling {
    fn new(billing: &str, kind: Option<&str>, profile: Option<&str>) -> Self {
        Self {
            billing: billing.to_string(),
            credential_kind: kind.map(str::to_string),
            profile: profile.map(str::to_string).filter(|p| !p.is_empty()),
        }
    }

    /// A native-harness (Pi/OpenCode/Kimi) launch.
    ///
    /// `declared` is the profile's own `billing` field. Without one, a profile
    /// that reads a provider credential is metered (`api`/`api-key`) — the
    /// documented default — and a credential-less profile is `unknown`.
    /// `credential_source` is `worker_spawn::credential::Source::as_str()`.
    #[must_use]
    pub fn native(
        profile: Option<&str>,
        declared: Option<&str>,
        has_credential_env: bool,
        credential_source: &str,
    ) -> Self {
        if credential_source == "gateway" {
            return Self::new("api", Some("api-key"), profile);
        }
        let credentialed = has_credential_env && credential_source != "none";
        match declared.and_then(known_billing) {
            Some("local") => Self::new("local", None, profile),
            Some("subscription") => {
                Self::new("subscription", credentialed.then_some("api-key"), profile)
            }
            Some("api") => Self::new("api", Some("api-key"), profile),
            Some(_) => Self::new("unknown", None, profile),
            None if has_credential_env => Self::new("api", Some("api-key"), profile),
            None => Self::new("unknown", None, profile),
        }
    }

    /// A Claude/Codex launch, classified by runtime. `metered_backstop` is a
    /// launch the preference walk settled on a governed metered tap.
    #[must_use]
    pub fn for_runtime(runtime: &str, profile: Option<&str>, metered_backstop: bool) -> Self {
        if metered_backstop {
            return Self::new("api", Some("api-key"), profile);
        }
        match runtime {
            "claude" => Self::new("subscription", Some("oauth-pool"), profile),
            "codex" => Self::new("subscription", Some("chatgpt-seat"), profile),
            _ => Self::new("unknown", None, profile),
        }
    }

    /// Re-read from already-emitted strings (launch record), keeping only the
    /// enumerated vocabulary.
    #[must_use]
    pub fn parse(billing: &str, kind: Option<&str>, profile: Option<&str>) -> Option<Self> {
        let billing = known_billing(billing)?;
        Some(Self::new(billing, kind.and_then(known_kind), profile))
    }

    /// Nothing establishes how the usage was billed.
    #[must_use]
    pub fn unknown() -> Self {
        Self::new("unknown", None, None)
    }

    /// A launch span's class; a span that carries none is `unknown`.
    #[must_use]
    pub fn of_launch(attributes: &TraceAttributes) -> Self {
        Self::from_attributes(attributes).unwrap_or_else(Self::unknown)
    }

    /// The class every launch in `launches` agrees on, for usage that cannot
    /// be split between them. Launches billed differently (or none at all)
    /// give `unknown`; a credential kind or profile is kept only when every
    /// launch shares it. Never picks one launch's class for all of them.
    #[must_use]
    pub fn agreed(launches: impl IntoIterator<Item = Self>) -> Self {
        let mut launches = launches.into_iter();
        let Some(mut agreed) = launches.next() else {
            return Self::unknown();
        };
        for launch in launches {
            if launch.billing != agreed.billing {
                return Self::unknown();
            }
            if launch.credential_kind != agreed.credential_kind {
                agreed.credential_kind = None;
            }
            if launch.profile != agreed.profile {
                agreed.profile = None;
            }
        }
        agreed
    }

    pub fn stamp(&self, attributes: &mut TraceAttributes) {
        attributes.insert(BILLING_KEY.into(), self.billing.clone());
        if let Some(kind) = &self.credential_kind {
            attributes.insert(CREDENTIAL_KIND_KEY.into(), kind.clone());
        }
        if let Some(profile) = &self.profile {
            attributes.insert(PROFILE_KEY.into(), profile.clone());
        }
    }

    /// The billing keys of `span`'s attributes, when it carries `llm.billing`.
    #[must_use]
    pub fn from_attributes(attributes: &TraceAttributes) -> Option<Self> {
        Self::parse(
            attributes.get(BILLING_KEY)?,
            attributes.get(CREDENTIAL_KIND_KEY).map(String::as_str),
            attributes.get(PROFILE_KEY).map(String::as_str),
        )
    }
}

#[cfg(test)]
#[path = "llm_billing_tests.rs"]
mod tests;
