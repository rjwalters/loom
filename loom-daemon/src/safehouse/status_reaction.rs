//! In-place issue status via reactions on the issue's thread root (#13069).
//!
//! Without this, every state change for an issue is a new room message. Here
//! the first `task` message of an issue becomes its **thread root**; the root
//! carries a single status reaction that moves ⏳ → ✅ / ❌ / ⚠️ / 🚧 via
//! safehoused's `react` / `redact` ops while phase lines stay threaded replies.
//!
//! Invariants:
//! - The `completion` envelope is never reacted to, edited or redacted: only
//!   `task` envelopes become roots, and only roots are reacted on (the egress
//!   delay buffer cancels a public publish if the completion is touched).
//! - Mixed fleets: a safehoused without `react` rejects the op; the sink then
//!   stops trying ([`StatusTracker::disable`]) and narration is unchanged.

use std::collections::HashMap;

use super::{qualify_task_id, Envelope, SafehouseClient, SendError};
use crate::types::Event;

pub(super) const PENDING: &str = "⏳";
pub(super) const DONE: &str = "✅";
pub(super) const FAILED: &str = "❌";
pub(super) const NO_PROGRESS: &str = "⚠️";
pub(super) const BLOCKED: &str = "🚧";

/// Bound on remembered roots; the oldest is evicted past this.
const MAX_ROOTS: usize = 512;

/// The terminal status an event puts on its issue's root, as
/// `(task_id, reaction key)`. `None` for events that do not change status.
pub(super) fn status_for_event(event: &Event) -> Option<(String, &'static str)> {
    match event {
        Event::SweepExited {
            issue,
            exit_code,
            no_progress,
            repo,
            ..
        } => {
            let key = match exit_code {
                Some(0) if *no_progress => NO_PROGRESS,
                Some(0) => DONE,
                _ => FAILED,
            };
            Some((qualify_task_id(repo.as_deref(), *issue), key))
        }
        Event::SweepBlocker { issue, repo, .. } | Event::SweepCrashed { issue, repo, .. } => {
            Some((qualify_task_id(repo.as_deref(), *issue), BLOCKED))
        }
        _ => None,
    }
}

#[derive(Debug, Clone)]
struct Root {
    room: Option<String>,
    event_id: String,
    /// Event id of our current status reaction, if one is on the root.
    reaction: Option<String>,
}

/// Remembers each issue's thread root and its current status reaction.
#[derive(Debug, Default)]
pub(super) struct StatusTracker {
    roots: HashMap<String, Root>,
    order: Vec<String>,
    disabled: bool,
}

impl StatusTracker {
    pub(super) fn disabled(&self) -> bool {
        self.disabled
    }

    /// Stop reacting for the rest of the run (peer has no `react`).
    pub(super) fn disable(&mut self) {
        self.disabled = true;
    }

    /// Whether a sent envelope may become a thread root: a `task` with a
    /// `task_id`. Never a `completion`.
    pub(super) fn is_root_candidate(env: &Envelope) -> bool {
        env.kind == "task" && env.task_id.is_some()
    }

    pub(super) fn has_root(&self, task_id: &str) -> bool {
        self.roots.contains_key(task_id)
    }

    pub(super) fn record_root(&mut self, task_id: &str, room: Option<&str>, event_id: String) {
        if self.roots.contains_key(task_id) {
            return;
        }
        if self.order.len() >= MAX_ROOTS {
            let oldest = self.order.remove(0);
            self.roots.remove(&oldest);
        }
        self.order.push(task_id.to_owned());
        self.roots.insert(
            task_id.to_owned(),
            Root {
                room: room.map(ToOwned::to_owned),
                event_id,
                reaction: None,
            },
        );
    }

    /// `(room, root event id, previous reaction id)` for `task_id`.
    pub(super) fn target(&self, task_id: &str) -> Option<(Option<String>, String, Option<String>)> {
        self.roots
            .get(task_id)
            .map(|r| (r.room.clone(), r.event_id.clone(), r.reaction.clone()))
    }

    pub(super) fn set_reaction(&mut self, task_id: &str, reaction: Option<String>) {
        if let Some(root) = self.roots.get_mut(task_id) {
            root.reaction = reaction;
        }
    }
}

/// Interpret a reaction-op result. A protocol rejection means the peer cannot
/// do this (older safehoused without `react`/`redact`): disable for the run,
/// warn once, and report success so narration carries on. Only a transport
/// failure is propagated, so the sink reconnects.
fn tolerate<T>(
    tracker: &mut StatusTracker,
    result: Result<T, SendError>,
) -> Result<Option<T>, SendError> {
    match result {
        Ok(v) => Ok(Some(v)),
        Err(SendError::Rejected { reason }) => {
            log::warn!(
                "safehouse: status reactions rejected ({reason}); falling back to plain \
                 narration for this run, sweep unaffected"
            );
            tracker.disable();
            Ok(None)
        }
        Err(err @ SendError::Transport(_)) => Err(err),
    }
}

/// After `env` was accepted as `event_id`, adopt it as its issue's thread root
/// (if it is the first) and put the ⏳ reaction on it.
pub(super) async fn note_sent(
    client: &mut SafehouseClient,
    tracker: &mut StatusTracker,
    env: &Envelope,
    room: Option<&str>,
    event_id: Option<String>,
) -> Result<(), SendError> {
    if tracker.disabled() || !StatusTracker::is_root_candidate(env) {
        return Ok(());
    }
    let (Some(task_id), Some(event_id)) = (env.task_id.as_deref(), event_id) else {
        return Ok(());
    };
    if tracker.has_root(task_id) {
        return Ok(());
    }
    tracker.record_root(task_id, room, event_id.clone());
    let Some(room) = room else { return Ok(()) };
    let res = client.react(room, &event_id, PENDING).await;
    if let Some(reaction) = tolerate(tracker, res)? {
        tracker.set_reaction(task_id, reaction);
    }
    Ok(())
}

/// Move `task_id`'s root to the status `key`: take back the previous reaction,
/// then add the new one. A no-op when the issue has no known root.
pub(super) async fn apply_status(
    client: &mut SafehouseClient,
    tracker: &mut StatusTracker,
    task_id: &str,
    key: &str,
) -> Result<(), SendError> {
    if tracker.disabled() {
        return Ok(());
    }
    let Some((Some(room), root, previous)) = tracker.target(task_id) else {
        return Ok(());
    };
    if let Some(previous) = previous {
        let res = client.redact(&room, &previous).await;
        if tolerate(tracker, res)?.is_none() {
            return Ok(());
        }
        tracker.set_reaction(task_id, None);
    }
    let res = client.react(&room, &root, key).await;
    if let Some(reaction) = tolerate(tracker, res)? {
        tracker.set_reaction(task_id, reaction);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exited(code: Option<i32>, no_progress: bool) -> Event {
        Event::SweepExited {
            issue: 7,
            exit_code: code,
            duration_sec: 5,
            no_progress,
            death_class: None,
            repo: None,
        }
    }

    #[test]
    fn exit_maps_to_status_key() {
        assert_eq!(status_for_event(&exited(Some(0), false)), Some(("7".into(), DONE)));
        assert_eq!(status_for_event(&exited(Some(1), false)), Some(("7".into(), FAILED)));
        assert_eq!(status_for_event(&exited(None, false)), Some(("7".into(), FAILED)));
        assert_eq!(status_for_event(&exited(Some(0), true)), Some(("7".into(), NO_PROGRESS)));
    }

    #[test]
    fn phase_does_not_change_status() {
        let ev = Event::SweepPhase {
            issue: 7,
            phase: "builder".into(),
            pr_number: None,
            repo: None,
        };
        assert!(status_for_event(&ev).is_none());
    }

    #[test]
    fn only_task_envelopes_are_root_candidates() {
        let mk = |kind: &str| Envelope {
            to: "*".into(),
            kind: kind.into(),
            task_id: Some("7".into()),
            body: String::new(),
            meta: None,
        };
        assert!(StatusTracker::is_root_candidate(&mk("task")));
        assert!(!StatusTracker::is_root_candidate(&mk("completion")));
        assert!(!StatusTracker::is_root_candidate(&mk("ack")));
    }

    #[test]
    fn first_root_wins_and_is_bounded() {
        let mut t = StatusTracker::default();
        t.record_root("a", Some("!r"), "$1".into());
        t.record_root("a", Some("!r"), "$2".into());
        assert_eq!(t.target("a").unwrap().1, "$1");
        for i in 0..(MAX_ROOTS + 5) {
            t.record_root(&format!("k{i}"), None, format!("$e{i}"));
        }
        assert!(!t.has_root("a"));
        assert!(t.has_root(&format!("k{}", MAX_ROOTS + 4)));
    }

    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::{json, Value};
    use serial_test::serial;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    use crate::event_bus::EventBus;
    use crate::safehouse::tests::SafehouseTestPaths;
    use crate::safehouse::{new_shared_state, run_sink, SafehouseConfig};

    /// Stub safehoused: acks `hello`, then answers `n` requests, replying
    /// `ok:false` (unknown op) to `react`/`redact` when `reject_reactions`.
    async fn recording_server(
        listener: UnixListener,
        n: usize,
        reject_reactions: bool,
    ) -> Vec<Value> {
        let (stream, _) = listener.accept().await.unwrap();
        let (rd, mut wr) = stream.into_split();
        let mut lines = BufReader::new(rd).lines();
        let _ = lines.next_line().await.unwrap().unwrap();
        wr.write_all(b"{\"ok\":true,\"id\":0}\n").await.unwrap();
        let mut got = Vec::new();
        for i in 0..n {
            let Some(line) = lines.next_line().await.unwrap() else {
                break;
            };
            let req: Value = serde_json::from_str(&line).unwrap();
            let id = req["id"].clone();
            let reply = if reject_reactions && req["op"] != json!("send") {
                json!({"ok": false, "error": "unknown op", "id": id})
            } else {
                json!({"ok": true, "event_id": format!("$e{i}"), "id": id})
            };
            got.push(req);
            wr.write_all(format!("{reply}\n").as_bytes()).await.unwrap();
        }
        got
    }

    async fn run_lifecycle(n: usize, reject: bool) -> Vec<Value> {
        let dir = tempfile::tempdir().unwrap();
        let _paths = SafehouseTestPaths::set(dir.path());
        let socket = dir.path().join("safehoused.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(recording_server(listener, n, reject));
        let bus = Arc::new(EventBus::new());
        let subscription = bus.subscribe(Vec::<String>::new());
        let sink = tokio::spawn(run_sink(
            SafehouseConfig {
                enabled: true,
                socket: Some(socket.clone()),
                room: Some("!r:x".to_owned()),
                ..SafehouseConfig::default()
            },
            socket,
            subscription,
            Duration::from_millis(20),
            Duration::from_millis(80),
            new_shared_state(),
            None,
            None,
        ));
        let phase = |p: &str| Event::SweepPhase {
            issue: 7,
            phase: p.to_owned(),
            pr_number: None,
            repo: None,
        };
        bus.publish(phase("builder")).unwrap();
        bus.publish(phase("judge")).unwrap();
        bus.publish(exited(Some(0), false)).unwrap();
        let got = tokio::time::timeout(Duration::from_secs(3), server)
            .await
            .expect("stub peer must see the lifecycle")
            .unwrap();
        drop(bus);
        let _ = tokio::time::timeout(Duration::from_secs(2), sink).await;
        got
    }

    #[tokio::test]
    #[serial]
    async fn lifecycle_reacts_in_place_on_the_first_message() {
        // send(builder), react ⏳, send(judge), send(ack), redact ⏳, react ✅
        let got = run_lifecycle(6, false).await;
        let ops: Vec<_> = got.iter().map(|r| r["op"].as_str().unwrap()).collect();
        assert_eq!(ops, ["send", "react", "send", "send", "redact", "react"]);
        assert_eq!(got[1]["event_id"], json!("$e0"), "reacts on the thread root");
        assert_eq!(got[1]["key"], json!(PENDING));
        assert_eq!(got[4]["event_id"], json!("$e1"), "redacts the ⏳ reaction");
        assert_eq!(got[5]["event_id"], json!("$e0"));
        assert_eq!(got[5]["key"], json!(DONE));
        assert!(got.iter().all(|r| r["type"] != json!("completion")));
    }

    #[tokio::test]
    #[serial]
    async fn peer_without_react_degrades_to_plain_narration() {
        // send(builder), react (rejected -> disabled), send(judge), send(ack).
        let got = run_lifecycle(4, true).await;
        let ops: Vec<_> = got.iter().map(|r| r["op"].as_str().unwrap()).collect();
        assert_eq!(ops, ["send", "react", "send", "send"]);
    }
}
