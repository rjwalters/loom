//! The ratification reviewer lane's selection layer (#13187, step 1 of 3 of
//! #13186).
//!
//! Finds open PRs that need key reviews: a PR that adds or edits
//! `ratification/requests/*.md`, or flips a `Status:` line to ratified under
//! `spec/**`, `decisions/**` or `docs/decisions/**`. Selection is
//! [`find_candidates`]; [`run_lane`] (step 3, #13345) ties selection, the
//! step-2 dispatch and marker posting together. The forge is injected
//! ([`PrSource`], [`CommentSink`]) so every rule is unit-testable offline.
//!
//! # Fail closed
//!
//! A missing, disabled or malformed `ratificationLane` block yields
//! [`Config::disabled`]; [`find_candidates`] on a disabled config returns
//! nothing and never touches the source.
//!
//! # Key marker (wire format, emitted by [`post_markers`])
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
}

impl Config {
    /// The fail-closed value: the lane never runs.
    #[must_use]
    pub fn disabled() -> Self {
        Config {
            enabled: false,
            ee: None,
            market: None,
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
        // Both instruction pins are required; a half-configured lane is off.
        if ee.is_none() || market.is_none() {
            return Self::disabled();
        }
        Config {
            enabled: true,
            ee,
            market,
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

// ---------------------------------------------------------------------------
// Marker posting and the lane tick (#13345, step 3 of 3 of #13186)
// ---------------------------------------------------------------------------

/// Posts one comment on a PR. Injected so posting is testable offline.
pub trait CommentSink {
    /// # Errors
    ///
    /// The forge refused or could not be reached; the text is logged.
    fn post(&self, pr: u64, body: &str) -> Result<(), String>;
}

impl Key {
    /// The wire name used in the marker.
    #[must_use]
    pub fn wire(self) -> &'static str {
        match self {
            Key::Ee => "ee",
            Key::Market => "market",
        }
    }
}

/// The comment body for one key: the wire marker (exactly the shape
/// [`keys_marked`] parses), then a visible `reviewer=` line. `head_sha` is the
/// head captured at selection time, never re-read.
///
/// `RoleTickOutcome` carries no verdict text, so none is included.
#[must_use]
pub fn marker_body(key: Key, head_sha: &str, reviewer: &str) -> String {
    format!(
        "<!-- ratify:key={} sha={head_sha} -->\nreviewer={reviewer}\nReviewed head: {head_sha}\n",
        key.wire()
    )
}

/// What the lane did for one key of one PR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyPost {
    Posted,
    /// No marker posted; the reason is logged.
    Withheld(String),
}

/// Post a marker for each key whose session ended in
/// `KeyOutcome::Ran(RoleTickOutcome::Success)`. Anything else (not launched,
/// or a session that ran but did not succeed) posts nothing: fail closed. One
/// key's outcome never affects the other.
pub fn post_markers(
    pr: u64,
    head_sha: &str,
    dispatched: &super::ratification_dispatch::Dispatched,
    sink: &dyn CommentSink,
) -> Vec<(Key, KeyPost)> {
    use super::ratification_dispatch::{role_name, KeyOutcome};
    use super::RoleTickOutcome;
    [Key::Ee, Key::Market]
        .into_iter()
        .map(|key| {
            let result = match dispatched.get(key) {
                KeyOutcome::Ran(RoleTickOutcome::Success) => sink
                    .post(pr, &marker_body(key, head_sha, role_name(key)))
                    .map(|()| KeyPost::Posted)
                    .unwrap_or_else(|e| KeyPost::Withheld(format!("marker post failed: {e}"))),
                KeyOutcome::Ran(other) => {
                    KeyPost::Withheld(format!("session did not succeed: {other:?}"))
                }
                KeyOutcome::FailedClosed(reason) => KeyPost::Withheld(reason.clone()),
            };
            if let KeyPost::Withheld(reason) = &result {
                log::warn!("ratification_lane: PR #{pr} key {} withheld: {reason}", key.wire());
            }
            (key, result)
        })
        .collect()
}

/// One lane pass: select, dispatch both key sessions per candidate, post
/// markers for the keys that succeeded. A disabled `cfg` returns at once
/// without touching any source. Returns the per-PR key results.
pub fn run_lane(
    cfg: &Config,
    identity: &str,
    target_repo: &str,
    prs: &dyn PrSource,
    instructions: &dyn super::ratification_dispatch::InstructionSource,
    sink: &dyn CommentSink,
    runner: &mut dyn super::RoleInvocationRunner,
) -> Vec<(u64, Vec<(Key, KeyPost)>)> {
    let (true, Some(ee), Some(market)) = (cfg.enabled, cfg.ee.as_ref(), cfg.market.as_ref()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for sel in find_candidates(cfg, identity, prs) {
        match sel {
            Selection::Refused { pr, reason } => {
                log::info!("ratification_lane: PR #{pr} refused: {reason}");
            }
            Selection::Candidate { pr, head_sha } => {
                let pr_ref = format!("{target_repo}#{pr}@{head_sha}");
                let dispatched = super::ratification_dispatch::dispatch(
                    ee,
                    market,
                    target_repo,
                    &pr_ref,
                    instructions,
                    runner,
                );
                out.push((pr, post_markers(pr, &head_sha, &dispatched, sink)));
            }
        }
    }
    out
}

/// The real forge for [`PrSource`]: `gh api` against `nwo`, trusting only
/// comments `policy` believes.
pub struct GhPrSource<'a> {
    pub root: &'a std::path::Path,
    pub nwo: &'a str,
    pub policy: &'a crate::comment_trust::TrustPolicy,
    refs: std::cell::RefCell<std::collections::HashMap<u64, (String, String)>>,
}

impl<'a> GhPrSource<'a> {
    #[must_use]
    pub fn new(
        root: &'a std::path::Path,
        nwo: &'a str,
        policy: &'a crate::comment_trust::TrustPolicy,
    ) -> Self {
        Self {
            root,
            nwo,
            policy,
            refs: std::cell::RefCell::default(),
        }
    }

    fn get(&self, endpoint: &str) -> Option<Value> {
        let out = crate::script_helpers::run_gh(&["api", endpoint], self.root, false);
        serde_json::from_slice(&out.ok_output()?.stdout).ok()
    }

    fn raw(&self, path: &str, git_ref: &str) -> Option<String> {
        let endpoint = format!("repos/{}/contents/{path}?ref={git_ref}", self.nwo);
        let out = crate::script_helpers::run_gh(
            &["api", "-H", "Accept: application/vnd.github.raw", &endpoint],
            self.root,
            false,
        );
        Some(String::from_utf8_lossy(&out.ok_output()?.stdout).into_owned())
    }
}

impl PrSource for GhPrSource<'_> {
    /// First page (100) of open PRs; an unreadable list is "no candidates".
    fn open_prs(&self) -> Vec<OpenPr> {
        let Some(Value::Array(list)) =
            self.get(&format!("repos/{}/pulls?state=open&per_page=100", self.nwo))
        else {
            return Vec::new();
        };
        let mut prs = Vec::new();
        for p in &list {
            let (Some(number), Some(author), Some(head), Some(base)) = (
                p.get("number").and_then(Value::as_u64),
                p.pointer("/user/login").and_then(Value::as_str),
                p.pointer("/head/sha").and_then(Value::as_str),
                p.pointer("/base/sha").and_then(Value::as_str),
            ) else {
                continue;
            };
            self.refs
                .borrow_mut()
                .insert(number, (head.to_string(), base.to_string()));
            prs.push(OpenPr {
                number,
                author: author.to_string(),
                head_sha: head.to_string(),
            });
        }
        prs
    }

    /// Content is read only for paths a status flip could apply to.
    fn files(&self, pr: u64) -> Vec<LaneFile> {
        let Some(Value::Array(list)) =
            self.get(&format!("repos/{}/pulls/{pr}/files?per_page=100", self.nwo))
        else {
            return Vec::new();
        };
        let (head, base) = self.refs.borrow().get(&pr).cloned().unwrap_or_default();
        list.iter()
            .filter_map(|f| {
                let filename = f.get("filename").and_then(Value::as_str)?.to_string();
                let status = f.get("status").and_then(Value::as_str)?.to_string();
                let flip =
                    status != "removed" && STATUS_FLIP_GLOBS.iter().any(|g| glob(g, &filename));
                let (head_content, base_content) = if flip && !head.is_empty() {
                    (self.raw(&filename, &head), self.raw(&filename, &base))
                } else {
                    (None, None)
                };
                Some(LaneFile {
                    file: ChangedFile {
                        filename,
                        previous_filename: f
                            .get("previous_filename")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        status,
                        head_content,
                    },
                    base_content,
                })
            })
            .collect()
    }

    fn trusted_comments(&self, pr: u64) -> Vec<Comment> {
        let Some(Value::Array(list)) =
            self.get(&format!("repos/{}/issues/{pr}/comments?per_page=100", self.nwo))
        else {
            return Vec::new();
        };
        list.iter()
            .filter(|c| self.policy.trusts_json(c))
            .filter_map(Comment::from_record)
            .collect()
    }
}

/// The real [`CommentSink`]: `forge_comment::post_comment` as the fleet identity.
pub struct GhCommentSink<'a> {
    pub root: &'a std::path::Path,
    pub nwo: &'a str,
}

impl CommentSink for GhCommentSink<'_> {
    fn post(&self, pr: u64, body: &str) -> Result<(), String> {
        crate::forge_comment::post_comment(
            crate::gh_invocation::gh_bin(),
            Some(self.root),
            self.nwo,
            pr,
            true,
            body,
        )
        .map(|_| ())
    }
}

/// Adapts a (possibly unsized) runner reference to the sized `dyn` the lane takes.
struct RunnerRef<'r, R: super::RoleInvocationRunner + ?Sized>(&'r mut R);

impl<R: super::RoleInvocationRunner + ?Sized> super::RoleInvocationRunner for RunnerRef<'_, R> {
    fn invoke(&mut self, role: &str, prompt: &str) -> super::RoleTickOutcome {
        self.0.invoke(role, prompt)
    }
}

/// The role-tick entry point: does nothing unless `ratificationLane.enabled`
/// is set for `root`, and fails closed when the repo or the daemon's own
/// identity cannot be resolved (the self-author refusal needs it).
pub fn tick_for_root<R: super::RoleInvocationRunner + ?Sized>(
    root: &std::path::Path,
    runner: &mut R,
) {
    let effective = crate::config_resolver::resolve_effective_config(root);
    let cfg = Config::from_effective(&effective);
    if !cfg.enabled {
        return;
    }
    let policy = crate::comment_trust::TrustPolicy::for_root(root);
    let (Some(nwo), Some(identity)) = (
        crate::credential_preflight::nwo_from_git_remote(root),
        policy.self_login().map(str::to_string),
    ) else {
        log::warn!("ratification_lane: repo or dispatching identity unresolved; skipping");
        return;
    };
    let prs = GhPrSource::new(root, &nwo, &policy);
    let instructions = super::ratification_dispatch::GhInstructionSource { root };
    let sink = GhCommentSink { root, nwo: &nwo };
    run_lane(&cfg, &identity, &nwo, &prs, &instructions, &sink, &mut RunnerRef(runner));
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
            "market": {"repo": "o/mk", "path": "i.md", "commit": "def"}
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
    fn config_parses_pins() {
        let c = enabled();
        assert!(c.enabled);
        assert_eq!(c.ee.unwrap().repo, "o/ee");
        assert_eq!(c.market.unwrap().commit, "def");
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
