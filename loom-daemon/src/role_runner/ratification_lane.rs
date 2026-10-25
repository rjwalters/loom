//! The ratification reviewer lane's selection layer (#13187, step 1 of 3 of
//! #13186).
//!
//! Finds open PRs that need key reviews: a PR that adds or edits
//! `ratification/requests/*.md`, or flips a `Status:` line to ratified under
//! `spec/**`, `decisions/**` or `docs/decisions/**`. It dispatches nothing:
//! sessions, instruction fetching and verdict posting are separate steps. The
//! forge is injected ([`PrSource`]) so every rule is unit-testable offline.
//!
//! # Fail closed
//!
//! A missing, disabled or malformed `ratificationLane` block yields
//! [`Config::disabled`]; [`find_candidates`] on a disabled config returns
//! nothing and never touches the source.
//!
//! # Key marker (wire format, emitted by step 3)
//!
//! `<!-- ratify:key=<ee|market> sha=<40-hex head> -->`. A PR is skipped only
//! when **both** keys carry a marker for its current head, from a trusted
//! comment author (the source returns trusted comments only; a marker from an
//! untrusted author is prose, `.loom/docs/comment-trust.md`).

use crate::champion_hold::ratification::{self, ChangedFile, Comment};
use regex::Regex;
use serde_json::Value;

/// Where an instruction set is pinned (fetched in step 2, only parsed here).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    pub repo: String,
    pub path: String,
    pub commit: String,
}

/// The `ratificationLane` block of `.loom/config.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub enabled: bool,
    pub ee: Option<Pin>,
    pub market: Option<Pin>,
    pub broker_endpoint: Option<String>,
}

impl Config {
    /// The fail-closed value: the lane never runs.
    #[must_use]
    pub fn disabled() -> Self {
        Config {
            enabled: false,
            ee: None,
            market: None,
            broker_endpoint: None,
        }
    }

    /// From the effective config document. Anything but a well-formed block
    /// with `enabled: true` yields [`Config::disabled`].
    #[must_use]
    pub fn from_effective(effective: &Value) -> Self {
        let Some(block) = crate::config_resolver::get_path(effective, "ratificationLane")
            .filter(|v| v.is_object())
        else {
            return Self::disabled();
        };
        if block.get("enabled").and_then(Value::as_bool) != Some(true) {
            return Self::disabled();
        }
        let pin = |key: &str| -> Option<Pin> {
            let p = block.get(key)?;
            let field = |f: &str| {
                p.get(f)
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                    .map(str::to_string)
            };
            Some(Pin {
                repo: field("repo")?,
                path: field("path")?,
                commit: field("commit")?,
            })
        };
        let (ee, market) = (pin("ee"), pin("market"));
        let broker_endpoint = block
            .get("brokerEndpoint")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string);
        // Both instruction pins are required; a half-configured lane is off.
        if ee.is_none() || market.is_none() {
            return Self::disabled();
        }
        Config {
            enabled: true,
            ee,
            market,
            broker_endpoint,
        }
    }
}

/// A changed file plus its content on the PR's base (for the status flip).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneFile {
    pub file: ChangedFile,
    /// `None` for an added file or when the base could not be read.
    pub base_content: Option<String>,
}

/// An open PR as the lane sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenPr {
    pub number: u64,
    pub author: String,
    pub head_sha: String,
}

/// The forge, injected.
pub trait PrSource {
    fn open_prs(&self) -> Vec<OpenPr>;
    fn files(&self, pr: u64) -> Vec<LaneFile>;
    /// Comments by trusted authors only.
    fn trusted_comments(&self, pr: u64) -> Vec<Comment>;
}

/// The two review keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Ee,
    Market,
}

/// What the finder decided for one PR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// Needs key reviews.
    Candidate { pr: u64, head_sha: String },
    /// Matches but must not be reviewed; the reason is recorded.
    Refused { pr: u64, reason: String },
}

const REQUEST_GLOB: &str = "ratification/requests/*.md";
const STATUS_FLIP_GLOBS: [&str; 3] = ["spec/**", "decisions/**", "docs/decisions/**"];

fn glob(g: &str, path: &str) -> bool {
    crate::main_health_gate::glob_matches(g, path)
}

fn status_is_ratified(content: &str) -> bool {
    Regex::new(&format!("(?is){}", ratification::STATUS))
        .ok()
        .and_then(|re| {
            re.captures(content)
                .map(|c| c[1].eq_ignore_ascii_case("ratified"))
        })
        .unwrap_or(false)
}

/// Whether the PR adds/edits a ratification request, or flips a `Status:` line
/// to ratified under a spec/decision path.
#[must_use]
pub fn matches_pr(files: &[LaneFile]) -> bool {
    files
        .iter()
        .filter(|f| f.file.status != "removed")
        .any(|f| {
            let path = f.file.filename.as_str();
            if glob(REQUEST_GLOB, path) {
                return true;
            }
            let in_scope = STATUS_FLIP_GLOBS.iter().any(|g| glob(g, path))
                && ratification::is_candidate(&f.file, &ratification::Config::default());
            in_scope
                && f.file
                    .head_content
                    .as_deref()
                    .is_some_and(status_is_ratified)
                && !f.base_content.as_deref().is_some_and(status_is_ratified)
        })
}

/// Which keys carry a marker for `head_sha` in `comments`.
#[must_use]
pub fn keys_marked(comments: &[Comment], head_sha: &str) -> Vec<Key> {
    let re =
        Regex::new(r"<!--\s*ratify:key=(ee|market)\s+sha=([0-9a-fA-F]{40})\s*-->").expect("static");
    let mut keys = Vec::new();
    for c in comments {
        for cap in re.captures_iter(&c.body) {
            if !cap[2].eq_ignore_ascii_case(head_sha) {
                continue;
            }
            let k = if &cap[1] == "ee" {
                Key::Ee
            } else {
                Key::Market
            };
            if !keys.contains(&k) {
                keys.push(k);
            }
        }
    }
    keys
}

/// Select the PRs needing key reviews. `identity` is the login the daemon's
/// forge credential resolves to; its own PRs are refused, never reviewed.
#[must_use]
pub fn find_candidates(cfg: &Config, identity: &str, src: &dyn PrSource) -> Vec<Selection> {
    if !cfg.enabled {
        return Vec::new();
    }
    let mut out = Vec::new();
    for pr in src.open_prs() {
        if !matches_pr(&src.files(pr.number)) {
            continue;
        }
        if pr.author.eq_ignore_ascii_case(identity) {
            out.push(Selection::Refused {
                pr: pr.number,
                reason: format!("authored by the dispatching identity {identity}"),
            });
            continue;
        }
        let marked = keys_marked(&src.trusted_comments(pr.number), &pr.head_sha);
        if marked.contains(&Key::Ee) && marked.contains(&Key::Market) {
            continue;
        }
        out.push(Selection::Candidate {
            pr: pr.number,
            head_sha: pr.head_sha,
        });
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn file(name: &str, head: &str, base: Option<&str>) -> LaneFile {
        LaneFile {
            file: ChangedFile {
                filename: name.into(),
                previous_filename: None,
                status: "modified".into(),
                head_content: Some(head.into()),
            },
            base_content: base.map(str::to_string),
        }
    }

    #[derive(Default)]
    struct Fake {
        prs: Vec<OpenPr>,
        files: HashMap<u64, Vec<LaneFile>>,
        comments: HashMap<u64, Vec<Comment>>,
    }

    impl PrSource for Fake {
        fn open_prs(&self) -> Vec<OpenPr> {
            self.prs.clone()
        }
        fn files(&self, pr: u64) -> Vec<LaneFile> {
            self.files.get(&pr).cloned().unwrap_or_default()
        }
        fn trusted_comments(&self, pr: u64) -> Vec<Comment> {
            self.comments.get(&pr).cloned().unwrap_or_default()
        }
    }

    fn enabled() -> Config {
        Config::from_effective(&json!({"ratificationLane": {
            "enabled": true,
            "ee": {"repo": "o/ee", "path": "i.md", "commit": "abc"},
            "market": {"repo": "o/mk", "path": "i.md", "commit": "def"},
            "brokerEndpoint": "https://broker.example"
        }}))
    }

    fn one_pr(author: &str, files: Vec<LaneFile>, comments: Vec<Comment>) -> Fake {
        let mut f = Fake::default();
        f.prs.push(OpenPr {
            number: 7,
            author: author.into(),
            head_sha: SHA.into(),
        });
        f.files.insert(7, files);
        f.comments.insert(7, comments);
        f
    }

    fn request() -> Vec<LaneFile> {
        vec![file("ratification/requests/fft.md", "x", None)]
    }

    #[test]
    fn config_unset_or_malformed_is_disabled() {
        assert_eq!(Config::from_effective(&json!({})), Config::disabled());
        assert_eq!(Config::from_effective(&json!({"ratificationLane": "yes"})), Config::disabled());
        assert_eq!(
            Config::from_effective(&json!({"ratificationLane": {"enabled": "true"}})),
            Config::disabled()
        );
        // enabled but a pin is missing: still off
        assert_eq!(
            Config::from_effective(&json!({"ratificationLane": {"enabled": true,
                "ee": {"repo": "o/ee", "path": "p", "commit": "c"}}})),
            Config::disabled()
        );
        // explicit disable
        assert!(!Config::from_effective(&json!({"ratificationLane": {"enabled": false}})).enabled);
    }

    #[test]
    fn config_parses_pins_and_endpoint() {
        let c = enabled();
        assert!(c.enabled);
        assert_eq!(c.ee.unwrap().repo, "o/ee");
        assert_eq!(c.market.unwrap().commit, "def");
        assert_eq!(c.broker_endpoint.as_deref(), Some("https://broker.example"));
    }

    #[test]
    fn disabled_lane_returns_nothing() {
        let src = one_pr("alice", request(), vec![]);
        assert!(find_candidates(&Config::disabled(), "bot[bot]", &src).is_empty());
    }

    #[test]
    fn request_file_is_candidate() {
        let src = one_pr("alice", request(), vec![]);
        assert_eq!(
            find_candidates(&enabled(), "bot[bot]", &src),
            vec![Selection::Candidate {
                pr: 7,
                head_sha: SHA.into()
            }]
        );
    }

    #[test]
    fn status_flip_is_candidate_but_unchanged_or_other_paths_are_not() {
        let flip = vec![file(
            "spec/fft.md",
            "Status: Ratified",
            Some("Status: Proposed"),
        )];
        assert!(matches_pr(&flip));
        let added = vec![file("docs/decisions/1.md", "Status: ratified", None)];
        assert!(matches_pr(&added));
        let already = vec![file(
            "spec/fft.md",
            "Status: Ratified",
            Some("Status: Ratified"),
        )];
        assert!(!matches_pr(&already));
        let proposed = vec![file("spec/fft.md", "Status: Proposed", Some("x"))];
        assert!(!matches_pr(&proposed));
        let elsewhere = vec![file("notes/fft.md", "Status: Ratified", None)];
        assert!(!matches_pr(&elsewhere));
        let nested_request = vec![file("ratification/requests/a/b.md", "x", None)];
        assert!(!matches_pr(&nested_request) || glob(REQUEST_GLOB, "ratification/requests/a/b.md"));
    }

    #[test]
    fn non_matching_pr_is_not_selected() {
        let src = one_pr("alice", vec![file("src/lib.rs", "fn main(){}", None)], vec![]);
        assert!(find_candidates(&enabled(), "bot[bot]", &src).is_empty());
    }

    #[test]
    fn removed_request_does_not_match() {
        let mut f = file("ratification/requests/fft.md", "", None);
        f.file.status = "removed".into();
        assert!(!matches_pr(&[f]));
    }

    fn marker(key: &str, sha: &str) -> Comment {
        Comment::new("loom[bot]", &format!("<!-- ratify:key={key} sha={sha} -->"))
    }

    #[test]
    fn both_keys_for_current_head_skips() {
        let src = one_pr("alice", request(), vec![marker("ee", SHA), marker("market", SHA)]);
        assert!(find_candidates(&enabled(), "bot[bot]", &src).is_empty());
    }

    #[test]
    fn one_key_or_stale_head_still_candidate() {
        let one = one_pr("alice", request(), vec![marker("ee", SHA)]);
        assert_eq!(find_candidates(&enabled(), "bot[bot]", &one).len(), 1);
        let stale = "f".repeat(40);
        let old = one_pr("alice", request(), vec![marker("ee", &stale), marker("market", &stale)]);
        assert_eq!(find_candidates(&enabled(), "bot[bot]", &old).len(), 1);
    }

    #[test]
    fn self_authored_pr_is_refused_with_reason() {
        let src = one_pr("Loom-Bot[bot]", request(), vec![]);
        let got = find_candidates(&enabled(), "loom-bot[bot]", &src);
        assert_eq!(got.len(), 1);
        match &got[0] {
            Selection::Refused { pr, reason } => {
                assert_eq!(*pr, 7);
                assert!(reason.contains("dispatching identity"));
            }
            other => panic!("expected refusal, got {other:?}"),
        }
    }
}
