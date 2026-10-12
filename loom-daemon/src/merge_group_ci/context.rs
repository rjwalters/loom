//! The three event contexts the audit evaluates every workflow under.
//!
//! Each context is a synthetic but realistic `github` payload: the values a
//! real run of that event would carry, with placeholder SHAs distinct enough
//! that a concurrency group or checkout ref can be traced back to the field it
//! came from. Anything a static reading cannot know is `Unknown`.

use serde_json::{json, Value as Json};

use super::expr::{Context, Value};

/// The merge-group head commit — the combined tree the queue validates.
pub const MG_HEAD_SHA: &str = "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
/// The base the merge group was built on.
pub const MG_BASE_SHA: &str = "b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2";
/// The temporary branch GitHub creates for a merge group.
pub const MG_HEAD_REF: &str =
    "refs/heads/gh-readonly-queue/main/pr-4242-a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
/// A pull request's head commit (never the combined tree).
pub const PR_HEAD_SHA: &str = "c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3";
const PR_BASE_SHA: &str = "d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4";
/// GitHub's synthetic PR merge commit (`refs/pull/N/merge`).
const PR_MERGE_SHA: &str = "e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5";
const PUSH_SHA: &str = "f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6";
const PUSH_BEFORE: &str = "0707070707070707070707070707070707070707";
/// `github.run_id` — unique per run, so a group keyed on it never collides.
pub const RUN_ID: &str = "990000000001";

/// Which event a run was triggered by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    PullRequest,
    Push,
    MergeGroup,
}

impl Event {
    pub const ALL: [Event; 3] = [Event::PullRequest, Event::Push, Event::MergeGroup];

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Event::PullRequest => "pull_request",
            Event::Push => "push",
            Event::MergeGroup => "merge_group",
        }
    }
}

/// The `github` context for one event.
#[derive(Debug, Clone)]
pub struct EventContext {
    pub event: Event,
    github: Json,
}

impl EventContext {
    #[must_use]
    pub fn new(event: Event, workflow_name: &str) -> Self {
        let github = match event {
            Event::PullRequest => json!({
                "event_name": "pull_request",
                "ref": "refs/pull/4242/merge",
                "ref_name": "4242/merge",
                "sha": PR_MERGE_SHA,
                "head_ref": "feature/issue-4242",
                "base_ref": "main",
                "event": {
                    "action": "synchronize",
                    "number": 4242,
                    "pull_request": {
                        "number": 4242,
                        "head": {"sha": PR_HEAD_SHA, "ref": "feature/issue-4242"},
                        "base": {"sha": PR_BASE_SHA, "ref": "main"}
                    }
                }
            }),
            Event::Push => json!({
                "event_name": "push",
                "ref": "refs/heads/main",
                "ref_name": "main",
                "sha": PUSH_SHA,
                "head_ref": "",
                "base_ref": "",
                "event": {"before": PUSH_BEFORE, "after": PUSH_SHA, "ref": "refs/heads/main"}
            }),
            Event::MergeGroup => json!({
                "event_name": "merge_group",
                "ref": MG_HEAD_REF,
                "ref_name": MG_HEAD_REF.trim_start_matches("refs/heads/"),
                "sha": MG_HEAD_SHA,
                "head_ref": "",
                "base_ref": "",
                "event": {
                    "action": "checks_requested",
                    "merge_group": {
                        "head_sha": MG_HEAD_SHA,
                        "head_ref": MG_HEAD_REF,
                        "base_sha": MG_BASE_SHA,
                        "base_ref": "refs/heads/main"
                    }
                }
            }),
        };
        let mut github = github;
        if let Some(obj) = github.as_object_mut() {
            obj.insert("workflow".into(), Json::String(workflow_name.to_string()));
            obj.insert("run_id".into(), Json::String(RUN_ID.to_string()));
            obj.insert("repository".into(), Json::String("owner/repo".into()));
        }
        Self { event, github }
    }

    /// Resolve a `github.*` path. Keys that exist resolve to their value;
    /// missing keys under `github.event` resolve to `null` (that is what the
    /// payload of this event really lacks); any other missing key is unknown.
    #[must_use]
    pub fn github(&self, path: &[String]) -> Value {
        let mut cur = &self.github;
        for (i, seg) in path.iter().enumerate() {
            match cur.get(seg) {
                Some(next) => cur = next,
                None => {
                    let in_event = path.first().is_some_and(|s| s == "event") && i > 0;
                    return if in_event || cur.is_null() {
                        Value::Null
                    } else {
                        Value::Unknown
                    };
                }
            }
        }
        json_value(cur)
    }
}

fn json_value(j: &Json) -> Value {
    match j {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) => n.as_f64().map_or(Value::Unknown, Value::Num),
        Json::String(s) => Value::Str(s.clone()),
        Json::Array(_) | Json::Object(_) => Value::Opaque(true),
    }
}

/// Context for workflow-level expressions (`concurrency:` at the top), which
/// may only read `github`, `inputs` and `vars`.
pub struct WorkflowScope<'a>(pub &'a EventContext);

impl Context for WorkflowScope<'_> {
    fn lookup(&self, path: &[String]) -> Value {
        match path.split_first() {
            Some((head, rest)) if head == "github" => self.0.github(rest),
            _ => Value::Unknown,
        }
    }
}

/// How a job resolved in one context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Runs,
    Skipped,
    Unknown,
}

/// Context for job- and step-level expressions.
pub struct JobScope<'a> {
    pub event: &'a EventContext,
    /// `(job id, state)` for every job in `needs:`.
    pub needs: Vec<(String, RunState)>,
    /// Whether `success()` is evaluated for the job itself (true) or a step.
    pub job_level: bool,
}

impl Context for JobScope<'_> {
    fn lookup(&self, path: &[String]) -> Value {
        match path.split_first() {
            Some((head, rest)) if head == "github" => self.event.github(rest),
            Some((head, rest)) if head == "needs" => {
                let Some((job, field)) = rest.split_first() else {
                    return Value::Unknown;
                };
                let Some(state) = self.needs.iter().find(|(id, _)| id == job).map(|(_, s)| *s)
                else {
                    // A `needs.X` for a job not in `needs:` is null at runtime.
                    return Value::Null;
                };
                match (field.first().map(String::as_str), state) {
                    (Some("outputs"), RunState::Skipped) => Value::Str(String::new()),
                    (Some("result"), RunState::Skipped) => Value::Str("skipped".into()),
                    (Some("result"), RunState::Runs) => Value::Str("success".into()),
                    _ => Value::Unknown,
                }
            }
            _ => Value::Unknown,
        }
    }

    fn success(&self) -> Value {
        if !self.job_level {
            return Value::Bool(true);
        }
        if self.needs.iter().any(|(_, s)| *s == RunState::Skipped) {
            Value::Bool(false)
        } else if self.needs.iter().all(|(_, s)| *s == RunState::Runs) {
            Value::Bool(true)
        } else {
            Value::Unknown
        }
    }
}
