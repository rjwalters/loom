//! The author gate on automatic promotion to `loom:issue` (#10827).
//!
//! The comment-trust rules (#9548) cover markers and control phrases, not who
//! wrote the issue being promoted. An issue authored by an untrusted identity
//! (an outside bot, a non-collaborator) may be curated, but an automated
//! Champion verdict alone must not approve it for building: a trusted
//! maintainer adopts it (applies `loom:issue` by hand) or re-files it. The
//! decision reuses [`TrustPolicy`], so there is no second trust table.
//!
//! The caller promotes only on [`Gate::Trusted`]; [`Gate::Untrusted`] and
//! [`Gate::Unknown`] both mean "do not promote", differing only in the
//! explanation. Evidence that cannot be read is never read as trust.

use serde_json::Value;

use crate::comment_trust::{Author, TrustPolicy};

/// Marker on the one explanatory comment, so retries do not repeat it.
pub const NOTICE_MARKER: &str = "<!-- loom:promotion-author-gate -->";

/// Whether an issue's author clears automatic promotion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// The author is trusted: normal promotion criteria apply.
    Trusted,
    /// The author is not trusted (the login, if the forge named one).
    Untrusted(Option<String>),
    /// Trust could not be established (the reason): treated as not trusted.
    Unknown(String),
}

impl Gate {
    /// The stdout token a script matches on.
    #[must_use]
    pub fn token(&self) -> String {
        match self {
            Self::Trusted => "TRUSTED".to_string(),
            Self::Untrusted(login) => {
                format!("UNTRUSTED {}", login.as_deref().unwrap_or("(unknown author)"))
            }
            Self::Unknown(why) => format!("UNKNOWN {why}"),
        }
    }
}

/// Classify the REST issue object `issue` (`None`: it could not be read).
///
/// An issue object with no author (a deleted account) is untrusted, as for any
/// other trust check. A pull request is not an issue to promote and is refused
/// as unknown rather than judged.
#[must_use]
pub fn classify(policy: &TrustPolicy, issue: Option<&Value>) -> Gate {
    let Some(issue) = issue else {
        return Gate::Unknown("the issue could not be read from the forge".to_string());
    };
    if issue.get("pull_request").is_some() {
        return Gate::Unknown("the target is a pull request, not an issue".to_string());
    }
    let author = Author::from_json(issue);
    if policy.trusts(&author) {
        Gate::Trusted
    } else {
        Gate::Untrusted(author.login)
    }
}

/// The one-time comment body explaining the hold, marker first.
#[must_use]
pub fn notice_body(gate: &Gate) -> String {
    let why = match gate {
        Gate::Untrusted(Some(login)) => {
            format!("was filed by `{login}`, who is not a trusted author")
        }
        Gate::Untrusted(None) => "has no identifiable author".to_string(),
        Gate::Unknown(why) => format!("has an author that could not be verified ({why})"),
        Gate::Trusted => return String::new(),
    };
    format!(
        "{NOTICE_MARKER}\n**Champion: held at `loom:curated` — author not trusted**\n\n\
         This issue {why} (the comment-trust rules, `comment-trust.md`). It may be curated, \
         but an automated verdict does not approve it for building. A trusted maintainer \
         adopts it by applying `loom:issue` by hand, or by re-filing it.\n"
    )
}

#[cfg(test)]
mod tests;
