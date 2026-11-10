//! The ratification reviewer lane's selection layer (#13187, step 1 of 3 of
//! #13186).
//!
//! Finds open PRs that need key reviews: exactly the PRs Champion's
//! ratification hold would hold ([`ratification::scan`] is `Hold`; one shared
//! predicate, #13553). Selection is
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

use crate::champion_hold::ratification::{self, ChangedFile, Comment, Verdict};
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
    /// The hold's detector config (`champion.ratificationPaths` / `Phrases`),
    /// so selection and Champion's hold read the same predicate.
    pub hold: ratification::Config,
}

impl Config {
    /// The fail-closed value: the lane never runs.
    #[must_use]
    pub fn disabled() -> Self {
        Config {
            enabled: false,
            ee: None,
            market: None,
            hold: ratification::Config::default(),
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
            hold: ratification::Config::from_effective(effective),
        }
    }
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
    /// The PR's change list with head content, as the hold's [`ratification::scan`]
    /// reads it.
    fn files(&self, pr: u64, hold: &ratification::Config) -> Vec<ChangedFile>;
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

/// Whether Champion's ratification hold would hold this PR. An unreadable
/// candidate file is not selected (fail closed: never marker an unread file)
/// and is logged.
#[must_use]
pub fn matches_pr(pr: u64, files: &[ChangedFile], hold: &ratification::Config) -> bool {
    match ratification::scan(files, hold) {
        Verdict::Hold(_) => true,
        Verdict::Clear => false,
        Verdict::Unreadable(paths) => {
            log::warn!("ratification_lane: PR #{pr} not selected; unreadable: {paths:?}");
            false
        }
    }
}

/// The sweep ids of the builder sessions recorded in `comments` by PR-lease
/// records (`<!-- loom:pr-lease[-released] role=builder ... sweep=<id> ... -->`).
#[must_use]
pub fn builder_sessions(comments: &[Comment]) -> Vec<String> {
    let re =
        Regex::new(r"<!--\s*loom:pr-lease(?:-released)?\s+role=builder\b[^>]*?\bsweep=([^\s>]+)")
            .expect("static");
    comments
        .iter()
        .flat_map(|c| re.captures_iter(&c.body).map(|m| m[1].to_string()))
        .collect()
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

/// Select the PRs needing key reviews. Independence is by session, not author
/// login: in a fleet the Builder opens the PR as the same identity that
/// dispatches the lane. A PR is refused only when `lane_session` is the
/// session recorded as having built it.
#[must_use]
pub fn find_candidates(
    cfg: &Config,
    lane_session: Option<&str>,
    src: &dyn PrSource,
) -> Vec<Selection> {
    if !cfg.enabled {
        return Vec::new();
    }
    let mut out = Vec::new();
    for pr in src.open_prs() {
        if !matches_pr(pr.number, &src.files(pr.number, &cfg.hold), &cfg.hold) {
            continue;
        }
        let comments = src.trusted_comments(pr.number);
        if let Some(sid) =
            lane_session.filter(|l| builder_sessions(&comments).iter().any(|b| b == l))
        {
            out.push(Selection::Refused {
                pr: pr.number,
                reason: format!("lane session {sid} is the session that built the PR"),
            });
            continue;
        }
        let marked = keys_marked(&comments, &pr.head_sha);
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
/// `verdict` is the tail of the session's output (#13502), appended after the
/// marker line. It is bounded to [`MAX_VERDICT_CHARS`] (keeping the end, with
/// an explicit truncation notice) and its `<!--` / `-->` are escaped so it can
/// never form or alter a marker that [`keys_marked`] would parse.
#[must_use]
pub fn marker_body(key: Key, head_sha: &str, reviewer: &str, verdict: Option<&str>) -> String {
    let mut body = format!(
        "<!-- ratify:key={} sha={head_sha} -->\nreviewer={reviewer}\nReviewed head: {head_sha}\n",
        key.wire()
    );
    if let Some(text) = verdict.map(str::trim).filter(|t| !t.is_empty()) {
        body.push_str("\nVerdict:\n\n");
        body.push_str(&bound_verdict(text));
        body.push('\n');
    }
    body
}

/// Longest verdict text posted, in characters.
pub const MAX_VERDICT_CHARS: usize = 4000;

fn bound_verdict(text: &str) -> String {
    let escaped = text.replace("<!--", "&lt;!--").replace("-->", "--&gt;");
    let total = escaped.chars().count();
    if total <= MAX_VERDICT_CHARS {
        return escaped;
    }
    let tail: String = escaped.chars().skip(total - MAX_VERDICT_CHARS).collect();
    format!("[truncated: first {} characters omitted]\n{tail}", total - MAX_VERDICT_CHARS)
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
                    .post(pr, &marker_body(key, head_sha, role_name(key), dispatched.output(key)))
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

/// Per-PR key results of one lane pass.
pub type LaneResults = Vec<(u64, Vec<(Key, KeyPost)>)>;

/// One lane pass: select, dispatch both key sessions per candidate, post
/// markers for the keys that succeeded. A disabled `cfg` returns at once
/// without touching any source. Returns the per-PR key results.
pub fn run_lane(
    cfg: &Config,
    lane_session: Option<&str>,
    target_repo: &str,
    prs: &dyn PrSource,
    instructions: &dyn super::ratification_dispatch::InstructionSource,
    sink: &dyn CommentSink,
    runner: &mut dyn super::RoleInvocationRunner,
) -> LaneResults {
    run_lane_counted(cfg, lane_session, target_repo, prs, instructions, sink, runner).0
}

/// [`run_lane`] plus the number of PRs selection refused (the tick's `skipped`).
pub fn run_lane_counted(
    cfg: &Config,
    lane_session: Option<&str>,
    target_repo: &str,
    prs: &dyn PrSource,
    instructions: &dyn super::ratification_dispatch::InstructionSource,
    sink: &dyn CommentSink,
    runner: &mut dyn super::RoleInvocationRunner,
) -> (LaneResults, usize) {
    let (true, Some(ee), Some(market)) = (cfg.enabled, cfg.ee.as_ref(), cfg.market.as_ref()) else {
        return (Vec::new(), 0);
    };
    let mut out = Vec::new();
    let mut refused = 0;
    for sel in find_candidates(cfg, lane_session, prs) {
        match sel {
            Selection::Refused { pr, reason } => {
                refused += 1;
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
    (out, refused)
}

/// What one lane tick did: a distinct `reason` for every way a tick can end,
/// so "never ran" and "nothing to do" are different records (#13707).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickOutcome {
    /// `disabled` | `repo_unresolved` | `in_flight` | `nothing_to_do` | `ran`.
    pub reason: &'static str,
    /// Candidates selected for key review.
    pub selected: usize,
    /// PRs selection refused.
    pub skipped: usize,
    /// Candidates whose key sessions were dispatched.
    pub ran: usize,
    /// Key markers posted.
    pub posted: usize,
}

impl TickOutcome {
    fn bare(reason: &'static str) -> Self {
        Self {
            reason,
            selected: 0,
            skipped: 0,
            ran: 0,
            posted: 0,
        }
    }

    /// Summarize a [`run_lane_counted`] result.
    #[must_use]
    pub fn from_run(results: &[(u64, Vec<(Key, KeyPost)>)], skipped: usize) -> Self {
        let posted = results
            .iter()
            .flat_map(|(_, keys)| keys)
            .filter(|(_, p)| *p == KeyPost::Posted)
            .count();
        Self {
            reason: if results.is_empty() {
                "nothing_to_do"
            } else {
                "ran"
            },
            selected: results.len(),
            skipped,
            ran: results.len(),
            posted,
        }
    }
}

/// Log the tick and emit its span (when spans are exported on this host).
fn record_tick(root: &std::path::Path, source: &str, o: &TickOutcome) {
    log::info!(
        "ratification_lane: tick root={} source={source} reason={} selected={} skipped={} ran={} posted={}",
        root.display(),
        o.reason,
        o.selected,
        o.skipped,
        o.ran,
        o.posted
    );
    if crate::observability::ops::spans_exported() {
        crate::observability::ops::emit_span(tick_span(root, source, o, chrono::Utc::now()));
    }
}

/// Span attribute keys on `ratification_lane.tick`. They must be namespaced
/// and registered in `bounded_attributes()`; bare keys are dropped at export.
pub const KEY_REASON: &str = "loom.ratification.reason";
pub const KEY_SOURCE: &str = "loom.ratification.source";
pub const KEY_SELECTED: &str = "loom.ratification.selected";
pub const KEY_SKIPPED: &str = "loom.ratification.skipped";
pub const KEY_RAN: &str = "loom.ratification.ran";
pub const KEY_POSTED: &str = "loom.ratification.posted";
pub const KEYS: [&str; 6] = [
    KEY_REASON,
    KEY_SOURCE,
    KEY_SELECTED,
    KEY_SKIPPED,
    KEY_RAN,
    KEY_POSTED,
];

/// The `ratification_lane.tick` span: an instant, its own root.
#[must_use]
pub fn tick_span(
    root: &std::path::Path,
    source: &str,
    o: &TickOutcome,
    at: chrono::DateTime<chrono::Utc>,
) -> crate::telemetry::trace::SpanRecord {
    use crate::telemetry::trace::{
        instant, SpanName, SpanRecord, SpanStatus, TraceAttributes, TraceContext,
    };
    let name = SpanName::RatificationLaneTick;
    let root_s = root.display().to_string();
    let context = TraceContext::derived(name.as_str(), &[&root_s, source, &instant(at)]);
    let mut attributes = TraceAttributes::new();
    let mut put = |k: &str, v: String| {
        attributes.insert(k.to_string(), v);
    };
    put(KEY_REASON, o.reason.to_string());
    put(KEY_SOURCE, source.to_string());
    put(KEY_SELECTED, o.selected.to_string());
    put(KEY_SKIPPED, o.skipped.to_string());
    put(KEY_RAN, o.ran.to_string());
    put(KEY_POSTED, o.posted.to_string());
    crate::telemetry::trace::provenance::stamp(&mut attributes);
    SpanRecord {
        context,
        parent_span_id: None,
        name,
        started_at: at,
        ended_at: at,
        status: SpanStatus::Ok,
        attributes,
        events: Vec::new(),
        links: Vec::new(),
    }
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
        // `--paginate` reads every page (#13502); `parse_listing` flattens the
        // concatenated arrays. Any failed or unparseable read is `None`.
        let gh = std::path::PathBuf::from(crate::gh_invocation::gh_bin());
        let inv = crate::claim_reconciliation::gh_call::read("ratification.pr_get", &gh, self.root)
            .args(["api", endpoint, "--paginate"]);
        let stdout = crate::claim_reconciliation::gh_call::ok_stdout(inv)?;
        crate::comment_trust::parse_listing(&stdout).map(Value::Array)
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
    /// Every open PR; an unreadable list is "no candidates".
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

    /// Head content is read only for paths the hold would scan.
    fn files(&self, pr: u64, hold: &ratification::Config) -> Vec<ChangedFile> {
        let Some(Value::Array(list)) =
            self.get(&format!("repos/{}/pulls/{pr}/files?per_page=100", self.nwo))
        else {
            return Vec::new();
        };
        let (head, _base) = self.refs.borrow().get(&pr).cloned().unwrap_or_default();
        list.iter()
            .filter_map(|f| {
                let mut file = ChangedFile {
                    filename: f.get("filename").and_then(Value::as_str)?.to_string(),
                    previous_filename: f
                        .get("previous_filename")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    status: f.get("status").and_then(Value::as_str)?.to_string(),
                    head_content: None,
                };
                if file.status != "removed"
                    && !head.is_empty()
                    && ratification::is_candidate(&file, hold)
                {
                    file.head_content = self.raw(&file.filename, &head);
                }
                Some(file)
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
    fn last_output(&self) -> Option<String> {
        self.0.last_output()
    }
}

/// Roots with a lane tick in flight, process-wide: the role-tick and
/// step-dispatch entry points share it, so a root is ticked by at most one
/// path at a time (#13707).
fn in_flight() -> &'static std::sync::Mutex<std::collections::HashSet<std::path::PathBuf>> {
    static SET: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashSet<std::path::PathBuf>>,
    > = std::sync::OnceLock::new();
    SET.get_or_init(Default::default)
}

struct InFlight(std::path::PathBuf);

impl InFlight {
    fn acquire(root: &std::path::Path) -> Option<Self> {
        let mut set = in_flight()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        set.insert(root.to_path_buf())
            .then(|| Self(root.to_path_buf()))
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        in_flight()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.0);
    }
}

/// The role-tick entry point: does nothing unless `ratificationLane.enabled`
/// is set for `root`, and fails closed when the repo cannot be resolved. The
/// lane's own session id (`LOOM_SWEEP_ID`) feeds the builder-session check.
/// Every tick, including a no-op one, logs and emits its outcome (#13707).
pub fn tick_for_root<R: super::RoleInvocationRunner + ?Sized>(
    root: &std::path::Path,
    runner: &mut R,
) -> TickOutcome {
    tick_for_root_from(root, runner, "role_tick")
}

fn tick_for_root_from<R: super::RoleInvocationRunner + ?Sized>(
    root: &std::path::Path,
    runner: &mut R,
    source: &str,
) -> TickOutcome {
    let outcome = tick_inner(root, runner);
    record_tick(root, source, &outcome);
    outcome
}

fn tick_inner<R: super::RoleInvocationRunner + ?Sized>(
    root: &std::path::Path,
    runner: &mut R,
) -> TickOutcome {
    let effective = crate::config_resolver::resolve_effective_config(root);
    let cfg = Config::from_effective(&effective);
    if !cfg.enabled {
        return TickOutcome::bare("disabled");
    }
    let policy = crate::comment_trust::TrustPolicy::for_root(root);
    let Some(nwo) = crate::credential_preflight::nwo_from_git_remote(root) else {
        log::warn!("ratification_lane: repo unresolved; skipping");
        return TickOutcome::bare("repo_unresolved");
    };
    let Some(_guard) = InFlight::acquire(root) else {
        return TickOutcome::bare("in_flight");
    };
    let lane_session = std::env::var("LOOM_SWEEP_ID")
        .ok()
        .filter(|s| !s.is_empty());
    let prs = GhPrSource::new(root, &nwo, &policy);
    let instructions = super::ratification_dispatch::GhInstructionSource { root };
    let sink = GhCommentSink { root, nwo: &nwo };
    let (results, skipped) = run_lane_counted(
        &cfg,
        lane_session.as_deref(),
        &nwo,
        &prs,
        &instructions,
        &sink,
        &mut RunnerRef(runner),
    );
    TickOutcome::from_run(&results, skipped)
}

/// How often the step-dispatch driver ticks the lane for a root.
pub const STEP_TICK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);

/// Whether `root` is due a step-dispatch tick at `now`, recording it as ticked
/// when so. Pure over `last` so the cadence is testable.
fn step_tick_due(
    last: &mut std::collections::HashMap<std::path::PathBuf, std::time::Instant>,
    root: &std::path::Path,
    now: std::time::Instant,
    interval: std::time::Duration,
) -> bool {
    match last.get(root) {
        Some(t) if now.duration_since(*t) < interval => false,
        _ => {
            last.insert(root.to_path_buf(), now);
            true
        }
    }
}

#[cfg(test)]
pub(crate) fn step_tick_due_for_test(
    last: &mut std::collections::HashMap<std::path::PathBuf, std::time::Instant>,
    root: &std::path::Path,
    now: std::time::Instant,
) -> bool {
    step_tick_due(last, root, now, STEP_TICK_INTERVAL)
}

/// The step-dispatch entry point (#13707): a fleet host that dispatches by
/// steps never reaches the champion role tick, so the driver calls this each
/// tick. Per root, at most once per [`STEP_TICK_INTERVAL`], on its own thread
/// (the lane shells out and runs reviewer sessions, which must not stall the
/// driver tick). The shared in-flight guard stops it overlapping a role tick.
pub fn spawn_step_ticks(roots: &[std::path::PathBuf]) {
    static LAST: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, std::time::Instant>>,
    > = std::sync::OnceLock::new();
    let last = LAST.get_or_init(Default::default);
    for root in roots {
        let due = {
            let mut m = last
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            step_tick_due(&mut m, root, std::time::Instant::now(), STEP_TICK_INTERVAL)
        };
        if !due {
            continue;
        }
        let root = root.clone();
        let spawned = std::thread::Builder::new()
            .name("ratification-lane".into())
            .spawn(move || {
                let mut runner = super::ScriptRoleInvocationRunner::new(root.clone());
                tick_for_root_from(&root, &mut runner, "step_dispatch");
            });
        if let Err(e) = spawned {
            log::warn!("ratification_lane: could not spawn step tick: {e}");
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn file(name: &str, head: &str) -> ChangedFile {
        ChangedFile {
            filename: name.into(),
            previous_filename: None,
            status: "modified".into(),
            head_content: Some(head.into()),
        }
    }

    const DECLARES: &str = "This record is ratified on merge.";

    #[derive(Default)]
    struct Fake {
        prs: Vec<OpenPr>,
        files: HashMap<u64, Vec<ChangedFile>>,
        comments: HashMap<u64, Vec<Comment>>,
    }

    impl PrSource for Fake {
        fn open_prs(&self) -> Vec<OpenPr> {
            self.prs.clone()
        }
        fn files(&self, pr: u64, _hold: &ratification::Config) -> Vec<ChangedFile> {
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

    fn one_pr(author: &str, files: Vec<ChangedFile>, comments: Vec<Comment>) -> Fake {
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

    fn request() -> Vec<ChangedFile> {
        vec![file("ratification/requests/fft.md", DECLARES)]
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
        assert!(find_candidates(&Config::disabled(), None, &src).is_empty());
    }

    #[test]
    fn request_file_is_candidate() {
        let src = one_pr("alice", request(), vec![]);
        assert_eq!(
            find_candidates(&enabled(), None, &src),
            vec![Selection::Candidate {
                pr: 7,
                head_sha: SHA.into()
            }]
        );
    }

    #[test]
    fn selection_is_the_holds_predicate() {
        let h = ratification::Config::default();
        // Status line with a ratification clause, and an additive record with a phrase.
        let status = vec![file(
            "spec/fft.md",
            "Status: Proposed\nRatification needs operator approval.",
        )];
        assert!(matches_pr(1, &status, &h));
        let record = vec![file("decisions/7.md", "Ratified by the operator on merge.")];
        assert!(matches_pr(1, &record, &h));
        // A bare status line, a non-candidate path, or no declaration: not selected.
        assert!(!matches_pr(1, &[file("spec/fft.md", "Status: Ratified")], &h));
        assert!(!matches_pr(1, &[file("notes/x.md", DECLARES)], &h));
        assert!(!matches_pr(1, &[file("spec/x.md", "nothing here")], &h));
        // The hold's own path extension applies.
        let ext = ratification::Config {
            paths: vec!["chips/**".into()],
            phrases: vec![],
        };
        assert!(matches_pr(1, &[file("chips/a.md", DECLARES)], &ext));
        assert!(!matches_pr(1, &[file("chips/a.md", DECLARES)], &h));
    }

    #[test]
    fn unreadable_candidate_is_not_selected() {
        let mut f = file("spec/fft.md", "");
        f.head_content = None;
        assert!(!matches_pr(1, &[f], &ratification::Config::default()));
    }

    #[test]
    fn removed_request_does_not_match() {
        let mut f = file("ratification/requests/fft.md", DECLARES);
        f.status = "removed".into();
        assert!(!matches_pr(1, &[f], &ratification::Config::default()));
    }

    #[test]
    fn non_matching_pr_is_not_selected() {
        let src = one_pr("alice", vec![file("src/lib.rs", "fn main(){}")], vec![]);
        assert!(find_candidates(&enabled(), None, &src).is_empty());
    }

    fn marker(key: &str, sha: &str) -> Comment {
        Comment::new("loom[bot]", &format!("<!-- ratify:key={key} sha={sha} -->"))
    }

    #[test]
    fn both_keys_for_current_head_skips() {
        let src = one_pr("alice", request(), vec![marker("ee", SHA), marker("market", SHA)]);
        assert!(find_candidates(&enabled(), None, &src).is_empty());
    }

    #[test]
    fn one_key_or_stale_head_still_candidate() {
        let one = one_pr("alice", request(), vec![marker("ee", SHA)]);
        assert_eq!(find_candidates(&enabled(), None, &one).len(), 1);
        let stale = "f".repeat(40);
        let old = one_pr("alice", request(), vec![marker("ee", &stale), marker("market", &stale)]);
        assert_eq!(find_candidates(&enabled(), None, &old).len(), 1);
    }

    #[test]
    fn fleet_authored_pr_is_selected() {
        // Author equals the dispatching identity: no longer a reason to refuse.
        let src = one_pr("loom-bot[bot]", request(), vec![]);
        assert_eq!(
            find_candidates(&enabled(), Some("lane-1"), &src),
            vec![Selection::Candidate {
                pr: 7,
                head_sha: SHA.into()
            }]
        );
    }

    #[test]
    fn lane_session_that_built_the_pr_is_refused() {
        let lease = Comment::new(
            "loom-bot[bot]",
            "<!-- loom:pr-lease-released role=builder host=h sweep=step-builder-9-1 -->",
        );
        let src = one_pr("loom-bot[bot]", request(), vec![lease]);
        let got = find_candidates(&enabled(), Some("step-builder-9-1"), &src);
        match got.as_slice() {
            [Selection::Refused { pr: 7, reason }] => assert!(reason.contains("built the PR")),
            other => panic!("expected refusal, got {other:?}"),
        }
        // A different lane session is fine.
        assert_eq!(find_candidates(&enabled(), Some("lane-1"), &src).len(), 1);
    }
}
