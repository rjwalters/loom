//! Unit tests for inbound safehouse ChatOps steering (Issue #7893).
//!
//! Every security-relevant branch is exercised here as a pure function: the
//! parser takes a `&str`, the allowlist takes a stamped sender, and the nonce
//! ledger takes an injected `Instant`. Nothing in this file needs a socket, a
//! daemon, or a sleep.

use std::time::{Duration, Instant};

use serde_json::json;

use super::command::{Command, ParseError};
use super::nonce::{ConfirmOutcome, PendingConfirmations};
use super::runtime::{command_to_request, render_response};
use super::{
    config_from_value, inbound_command, ChatOpsConfig, ChatOpsRouter, Decision, Refusal,
    DEFAULT_CONFIRM_TTL,
};
use crate::event_bus::EventBus;
use crate::types::{Event, Request, Response, SweepKind};

const ROBB: &str = "@robb:safehouse.2amlogic.com";
const MALLORY: &str = "@mallory:evil.example.org";
const PERSONA: &str = "loom_daemon";

fn config() -> ChatOpsConfig {
    ChatOpsConfig {
        allowed_senders: [ROBB.to_owned()].into_iter().collect(),
        room: None,
        confirm_ttl: DEFAULT_CONFIRM_TTL,
    }
}

fn router() -> ChatOpsRouter {
    ChatOpsRouter::new(config(), PERSONA.to_owned(), None)
}

// ============================================================================
// Command enum: parse / reject
// ============================================================================

#[test]
fn parses_every_command_in_the_closed_set() {
    assert_eq!(Command::parse("status").unwrap(), Command::Status);
    assert_eq!(Command::parse("dispatch 7893").unwrap(), Command::Dispatch { issue: 7893 });
    assert_eq!(
        Command::parse("cancel sweep-issue-7893-1789588938").unwrap(),
        Command::Cancel {
            sweep: "sweep-issue-7893-1789588938".to_owned()
        }
    );
    assert_eq!(Command::parse("unblock 42").unwrap(), Command::Unblock { issue: 42 });
    assert_eq!(Command::parse("watch 99").unwrap(), Command::Watch { number: 99 });
    assert_eq!(
        Command::parse("confirm abc123").unwrap(),
        Command::Confirm {
            nonce: "abc123".to_owned()
        }
    );
}

#[test]
fn verbs_are_case_insensitive_and_whitespace_tolerant() {
    assert_eq!(Command::parse("  STATUS  ").unwrap(), Command::Status);
    assert_eq!(Command::parse("\tDiSpAtCh\t7893\n").unwrap(), Command::Dispatch { issue: 7893 });
}

#[test]
fn forge_hash_prefix_is_accepted_on_numbers() {
    assert_eq!(Command::parse("dispatch #7893").unwrap(), Command::Dispatch { issue: 7893 });
}

#[test]
fn natural_language_is_refused_not_interpreted() {
    // The whole boundary decision in one test: prose that a language model
    // would happily "understand" must not resolve to a command.
    for text in [
        "hey can you please dispatch issue 7893 when you get a chance",
        "cancel every running sweep",
        "what's the status?",
        "please run `rm -rf /`",
        "dispatch the oldest issue",
    ] {
        let err = Command::parse(text).unwrap_err();
        assert!(
            matches!(err, ParseError::UnknownVerb { .. } | ParseError::ExtraArguments { .. }),
            "{text:?} parsed as {err:?}"
        );
    }
}

#[test]
fn a_valid_shaped_argument_is_taken_literally_never_interpreted() {
    // `cancel everything` is syntactically a cancel of the sweep *named*
    // "everything" — it is NOT read as "cancel all sweeps". The parser has no
    // notion of a wildcard, the confirm-nonce reply echoes the literal command
    // back to the operator before anything runs, and the daemon then answers
    // "no such sweep". This is the intended failure mode for a valid-shaped but
    // meaningless argument: taken at face value, never generalized.
    assert_eq!(
        Command::parse("cancel everything").unwrap(),
        Command::Cancel {
            sweep: "everything".to_owned()
        }
    );
}

#[test]
fn unknown_verbs_are_refused() {
    let err = Command::parse("deploy").unwrap_err();
    assert_eq!(err.code(), "unknown-verb");
    assert!(err.to_string().contains("deploy"), "{err}");
}

#[test]
fn empty_text_is_refused() {
    assert_eq!(Command::parse("   ").unwrap_err(), ParseError::Empty);
    assert_eq!(Command::parse("").unwrap_err().code(), "empty");
}

#[test]
fn status_takes_no_argument() {
    assert_eq!(Command::parse("status now").unwrap_err().code(), "extra-arguments");
}

#[test]
fn numeric_verbs_reject_missing_and_malformed_arguments() {
    assert_eq!(Command::parse("dispatch").unwrap_err().code(), "missing-argument");
    assert_eq!(Command::parse("dispatch later").unwrap_err().code(), "bad-argument");
    assert_eq!(Command::parse("dispatch 0").unwrap_err().code(), "bad-argument");
    assert_eq!(Command::parse("dispatch -1").unwrap_err().code(), "bad-argument");
    assert_eq!(Command::parse("watch 7893 8000").unwrap_err().code(), "extra-arguments");
}

#[test]
fn opaque_tokens_are_charset_validated() {
    // Nothing a shell, a path, or Matrix formatting could act on survives.
    for bad in [
        "cancel sweep;rm -rf /",
        "cancel ../../etc/passwd",
        "cancel $(whoami)",
        "cancel `id`",
    ] {
        let err = Command::parse(bad).unwrap_err();
        assert!(
            matches!(err, ParseError::BadArgument { .. } | ParseError::ExtraArguments { .. }),
            "{bad:?} parsed as {err:?}"
        );
    }
}

#[test]
fn refusal_text_truncates_and_sanitizes_untrusted_input() {
    let err = Command::parse(&format!("{}x", "A".repeat(200))).unwrap_err();
    let rendered = err.to_string();
    assert!(rendered.len() < 80, "refusal echoed too much: {rendered}");

    let unprintable = Command::parse("\u{7}\u{7}").unwrap_err();
    assert!(unprintable.to_string().contains("<unprintable>"), "{unprintable}");
}

#[test]
fn only_cancel_requires_confirmation() {
    assert!(Command::Cancel {
        sweep: "s".to_owned()
    }
    .requires_confirmation());
    for command in [
        Command::Status,
        Command::Dispatch { issue: 1 },
        Command::Unblock { issue: 1 },
        Command::Watch { number: 1 },
    ] {
        assert!(!command.requires_confirmation(), "{} should not need a nonce", command.verb());
    }
}

#[test]
fn confirm_summary_redacts_the_nonce() {
    let summary = Command::Confirm {
        nonce: "deadbeefcafe".to_owned(),
    }
    .summary();
    assert!(!summary.contains("deadbeef"), "{summary}");
}

// ============================================================================
// Allowlist: accept / refuse
// ============================================================================

#[test]
fn allowlisted_sender_executes() {
    let router = router();
    let decision = router.handle(ROBB, "status");
    assert_eq!(
        decision,
        Decision::Execute {
            sender: ROBB.to_owned(),
            command: Command::Status,
        }
    );
}

#[test]
fn non_allowlisted_sender_is_refused_and_never_executes() {
    let router = router();
    let decision = router.handle(MALLORY, "dispatch 7893");
    match &decision {
        Decision::Refuse {
            refusal: Refusal::NotAllowlisted { sender },
        } => assert_eq!(sender, MALLORY),
        other => panic!("expected a not-allowlisted refusal, got {other:?}"),
    }
    // Logged/published, but deliberately not replied to in-room.
    assert!(decision.reply().is_none());
}

#[test]
fn an_empty_sender_is_refused() {
    let router = router();
    match router.handle("   ", "status") {
        Decision::Refuse {
            refusal: Refusal::NoSender,
        } => {}
        other => panic!("expected a no-sender refusal, got {other:?}"),
    }
}

#[test]
fn allowlist_matching_is_case_insensitive_but_not_wider() {
    let router = router();
    assert!(matches!(
        router.handle("@RoBB:SafeHouse.2amlogic.com", "status"),
        Decision::Execute { .. }
    ));
    // A near-miss on the server part is a different account, not a match.
    assert!(matches!(
        router.handle("@robb:safehouse.2amlogic.com.evil.test", "status"),
        Decision::Refuse {
            refusal: Refusal::NotAllowlisted { .. }
        }
    ));
}

#[test]
fn a_self_declared_sender_in_the_body_cannot_impersonate() {
    // The stamped `from` is the only identity. A body that claims otherwise is
    // just an unparseable command from an unauthorized sender.
    let router = router();
    let decision = router.handle(MALLORY, "from: @robb:safehouse.2amlogic.com\nstatus");
    assert!(
        matches!(
            decision,
            Decision::Refuse {
                refusal: Refusal::NotAllowlisted { .. }
            }
        ),
        "{decision:?}"
    );
}

#[test]
fn unparseable_text_from_an_allowlisted_sender_gets_a_reply() {
    let router = router();
    let decision = router.handle(ROBB, "deploy everything");
    assert!(matches!(
        decision,
        Decision::Refuse {
            refusal: Refusal::Unparsed { .. }
        }
    ));
    let reply = decision
        .reply()
        .expect("an allowlisted sender gets a reply");
    assert!(reply.contains("Refused"), "{reply}");
    assert!(reply.contains("`status`"), "usage missing from: {reply}");
}

// ============================================================================
// Nonce: issue / confirm / expiry / replay
// ============================================================================

#[test]
fn destructive_command_awaits_confirmation_instead_of_executing() {
    let router = router();
    let now = Instant::now();
    let decision = router.handle_at(ROBB, "cancel sweep-issue-7893-1", now);
    let Decision::AwaitConfirmation {
        sender,
        command,
        nonce,
    } = decision.clone()
    else {
        panic!("expected a nonce round-trip, got {decision:?}");
    };
    assert_eq!(sender, ROBB);
    assert_eq!(
        command,
        Command::Cancel {
            sweep: "sweep-issue-7893-1".to_owned()
        }
    );
    assert!(!nonce.is_empty());
    assert_eq!(router.pending_len(), 1);
    let reply = decision.reply().expect("the nonce must be echoed");
    assert!(reply.contains(&nonce), "{reply}");
}

#[test]
fn non_destructive_commands_execute_without_a_nonce() {
    let router = router();
    for text in ["status", "watch 7893", "dispatch 7893", "unblock 7893"] {
        assert!(
            matches!(router.handle(ROBB, text), Decision::Execute { .. }),
            "{text} should not have needed a nonce"
        );
    }
    assert_eq!(router.pending_len(), 0);
}

#[test]
fn confirming_within_the_ttl_executes_the_stored_command() {
    let router = router();
    let now = Instant::now();
    let Decision::AwaitConfirmation { nonce, .. } = router.handle_at(ROBB, "cancel sweep-x", now)
    else {
        panic!("expected a nonce");
    };
    let confirmed =
        router.handle_at(ROBB, &format!("confirm {nonce}"), now + Duration::from_secs(5));
    assert_eq!(
        confirmed,
        Decision::Execute {
            sender: ROBB.to_owned(),
            command: Command::Cancel {
                sweep: "sweep-x".to_owned()
            },
        }
    );
    assert_eq!(router.pending_len(), 0, "the nonce must be consumed");
}

#[test]
fn a_replayed_nonce_is_refused() {
    let router = router();
    let now = Instant::now();
    let Decision::AwaitConfirmation { nonce, .. } = router.handle_at(ROBB, "cancel sweep-x", now)
    else {
        panic!("expected a nonce");
    };
    assert!(matches!(
        router.handle_at(ROBB, &format!("confirm {nonce}"), now),
        Decision::Execute { .. }
    ));
    let replay = router.handle_at(ROBB, &format!("confirm {nonce}"), now);
    assert!(
        matches!(
            replay,
            Decision::Refuse {
                refusal: Refusal::ConfirmUnknown { .. }
            }
        ),
        "{replay:?}"
    );
}

#[test]
fn an_expired_nonce_is_refused() {
    let router = router();
    let now = Instant::now();
    let Decision::AwaitConfirmation { nonce, .. } = router.handle_at(ROBB, "cancel sweep-x", now)
    else {
        panic!("expected a nonce");
    };
    let too_late = now + DEFAULT_CONFIRM_TTL + Duration::from_secs(1);
    let expired = router.handle_at(ROBB, &format!("confirm {nonce}"), too_late);
    assert!(
        matches!(
            expired,
            Decision::Refuse {
                refusal: Refusal::ConfirmExpired { .. }
            }
        ),
        "{expired:?}"
    );
    assert_eq!(router.pending_len(), 0, "an expired entry must be dropped");
}

#[test]
fn a_nonce_is_bound_to_the_sender_who_requested_it() {
    let second = "@other:safehouse.2amlogic.com";
    let config = ChatOpsConfig {
        allowed_senders: [ROBB.to_owned(), second.to_owned()].into_iter().collect(),
        room: None,
        confirm_ttl: DEFAULT_CONFIRM_TTL,
    };
    let router = ChatOpsRouter::new(config, PERSONA.to_owned(), None);
    let now = Instant::now();
    let Decision::AwaitConfirmation { nonce, .. } = router.handle_at(ROBB, "cancel sweep-x", now)
    else {
        panic!("expected a nonce");
    };
    // A *different allowlisted* operator cannot redeem it...
    let stolen = router.handle_at(second, &format!("confirm {nonce}"), now);
    assert!(
        matches!(
            stolen,
            Decision::Refuse {
                refusal: Refusal::ConfirmWrongSender { .. }
            }
        ),
        "{stolen:?}"
    );
    // ...and the rightful owner's nonce survives the attempt.
    assert_eq!(router.pending_len(), 1);
    assert!(matches!(
        router.handle_at(ROBB, &format!("confirm {nonce}"), now),
        Decision::Execute { .. }
    ));
}

#[test]
fn an_unknown_nonce_is_refused() {
    let router = router();
    let decision = router.handle(ROBB, "confirm 000000000000");
    assert!(
        matches!(
            decision,
            Decision::Refuse {
                refusal: Refusal::ConfirmUnknown { .. }
            }
        ),
        "{decision:?}"
    );
}

#[test]
fn a_non_allowlisted_sender_cannot_confirm() {
    let router = router();
    let now = Instant::now();
    let Decision::AwaitConfirmation { nonce, .. } = router.handle_at(ROBB, "cancel sweep-x", now)
    else {
        panic!("expected a nonce");
    };
    let decision = router.handle_at(MALLORY, &format!("confirm {nonce}"), now);
    assert!(
        matches!(
            decision,
            Decision::Refuse {
                refusal: Refusal::NotAllowlisted { .. }
            }
        ),
        "{decision:?}"
    );
    assert_eq!(router.pending_len(), 1, "the allowlist gate runs first");
}

#[test]
fn nonces_are_unique_per_issue() {
    let router = router();
    let now = Instant::now();
    let mut seen = std::collections::HashSet::new();
    for n in 0..8 {
        let Decision::AwaitConfirmation { nonce, .. } =
            router.handle_at(ROBB, &format!("cancel sweep-{n}"), now)
        else {
            panic!("expected a nonce");
        };
        assert!(seen.insert(nonce), "nonce reuse");
    }
}

#[test]
fn the_pending_ledger_is_bounded() {
    let mut pending = PendingConfirmations::with_capacity(DEFAULT_CONFIRM_TTL, 2);
    let now = Instant::now();
    let first = pending.issue_at(ROBB, Command::Cancel { sweep: "a".into() }, now);
    let second =
        pending.issue_at(ROBB, Command::Cancel { sweep: "b".into() }, now + Duration::from_secs(1));
    let third =
        pending.issue_at(ROBB, Command::Cancel { sweep: "c".into() }, now + Duration::from_secs(2));
    assert_eq!(pending.len(), 2);
    // The oldest was evicted; the two newest survive.
    assert_eq!(
        pending.confirm_at(ROBB, &first, now + Duration::from_secs(3)),
        ConfirmOutcome::Unknown
    );
    assert!(matches!(
        pending.confirm_at(ROBB, &second, now + Duration::from_secs(3)),
        ConfirmOutcome::Confirmed(_)
    ));
    assert!(matches!(
        pending.confirm_at(ROBB, &third, now + Duration::from_secs(3)),
        ConfirmOutcome::Confirmed(_)
    ));
}

#[test]
fn pruning_drops_only_expired_entries() {
    let mut pending = PendingConfirmations::new(Duration::from_secs(60));
    let now = Instant::now();
    let old = pending.issue_at(ROBB, Command::Cancel { sweep: "a".into() }, now);
    let fresh = pending.issue_at(
        ROBB,
        Command::Cancel { sweep: "b".into() },
        now + Duration::from_secs(50),
    );
    assert_eq!(pending.prune_expired_at(now + Duration::from_secs(70)), 1);
    assert_eq!(
        pending.confirm_at(ROBB, &old, now + Duration::from_secs(70)),
        ConfirmOutcome::Unknown
    );
    assert!(matches!(
        pending.confirm_at(ROBB, &fresh, now + Duration::from_secs(70)),
        ConfirmOutcome::Confirmed(_)
    ));
}

// ============================================================================
// Config: off unless the block is present
// ============================================================================

#[test]
fn absent_config_block_means_inbound_steering_is_off() {
    assert!(config_from_value(None).is_none());
    assert!(config_from_value(Some(&json!("not-an-object"))).is_none());
}

#[test]
fn an_explicitly_disabled_block_is_off() {
    let block = json!({ "enabled": false, "allowedSenders": [ROBB] });
    assert!(config_from_value(Some(&block)).is_none());
}

#[test]
fn a_block_with_senders_is_on() {
    let block = json!({ "allowedSenders": [ROBB], "confirmTtlSecs": 300 });
    let resolved = config_from_value(Some(&block)).expect("block present ⇒ on");
    assert!(resolved.allows(ROBB));
    assert!(!resolved.allows(MALLORY));
    assert_eq!(resolved.confirm_ttl, Duration::from_secs(300));
}

#[test]
fn malformed_allowlist_entries_are_dropped() {
    let block = json!({
        "allowedSenders": ["loom_daemon", "robb", "", "@robb:safehouse.2amlogic.com"]
    });
    let resolved = config_from_value(Some(&block)).expect("one good entry remains");
    assert_eq!(resolved.allowed_senders.len(), 1);
    assert!(resolved.allows(ROBB));
}

#[test]
fn confirm_ttl_is_clamped_to_a_usable_window() {
    let tiny = config_from_value(Some(&json!({
        "allowedSenders": [ROBB],
        "confirmTtlSecs": 1
    })))
    .unwrap();
    assert_eq!(tiny.confirm_ttl, Duration::from_secs(10));
    let huge = config_from_value(Some(&json!({
        "allowedSenders": [ROBB],
        "confirmTtlSecs": 999_999
    })))
    .unwrap();
    assert_eq!(huge.confirm_ttl, Duration::from_secs(3600));
}

#[test]
fn chatops_room_falls_back_to_the_signal_room() {
    let safehouse = crate::safehouse::SafehouseConfig {
        room: Some("!signal:example.org".to_owned()),
        ..Default::default()
    };
    let resolved = config_from_value(Some(&json!({ "allowedSenders": [ROBB] }))).unwrap();
    assert_eq!(resolved.room(&safehouse), Some("!signal:example.org"));

    let explicit = config_from_value(Some(&json!({
        "allowedSenders": [ROBB],
        "room": "!steering:example.org"
    })))
    .unwrap();
    assert_eq!(explicit.room(&safehouse), Some("!steering:example.org"));
}

// ============================================================================
// Addressing
// ============================================================================

#[test]
fn reads_the_nested_envelope_shape_safehoused_actually_pushes() {
    let event = json!({
        "event": "message",
        "envelope": { "from": ROBB, "to": PERSONA, "body": "status" }
    });
    assert_eq!(inbound_command(&event, PERSONA), Some((ROBB.to_owned(), "status".to_owned())));
}

#[test]
fn accepts_the_at_mention_convention() {
    let event = json!({
        "event": "message",
        "envelope": { "from": ROBB, "to": "*", "body": "@loom_daemon dispatch 7893" }
    });
    assert_eq!(
        inbound_command(&event, PERSONA),
        Some((ROBB.to_owned(), "dispatch 7893".to_owned()))
    );
    let colon = json!({
        "event": "message",
        "envelope": { "from": ROBB, "to": "*", "body": "loom_daemon: status" }
    });
    assert_eq!(inbound_command(&colon, PERSONA), Some((ROBB.to_owned(), "status".to_owned())));
}

#[test]
fn ignores_room_traffic_that_is_not_addressed_to_the_daemon() {
    for body in [
        "just chatting",
        "@someone_else status",
        "@loom_daemonx status",
    ] {
        let event = json!({
            "event": "message",
            "envelope": { "from": ROBB, "to": "*", "body": body }
        });
        assert_eq!(inbound_command(&event, PERSONA), None, "{body:?}");
    }
}

#[test]
fn ignores_our_own_messages() {
    let event = json!({
        "event": "message",
        "envelope": { "from": PERSONA, "to": PERSONA, "body": "status" }
    });
    assert_eq!(inbound_command(&event, PERSONA), None);
}

#[test]
fn ignores_events_with_no_body_or_no_sender() {
    let no_body = json!({ "event": "message", "envelope": { "from": ROBB, "to": PERSONA } });
    assert_eq!(inbound_command(&no_body, PERSONA), None);
    let no_sender = json!({
        "event": "message",
        "envelope": { "to": PERSONA, "body": "status" }
    });
    assert_eq!(inbound_command(&no_sender, PERSONA), None);
}

// ============================================================================
// Event bus: every accepted and refused command is published with the sender
// ============================================================================

#[test]
fn accepted_and_refused_commands_are_published_with_sender_identity() {
    let bus = std::sync::Arc::new(EventBus::new());
    let mut sub = bus.subscribe(["safehouse.chatops"]);
    let router = ChatOpsRouter::new(config(), PERSONA.to_owned(), Some(bus.clone()));

    router.handle(ROBB, "status");
    router.handle(MALLORY, "status");
    let Decision::AwaitConfirmation { .. } = router.handle(ROBB, "cancel sweep-x") else {
        panic!("expected a nonce");
    };

    let mut seen = Vec::new();
    while let Ok(event) = sub.try_recv() {
        let Event::Generic { topic, payload } = event else {
            panic!("chatops must not add a typed event variant");
        };
        seen.push((topic, payload));
    }
    assert_eq!(seen.len(), 3, "{seen:?}");

    assert_eq!(seen[0].0, super::TOPIC_ACCEPTED);
    assert_eq!(seen[0].1["sender"], json!(ROBB));
    assert_eq!(seen[0].1["command"], json!("status"));

    assert_eq!(seen[1].0, super::TOPIC_REFUSED);
    assert_eq!(seen[1].1["sender"], json!(MALLORY));
    assert_eq!(seen[1].1["reason"], json!("sender-not-allowlisted"));

    assert_eq!(seen[2].0, super::TOPIC_CONFIRM_REQUIRED);
    assert_eq!(seen[2].1["sender"], json!(ROBB));
    assert_eq!(seen[2].1["command"], json!("cancel sweep-x"));
}

#[test]
fn a_nonce_is_never_published_on_the_bus() {
    let bus = std::sync::Arc::new(EventBus::new());
    let mut sub = bus.subscribe(["safehouse.chatops"]);
    let router = ChatOpsRouter::new(config(), PERSONA.to_owned(), Some(bus.clone()));
    let Decision::AwaitConfirmation { nonce, .. } = router.handle(ROBB, "cancel sweep-x") else {
        panic!("expected a nonce");
    };
    router.handle(ROBB, &format!("confirm {nonce}"));
    while let Ok(event) = sub.try_recv() {
        let rendered = serde_json::to_string(&event).unwrap();
        assert!(!rendered.contains(&nonce), "a nonce leaked onto the bus: {rendered}");
    }
}

// ============================================================================
// Mapping onto the existing typed IPC surface
// ============================================================================

#[test]
fn every_executable_command_maps_to_an_existing_request() {
    assert!(matches!(command_to_request(&Command::Status), Some(Request::DaemonStatus)));
    match command_to_request(&Command::Dispatch { issue: 7893 }) {
        Some(Request::DispatchSweep { kind, force, .. }) => {
            assert_eq!(kind, SweepKind::Issue(7893));
            assert!(!force, "chatops must never force past the breaker");
        }
        other => panic!("{other:?}"),
    }
    match command_to_request(&Command::Cancel {
        sweep: "sweep-x".to_owned(),
    }) {
        Some(Request::CancelSweep { sweep_id, .. }) => assert_eq!(sweep_id, "sweep-x"),
        other => panic!("{other:?}"),
    }
    match command_to_request(&Command::Unblock { issue: 42 }) {
        Some(Request::ClearQuarantine { issue, .. }) => assert_eq!(issue, 42),
        other => panic!("{other:?}"),
    }
    match command_to_request(&Command::Watch { number: 42 }) {
        Some(Request::RegisterWatch { kind, number, .. }) => {
            assert_eq!(kind, crate::watch_registry::WatchKind::Issue);
            assert_eq!(number, 42);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn confirm_is_never_executable() {
    assert!(command_to_request(&Command::Confirm {
        nonce: "abc".to_owned()
    })
    .is_none());
}

#[test]
fn responses_render_as_one_terse_room_line() {
    let cancelled = render_response(
        &Command::Cancel {
            sweep: "sweep-x".to_owned(),
        },
        &Response::SweepCancelled {
            sweep_id: "sweep-x".to_owned(),
            pid: 42,
            sigkill_sent: false,
            was_running: true,
        },
    );
    assert!(cancelled.contains("sweep-x"), "{cancelled}");

    let failed = render_response(
        &Command::Status,
        &Response::Error {
            message: "nope".to_owned(),
        },
    );
    assert!(failed.contains("nope"), "{failed}");
}
