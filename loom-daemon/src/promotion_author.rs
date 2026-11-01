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

/// Marker on the one explanatory comment, so retries do not repeat it. A
/// legacy notice (before #13142) is exactly this; a current one carries its
/// [`Stamp`] (`<!-- loom:promotion-author-gate rev=… trust=… -->`).
pub const NOTICE_MARKER: &str = "<!-- loom:promotion-author-gate -->";

const NOTICE_OPEN: &str = "<!-- loom:promotion-author-gate";

/// What a gate verdict was reached from (#13142): the issue's title and body
/// and the trust rules. The step tick holds a gated issue out of
/// `champion_promote` while both still match, so an unchanged re-draw (a full
/// Champion session that can only repeat the verdict) is not made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stamp {
    /// [`revision`] of the issue's title and body.
    pub rev: String,
    /// [`TrustPolicy::digest`] of the trust rules.
    pub trust: String,
}

impl Stamp {
    fn marker(&self) -> String {
        format!("{NOTICE_OPEN} rev={} trust={} -->", self.rev, self.trust)
    }

    /// The stamp in the first gate marker of `comment`; `None` for a legacy
    /// marker (no stamp) or a comment with no gate marker.
    #[must_use]
    pub fn parse(comment: &str) -> Option<Self> {
        let start = comment.find(NOTICE_OPEN)? + NOTICE_OPEN.len();
        let rest = &comment[start..];
        let inner = &rest[..rest.find("-->")?];
        let field = |key: &str| {
            inner
                .split_whitespace()
                .find_map(|w| w.strip_prefix(key))
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };
        Some(Self {
            rev: field("rev=")?,
            trust: field("trust=")?,
        })
    }
}

/// The digest of an issue's `title` and `body` (the revise marker's hash,
/// `champion-issue-promo.md`): it moves when the issue is edited.
#[must_use]
pub fn revision(title: &str, body: &str) -> String {
    let text = format!("{}\n{}", title.trim_end_matches('\n'), body.trim_end_matches('\n'));
    crate::short_hash::short_sha16(&text)
}

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

/// The Actions bot, the only login the tracker exception admits (#13378).
const CI_DAILY_AUTHOR: &str = "github-actions[bot]";
/// Title prefix of a `ci-daily.yml` tracker.
const CI_DAILY_TITLE_PREFIX: &str = "[ci-daily] ";
/// The sentence `ci-daily.yml` writes into every tracker body it files.
const CI_DAILY_BODY_MARKER: &str = "opened, updated and closed by `.github/workflows/ci-daily.yml`";

/// Whether `issue` is a `ci-daily.yml` tracker filed by the Actions bot.
///
/// This is an exception to the *promotion author gate only*: it is not part of
/// [`TrustPolicy`], so the bot's comments and markers stay untrusted everywhere
/// (verdicts, holds, `forge trusted-comments`). All three conditions must hold.
fn is_ci_daily_tracker(author: &Author, issue: &Value) -> bool {
    let field = |key: &str| issue.get(key).and_then(Value::as_str).unwrap_or_default();
    author.login.as_deref() == Some(CI_DAILY_AUTHOR)
        && field("title").starts_with(CI_DAILY_TITLE_PREFIX)
        && field("body").contains(CI_DAILY_BODY_MARKER)
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
    if policy.trusts(&author) || is_ci_daily_tracker(&author, issue) {
        Gate::Trusted
    } else {
        Gate::Untrusted(author.login)
    }
}

/// The one-time comment body explaining the hold, marker first. `stamp`
/// (`None`: the legacy marker) is what the verdict was reached from.
#[must_use]
pub fn notice_body(gate: &Gate, stamp: Option<&Stamp>) -> String {
    let why = match gate {
        Gate::Untrusted(Some(login)) => {
            format!("was filed by `{login}`, who is not a trusted author")
        }
        Gate::Untrusted(None) => "has no identifiable author".to_string(),
        Gate::Unknown(why) => format!("has an author that could not be verified ({why})"),
        Gate::Trusted => return String::new(),
    };
    let marker = stamp.map_or_else(|| NOTICE_MARKER.to_string(), Stamp::marker);
    format!(
        "{marker}\n**Champion: held at `loom:curated` — author not trusted**\n\n\
         This issue {why} (the comment-trust rules, `comment-trust.md`). It may be curated, \
         but an automated verdict does not approve it for building. A trusted maintainer \
         adopts it by applying `loom:issue` by hand, or by re-filing it.\n"
    )
}

#[cfg(test)]
mod tests;
