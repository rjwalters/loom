//! The ranked-options contract, as named reasons.
//!
//! [`validate`] returns EVERY failing reason, not the first: the caller is an
//! agent that will rewrite its input once, and a one-at-a-time refusal turns
//! that into N round trips.

use super::{Decision, MAX_OPTIONS, MIN_OPTIONS};
use std::collections::HashSet;
use std::fmt;

/// One contract failure. [`Reason::code`] is the stable machine token callers
/// and tests match on; [`fmt::Display`] adds the human detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// The input is not decision JSON at all.
    InvalidJson(String),
    /// `question` is empty or whitespace.
    NoQuestion,
    /// Fewer than [`MIN_OPTIONS`] options.
    TooFewOptions(usize),
    /// More than [`MAX_OPTIONS`] options.
    TooManyOptions(usize),
    /// An option has no `id` (position, 1-based).
    EmptyId(usize),
    /// An option has no `label` (option id).
    EmptyLabel(String),
    /// An option has no `why` key (option id).
    MissingWhy(String),
    /// An option's `why` is empty or whitespace (option id).
    EmptyWhy(String),
    /// Two options share an id.
    DuplicateId(String),
    /// `recommended` is absent or empty.
    RecommendedMissing,
    /// `recommended` names no option.
    UnknownRecommended(String),
    /// `recommended` names an option, but not the first (best-ranked) one.
    RecommendedNotFirst { recommended: String, first: String },
}

impl Reason {
    /// The stable reason code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Reason::InvalidJson(_) => "invalid_json",
            Reason::NoQuestion => "no_question",
            Reason::TooFewOptions(_) => "too_few_options",
            Reason::TooManyOptions(_) => "too_many_options",
            Reason::EmptyId(_) => "empty_id",
            Reason::EmptyLabel(_) => "empty_label",
            Reason::MissingWhy(_) => "missing_why",
            Reason::EmptyWhy(_) => "empty_why",
            Reason::DuplicateId(_) => "duplicate_id",
            Reason::RecommendedMissing => "recommended_missing",
            Reason::UnknownRecommended(_) => "unknown_recommended",
            Reason::RecommendedNotFirst { .. } => "recommended_not_first",
        }
    }
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let code = self.code();
        match self {
            Reason::InvalidJson(e) => write!(f, "{code}: input is not decision JSON ({e})"),
            Reason::NoQuestion => write!(f, "{code}: `question` is empty"),
            Reason::TooFewOptions(n) => {
                write!(f, "{code}: {n} option(s); a decision needs at least {MIN_OPTIONS}")
            }
            Reason::TooManyOptions(n) => {
                write!(f, "{code}: {n} options; a decision offers at most {MAX_OPTIONS}")
            }
            Reason::EmptyId(pos) => write!(f, "{code}: option #{pos} has no `id`"),
            Reason::EmptyLabel(id) => write!(f, "{code}: option `{id}` has no `label`"),
            Reason::MissingWhy(id) => write!(f, "{code}: option `{id}` has no `why`"),
            Reason::EmptyWhy(id) => write!(f, "{code}: option `{id}` has an empty `why`"),
            Reason::DuplicateId(id) => write!(f, "{code}: option id `{id}` is used twice"),
            Reason::RecommendedMissing => write!(f, "{code}: `recommended` is not set"),
            Reason::UnknownRecommended(id) => {
                write!(f, "{code}: `recommended` is `{id}`, which names no option")
            }
            Reason::RecommendedNotFirst { recommended, first } => write!(
                f,
                "{code}: `recommended` is `{recommended}` but the best-ranked option is \
                 `{first}`; order options best -> worst so the recommended one is first"
            ),
        }
    }
}

/// Parse decision JSON. A parse failure is itself a contract reason, so a
/// caller can report it exactly like any other refusal.
pub fn parse(input: &str) -> Result<Decision, Reason> {
    serde_json::from_str(input).map_err(|e| Reason::InvalidJson(e.to_string()))
}

/// Every contract failure in `d`, in a stable order. Empty = valid.
#[must_use]
pub fn validate(d: &Decision) -> Vec<Reason> {
    let mut out = Vec::new();
    if d.question.trim().is_empty() {
        out.push(Reason::NoQuestion);
    }
    let n = d.options.len();
    if n < MIN_OPTIONS {
        out.push(Reason::TooFewOptions(n));
    }
    if n > MAX_OPTIONS {
        out.push(Reason::TooManyOptions(n));
    }

    let mut seen = HashSet::new();
    for (i, o) in d.options.iter().enumerate() {
        let id = o.id.trim();
        if id.is_empty() {
            out.push(Reason::EmptyId(i + 1));
        } else if !seen.insert(id) {
            out.push(Reason::DuplicateId(id.to_string()));
        }
        // Name the option by id when it has one, by position otherwise, so
        // the reason is actionable either way.
        let name = if id.is_empty() {
            format!("#{}", i + 1)
        } else {
            id.to_string()
        };
        if o.label.trim().is_empty() {
            out.push(Reason::EmptyLabel(name.clone()));
        }
        match &o.why {
            None => out.push(Reason::MissingWhy(name)),
            Some(w) if w.trim().is_empty() => out.push(Reason::EmptyWhy(name)),
            Some(_) => {}
        }
    }

    match d.recommended.as_deref().map(str::trim) {
        None | Some("") => out.push(Reason::RecommendedMissing),
        Some(rec) => {
            if !d.options.iter().any(|o| o.id.trim() == rec) {
                out.push(Reason::UnknownRecommended(rec.to_string()));
            } else if let Some(first) = d.options.first() {
                if first.id.trim() != rec {
                    out.push(Reason::RecommendedNotFirst {
                        recommended: rec.to_string(),
                        first: first.id.trim().to_string(),
                    });
                }
            }
        }
    }
    out
}
