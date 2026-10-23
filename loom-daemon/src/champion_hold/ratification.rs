//! The ratification hold (#12992): Champion's hard hold on a PR whose merge
//! **is** the act that ratifies a spec or decision record.
//!
//! A chip repo's target spec or decision record can say "ratified on merge" /
//! "ratified only on human merge approval". Merging such a PR decides
//! something that belongs to the operator, and every mechanical merge
//! criterion can pass on it (2AMLogic/sky130-dsp-fft#6 would have merged in
//! the 2026-10-20 shadow run). This module is the mechanical detector plus the
//! hold's state machine; `champion-ratification-hold.md` is the tick that
//! applies [`decide`]'s answer, `defaults/docs/ratification-hold.md` the
//! rationale.
//!
//! # What makes it a *hard* hold
//!
//! - It is a pure function of the PR's **head contents** ([`scan`]), so a
//!   re-evaluation is deterministic.
//! - No `champion.standingAuthorizations` class is ever consulted for it
//!   (`standing_auth_eval` runs only inside criteria #2/#3), and the #12950
//!   self-release path skips it: the hold's own marker ([`HOLD_MARKER`]) is
//!   what [`super::standing_hold`] reads, and [`super::HoldKind::Ratification`]
//!   outranks the other holds there.
//! - It is released by a **person's comment** ([`release_due`]), never by the
//!   label's absence (the critical-file hold's #9016 rule is deliberately NOT
//!   reused): a hand-removed `loom:blocked` is re-asserted by the next tick.
//!
//! The marker strings are a wire format: they live in comments on open PRs.

use regex::Regex;
use serde_json::Value;

/// The hold notice's first line.
pub const HOLD_MARKER: &str = "<!-- champion:ratification-hold -->";
/// An episode closed because the head no longer declares ratification-by-merge.
pub const CLEARED_MARKER: &str = "<!-- champion:ratification-hold-cleared -->";
/// A trusted person released the hold (`clear the hold — …`).
pub const RELEASED_MARKER: &str = "<!-- champion:ratification-release-respected -->";
/// The park record `reason` of the hold.
pub const HOLD_REASON: &str = "champion ratification hold";

/// Path globs of the files that can declare ratification. A pattern with a `/`
/// matches the whole path; one without matches the basename
/// ([`crate::main_health_gate::glob_matches`]).
pub const DEFAULT_PATHS: [&str; 4] = [
    "spec/**",
    "decisions/**",
    "docs/decisions/**",
    "*decision*.md",
];

/// Built-in ratification phrases (regexes, matched case-insensitively against
/// the file with markdown emphasis stripped). A repo extends, never replaces,
/// this list (`champion.ratificationPhrases`).
pub const DEFAULT_PHRASES: [&str; 3] = [
    r"ratified\s+(on|by)\s+(merge|the\s+operator)",
    r"ratified\s+only\s+(on|by)",
    r"merging\s+this\s+pr\s+(is|ratifies)",
];

/// A `Status: Proposed|Ratified` line: a hold only when the file also carries
/// a ratification clause ([`CLAUSE`]), so a bare status line never holds.
const STATUS: &str = r"status:\s*(proposed|ratified)\b";
/// "ratif…" within one clause of a merge / approval / operator.
const CLAUSE: &str = r"ratif\w*[^.\n]{0,80}\b(merge|merging|merged|approv\w*|operator)\b";

/// The repo's additions to the detector (`champion.ratificationPaths`,
/// `champion.ratificationPhrases`). Additive only.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    pub paths: Vec<String>,
    pub phrases: Vec<String>,
}

impl Config {
    /// From the effective config document (`champion.*`). Absent or
    /// non-string entries are ignored.
    #[must_use]
    pub fn from_effective(effective: &Value) -> Self {
        let list = |key: &str| -> Vec<String> {
            crate::config_resolver::get_path(effective, key)
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default()
        };
        Config {
            paths: list("champion.ratificationPaths"),
            phrases: list("champion.ratificationPhrases"),
        }
    }
}

/// One file of a PR's change list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    pub filename: String,
    pub previous_filename: Option<String>,
    /// The REST `status` (`added`, `modified`, `removed`, `renamed`, …).
    pub status: String,
    /// The file's content at the PR head. `None` when it could not be read.
    pub head_content: Option<String>,
}

/// What [`scan`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// No changed file declares ratification-by-merge.
    Clear,
    /// These files do (head path of each).
    Hold(Vec<String>),
    /// A candidate file's head content could not be read. Fail closed: never a
    /// `Clear`.
    Unreadable(Vec<String>),
}

fn compile(pat: &str) -> Regex {
    Regex::new(&format!("(?is){pat}"))
        .or_else(|_| Regex::new(&format!("(?is){}", regex::escape(pat))))
        .expect("an escaped literal is always a valid regex")
}

/// Whether the file (or the name it was renamed from) is a spec/decision path.
#[must_use]
pub fn is_candidate(f: &ChangedFile, cfg: &Config) -> bool {
    let globs: Vec<&str> = DEFAULT_PATHS
        .iter()
        .copied()
        .chain(cfg.paths.iter().map(String::as_str))
        .collect();
    std::iter::once(f.filename.as_str())
        .chain(f.previous_filename.as_deref())
        .any(|p| {
            globs
                .iter()
                .any(|g| crate::main_health_gate::glob_matches(g, p))
        })
}

/// Whether `content` declares that merging ratifies it. Markdown emphasis and
/// code ticks are dropped first (`ratified **on merge**`); a phrase inside a
/// fence or quote still counts: a false hold costs one operator comment, a
/// false pass ratifies a spec.
#[must_use]
pub fn declares_ratification(content: &str, cfg: &Config) -> bool {
    let text: String = content
        .chars()
        .filter(|c| !matches!(c, '*' | '_' | '`'))
        .collect();
    let phrases = DEFAULT_PHRASES
        .iter()
        .copied()
        .chain(cfg.phrases.iter().map(String::as_str));
    if phrases.map(compile).any(|re| re.is_match(&text)) {
        return true;
    }
    compile(STATUS).is_match(&text) && compile(CLAUSE).is_match(&text)
}

/// Scan a PR's change list. Only files the PR adds, modifies or renames count:
/// a deleted file, or a file whose head no longer carries the clause, does not
/// hold; a PR that merely cites a spec touches none.
#[must_use]
pub fn scan(files: &[ChangedFile], cfg: &Config) -> Verdict {
    let mut hits = Vec::new();
    let mut unreadable = Vec::new();
    for f in files
        .iter()
        .filter(|f| f.status != "removed" && is_candidate(f, cfg))
    {
        match &f.head_content {
            Some(c) if declares_ratification(c, cfg) => hits.push(f.filename.clone()),
            Some(_) => {}
            None => unreadable.push(f.filename.clone()),
        }
    }
    if !hits.is_empty() {
        Verdict::Hold(hits)
    } else if !unreadable.is_empty() {
        Verdict::Unreadable(unreadable)
    } else {
        Verdict::Clear
    }
}

/// A PR comment, as the hold reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    /// The author's login (an App is `x[bot]`).
    pub author: String,
    pub body: String,
}

impl Comment {
    #[must_use]
    pub fn new(author: &str, body: &str) -> Self {
        Comment {
            author: author.into(),
            body: body.into(),
        }
    }

    /// From a REST comment record (`user.login`, `body`).
    #[must_use]
    pub fn from_record(v: &Value) -> Option<Self> {
        Some(Comment {
            author: v
                .pointer("/user/login")
                .and_then(Value::as_str)?
                .to_string(),
            body: v.get("body").and_then(Value::as_str)?.to_string(),
        })
    }

    /// Posted by a person: not an App, and not one of Champion's own markers.
    fn by_person(&self) -> bool {
        !self.author.to_ascii_lowercase().ends_with("[bot]")
            && !self.body.trim_start().starts_with("<!-- champion:")
    }
}

/// Whether `body` opens with a release instruction: `clear|lift|override [the|
/// this] hold` as the leading clause (first sentence of the first line, minus
/// an `@mention` / `please` courtesy prefix), and that clause is not a
/// question. Narrower than criterion #2's `RELEASE_DEFS`: "merge anyway" and
/// "ok to merge" do not release a ratification, only an explicit hold clear.
#[must_use]
pub fn is_release_instruction(body: &str) -> bool {
    let first = body.trim_start().lines().next().unwrap_or("");
    let lead = Regex::new(r"^[^.?!]*[.?!]?")
        .expect("static")
        .find(first)
        .map_or("", |m| m.as_str());
    let lead = Regex::new(r"^@[A-Za-z0-9_-]+\s+")
        .expect("static")
        .replace(lead, "");
    let lead = Regex::new(r"(?i)^please\s*,?\s*")
        .expect("static")
        .replace(&lead, "")
        .into_owned();
    if lead.trim_end().ends_with('?') {
        return false;
    }
    Regex::new(r"(?i)^(clear|clearing|cleared|lift|lifting|lifted|override|overriding)\s+(this\s+|the\s+)?hold\b")
        .expect("static")
        .is_match(&lead)
}

/// The head SHA a marker comment records (`champion:hold-state head=<sha>`).
fn recorded_head(body: &str) -> Option<String> {
    crate::merge_pr::hold_state::recorded_head(body)
}

/// Where a PR's ratification episode stands, from its **trusted** comments,
/// oldest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Episode {
    /// No open episode (never held, or cleared).
    None,
    /// A hold notice stands; `head` is the head it recorded.
    Held { head: Option<String> },
    /// A person released it at `head`.
    Released { head: Option<String> },
}

/// The latest of the three markers (`starts_with`: a quote is prose, #5371).
#[must_use]
pub fn episode(comments: &[Comment]) -> Episode {
    let Some(c) = comments.iter().rev().find(|c| {
        [HOLD_MARKER, CLEARED_MARKER, RELEASED_MARKER]
            .iter()
            .any(|m| c.body.starts_with(m))
    }) else {
        return Episode::None;
    };
    if c.body.starts_with(HOLD_MARKER) {
        Episode::Held {
            head: recorded_head(&c.body),
        }
    } else if c.body.starts_with(RELEASED_MARKER) {
        Episode::Released {
            head: recorded_head(&c.body),
        }
    } else {
        Episode::None
    }
}

/// Whether a person has released the standing hold: a comment after the latest
/// hold notice, by a non-bot author, that [`is_release_instruction`]. Label
/// state is not an input: a hand-removed `loom:blocked` is not a release.
#[must_use]
pub fn release_due(comments: &[Comment]) -> bool {
    let Some(at) = comments
        .iter()
        .rposition(|c| c.body.starts_with(HOLD_MARKER))
    else {
        return false;
    };
    comments[at + 1..]
        .iter()
        .any(|c| c.by_person() && is_release_instruction(&c.body))
}

/// What the tick does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Nothing: not held, or released at this very head.
    None,
    /// Open a hold: notice + `loom:blocked` + park record.
    Hold,
    /// A hold stands: re-assert `loom:blocked` and the park record, no comment.
    Stands,
    /// A person released it: post [`RELEASED_MARKER`], remove `loom:blocked`.
    Release,
    /// The head no longer declares it: post [`CLEARED_MARKER`], remove the label.
    Clear,
    /// A candidate could not be read and no hold stands: do not merge this
    /// tick, write nothing.
    Defer,
}

impl Action {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Action::None => "none",
            Action::Hold => "hold",
            Action::Stands => "stands",
            Action::Release => "release",
            Action::Clear => "clear",
            Action::Defer => "defer",
        }
    }
}

/// The tick's decision from the detector's verdict, the trusted comments and
/// the current head. The release is scoped to a head: a push after it that
/// still declares ratification re-arms the hold, because the text the operator
/// released may not be the text now on the PR.
#[must_use]
pub fn decide(verdict: &Verdict, comments: &[Comment], head: &str) -> Action {
    let ep = episode(comments);
    if let Episode::Released { head: Some(h) } = &ep {
        if h == head {
            return Action::None;
        }
    }
    match (verdict, ep) {
        (Verdict::Clear, Episode::None) => Action::None,
        (Verdict::Clear, _) => Action::Clear,
        (Verdict::Unreadable(_), Episode::None | Episode::Released { .. }) => Action::Defer,
        (_, Episode::Held { .. }) if release_due(comments) => Action::Release,
        (_, Episode::Held { .. }) => Action::Stands,
        (_, Episode::None | Episode::Released { .. }) => Action::Hold,
    }
}

#[cfg(test)]
mod tests;
