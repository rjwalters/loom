use super::*;
use crate::types::{SweepId, SweepKind};
use crate::workspace_registry::normalize_path;
use serde_json::json;
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};
use tokio::net::UnixListener;

/// Test isolation guard (issue #4583). `run_sink` now reads/writes two
/// machine-level paths by default: the persisted merge-reconciliation
/// dedup file (`~/.loom/safehouse-completed.json`) and the workspace
/// registry (`~/.loom/workspaces.json`, consulted read-only as a
/// reconciliation-target source). A host actually running `loom-daemon`
/// has both populated with real state — without this guard, the test
/// suite would read/write that real state instead of a hermetic tempdir,
/// making both correctness (never touch a real daemon's files from a unit
/// test) and determinism (reconciliation's target set must not depend on
/// whatever repos happen to be registered on the machine running the
/// tests) fail silently. Every `run_sink` test holds one of these for its
/// duration; `Drop` restores the ambient (unset) env regardless of panic.
struct SafehouseTestPaths;

impl SafehouseTestPaths {
    fn set(dir: &std::path::Path) -> Self {
        std::env::set_var(COMPLETIONS_PATH_ENV, dir.join("safehouse-completed.json"));
        std::env::set_var(
            crate::workspace_registry::REGISTRY_PATH_ENV,
            dir.join("workspaces.json"),
        );
        Self
    }
}

impl Drop for SafehouseTestPaths {
    fn drop(&mut self) {
        std::env::remove_var(COMPLETIONS_PATH_ENV);
        std::env::remove_var(crate::workspace_registry::REGISTRY_PATH_ENV);
    }
}

// ---- connection-state cell + wire rendering (#4345) ----

#[test]
fn new_shared_state_defaults_to_not_configured() {
    let state = new_shared_state();
    assert_eq!(snapshot_state(&state), SafehouseState::NotConfigured);
}

#[test]
fn set_not_configured_overwrites_any_prior_state() {
    let state = new_shared_state();
    set_state(
        &state,
        SafehouseState::Connected {
            socket: PathBuf::from("/tmp/x.sock"),
            room: None,
        },
    );
    set_not_configured(&state);
    assert_eq!(snapshot_state(&state), SafehouseState::NotConfigured);
}

#[test]
fn state_to_status_maps_all_three_states() {
    let not_configured = SafehouseState::NotConfigured.to_status();
    assert_eq!(not_configured.state, "not_configured");
    assert!(not_configured.socket.is_none());
    assert!(not_configured.room.is_none());

    let unreachable = SafehouseState::Unreachable {
        socket: PathBuf::from("/tmp/x.sock"),
    }
    .to_status();
    assert_eq!(unreachable.state, "unreachable");
    assert_eq!(unreachable.socket, Some(PathBuf::from("/tmp/x.sock")));
    assert!(unreachable.room.is_none());

    let connected = SafehouseState::Connected {
        socket: PathBuf::from("/tmp/x.sock"),
        room: Some("fleet".to_owned()),
    }
    .to_status();
    assert_eq!(connected.state, "connected");
    assert_eq!(connected.socket, Some(PathBuf::from("/tmp/x.sock")));
    assert_eq!(connected.room.as_deref(), Some("fleet"));

    // A connected state with no configured room (safehoused resolved the
    // sole joined room server-side) still reports "connected" — `room`
    // just stays `None` rather than inventing a name.
    let connected_no_room = SafehouseState::Connected {
        socket: PathBuf::from("/tmp/x.sock"),
        room: None,
    }
    .to_status();
    assert_eq!(connected_no_room.state, "connected");
    assert!(connected_no_room.room.is_none());

    // #4464: the send-rejected state renders a socket + reason, no room,
    // and a distinct wire string so `loom-daemon status` can say
    // "connected, sends rejected: <reason>" rather than "unreachable".
    let send_rejected = SafehouseState::SendRejected {
        socket: PathBuf::from("/tmp/x.sock"),
        reason: "'room' required: 3 rooms joined".to_owned(),
    }
    .to_status();
    assert_eq!(send_rejected.state, "send_rejected");
    assert_eq!(send_rejected.socket, Some(PathBuf::from("/tmp/x.sock")));
    assert!(send_rejected.room.is_none());
    assert_eq!(send_rejected.reason.as_deref(), Some("'room' required: 3 rooms joined"));
}

// ---- config resolution (config > default, no env) ----

#[test]
fn config_absent_block_is_disabled_default() {
    let cfg = config_from_value(None);
    assert_eq!(cfg, SafehouseConfig::default());
    assert!(!cfg.enabled);
    assert_eq!(cfg.persona, "loom_daemon");
    assert!(cfg.socket.is_none());
}

#[test]
fn config_reads_block_over_default() {
    let block = json!({
        "enabled": true,
        "socket": "/tmp/x.sock",
        "room": "fleet",
        "persona": "loom_daemon"
    });
    let cfg = config_from_value(Some(&block));
    assert!(cfg.enabled);
    assert_eq!(cfg.socket, Some(PathBuf::from("/tmp/x.sock")));
    assert_eq!(cfg.room.as_deref(), Some("fleet"));
    assert_eq!(cfg.persona, "loom_daemon");
}

#[test]
fn config_malformed_block_is_disabled_not_panic() {
    // A non-object (e.g. a stray string) must resolve to disabled default.
    let cfg = config_from_value(Some(&json!("not-an-object")));
    assert!(!cfg.enabled);
    assert_eq!(cfg, SafehouseConfig::default());
}

#[test]
fn config_empty_string_fields_fall_back_to_default() {
    let block = json!({"enabled": true, "socket": "", "room": "  ", "persona": ""});
    let cfg = config_from_value(Some(&block));
    assert!(cfg.enabled);
    assert!(cfg.socket.is_none());
    assert!(cfg.room.is_none());
    assert_eq!(cfg.persona, "loom_daemon");
}

// ---- config resolution (env > config) ----

#[test]
#[serial]
fn env_overrides_config_for_all_keys() {
    std::env::set_var(ENABLED_ENV, "true");
    std::env::set_var(SOCKET_ENV, "/env/sock");
    std::env::set_var(ROOM_ENV, "env-room");
    std::env::set_var(PERSONA_ENV, "loom_env");

    let base = config_from_value(Some(&json!({
        "enabled": false, "socket": "/cfg/sock", "room": "cfg-room", "persona": "loom_cfg"
    })));
    let cfg = apply_env_overrides(base);

    assert!(cfg.enabled);
    assert_eq!(cfg.socket, Some(PathBuf::from("/env/sock")));
    assert_eq!(cfg.room.as_deref(), Some("env-room"));
    assert_eq!(cfg.persona, "loom_env");

    std::env::remove_var(ENABLED_ENV);
    std::env::remove_var(SOCKET_ENV);
    std::env::remove_var(ROOM_ENV);
    std::env::remove_var(PERSONA_ENV);
}

#[test]
#[serial]
fn env_enabled_false_overrides_config_true() {
    std::env::set_var(ENABLED_ENV, "0");
    let cfg = apply_env_overrides(config_from_value(Some(&json!({"enabled": true}))));
    assert!(!cfg.enabled);
    std::env::remove_var(ENABLED_ENV);
}

#[test]
#[serial]
fn socket_falls_back_to_safehoused_socket_env() {
    std::env::remove_var(SOCKET_ENV);
    std::env::set_var(SAFEHOUSED_SOCKET_ENV, "/run/safehoused.sock");
    let cfg = SafehouseConfig {
        enabled: true,
        ..SafehouseConfig::default()
    };
    assert_eq!(resolve_socket(&cfg), Some(PathBuf::from("/run/safehoused.sock")));
    std::env::remove_var(SAFEHOUSED_SOCKET_ENV);
}

/// #5457 regression: a committed `cfg.socket` (e.g. a stale foreign-host
/// path in shared `.loom/config.json`) must never shadow
/// `$SAFEHOUSED_SOCKET` — env wins over config, mirroring
/// `resolve_ingest_key_file_env_overrides_config` in
/// `observability/mod.rs`. Before this fix `resolve_socket` checked
/// `cfg.socket` first, so this env var could never take effect while any
/// `socket` value was configured.
#[test]
#[serial]
fn resolve_socket_safehoused_env_overrides_configured_socket() {
    std::env::remove_var(SOCKET_ENV);
    std::env::set_var(SAFEHOUSED_SOCKET_ENV, "/run/safehoused.sock");
    let cfg = SafehouseConfig {
        enabled: true,
        socket: Some(PathBuf::from("/Users/somebody/.loom/safehoused/state/safehoused.sock")),
        ..SafehouseConfig::default()
    };
    assert_eq!(resolve_socket(&cfg), Some(PathBuf::from("/run/safehoused.sock")));
    std::env::remove_var(SAFEHOUSED_SOCKET_ENV);
}

/// `$LOOM_SAFEHOUSE_SOCKET` also wins over a configured `cfg.socket`,
/// checked directly by `resolve_socket` (not only via the earlier
/// `apply_env_overrides` merge) so the precedence holds even if a caller
/// passes a config value that predates that merge.
#[test]
#[serial]
fn resolve_socket_loom_safehouse_socket_env_overrides_configured_socket() {
    std::env::remove_var(SAFEHOUSED_SOCKET_ENV);
    std::env::set_var(SOCKET_ENV, "/env/sock");
    let cfg = SafehouseConfig {
        enabled: true,
        socket: Some(PathBuf::from("/Users/somebody/.loom/safehoused/state/safehoused.sock")),
        ..SafehouseConfig::default()
    };
    assert_eq!(resolve_socket(&cfg), Some(PathBuf::from("/env/sock")));
    std::env::remove_var(SOCKET_ENV);
}

/// No env set at all ⇒ falls back to the configured value (still the
/// lowest-priority tier, never removed — a deliberately configured
/// `safehouse.socket` with no env override present must still resolve).
#[test]
#[serial]
fn resolve_socket_config_wins_over_none_when_no_env_set() {
    std::env::remove_var(SOCKET_ENV);
    std::env::remove_var(SAFEHOUSED_SOCKET_ENV);
    let cfg = SafehouseConfig {
        enabled: true,
        socket: Some(PathBuf::from("/run/configured.sock")),
        ..SafehouseConfig::default()
    };
    assert_eq!(resolve_socket(&cfg), Some(PathBuf::from("/run/configured.sock")));
}

/// No committed value and no env var ⇒ `None`, never a panic.
#[test]
#[serial]
fn resolve_socket_resolves_to_none_with_nothing_configured() {
    std::env::remove_var(SOCKET_ENV);
    std::env::remove_var(SAFEHOUSED_SOCKET_ENV);
    let cfg = SafehouseConfig::default();
    assert_eq!(resolve_socket(&cfg), None);
}

// ---- attention-class room routing: kind → room table (#4225) ----

/// The routing `match` is exhaustive over [`EnvelopeKind`] with no wildcard
/// arm, so a sixth *enum* member fails to compile. This test is the other
/// half of that guard: it pins the enum to the wire-level [`KNOWN_TYPES`], so
/// a sixth member added to only one of the two representations fails here
/// instead of silently escaping the routing table.
#[test]
fn known_types_and_envelope_kind_stay_in_lockstep() {
    assert_eq!(
        KNOWN_TYPES.len(),
        EnvelopeKind::ALL.len(),
        "a new envelope type must be added to BOTH KNOWN_TYPES and EnvelopeKind \
             (the latter is what makes the #4225 routing match exhaustive)"
    );
    for (wire, kind) in KNOWN_TYPES.iter().zip(EnvelopeKind::ALL) {
        assert_eq!(*wire, kind.as_str(), "KNOWN_TYPES and EnvelopeKind::ALL must agree");
        assert_eq!(EnvelopeKind::parse(wire), Some(kind));
    }
    assert_eq!(EnvelopeKind::parse("smoke_signal"), None);
}

/// The final routing table (#4225, extended by #4217's `digest`):
/// `handoff`/`ack`/`completion`/`digest` → signal, `task`/`chat` → the repo
/// firehose. Every known type is covered and each resolves to exactly one
/// class.
#[test]
fn attention_class_routes_every_known_type_to_exactly_one_tier() {
    let expected = [
        ("chat", AttentionClass::Firehose),
        ("task", AttentionClass::Firehose),
        ("handoff", AttentionClass::Signal),
        ("ack", AttentionClass::Signal),
        ("completion", AttentionClass::Signal),
        ("digest", AttentionClass::Signal),
    ];
    assert_eq!(
        expected.len(),
        KNOWN_TYPES.len(),
        "every KNOWN_TYPES member needs a routing expectation here"
    );
    for (kind, class) in expected {
        let parsed = EnvelopeKind::parse(kind).expect("known type must parse");
        assert_eq!(
            parsed.attention_class(),
            class,
            "{kind:?} must route to {class:?} and nowhere else"
        );
    }
}

/// `completion` (#4553, the newest `KNOWN_TYPES` member) is a terminal
/// outcome and belongs in the operator's signal room — called out explicitly
/// because it is the easiest one to miss.
#[test]
fn completion_routes_to_the_signal_room() {
    assert_eq!(EnvelopeKind::Completion.attention_class(), AttentionClass::Signal);

    let cfg = routing_config();
    let router = RoomRouter::new(&cfg);
    assert_eq!(
        router.resolve("completion", Some("/home/x/GitHub/loom")),
        RoomDecision::Send(Some("!signal:example.org".to_owned())),
        "a completion must reach the signal room even when the repo has its own firehose"
    );
}

// ---- attention-class room routing: RoomRouter (#4225) ----

/// A `rooms` map with a signal room and one pre-configured repo firehose.
fn routing_config() -> SafehouseConfig {
    SafehouseConfig {
        enabled: true,
        room: None,
        rooms: Some(RoomMap {
            signal: Some("!signal:example.org".to_owned()),
            by_repo: [("loom".to_owned(), "!fleet-loom:example.org".to_owned())]
                .into_iter()
                .collect(),
            claims: None,
        }),
        ..SafehouseConfig::default()
    }
}

/// **The most important regression guard of #4225**: with no `rooms` map,
/// every envelope of every kind resolves to the single configured `room`,
/// exactly as before — including the `None` "let safehoused resolve its sole
/// room" form, which must still serialize with no `room` key at all.
///
/// Also covers #4713: with no `rooms` map (and so no `rooms.claims`),
/// [`SafehouseConfig::claims_room`] must resolve to the exact same single
/// `room` — the claim-ad routing path must be byte-identical to pre-#4713
/// behavior too, not just the narration path.
#[test]
fn absent_rooms_map_is_byte_identical_single_room_behavior() {
    for room in [Some("loom-fleet".to_owned()), None] {
        let cfg = SafehouseConfig {
            enabled: true,
            room: room.clone(),
            ..SafehouseConfig::default()
        };
        assert!(!cfg.routes_by_attention());
        assert_eq!(
            cfg.claims_room(),
            room.as_deref(),
            "with no rooms map, claim ads must resolve to the same single room as narration"
        );
        let router = RoomRouter::new(&cfg);
        for kind in KNOWN_TYPES {
            for repo in [Some("/home/x/GitHub/vibesql"), None] {
                assert_eq!(
                    router.resolve(kind, repo),
                    RoomDecision::Send(room.clone()),
                    "with no rooms map, {kind:?} (repo={repo:?}) must go to the single room"
                );
            }
        }
    }

    // And the wire shape of that `None` case: no `room` key is emitted.
    let env = Envelope {
        to: "*".to_owned(),
        kind: "task".to_owned(),
        task_id: Some("loom_4225".to_owned()),
        body: "loom#4225 · dispatch".to_owned(),
        meta: None,
    };
    let req = build_send_request(&env, 1, None).unwrap();
    assert!(req.get("room").is_none(), "single-room mode with room=null sends no room key");
}

#[test]
fn signal_room_falls_back_to_the_legacy_room_key() {
    // Migration shape: an operator adds `rooms.byRepo` but leaves the signal
    // room as the existing scalar `safehouse.room`.
    let cfg = SafehouseConfig {
        enabled: true,
        room: Some("!legacy:example.org".to_owned()),
        rooms: Some(RoomMap {
            signal: None,
            by_repo: [("loom".to_owned(), "!fleet-loom:example.org".to_owned())]
                .into_iter()
                .collect(),
            claims: None,
        }),
        ..SafehouseConfig::default()
    };
    assert_eq!(cfg.signal_room(), Some("!legacy:example.org"));
    let router = RoomRouter::new(&cfg);
    assert_eq!(
        router.resolve("handoff", Some("/home/x/GitHub/loom")),
        RoomDecision::Send(Some("!legacy:example.org".to_owned()))
    );
    // …and `rooms.signal`, when set, wins over the legacy scalar.
    assert_eq!(routing_config().signal_room(), Some("!signal:example.org"));
}

/// #4713: `claims_room()` falls back to `signal_room()` — which itself may
/// fall back further to the legacy scalar `room` — when `rooms.claims` is
/// absent, mirroring `signal_room`'s own fallback chain one level down.
#[test]
fn claims_room_falls_back_to_signal_room_when_absent() {
    // rooms.claims absent, rooms.signal present ⇒ claims_room is the signal room.
    let cfg = routing_config();
    assert!(cfg.rooms.as_ref().unwrap().claims.is_none());
    assert_eq!(cfg.claims_room(), Some("!signal:example.org"));
    assert_eq!(cfg.claims_room(), cfg.signal_room());

    // rooms.claims present ⇒ it wins over the signal room.
    let cfg_with_claims = SafehouseConfig {
        rooms: Some(RoomMap {
            claims: Some("!claims:example.org".to_owned()),
            ..cfg.rooms.clone().unwrap()
        }),
        ..cfg
    };
    assert_eq!(cfg_with_claims.claims_room(), Some("!claims:example.org"));
    assert_eq!(
        cfg_with_claims.signal_room(),
        Some("!signal:example.org"),
        "narration routing (signal_room) must be unaffected by rooms.claims"
    );

    // No rooms map at all ⇒ claims_room falls all the way back to the
    // legacy scalar `room`, exactly like `signal_room` does.
    let legacy = SafehouseConfig {
        enabled: true,
        room: Some("!legacy:example.org".to_owned()),
        ..SafehouseConfig::default()
    };
    assert_eq!(legacy.claims_room(), Some("!legacy:example.org"));
}

/// #4713's "opt-in, default-unchanged" contract, claims-only edition:
/// setting `rooms.claims` **alone** (no `signal`, no `byRepo`) redirects
/// the peer-claim coordination connection and nothing else. Narration must
/// stay in single-room mode — byte-identical to the absent-map case —
/// rather than silently activating attention-class routing (per-repo lazy
/// firehose creation) as a side effect of an unrelated knob.
#[test]
fn claims_only_map_does_not_activate_attention_class_narration_routing() {
    for room in [Some("loom-fleet".to_owned()), None] {
        let cfg = SafehouseConfig {
            enabled: true,
            room: room.clone(),
            rooms: Some(RoomMap {
                signal: None,
                by_repo: std::collections::BTreeMap::new(),
                claims: Some("!claims:example.org".to_owned()),
            }),
            ..SafehouseConfig::default()
        };
        // The claims knob works…
        assert_eq!(cfg.claims_room(), Some("!claims:example.org"));
        // …and narration routing does not notice it.
        assert!(
            !cfg.routes_by_attention(),
            "a claims-only rooms map must not enable attention-class narration routing"
        );
        let router = RoomRouter::new(&cfg);
        assert_eq!(router.signal_room(), room.clone());
        for kind in KNOWN_TYPES {
            for repo in [Some("/home/x/GitHub/vibesql"), None] {
                assert_eq!(
                    router.resolve(kind, repo),
                    RoomDecision::Send(room.clone()),
                    "claims-only map: {kind:?} (repo={repo:?}) must stay in \
                         single-room mode, never Create a firehose room"
                );
            }
        }
    }
}

#[test]
fn firehose_kinds_route_to_the_configured_repo_room() {
    let cfg = routing_config();
    let router = RoomRouter::new(&cfg);
    for kind in ["task", "chat"] {
        assert_eq!(
            router.resolve(kind, Some("/home/x/GitHub/loom")),
            RoomDecision::Send(Some("!fleet-loom:example.org".to_owned())),
            "{kind:?} is repo chatter and belongs in that repo's firehose"
        );
    }
    for kind in ["handoff", "ack", "completion"] {
        assert_eq!(
            router.resolve(kind, Some("/home/x/GitHub/loom")),
            RoomDecision::Send(Some("!signal:example.org".to_owned())),
            "{kind:?} is a human-attention outcome and belongs in the signal room"
        );
    }
}

#[test]
fn unconfigured_repo_firehose_is_created_lazily_then_reused() {
    let cfg = routing_config();
    let mut router = RoomRouter::new(&cfg);
    // vibesql has no configured firehose ⇒ create `fleet-vibesql` on first
    // narration (lazily — never eagerly for every managed repo).
    assert_eq!(
        router.resolve("task", Some("/home/x/GitHub/vibesql")),
        RoomDecision::Create {
            repo: "vibesql".to_owned(),
            alias: "fleet-vibesql".to_owned(),
            fallback: Some("!signal:example.org".to_owned()),
        }
    );
    router.record_created("vibesql", "!created:example.org".to_owned());
    // Every later message reuses the recorded id — no second create_room op.
    assert_eq!(
        router.resolve("task", Some("/home/x/GitHub/vibesql")),
        RoomDecision::Send(Some("!created:example.org".to_owned()))
    );
    // Signal-class traffic for the same repo still goes to the signal room.
    assert_eq!(
        router.resolve("handoff", Some("/home/x/GitHub/vibesql")),
        RoomDecision::Send(Some("!signal:example.org".to_owned()))
    );
}

#[test]
fn uncreatable_repo_room_degrades_to_signal_and_warns_once() {
    let cfg = routing_config();
    let mut router = RoomRouter::new(&cfg);
    assert!(matches!(
        router.resolve("task", Some("/home/x/GitHub/anvil")),
        RoomDecision::Create { .. }
    ));
    // First failure ⇒ warn (record_degraded returns true exactly once).
    assert!(router.record_degraded("anvil"), "the first failure must warn");
    assert!(!router.record_degraded("anvil"), "later failures must NOT warn again");
    // …and from then on this repo narrates into the signal room, with no
    // further creation attempts (never a blocked or failed sweep).
    for kind in KNOWN_TYPES {
        assert_eq!(
            router.resolve(kind, Some("/home/x/GitHub/anvil")),
            RoomDecision::Send(Some("!signal:example.org".to_owned())),
            "a degraded repo must keep narrating ({kind:?}), just into the signal room"
        );
    }
}

#[test]
fn firehose_without_a_repo_degrades_to_signal_rather_than_inventing_a_room() {
    let cfg = routing_config();
    let router = RoomRouter::new(&cfg);
    // No repo stamped (a synthetic/test event, or daemon-wide news): there is
    // no per-repo firehose to route to, so it lands in the signal room.
    assert_eq!(
        router.resolve("task", None),
        RoomDecision::Send(Some("!signal:example.org".to_owned()))
    );
    // A routing mode with no signal room configured at all resolves to `None`
    // — the documented "explicit ids required once the map exists" caveat,
    // where safehoused answers `'room' required` and #4464's send-rejected
    // status names the fix. It never panics and never drops the message.
    let cfg = SafehouseConfig {
        enabled: true,
        room: None,
        rooms: Some(RoomMap {
            signal: None,
            by_repo: [("loom".to_owned(), "!fleet-loom:example.org".to_owned())]
                .into_iter()
                .collect(),
            claims: None,
        }),
        ..SafehouseConfig::default()
    };
    assert_eq!(RoomRouter::new(&cfg).resolve("handoff", None), RoomDecision::Send(None));
}

#[test]
fn repo_room_alias_uses_the_narration_basename_convention() {
    assert_eq!(repo_room_alias("vibesql"), "fleet-vibesql");
    // The basename convention (#4201) is what keys the map, so a full
    // workspace path resolves to the same room as its basename.
    let cfg = routing_config();
    let router = RoomRouter::new(&cfg);
    assert_eq!(
        router.resolve("task", Some("/Users/someone/GitHub/loom")),
        router.resolve("task", Some("loom"))
    );
}

#[test]
fn event_repo_reads_the_stamped_workspace_root() {
    assert_eq!(
        event_repo(&Event::SweepPhase {
            issue: 4225,
            phase: "builder".to_owned(),
            pr_number: None,
            repo: Some("/home/x/GitHub/loom".to_owned()),
        }),
        Some("/home/x/GitHub/loom")
    );
    assert_eq!(
        event_repo(&Event::SweepGlobalCompleted {
            sweep_id: "sweep-issue-4225-1".to_owned() as SweepId,
            outcome: crate::types::SweepOutcome::Exited,
        }),
        None
    );
}

// ---- attention-class room routing: config + env (#4225) ----

#[test]
fn config_parses_the_rooms_map() {
    let block = json!({
        "enabled": true,
        "rooms": {
            "signal": "!signal:example.org",
            "byRepo": {"loom": "!fleet-loom:example.org", "vibesql": "!fleet-vibesql:example.org"},
            "claims": "!claims:example.org"
        }
    });
    let cfg = config_from_value(Some(&block));
    let rooms = cfg.rooms.expect("the rooms map must parse");
    assert_eq!(rooms.signal.as_deref(), Some("!signal:example.org"));
    assert_eq!(rooms.by_repo.get("loom").map(String::as_str), Some("!fleet-loom:example.org"));
    assert_eq!(
        rooms.by_repo.get("vibesql").map(String::as_str),
        Some("!fleet-vibesql:example.org")
    );
    assert_eq!(rooms.claims.as_deref(), Some("!claims:example.org"));
}

/// #4713: `rooms.claims` parses independently of `rooms.signal`/`byRepo` —
/// an operator can set only the claims room and leave narration routing
/// entirely on the legacy single-room scalar.
#[test]
fn config_parses_the_rooms_claims_field_alone() {
    let block = json!({
        "enabled": true,
        "room": "loom-fleet",
        "rooms": {"claims": "!claims:example.org"}
    });
    let cfg = config_from_value(Some(&block));
    assert_eq!(cfg.claims_room(), Some("!claims:example.org"));
    assert_eq!(
        cfg.signal_room(),
        Some("loom-fleet"),
        "narration keeps using the legacy scalar room when only rooms.claims is set"
    );
    let rooms = cfg
        .rooms
        .expect("rooms.claims alone must still produce a non-empty map");
    assert!(rooms.signal.is_none());
    assert!(rooms.by_repo.is_empty());
    assert_eq!(rooms.claims.as_deref(), Some("!claims:example.org"));
}

#[test]
fn config_without_a_rooms_map_stays_in_single_room_mode() {
    // The migration default: absent, malformed, and present-but-empty all
    // resolve to `None` ⇒ unchanged single-room behavior.
    for block in [
        json!({"enabled": true, "room": "loom-fleet"}),
        json!({"enabled": true, "rooms": {}}),
        json!({"enabled": true, "rooms": "loom-fleet"}),
        json!({"enabled": true, "rooms": {"signal": "  ", "byRepo": {}}}),
        json!({"enabled": true, "rooms": {"byRepo": {"loom": ""}}}),
        json!({"enabled": true, "rooms": {"byRepo": ["loom"]}}),
        json!({"enabled": true, "rooms": {"claims": "  "}}),
    ] {
        let cfg = config_from_value(Some(&block));
        assert!(
            cfg.rooms.is_none(),
            "{block} must resolve to single-room mode, got {:?}",
            cfg.rooms
        );
        assert!(!cfg.routes_by_attention());
    }
}

#[test]
#[serial]
fn env_overrides_config_for_the_rooms_map() {
    std::env::set_var(ROOM_SIGNAL_ENV, "!env-signal:example.org");
    std::env::set_var(ROOM_CLAIMS_ENV, "!env-claims:example.org");
    std::env::set_var(ROOMS_BY_REPO_ENV, "loom=!env-loom:example.org, anvil=!env-anvil:x");

    let cfg = apply_env_overrides(config_from_value(Some(&json!({
        "enabled": true,
        "rooms": {
            "signal": "!cfg-signal:example.org",
            "byRepo": {"loom": "!cfg-loom:example.org", "vibesql": "!cfg-vibesql:example.org"},
            "claims": "!cfg-claims:example.org"
        }
    }))));
    let rooms = cfg.rooms.expect("env must keep the map present");
    assert_eq!(rooms.signal.as_deref(), Some("!env-signal:example.org"));
    assert_eq!(rooms.by_repo.get("loom").map(String::as_str), Some("!env-loom:example.org"));
    assert_eq!(rooms.by_repo.get("anvil").map(String::as_str), Some("!env-anvil:x"));
    assert!(
        !rooms.by_repo.contains_key("vibesql"),
        "the byRepo env override replaces the whole map rather than merging into it"
    );
    assert_eq!(rooms.claims.as_deref(), Some("!env-claims:example.org"));

    std::env::remove_var(ROOM_SIGNAL_ENV);
    std::env::remove_var(ROOM_CLAIMS_ENV);
    std::env::remove_var(ROOMS_BY_REPO_ENV);
}

#[test]
#[serial]
fn env_alone_can_enable_routing_and_absent_env_changes_nothing() {
    // Hermetic against ambient `LOOM_SAFEHOUSE_ROOM` (#5801): a real
    // loom-daemon dogfooding host sets this permanently in its process
    // environment, and `signal_room()` falls back to the legacy scalar
    // `room` field (populated from this env var by `apply_env_overrides`)
    // whenever `rooms.signal` is unset — exactly the state the
    // `rooms.claims`-alone assertion below exercises. Save/clear it for the
    // duration of this test and restore whatever the host had on exit
    // (even on panic), the same guard-on-Drop idiom `SafehouseTestPaths`
    // uses above for a similar ambient-state hazard.
    struct RestoreAmbientRoom(Option<String>);
    impl Drop for RestoreAmbientRoom {
        fn drop(&mut self) {
            match &self.0 {
                Some(value) => std::env::set_var(ROOM_ENV, value),
                None => std::env::remove_var(ROOM_ENV),
            }
        }
    }
    let _restore_ambient_room = RestoreAmbientRoom(std::env::var(ROOM_ENV).ok());
    std::env::remove_var(ROOM_ENV);

    // Env with no config `rooms` block at all ⇒ routing enabled from env.
    std::env::set_var(ROOM_SIGNAL_ENV, "!env-signal:example.org");
    let cfg = apply_env_overrides(config_from_value(Some(&json!({"enabled": true}))));
    assert_eq!(cfg.signal_room(), Some("!env-signal:example.org"));
    std::env::remove_var(ROOM_SIGNAL_ENV);

    // `LOOM_SAFEHOUSE_ROOM_CLAIMS` alone (#4713) is likewise enough to
    // enable routing from env, with no `rooms.signal` present at all.
    std::env::set_var(ROOM_CLAIMS_ENV, "!env-claims:example.org");
    let cfg = apply_env_overrides(config_from_value(Some(&json!({"enabled": true}))));
    assert_eq!(cfg.claims_room(), Some("!env-claims:example.org"));
    assert!(
        cfg.signal_room().is_none(),
        "rooms.claims alone must not affect the (unset) signal room"
    );
    std::env::remove_var(ROOM_CLAIMS_ENV);

    // Neither env var set ⇒ the config layer's map is returned untouched, so
    // the absent-map single-room default stays byte-identical. `LOOM_SAFEHOUSE_ROOM`
    // itself is cleared for the whole test above, so `room` precedence here is
    // deterministic too (it's covered in depth by `env_overrides_config_for_all_keys`).
    std::env::remove_var(ROOMS_BY_REPO_ENV);
    let cfg = apply_env_overrides(config_from_value(Some(
        &json!({"enabled": true, "room": "loom-fleet"}),
    )));
    assert!(cfg.rooms.is_none());
    assert_eq!(
        config_from_value(Some(&json!({"enabled": true, "room": "loom-fleet"}))).signal_room(),
        Some("loom-fleet"),
        "with no rooms map the signal room IS the legacy scalar room"
    );
    assert_eq!(
        config_from_value(Some(&json!({"enabled": true, "room": "loom-fleet"}))).claims_room(),
        Some("loom-fleet"),
        "with no rooms map the claims room IS also the legacy scalar room"
    );

    // A garbage byRepo env value degrades to the pairs it can parse (here:
    // none) instead of panicking.
    assert!(parse_by_repo_env("loom,,=,=x,vibesql=").is_empty());
    assert_eq!(
        parse_by_repo_env("loom=!a:x,,vibesql = !b:x ")
            .get("vibesql")
            .map(String::as_str),
        Some("!b:x")
    );
}

// ---- envelope serialization / validation ----

#[test]
fn send_request_emits_v1_and_never_from() {
    let env = Envelope {
        to: "*".to_owned(),
        kind: "task".to_owned(),
        task_id: Some("4137".to_owned()),
        body: "hi".to_owned(),
        meta: None,
    };
    let req = build_send_request(&env, 7, None).unwrap();
    assert_eq!(req["v"], json!(1));
    assert_eq!(req["id"], json!(7));
    assert_eq!(req["op"], json!("send"));
    assert_eq!(req["to"], json!("*"));
    assert_eq!(req["type"], json!("task"));
    assert_eq!(req["task_id"], json!("4137"));
    assert!(req.get("from").is_none(), "from must never be serialized");
}

#[test]
fn send_request_omits_task_id_when_absent() {
    let env = Envelope {
        to: "*".to_owned(),
        kind: "ack".to_owned(),
        task_id: None,
        body: "done".to_owned(),
        meta: None,
    };
    let req = build_send_request(&env, 1, None).unwrap();
    assert!(req.get("task_id").is_none());
}

#[test]
fn send_request_includes_room_when_present() {
    let env = Envelope {
        to: "*".to_owned(),
        kind: "task".to_owned(),
        task_id: None,
        body: "x".to_owned(),
        meta: None,
    };
    let req = build_send_request(&env, 1, Some("fleet")).unwrap();
    assert_eq!(req["room"], json!("fleet"));
}

#[test]
fn send_request_rejects_unknown_type() {
    let env = Envelope {
        to: "*".to_owned(),
        kind: "smoke_signal".to_owned(),
        task_id: None,
        body: "x".to_owned(),
        meta: None,
    };
    assert!(build_send_request(&env, 1, None).is_err());
}

#[test]
fn send_request_rejects_hyphenated_task_id() {
    let env = Envelope {
        to: "*".to_owned(),
        kind: "task".to_owned(),
        task_id: Some("issue-4137".to_owned()),
        body: "x".to_owned(),
        meta: None,
    };
    assert!(build_send_request(&env, 1, None).is_err());
}

#[test]
fn to_normalization_folds_hyphens_and_rejects_garbage() {
    // Hyphenated persona is normalized to underscore (not sent into the void).
    let env = Envelope {
        to: "loom-builder".to_owned(),
        kind: "task".to_owned(),
        task_id: None,
        body: "x".to_owned(),
        meta: None,
    };
    let req = build_send_request(&env, 1, None).unwrap();
    assert_eq!(req["to"], json!("loom_builder"));

    // "*" and @matrix ids pass through untouched.
    assert_eq!(normalize_to("*").unwrap(), "*");
    assert_eq!(normalize_to("@a:b.c").unwrap(), "@a:b.c");

    // A value that cannot be a persona is rejected, not silently sent.
    assert!(normalize_to("has space").is_err());
}

// ---- completion envelopes + completion-v1 meta (#4426) ----

/// A valid `completion-v1` source, tweaked per-test.
fn sample_completion_meta() -> CompletionMeta {
    CompletionMeta {
        agent: "loom_daemon".to_owned(),
        repo_slug: "rjwalters/loom".to_owned(),
        pr_url: "https://github.com/rjwalters/loom/pull/4321".to_owned(),
        result: CompletionResult::Success,
        started_at: "2026-07-29T10:00:00Z".to_owned(),
        completed_at: "2026-07-29T10:12:30Z".to_owned(),
        issue: Some(4321),
        tokens: Some(791_000),
        tokens_by_model: Some(vec![ModelUsageTotals {
            model: "claude-sonnet-5".to_owned(),
            speed: "standard".to_owned(),
            service_tier: "standard".to_owned(),
            input: 1_000,
            cache_read: 700_000,
            cache_write_5m: 1_000,
            cache_write_1h: 89_000,
            output: 30_000,
        }]),
        title: Some("Add repo-qualified task_id".to_owned()),
        additions: Some(214),
        deletions: Some(37),
        visibility: Some(RepoVisibility::Public),
    }
}

#[test]
fn completion_is_a_known_type() {
    assert!(KNOWN_TYPES.contains(&"completion"));
}

#[test]
fn send_request_accepts_completion_with_valid_meta() {
    let meta = sample_completion_meta().to_meta_value().unwrap();
    let env = Envelope {
        to: "*".to_owned(),
        kind: "completion".to_owned(),
        task_id: Some("loom_4321".to_owned()),
        body: "loom#4321 · merged ✓ · PR #4321 · 12m30s".to_owned(),
        meta: Some(meta),
    };
    let req = build_send_request(&env, 3, Some("fleet")).unwrap();

    assert_eq!(req["type"], json!("completion"));
    assert_eq!(req["v"], json!(1));
    assert_eq!(req["room"], json!("fleet"));
    assert!(req.get("from").is_none(), "from must never be serialized");
    // The whole completion-v1 payload rides in `meta`, and `body` stays
    // human prose (a room reader sees a sentence, not JSON).
    assert_eq!(req["meta"]["schema"], json!("completion-v1"));
    assert_eq!(req["meta"]["agent"], json!("loom_daemon"));
    assert_eq!(req["meta"]["repo"], json!("rjwalters/loom"));
    assert_eq!(req["meta"]["ref"], json!("https://github.com/rjwalters/loom/pull/4321"));
    assert_eq!(req["meta"]["result"], json!("success"));
    assert_eq!(req["meta"]["started_at"], json!("2026-07-29T10:00:00Z"));
    assert_eq!(req["meta"]["completed_at"], json!("2026-07-29T10:12:30Z"));
    assert_eq!(req["meta"]["issue"], json!(4321));
    assert_eq!(req["meta"]["tokens"], json!(791_000));
    // Per-model breakdown (#5740) rides the same `meta`, additive
    // alongside `tokens`.
    assert_eq!(
        req["meta"]["tokens_by_model"],
        json!([{
            "model": "claude-sonnet-5",
            "speed": "standard",
            "service_tier": "standard",
            "input": 1_000,
            "cache_read": 700_000,
            "cache_write_5m": 1_000,
            "cache_write_1h": 89_000,
            "output": 30_000,
        }])
    );
    // Feed display fields (#4497) ride the same `meta`, so the egress
    // publishes them with no schema revision.
    assert_eq!(req["meta"]["title"], json!("Add repo-qualified task_id"));
    assert_eq!(req["meta"]["additions"], json!(214));
    assert_eq!(req["meta"]["deletions"], json!(37));
    assert!(req["body"].as_str().unwrap().contains("merged ✓"));
}

#[test]
fn send_request_refuses_completion_without_meta() {
    // safehoused would degrade this to a `chat` and it would vanish from
    // the public feed with no error — so it must never leave this client.
    let env = Envelope {
        to: "*".to_owned(),
        kind: "completion".to_owned(),
        task_id: None,
        body: "merged".to_owned(),
        meta: None,
    };
    assert!(build_send_request(&env, 1, None).is_err());
}

#[test]
fn send_request_refuses_meta_on_non_completion_types() {
    for kind in ["chat", "task", "handoff", "ack"] {
        let env = Envelope {
            to: "*".to_owned(),
            kind: kind.to_owned(),
            task_id: None,
            body: "x".to_owned(),
            meta: Some(sample_completion_meta().to_meta_value().unwrap()),
        };
        assert!(
            build_send_request(&env, 1, None).is_err(),
            "`meta` must be rejected on a {kind:?} envelope, not silently dropped"
        );
    }
}

#[test]
fn send_request_refuses_every_flavor_of_malformed_completion_meta() {
    let valid = sample_completion_meta().to_meta_value().unwrap();
    let mut cases: Vec<(&str, Value)> = vec![
        ("not an object", json!("completion-v1")),
        ("wrong schema", {
            let mut m = valid.clone();
            m["schema"] = json!("completion-v2");
            m
        }),
        ("empty agent", {
            let mut m = valid.clone();
            m["agent"] = json!("");
            m
        }),
        ("invalid persona charset", {
            let mut m = valid.clone();
            m["agent"] = json!("Loom-Daemon");
            m
        }),
        ("repo is a path basename, not a forge slug", {
            let mut m = valid.clone();
            m["repo"] = json!("loom");
            m
        }),
        ("ref is not an absolute URL", {
            let mut m = valid.clone();
            m["ref"] = json!("rjwalters/loom#4321");
            m
        }),
        ("unknown result", {
            let mut m = valid.clone();
            m["result"] = json!("merged");
            m
        }),
        ("started_at is not RFC3339", {
            let mut m = valid.clone();
            m["started_at"] = json!("29 July 2026");
            m
        }),
        ("completed_at precedes started_at", {
            let mut m = valid.clone();
            m["completed_at"] = json!("2026-07-29T09:00:00Z");
            m
        }),
        ("tokens is a string", {
            let mut m = valid.clone();
            m["tokens"] = json!("791000");
            m
        }),
        // #5740 per-model breakdown validation.
        ("tokens_by_model is not an array", {
            let mut m = valid.clone();
            m["tokens_by_model"] = json!({"model": "claude-sonnet-5"});
            m
        }),
        ("tokens_by_model is an empty array", {
            let mut m = valid.clone();
            m["tokens_by_model"] = json!([]);
            m
        }),
        ("tokens_by_model row is missing model", {
            let mut m = valid.clone();
            m["tokens_by_model"] = json!([{
                "speed": "standard", "service_tier": "standard",
                "input": 1, "cache_read": 1, "cache_write_5m": 1,
                "cache_write_1h": 1, "output": 1,
            }]);
            m
        }),
        ("tokens_by_model row has a blank model", {
            let mut m = valid.clone();
            m["tokens_by_model"] = json!([{
                "model": "  ", "speed": "standard", "service_tier": "standard",
                "input": 1, "cache_read": 1, "cache_write_5m": 1,
                "cache_write_1h": 1, "output": 1,
            }]);
            m
        }),
        ("tokens_by_model row has a negative counter", {
            let mut m = valid.clone();
            m["tokens_by_model"] = json!([{
                "model": "claude-sonnet-5", "speed": "standard", "service_tier": "standard",
                "input": -1, "cache_read": 1, "cache_write_5m": 1,
                "cache_write_1h": 1, "output": 1,
            }]);
            m
        }),
        // #4497 display fields are validated to the same standard as the
        // pre-existing extensions.
        ("additions is a string", {
            let mut m = valid.clone();
            m["additions"] = json!("214");
            m
        }),
        ("deletions is negative", {
            let mut m = valid.clone();
            m["deletions"] = json!(-1);
            m
        }),
        ("title is blank", {
            let mut m = valid.clone();
            m["title"] = json!("   ");
            m
        }),
        ("title is not a string", {
            let mut m = valid.clone();
            m["title"] = json!(4497);
            m
        }),
    ];
    // Every required key, dropped one at a time.
    for key in COMPLETION_REQUIRED_KEYS {
        let mut m = valid.clone();
        m.as_object_mut().unwrap().remove(key);
        cases.push((key, m));
    }

    for (label, meta) in cases {
        assert!(
            validate_completion_meta(&meta).is_err(),
            "validate_completion_meta must reject: {label}"
        );
        let env = Envelope {
            to: "*".to_owned(),
            kind: "completion".to_owned(),
            task_id: None,
            body: "merged".to_owned(),
            meta: Some(meta),
        };
        assert!(
            build_send_request(&env, 1, None).is_err(),
            "a completion with malformed meta must not be sent: {label}"
        );
    }
}

#[test]
fn completion_meta_omits_absent_optional_fields() {
    let meta = CompletionMeta {
        issue: None,
        tokens: None,
        tokens_by_model: None,
        title: None,
        additions: None,
        deletions: None,
        visibility: None,
        ..sample_completion_meta()
    }
    .to_meta_value()
    .unwrap();
    assert!(meta.get("issue").is_none(), "absent issue must be omitted, not null/0");
    assert!(meta.get("tokens").is_none(), "absent tokens must be omitted, not null/0");
    assert!(
        meta.get("tokens_by_model").is_none(),
        "absent tokens_by_model must be omitted, not null/[]"
    );
    assert!(meta.get("title").is_none(), "absent title must be omitted, not null/empty");
    assert!(meta.get("additions").is_none(), "absent additions must be omitted, not 0");
    assert!(meta.get("deletions").is_none(), "absent deletions must be omitted, not 0");
    assert!(
        meta.get("visibility").is_none(),
        "an undetermined visibility must be omitted, never guessed as public (#6596)"
    );
    // With every extension absent, the envelope is exactly the required
    // completion-v1 object — no new keys, so no new failure modes (#4497).
    assert_eq!(
        meta.as_object().unwrap().len(),
        COMPLETION_REQUIRED_KEYS.len(),
        "all-absent extensions ⇒ meta identical to the required-keys-only envelope; got {meta}"
    );
    // A zero token count is indistinguishable from "no accounting data".
    let zeroed = CompletionMeta {
        tokens: Some(0),
        ..sample_completion_meta()
    }
    .to_meta_value()
    .unwrap();
    assert!(zeroed.get("tokens").is_none());
    // An empty `tokens_by_model` vec is likewise "no data", not a real
    // (vacuous) breakdown — omitted rather than published as `[]`.
    let empty_breakdown = CompletionMeta {
        tokens_by_model: Some(Vec::new()),
        ..sample_completion_meta()
    }
    .to_meta_value()
    .unwrap();
    assert!(empty_breakdown.get("tokens_by_model").is_none());
    // A blank/whitespace title would render as an empty feed row label.
    for blank in ["", "   ", "\n\t"] {
        let meta = CompletionMeta {
            title: Some(blank.to_owned()),
            ..sample_completion_meta()
        }
        .to_meta_value()
        .unwrap();
        assert!(meta.get("title").is_none(), "blank title must be omitted: {blank:?}");
    }
}

#[test]
fn completion_meta_publishes_zero_diff_counts_and_trims_the_title() {
    // Unlike `tokens`, `0` additions/deletions is a *fact* about the merge
    // (a pure revert, an empty-diff merge commit), not a "no data" sentinel
    // — so it is published rather than filtered (#4497).
    let meta = CompletionMeta {
        additions: Some(0),
        deletions: Some(0),
        title: Some("  fix: trim me  ".to_owned()),
        ..sample_completion_meta()
    }
    .to_meta_value()
    .unwrap();
    assert_eq!(meta["additions"], json!(0));
    assert_eq!(meta["deletions"], json!(0));
    assert_eq!(meta["title"], json!("fix: trim me"));
}

#[test]
fn completion_meta_carries_the_title_verbatim_for_downstream_redaction() {
    // Deny-pattern redaction is safehoused's egress job (it redacts every
    // string in the published payload), not loom's — so the contract loom
    // owns is that `title` reaches the wire as an ordinary JSON string in
    // `meta`, exactly like `repo`/`ref`, with no escaping or bespoke
    // encoding that would let it bypass that pass (#4497 AC3).
    let secretish = "fix: rotate ghp_EXAMPLETOKEN0123456789 in \"prod\"\\config";
    let env = build_completion_envelope(
        Some("/x/loom"),
        4497,
        4500,
        60,
        &CompletionMeta {
            title: Some(secretish.to_owned()),
            ..sample_completion_meta()
        },
    )
    .unwrap();
    let req = build_send_request(&env, 1, Some("fleet")).unwrap();
    assert_eq!(
        req["meta"]["title"],
        json!(secretish),
        "title must be a plain JSON string in meta, like every other redactable field"
    );
    // And it survives a JSON round-trip through the wire encoding intact —
    // the shape safehoused's redactor walks.
    let line = serde_json::to_string(&req).unwrap();
    let parsed: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(parsed["meta"]["title"], json!(secretish));
    assert!(parsed["meta"]["title"].is_string());
}

#[test]
fn completion_meta_construction_fails_on_bad_fields() {
    // The typed constructor is the only route to a `completion`, so it
    // must refuse the same things validate_completion_meta does.
    assert!(CompletionMeta {
        repo_slug: "not a slug".to_owned(),
        ..sample_completion_meta()
    }
    .to_meta_value()
    .is_err());
    assert!(CompletionMeta {
        started_at: "yesterday".to_owned(),
        ..sample_completion_meta()
    }
    .to_meta_value()
    .is_err());
}

#[test]
fn build_completion_envelope_threads_with_the_issue_and_reads_as_prose() {
    let env = build_completion_envelope(
        Some("/Users/x/GitHub/loom"),
        4321,
        4400,
        750,
        &sample_completion_meta(),
    )
    .unwrap();

    assert_eq!(env.kind, "completion");
    assert_eq!(env.to, "*");
    // Same repo-qualified thread key as this issue's other narration lines
    // (#4201), so the completion lands in the existing Matrix thread.
    assert_eq!(env.task_id.as_deref(), Some("loom_4321"));
    assert_eq!(env.body, "loom#4321 · merged ✓ · PR #4400 · 12m30s");
    assert_eq!(env.meta.as_ref().unwrap()["schema"], json!("completion-v1"));
    // And it survives the pre-send gate.
    assert!(build_send_request(&env, 1, None).is_ok());
}

#[test]
fn valid_repo_slug_accepts_owner_repo_and_rejects_the_rest() {
    assert!(valid_repo_slug("rjwalters/loom"));
    assert!(valid_repo_slug("2AMLogic/marketing"));
    assert!(valid_repo_slug("owner/kicad-tools.git"));
    assert!(!valid_repo_slug("loom"), "a bare basename is not a forge slug");
    assert!(!valid_repo_slug("a/b/c"));
    assert!(!valid_repo_slug("/loom"));
    assert!(!valid_repo_slug("rjwalters/"));
    assert!(!valid_repo_slug("rjwalters/lo om"));
}

// ---- repo qualification helpers (issue #4201) ----

#[test]
fn repo_basename_extracts_final_path_segment() {
    assert_eq!(repo_basename(Some("/Users/x/GitHub/vibesql")).as_deref(), Some("vibesql"));
    assert_eq!(repo_basename(Some("/repos/kicad-tools")).as_deref(), Some("kicad-tools"));
    assert_eq!(repo_basename(None), None);
    assert_eq!(repo_basename(Some("")), None);
}

#[test]
fn qualify_task_id_sanitizes_and_qualifies() {
    assert_eq!(qualify_task_id(Some("/repos/vibesql"), 6173), "vibesql_6173");
    // Non-alphanumeric basename characters (hyphen) fold to `_` so the
    // result stays in the task_id charset validated by build_send_request.
    assert_eq!(qualify_task_id(Some("/repos/kicad-tools"), 9), "kicad_tools_9");
    // No repo known ⇒ bare issue number (pre-#4201 behavior preserved).
    assert_eq!(qualify_task_id(None, 42), "42");
}

#[test]
fn cross_repo_same_issue_number_gets_distinct_task_ids() {
    // The bug this issue fixes: loom #4201 and vibesql #4201 must not
    // collide into the same Matrix thread.
    let loom_id = qualify_task_id(Some("/Users/x/GitHub/loom"), 4201);
    let vibesql_id = qualify_task_id(Some("/Users/x/GitHub/vibesql"), 4201);
    assert_ne!(loom_id, vibesql_id);
    assert_eq!(loom_id, "loom_4201");
    assert_eq!(vibesql_id, "vibesql_4201");
}

#[test]
fn repo_issue_prefix_falls_back_without_repo() {
    assert_eq!(repo_issue_prefix(Some("/repos/vibesql"), 6173), "vibesql#6173");
    assert_eq!(repo_issue_prefix(None, 42), "#42");
}

// ---- dispatch-digest batching (issue #4217) ----

#[test]
fn dispatch_envelope_matches_the_pre_4217_single_dispatch_shape() {
    let env = dispatch_envelope(Some("/repos/vibesql"), 42);
    assert_eq!(env.kind, "task");
    assert_eq!(env.task_id.as_deref(), Some("vibesql_42"));
    assert_eq!(env.body, "vibesql#42 · dispatch");
}

/// The example from the issue body, byte-for-byte: `dispatched 7: loom×6
/// (#4028 #4106 #4144 #4157 #4162 #4164), vibesql×1 (#6173)` — groups sort
/// by descending count (loom's 6 before vibesql's 1), issue numbers within
/// a group sort ascending.
#[test]
fn build_dispatch_digest_envelope_groups_by_repo_and_sorts() {
    let batch: Vec<PendingDispatch> = [
        (Some("/Users/x/GitHub/loom"), 4164),
        (Some("/Users/x/GitHub/loom"), 4028),
        (Some("/Users/x/GitHub/loom"), 4157),
        (Some("/Users/x/GitHub/vibesql"), 6173),
        (Some("/Users/x/GitHub/loom"), 4106),
        (Some("/Users/x/GitHub/loom"), 4162),
        (Some("/Users/x/GitHub/loom"), 4144),
    ]
    .into_iter()
    .map(|(repo, issue)| PendingDispatch {
        repo: repo.map(str::to_owned),
        issue,
    })
    .collect();

    let env = build_dispatch_digest_envelope(&batch, 1);

    assert_eq!(env.kind, "digest");
    assert_eq!(env.task_id.as_deref(), Some("dispatch_digest_1"));
    assert_eq!(
        env.body,
        "dispatched 7: loom×6 (#4028 #4106 #4144 #4157 #4162 #4164), vibesql×1 (#6173)"
    );
}

/// Each flushed batch gets a distinct `task_id` (a new root), not one
/// perpetual thread every dispatch wave piles onto.
#[test]
fn build_dispatch_digest_envelope_task_id_is_unique_per_batch() {
    let batch = vec![
        PendingDispatch {
            repo: Some("/repos/loom".to_owned()),
            issue: 1,
        },
        PendingDispatch {
            repo: Some("/repos/loom".to_owned()),
            issue: 2,
        },
    ];
    let first = build_dispatch_digest_envelope(&batch, 1);
    let second = build_dispatch_digest_envelope(&batch, 2);
    assert_ne!(first.task_id, second.task_id);
}

/// A dispatch with no `repo` stamped (synthetic/test event) still narrates
/// in a digest — grouped under a fallback label rather than panicking or
/// silently dropping the line.
#[test]
fn build_dispatch_digest_envelope_falls_back_for_missing_repo() {
    let batch = vec![
        PendingDispatch {
            repo: None,
            issue: 10,
        },
        PendingDispatch {
            repo: None,
            issue: 11,
        },
    ];
    let env = build_dispatch_digest_envelope(&batch, 1);
    assert_eq!(env.body, "dispatched 2: unscoped×2 (#10 #11)");
}

/// Mirrors `completion_routes_to_the_signal_room` (#4225): the digest kind
/// must resolve to the signal room even when the (irrelevant, since
/// `Signal` ignores `repo`) repo has its own firehose configured.
#[test]
fn digest_routes_to_the_signal_room() {
    assert_eq!(EnvelopeKind::Digest.attention_class(), AttentionClass::Signal);

    let cfg = routing_config();
    let router = RoomRouter::new(&cfg);
    assert_eq!(
        router.resolve("digest", Some("/home/x/GitHub/loom")),
        RoomDecision::Send(Some("!signal:example.org".to_owned())),
        "a digest must reach the signal room even when the repo has its own firehose"
    );
    assert_eq!(
        router.resolve("digest", None),
        RoomDecision::Send(Some("!signal:example.org".to_owned())),
        "a digest (inherently cross-repo) must not depend on a repo being stamped"
    );
}

#[test]
fn format_narrated_duration_drops_zero_minutes() {
    assert_eq!(format_narrated_duration(415), "6m55s");
    assert_eq!(format_narrated_duration(24), "24s");
    assert_eq!(format_narrated_duration(0), "0s");
    assert_eq!(format_narrated_duration(60), "1m0s");
}

#[test]
fn decode_exit_code_annotates_only_well_known_codes() {
    assert_eq!(decode_exit_code_annotation(78), " (EX_CONFIG: token pool)");
    assert_eq!(decode_exit_code_annotation(1), "");
    assert_eq!(decode_exit_code_annotation(0), "");
}

// ---- event → envelope mapping ----

#[test]
fn maps_the_narrated_events() {
    let dispatch = Event::SweepGlobalDispatch {
        sweep_id: "sweep-issue-42-1".to_owned() as SweepId,
        kind: SweepKind::Issue(42),
        runtime: None,
        runtime_source: None,
        repo: Some("/repos/vibesql".to_owned()),
    };
    let env = event_to_envelope(&dispatch).unwrap();
    assert_eq!(env.kind, "task");
    assert_eq!(env.task_id.as_deref(), Some("vibesql_42"));
    assert_eq!(env.body, "vibesql#42 · dispatch");

    let phase = Event::SweepPhase {
        issue: 42,
        phase: "builder".to_owned(),
        pr_number: Some(99),
        repo: Some("/repos/vibesql".to_owned()),
    };
    let env = event_to_envelope(&phase).unwrap();
    assert_eq!(env.kind, "task");
    assert_eq!(env.task_id.as_deref(), Some("vibesql_42"));
    assert!(env.body.starts_with("vibesql#42 · builder"));
    assert!(env.body.contains("PR #99 open"));

    let blocker = Event::SweepBlocker {
        issue: 42,
        reason: "missing dep".to_owned(),
        label_added: "loom:blocked".to_owned(),
        repo: Some("/repos/vibesql".to_owned()),
    };
    let env = event_to_envelope(&blocker).unwrap();
    assert_eq!(env.kind, "handoff");
    assert_eq!(env.body, "vibesql#42 · BLOCKED — missing dep");

    let exited = Event::SweepExited {
        issue: 42,
        exit_code: Some(0),
        duration_sec: 12,
        no_progress: false,
        death_class: None,
        repo: Some("/repos/vibesql".to_owned()),
    };
    let env = event_to_envelope(&exited).unwrap();
    assert_eq!(env.kind, "ack");
    assert_eq!(env.body, "vibesql#42 · done ✓ · 12s");

    // A non-zero exit decodes its well-known meaning (78 = EX_CONFIG).
    let exited_failed = Event::SweepExited {
        issue: 42,
        exit_code: Some(78),
        duration_sec: 24,
        no_progress: false,
        death_class: None,
        repo: Some("/repos/vibesql".to_owned()),
    };
    let env = event_to_envelope(&exited_failed).unwrap();
    assert_eq!(env.body, "vibesql#42 · failed ✗ · exit 78 (EX_CONFIG: token pool) · 24s");

    // #4366: a clean exit 0 classified as no-progress (parked on a
    // monitored background task) narrates distinctly from an ordinary
    // benign self-skip.
    let exited_no_progress = Event::SweepExited {
        issue: 42,
        exit_code: Some(0),
        duration_sec: 90,
        no_progress: true,
        death_class: None,
        repo: Some("/repos/vibesql".to_owned()),
    };
    let env = event_to_envelope(&exited_no_progress).unwrap();
    assert_eq!(env.kind, "ack");
    assert_eq!(env.body, "vibesql#42 · no progress ⚠ · exit 0, no checkpoint/PR · 1m30s");

    let crashed = Event::SweepCrashed {
        issue: 42,
        checkpoint_phase: Some("judge".to_owned()),
        classification: None,
        death_class: None,
        repo: Some("/repos/vibesql".to_owned()),
    };
    let env = event_to_envelope(&crashed).unwrap();
    assert_eq!(env.kind, "handoff");
    assert_eq!(env.body, "vibesql#42 · crashed ✗ at judge — resumable (checkpoint kept)");

    // Issue #4256: a successful reaper-driven resume narrates as a
    // `handoff` naming the phase + PR, so an operator watching chat sees
    // recovery happen without having to run `/loom:sweep --prs` by hand.
    let resumed = Event::SweepResumeDispatched {
        issue: 42,
        pr: 4300,
        checkpoint_phase: Some("builder-done".to_owned()),
        dispatched: true,
        repo: Some("/repos/vibesql".to_owned()),
    };
    let env = event_to_envelope(&resumed).unwrap();
    assert_eq!(env.kind, "handoff");
    assert_eq!(
        env.body,
        "vibesql#42 · reaper resumed crashed sweep at builder-done (open PR #4300) — \
             resuming without operator intervention"
    );

    // A resume ATTEMPT whose own dispatch call failed still narrates —
    // the recovery attempt must never be silent even on failure.
    let resume_failed = Event::SweepResumeDispatched {
        issue: 42,
        pr: 4300,
        checkpoint_phase: Some("builder-done".to_owned()),
        dispatched: false,
        repo: Some("/repos/vibesql".to_owned()),
    };
    let env = event_to_envelope(&resume_failed).unwrap();
    assert!(env.body.contains("still stranded"), "got: {}", env.body);

    let idle_exit = Event::DaemonIdleExit {
        trigger: "token_starvation".to_owned(),
        idle_minutes: 60,
        in_flight_sweeps: 0,
        active_role_runs: 1,
        healthy_tokens: 0,
        total_tokens: 8,
        message: "idle for 60m — exiting for host idle-shutdown".to_owned(),
    };
    let env = event_to_envelope(&idle_exit).unwrap();
    assert_eq!(env.kind, "handoff");
    assert_eq!(env.task_id.as_deref(), Some("daemon-idle-exit"));
    assert!(env.body.contains("host idle-shutdown"));
    assert_eq!(env.meta.unwrap()["trigger"], "token_starvation");
}

#[test]
fn maps_events_without_repo_using_bare_fallback() {
    // No `repo` stamped (a pre-#4201 event, or a registry that never
    // wires the bus) still narrates — just without repo qualification,
    // matching the pre-#4201 behavior for task_id and body prefix.
    let dispatch = Event::SweepGlobalDispatch {
        sweep_id: "sweep-issue-42-1".to_owned() as SweepId,
        kind: SweepKind::Issue(42),
        runtime: None,
        runtime_source: None,
        repo: None,
    };
    let env = event_to_envelope(&dispatch).unwrap();
    assert_eq!(env.task_id.as_deref(), Some("42"));
    assert_eq!(env.body, "#42 · dispatch");
}

#[test]
fn does_not_narrate_global_completed_or_generic() {
    let completed = Event::SweepGlobalCompleted {
        sweep_id: "sweep-issue-42-1".to_owned() as SweepId,
        outcome: crate::types::SweepOutcome::Exited,
    };
    assert!(event_to_envelope(&completed).is_none());
    assert!(event_to_envelope(&Event::TopicLag { skipped: 3 }).is_none());
}

#[test]
fn narrates_child_published_phase_and_blocker_after_typed_upgrade() {
    // Issue #4466: a child-published `sweep.issue.{N}.*` event arrives via
    // `PublishEvent`; before the typed-upgrade fix it became `Event::Generic`
    // and `event_to_envelope` returned `None` (silently dropped). Drive the
    // exact `PublishEvent` construction (`Event::from_published`) end to end
    // and assert the documented room lines now appear.
    let phase = Event::from_published(
        "sweep.issue.42.phase".to_owned(),
        json!({"phase": "builder", "pr_number": 99, "repo": "/repos/vibesql"}),
    );
    let env = event_to_envelope(&phase).expect("phase must narrate (was dropped as Generic)");
    assert_eq!(env.kind, "task");
    assert_eq!(env.task_id.as_deref(), Some("vibesql_42"));
    assert!(env.body.starts_with("vibesql#42 · builder"));
    assert!(env.body.contains("PR #99 open"));

    let blocker = Event::from_published(
        "sweep.issue.42.blocker".to_owned(),
        json!({"reason": "missing dep", "label_added": "loom:blocked", "repo": "/repos/vibesql"}),
    );
    let env = event_to_envelope(&blocker).expect("blocker must narrate (was dropped as Generic)");
    assert_eq!(env.kind, "handoff");
    assert_eq!(env.body, "vibesql#42 · BLOCKED — missing dep");

    // A malformed child payload still falls through to Generic and stays
    // un-narrated (publish is fire-and-forget advisory — never rejected).
    let malformed =
        Event::from_published("sweep.issue.42.phase".to_owned(), json!({"pr_number": 1}));
    assert!(matches!(malformed, Event::Generic { .. }));
    assert!(event_to_envelope(&malformed).is_none());
}

// ---- dispatch-title fetch (issue #4201, sink-side gh lookup) ----

/// Write an executable fake `gh` script at `dir/fake-gh.sh` that logs its
/// argv to `dir/gh-invocations.log` (one line per call) and prints `stdout`
/// on success, or exits 1 when `stdout` is `None` — mirrors the fake-`gh`
/// convention already used in `sweep_registry.rs`'s tests.
fn write_fake_gh(dir: &std::path::Path, stdout: Option<&str>) -> (PathBuf, PathBuf) {
    let log = dir.join("gh-invocations.log");
    let script_path = dir.join("fake-gh.sh");
    let body = match stdout {
        Some(text) => format!(
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >> \"{}\"\nprintf '%s\\n' {}\nexit 0\n",
            log.display(),
            shell_quote(text),
        ),
        None => {
            format!("#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >> \"{}\"\nexit 1\n", log.display())
        }
    };
    std::fs::write(&script_path, body).unwrap();
    let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script_path, perms).unwrap();
    (script_path, log)
}

/// Minimal single-quote shell escaping sufficient for test title strings.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[tokio::test]
#[serial]
async fn fetch_issue_title_returns_title_on_success() {
    let dir = tempfile::tempdir().unwrap();
    let (fake_gh, _log) = write_fake_gh(dir.path(), Some("Fix the frobnicator"));
    std::env::set_var(GH_BIN_ENV, &fake_gh);

    let title = fetch_issue_title(dir.path(), 42).await;

    std::env::remove_var(GH_BIN_ENV);
    assert_eq!(title.as_deref(), Some("Fix the frobnicator"));
}

#[tokio::test]
#[serial]
async fn fetch_issue_title_degrades_to_none_on_gh_failure() {
    let dir = tempfile::tempdir().unwrap();
    let (fake_gh, _log) = write_fake_gh(dir.path(), None);
    std::env::set_var(GH_BIN_ENV, &fake_gh);

    let title = fetch_issue_title(dir.path(), 42).await;

    std::env::remove_var(GH_BIN_ENV);
    assert!(title.is_none(), "a failing gh call must degrade to None, not panic/hang");
}

#[tokio::test]
#[serial]
async fn fetch_title_cached_reuses_cache_within_ttl() {
    let dir = tempfile::tempdir().unwrap();
    let (fake_gh, log) = write_fake_gh(dir.path(), Some("Cached title"));
    std::env::set_var(GH_BIN_ENV, &fake_gh);

    let mut cache: HashMap<(String, u32), (String, Instant)> = HashMap::new();
    let root = dir.path().to_string_lossy().into_owned();
    let first = fetch_title_cached(&mut cache, &root, 7).await;
    let second = fetch_title_cached(&mut cache, &root, 7).await;

    std::env::remove_var(GH_BIN_ENV);
    assert_eq!(first.as_deref(), Some("Cached title"));
    assert_eq!(second.as_deref(), Some("Cached title"));
    let calls = std::fs::read_to_string(&log).unwrap_or_default();
    assert_eq!(
        calls.lines().count(),
        1,
        "second lookup within the TTL must reuse the cache, not re-shell to gh; log: {calls:?}"
    );
}

#[tokio::test]
#[serial]
async fn run_sink_enriches_dispatch_body_with_title() {
    let dir = tempfile::tempdir().unwrap();
    let _safehouse_test_paths = SafehouseTestPaths::set(dir.path());
    let (fake_gh, _log) = write_fake_gh(dir.path(), Some("Add repo-qualified task_id"));
    std::env::set_var(GH_BIN_ENV, &fake_gh);
    // A single dispatch still flushes at the end of the digest window
    // (issue #4217) — shrink it so this test does not wait out the 30s
    // production default.
    std::env::set_var(DISPATCH_DIGEST_WINDOW_ENV, "10");

    let socket = dir.path().join("safehoused.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(stub_server(listener, false, 1));

    let bus = Arc::new(EventBus::new());
    let subscription = bus.subscribe(Vec::<String>::new());
    let sink = tokio::spawn(run_sink(
        SafehouseConfig {
            enabled: true,
            socket: Some(socket.clone()),
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

    bus.publish(Event::SweepGlobalDispatch {
        sweep_id: "sweep-issue-4201-1".to_owned() as SweepId,
        kind: SweepKind::Issue(4201),
        runtime: None,
        runtime_source: None,
        repo: Some(dir.path().to_string_lossy().into_owned()),
    })
    .unwrap();

    let received = tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("stub server must receive the enriched dispatch send")
        .unwrap();

    drop(bus);
    let _ = tokio::time::timeout(Duration::from_secs(2), sink).await;
    std::env::remove_var(GH_BIN_ENV);
    std::env::remove_var(DISPATCH_DIGEST_WINDOW_ENV);

    assert_eq!(received.len(), 1);
    let body = received[0]["body"].as_str().unwrap();
    assert!(
        body.contains("Add repo-qualified task_id"),
        "dispatch body must be enriched with the fetched title; got: {body:?}"
    );
    assert!(body.contains("#4201 · dispatch"));
}

/// The motivating scenario (issue #4217): a work-finder tick admits
/// several issues in a burst. Instead of N near-identical `task` roots,
/// the sink emits **one** `digest` root, grouped per repo, once the
/// window closes — no per-issue `task` send is ever made for these four.
#[tokio::test]
#[serial]
async fn run_sink_batches_a_dispatch_burst_into_one_digest_root() {
    let dir = tempfile::tempdir().unwrap();
    let _safehouse_test_paths = SafehouseTestPaths::set(dir.path());
    std::env::set_var(DISPATCH_DIGEST_WINDOW_ENV, "50");

    let socket = dir.path().join("safehoused.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    // Exactly one send is expected — the digest, never one per issue.
    let server = tokio::spawn(stub_server(listener, false, 1));

    let bus = Arc::new(EventBus::new());
    let subscription = bus.subscribe(Vec::<String>::new());
    let sink = tokio::spawn(run_sink(
        SafehouseConfig {
            enabled: true,
            socket: Some(socket.clone()),
            room: Some("loom-fleet".to_owned()),
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

    for issue in [4028u32, 4106, 4144] {
        bus.publish(Event::SweepGlobalDispatch {
            sweep_id: format!("sweep-issue-{issue}-1") as SweepId,
            kind: SweepKind::Issue(issue),
            runtime: None,
            runtime_source: None,
            repo: Some("/Users/x/GitHub/loom".to_owned()),
        })
        .unwrap();
    }
    bus.publish(Event::SweepGlobalDispatch {
        sweep_id: "sweep-issue-6173-1".to_owned() as SweepId,
        kind: SweepKind::Issue(6173),
        runtime: None,
        runtime_source: None,
        repo: Some("/Users/x/GitHub/vibesql".to_owned()),
    })
    .unwrap();

    let received = tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("stub server must receive exactly one digest send")
        .unwrap();

    drop(bus);
    let _ = tokio::time::timeout(Duration::from_secs(2), sink).await;
    std::env::remove_var(DISPATCH_DIGEST_WINDOW_ENV);

    assert_eq!(received.len(), 1);
    assert_eq!(received[0]["type"], json!("digest"));
    assert_eq!(
        received[0]["body"],
        json!("dispatched 4: loom×3 (#4028 #4106 #4144), vibesql×1 (#6173)")
    );
}

/// A burst's per-issue `task_id` threads are untouched by the digest: once
/// the burst is narrated, the *next* real per-issue event (e.g. a `phase`
/// transition) still starts/continues that issue's own thread with the
/// #4201 grammar — the digest never substitutes for it.
#[tokio::test]
#[serial]
async fn run_sink_still_narrates_per_issue_events_after_a_digest() {
    let dir = tempfile::tempdir().unwrap();
    let _safehouse_test_paths = SafehouseTestPaths::set(dir.path());
    std::env::set_var(DISPATCH_DIGEST_WINDOW_ENV, "50");

    let socket = dir.path().join("safehoused.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    // 1: the digest. 2: the phase transition for issue 4028.
    let server = tokio::spawn(stub_server(listener, false, 2));

    let bus = Arc::new(EventBus::new());
    let subscription = bus.subscribe(Vec::<String>::new());
    let sink = tokio::spawn(run_sink(
        SafehouseConfig {
            enabled: true,
            socket: Some(socket.clone()),
            room: Some("loom-fleet".to_owned()),
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

    for issue in [4028u32, 4106] {
        bus.publish(Event::SweepGlobalDispatch {
            sweep_id: format!("sweep-issue-{issue}-1") as SweepId,
            kind: SweepKind::Issue(issue),
            runtime: None,
            runtime_source: None,
            repo: Some("/Users/x/GitHub/loom".to_owned()),
        })
        .unwrap();
    }
    // Give the digest window time to close before the phase transition,
    // so the two sends are unambiguously ordered for the assertions below.
    tokio::time::sleep(Duration::from_millis(150)).await;
    bus.publish(Event::SweepPhase {
        issue: 4028,
        phase: "builder".to_owned(),
        pr_number: None,
        repo: Some("/Users/x/GitHub/loom".to_owned()),
    })
    .unwrap();

    let received = tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("stub server must receive the digest then the phase send")
        .unwrap();

    drop(bus);
    let _ = tokio::time::timeout(Duration::from_secs(2), sink).await;
    std::env::remove_var(DISPATCH_DIGEST_WINDOW_ENV);

    assert_eq!(received.len(), 2);
    assert_eq!(received[0]["type"], json!("digest"));
    assert_eq!(received[1]["type"], json!("task"));
    assert_eq!(received[1]["task_id"], json!("loom_4028"));
    assert!(received[1]["body"]
        .as_str()
        .unwrap()
        .starts_with("loom#4028 · builder"));
}

// ---- run_sink room routing end-to-end (#4225) ----

/// End-to-end through the sink and a stub safehoused: a `task` (phase) line
/// lazily creates and lands in the repo firehose room, while a `handoff`
/// (blocker) line from the *same* repo lands in the signal room — one room per
/// message, chosen by severity.
#[tokio::test]
#[serial]
async fn run_sink_routes_by_attention_class_and_creates_the_repo_room_lazily() {
    let dir = tempfile::tempdir().unwrap();
    let _safehouse_test_paths = SafehouseTestPaths::set(dir.path());
    let repo = dir.path().to_string_lossy().into_owned();
    let repo_name = dir
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let alias = format!("fleet-{repo_name}");

    let socket = dir.path().join("safehoused.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    // 3 requests: create_room, the routed task send, the routed handoff send.
    let server = tokio::spawn(stub_server(listener, false, 3));

    let bus = Arc::new(EventBus::new());
    let subscription = bus.subscribe(Vec::<String>::new());
    let sink = tokio::spawn(run_sink(
        SafehouseConfig {
            enabled: true,
            socket: Some(socket.clone()),
            room: None,
            rooms: Some(RoomMap {
                signal: Some("!signal:example.org".to_owned()),
                by_repo: std::collections::BTreeMap::new(),
                claims: None,
            }),
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

    bus.publish(Event::SweepPhase {
        issue: 4225,
        phase: "builder".to_owned(),
        pr_number: None,
        repo: Some(repo.clone()),
    })
    .unwrap();
    bus.publish(Event::SweepBlocker {
        issue: 4225,
        reason: "needs a human".to_owned(),
        label_added: "loom:blocked".to_owned(),
        repo: Some(repo),
    })
    .unwrap();

    let received = tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("stub server must receive the create_room + both routed sends")
        .unwrap();
    drop(bus);
    let _ = tokio::time::timeout(Duration::from_secs(2), sink).await;

    assert_eq!(received.len(), 3);
    // 1. The firehose room is created lazily, on this repo's first narration.
    assert_eq!(received[0]["op"], json!("create_room"));
    assert_eq!(received[0]["name"], json!(alias));
    // 2. `task` (dispatch/phase chatter) → the repo firehose room.
    assert_eq!(received[1]["op"], json!("send"));
    assert_eq!(received[1]["type"], json!("task"));
    assert_eq!(received[1]["room"], json!(alias));
    // 3. `handoff` (a human must act) → the signal room, same repo.
    assert_eq!(received[2]["type"], json!("handoff"));
    assert_eq!(received[2]["room"], json!("!signal:example.org"));
}

/// The migration default: with **no** `rooms` map the wire shape is exactly
/// the pre-#4225 one — one send per event addressed at the single configured
/// room, and no `create_room` op ever.
#[tokio::test]
#[serial]
async fn run_sink_without_a_rooms_map_keeps_the_single_room_wire_shape() {
    let dir = tempfile::tempdir().unwrap();
    let _safehouse_test_paths = SafehouseTestPaths::set(dir.path());
    let socket = dir.path().join("safehoused.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(stub_server(listener, false, 1));

    let bus = Arc::new(EventBus::new());
    let subscription = bus.subscribe(Vec::<String>::new());
    let state = new_shared_state();
    let sink = tokio::spawn(run_sink(
        SafehouseConfig {
            enabled: true,
            socket: Some(socket.clone()),
            room: Some("loom-fleet".to_owned()),
            rooms: None, // ← the migration default
            ..SafehouseConfig::default()
        },
        socket.clone(),
        subscription,
        Duration::from_millis(20),
        Duration::from_millis(80),
        state.clone(),
        None,
        None,
    ));

    // A `task`-class event, which routing mode would have sent to a firehose.
    bus.publish(Event::SweepPhase {
        issue: 4225,
        phase: "builder".to_owned(),
        pr_number: None,
        repo: Some(dir.path().to_string_lossy().into_owned()),
    })
    .unwrap();

    let received = tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("stub server must receive the single-room send")
        .unwrap();
    drop(bus);
    let _ = tokio::time::timeout(Duration::from_secs(2), sink).await;

    assert_eq!(received.len(), 1, "no create_room op, no extra sends");
    assert_eq!(received[0]["op"], json!("send"));
    assert_eq!(received[0]["type"], json!("task"));
    assert_eq!(received[0]["room"], json!("loom-fleet"));
    // …and `status` still reports the same room it always did.
    assert_eq!(
        snapshot_state(&state),
        SafehouseState::Connected {
            socket,
            room: Some("loom-fleet".to_owned()),
        }
    );
}

// ---- completion emit point: SweepExited + forge merge check (#4426) ----

/// Write an executable fake `gh` that answers the two forge lookups the
/// completion path makes — `gh pr list …` and `gh repo view …` — logging
/// every invocation. `None` for either makes that subcommand exit 1, which
/// is how a missing/unauthenticated/offline `gh` presents. The `repo view`
/// answer is the **public**-repo shape (#6596); use
/// [`write_fake_forge_gh_with_repo_view`] for anything else.
fn write_fake_forge_gh(
    dir: &std::path::Path,
    pr_list_json: Option<&str>,
    slug: Option<&str>,
) -> (PathBuf, PathBuf) {
    let repo_view = slug.map(|s| repo_view_json(s, Some(false)));
    write_fake_forge_gh_with_repo_view(dir, pr_list_json, repo_view.as_deref())
}

/// One `gh repo view --json nameWithOwner,isPrivate` response body.
/// `is_private: None` omits the field entirely — the shape a `gh` too old
/// to know it returns from the fallback query (#6596).
fn repo_view_json(slug: &str, is_private: Option<bool>) -> String {
    match is_private {
        Some(private) => format!(r#"{{"nameWithOwner":"{slug}","isPrivate":{private}}}"#),
        None => format!(r#"{{"nameWithOwner":"{slug}"}}"#),
    }
}

/// [`write_fake_forge_gh`] with the raw `gh repo view` stdout supplied, so
/// a test can model a private repo or a response with no `isPrivate` field.
fn write_fake_forge_gh_with_repo_view(
    dir: &std::path::Path,
    pr_list_json: Option<&str>,
    repo_view_json: Option<&str>,
) -> (PathBuf, PathBuf) {
    let log = dir.join("gh-forge-invocations.log");
    let script_path = dir.join("fake-forge-gh.sh");
    let arm = |stdout: Option<&str>| match stdout {
        Some(text) => format!("printf '%s\\n' {}; exit 0", shell_quote(text)),
        None => "exit 1".to_owned(),
    };
    let body = format!(
        "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >> \"{}\"\ncase \"$1 $2\" in\n  \
             'pr list') {} ;;\n  'repo view') {} ;;\n  *) exit 1 ;;\nesac\n",
        log.display(),
        arm(pr_list_json),
        arm(repo_view_json),
    );
    std::fs::write(&script_path, body).unwrap();
    let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script_path, perms).unwrap();
    (script_path, log)
}

/// A `gh pr list` row carrying everything the enriched query asks for: the
/// load-bearing merge facts plus the #4497 feed display fields.
const MERGED_PR_JSON: &str = r#"[{"number":4400,"url":"https://github.com/rjwalters/loom/pull/4400","mergedAt":"2026-07-29T10:12:00Z","title":"feat: enrich completion meta","additions":214,"deletions":37}]"#;

/// The pre-#4497 row shape: merge facts only. Stands in for a forge/`gh`
/// that answers the query but returns none of the display fields — the
/// completion must still publish, minus those keys.
const MERGED_PR_JSON_NO_DISPLAY_FIELDS: &str = r#"[{"number":4400,"url":"https://github.com/rjwalters/loom/pull/4400","mergedAt":"2026-07-29T10:12:00Z"}]"#;

/// Build an activity DB under `dir` carrying exactly one per-issue token
/// rollup for `issue`, wired the way [`fetch_issue_tokens`]'s query reads it:
/// a recorded input, a forge event linking that input to the issue, and a
/// resource-usage sample linked to the same input.
fn seed_activity_db_with_issue_tokens(
    dir: &std::path::Path,
    issue: i32,
    tokens_input: i64,
    tokens_output: i64,
) -> Arc<Mutex<ActivityDb>> {
    use crate::activity::{
        AgentInput, InputContext, InputType, PromptForgeEvent, PromptForgeEventType,
    };

    let db = ActivityDb::new(dir.join("activity.db")).unwrap();
    let input_id = db
        .record_input(&AgentInput {
            id: None,
            terminal_id: "loom-builder-1".to_owned(),
            timestamp: Utc::now(),
            input_type: InputType::Autonomous,
            content: "/loom:builder".to_owned(),
            agent_role: Some("builder".to_owned()),
            context: InputContext::default(),
        })
        .unwrap();
    db.record_prompt_forge_event(&PromptForgeEvent {
        id: None,
        input_id: Some(input_id),
        issue_number: Some(issue),
        pr_number: None,
        label_before: None,
        label_after: None,
        event_type: PromptForgeEventType::PrCreated,
    })
    .unwrap();
    db.record_resource_usage(&crate::activity::resource_usage::ResourceUsage {
        input_id: Some(input_id),
        model: "claude-opus-5".to_owned(),
        tokens_input,
        tokens_output,
        tokens_cache_read: None,
        tokens_cache_write: None,
        cost_usd: 1.25,
        duration_ms: Some(1_000),
        provider: "anthropic".to_owned(),
        timestamp: Utc::now(),
    })
    .unwrap();
    Arc::new(Mutex::new(db))
}

/// Write an executable fake `gh` whose `pr list` arm **rejects** the
/// enriched `--json` field set exactly the way a `gh` too old to know
/// `additions` does, and answers the narrower pre-#4497 set. Also logs every
/// invocation so a test can prove the retry happened.
/// (Also rejects the #6596 `isPrivate` repo-view field, which the same
/// vintage of `gh` would not know either — so this fake exercises **both**
/// narrower-field-set retries.)
fn write_fake_gh_rejecting_display_fields(dir: &std::path::Path) -> (PathBuf, PathBuf) {
    let log = dir.join("gh-forge-invocations.log");
    let script_path = dir.join("fake-old-gh.sh");
    let body = format!(
        "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >> \"{}\"\n\
             case \"$1 $2\" in\n  \
             'pr list')\n    \
             if [[ \"$*\" == *additions* ]]; then\n      \
             echo 'unknown JSON field: \"additions\"' >&2\n      exit 1\n    fi\n    \
             printf '%s\\n' {}; exit 0 ;;\n  \
             'repo view')\n    \
             if [[ \"$*\" == *isPrivate* ]]; then\n      \
             echo 'unknown JSON field: \"isPrivate\"' >&2\n      exit 1\n    fi\n    \
             printf '%s\\n' {}; exit 0 ;;\n  \
             *) exit 1 ;;\nesac\n",
        log.display(),
        shell_quote(MERGED_PR_JSON_NO_DISPLAY_FIELDS),
        shell_quote(&repo_view_json("rjwalters/loom", None)),
    );
    std::fs::write(&script_path, body).unwrap();
    let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script_path, perms).unwrap();
    (script_path, log)
}

#[tokio::test]
#[serial]
async fn completion_for_exit_emits_when_the_pr_merged() {
    let dir = tempfile::tempdir().unwrap();
    let (fake_gh, _log) =
        write_fake_forge_gh(dir.path(), Some(MERGED_PR_JSON), Some("rjwalters/loom"));
    std::env::set_var(GH_BIN_ENV, &fake_gh);

    let root = dir.path().to_string_lossy().into_owned();
    let exited_at = DateTime::parse_from_rfc3339("2026-07-29T10:12:30Z")
        .unwrap()
        .with_timezone(&Utc);
    let envelope = completion_for_exit(
        "loom_daemon",
        &root,
        4426,
        750,
        exited_at,
        &mut HashMap::new(),
        &mut std::collections::HashSet::new(),
        None,
        None,
    )
    .await;

    std::env::remove_var(GH_BIN_ENV);
    let envelope = envelope.expect("a merged PR must produce a completion envelope");
    assert_eq!(envelope.kind, "completion");
    let meta = envelope.meta.as_ref().unwrap();
    assert_eq!(meta["schema"], json!("completion-v1"));
    assert_eq!(meta["agent"], json!("loom_daemon"));
    // The forge slug, not the `#4201` path-basename narration convention.
    assert_eq!(meta["repo"], json!("rjwalters/loom"));
    assert_eq!(meta["ref"], json!("https://github.com/rjwalters/loom/pull/4400"));
    assert_eq!(meta["result"], json!("success"));
    assert_eq!(meta["issue"], json!(4426));
    // started_at is derived from the exit clock minus duration_sec.
    assert_eq!(meta["started_at"], json!("2026-07-29T10:00:00Z"));
    assert_eq!(meta["completed_at"], json!("2026-07-29T10:12:30Z"));
    // Feed display fields (#4497), harvested from the same `gh pr list` call
    // that verified the merge — no extra forge round-trip.
    assert_eq!(meta["title"], json!("feat: enrich completion meta"));
    assert_eq!(meta["additions"], json!(214));
    assert_eq!(meta["deletions"], json!(37));
    // No activity-DB handle was threaded in ⇒ `tokens` is omitted, never
    // guessed (the degradation contract).
    assert!(meta.get("tokens").is_none());
    assert!(build_send_request(&envelope, 1, None).is_ok());
}

#[tokio::test]
#[serial]
async fn completion_for_exit_omits_display_fields_the_forge_did_not_return() {
    // The pre-#4497 row shape: the merge facts are all there, none of the
    // display fields are. The completion must still publish, with an
    // envelope byte-identical to the pre-#4497 one (#4497 AC2).
    let dir = tempfile::tempdir().unwrap();
    let (fake_gh, _log) = write_fake_forge_gh(
        dir.path(),
        Some(MERGED_PR_JSON_NO_DISPLAY_FIELDS),
        Some("rjwalters/loom"),
    );
    std::env::set_var(GH_BIN_ENV, &fake_gh);

    let envelope = completion_for_exit(
        "loom_daemon",
        &dir.path().to_string_lossy(),
        4426,
        750,
        Utc::now(),
        &mut HashMap::new(),
        &mut std::collections::HashSet::new(),
        None,
        None,
    )
    .await;

    std::env::remove_var(GH_BIN_ENV);
    let envelope = envelope.expect("missing display fields must not cost us the completion");
    let meta = envelope.meta.as_ref().unwrap();
    for key in ["title", "additions", "deletions", "tokens"] {
        assert!(meta.get(key).is_none(), "{key} must be omitted when unavailable");
    }
    // Required keys + `issue` + the #6596 `visibility` tag, i.e. the
    // pre-#4497 envelope with no display field reinstated.
    assert_eq!(meta["visibility"], json!("public"));
    assert_eq!(meta.as_object().unwrap().len(), COMPLETION_REQUIRED_KEYS.len() + 2);
    assert!(build_send_request(&envelope, 1, None).is_ok());
}

#[tokio::test]
#[serial]
async fn completion_for_exit_retries_the_base_field_set_when_gh_rejects_the_new_ones() {
    // A `gh` that does not know `additions` rejects the *whole* request, so
    // without the narrower retry #4497 would have silently cost every
    // completion on such a host.
    let dir = tempfile::tempdir().unwrap();
    let (fake_gh, log) = write_fake_gh_rejecting_display_fields(dir.path());
    std::env::set_var(GH_BIN_ENV, &fake_gh);

    let envelope = completion_for_exit(
        "loom_daemon",
        &dir.path().to_string_lossy(),
        4426,
        750,
        Utc::now(),
        &mut HashMap::new(),
        &mut std::collections::HashSet::new(),
        None,
        None,
    )
    .await;

    std::env::remove_var(GH_BIN_ENV);
    let envelope = envelope.expect("an old gh must still yield a completion, minus the extras");
    let meta = envelope.meta.as_ref().unwrap();
    assert_eq!(meta["ref"], json!("https://github.com/rjwalters/loom/pull/4400"));
    assert!(meta.get("title").is_none());
    assert!(meta.get("additions").is_none());
    // Same contract for the #6596 visibility tag: an old `gh` costs the
    // tag, not the completion — and the tag is omitted rather than
    // defaulted, so a correct egress consumer fails closed on this host.
    assert!(meta.get("visibility").is_none());
    let calls = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        calls.contains("additions"),
        "the enriched field set must be tried first; log: {calls:?}"
    );
    assert!(
        calls
            .lines()
            .any(|l| l.starts_with("pr list") && !l.contains("additions")),
        "the rejection must be retried with the base field set; log: {calls:?}"
    );
    assert!(
        calls
            .lines()
            .any(|l| l.starts_with("repo view") && !l.contains("isPrivate")),
        "the repo-view rejection must likewise be retried narrower; log: {calls:?}"
    );
}

// ---- repo-visibility tag + per-owner credential routing (#6596) ----

#[tokio::test]
#[serial]
async fn completion_for_a_private_repo_still_narrates_but_is_tagged_private() {
    // The disclosure half of #6596: a private repo must keep narrating into
    // the (private) signal room exactly as before, while carrying the tag a
    // public-feed egress can refuse to publish on.
    let dir = tempfile::tempdir().unwrap();
    let (fake_gh, _log) = write_fake_forge_gh_with_repo_view(
        dir.path(),
        Some(MERGED_PR_JSON),
        Some(&repo_view_json("2AMLogic/product", Some(true))),
    );
    std::env::set_var(GH_BIN_ENV, &fake_gh);

    let envelope = completion_for_exit(
        "loom_daemon",
        &dir.path().to_string_lossy(),
        4426,
        750,
        Utc::now(),
        &mut HashMap::new(),
        &mut std::collections::HashSet::new(),
        None,
        None,
    )
    .await;

    std::env::remove_var(GH_BIN_ENV);
    let envelope = envelope.expect("a private repo must still narrate its completion");
    let meta = envelope.meta.as_ref().unwrap();
    assert_eq!(meta["repo"], json!("2AMLogic/product"));
    assert_eq!(
        meta["visibility"],
        json!("private"),
        "a private repo's completion must be tagged so egress can withhold it"
    );
    assert!(
        build_send_request(&envelope, 1, None).is_ok(),
        "the tag must not make the envelope unsendable — the room post is unchanged"
    );
}

#[tokio::test]
#[serial]
async fn completion_for_a_public_repo_is_tagged_public() {
    let dir = tempfile::tempdir().unwrap();
    let (fake_gh, _log) =
        write_fake_forge_gh(dir.path(), Some(MERGED_PR_JSON), Some("rjwalters/loom"));
    std::env::set_var(GH_BIN_ENV, &fake_gh);

    let envelope = completion_for_exit(
        "loom_daemon",
        &dir.path().to_string_lossy(),
        4426,
        750,
        Utc::now(),
        &mut HashMap::new(),
        &mut std::collections::HashSet::new(),
        None,
        None,
    )
    .await;

    std::env::remove_var(GH_BIN_ENV);
    let meta = envelope.as_ref().and_then(|e| e.meta.as_ref()).unwrap();
    assert_eq!(meta["visibility"], json!("public"));
}

#[test]
fn validate_completion_meta_rejects_an_unknown_visibility() {
    // The egress gate keys on this value, so a third value must never reach
    // the wire where it would have to be interpreted.
    for bogus in [json!("internal"), json!(true), json!(""), json!(null)] {
        let mut meta = sample_completion_meta().to_meta_value().unwrap();
        meta["visibility"] = bogus.clone();
        assert!(
            validate_completion_meta(&meta).is_err(),
            "visibility {bogus} must be refused, not sent"
        );
    }
    // Both legal values, and absence, stay valid.
    for legal in [
        Some(RepoVisibility::Public),
        Some(RepoVisibility::Private),
        None,
    ] {
        let meta = CompletionMeta {
            visibility: legal,
            ..sample_completion_meta()
        }
        .to_meta_value();
        assert!(meta.is_ok(), "{legal:?} must build a valid completion meta");
    }
}

/// A fake `gh` that records the `GH_CONFIG_DIR` each invocation was spawned
/// with (`<unset>` when it carried none), so a test can assert which
/// credential the sink's forge lookups actually used (#6596). Answers all
/// three subcommands the module shells out to.
fn write_env_probing_gh(dir: &std::path::Path) -> (PathBuf, PathBuf) {
    let log = dir.join("gh-config-dir.log");
    let script_path = dir.join("fake-env-probe-gh.sh");
    let body = format!(
            "#!/usr/bin/env bash\nprintf '%s\\t%s\\n' \"$1 $2\" \"${{GH_CONFIG_DIR:-<unset>}}\" >> \"{}\"\n\
             case \"$1 $2\" in\n  \
             'pr list') printf '%s\\n' '[]'; exit 0 ;;\n  \
             'repo view') printf '%s\\n' '{{\"nameWithOwner\":\"2AMLogic/product\",\"isPrivate\":true}}'; exit 0 ;;\n  \
             'issue view') printf '%s\\n' 'a title'; exit 0 ;;\n  \
             *) exit 1 ;;\nesac\n",
            log.display(),
        );
    std::fs::write(&script_path, body).unwrap();
    let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script_path, perms).unwrap();
    (script_path, log)
}

/// Drive every `gh` call site in this module against `root`, returning the
/// `(subcommand, GH_CONFIG_DIR)` pairs the probe recorded.
async fn probe_gh_config_dirs(probe_log: &Path, root: &Path) -> Vec<(String, String)> {
    let _ = fetch_merged_pr(root, 6596).await;
    let _ = fetch_repo_identity(root).await;
    let _ = fetch_recent_merged_prs(root).await;
    let _ = fetch_issue_title(root, 6596).await;
    let text = std::fs::read_to_string(probe_log).unwrap_or_default();
    text.lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(call, dir)| (call.to_owned(), dir.to_owned()))
        .collect()
}

#[tokio::test]
#[serial]
async fn sink_forge_lookups_carry_the_workspace_owners_gh_config_dir() {
    // #6596: the daemon process runs under the PRIMARY installation's
    // credential, which cannot see a private repo owned by another org.
    // Every forge lookup in this module must use the same per-owner
    // GH_CONFIG_DIR the dispatch paths hand their sweep children.
    crate::credential_preflight::clear_owner_root_registry();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("product");
    std::fs::create_dir_all(&root).unwrap();
    let owner_dir = dir.path().join(".loom/gh-config-by-owner/2AMLogic");
    crate::credential_preflight::register_root_gh_config_dir(&root, &owner_dir);
    let (fake_gh, probe_log) = write_env_probing_gh(dir.path());
    std::env::set_var(GH_BIN_ENV, &fake_gh);

    let calls = probe_gh_config_dirs(&probe_log, &root).await;

    std::env::remove_var(GH_BIN_ENV);
    crate::credential_preflight::clear_owner_root_registry();
    let expected = owner_dir.display().to_string();
    assert_eq!(calls.len(), 4, "all four call sites must have shelled out; got {calls:?}");
    for (call, config_dir) in &calls {
        assert_eq!(
            config_dir, &expected,
            "`gh {call}` must run under the owner's credential; got {calls:?}"
        );
    }
}

#[tokio::test]
#[serial]
async fn sink_forge_lookups_leave_an_unregistered_workspace_untouched() {
    // The no-op half: a single-owner fleet (and the root owner's own repos)
    // must be byte-identical to pre-#6596 — the child simply inherits the
    // daemon's process-global GH_CONFIG_DIR.
    crate::credential_preflight::clear_owner_root_registry();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("loom");
    std::fs::create_dir_all(&root).unwrap();
    let (fake_gh, probe_log) = write_env_probing_gh(dir.path());
    std::env::set_var(GH_BIN_ENV, &fake_gh);

    let calls = probe_gh_config_dirs(&probe_log, &root).await;

    std::env::remove_var(GH_BIN_ENV);
    let inherited = std::env::var("GH_CONFIG_DIR").unwrap_or_else(|_| "<unset>".to_owned());
    assert_eq!(calls.len(), 4);
    for (call, config_dir) in &calls {
        assert_eq!(
            config_dir, &inherited,
            "`gh {call}` on an unregistered root must inherit, not override; got {calls:?}"
        );
    }
}

#[test]
fn a_failing_forge_lookup_warns_once_per_call_and_workspace() {
    // The diagnosability half of #6596: silent-by-contract behavior, but
    // one breadcrumb per workspace — and not one per reconciliation tick.
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("product");
    let b = dir.path().join("sky130-pll");
    assert!(first_gh_failure_for("pr list", &a), "first failure warns");
    assert!(!first_gh_failure_for("pr list", &a), "a repeat must not re-warn");
    assert!(
        first_gh_failure_for("repo view", &a),
        "a different call in the same workspace is its own breadcrumb"
    );
    assert!(
        first_gh_failure_for("pr list", &b),
        "a different workspace warns on its own first failure"
    );
}

#[test]
fn a_recovered_forge_lookup_clears_the_warned_state_once_and_can_rewarn() {
    // #6619: the recovery half of the #6596 warn-once contract. Mirrors
    // `a_failing_forge_lookup_warns_once_per_call_and_workspace`'s shape,
    // unit-testing the clear/reset primitive directly — `clear_gh_failure_for`
    // returning `true` is exactly the condition under which
    // `log_gh_recovery_once` fires its `log::info!` exactly once.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("product");

    // Nothing was ever recorded as failing — clearing is a no-op, so the
    // caller's `log::info!` must not fire on an ordinary healthy success.
    assert!(
        !clear_gh_failure_for("pr list", &root),
        "nothing to clear before any failure was recorded"
    );

    assert!(first_gh_failure_for("pr list", &root), "first failure warns");
    assert!(
        clear_gh_failure_for("pr list", &root),
        "a recorded failure is removed on the first subsequent success"
    );
    assert!(
        !clear_gh_failure_for("pr list", &root),
        "a second success has nothing left to clear — the recovery info! must not re-fire"
    );

    // The clear must not permanently disable warn-once for this key: a
    // fresh failure after recovery has to warn again, not stay silently
    // suppressed forever.
    assert!(
        first_gh_failure_for("pr list", &root),
        "a fresh failure after recovery warns again"
    );
    assert!(
        !first_gh_failure_for("pr list", &root),
        "and that fresh failure still only warns once"
    );
}

#[test]
fn stderr_head_is_a_single_capped_line() {
    assert_eq!(
            stderr_head(b"GraphQL: Could not resolve to a Repository with the name '2AMLogic/product'. (repository)\n"),
            "GraphQL: Could not resolve to a Repository with the name '2AMLogic/product'. (repository)"
        );
    assert_eq!(stderr_head(b""), "no stderr");
    assert_eq!(stderr_head(b"\n   \n"), "no stderr");
    // Only the first non-empty line, never a screenful.
    assert_eq!(stderr_head(b"\nfirst line\nsecond line\n"), "first line");
    let long = "x".repeat(500);
    let head = stderr_head(long.as_bytes());
    assert_eq!(head.chars().count(), 201, "200 chars plus the ellipsis");
    assert!(head.ends_with('…'));
}

#[tokio::test]
#[serial]
async fn completion_for_exit_carries_tokens_from_the_activity_db_rollup() {
    // The per-issue rollup is the sink's token source (#4497). Attribution is
    // knowingly imperfect (see `fetch_issue_tokens`), but when the DB *has* a
    // rollup for the issue the completion must publish input+output.
    let dir = tempfile::tempdir().unwrap();
    let (fake_gh, _log) =
        write_fake_forge_gh(dir.path(), Some(MERGED_PR_JSON), Some("rjwalters/loom"));
    std::env::set_var(GH_BIN_ENV, &fake_gh);

    let db = seed_activity_db_with_issue_tokens(dir.path(), 4426, 700_000, 91_000);

    let envelope = completion_for_exit(
        "loom_daemon",
        &dir.path().to_string_lossy(),
        4426,
        750,
        Utc::now(),
        &mut HashMap::new(),
        &mut std::collections::HashSet::new(),
        Some(&db),
        None,
    )
    .await;

    std::env::remove_var(GH_BIN_ENV);
    let envelope = envelope.expect("a merged PR must produce a completion envelope");
    let meta = envelope.meta.as_ref().unwrap();
    assert_eq!(meta["tokens"], json!(791_000), "tokens = input + output");
    assert!(build_send_request(&envelope, 1, None).is_ok());
}

/// Point `CLAUDE_CONFIG_DIR` at an empty directory so the #4699 transcript
/// fallback is exercised hermetically rather than against whatever sweeps
/// the developer's real `~/.claude` happens to hold.
fn isolate_claude_config_dir(dir: &Path) -> PathBuf {
    let config = dir.join("claude-config");
    std::fs::create_dir_all(config.join("projects")).unwrap();
    std::env::set_var("CLAUDE_CONFIG_DIR", &config);
    config
}

/// Seed one `/loom:sweep <issue>` session transcript for `workspace_root`
/// under `config_dir`, laid out exactly as Claude Code writes it: the parent
/// `<uuid>.jsonl` plus one `<uuid>/subagents/agent-*.jsonl` phase file.
fn seed_sweep_transcript(config_dir: &Path, workspace_root: &Path, issue: u32) {
    let project = config_dir
        .join("projects")
        .join(crate::transcript_tokens::project_slug(workspace_root));
    std::fs::create_dir_all(project.join("uuid-1/subagents")).unwrap();
    let head = format!(
        "{{\"type\":\"user\",\"message\":{{\"content\":\
             \"<command-name>/loom:sweep</command-name>\\n\
             <command-args>{issue} --claim-owned {issue}</command-args>\"}}}}\n"
    );
    let usage = |input: u32, output: u32, read: u32, create: u32| {
        format!(
            "{{\"message\":{{\"usage\":{{\"input_tokens\":{input},\
                 \"output_tokens\":{output},\"cache_read_input_tokens\":{read},\
                 \"cache_creation_input_tokens\":{create}}}}}}}\n"
        )
    };
    std::fs::write(
        project.join("uuid-1.jsonl"),
        format!("{head}{}", usage(1_000, 2_000, 30_000, 4_000)),
    )
    .unwrap();
    std::fs::write(project.join("uuid-1/subagents/agent-bld.jsonl"), usage(100, 200, 3_000, 400))
        .unwrap();
}

#[tokio::test]
#[serial]
async fn completion_for_exit_omits_tokens_when_the_rollup_is_empty() {
    // "Omit rather than guess": an activity DB with no rollup for this issue
    // must not publish a `0`, which the feed would chart as free work. With
    // #4699 the transcript fallback must come up empty too — hence the
    // isolated (and unseeded) `CLAUDE_CONFIG_DIR`.
    let dir = tempfile::tempdir().unwrap();
    let (fake_gh, _log) =
        write_fake_forge_gh(dir.path(), Some(MERGED_PR_JSON), Some("rjwalters/loom"));
    std::env::set_var(GH_BIN_ENV, &fake_gh);
    isolate_claude_config_dir(dir.path());

    // Seeded for a *different* issue, so this issue's rollup is empty.
    let db = seed_activity_db_with_issue_tokens(dir.path(), 999, 12_345, 678);

    let envelope = completion_for_exit(
        "loom_daemon",
        &dir.path().to_string_lossy(),
        4426,
        750,
        Utc::now(),
        &mut HashMap::new(),
        &mut std::collections::HashSet::new(),
        Some(&db),
        None,
    )
    .await;

    std::env::remove_var(GH_BIN_ENV);
    std::env::remove_var("CLAUDE_CONFIG_DIR");
    let meta = envelope
        .as_ref()
        .and_then(|e| e.meta.as_ref())
        .expect("an empty rollup must never cost us the completion");
    assert!(meta.get("tokens").is_none(), "an empty rollup ⇒ omitted, not 0");
    // The forge-sourced display fields are unaffected by the token miss.
    assert_eq!(meta["title"], json!("feat: enrich completion meta"));
}

#[tokio::test]
#[serial]
async fn completion_for_exit_falls_back_to_sweep_transcripts_when_the_db_is_empty() {
    // Regression for #4699: on a dispatch-driven host the activity DB's
    // `resource_usage`/`prompt_github` tables have no writer at all, so the
    // rollup is *structurally* empty and every published completion carried
    // `tokens: null`. The sweep's own on-disk transcripts must supply the
    // total instead — parent session + every subagent phase file, all four
    // usage counters (cache reads included).
    let dir = tempfile::tempdir().unwrap();
    let (fake_gh, _log) =
        write_fake_forge_gh(dir.path(), Some(MERGED_PR_JSON), Some("rjwalters/loom"));
    std::env::set_var(GH_BIN_ENV, &fake_gh);
    let config = isolate_claude_config_dir(dir.path());
    seed_sweep_transcript(&config, dir.path(), 4426);

    // No activity DB handle at all — the exact shape of the dispatch path.
    let envelope = completion_for_exit(
        "loom_daemon",
        &dir.path().to_string_lossy(),
        4426,
        750,
        Utc::now(),
        &mut HashMap::new(),
        &mut std::collections::HashSet::new(),
        None,
        None,
    )
    .await;

    std::env::remove_var(GH_BIN_ENV);
    std::env::remove_var("CLAUDE_CONFIG_DIR");
    let envelope = envelope.expect("a merged PR must produce a completion envelope");
    let meta = envelope.meta.as_ref().unwrap();
    // parent 37_000 + subagent 3_700
    assert_eq!(
        meta["tokens"],
        json!(40_700),
        "tokens must come from the transcripts when the DB rollup is empty"
    );
    assert!(build_send_request(&envelope, 1, None).is_ok());
}

#[tokio::test]
#[serial]
async fn completion_for_exit_prefers_the_activity_db_over_the_transcripts() {
    // The DB is the cheaper lookup and stays authoritative on hosts that do
    // drive managed terminals; the transcript scan is a fallback, not an
    // override. Both sources are present here, so the DB figure must win.
    let dir = tempfile::tempdir().unwrap();
    let (fake_gh, _log) =
        write_fake_forge_gh(dir.path(), Some(MERGED_PR_JSON), Some("rjwalters/loom"));
    std::env::set_var(GH_BIN_ENV, &fake_gh);
    let config = isolate_claude_config_dir(dir.path());
    seed_sweep_transcript(&config, dir.path(), 4426);

    let db = seed_activity_db_with_issue_tokens(dir.path(), 4426, 700_000, 91_000);

    let envelope = completion_for_exit(
        "loom_daemon",
        &dir.path().to_string_lossy(),
        4426,
        750,
        Utc::now(),
        &mut HashMap::new(),
        &mut std::collections::HashSet::new(),
        Some(&db),
        None,
    )
    .await;

    std::env::remove_var(GH_BIN_ENV);
    std::env::remove_var("CLAUDE_CONFIG_DIR");
    let meta = envelope.as_ref().and_then(|e| e.meta.as_ref()).unwrap();
    assert_eq!(meta["tokens"], json!(791_000), "the DB rollup wins when non-empty");
}

#[tokio::test]
#[serial]
async fn the_transcript_fallback_can_be_disabled_by_env() {
    // Escape hatch for an operator who does not want the completion path
    // reading the Claude Code project directory at all.
    let dir = tempfile::tempdir().unwrap();
    let (fake_gh, _log) =
        write_fake_forge_gh(dir.path(), Some(MERGED_PR_JSON), Some("rjwalters/loom"));
    std::env::set_var(GH_BIN_ENV, &fake_gh);
    let config = isolate_claude_config_dir(dir.path());
    seed_sweep_transcript(&config, dir.path(), 4426);
    std::env::set_var("LOOM_SAFEHOUSE_TRANSCRIPT_TOKENS", "0");

    let envelope = completion_for_exit(
        "loom_daemon",
        &dir.path().to_string_lossy(),
        4426,
        750,
        Utc::now(),
        &mut HashMap::new(),
        &mut std::collections::HashSet::new(),
        None,
        None,
    )
    .await;

    std::env::remove_var(GH_BIN_ENV);
    std::env::remove_var("CLAUDE_CONFIG_DIR");
    std::env::remove_var("LOOM_SAFEHOUSE_TRANSCRIPT_TOKENS");
    let meta = envelope
        .as_ref()
        .and_then(|e| e.meta.as_ref())
        .expect("opting out of the fallback must not cost the completion");
    assert!(meta.get("tokens").is_none(), "opted out ⇒ tokens omitted");
}

#[tokio::test]
#[serial]
async fn completion_for_exit_is_silent_when_the_pr_did_not_merge() {
    // Exit 0 is not a merge: a clean sweep whose PR is still open must not
    // claim `result: "success"` on the public feed.
    let dir = tempfile::tempdir().unwrap();
    let (fake_gh, _log) = write_fake_forge_gh(dir.path(), Some("[]"), Some("rjwalters/loom"));
    std::env::set_var(GH_BIN_ENV, &fake_gh);

    let out = completion_for_exit(
        "loom_daemon",
        &dir.path().to_string_lossy(),
        4426,
        750,
        Utc::now(),
        &mut HashMap::new(),
        &mut std::collections::HashSet::new(),
        None,
        None,
    )
    .await;

    std::env::remove_var(GH_BIN_ENV);
    assert!(out.is_none(), "no merged PR ⇒ no completion");
}

#[tokio::test]
#[serial]
async fn completion_for_exit_degrades_to_none_when_gh_fails() {
    let dir = tempfile::tempdir().unwrap();
    let (fake_gh, _log) = write_fake_forge_gh(dir.path(), None, None);
    std::env::set_var(GH_BIN_ENV, &fake_gh);

    let out = completion_for_exit(
        "loom_daemon",
        &dir.path().to_string_lossy(),
        4426,
        750,
        Utc::now(),
        &mut HashMap::new(),
        &mut std::collections::HashSet::new(),
        None,
        None,
    )
    .await;

    std::env::remove_var(GH_BIN_ENV);
    assert!(out.is_none(), "a failing gh must degrade to None, never panic or block");
}

#[tokio::test]
#[serial]
async fn completion_for_exit_emits_at_most_once_per_merge() {
    // A resumed sweep produces a second SweepExited for the same issue —
    // the merge is still the same one, so only one completion is emitted.
    //
    // Unlike the pre-#6062 shape, the dedup key now includes the merged
    // PR number, which is only known once `fetch_merged_pr` answers — so
    // the second exit still re-runs that one bounded `pr list` lookup
    // (rather than short-circuiting on the issue number alone, which
    // would incorrectly suppress a genuinely *different* merged PR for
    // the same still-open issue) but stops there, before the more
    // expensive repo-identity/token lookups.
    let dir = tempfile::tempdir().unwrap();
    let (fake_gh, log) =
        write_fake_forge_gh(dir.path(), Some(MERGED_PR_JSON), Some("rjwalters/loom"));
    std::env::set_var(GH_BIN_ENV, &fake_gh);

    let root = dir.path().to_string_lossy().into_owned();
    let mut slug_cache = HashMap::new();
    let mut completed = std::collections::HashSet::new();
    let first = completion_for_exit(
        "loom_daemon",
        &root,
        4426,
        750,
        Utc::now(),
        &mut slug_cache,
        &mut completed,
        None,
        None,
    )
    .await;
    let second = completion_for_exit(
        "loom_daemon",
        &root,
        4426,
        760,
        Utc::now(),
        &mut slug_cache,
        &mut completed,
        None,
        None,
    )
    .await;

    std::env::remove_var(GH_BIN_ENV);
    assert!(first.is_some());
    assert!(second.is_none(), "a second exit for the same issue must not double-post");
    let calls = std::fs::read_to_string(&log).unwrap_or_default();
    assert_eq!(
        calls.lines().count(),
        3,
        "the second exit re-runs the merge-verification lookup (PR number is unknown \
             until then) but must short-circuit before the repo-identity/token lookups once \
             the same merged PR is confirmed already-narrated; log: {calls:?}"
    );
    assert_eq!(
        calls.lines().filter(|l| l.starts_with("pr list")).count(),
        2,
        "both exits run the merge-verification lookup; log: {calls:?}"
    );
    assert_eq!(
        calls.lines().filter(|l| l.starts_with("repo view")).count(),
        1,
        "only the first (narrating) exit reaches the repo-identity lookup; log: {calls:?}"
    );
}

#[tokio::test]
#[serial]
async fn completion_for_exit_narrates_a_second_distinct_merged_pr_on_the_same_issue() {
    // Issue #6062's crux: an issue can legitimately merge more than one
    // PR over its lifetime (partial increments, `Part of #N`). Keying the
    // dedup on the issue alone (the pre-#6062 shape) would permanently
    // suppress the second merge; keying on `(issue, merged-PR-number)`
    // narrates both.
    let dir = tempfile::tempdir().unwrap();
    let (fake_gh, _log) =
        write_fake_forge_gh(dir.path(), Some(MERGED_PR_JSON), Some("rjwalters/loom"));
    std::env::set_var(GH_BIN_ENV, &fake_gh);

    let root = dir.path().to_string_lossy().into_owned();
    let mut slug_cache = HashMap::new();
    let mut completed = std::collections::HashSet::new();
    let first = completion_for_exit(
        "loom_daemon",
        &root,
        4426,
        750,
        Utc::now(),
        &mut slug_cache,
        &mut completed,
        None,
        None,
    )
    .await;
    assert!(first.is_some(), "the first merged PR (#4400) must narrate");

    // A second, later PR merges against the SAME still-open issue —
    // simulated by rewriting the fake `gh`'s `pr list` answer in place.
    const SECOND_MERGED_PR_JSON: &str = r#"[{"number":4401,"url":"https://github.com/rjwalters/loom/pull/4401","mergedAt":"2026-07-30T09:00:00Z","title":"test: cover the PEEC extraction path","additions":40,"deletions":3}]"#;
    write_fake_forge_gh(dir.path(), Some(SECOND_MERGED_PR_JSON), Some("rjwalters/loom"));

    let second = completion_for_exit(
        "loom_daemon",
        &root,
        4426,
        300,
        Utc::now(),
        &mut slug_cache,
        &mut completed,
        None,
        None,
    )
    .await;
    std::env::remove_var(GH_BIN_ENV);
    assert!(
        second.is_some(),
        "a distinct merged PR (#4401) against the same still-open issue must still narrate, \
             not be silently swallowed by the first merge's dedup key"
    );
    let second_meta = second.unwrap().meta.unwrap();
    assert_eq!(second_meta["ref"], json!("https://github.com/rjwalters/loom/pull/4401"));
    assert!(completed.contains(&(root.clone(), 4426, 4400)));
    assert!(completed.contains(&(root, 4426, 4401)));
}

// ---- periodic merge reconciliation: the champion-merge path (#4583) ----

/// One bulk `gh pr list --state merged` row (the reconciliation query
/// shape): `headRefName`/`createdAt` on top of the per-issue lookup's
/// fields, since the bulk query is not filtered to one issue's branch and
/// has no live sweep clock to borrow `started_at` from.
///
/// Timestamps are **relative to now** rather than literals: the pass drops
/// rows merged longer than [`reconcile_max_age`] ago, so hard-coded dates
/// would silently turn every reconciliation test into a no-op assertion
/// once wall-clock time drifted a week past them.
fn reconcile_pr_row(issue: u32, merged_minutes_ago: i64, title: &str) -> String {
    let merged_at = Utc::now() - chrono::Duration::minutes(merged_minutes_ago);
    let created_at = merged_at - chrono::Duration::minutes(30);
    format!(
        r#"{{"number":{issue},"headRefName":"feature/issue-{issue}","url":"https://github.com/rjwalters/loom/pull/{issue}","mergedAt":"{}","createdAt":"{}","title":"{title}","additions":120,"deletions":18}}"#,
        merged_at.to_rfc3339(),
        created_at.to_rfc3339(),
    )
}

/// The single-row response used by most reconciliation tests: issue #4610,
/// merged well inside the lookback window.
fn reconcile_merged_pr_json() -> String {
    format!("[{}]", reconcile_pr_row(4610, 20, "fix: champion-merge safehouse gap"))
}

/// Two merged PRs in one bulk response: issue #4610 (already narrated
/// in-sweep) and issue #4611 (a distinct, not-yet-narrated champion merge).
fn reconcile_two_merged_prs_json() -> String {
    format!(
        "[{},{}]",
        reconcile_pr_row(4610, 20, "fix: champion-merge safehouse gap"),
        reconcile_pr_row(4611, 5, "docs: unrelated cleanup"),
    )
}

#[tokio::test]
#[serial]
async fn reconcile_recent_merges_emits_completion_for_a_champion_driven_merge() {
    // The AC1 case: no live sweep ever observed this merge (no `SweepExited`
    // at merge time — the champion-tick steady state), so the only way it
    // is discovered at all is the bulk reconciliation query.
    let dir = tempfile::tempdir().unwrap();
    let (fake_gh, _log) =
        write_fake_forge_gh(dir.path(), Some(&reconcile_merged_pr_json()), Some("rjwalters/loom"));
    std::env::set_var(GH_BIN_ENV, &fake_gh);

    let root = dir.path().to_string_lossy().into_owned();
    let mut slug_cache = HashMap::new();
    let mut completed = std::collections::HashSet::new();
    let envelopes =
        reconcile_recent_merges("loom_daemon", &root, &mut slug_cache, &mut completed, None, None)
            .await;

    std::env::remove_var(GH_BIN_ENV);
    assert_eq!(envelopes.len(), 1, "exactly one completion for the champion merge");
    let meta = envelopes[0].meta.as_ref().unwrap();
    assert_eq!(meta["schema"], json!("completion-v1"));
    assert_eq!(meta["issue"], json!(4610));
    assert_eq!(meta["result"], json!("success"));
    assert_eq!(meta["title"], json!("fix: champion-merge safehouse gap"));
    assert_eq!(meta["additions"], json!(120));
    assert_eq!(meta["deletions"], json!(18));
    assert!(
        completed.contains(&(root, 4610, 4610)),
        "the shared dedup set must record the reconciled merge"
    );
}

#[tokio::test]
#[serial]
async fn reconcile_recent_merges_skips_a_merge_already_narrated_in_sweep() {
    // Regression (AC2): an in-sweep `SweepExited` narrates issue #4610
    // first; the reconciliation pass then observes the *same* merge via
    // its bulk query and must not double-post it — the two trigger paths
    // share one dedup set.
    let dir = tempfile::tempdir().unwrap();
    let (fake_gh, log) =
        write_fake_forge_gh(dir.path(), Some(&reconcile_merged_pr_json()), Some("rjwalters/loom"));
    std::env::set_var(GH_BIN_ENV, &fake_gh);

    let root = dir.path().to_string_lossy().into_owned();
    let mut slug_cache = HashMap::new();
    let mut completed = std::collections::HashSet::new();

    let in_sweep = completion_for_exit(
        "loom_daemon",
        &root,
        4610,
        1800,
        Utc::now(),
        &mut slug_cache,
        &mut completed,
        None,
        None,
    )
    .await;
    assert!(in_sweep.is_some(), "the in-sweep SweepExited path must narrate first");

    let reconciled =
        reconcile_recent_merges("loom_daemon", &root, &mut slug_cache, &mut completed, None, None)
            .await;

    std::env::remove_var(GH_BIN_ENV);
    assert!(
        reconciled.is_empty(),
        "the reconciliation pass must not double-post a merge already narrated in-sweep"
    );
    let calls = std::fs::read_to_string(&log).unwrap_or_default();
    assert_eq!(
        calls.lines().filter(|l| l.starts_with("pr list")).count(),
        2,
        "both the per-issue lookup and the bulk reconciliation query must still run \
             (the skip happens at the dedup check, not by avoiding the query); log: {calls:?}"
    );
}

// ---- fleet-wide completion dedup (Issue #6352) ----

/// Build a [`PeerCompletionHandle`] wrapping a fresh view + a bounded
/// outbound channel, returning both so a test can drain what was
/// published. Mirrors the shape `WorkspacePool::start_safehouse_narration`
/// wires in production.
fn peer_completion_handle_with_view(
    self_host: &str,
) -> (
    PeerCompletionHandle,
    Arc<Mutex<PeerClaimView>>,
    tokio::sync::mpsc::Receiver<ClaimAd>,
) {
    let view =
        Arc::new(Mutex::new(PeerClaimView::new(self_host.to_owned(), Duration::from_secs(3600))));
    let (tx, rx) = tokio::sync::mpsc::channel::<ClaimAd>(8);
    (PeerCompletionHandle::new(tx, view.clone()), view, rx)
}

#[tokio::test]
#[serial]
async fn build_and_narrate_completion_publishes_a_completed_ad_after_narrating() {
    // The publish half of #6352: once this host narrates, it must tell
    // peers so they back off — see the two-host test below for the
    // suppress half.
    let dir = tempfile::tempdir().unwrap();
    let (fake_gh, _log) =
        write_fake_forge_gh(dir.path(), Some(MERGED_PR_JSON), Some("rjwalters/loom"));
    std::env::set_var(GH_BIN_ENV, &fake_gh);

    let root = dir.path().to_string_lossy().into_owned();
    let (handle, _view, mut rx) = peer_completion_handle_with_view("host-a");

    let envelope = completion_for_exit(
        "loom_daemon",
        &root,
        4426,
        750,
        Utc::now(),
        &mut HashMap::new(),
        &mut std::collections::HashSet::new(),
        None,
        Some(&handle),
    )
    .await;
    std::env::remove_var(GH_BIN_ENV);
    assert!(envelope.is_some(), "the local narration must still succeed");

    let ad = rx
        .try_recv()
        .expect("a Completed ad must have been published");
    assert_eq!(ad.kind, crate::peer_claims::ClaimKind::Completed);
    assert_eq!(ad.issue, 4426);
    assert_eq!(ad.repo, crate::peer_claims::repo_slug(dir.path()));
    assert!(rx.try_recv().is_err(), "exactly one Completed ad per narrated completion");
}

#[tokio::test]
#[serial]
async fn build_and_narrate_completion_suppresses_when_a_peer_already_narrated() {
    // The suppress half of #6352, exercised directly against the shared
    // funnel: a peer's `Completed` ad has already landed in the view
    // (simulating the room relay) before this host's own narration
    // attempt — it must not re-narrate, and must still adopt the outcome
    // into its own local dedup set.
    let dir = tempfile::tempdir().unwrap();
    let (fake_gh, log) =
        write_fake_forge_gh(dir.path(), Some(MERGED_PR_JSON), Some("rjwalters/loom"));
    std::env::set_var(GH_BIN_ENV, &fake_gh);

    let root = dir.path().to_string_lossy().into_owned();
    let (handle, view, _rx) = peer_completion_handle_with_view("host-b");
    let repo_key = crate::peer_claims::repo_slug(dir.path());
    {
        let mut v = view.lock().unwrap();
        v.observe_completion_at(
            // PR #4400 matches `MERGED_PR_JSON`'s merged-PR number — the
            // dedup key is now `(repo, issue, pr)` (Issue #6062), so the
            // peer ad must name the same PR `fetch_merged_pr` will
            // discover for this suppression to actually engage.
            &ClaimAd::completed(4426, repo_key, "host-a".into(), 1, "ts".into(), 4400),
            Instant::now(),
        );
    }

    let mut already_narrated = std::collections::HashSet::new();
    let envelope = completion_for_exit(
        "loom_daemon",
        &root,
        4426,
        750,
        Utc::now(),
        &mut HashMap::new(),
        &mut already_narrated,
        None,
        Some(&handle),
    )
    .await;
    std::env::remove_var(GH_BIN_ENV);
    assert!(
        envelope.is_none(),
        "a completion already narrated by a peer must not be re-narrated"
    );
    assert!(
        already_narrated.contains(&(root, 4426, 4400)),
        "the peer's outcome must be adopted into local dedup state too"
    );
    // `completion_for_exit`'s own merge-verification lookup (`pr list`,
    // to confirm the sweep's PR actually merged before ever reaching the
    // shared funnel) still runs — the peer check lives *inside*
    // `build_and_narrate_completion`, not in the caller. What it must
    // skip is the `repo view` slug lookup and everything after it, which
    // only happens once the peer check has already passed.
    let calls = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        calls.lines().all(|l| l.starts_with("pr list")),
        "the peer check must short-circuit before the repo-slug lookup (`repo view`); \
             log: {calls:?}"
    );
    assert_eq!(
        calls.lines().count(),
        1,
        "exactly the one pre-existing merge-verification lookup, nothing from inside \
             build_and_narrate_completion; log: {calls:?}"
    );
}

/// The crux of the issue: build on host A, merge observed on host B —
/// exactly one completion envelope fleet-wide. Mirrors
/// `reconcile_recent_merges_skips_a_merge_already_narrated_in_sweep`
/// above, but across two INDEPENDENT dedup states (separate
/// `already_narrated` sets, separate `PeerClaimView`s) connected only by
/// manually relaying the outbound `ClaimAd` — simulating the room —
/// rather than sharing one process-local set.
#[tokio::test]
#[serial]
async fn two_hosts_build_on_a_merge_on_b_produce_exactly_one_completion_envelope() {
    let dir = tempfile::tempdir().unwrap();
    let (fake_gh, _log) =
        write_fake_forge_gh(dir.path(), Some(&reconcile_merged_pr_json()), Some("rjwalters/loom"));
    std::env::set_var(GH_BIN_ENV, &fake_gh);
    let root = dir.path().to_string_lossy().into_owned();

    // Host A: the sweep that built and opened the PR narrates it on
    // `SweepExited` first (AC1's "build on A").
    let (handle_a, _view_a, mut rx_a) = peer_completion_handle_with_view("host-a");
    let mut completed_a = std::collections::HashSet::new();
    let envelope_a = completion_for_exit(
        "loom_daemon",
        &root,
        4610,
        1800,
        Utc::now(),
        &mut HashMap::new(),
        &mut completed_a,
        None,
        Some(&handle_a),
    )
    .await;
    assert!(envelope_a.is_some(), "host A must narrate first");

    // The room relays A's outbound Completed ad to every peer, including
    // host B — simulated here as a direct hand-off (no socket in this
    // unit test; the socket transport itself is exercised by
    // `run_sink_*`/`coordination_*` tests elsewhere in this module).
    let ad = rx_a
        .try_recv()
        .expect("host A must have published a Completed ad");

    // Host B: a champion-tick merge with no live sweep of its own —
    // reconciliation is the only path that would ever discover this
    // merge on B (AC1's "merge on B"). B's view has already observed A's
    // ad by the time its reconciliation tick runs.
    let (handle_b, view_b, _rx_b) = peer_completion_handle_with_view("host-b");
    {
        let mut v = view_b.lock().unwrap();
        v.observe_completion_at(&ad, Instant::now());
    }
    let mut completed_b = std::collections::HashSet::new();
    let reconciled_b = reconcile_recent_merges(
        "loom_daemon",
        &root,
        &mut HashMap::new(),
        &mut completed_b,
        None,
        Some(&handle_b),
    )
    .await;
    std::env::remove_var(GH_BIN_ENV);

    assert!(
        reconciled_b.is_empty(),
        "host B must not narrate a completion host A already narrated fleet-wide"
    );
    assert!(
        completed_b.contains(&(root.clone(), 4610, 4610)),
        "host B's own local dedup must reflect the peer-narrated outcome"
    );
    // Exactly one completion envelope was produced across BOTH hosts —
    // the issue's own AC1.
    assert_eq!(
        1,
        usize::from(envelope_a.is_some()) + reconciled_b.len(),
        "exactly one completion envelope fleet-wide for this merge"
    );
}

/// AC2: a role tick that merely *observes* an already-merged PR it did
/// not merge itself must not publish a duplicate — the peer-suppressed
/// call above must not, in turn, re-broadcast its own `Completed` ad
/// (that would defeat the dedup by re-arming every peer's TTL forever).
#[tokio::test]
#[serial]
async fn a_suppressed_completion_never_re_publishes_its_own_completed_ad() {
    let dir = tempfile::tempdir().unwrap();
    let (fake_gh, _log) =
        write_fake_forge_gh(dir.path(), Some(MERGED_PR_JSON), Some("rjwalters/loom"));
    std::env::set_var(GH_BIN_ENV, &fake_gh);
    let root = dir.path().to_string_lossy().into_owned();

    let (handle, view, mut rx) = peer_completion_handle_with_view("host-b");
    let repo_key = crate::peer_claims::repo_slug(dir.path());
    {
        let mut v = view.lock().unwrap();
        v.observe_completion_at(
            // PR #4400 matches `MERGED_PR_JSON`'s merged-PR number — the
            // dedup key is now `(repo, issue, pr)` (Issue #6062), so the
            // peer ad must name the same PR `fetch_merged_pr` will
            // discover for this suppression to actually engage.
            &ClaimAd::completed(4426, repo_key, "host-a".into(), 1, "ts".into(), 4400),
            Instant::now(),
        );
    }

    let envelope = completion_for_exit(
        "loom_daemon",
        &root,
        4426,
        750,
        Utc::now(),
        &mut HashMap::new(),
        &mut std::collections::HashSet::new(),
        None,
        Some(&handle),
    )
    .await;
    std::env::remove_var(GH_BIN_ENV);
    assert!(envelope.is_none());
    assert!(
        rx.try_recv().is_err(),
        "a suppressed completion must not re-publish its own Completed ad"
    );
}

#[tokio::test]
#[serial]
async fn reconcile_recent_merges_narrates_distinct_issues_independently() {
    // Edge case (AC4): two merges for different issues in the same
    // workspace, one already narrated (in-sweep) and one not (champion-
    // driven) — both must be handled independently with no cross-
    // contamination of the dedup key.
    let dir = tempfile::tempdir().unwrap();
    let (fake_gh, _log) = write_fake_forge_gh(
        dir.path(),
        Some(&reconcile_two_merged_prs_json()),
        Some("rjwalters/loom"),
    );
    std::env::set_var(GH_BIN_ENV, &fake_gh);

    let root = dir.path().to_string_lossy().into_owned();
    let mut slug_cache = HashMap::new();
    let mut completed = std::collections::HashSet::new();
    // Issue #4610 was already narrated in-sweep before this reconciliation
    // tick ever ran.
    completed.insert((root.clone(), 4610, 4610));

    let envelopes =
        reconcile_recent_merges("loom_daemon", &root, &mut slug_cache, &mut completed, None, None)
            .await;

    std::env::remove_var(GH_BIN_ENV);
    assert_eq!(envelopes.len(), 1, "only the not-yet-narrated issue produces a completion");
    assert_eq!(envelopes[0].meta.as_ref().unwrap()["issue"], json!(4611));
    assert!(completed.contains(&(root.clone(), 4610, 4610)));
    assert!(completed.contains(&(root, 4611, 4611)));
}

#[tokio::test]
#[serial]
async fn reconcile_recent_merges_ignores_merges_older_than_the_lookback_window() {
    // The first pass on a host with no persisted dedup set (fresh install,
    // upgrade, lost completions file) must not backfill the public feed
    // with stale history: `RECONCILE_PR_LIMIT` bounds the burst size, the
    // lookback window bounds its age. Here the older row is outside the
    // window and the newer one is inside, with an empty dedup set — so only
    // the recent merge may be narrated.
    let dir = tempfile::tempdir().unwrap();
    let rows = format!(
        "[{},{}]",
        reconcile_pr_row(4610, 240, "chore: merged long before this daemon started"),
        reconcile_pr_row(4611, 5, "fix: just merged by a champion tick"),
    );
    let (fake_gh, _log) = write_fake_forge_gh(dir.path(), Some(&rows), Some("rjwalters/loom"));
    std::env::set_var(GH_BIN_ENV, &fake_gh);
    // One hour of lookback: the 240-minute-old row falls outside it.
    std::env::set_var(RECONCILE_MAX_AGE_ENV, "3600");

    let root = dir.path().to_string_lossy().into_owned();
    let mut slug_cache = HashMap::new();
    let mut completed = std::collections::HashSet::new();
    let envelopes =
        reconcile_recent_merges("loom_daemon", &root, &mut slug_cache, &mut completed, None, None)
            .await;

    std::env::remove_var(GH_BIN_ENV);
    std::env::remove_var(RECONCILE_MAX_AGE_ENV);
    assert_eq!(envelopes.len(), 1, "only the in-window merge may be narrated");
    assert_eq!(envelopes[0].meta.as_ref().unwrap()["issue"], json!(4611));
    assert!(
        !completed.contains(&(root.clone(), 4610, 4610)),
        "an out-of-window row is skipped before the dedup set, so a later in-window \
             observation of it (e.g. a resumed sweep's SweepExited) can still narrate"
    );
    assert!(completed.contains(&(root, 4611, 4611)));
}

#[test]
#[serial]
fn reconcile_max_age_defaults_and_honours_its_env_override() {
    std::env::remove_var(RECONCILE_MAX_AGE_ENV);
    assert_eq!(reconcile_max_age(), chrono::Duration::seconds(DEFAULT_RECONCILE_MAX_AGE_SECS));

    std::env::set_var(RECONCILE_MAX_AGE_ENV, "600");
    assert_eq!(reconcile_max_age(), chrono::Duration::seconds(600));

    // Garbage and negatives fall back to the default rather than degrading
    // into a window that silently narrates nothing.
    std::env::set_var(RECONCILE_MAX_AGE_ENV, "not-a-number");
    assert_eq!(reconcile_max_age(), chrono::Duration::seconds(DEFAULT_RECONCILE_MAX_AGE_SECS));
    std::env::set_var(RECONCILE_MAX_AGE_ENV, "-5");
    assert_eq!(reconcile_max_age(), chrono::Duration::seconds(DEFAULT_RECONCILE_MAX_AGE_SECS));
    std::env::remove_var(RECONCILE_MAX_AGE_ENV);
}

#[test]
fn load_persisted_completed_round_trips_through_persist_completed_best_effort() {
    // Building block for AC3 (restart survival): what one process wrote,
    // the next process's startup load must read back identically.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("safehouse-completed.json");

    let mut completed: std::collections::HashSet<(String, u32, u32)> =
        std::collections::HashSet::new();
    completed.insert(("/home/x/GitHub/loom".to_owned(), 4610, 815));
    completed.insert(("/home/x/GitHub/loom".to_owned(), 4611, 828));
    completed.insert(("/home/x/GitHub/vibesql".to_owned(), 42, 99));
    persist_completed_best_effort(Some(&path), &completed);

    let reloaded = load_persisted_completed(Some(&path));
    assert_eq!(reloaded, completed);
}

#[test]
fn load_persisted_completed_degrades_to_empty_when_the_file_is_absent_or_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        load_persisted_completed(Some(&dir.path().join("does-not-exist.json"))),
        std::collections::HashSet::new()
    );

    let corrupt = dir.path().join("corrupt.json");
    std::fs::write(&corrupt, "not valid json").unwrap();
    assert_eq!(load_persisted_completed(Some(&corrupt)), std::collections::HashSet::new());
}

#[tokio::test]
#[serial]
async fn run_sink_reconciliation_narrates_a_champion_merge_with_no_live_sweep() {
    // End-to-end AC1: reconciliation alone — no `SweepExited` published at
    // all — must still get the completion onto the wire, discovered
    // purely from the registered workspace + the bulk `gh pr list` query.
    let dir = tempfile::tempdir().unwrap();
    let _safehouse_test_paths = SafehouseTestPaths::set(dir.path());
    // A non-fresh (but empty) persisted dedup file (#4649): this test
    // exercises the steady-state narrate-immediately behavior, not the
    // seed-only first pass (covered separately below) — pre-seeding an
    // already-valid, empty file is what tells reconciliation this host
    // has reconciled before.
    persist_completed_best_effort(
        Some(&dir.path().join("safehouse-completed.json")),
        &std::collections::HashSet::new(),
    );
    let (fake_gh, _log) =
        write_fake_forge_gh(dir.path(), Some(&reconcile_merged_pr_json()), Some("rjwalters/loom"));
    std::env::set_var(GH_BIN_ENV, &fake_gh);
    // A fast reconciliation cadence so the test does not wait minutes.
    std::env::set_var(RECONCILE_INTERVAL_ENV, "20");
    // Register the workspace directly (mirrors what a real multi-repo
    // daemon already has on disk) so reconciliation has a target with zero
    // live events ever having flowed through the sink for it.
    let mut registry = crate::workspace_registry::WorkspaceRegistry::default();
    registry.add(dir.path(), None).unwrap();
    registry
        .save(&std::path::PathBuf::from(
            std::env::var(crate::workspace_registry::REGISTRY_PATH_ENV).unwrap(),
        ))
        .unwrap();

    let socket = dir.path().join("safehoused.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(stub_server(listener, false, 1));

    let bus = Arc::new(EventBus::new());
    let subscription = bus.subscribe(Vec::<String>::new());
    let sink = tokio::spawn(run_sink(
        SafehouseConfig {
            enabled: true,
            socket: Some(socket.clone()),
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

    // No SweepExited is ever published — the only way this can reach the
    // wire is the reconciliation pass.
    let received = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("reconciliation must narrate the champion merge without any live sweep")
        .unwrap();

    drop(bus);
    let _ = tokio::time::timeout(Duration::from_secs(2), sink).await;
    std::env::remove_var(GH_BIN_ENV);
    std::env::remove_var(RECONCILE_INTERVAL_ENV);

    assert_eq!(received.len(), 1);
    assert_eq!(received[0]["type"], json!("completion"));
    assert_eq!(received[0]["meta"]["issue"], json!(4610));
    assert_eq!(received[0]["meta"]["result"], json!("success"));
}

#[tokio::test]
#[serial]
async fn run_sink_reconciliation_seeds_a_fresh_dedup_file_without_narrating_the_backlog() {
    // #4649: a host with no persisted dedup file (fresh install, upgrade,
    // lost/corrupt file) must not burst-narrate every in-window merge on
    // its very first reconciliation tick. Two merges land inside the
    // lookback window with an absent completions file — every tick must
    // seed the dedup set (and persist it) without ever narrating either
    // one, even across several ticks (the same workspace round-robins
    // back to itself every tick here).
    let dir = tempfile::tempdir().unwrap();
    let _safehouse_test_paths = SafehouseTestPaths::set(dir.path());
    let (fake_gh, _log) = write_fake_forge_gh(
        dir.path(),
        Some(&reconcile_two_merged_prs_json()),
        Some("rjwalters/loom"),
    );
    std::env::set_var(GH_BIN_ENV, &fake_gh);
    // A fast reconciliation cadence so several ticks fire during the test.
    std::env::set_var(RECONCILE_INTERVAL_ENV, "20");
    let mut registry = crate::workspace_registry::WorkspaceRegistry::default();
    registry.add(dir.path(), None).unwrap();
    registry
        .save(&std::path::PathBuf::from(
            std::env::var(crate::workspace_registry::REGISTRY_PATH_ENV).unwrap(),
        ))
        .unwrap();

    let socket = dir.path().join("safehoused.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    // Expects one send; the assertion below is that this never resolves
    // within the timeout — i.e. the backlog is never narrated at all.
    let server = tokio::spawn(stub_server(listener, false, 1));

    let bus = Arc::new(EventBus::new());
    let subscription = bus.subscribe(Vec::<String>::new());
    let sink = tokio::spawn(run_sink(
        SafehouseConfig {
            enabled: true,
            socket: Some(socket.clone()),
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

    // Comfortably longer than several reconciliation ticks at the 20ms
    // cadence above.
    let outcome = tokio::time::timeout(Duration::from_millis(400), server).await;
    assert!(
        outcome.is_err(),
        "the seed-only first pass must never narrate a backlog merge, on this tick or any \
             later one"
    );

    drop(bus);
    let _ = tokio::time::timeout(Duration::from_secs(2), sink).await;
    std::env::remove_var(GH_BIN_ENV);
    std::env::remove_var(RECONCILE_INTERVAL_ENV);

    let completions_path = dir.path().join("safehouse-completed.json");
    let persisted = load_persisted_completed(Some(&completions_path));
    // `run_sink` keys the dedup set by the *registry* root, which
    // `WorkspaceRegistry::add` stores as `normalize_path(root)`. Normalize
    // the expected key the same way so a raw `tempdir().path()` (which can
    // differ from its canonical form, e.g. macOS `/var/folders` ->
    // `/private/var/folders`) still compares equal.
    let root = normalize_path(dir.path()).to_string_lossy().into_owned();
    assert!(
        persisted.contains(&(root.clone(), 4610, 4610)) && persisted.contains(&(root, 4611, 4611)),
        "the seed pass must still persist both backlog merges into the dedup set, so a \
             later daemon restart does not narrate them either: {persisted:?}"
    );
}

#[tokio::test]
#[serial]
async fn run_sink_reconciliation_narrates_normally_once_the_dedup_file_is_no_longer_fresh() {
    // #4649, the other half: once a workspace's one-time seed-only pass
    // has run (or the persisted file was never fresh to begin with), a
    // *new* merge must still reach the wire — no regression to the
    // steady-state behavior added by #4646.
    let dir = tempfile::tempdir().unwrap();
    let _safehouse_test_paths = SafehouseTestPaths::set(dir.path());
    let (fake_gh, _log) =
        write_fake_forge_gh(dir.path(), Some(&reconcile_merged_pr_json()), Some("rjwalters/loom"));
    std::env::set_var(GH_BIN_ENV, &fake_gh);
    std::env::set_var(RECONCILE_INTERVAL_ENV, "20");
    let mut registry = crate::workspace_registry::WorkspaceRegistry::default();
    registry.add(dir.path(), None).unwrap();
    registry
        .save(&std::path::PathBuf::from(
            std::env::var(crate::workspace_registry::REGISTRY_PATH_ENV).unwrap(),
        ))
        .unwrap();

    let socket = dir.path().join("safehoused.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(stub_server(listener, false, 1));

    let bus = Arc::new(EventBus::new());
    let subscription = bus.subscribe(Vec::<String>::new());
    let sink = tokio::spawn(run_sink(
        SafehouseConfig {
            enabled: true,
            socket: Some(socket.clone()),
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

    // Wait for the seed-only first tick to persist issue #4610 without
    // narrating it.
    let completions_path = dir.path().join("safehouse-completed.json");
    // See the sibling seed-pass test: the persisted key is the normalized
    // registry root, so normalize the tempdir path the same way rather
    // than comparing against the (possibly uncanonicalized) raw path.
    let root = normalize_path(dir.path()).to_string_lossy().into_owned();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if load_persisted_completed(Some(&completions_path)).contains(&(
                root.clone(),
                4610,
                4610,
            )) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the seed-only first pass must persist issue #4610 within the timeout");

    // Now a second, distinct merge lands — the fake `gh` is rewritten in
    // place (same script path, same GH_BIN_ENV) to answer with both the
    // already-seeded issue and the new one.
    write_fake_forge_gh(dir.path(), Some(&reconcile_two_merged_prs_json()), Some("rjwalters/loom"));

    let received = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("a merge landing after the seed-only pass must still be narrated")
        .unwrap();

    drop(bus);
    let _ = tokio::time::timeout(Duration::from_secs(2), sink).await;
    std::env::remove_var(GH_BIN_ENV);
    std::env::remove_var(RECONCILE_INTERVAL_ENV);

    assert_eq!(received.len(), 1);
    assert_eq!(received[0]["type"], json!("completion"));
    assert_eq!(
        received[0]["meta"]["issue"],
        json!(4611),
        "only the not-already-seeded issue is narrated; #4610 stays suppressed"
    );
}

#[test]
fn persisted_dedup_state_is_fresh_treats_absent_and_corrupt_files_as_fresh() {
    let dir = tempfile::tempdir().unwrap();
    assert!(
        persisted_dedup_state_is_fresh(Some(&dir.path().join("does-not-exist.json"))),
        "an absent file (fresh install, lost file) has no reliable prior state"
    );

    let corrupt = dir.path().join("corrupt.json");
    std::fs::write(&corrupt, "not valid json").unwrap();
    assert!(
        persisted_dedup_state_is_fresh(Some(&corrupt)),
        "a corrupt file is indistinguishable from no prior state"
    );

    assert!(
        persisted_dedup_state_is_fresh(None),
        "no resolvable path (no home dir) also has no reliable prior state"
    );
}

#[test]
fn persisted_dedup_state_is_fresh_is_false_for_a_valid_even_if_empty_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("safehouse-completed.json");

    // A host that has already reconciled at least once, with zero
    // completions recorded, must not be re-treated as fresh on the next
    // restart — that would otherwise re-suppress narration forever.
    persist_completed_best_effort(Some(&path), &std::collections::HashSet::new());
    assert!(!persisted_dedup_state_is_fresh(Some(&path)));

    let mut completed = std::collections::HashSet::new();
    completed.insert(("/home/x/GitHub/loom".to_owned(), 4610, 815));
    persist_completed_best_effort(Some(&path), &completed);
    assert!(!persisted_dedup_state_is_fresh(Some(&path)));
}

#[tokio::test]
#[serial]
async fn reconcile_recent_merges_does_not_repost_a_merge_loaded_from_a_persisted_restart_file() {
    // AC3, the other half: a merge already narrated by a *previous* daemon
    // process must not be re-posted by a fresh process's reconciliation
    // pass just because its in-memory `completed` set starts empty —
    // `load_persisted_completed` (exactly what `run_sink` calls at
    // startup) is what makes that survive the restart.
    let dir = tempfile::tempdir().unwrap();
    let (fake_gh, _log) =
        write_fake_forge_gh(dir.path(), Some(&reconcile_merged_pr_json()), Some("rjwalters/loom"));
    std::env::set_var(GH_BIN_ENV, &fake_gh);

    let root = dir.path().to_string_lossy().into_owned();
    let completions_path = dir.path().join("safehouse-completed.json");
    let mut pre_seeded = std::collections::HashSet::new();
    pre_seeded.insert((root.clone(), 4610, 4610));
    persist_completed_best_effort(Some(&completions_path), &pre_seeded);

    // Simulate the fresh process's startup load.
    let mut completed = load_persisted_completed(Some(&completions_path));
    let mut slug_cache = HashMap::new();
    let envelopes =
        reconcile_recent_merges("loom_daemon", &root, &mut slug_cache, &mut completed, None, None)
            .await;

    std::env::remove_var(GH_BIN_ENV);
    assert!(
        envelopes.is_empty(),
        "a merge already narrated pre-restart must not be re-posted after loading the \
             persisted dedup set"
    );
}

#[tokio::test]
#[serial]
async fn run_sink_narrates_exit_ack_then_completion() {
    // The emit-point mapping test: one `SweepExited` whose PR merged
    // produces the human `ack` and exactly one public-feed `completion`.
    let dir = tempfile::tempdir().unwrap();
    let _safehouse_test_paths = SafehouseTestPaths::set(dir.path());
    let (fake_gh, _log) =
        write_fake_forge_gh(dir.path(), Some(MERGED_PR_JSON), Some("rjwalters/loom"));
    std::env::set_var(GH_BIN_ENV, &fake_gh);

    let socket = dir.path().join("safehoused.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(stub_server(listener, false, 2));

    let bus = Arc::new(EventBus::new());
    let subscription = bus.subscribe(Vec::<String>::new());
    let sink = tokio::spawn(run_sink(
        SafehouseConfig {
            enabled: true,
            socket: Some(socket.clone()),
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

    bus.publish(Event::SweepExited {
        issue: 4426,
        exit_code: Some(0),
        duration_sec: 750,
        no_progress: false,
        death_class: None,
        repo: Some(dir.path().to_string_lossy().into_owned()),
    })
    .unwrap();

    let received = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("stub server must receive the ack and the completion")
        .unwrap();

    drop(bus);
    let _ = tokio::time::timeout(Duration::from_secs(2), sink).await;
    std::env::remove_var(GH_BIN_ENV);

    assert_eq!(received.len(), 2);
    assert_eq!(received[0]["type"], json!("ack"));
    assert!(received[0].get("meta").is_none(), "an ack carries no meta");
    assert_eq!(received[1]["type"], json!("completion"));
    assert_eq!(received[1]["meta"]["schema"], json!("completion-v1"));
    assert_eq!(received[1]["meta"]["repo"], json!("rjwalters/loom"));
    assert_eq!(received[1]["meta"]["result"], json!("success"));
    assert_eq!(received[1]["meta"]["issue"], json!(4426));
    // The #4497 display fields make it all the way onto the wire, where
    // safehoused's egress redacts and publishes them.
    assert_eq!(received[1]["meta"]["title"], json!("feat: enrich completion meta"));
    assert_eq!(received[1]["meta"]["additions"], json!(214));
    assert_eq!(received[1]["meta"]["deletions"], json!(37));
    // Both lines thread together under the repo-qualified task_id.
    assert_eq!(received[0]["task_id"], received[1]["task_id"]);
    assert!(received[1]["body"].as_str().unwrap().contains("merged ✓"));
}

#[tokio::test]
#[serial]
async fn run_sink_narrates_only_the_ack_when_nothing_merged() {
    let dir = tempfile::tempdir().unwrap();
    let _safehouse_test_paths = SafehouseTestPaths::set(dir.path());
    let (fake_gh, _log) = write_fake_forge_gh(dir.path(), Some("[]"), Some("rjwalters/loom"));
    std::env::set_var(GH_BIN_ENV, &fake_gh);

    let socket = dir.path().join("safehoused.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(stub_server(listener, false, 1));

    let bus = Arc::new(EventBus::new());
    let subscription = bus.subscribe(Vec::<String>::new());
    let sink = tokio::spawn(run_sink(
        SafehouseConfig {
            enabled: true,
            socket: Some(socket.clone()),
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

    bus.publish(Event::SweepExited {
        issue: 4426,
        exit_code: Some(1),
        duration_sec: 90,
        no_progress: false,
        death_class: None,
        repo: Some(dir.path().to_string_lossy().into_owned()),
    })
    .unwrap();

    let received = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("stub server must receive the failure ack")
        .unwrap();

    drop(bus);
    let _ = tokio::time::timeout(Duration::from_secs(2), sink).await;
    std::env::remove_var(GH_BIN_ENV);

    assert_eq!(received.len(), 1);
    assert_eq!(received[0]["type"], json!("ack"));
    assert!(received[0]["body"].as_str().unwrap().contains("failed ✗"));
}

// ---- integration: stub AF_UNIX socket ----

/// Minimal stub safehoused: accept one connection, read the `hello`, reply
/// `{"ok":true}`, then for each `send` optionally emit an interleaved push
/// line (no id) before the id-echoed reply. Returns received `send` bodies.
async fn stub_server(
    listener: UnixListener,
    interleave_push: bool,
    expected_sends: usize,
) -> Vec<Value> {
    let (stream, _) = listener.accept().await.unwrap();
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    // hello
    let hello: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(hello["op"], json!("hello"), "first request must be hello");
    write_half
        .write_all(b"{\"ok\":true,\"id\":0}\n")
        .await
        .unwrap();

    let mut received = Vec::new();
    for _ in 0..expected_sends {
        let Some(line) = lines.next_line().await.unwrap() else {
            break;
        };
        let req: Value = serde_json::from_str(&line).unwrap();
        let id = req["id"].clone();
        received.push(req.clone());
        if interleave_push {
            // An async inbound room event: has `event`, no `id`. The client
            // must skip this and still match the reply below.
            write_half
                .write_all(b"{\"event\":\"message\",\"body\":\"hello human\"}\n")
                .await
                .unwrap();
        }
        let reply = json!({"ok": true, "event_id": "$evt", "id": id});
        write_half
            .write_all(format!("{reply}\n").as_bytes())
            .await
            .unwrap();
    }
    received
}

#[tokio::test]
async fn client_hello_send_and_push_demux() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("safehoused.sock");
    let listener = UnixListener::bind(&socket).unwrap();

    let server = tokio::spawn(stub_server(listener, true, 1));

    let mut client = SafehouseClient::connect(&socket, "loom_daemon", None)
        .await
        .unwrap();
    client
        .send(&Envelope {
            to: "*".to_owned(),
            kind: "task".to_owned(),
            task_id: Some("42".to_owned()),
            body: "issue #42 → builder".to_owned(),
            meta: None,
        })
        .await
        .expect("send must succeed despite the interleaved push line");

    let received = server.await.unwrap();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0]["op"], json!("send"));
    assert_eq!(received[0]["v"], json!(1));
    assert_eq!(received[0]["task_id"], json!("42"));
    assert!(received[0].get("from").is_none());
}

/// #4464: a protocol rejection (`ok:false`) is surfaced as the typed
/// [`SendError::Rejected`] carrying safehoused's raw reason — the sink can
/// tell it from a transport failure without string-matching an untyped
/// error chain.
#[tokio::test]
async fn send_rejection_is_typed_and_names_the_reason() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("safehoused.sock");
    let listener = UnixListener::bind(&socket).unwrap();

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read_half, mut write_half) = stream.into_split();
        let mut lines = BufReader::new(read_half).lines();
        // hello → ok
        let _ = lines.next_line().await.unwrap().unwrap();
        write_half
            .write_all(b"{\"ok\":true,\"id\":0}\n")
            .await
            .unwrap();
        // send → rejected with the canonical multi-room reason
        let req: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        let id = req["id"].clone();
        write_half
            .write_all(
                format!(
                    "{{\"ok\":false,\"error\":\"'room' required: 3 rooms joined\",\"id\":{id}}}\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        // Hold the connection open so the client observes the reply (a
        // rejection, not an EOF), which is the whole point of the split.
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    let mut client = SafehouseClient::connect(&socket, "loom_daemon", None)
        .await
        .unwrap();
    let err = client
        .send(&Envelope {
            to: "*".to_owned(),
            kind: "task".to_owned(),
            task_id: Some("42".to_owned()),
            body: "issue #42 → builder".to_owned(),
            meta: None,
        })
        .await
        .expect_err("a rejected send must be an error");
    match err {
        SendError::Rejected { reason } => {
            assert!(
                reason.contains("'room' required"),
                "reason must carry safehoused's raw error, got: {reason}"
            );
        }
        SendError::Transport(e) => panic!("expected Rejected, got Transport: {e:#}"),
    }
    server.abort();
}

/// #4464: a transport-level failure (peer closes mid-send) is surfaced as
/// [`SendError::Transport`], NOT `Rejected` — the sink must reconnect
/// rather than stick on a rejection diagnosis.
#[tokio::test]
async fn send_transport_failure_is_typed_transport() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("safehoused.sock");
    let listener = UnixListener::bind(&socket).unwrap();

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read_half, mut write_half) = stream.into_split();
        let mut lines = BufReader::new(read_half).lines();
        // hello → ok, then drop the connection: the send's reply read hits EOF.
        let _ = lines.next_line().await.unwrap().unwrap();
        write_half
            .write_all(b"{\"ok\":true,\"id\":0}\n")
            .await
            .unwrap();
        drop(write_half);
        drop(lines);
    });

    let mut client = SafehouseClient::connect(&socket, "loom_daemon", None)
        .await
        .unwrap();
    // The server has (or will imminently) close the connection after the
    // hello; the send's reply read then hits EOF.
    server.await.unwrap();
    let err = client
        .send(&Envelope {
            to: "*".to_owned(),
            kind: "task".to_owned(),
            task_id: Some("42".to_owned()),
            body: "body".to_owned(),
            meta: None,
        })
        .await
        .expect_err("a closed connection must fail the send");
    assert!(
        matches!(err, SendError::Transport(_)),
        "a closed connection is a transport failure, got: {err:?}"
    );
}

/// #4464: the sink reports `send_rejected` (with the reason) rather than
/// `unreachable` when safehoused rejects the send, and clears back to
/// `connected` once a send is accepted (config fixed + daemon restarted).
#[tokio::test]
#[serial]
async fn sink_send_rejection_reports_send_rejected_then_clears_on_success() {
    let dir = tempfile::tempdir().unwrap();
    let _safehouse_test_paths = SafehouseTestPaths::set(dir.path());
    let socket = dir.path().join("safehoused.sock");
    let listener = UnixListener::bind(&socket).unwrap();

    // hello → ok; send 1 → rejected; send 2 → accepted.
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read_half, mut write_half) = stream.into_split();
        let mut lines = BufReader::new(read_half).lines();
        let _ = lines.next_line().await.unwrap().unwrap(); // hello
        write_half
            .write_all(b"{\"ok\":true,\"id\":0}\n")
            .await
            .unwrap();
        // send 1 → reject
        let r1: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        let id1 = r1["id"].clone();
        write_half
            .write_all(
                format!(
                    "{{\"ok\":false,\"error\":\"'room' required: 3 rooms joined\",\"id\":{id1}}}\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        // send 2 → accept (the operator set safehouse.room + restarted, in
        // effect — here we just accept to exercise the clear-on-success arm)
        let r2: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        let id2 = r2["id"].clone();
        write_half
            .write_all(format!("{{\"ok\":true,\"event_id\":\"$e\",\"id\":{id2}}}\n").as_bytes())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
    });

    let bus = Arc::new(EventBus::new());
    let subscription = bus.subscribe(Vec::<String>::new());
    let state = new_shared_state();
    let sink = tokio::spawn(run_sink(
        SafehouseConfig {
            enabled: true,
            socket: Some(socket.clone()),
            ..SafehouseConfig::default()
        },
        socket.clone(),
        subscription,
        Duration::from_millis(50),
        Duration::from_millis(200),
        state.clone(),
        None,
        None,
    ));

    // Event 1 → rejected → send_rejected (with reason), NOT unreachable.
    let _ = bus.publish(Event::SweepPhase {
        issue: 1,
        phase: "builder".to_owned(),
        pr_number: None,
        repo: None,
    });
    let s = wait_for_state(&state, |s| matches!(s, SafehouseState::SendRejected { .. })).await;
    match s {
        SafehouseState::SendRejected { socket: sk, reason } => {
            assert_eq!(sk, socket);
            assert!(reason.contains("'room' required"), "reason: {reason}");
        }
        other => panic!("expected SendRejected, got {other:?}"),
    }

    // Event 2 → accepted → clears back to connected.
    let _ = bus.publish(Event::SweepPhase {
        issue: 2,
        phase: "builder".to_owned(),
        pr_number: None,
        repo: None,
    });
    let s = wait_for_state(&state, |s| matches!(s, SafehouseState::Connected { .. })).await;
    assert!(matches!(s, SafehouseState::Connected { .. }), "got {s:?}");

    drop(bus);
    let _ = tokio::time::timeout(Duration::from_secs(2), sink).await;
    server.abort();
}

/// #4464: the send-rejected diagnosis is sticky across a reconnect — a
/// fresh `hello` after a transport blip must NOT flash "connected"; only an
/// accepted send clears it.
#[tokio::test]
#[serial]
async fn sink_send_rejected_survives_reconnect() {
    let dir = tempfile::tempdir().unwrap();
    let _safehouse_test_paths = SafehouseTestPaths::set(dir.path());
    let socket = dir.path().join("safehoused.sock");
    let listener = UnixListener::bind(&socket).unwrap();

    let server = tokio::spawn(async move {
        // Connection 1: hello ok, reject send 1, then close (transport drop).
        let (stream, _) = listener.accept().await.unwrap();
        let (read_half, mut write_half) = stream.into_split();
        let mut lines = BufReader::new(read_half).lines();
        let _ = lines.next_line().await.unwrap().unwrap(); // hello
        write_half
            .write_all(b"{\"ok\":true,\"id\":0}\n")
            .await
            .unwrap();
        let r1: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        let id1 = r1["id"].clone();
        write_half
            .write_all(
                format!(
                    "{{\"ok\":false,\"error\":\"'room' required: 3 rooms joined\",\"id\":{id1}}}\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        drop(write_half);
        drop(lines);

        // Connection 2: hello ok, then stall before replying to the send so
        // the sink's state is observable at "just reconnected, no send
        // accepted yet" — it must be SendRejected, not Connected.
        let (stream, _) = listener.accept().await.unwrap();
        let (read_half, mut write_half) = stream.into_split();
        let mut lines = BufReader::new(read_half).lines();
        let _ = lines.next_line().await.unwrap().unwrap(); // hello
        write_half
            .write_all(b"{\"ok\":true,\"id\":0}\n")
            .await
            .unwrap();
        let _ = lines.next_line().await.unwrap().unwrap(); // send (read, do not reply yet)
        tokio::time::sleep(Duration::from_millis(400)).await;
    });

    let bus = Arc::new(EventBus::new());
    let subscription = bus.subscribe(Vec::<String>::new());
    let state = new_shared_state();
    let sink = tokio::spawn(run_sink(
        SafehouseConfig {
            enabled: true,
            socket: Some(socket.clone()),
            ..SafehouseConfig::default()
        },
        socket.clone(),
        subscription,
        Duration::from_millis(30),
        Duration::from_millis(60),
        state.clone(),
        None,
        None,
    ));

    // Event 1 → conn1 rejects → SendRejected.
    let _ = bus.publish(Event::SweepPhase {
        issue: 1,
        phase: "builder".to_owned(),
        pr_number: None,
        repo: None,
    });
    wait_for_state(&state, |s| matches!(s, SafehouseState::SendRejected { .. })).await;

    // Event 2 → send on the now-closed conn1 fails (transport) → Unreachable.
    let _ = bus.publish(Event::SweepPhase {
        issue: 2,
        phase: "builder".to_owned(),
        pr_number: None,
        repo: None,
    });
    wait_for_state(&state, |s| matches!(s, SafehouseState::Unreachable { .. })).await;

    // Event 3 → reconnect (conn2 hello ok); the send stalls server-side so
    // the state settles at the reconnect value. Stickiness ⇒ SendRejected.
    tokio::time::sleep(Duration::from_millis(80)).await; // clear the backoff window
    let _ = bus.publish(Event::SweepPhase {
        issue: 3,
        phase: "builder".to_owned(),
        pr_number: None,
        repo: None,
    });
    let s = wait_for_state(&state, |s| matches!(s, SafehouseState::SendRejected { .. })).await;
    assert!(
        matches!(s, SafehouseState::SendRejected { .. }),
        "a reconnect whose hello succeeds must NOT clear the send-rejected \
             diagnosis, got {s:?}"
    );

    drop(bus);
    let _ = tokio::time::timeout(Duration::from_secs(2), sink).await;
    server.abort();
}

/// Poll `state` until `pred` holds (or panic after ~1s) — test helper for
/// the lazily-connecting sink whose transitions are not synchronous with
/// `bus.publish`.
async fn wait_for_state(
    state: &SharedSafehouseState,
    pred: impl Fn(&SafehouseState) -> bool,
) -> SafehouseState {
    for _ in 0..100 {
        let s = snapshot_state(state);
        if pred(&s) {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let s = snapshot_state(state);
    panic!("state never satisfied predicate; last = {s:?}");
}

#[tokio::test]
async fn disabled_config_does_not_subscribe() {
    let bus = EventBus::new();
    assert_eq!(bus.receiver_count(), 0);
    let state = new_shared_state();
    let handle = spawn_sink(
        SafehouseConfig::default(), // disabled
        &bus,
        &tokio::runtime::Handle::current(),
        state.clone(),
        None,
        None,
    );
    assert!(handle.is_none(), "disabled ⇒ no sink task");
    // The load-bearing no-op assertion: no subscription was created.
    assert_eq!(bus.receiver_count(), 0, "disabled ⇒ no bus subscription");
    // #4345: disabled must report as "not configured", never silence.
    assert_eq!(snapshot_state(&state), SafehouseState::NotConfigured);
}

#[tokio::test]
#[serial]
async fn absent_peer_degrades_without_blocking() {
    // enabled + nonexistent socket: the sink subscribes, but every connect
    // fails and is swallowed — publishing never blocks or errors.
    let dir = tempfile::tempdir().unwrap();
    let _safehouse_test_paths = SafehouseTestPaths::set(dir.path());
    let socket = dir.path().join("does-not-exist.sock");
    let bus = Arc::new(EventBus::new());
    let subscription = bus.subscribe(Vec::<String>::new());

    let state = new_shared_state();
    let sink = tokio::spawn(run_sink(
        SafehouseConfig {
            enabled: true,
            socket: Some(socket),
            ..SafehouseConfig::default()
        },
        PathBuf::from("/nonexistent/safehoused.sock"),
        subscription,
        Duration::from_millis(50),
        Duration::from_millis(200),
        state.clone(),
        None,
        None,
    ));

    // Publish a burst; the sink must consume them all without wedging.
    for issue in 0..5u32 {
        let _ = bus.publish(Event::SweepPhase {
            issue,
            phase: "builder".to_owned(),
            pr_number: None,
            repo: None,
        });
    }
    // Give the sink a moment to drain, then drop the bus to close the sub.
    tokio::time::sleep(Duration::from_millis(100)).await;
    // #4345: an absent peer must report "unreachable" (with the resolved
    // socket path), never silence and never a stale "not configured".
    match snapshot_state(&state) {
        SafehouseState::Unreachable { socket } => {
            assert_eq!(socket, PathBuf::from("/nonexistent/safehoused.sock"));
        }
        other => panic!("expected Unreachable, got {other:?}"),
    }
    drop(bus);
    // The sink must exit cleanly once the bus closes (it never blocked).
    tokio::time::timeout(Duration::from_secs(2), sink)
        .await
        .expect("sink must terminate after bus close")
        .unwrap();
}

#[tokio::test]
#[serial]
async fn reconnects_after_mid_run_disconnect() {
    let dir = tempfile::tempdir().unwrap();
    let _safehouse_test_paths = SafehouseTestPaths::set(dir.path());
    let socket = dir.path().join("safehoused.sock");

    // First listener: serve one send, then drop (simulating a restart).
    let listener1 = UnixListener::bind(&socket).unwrap();
    let server1 = tokio::spawn(stub_server(listener1, false, 1));

    let bus = Arc::new(EventBus::new());
    let subscription = bus.subscribe(Vec::<String>::new());
    let state = new_shared_state();
    let sink = tokio::spawn(run_sink(
        SafehouseConfig {
            enabled: true,
            socket: Some(socket.clone()),
            ..SafehouseConfig::default()
        },
        socket.clone(),
        subscription,
        Duration::from_millis(20),
        Duration::from_millis(80),
        state.clone(),
        None,
        None,
    ));

    // First event: delivered over listener1.
    bus.publish(Event::SweepPhase {
        issue: 1,
        phase: "builder".to_owned(),
        pr_number: None,
        repo: None,
    })
    .unwrap();
    let first = tokio::time::timeout(Duration::from_secs(2), server1)
        .await
        .expect("first server should receive one send")
        .unwrap();
    assert_eq!(first.len(), 1);

    // listener1 is dropped now; the client's next send will fail. Rebind a
    // fresh listener on the same path (the "restart") — a real daemon
    // restart unlinks the stale socket file first, so mirror that here.
    std::fs::remove_file(&socket).ok();
    let listener2 = UnixListener::bind(&socket).unwrap();
    let server2 = tokio::spawn(stub_server(listener2, false, 1));

    // Publish more events until one lands on listener2 (the first may hit
    // the dead connection and be dropped; the sink reconnects with backoff).
    let sink_done = tokio::spawn(async move {
        for issue in 2..40u32 {
            let _ = bus.publish(Event::SweepPhase {
                issue,
                phase: "judge".to_owned(),
                pr_number: None,
                repo: None,
            });
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        drop(bus);
    });

    let second = tokio::time::timeout(Duration::from_secs(5), server2)
        .await
        .expect("sink must reconnect and deliver to the second server")
        .unwrap();
    assert!(!second.is_empty(), "a narration must land post-reconnect");
    // #4345: the reconnect must be visible as Connected again, not stuck
    // reporting the mid-outage Unreachable value.
    match snapshot_state(&state) {
        SafehouseState::Connected { socket: s, .. } => assert_eq!(s, socket),
        other => panic!("expected Connected after reconnect, got {other:?}"),
    }

    sink_done.await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(2), sink).await;
}

// ---- peer-claim coordination (#4028) ----

#[test]
fn claim_ad_serializes_to_a_valid_task_envelope() {
    // Envelope-validity AC: the advertisement must serialize to a `type`
    // within KNOWN_TYPES and a `task_id` matching [A-Za-z0-9_], asserted
    // against build_send_request so a rejected-by-safehoused envelope fails
    // at test time, not runtime.
    let ad = ClaimAd::advertise(4028, "loom".into(), "maple".into(), 7, "ts".into());
    let env = claim_ad_to_envelope(&ad);
    assert_eq!(env.kind, "task");
    assert!(KNOWN_TYPES.contains(&env.kind.as_str()));
    assert_eq!(env.task_id.as_deref(), Some("4028"));

    let req = build_send_request(&env, 1, Some("fleet")).unwrap();
    assert_eq!(req["type"], json!("task"));
    assert_eq!(req["task_id"], json!("4028"));
    // The body round-trips back to the same claim on the receive side.
    let body = req["body"].as_str().unwrap();
    assert_eq!(ClaimAd::from_body_str(body), Some(ad));
}

#[test]
fn peer_claim_sink_reads_envelope_body_from_safehoused_push_shape() {
    // #6249 regression: this push line is derived from safehoused's actual
    // dispatch format (`main.rs` `on_message` builds
    // `json!({"event":"message", ..., "envelope": env})`, and its `Envelope`
    // serde shape carries the message text as `body`, with `kind` renamed to
    // `type`) — NOT from the sink's expectation. The sink previously read
    // only a top-level `body` that this shape never carries, so every
    // cross-host claim was silently dropped (`received=0` fleet-wide).
    let ad = ClaimAd::advertise(6249, "loom".into(), "peer-host".into(), 7, "ts".into());
    let push = json!({
        "event": "message",
        "room_id": "!x:example.org",
        "room_name": "loom-claims",
        "sender": "@bot:example.org",
        "event_id": "$e",
        "envelope": {
            "v": 1,
            "from": "loom_daemon",
            "to": "*",
            "type": "task",
            "task_id": "6249",
            "body": ad.to_body_json(),
        },
    });

    let view = Arc::new(Mutex::new(PeerClaimView::new("me".into(), Duration::from_secs(120))));
    let sink = PeerClaimSink::new(view.clone());
    sink.on_event(&push);

    assert!(
        view.lock()
            .unwrap()
            .is_claimed_at("loom", 6249, Instant::now()),
        "claim carried in envelope.body must fold into the PeerClaimView"
    );
}

#[test]
fn peer_claim_sink_still_reads_legacy_top_level_body() {
    // #6249 fallback: a flat `{"event":"message","body":...}` line (any
    // legacy emitter of the pre-envelope shape) must keep parsing.
    let ad = ClaimAd::advertise(6250, "loom".into(), "peer-host".into(), 7, "ts".into());
    let push = json!({
        "event": "message",
        "from": "loom_daemon",
        "body": ad.to_body_json(),
    });

    let view = Arc::new(Mutex::new(PeerClaimView::new("me".into(), Duration::from_secs(120))));
    let sink = PeerClaimSink::new(view.clone());
    sink.on_event(&push);

    assert!(
        view.lock()
            .unwrap()
            .is_claimed_at("loom", 6250, Instant::now()),
        "claim carried in a legacy top-level body must still fold into the view"
    );
}

#[tokio::test]
async fn disabled_config_spawns_no_coordination_task() {
    // Byte-for-byte no-op AC: safehouse.enabled=false ⇒ no coordination task
    // and no socket.
    let view = Arc::new(Mutex::new(PeerClaimView::new("me".into(), Duration::from_secs(1))));
    let sink: Arc<dyn InboundEventSink> = Arc::new(PeerClaimSink::new(view));
    let (_tx, rx) = tokio::sync::mpsc::channel::<ClaimAd>(1);
    let state = new_shared_state();
    let handle = spawn_peer_coordination(
        SafehouseConfig::default(), // disabled
        sink,
        rx,
        &tokio::runtime::Handle::current(),
        state.clone(),
    );
    assert!(handle.is_none(), "disabled ⇒ no coordination task");
    // #4345: disabled must report as "not configured".
    assert_eq!(snapshot_state(&state), SafehouseState::NotConfigured);
}

#[tokio::test]
async fn idle_daemon_still_reads_inbound_peer_ad() {
    // The core regression Gap 1a exists to fix: a daemon that emits NOTHING
    // must still observe an inbound peer advertisement. A read_reply-piggyback
    // implementation only reads while sending, so it would never see this.
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("safehoused.sock");
    let listener = UnixListener::bind(&socket).unwrap();

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read_half, mut write_half) = stream.into_split();
        let mut lines = BufReader::new(read_half).lines();
        let hello: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(hello["op"], json!("hello"));
        write_half
            .write_all(b"{\"ok\":true,\"id\":0}\n")
            .await
            .unwrap();
        // Unprompted inbound room event carrying a peer claim — the client
        // sends nothing.
        let claim = ClaimAd::advertise(4028, "loom".into(), "peer".into(), 5, "ts".into());
        let push = json!({"event": "message", "from": "loom_daemon", "body": claim.to_body_json()});
        write_half
            .write_all(format!("{push}\n").as_bytes())
            .await
            .unwrap();
        // Hold the connection open so the client can read the push.
        tokio::time::sleep(Duration::from_millis(300)).await;
    });

    let view = Arc::new(Mutex::new(PeerClaimView::new("me".into(), Duration::from_secs(120))));
    let sink: Arc<dyn InboundEventSink> = Arc::new(PeerClaimSink::new(view.clone()));
    let (_tx, rx) = tokio::sync::mpsc::channel::<ClaimAd>(8); // held → task stays alive; never send (idle)

    let state = new_shared_state();
    let task = tokio::spawn(run_coordination(
        SafehouseConfig {
            enabled: true,
            socket: Some(socket.clone()),
            ..SafehouseConfig::default()
        },
        socket.clone(),
        sink,
        rx,
        Duration::from_millis(20),
        Duration::from_millis(80),
        state.clone(),
    ));

    // Poll the view for the claim (bounded condition-poll, not a fixed
    // ordering sleep).
    let mut seen = false;
    for _ in 0..50 {
        if view
            .lock()
            .unwrap()
            .is_claimed_at("loom", 4028, Instant::now())
        {
            seen = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(seen, "an idle daemon must still observe the inbound peer claim (Gap 1a)");
    // #4345: a live, idle coordination connection must report Connected.
    match snapshot_state(&state) {
        SafehouseState::Connected { socket: s, .. } => assert_eq!(s, socket),
        other => panic!("expected Connected, got {other:?}"),
    }
    server.await.unwrap();
    task.abort();
}

/// #4225's resolved open question: claim ads are per-repo `task` chatter, but
/// they are advertised into the **signal room** as a deliberate exception —
/// it is the only room every host's bot is guaranteed to be joined to, and
/// cross-host dedup is a correctness property (see `run_coordination`'s doc
/// comment). The reader is trivially consistent with that choice because it
/// consumes any inbound claim regardless of room.
#[tokio::test]
async fn coordination_advertises_claim_ads_into_the_signal_room() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("safehoused.sock");
    let listener = UnixListener::bind(&socket).unwrap();

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read_half, mut write_half) = stream.into_split();
        let mut lines = BufReader::new(read_half).lines();
        let hello: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(hello["op"], json!("hello"));
        write_half
            .write_all(b"{\"ok\":true,\"id\":0}\n")
            .await
            .unwrap();
        let ad: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        let reply = json!({"ok": true, "id": ad["id"].clone()});
        write_half
            .write_all(format!("{reply}\n").as_bytes())
            .await
            .unwrap();
        ad
    });

    let view = Arc::new(Mutex::new(PeerClaimView::new("me".into(), Duration::from_secs(120))));
    let sink: Arc<dyn InboundEventSink> = Arc::new(PeerClaimSink::new(view));
    let (tx, rx) = tokio::sync::mpsc::channel::<ClaimAd>(8);

    let task = tokio::spawn(run_coordination(
        SafehouseConfig {
            enabled: true,
            socket: Some(socket.clone()),
            // Routing mode with the legacy scalar `room` unset — exactly the
            // configuration in which an ad would be rejected outright
            // (`'room' required`) if it did not resolve the signal room.
            room: None,
            rooms: Some(RoomMap {
                signal: Some("!signal:example.org".to_owned()),
                by_repo: [("loom".to_owned(), "!fleet-loom:example.org".to_owned())]
                    .into_iter()
                    .collect(),
                claims: None,
            }),
            ..SafehouseConfig::default()
        },
        socket,
        sink,
        rx,
        Duration::from_millis(20),
        Duration::from_millis(80),
        new_shared_state(),
    ));

    tx.send(ClaimAd::advertise(4225, "loom".into(), "maple".into(), 7, "ts".into()))
        .await
        .unwrap();

    let ad = tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("the stub must receive the claim ad")
        .unwrap();
    assert_eq!(ad["op"], json!("send"));
    assert_eq!(ad["type"], json!("task"));
    assert_eq!(
        ad["room"],
        json!("!signal:example.org"),
        "claim ads ride the signal room, NOT the repo firehose (documented exception)"
    );
    task.abort();
}

/// #4713: when `rooms.claims` is configured, claim ads route into that
/// dedicated coordination room instead of the signal room — the opt-in
/// escape hatch `run_coordination`'s doc comment describes. The signal room
/// stays reserved for sweep-lifecycle narration.
#[tokio::test]
async fn coordination_advertises_claim_ads_into_a_dedicated_claims_room_when_configured() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("safehoused.sock");
    let listener = UnixListener::bind(&socket).unwrap();

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read_half, mut write_half) = stream.into_split();
        let mut lines = BufReader::new(read_half).lines();
        let hello: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(hello["op"], json!("hello"));
        write_half
            .write_all(b"{\"ok\":true,\"id\":0}\n")
            .await
            .unwrap();
        let ad: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        let reply = json!({"ok": true, "id": ad["id"].clone()});
        write_half
            .write_all(format!("{reply}\n").as_bytes())
            .await
            .unwrap();
        ad
    });

    let view = Arc::new(Mutex::new(PeerClaimView::new("me".into(), Duration::from_secs(120))));
    let sink: Arc<dyn InboundEventSink> = Arc::new(PeerClaimSink::new(view));
    let (tx, rx) = tokio::sync::mpsc::channel::<ClaimAd>(8);
    let state = new_shared_state();

    let task = tokio::spawn(run_coordination(
        SafehouseConfig {
            enabled: true,
            socket: Some(socket.clone()),
            room: None,
            rooms: Some(RoomMap {
                signal: Some("!signal:example.org".to_owned()),
                by_repo: [("loom".to_owned(), "!fleet-loom:example.org".to_owned())]
                    .into_iter()
                    .collect(),
                claims: Some("!claims:example.org".to_owned()),
            }),
            ..SafehouseConfig::default()
        },
        socket,
        sink,
        rx,
        Duration::from_millis(20),
        Duration::from_millis(80),
        state.clone(),
    ));

    tx.send(ClaimAd::advertise(4713, "loom".into(), "maple".into(), 7, "ts".into()))
        .await
        .unwrap();

    let ad = tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("the stub must receive the claim ad")
        .unwrap();
    assert_eq!(ad["op"], json!("send"));
    assert_eq!(ad["type"], json!("task"));
    assert_eq!(
        ad["room"],
        json!("!claims:example.org"),
        "with rooms.claims configured, claim ads ride the dedicated claims room, \
             not the signal room and not the repo firehose"
    );
    // The connection state (#4345) reflects the room this connection
    // actually advertises into.
    match snapshot_state(&state) {
        SafehouseState::Connected { room, .. } => {
            assert_eq!(room.as_deref(), Some("!claims:example.org"));
        }
        other => panic!("expected Connected, got {other:?}"),
    }
    task.abort();
}

#[tokio::test]
async fn coordination_reconnects_when_socket_absent_and_exits_on_sender_drop() {
    // Fail-open: an absent socket must never wedge — the task loops with
    // backoff and exits cleanly once its senders drop.
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("nope.sock"); // never bound
    let view = Arc::new(Mutex::new(PeerClaimView::new("me".into(), Duration::from_secs(1))));
    let sink: Arc<dyn InboundEventSink> = Arc::new(PeerClaimSink::new(view));
    let (tx, rx) = tokio::sync::mpsc::channel::<ClaimAd>(4);
    let state = new_shared_state();

    let task = tokio::spawn(run_coordination(
        SafehouseConfig {
            enabled: true,
            socket: Some(socket.clone()),
            ..SafehouseConfig::default()
        },
        socket.clone(),
        sink,
        rx,
        Duration::from_millis(10),
        Duration::from_millis(30),
        state.clone(),
    ));

    // Drop the only sender: the connect-fail drain loop must observe
    // Disconnected and return, so the task terminates rather than spinning.
    drop(tx);
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("coordination task must terminate after its senders drop")
        .unwrap();
    // #4345: a socket that never accepts must report Unreachable, never a
    // stale "not configured" (the config here IS enabled).
    match snapshot_state(&state) {
        SafehouseState::Unreachable { socket: s } => assert_eq!(s, socket),
        other => panic!("expected Unreachable, got {other:?}"),
    }
}
