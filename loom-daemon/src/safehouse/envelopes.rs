//! Event → envelope-v1 rendering for the Safehouse narration sink.
//!
//! Two pure renderers, extracted from `safehouse.rs` when the second one
//! arrived (#9321):
//!
//! - [`event_to_envelope`] — the frozen bus taxonomy's 1:1 mapping.
//! - [`operator_priority_envelope`] — the operator-priority escalation line.
//!
//! Neither touches the socket, the config or the clock: [`super::run_sink`]
//! owns routing, dedupe and delivery. Keeping them here means an agent editing
//! a narration body reads a few hundred lines instead of the whole sink.

use super::{
    decode_exit_code_annotation, dispatch_envelope, format_narrated_duration, qualify_task_id,
    repo_issue_prefix, sanitize_task_id_segment, Envelope,
};
use crate::types::{Event, SweepKind};

/// The env override for `safehouse.operatorMention` (#9321). A Matrix display
/// name or user id (`@operator:example.org`) — a handle, never a secret.
pub const OPERATOR_MENTION_ENV: &str = "LOOM_SAFEHOUSE_OPERATOR_MENTION";

/// Map an existing bus [`Event`] to a narration [`Envelope`], or `None` for
/// events phase 1 does not narrate.
///
/// Every narrated body starts with the repo-qualified `<repo>#<issue>` prefix
/// ([`super::repo_issue_prefix`]) and every narrated `task_id` is likewise
/// repo-qualified ([`super::qualify_task_id`]) — issue #4201, problem 1 — so the same
/// issue number in two managed repos threads into distinct Matrix threads
/// instead of colliding:
///
/// | Event | type | body |
/// |---|---|---|
/// | `SweepGlobalDispatch(Issue n)` | `task` | `<repo>#n · dispatch` (the sink, [`super::run_sink`], best-effort appends ` — "<issue title>"`) |
/// | `SweepPhase` | `task` | `<repo>#n · <phase>` (+ ` · PR #m open` when present) |
/// | `SweepBlocker` | `handoff` | `<repo>#n · BLOCKED — <reason>` |
/// | `SweepExited` | `ack` | `<repo>#n · done ✓ · <dur>` or `<repo>#n · failed ✗ · exit <code>[ (decoded)] · <dur>` |
/// | `SweepCrashed` | `handoff` | `<repo>#n · crashed ✗ at <checkpoint_phase> — resumable (checkpoint kept)` |
/// | `SweepResumeDispatched` (#4256) | `handoff` | `<repo>#n · reaper resumed crashed sweep at <phase> (open PR #m) — resuming without operator intervention` (or a "still stranded" variant when the resume dispatch itself failed) |
///
/// `SweepGlobalCompleted` is intentionally **not** narrated: it carries only a
/// `sweep_id` (no issue number), and `SweepExited` already emits the completion
/// `ack` with richer data — narrating both would double-post per completion.
///
/// This mapping is 1:1 and pure. The **second** envelope a `SweepExited` can
/// produce — the public-feed `completion` (#4426) — is built by
/// [`super::completion_for_exit`] instead, since it needs an async forge lookup to
/// confirm the merge; [`super::run_sink`] emits it after this one.
#[must_use]
pub fn event_to_envelope(event: &Event) -> Option<Envelope> {
    match event {
        Event::SweepGlobalDispatch {
            kind: SweepKind::Issue(issue),
            repo,
            ..
        } => Some(dispatch_envelope(repo.as_deref(), *issue)),
        Event::SweepPhase {
            issue,
            phase,
            pr_number,
            repo,
        } => {
            let mut body = format!("{} · {phase}", repo_issue_prefix(repo.as_deref(), *issue));
            if let Some(pr) = pr_number {
                body.push_str(&format!(" · PR #{pr} open"));
            }
            Some(Envelope {
                to: "*".to_owned(),
                kind: "task".to_owned(),
                task_id: Some(qualify_task_id(repo.as_deref(), *issue)),
                body,
                meta: None,
            })
        }
        Event::SweepBlocker {
            issue,
            reason,
            repo,
            ..
        } => Some(Envelope {
            to: "*".to_owned(),
            kind: "handoff".to_owned(),
            task_id: Some(qualify_task_id(repo.as_deref(), *issue)),
            body: format!("{} · BLOCKED — {reason}", repo_issue_prefix(repo.as_deref(), *issue)),
            meta: None,
        }),
        Event::SweepExited {
            issue,
            exit_code,
            duration_sec,
            no_progress,
            death_class: _,
            repo,
        } => {
            let prefix = repo_issue_prefix(repo.as_deref(), *issue);
            let dur = format_narrated_duration(*duration_sec);
            let body = match exit_code {
                // #4366: a clean exit with zero lifecycle progress (parked on
                // a monitored background task) narrates distinctly from an
                // ordinary benign self-skip so operators can see the failure
                // class at a glance.
                Some(0) if *no_progress => {
                    format!("{prefix} · no progress ⚠ · exit 0, no checkpoint/PR · {dur}")
                }
                Some(0) => format!("{prefix} · done ✓ · {dur}"),
                Some(code) => format!(
                    "{prefix} · failed ✗ · exit {code}{} · {dur}",
                    decode_exit_code_annotation(*code)
                ),
                None => format!("{prefix} · failed ✗ · exit ? · {dur}"),
            };
            Some(Envelope {
                to: "*".to_owned(),
                kind: "ack".to_owned(),
                task_id: Some(qualify_task_id(repo.as_deref(), *issue)),
                body,
                meta: None,
            })
        }
        Event::SweepCrashed {
            issue,
            checkpoint_phase,
            classification: _,
            death_class: _,
            repo,
        } => {
            let phase = checkpoint_phase.as_deref().unwrap_or("unknown");
            Some(Envelope {
                to: "*".to_owned(),
                kind: "handoff".to_owned(),
                task_id: Some(qualify_task_id(repo.as_deref(), *issue)),
                body: format!(
                    "{} · crashed ✗ at {phase} — resumable (checkpoint kept)",
                    repo_issue_prefix(repo.as_deref(), *issue)
                ),
                meta: None,
            })
        }
        Event::SweepResumeDispatched {
            issue,
            pr,
            checkpoint_phase,
            dispatched,
            repo,
        } => {
            let phase = checkpoint_phase.as_deref().unwrap_or("unknown");
            let prefix = repo_issue_prefix(repo.as_deref(), *issue);
            let body = if *dispatched {
                format!(
                    "{prefix} · reaper resumed crashed sweep at {phase} (open PR #{pr}) — \
                     resuming without operator intervention"
                )
            } else {
                format!(
                    "{prefix} · reaper attempted resume at {phase} (open PR #{pr}) but the \
                     dispatch itself failed — still stranded, needs a look"
                )
            };
            Some(Envelope {
                to: "*".to_owned(),
                kind: "handoff".to_owned(),
                task_id: Some(qualify_task_id(repo.as_deref(), *issue)),
                body,
                meta: None,
            })
        }
        Event::DaemonIdleExit {
            trigger,
            idle_minutes,
            in_flight_sweeps,
            active_role_runs,
            healthy_tokens,
            total_tokens,
            message,
        } => Some(Envelope {
            to: "*".to_owned(),
            kind: "handoff".to_owned(),
            task_id: Some("daemon-idle-exit".to_owned()),
            body: message.clone(),
            meta: Some(serde_json::json!({
                "trigger": trigger,
                "idle_minutes": idle_minutes,
                "in_flight_sweeps": in_flight_sweeps,
                "active_role_runs": active_role_runs,
                "healthy_tokens": healthy_tokens,
                "total_tokens": total_tokens,
            })),
        }),
        // SweepGlobalCompleted (no issue number — SweepExited covers it),
        // SweepGlobalDispatch(PrSet), EpicAction, CapacityAdvisory, TopicLag,
        // Generic: not narrated in phase 1.
        //
        // OperatorPriorityEscalation (#9321) is narrated, but by
        // [`operator_priority_envelope`] rather than here: its body needs
        // `safehouse.operatorMention` from the config, which this pure
        // per-event mapping deliberately does not take. `run_sink` routes the
        // variant there before reaching this function.
        _ => None,
    }
}

/// Render an `operator_priority.escalation` event as a room message (#9321):
/// the Matrix half of a starred issue's operator ask, whose forge-comment half
/// [`crate::star_liveness::escalate`] already posted.
///
/// `operator_mention` is `safehouse.operatorMention` — prepended to the
/// escalation line so the room notifies a human. It is deliberately **not**
/// prepended to the `resolved` line: the ping's job is to summon the operator
/// to an open ask, and re-pinging them to say the ask went away is noise.
/// `None` posts the line without a mention rather than dropping it.
///
/// The envelope kind is `handoff`, i.e. [`super::AttentionClass::Signal`] — the
/// notifications-on cross-repo room (the team General room the operator
/// actually watches), not a per-repo firehose. That choice is the whole point
/// of the issue: a `task`-kind line would land in a muted room and repeat the
/// #9268 failure mode in a different medium.
///
/// Returns `None` for every other event variant, so the sink's dispatch chain
/// can call it unconditionally.
///
/// **No `meta`, by protocol.** Envelope-v1 permits `meta` only on a
/// `completion` ([`super::build_send_request`] *refuses* it on any other type, which
/// would drop the ask at the last hop and reproduce the very silence this issue
/// fixes — it did, in development). So everything the operator needs is in the
/// body, and the structured facts stay on the bus event for any other consumer.
///
/// **Dedupe is upstream, not here.** This function is a pure relay: fed the
/// same event twice it renders two envelopes. The once-per-cause property comes
/// from the publisher, which only emits on the pass that actually posts the
/// forge comment carrying that key's marker — see
/// [`crate::star_liveness::escalate`]'s module docs.
#[must_use]
pub fn operator_priority_envelope(
    event: &Event,
    operator_mention: Option<&str>,
) -> Option<Envelope> {
    let Event::OperatorPriorityEscalation {
        slug,
        issue,
        kind,
        stage,
        text,
        url,
        host,
        inherited_from,
        resolved,
        // The dedupe key is the *publisher's* business (and is already in the
        // forge comment's marker); the room line says `kind at stage` instead,
        // which is the same fact in words a human reads.
        key: _,
    } = event
    else {
        return None;
    };
    // `<repo>#<issue>` and `<repo>_<issue>` from the forge slug's repo
    // segment, so an escalation threads with that issue's other narration
    // (whose repo name comes from the workspace-root basename — the same name
    // for every normally-cloned checkout).
    let name = slug.rsplit('/').next().unwrap_or(slug.as_str());
    // `issue == 0` is a fleet-wide alert (#10164): no `#N`, and no link when
    // the publisher has no URL to offer.
    let at = if *issue == 0 {
        name.to_owned()
    } else {
        format!("{name}#{issue}")
    };
    let task_id = Some(format!("{}_{issue}", sanitize_task_id_segment(name)));
    let link = if url.is_empty() {
        String::new()
    } else {
        format!("{url} · ")
    };
    let body = if *resolved {
        let tail = if url.is_empty() {
            String::new()
        } else {
            format!(" · {url}")
        };
        format!("{at} · operator ask resolved ✓ · {kind}{tail}")
    } else {
        let mention = operator_mention
            .map(|m| format!("{m}: "))
            .unwrap_or_default();
        let inherited = inherited_from
            .map(|n| format!(" (blocks starred #{n})"))
            .unwrap_or_default();
        format!(
            "{mention}{at} · OPERATOR NEEDED · {kind} at {stage}{inherited}\n\
             {text}\n{link}observed by {host}"
        )
    };
    Some(Envelope {
        to: "*".to_owned(),
        kind: "handoff".to_owned(),
        task_id,
        body,
        // Protocol-mandated `None` — see this function's docs.
        meta: None,
    })
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::json;
    use serial_test::serial;
    use tokio::net::UnixListener;

    use super::*;
    use crate::event_bus::EventBus;
    use crate::safehouse::tests::{stub_server, SafehouseTestPaths};
    use crate::safehouse::{
        build_send_request, new_shared_state, run_sink, AttentionClass, EnvelopeKind,
        SafehouseConfig,
    };

    /// One escalation event, as `star_liveness` publishes it.
    fn escalation_event(resolved: bool) -> Event {
        Event::OperatorPriorityEscalation {
            slug: "rjwalters/loom".to_owned(),
            issue: 9268,
            key: "merge-refused:405".to_owned(),
            kind: "merge-refused".to_owned(),
            stage: "needs-operator".to_owned(),
            text: if resolved {
                String::new()
            } else {
                "Allow merge commits on the repository, or re-run the merge with squash.".to_owned()
            },
            url: "https://github.com/rjwalters/loom/issues/9268".to_owned(),
            host: "host-a".to_owned(),
            inherited_from: None,
            resolved,
        }
    }

    #[test]
    fn an_escalation_pings_the_operator_and_carries_the_ask_and_a_link() {
        let env =
            operator_priority_envelope(&escalation_event(false), Some("@operator:example.org"))
                .expect("the escalation variant must render");
        assert_eq!(env.kind, "handoff", "handoff ⇒ the Signal (notifications-on) room");
        assert_eq!(
            EnvelopeKind::parse(&env.kind).unwrap().attention_class(),
            AttentionClass::Signal,
            "an operator ask must not land in a muted per-repo firehose"
        );
        assert_eq!(env.task_id.as_deref(), Some("loom_9268"), "threads with the issue");
        assert!(env.body.starts_with("@operator:example.org: "), "got {:?}", env.body);
        assert!(env
            .body
            .contains("loom#9268 · OPERATOR NEEDED · merge-refused at needs-operator"));
        assert!(env.body.contains("Allow merge commits"), "the concrete ask");
        assert!(
            env.body
                .contains("https://github.com/rjwalters/loom/issues/9268"),
            "a link"
        );
        assert!(env.body.contains("observed by host-a"), "and which host saw it");
    }

    /// The last-hop regression guard. Envelope-v1 permits `meta` only on a
    /// `completion`, so a `handoff` carrying one is **refused by
    /// [`build_send_request`] before it is written** — the ask would vanish at the
    /// final hop and reproduce exactly the silence this issue fixes. That happened
    /// in development, which is why this asserts sendability rather than shape.
    #[test]
    fn an_escalation_envelope_is_actually_sendable_over_envelope_v1() {
        for resolved in [false, true] {
            let env = operator_priority_envelope(
                &escalation_event(resolved),
                Some("@operator:example.org"),
            )
            .unwrap();
            assert!(env.meta.is_none(), "envelope-v1: `meta` is completion-only");
            let req = build_send_request(&env, 7, None)
                .expect("an escalation must survive send validation");
            assert_eq!(req["type"], json!("handoff"));
            assert_eq!(req["task_id"], json!("loom_9268"));
            assert!(req.get("meta").is_none());
        }
    }

    #[test]
    fn a_recovery_says_resolved_without_re_pinging_the_operator() {
        let env =
            operator_priority_envelope(&escalation_event(true), Some("@operator:example.org"))
                .expect("the resolved variant must render");
        assert_eq!(env.kind, "handoff");
        assert_eq!(env.task_id.as_deref(), Some("loom_9268"), "same thread as the ask");
        assert!(
            !env.body.contains("@operator:example.org"),
            "a recovery must not re-ping: {:?}",
            env.body
        );
        assert!(env
            .body
            .contains("loom#9268 · operator ask resolved ✓ · merge-refused"));
        assert!(env
            .body
            .contains("https://github.com/rjwalters/loom/issues/9268"));
    }

    #[test]
    fn an_unconfigured_mention_still_posts_the_ask() {
        let env = operator_priority_envelope(&escalation_event(false), None).unwrap();
        assert!(
            env.body.starts_with("loom#9268 · OPERATOR NEEDED"),
            "no mention configured ⇒ no prefix, never a dropped ask: {:?}",
            env.body
        );
    }

    #[test]
    fn an_inheriting_blocker_says_which_star_it_blocks() {
        let mut event = escalation_event(false);
        if let Event::OperatorPriorityEscalation { inherited_from, .. } = &mut event {
            *inherited_from = Some(9244);
        }
        let env = operator_priority_envelope(&event, None).unwrap();
        assert!(env.body.contains("(blocks starred #9244)"), "got {:?}", env.body);
    }

    #[test]
    fn a_fleet_wide_alert_has_no_issue_number_or_empty_link() {
        let mut event = escalation_event(false);
        if let Event::OperatorPriorityEscalation {
            issue, url, slug, ..
        } = &mut event
        {
            *issue = 0;
            url.clear();
            *slug = "fleet/box".to_owned();
        }
        let env = operator_priority_envelope(&event, None).unwrap();
        assert!(env.body.starts_with("box · OPERATOR NEEDED"), "got {:?}", env.body);
        assert!(!env.body.contains('#') && !env.body.contains(" · observed"));
    }

    #[test]
    fn every_other_event_renders_no_escalation_envelope() {
        assert!(operator_priority_envelope(&Event::TopicLag { skipped: 3 }, None).is_none());
        assert!(operator_priority_envelope(
            &Event::SweepPhase {
                issue: 1,
                phase: "builder".to_owned(),
                pr_number: None,
                repo: None,
            },
            None
        )
        .is_none());
    }

    /// End-to-end over the fake `safehoused` peer (#9321): the **real**
    /// star-liveness pass publishes, the **real** sink relays, and the stub
    /// server counts the Matrix sends.
    ///
    /// This is the composition the acceptance criteria are about — one post per
    /// `<kind>:<specifics>` dedupe key fleet-wide, none for an unstarred issue, no
    /// repeat across ticks/hosts/restarts, and one recovery post when the ask
    /// clears. The dedupe itself is asserted on the publisher side
    /// (`star_liveness::tests::notice_tests`); what this proves is that the two
    /// halves actually compose over the wire, with nothing in the sink
    /// duplicating or swallowing an ask.
    #[tokio::test]
    #[serial]
    async fn run_sink_relays_one_escalation_and_one_recovery_for_a_starred_issue() {
        use crate::star_liveness::tests::fake::{issue, pr, repo_input, t, Host, World, STAR};

        let dir = tempfile::tempdir().unwrap();
        let _safehouse_test_paths = SafehouseTestPaths::set(dir.path());
        let socket = dir.path().join("safehoused.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        // Exactly two sends are expected for the whole scenario below.
        let server = tokio::spawn(stub_server(listener, false, 2));

        let bus = Arc::new(EventBus::new());
        let subscription = bus.subscribe(Vec::<String>::new());
        let sink = tokio::spawn(run_sink(
            SafehouseConfig {
                enabled: true,
                socket: Some(socket.clone()),
                operator_mention: Some("@operator:example.org".to_owned()),
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

        // A starred issue whose approved PR is held by Champion's merge-risk
        // hold, plus an unstarred issue in exactly the same state.
        let world = World::default();
        world.add("o/r", issue(1, &[STAR, "loom:building"]));
        world.add("o/r", pr(2, 1, &["loom:pr", "loom:operator"]));
        world.add("o/r", issue(3, &["loom:building"]));
        world.add("o/r", pr(4, 3, &["loom:pr", "loom:operator"]));
        let repos = vec![repo_input("o/r")];

        let mut host = Host::new("host-a");
        // Three ticks with the hold in place.
        for minute in [0, 2, 4] {
            host.pass(&world, &repos, Vec::new(), t(10, minute));
        }
        // A peer host, and a restarted daemon (a fresh ledger against the same
        // forge): both find the marker the first pass posted.
        let mut peer = Host::new("host-b");
        peer.pass(&world, &repos, Vec::new(), t(10, 5));
        let mut restarted = Host::new("host-a");
        restarted.pass(&world, &repos, Vec::new(), t(10, 6));
        // Champion's hold comes off: the instance that announced the ask — the
        // only one that knows it announced — narrates the recovery.
        world.repo("o/r").items.get_mut(&2).unwrap().labels = vec!["loom:pr".into()];
        host.pass(&world, &repos, Vec::new(), t(10, 8));

        assert!(peer.notices.is_empty(), "no repeat on a second host");
        assert!(restarted.notices.is_empty(), "no repeat across a restart");
        assert_eq!(
            host.notices.len(),
            2,
            "one escalation + one recovery for the starred issue, nothing for #3: {:?}",
            host.notices
        );
        for event in host
            .events()
            .into_iter()
            .chain(peer.events())
            .chain(restarted.events())
        {
            bus.publish(event).unwrap();
        }

        let received = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("the stub safehoused peer must receive both sends")
            .unwrap();
        drop(bus);
        let _ = tokio::time::timeout(Duration::from_secs(2), sink).await;

        assert_eq!(received.len(), 2);
        let ask = received[0]["body"].as_str().unwrap();
        assert!(ask.starts_with("@operator:example.org: "), "got {ask:?}");
        assert!(ask.contains("r#1 · OPERATOR NEEDED · merge-risk-hold"), "got {ask:?}");
        assert!(ask.contains("https://github.com/o/r/issues/1"), "got {ask:?}");
        assert!(!ask.contains("#3"), "the unstarred issue is never narrated: {ask:?}");
        assert_eq!(received[0]["type"], json!("handoff"));

        let done = received[1]["body"].as_str().unwrap();
        assert!(done.contains("r#1 · operator ask resolved ✓"), "got {done:?}");
        assert!(!done.contains("@operator:example.org"), "a recovery does not re-ping: {done:?}");
    }
}
