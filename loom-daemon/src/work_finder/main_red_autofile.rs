//! A red `main` files its own fix issue (#11766, operator ruling 3 on
//! 2AMLogic/2am#4344).
//!
//! The red-main-fix lane ([`super::main_red_fix`]) only reorders and admits an
//! issue that carries [`MAIN_RED_FIX_MARKER`]; nothing used to file one, so a
//! red `main` waited for a person. While a repo's `main` is **verified red**
//! this module makes sure exactly one open "Fix main CI" issue exists for it,
//! and closes that issue again once `main` is verified green.
//!
//! # The verdict
//!
//! The same two sources the lane reads ([`RedMainLane`]):
//!
//! - **Gate on.** Red is [`RedMainLane::verified_red`] (the main-health gate's
//!   halt). Green is "not halted, and the gate's latest completed verdict was
//!   green" ([`note_gate_outcome`], fed by the gate loop, which also keeps the
//!   tail of a red run's output for the issue body). Before the gate's first
//!   verdict in this process the answer is unknown, and nothing happens.
//! - **Gate off** ([`RedMainLane::gate_disabled`]). The newest default-branch
//!   commit's workflow runs ([`ci_verdict`]): any run concluded `failure`,
//!   `timed_out` or `startup_failure` is red (the lane's own test,
//!   [`super::main_red_fix::latest_run_is_failure`]); every run completed with
//!   none failed and at least one success is green; anything else (runs still
//!   going, no runs, an unreadable listing) is unknown.
//!
//! Unknown never files and never closes.
//!
//! # The issue
//!
//! An open issue counts as the auto-filed fix when its body carries both
//! [`MAIN_RED_FIX_MARKER`] and [`MAIN_RED_AUTO_MARKER`] at the start of a line
//! and it is labelled `loom:very-important` (the listing reads that label, so
//! only someone who can label issues can make one count). [`decide`] is the
//! whole policy, pure:
//!
//! - red, none open: create `Fix red main CI (<short sha>)` with `loom:issue`
//!   and `loom:very-important`, both markers, the failing SHA, what failed and
//!   a run link (or the gate output tail);
//! - red, one open: reuse it; when the failing SHA moved, comment once naming
//!   the newer SHA;
//! - green: close each auto-filed issue with a comment naming the green SHA,
//!   unless a PR is open against it;
//! - more than one open (two hosts filed at once): keep the oldest, close the
//!   rest as duplicates of it.
//!
//! An issue a person filed with only the fix marker is never counted, reused
//! or closed here.
//!
//! # Many hosts
//!
//! Every host sees the same red `main`. Each one lists before it creates, so
//! the forge is the lock; two hosts that list in the same instant can both
//! create, and the next reconcile on any host closes the newer one. The issue
//! is filed through the daemon's own `gh` credential (its App identity), which
//! [`crate::comment_trust`] trusts, and only into a repo [`crate::write_scope`]
//! says this installation manages and can write (#9548).
//!
//! # Cost
//!
//! At most one reconcile per repo per [`DEFAULT_INTERVAL`]
//! (`autonomous.workFinder.redMainAutofile.intervalSecs`), sooner only when
//! the gate's verdict changes. A reconcile is one CI read (gate off), one
//! issue listing, and writes only when something changes. A green repo whose
//! last listing found nothing is listed again only every
//! [`GREEN_RELIST_EVERY`]. `autonomous.workFinder.redMainAutofile.enabled:
//! false` turns it off. The rate-limit breaker skips it like any forge read.
//!
//! Each create, reuse, close and duplicate close is one `main_red.autofile`
//! span (repo, SHA, action, issue number).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::main_red_fix::{RedMainLane, MAIN_RED_FIX_MARKER};
use crate::cmd_out::CmdOutcome;

/// The body marker that says the daemon filed this fix issue itself.
pub const MAIN_RED_AUTO_MARKER: &str = "<!-- loom:main-red-auto -->";

/// Prefix of the marker carrying the failing SHA an issue or comment names:
/// `<!-- loom:main-red-sha <sha> -->`.
const SHA_MARKER_PREFIX: &str = "<!-- loom:main-red-sha ";

/// The labels a created issue carries: ready for dispatch, and the
/// very-important level (the label the listing also reads).
pub const AUTO_LABELS: [&str; 2] = ["loom:issue", "loom:very-important"];

/// Default time between reconciles of one repo.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(600);

/// How long a green repo whose last listing found no auto-filed issue goes
/// before it is listed again (to close an issue another host filed).
pub const GREEN_RELIST_EVERY: Duration = Duration::from_secs(3600);

/// Longest gate output tail written into an issue body, in characters.
const MAX_TAIL_CHARS: usize = 3000;

/// Where a verdict came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The local main-health gate.
    Gate,
    /// The default-branch CI conclusion (gate off).
    Ci,
}

impl Source {
    fn label(self) -> &'static str {
        match self {
            Source::Gate => "gate",
            Source::Ci => "ci",
        }
    }
}

/// What a red verdict knows about the failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedEvidence {
    pub source: Source,
    /// The failing commit (full SHA), when known.
    pub sha: Option<String>,
    /// The failing workflows, or the gate.
    pub failing: Vec<String>,
    /// A link to a failing run.
    pub run_url: Option<String>,
    /// The tail of the gate's output (gate verdicts only).
    pub gate_tail: Option<String>,
}

/// This tick's verdict on a repo's `main`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Red(RedEvidence),
    Green { source: Source, sha: Option<String> },
    Unknown,
}

impl Verdict {
    fn kind(&self) -> u8 {
        match self {
            Verdict::Red(_) => 1,
            Verdict::Green { .. } => 2,
            Verdict::Unknown => 0,
        }
    }

    fn sha(&self) -> Option<&str> {
        match self {
            Verdict::Red(e) => e.sha.as_deref(),
            Verdict::Green { sha, .. } => sha.as_deref(),
            Verdict::Unknown => None,
        }
    }

    fn source(&self) -> Option<Source> {
        match self {
            Verdict::Red(e) => Some(e.source),
            Verdict::Green { source, .. } => Some(*source),
            Verdict::Unknown => None,
        }
    }
}

/// One open auto-filed issue, as the listing reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAuto {
    pub number: u32,
    /// The failing SHA its body names.
    pub sha: Option<String>,
    /// A PR is open against it.
    pub has_open_pr: bool,
}

/// One forge change [`decide`] asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Create {
        title: String,
        body: String,
        labels: Vec<String>,
    },
    /// Keep using `number`; `newer_sha` is set when `main` is now red at a
    /// different commit than the issue names.
    Reuse {
        number: u32,
        newer_sha: Option<String>,
    },
    /// `main` is green: close `number`.
    Close { number: u32, comment: String },
    /// A second auto-filed issue for the same red `main`: close it.
    CloseDuplicate { number: u32, of: u32 },
}

/// Whether `body` carries `marker` at the start of a line (the lane's
/// line-anchored style: prose quoting a marker mid-sentence does not count).
fn has_marker(body: &str, marker: &str) -> bool {
    body.lines().any(|l| l.trim_start().starts_with(marker))
}

/// Whether `body` is an auto-filed fix issue's: both markers.
#[must_use]
pub fn is_auto_body(body: &str) -> bool {
    has_marker(body, MAIN_RED_FIX_MARKER) && has_marker(body, MAIN_RED_AUTO_MARKER)
}

/// The SHA marker for `sha`.
#[must_use]
pub fn sha_marker(sha: &str) -> String {
    format!("{SHA_MARKER_PREFIX}{sha} -->")
}

/// The first SHA marker's value in `body`.
#[must_use]
pub fn body_sha(body: &str) -> Option<String> {
    body.lines().find_map(|l| {
        l.trim_start()
            .strip_prefix(SHA_MARKER_PREFIX)
            .and_then(|r| r.strip_suffix("-->"))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    })
}

fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

/// The created issue's title.
#[must_use]
pub fn title_for(sha: Option<&str>) -> String {
    format!("Fix red main CI ({})", sha.map_or("unknown", short))
}

/// The created issue's body: both markers first (line-anchored), then the
/// evidence.
#[must_use]
pub fn body_for(e: &RedEvidence, branch: &str) -> String {
    let mut body = format!("{MAIN_RED_FIX_MARKER}\n{MAIN_RED_AUTO_MARKER}\n");
    if let Some(sha) = &e.sha {
        body.push_str(&sha_marker(sha));
        body.push('\n');
    }
    let source = match e.source {
        Source::Gate => "the daemon's main-health gate (`buildGate.command`)",
        Source::Ci => "the latest CI runs on it",
    };
    body.push_str(&format!(
        "\n`{branch}` is verified red at {} according to {source}. Every slot spent on \
         other work in this repo is wasted until it is green again.\n",
        e.sha
            .as_deref()
            .map_or("an unknown commit".to_string(), |s| format!("`{s}`")),
    ));
    if !e.failing.is_empty() {
        body.push_str(&format!("\n**Failing:** {}\n", e.failing.join(", ")));
    }
    if let Some(url) = &e.run_url {
        body.push_str(&format!("**Run:** {url}\n"));
    }
    if let Some(tail) = e.gate_tail.as_deref().filter(|t| !t.trim().is_empty()) {
        body.push_str(&format!(
            "\n<details><summary>Gate output (tail)</summary>\n\n```\n{}\n```\n\n</details>\n",
            tail.replace("```", "'''")
        ));
    }
    body.push_str(
        "\nFix `main` in a PR that references this issue. Filed automatically when `main` \
         was verified red (#11766); the daemon closes it when `main` is verified green \
         again, unless a PR is open against it.\n",
    );
    body
}

/// What to do this reconcile. `open` is every open auto-filed issue.
#[must_use]
pub fn decide(verdict: &Verdict, open: &[OpenAuto], branch: &str) -> Vec<Action> {
    let mut open: Vec<&OpenAuto> = open.iter().collect();
    open.sort_by_key(|o| o.number);
    match verdict {
        Verdict::Unknown => Vec::new(),
        Verdict::Green { sha, .. } => open
            .iter()
            .filter(|o| !o.has_open_pr)
            .map(|o| Action::Close {
                number: o.number,
                comment: format!(
                    "`{branch}` is verified green again at {}. Closing this auto-filed \
                     fix issue (#11766).\n\n{}",
                    sha.as_deref()
                        .map_or("its latest commit".to_string(), |s| format!("`{s}`")),
                    sha.as_deref().map(sha_marker).unwrap_or_default(),
                ),
            })
            .collect(),
        Verdict::Red(e) => {
            let Some((keep, rest)) = open.split_first() else {
                return vec![Action::Create {
                    title: title_for(e.sha.as_deref()),
                    body: body_for(e, branch),
                    labels: AUTO_LABELS.iter().map(ToString::to_string).collect(),
                }];
            };
            let newer_sha = e
                .sha
                .clone()
                .filter(|s| keep.sha.as_deref() != Some(s.as_str()));
            let mut actions = vec![Action::Reuse {
                number: keep.number,
                newer_sha,
            }];
            actions.extend(rest.iter().filter(|o| !o.has_open_pr).map(|o| {
                Action::CloseDuplicate {
                    number: o.number,
                    of: keep.number,
                }
            }));
            actions
        }
    }
}

/// An open issue as the forge listing returns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedIssue {
    pub number: u32,
    pub body: String,
}

/// The auto-filed issues in a REST `repos/{o}/{r}/issues` page (PRs and
/// issues without both markers dropped). `None` when the payload is not a
/// JSON array.
#[must_use]
pub fn parse_listing(json: &str) -> Option<Vec<ListedIssue>> {
    let rows: Vec<serde_json::Value> = serde_json::from_str(json).ok()?;
    Some(
        rows.iter()
            .filter(|r| r.get("pull_request").is_none())
            .filter_map(|r| {
                let number = u32::try_from(r.get("number")?.as_u64()?).ok()?;
                let body = r.get("body").and_then(serde_json::Value::as_str)?;
                is_auto_body(body).then(|| ListedIssue {
                    number,
                    body: body.to_string(),
                })
            })
            .collect(),
    )
}

/// The verdict a `gh run list --json headSha,status,conclusion,workflowName,url`
/// payload (newest first) gives for its newest commit.
#[must_use]
pub fn ci_verdict(stdout: &str) -> Verdict {
    let Ok(runs) = serde_json::from_str::<Vec<serde_json::Value>>(stdout) else {
        return Verdict::Unknown;
    };
    let field = |r: &serde_json::Value, k: &str| {
        r.get(k)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    let Some(sha) = runs.first().and_then(|r| field(r, "headSha")) else {
        return Verdict::Unknown;
    };
    let newest: Vec<&serde_json::Value> = runs
        .iter()
        .filter(|r| field(r, "headSha").as_deref() == Some(sha.as_str()))
        .collect();
    let failed: Vec<&&serde_json::Value> = newest
        .iter()
        .filter(|r| {
            matches!(
                field(r, "conclusion").as_deref(),
                Some("failure" | "timed_out" | "startup_failure")
            )
        })
        .collect();
    if !failed.is_empty() {
        let mut seen = HashSet::new();
        let failing: Vec<String> = failed
            .iter()
            .filter_map(|r| field(r, "workflowName"))
            .filter(|w| seen.insert(w.clone()))
            .collect();
        return Verdict::Red(RedEvidence {
            source: Source::Ci,
            sha: Some(sha),
            failing,
            run_url: failed.iter().find_map(|r| field(r, "url")),
            gate_tail: None,
        });
    }
    let all_done = newest
        .iter()
        .all(|r| field(r, "status").as_deref() == Some("completed"));
    let any_success = newest
        .iter()
        .any(|r| field(r, "conclusion").as_deref() == Some("success"));
    if all_done && any_success {
        Verdict::Green {
            source: Source::Ci,
            sha: Some(sha),
        }
    } else {
        Verdict::Unknown
    }
}

/// The last completed gate verdict for one root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateNote {
    Green,
    Red { tail: String },
}

fn gate_notes() -> &'static Mutex<HashMap<PathBuf, GateNote>> {
    static NOTES: OnceLock<Mutex<HashMap<PathBuf, GateNote>>> = OnceLock::new();
    NOTES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record a completed main-health gate verdict for `root` (called by the
/// gate loop on each logged transition): green, or red with the tail of its
/// output. An unevaluated run records nothing.
pub fn note_gate_outcome(root: &Path, outcome: &crate::main_health_gate::GateOutcome) {
    use crate::main_health_gate::GateOutcome;
    let note = match outcome {
        GateOutcome::Green { .. } => GateNote::Green,
        GateOutcome::Red { detail } => GateNote::Red {
            tail: tail_of(detail, MAX_TAIL_CHARS),
        },
        GateOutcome::Unevaluated { .. } => return,
    };
    gate_notes()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(root.to_path_buf(), note);
}

fn gate_note(root: &Path) -> Option<GateNote> {
    gate_notes()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(root)
        .cloned()
}

/// The last `max` characters of `text`, with the home directory written as
/// `~` (the body may land in a public repo).
#[must_use]
pub fn tail_of(text: &str, max: usize) -> String {
    let text = match std::env::var("HOME") {
        Ok(home) if home.len() > 1 => text.replace(&home, "~"),
        _ => text.to_string(),
    };
    let count = text.chars().count();
    if count <= max {
        return text;
    }
    let tail: String = text.chars().skip(count - max).collect();
    format!("…{tail}")
}

/// The verdict for one repo this tick. `ci` is read only when the gate is
/// off; `gate_sha` only when the gate says red.
pub fn verdict_for(
    lane: RedMainLane,
    note: Option<GateNote>,
    ci: impl FnOnce() -> Verdict,
    gate_sha: impl FnOnce() -> Option<String>,
) -> Verdict {
    if lane.gate_disabled {
        return ci();
    }
    if lane.verified_red {
        return Verdict::Red(RedEvidence {
            source: Source::Gate,
            sha: gate_sha(),
            failing: vec!["main-health gate (`buildGate.command`)".to_string()],
            run_url: None,
            gate_tail: match note {
                Some(GateNote::Red { tail }) => Some(tail),
                _ => None,
            },
        });
    }
    match note {
        Some(GateNote::Green) => Verdict::Green {
            source: Source::Gate,
            sha: gate_sha(),
        },
        _ => Verdict::Unknown,
    }
}

/// The forge I/O a reconcile needs, so tests run without a network.
pub trait AutofileForge {
    /// Every open issue labelled `loom:very-important` that carries both
    /// markers.
    fn list_open_auto(&mut self) -> Result<Vec<ListedIssue>, String>;
    /// Which of `issues` have an open PR against them.
    fn open_prs(&mut self, issues: &[u32]) -> HashSet<u32>;
    /// Create an issue; its number.
    fn create(&mut self, title: &str, body: &str, labels: &[String]) -> Result<u32, String>;
    /// The bodies of `issue`'s comments.
    fn comments(&mut self, issue: u32) -> Result<Vec<String>, String>;
    /// Post a comment.
    fn comment(&mut self, issue: u32, body: &str) -> Result<(), String>;
    /// Close an issue (`not_planned` for a duplicate, else `completed`).
    fn close(&mut self, issue: u32, not_planned: bool) -> Result<(), String>;
}

/// What a reconcile did, one entry per issue touched (the telemetry and log
/// unit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    /// `created` / `reused` / `closed` / `closed_duplicate`.
    pub action: &'static str,
    pub issue: u32,
}

/// Per-repo memory across reconciles on this host.
#[derive(Debug, Default)]
pub struct Memory {
    /// `(issue, sha)` pairs already reported or commented: `sha` empty for
    /// "seen this issue".
    noted: HashSet<(u32, String)>,
}

/// List, decide, apply. Returns what changed, or the listing error.
///
/// # Errors
///
/// When the listing fails: nothing is created on a listing the forge did not
/// answer.
pub fn reconcile(
    forge: &mut impl AutofileForge,
    verdict: &Verdict,
    branch: &str,
    memory: &mut Memory,
) -> Result<(Vec<Applied>, usize), String> {
    if matches!(verdict, Verdict::Unknown) {
        return Ok((Vec::new(), 0));
    }
    let listed = forge.list_open_auto()?;
    let numbers: Vec<u32> = listed.iter().map(|i| i.number).collect();
    let with_pr = if numbers.is_empty() || (matches!(verdict, Verdict::Red(_)) && numbers.len() < 2)
    {
        HashSet::new()
    } else {
        forge.open_prs(&numbers)
    };
    let open: Vec<OpenAuto> = listed
        .iter()
        .map(|i| OpenAuto {
            number: i.number,
            sha: body_sha(&i.body),
            has_open_pr: with_pr.contains(&i.number),
        })
        .collect();
    let mut applied = Vec::new();
    for action in decide(verdict, &open, branch) {
        match action {
            Action::Create {
                title,
                body,
                labels,
            } => match forge.create(&title, &body, &labels) {
                Ok(n) => {
                    memory.noted.insert((n, String::new()));
                    applied.push(Applied {
                        action: "created",
                        issue: n,
                    });
                }
                Err(e) => log::warn!("main_red_autofile: creating the fix issue failed: {e}"),
            },
            Action::Reuse { number, newer_sha } => {
                if memory.noted.insert((number, String::new())) {
                    applied.push(Applied {
                        action: "reused",
                        issue: number,
                    });
                }
                if let Some(sha) = newer_sha {
                    note_newer_sha(forge, number, &sha, branch, memory);
                }
            }
            Action::Close { number, comment } => {
                if let Err(e) = forge.comment(number, &comment) {
                    log::warn!("main_red_autofile: comment on #{number} failed: {e}");
                    continue;
                }
                match forge.close(number, false) {
                    Ok(()) => applied.push(Applied {
                        action: "closed",
                        issue: number,
                    }),
                    Err(e) => log::warn!("main_red_autofile: closing #{number} failed: {e}"),
                }
            }
            Action::CloseDuplicate { number, of } => {
                let comment = format!(
                    "Duplicate of #{of}: two hosts filed a fix issue for the same red `main` \
                     at once, and the older one is kept (#11766)."
                );
                if let Err(e) = forge.comment(number, &comment) {
                    log::warn!("main_red_autofile: comment on #{number} failed: {e}");
                    continue;
                }
                match forge.close(number, true) {
                    Ok(()) => applied.push(Applied {
                        action: "closed_duplicate",
                        issue: number,
                    }),
                    Err(e) => log::warn!("main_red_autofile: closing #{number} failed: {e}"),
                }
            }
        }
    }
    Ok((applied, open.len()))
}

/// Comment once (per host, and per forge when another host already did) that
/// `main` is now red at `sha`.
fn note_newer_sha(
    forge: &mut impl AutofileForge,
    number: u32,
    sha: &str,
    branch: &str,
    memory: &mut Memory,
) {
    let key = (number, sha.to_string());
    if memory.noted.contains(&key) {
        return;
    }
    let marker = sha_marker(sha);
    match forge.comments(number) {
        Ok(bodies) if bodies.iter().any(|b| has_marker(b, &marker)) => {
            memory.noted.insert(key);
        }
        Ok(_) => {
            let body = format!("`{branch}` is still verified red, now at `{sha}`.\n\n{marker}");
            match forge.comment(number, &body) {
                Ok(()) => {
                    memory.noted.insert(key);
                }
                Err(e) => log::warn!("main_red_autofile: comment on #{number} failed: {e}"),
            }
        }
        Err(e) => log::debug!("main_red_autofile: reading #{number}'s comments failed: {e}"),
    }
}

/// When one repo was last reconciled, and what it found.
#[derive(Debug, Default)]
struct Schedule {
    last_at: Option<Instant>,
    last_kind: u8,
    /// The last reconcile was green and found no auto-filed issue open.
    green_and_none: bool,
    last_listing: Option<Instant>,
    memory: Memory,
}

/// Whether a repo is due a reconcile. `gate_kind` is the gate verdict's kind
/// when the gate decides (free to read), `None` when CI does (a forge read).
fn due(s: &Schedule, now: Instant, interval: Duration, gate_kind: Option<u8>) -> bool {
    let elapsed = s
        .last_at
        .is_none_or(|at| now.saturating_duration_since(at) >= interval);
    match gate_kind {
        Some(0) => false,
        Some(kind) => kind != s.last_kind || elapsed,
        None => elapsed,
    }
}

/// Whether a green verdict needs a listing at all: not when the last green
/// listing found nothing and [`GREEN_RELIST_EVERY`] has not passed.
fn green_needs_listing(s: &Schedule, now: Instant) -> bool {
    !s.green_and_none
        || s.last_listing
            .is_none_or(|at| now.saturating_duration_since(at) >= GREEN_RELIST_EVERY)
}

fn schedules() -> &'static Mutex<HashMap<PathBuf, Schedule>> {
    static S: OnceLock<Mutex<HashMap<PathBuf, Schedule>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `autonomous.workFinder.redMainAutofile.{enabled,intervalSecs}` for `root`,
/// read on each call so a change applies live.
fn config(root: &Path) -> (bool, Duration) {
    let effective = crate::config_resolver::resolve_effective_config(root);
    let get = |k: &str| {
        crate::config_resolver::get_path(
            &effective,
            &format!("autonomous.workFinder.redMainAutofile.{k}"),
        )
    };
    let enabled = get("enabled")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true);
    let interval = get("intervalSecs")
        .and_then(serde_json::Value::as_u64)
        .filter(|s| *s > 0)
        .map_or(DEFAULT_INTERVAL, Duration::from_secs);
    (enabled, interval)
}

/// The production entry, once per repo per work-finder tick: reconcile
/// `root` when due. `target` is `(gh, owner/repo)`, `open_prs` the open-PR
/// pre-filter for this repo.
pub fn tick(
    root: &Path,
    lane: RedMainLane,
    target: Option<(PathBuf, String)>,
    open_prs: &dyn Fn(&[u32]) -> HashSet<u32>,
) {
    let (enabled, interval) = config(root);
    let Some((gh, nwo)) = target.filter(|_| enabled) else {
        return;
    };
    let note = gate_note(root);
    let now = Instant::now();
    let gate_kind = (!lane.gate_disabled)
        .then(|| verdict_for(lane, note.clone(), || Verdict::Unknown, || None).kind());
    // Taken out of the map for the reconcile, so no lock is held across
    // forge I/O; put back on every path.
    let mut sched = schedules()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(root)
        .unwrap_or_default();
    if due(&sched, now, interval, gate_kind)
        && !crate::rate_limit_breaker::global_skip_pass("main_red_autofile")
    {
        reconcile_due(root, lane, note, (&gh, &nwo), open_prs, &mut sched, now);
    }
    schedules()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(root.to_path_buf(), sched);
}

/// [`tick`]'s reconcile of one due repo.
fn reconcile_due(
    root: &Path,
    lane: RedMainLane,
    note: Option<GateNote>,
    (gh, nwo): (&Path, &str),
    open_prs: &dyn Fn(&[u32]) -> HashSet<u32>,
    sched: &mut Schedule,
    now: Instant,
) {
    let branch = super::main_red_fix::default_branch_for(root);
    let verdict =
        verdict_for(lane, note, || probe_ci(gh, root, &branch), || origin_sha(root, &branch));
    sched.last_at = Some(now);
    sched.last_kind = verdict.kind();
    if matches!(verdict, Verdict::Green { .. }) && !green_needs_listing(sched, now) {
        return;
    }
    // #9548: write only to a repo this installation manages and can write.
    if !crate::write_scope::gate_repo_with(root, nwo, gh, "main-red fix issue (#11766)") {
        return;
    }
    let mut forge = GhForge {
        gh,
        root,
        nwo,
        open_prs,
    };
    match reconcile(&mut forge, &verdict, &branch, &mut sched.memory) {
        Ok((applied, open)) => {
            sched.last_listing = Some(now);
            sched.green_and_none = matches!(verdict, Verdict::Green { .. })
                && applied.len() == open
                && applied.iter().all(|a| a.action == "closed");
            for a in &applied {
                log::warn!(
                    "main_red_autofile: {nwo} main is {}: {} fix issue #{} (#11766)",
                    if matches!(verdict, Verdict::Red(_)) {
                        "red"
                    } else {
                        "green"
                    },
                    a.action,
                    a.issue
                );
                emit(nwo, &verdict, a);
            }
        }
        Err(e) => log::warn!("main_red_autofile: listing fix issues in {nwo} failed: {e}"),
    }
}

/// One `main_red.autofile` span.
fn emit(nwo: &str, verdict: &Verdict, a: &Applied) {
    use crate::telemetry::trace::{
        SpanName, SpanRecord, SpanStatus, TraceAttributes, TraceContext,
    };
    if !crate::observability::ops::spans_exported() {
        return;
    }
    let at = chrono::Utc::now();
    let name = SpanName::MainRedAutofile;
    let issue = a.issue.to_string();
    let sha = verdict.sha().unwrap_or("unknown").to_string();
    let instant = crate::telemetry::trace::instant(at);
    let context = TraceContext::derived(name.as_str(), &[nwo, &issue, a.action, &instant]);
    let mut attributes = TraceAttributes::new();
    attributes.insert("loom.repo".into(), nwo.to_string());
    attributes.insert("loom.issue".into(), issue);
    attributes.insert("loom.main_red.sha".into(), sha);
    attributes.insert("loom.main_red.action".into(), a.action.to_string());
    attributes.insert(
        "loom.main_red.source".into(),
        verdict
            .source()
            .map_or("unknown", Source::label)
            .to_string(),
    );
    crate::telemetry::trace::provenance::stamp(&mut attributes);
    crate::observability::ops::emit_span(SpanRecord {
        context,
        parent_span_id: None,
        name,
        started_at: at,
        ended_at: at,
        status: SpanStatus::Ok,
        attributes,
        events: Vec::new(),
        links: Vec::new(),
    });
}

/// `origin/<branch>`'s SHA in `root`: the commit the gate last synced to.
fn origin_sha(root: &Path, branch: &str) -> Option<String> {
    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(root)
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("refs/remotes/origin/{branch}"));
    match crate::cmd_out::run_command(cmd, Duration::from_secs(10)) {
        CmdOutcome::Ran(out) if out.status.success() => {
            let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
            (!sha.is_empty()).then_some(sha)
        }
        _ => None,
    }
}

fn gh_read(
    gh: &Path,
    root: &Path,
    op: &'static str,
    forge_op: crate::forge_call_stats::ForgeOp,
) -> crate::gh_invocation::GhInvocation {
    crate::gh_invocation::GhInvocation::new(
        crate::gh_invocation::Operation::new(op),
        crate::gh_invocation::AccessIntent::Read,
        crate::gh_invocation::GhTarget::None,
        Duration::from_secs(30),
    )
    .forge_op(forge_op)
    .program(gh)
    .current_dir(root)
}

fn stdout_of(outcome: CmdOutcome) -> Result<String, String> {
    match outcome {
        CmdOutcome::Ran(out) if out.status.success() => {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        }
        CmdOutcome::Ran(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            crate::rate_limit_breaker::global_observe_failure(&stderr, "main_red_autofile");
            Err(stderr)
        }
        CmdOutcome::Unavailable(u) => Err(format!("{u:?}")),
    }
}

fn probe_ci(gh: &Path, root: &Path, branch: &str) -> Verdict {
    let out = gh_read(gh, root, "run.list", crate::forge_call_stats::ops::CI_WORKFLOW_RUNS_FOR_SHA)
        .args(["run", "list", "--branch", branch, "--limit", "30"])
        .args(["--json", "headSha,status,conclusion,workflowName,url"])
        .run();
    match stdout_of(out) {
        Ok(stdout) => ci_verdict(&stdout),
        Err(e) => {
            log::debug!("main_red_autofile: CI read for {} failed ({e})", root.display());
            Verdict::Unknown
        }
    }
}

/// The production forge: `gh` through the counted facade, writes as `--input`
/// JSON ([`crate::forge_comment`]), never a token in argv.
struct GhForge<'a> {
    gh: &'a Path,
    root: &'a Path,
    nwo: &'a str,
    open_prs: &'a dyn Fn(&[u32]) -> HashSet<u32>,
}

impl AutofileForge for GhForge<'_> {
    fn list_open_auto(&mut self) -> Result<Vec<ListedIssue>, String> {
        let path =
            format!("repos/{}/issues?state=open&labels={}&per_page=100", self.nwo, AUTO_LABELS[1]);
        let out =
            gh_read(self.gh, self.root, "issue.list", crate::forge_call_stats::ops::ISSUE_LIST)
                .args(["api", &path])
                .run();
        parse_listing(&stdout_of(out)?).ok_or_else(|| "listing was not a JSON array".to_string())
    }

    fn open_prs(&mut self, issues: &[u32]) -> HashSet<u32> {
        (self.open_prs)(issues)
    }

    fn create(&mut self, title: &str, body: &str, labels: &[String]) -> Result<u32, String> {
        let payload = serde_json::json!({ "title": title, "body": body, "labels": labels });
        let out = crate::forge_comment::gh_api_write_json(
            self.gh.as_os_str(),
            Some(self.root),
            "issue.create",
            crate::forge_call_stats::ops::ISSUE_CREATE,
            &format!("repos/{}/issues", self.nwo),
            None,
            &payload.to_string(),
        )
        .map_err(|e| e.replace("{what}", "issue create"))?;
        serde_json::from_str::<serde_json::Value>(&out)
            .ok()
            .and_then(|v| v.get("number")?.as_u64())
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| "the create response carried no issue number".to_string())
    }

    fn comments(&mut self, issue: u32) -> Result<Vec<String>, String> {
        let path = format!("repos/{}/issues/{issue}/comments?per_page=100", self.nwo);
        let out =
            gh_read(self.gh, self.root, "comment.list", crate::forge_call_stats::ops::COMMENT_LIST)
                .args(["api", &path])
                .run();
        let rows: Vec<serde_json::Value> =
            serde_json::from_str(&stdout_of(out)?).map_err(|e| e.to_string())?;
        Ok(rows
            .iter()
            .filter_map(|r| r.get("body").and_then(serde_json::Value::as_str))
            .map(str::to_string)
            .collect())
    }

    fn comment(&mut self, issue: u32, body: &str) -> Result<(), String> {
        crate::forge_comment::post_comment(self.gh, Some(self.root), self.nwo, issue, false, body)
            .map(|_| ())
    }

    fn close(&mut self, issue: u32, not_planned: bool) -> Result<(), String> {
        let reason = if not_planned {
            "not_planned"
        } else {
            "completed"
        };
        let payload = serde_json::json!({ "state": "closed", "state_reason": reason });
        crate::forge_comment::gh_api_write_json(
            self.gh.as_os_str(),
            Some(self.root),
            "issue.close",
            crate::forge_call_stats::ops::ISSUE_CLOSE_WITH_REASON,
            &format!("repos/{}/issues/{issue}", self.nwo),
            Some("PATCH"),
            &payload.to_string(),
        )
        .map(|_| ())
        .map_err(|e| e.replace("{what}", &format!("close #{issue}")))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
