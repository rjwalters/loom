//! Run invalidation keys on the `forge.event` payload (issue #9201, under the
//! ADR-0021 2026-09-27 amendment).
//!
//! # What this is
//!
//! The amended ADR lets the `forge.event` payload carry **invalidation keys**
//! — `repo`, and optionally a kind and a number — that choose *what to
//! re-query*, and nothing else. This module extracts exactly one kind of key:
//! a finished GitHub Actions run, `(owner/repo, run_id)`, from each
//! `workflow_run` event on a page. The CI telemetry poller's feed consumer
//! (`ci_telemetry::feed`) turns each key into one live `GET` of that run
//! through its own rate-limited client — it never records anything the event
//! says, only what the forge answers.
//!
//! # Why it is still a prompt, not a truth
//!
//! - A key names a run to *fetch*. The record path re-reads the run and its
//!   jobs from GitHub and decides completion, attempt and conclusion from that
//!   answer alone. A key for a run that is not finished, not ours, or does not
//!   exist costs one request and records nothing.
//! - An over-reporting feed costs extra requests, bounded per page by
//!   [`MAX_RUN_KEYS_PER_PAGE`] and by the consumer's own batch cap and the
//!   #4429 rate-limit breaker. An under-reporting feed delays capture by at
//!   most the poller's correction-floor sweep. Neither can steer a decision.
//! - A key is only emitted when both halves are well formed: an `owner/name`
//!   made of GitHub-legal characters (it is interpolated into an API path, so
//!   anything else — `..`, a second `/`, a `?` — is dropped, never encoded),
//!   and a non-zero integer run id.
//!
//! # Worker contract
//!
//! The operator's Worker is outside this repo, so this reads the fields it is
//! most likely to forward and tolerates their absence: the repo from `repo`
//! or `repository.full_name`; the run id from `run_id`, `workflow_run.id`,
//! or the same two under a `payload` object; the action from `action` or
//! `payload.action`. When an action is present it must be `completed`
//! (`requested` / `in_progress` deliveries would only cost a request to learn
//! the run is not finished). A Worker that forwards none of these produces no
//! keys, and the consumer never sees a target — which the poller treats as
//! "the feed has not proven it carries runs", keeping its normal cadence.

/// Hard cap on keys extracted from one page. The default page size is 100,
/// so this only bites on an oversized or hostile page.
pub const MAX_RUN_KEYS_PER_PAGE: usize = 100;

/// The payload field the keys ride in. Absent when a page has none, so every
/// pre-#9201 payload is byte-identical.
pub const RUNS_FIELD: &str = "runs";

/// The event-type name whose events can carry a run key.
pub const WORKFLOW_RUN_TYPE: &str = "workflow_run";

/// One finished-run invalidation key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RunKey {
    /// `owner/name`, as the feed spelled it (matching is case-insensitive
    /// downstream).
    pub repo: String,
    pub run_id: u64,
}

/// `true` when `repo` is exactly `owner/name`, each half non-empty and made of
/// the characters GitHub permits, with no `..` — safe to interpolate into a
/// `repos/{repo}/...` API path without encoding.
#[must_use]
pub fn repo_is_path_safe(repo: &str) -> bool {
    let Some((owner, name)) = repo.split_once('/') else {
        return false;
    };
    let part_ok = |part: &str| {
        !part.is_empty()
            && part.len() <= 100
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    part_ok(owner) && part_ok(name) && !repo.contains("..")
}

fn str_at<'a>(event: &'a serde_json::Value, path: &[&str]) -> Option<&'a str> {
    path.iter()
        .try_fold(event, |value, key| value.get(key))
        .and_then(serde_json::Value::as_str)
}

fn u64_at(event: &serde_json::Value, path: &[&str]) -> Option<u64> {
    path.iter()
        .try_fold(event, |value, key| value.get(key))
        .and_then(serde_json::Value::as_u64)
}

/// The run key one event carries, if any.
#[must_use]
pub fn run_key(event: &serde_json::Value) -> Option<RunKey> {
    if str_at(event, &["type"]) != Some(WORKFLOW_RUN_TYPE) {
        return None;
    }
    let action = str_at(event, &["action"]).or_else(|| str_at(event, &["payload", "action"]));
    if action.is_some_and(|action| action != "completed") {
        return None;
    }
    let repo = str_at(event, &["repo"])
        .or_else(|| str_at(event, &["repository", "full_name"]))
        .or_else(|| str_at(event, &["payload", "repository", "full_name"]))?
        .trim();
    let run_id = u64_at(event, &["run_id"])
        .or_else(|| u64_at(event, &["workflow_run", "id"]))
        .or_else(|| u64_at(event, &["payload", "run_id"]))
        .or_else(|| u64_at(event, &["payload", "workflow_run", "id"]))
        .filter(|id| *id > 0)?;
    repo_is_path_safe(repo).then(|| RunKey {
        repo: repo.to_string(),
        run_id,
    })
}

/// Every distinct run key on a page, sorted, capped at
/// [`MAX_RUN_KEYS_PER_PAGE`].
#[must_use]
pub fn run_keys(events: &[serde_json::Value]) -> Vec<RunKey> {
    let mut keys: Vec<RunKey> = events.iter().filter_map(run_key).collect();
    keys.sort_unstable();
    keys.dedup();
    keys.truncate(MAX_RUN_KEYS_PER_PAGE);
    keys
}

/// Render keys for the payload's [`RUNS_FIELD`].
#[must_use]
pub fn render(keys: &[RunKey]) -> serde_json::Value {
    serde_json::Value::Array(
        keys.iter()
            .map(|key| serde_json::json!({"repo": key.repo, "run_id": key.run_id}))
            .collect(),
    )
}

/// Read the keys back off a `forge.event` payload, re-validating each one — a
/// payload is not trusted merely because this process built it.
#[must_use]
pub fn from_payload(payload: &serde_json::Value) -> Vec<RunKey> {
    let Some(entries) = payload
        .get(RUNS_FIELD)
        .and_then(serde_json::Value::as_array)
    else {
        return Vec::new();
    };
    let mut keys: Vec<RunKey> = entries
        .iter()
        .filter_map(|entry| {
            let repo = entry.get("repo").and_then(serde_json::Value::as_str)?;
            let run_id = entry
                .get("run_id")
                .and_then(serde_json::Value::as_u64)
                .filter(|id| *id > 0)?;
            repo_is_path_safe(repo).then(|| RunKey {
                repo: repo.to_string(),
                run_id,
            })
        })
        .collect();
    keys.sort_unstable();
    keys.dedup();
    keys.truncate(MAX_RUN_KEYS_PER_PAGE);
    keys
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_completed_workflow_run_event_yields_one_key_in_any_supported_shape() {
        for event in [
            json!({"seq": 1, "type": "workflow_run", "repo": "o/r", "run_id": 42}),
            json!({"seq": 1, "type": "workflow_run", "repo": "o/r", "action": "completed",
                   "workflow_run": {"id": 42}}),
            json!({"seq": 1, "type": "workflow_run",
                   "payload": {"action": "completed", "workflow_run": {"id": 42},
                               "repository": {"full_name": "o/r"}}}),
        ] {
            assert_eq!(
                run_key(&event),
                Some(RunKey {
                    repo: "o/r".into(),
                    run_id: 42
                }),
                "{event}"
            );
        }
    }

    #[test]
    fn non_completed_foreign_and_malformed_events_yield_nothing() {
        for event in [
            json!({"type": "workflow_run", "repo": "o/r", "run_id": 42, "action": "requested"}),
            json!({"type": "workflow_job", "repo": "o/r", "run_id": 42}),
            json!({"type": "workflow_run", "repo": "o/r"}),
            json!({"type": "workflow_run", "repo": "o/r", "run_id": 0}),
            json!({"type": "workflow_run", "repo": "o/r/../x", "run_id": 42}),
            json!({"type": "workflow_run", "repo": "../etc", "run_id": 42}),
            json!({"type": "workflow_run", "repo": "o/r?x=1", "run_id": 42}),
            json!({"type": "workflow_run", "repo": "justone", "run_id": 42}),
            json!({"type": "workflow_run", "repo": "o/r", "run_id": "42"}),
        ] {
            assert_eq!(run_key(&event), None, "{event}");
        }
    }

    #[test]
    fn keys_are_deduped_sorted_capped_and_round_trip_through_the_payload() {
        let mut events: Vec<_> = (0..(MAX_RUN_KEYS_PER_PAGE as u64 + 20))
            .map(|n| json!({"type": "workflow_run", "repo": "o/r", "run_id": n + 1}))
            .collect();
        events.push(json!({"type": "workflow_run", "repo": "o/r", "run_id": 1}));
        let keys = run_keys(&events);
        assert_eq!(keys.len(), MAX_RUN_KEYS_PER_PAGE);
        assert_eq!(keys[0].run_id, 1);
        let payload = json!({ RUNS_FIELD: render(&keys) });
        assert_eq!(from_payload(&payload), keys);
    }

    #[test]
    fn from_payload_revalidates_every_entry() {
        let payload = json!({ RUNS_FIELD: [
            {"repo": "o/r", "run_id": 7},
            {"repo": "o/../r", "run_id": 8},
            {"repo": "o/r", "run_id": 0},
            {"repo": 5, "run_id": 9},
        ]});
        assert_eq!(
            from_payload(&payload),
            vec![RunKey {
                repo: "o/r".into(),
                run_id: 7
            }]
        );
        assert!(from_payload(&json!({})).is_empty());
    }
}
