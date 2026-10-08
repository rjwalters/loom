//! Tests for `loom-daemon status --section` (Issue #10787).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::cli::status::sample_report::sample_report;
use crate::cli::status_render::{build_status_json_value, build_status_json_value_for};
use clap::{CommandFactory, Parser};

/// `StatusArgs` on its own, so these tests parse without the whole `Cli`
/// tree (see `cli::whole_cli_parse` for why that matters).
#[derive(Debug, Parser)]
struct Status {
    #[command(flatten)]
    args: StatusArgs,
}

fn parse(argv: &[&str]) -> Result<StatusArgs, clap::Error> {
    Status::try_parse_from(std::iter::once("status").chain(argv.iter().copied())).map(|s| s.args)
}

#[test]
fn a_comma_list_and_repeated_flags_merge_into_one_selection() {
    let args = parse(&["--json", "--section", "daemon_build,auto_update"]).unwrap();
    assert_eq!(
        args.selection(),
        SectionSet::only([StatusSection::DaemonBuild, StatusSection::AutoUpdate])
    );
    let args = parse(&[
        "--json",
        "--section",
        "auto_update",
        "--section",
        "daemon_build,auto_update",
    ])
    .unwrap();
    assert_eq!(
        args.selection(),
        SectionSet::only([StatusSection::DaemonBuild, StatusSection::AutoUpdate])
    );
    assert!(parse(&["--json"]).unwrap().selection().is_all());
}

/// An unknown section is a usage error (exit 2) that names every valid one,
/// raised by the parser — before any connection to the daemon.
#[test]
fn an_unknown_section_exits_2_naming_the_valid_ones() {
    let err = parse(&["--json", "--section", "daemon_build,bogus"]).unwrap_err();
    assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
    assert_eq!(err.exit_code(), 2);
    let msg = err.to_string();
    assert!(msg.contains("bogus"), "{msg}");
    for section in StatusSection::all() {
        assert!(msg.contains(section.as_str()), "{msg} omits {section}");
    }
}

#[test]
fn section_requires_json() {
    let err = parse(&["--section", "daemon_build"]).unwrap_err();
    assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    assert_eq!(err.exit_code(), 2);
}

/// `--help` lists every section — from the same enum the parser and the
/// wire use, so the list cannot drift.
#[test]
fn help_lists_every_section() {
    let help = Status::command().render_long_help().to_string();
    assert!(help.contains("--section <SECTION>"), "{help}");
    for section in StatusSection::all() {
        assert!(help.contains(section.as_str()), "--help omits {section}");
    }
}

/// The real top-level parse routes `status --json --section …` to the
/// `Status` subcommand with the selection intact.
#[test]
fn the_binary_cli_parses_status_sections() {
    let cli = crate::cli::whole_cli_parse::try_parse_cli(&[
        "loom-daemon",
        "status",
        "--json",
        "--section",
        "daemon_build,auto_update",
    ])
    .unwrap();
    match cli.command {
        Some(crate::Commands::Status(args)) => {
            assert!(args.json);
            assert_eq!(args.section, vec![StatusSection::DaemonBuild, StatusSection::AutoUpdate]);
        }
        _ => panic!("expected the Status subcommand"),
    }
}

/// Wire forms: no `--section` sends the unchanged `{"type":"DaemonStatus"}`
/// frame; a selection sends `DaemonStatusSections` with snake_case names.
#[test]
fn the_request_is_unchanged_without_sections_and_round_trips_with_them() {
    let full = serde_json::to_string(&request(&SectionSet::all())).unwrap();
    assert_eq!(full, r#"{"type":"DaemonStatus"}"#);

    let selected = SectionSet::only([StatusSection::AutoUpdate, StatusSection::DaemonBuild]);
    let wire = serde_json::to_string(&request(&selected)).unwrap();
    assert_eq!(
        wire,
        r#"{"type":"DaemonStatusSections","payload":{"sections":["daemon_build","auto_update"]}}"#
    );
    // Naming every section is a full request: the unchanged frame again.
    let every = SectionSet::only(StatusSection::all().iter().copied());
    assert_eq!(serde_json::to_string(&request(&every)).unwrap(), full);

    match serde_json::from_str::<Request>(&wire).unwrap() {
        Request::DaemonStatusSections { sections } => {
            assert_eq!(SectionSet::only(sections), selected);
        }
        other => panic!("expected DaemonStatusSections, got {other:?}"),
    }
}

/// The parse-error frame a daemon that predates `DaemonStatusSections`
/// sends back, built the way that daemon builds it: `handle_client` fails to
/// parse the line as its (older) `Request` and replies
/// `DaemonError::ipc_parse_error`.
fn old_daemon_reply(line: &str) -> Response {
    #[derive(Debug, serde::Deserialize)]
    #[serde(tag = "type", content = "payload")]
    #[allow(dead_code)]
    enum OldRequest {
        Ping,
        DaemonStatus,
    }
    let err = serde_json::from_str::<OldRequest>(line).unwrap_err();
    Response::StructuredError(loom_daemon::errors::DaemonError::ipc_parse_error(line, &err))
}

#[test]
fn an_old_daemons_parse_error_maps_to_daemon_too_old() {
    let selected = SectionSet::only([StatusSection::DaemonBuild]);
    let line = serde_json::to_string(&request(&selected)).unwrap();
    let err = unexpected_response(old_daemon_reply(&line));
    assert!(is_daemon_too_old(&err), "{err}");
    let msg = err.to_string();
    assert!(msg.starts_with("daemon too old for --section"), "{msg}");
    assert!(msg.contains("run without --section"), "{msg}");

    let other = unexpected_response(Response::Error {
        message: "boom".to_string(),
    });
    assert!(!is_daemon_too_old(&other), "{other}");
}

/// End to end over a socket: the CLI's sectioned query against a daemon
/// that does not know the variant fails as [`DaemonTooOld`] (exit 1 in the
/// handler), not as an unreachable daemon.
#[tokio::test]
async fn a_sectioned_query_against_an_old_daemon_fails_as_too_old() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("daemon.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let line = BufReader::new(reader)
            .lines()
            .next_line()
            .await
            .unwrap()
            .unwrap();
        let reply = serde_json::to_string(&old_daemon_reply(&line)).unwrap();
        writer.write_all(reply.as_bytes()).await.unwrap();
        writer.write_all(b"\n").await.unwrap();
    });

    let timeout = crate::cli::status::resolve_status_timeout(Some(5));
    let request = request(&SectionSet::only([StatusSection::AutoUpdate]));
    let err = crate::cli::status::query_daemon_status_for(&socket, &timeout, &request)
        .await
        .unwrap_err();
    server.await.unwrap();
    assert!(is_daemon_too_old(&err), "{err}");
}

fn full_payload() -> serde_json::Value {
    let report = sample_report();
    build_status_json_value(
        &report,
        Some(&serde_json::json!({"accounts": []})),
        &self_update_status(&SectionSet::all()),
        Some(&[]),
        daemon_install_state::probe_protection().as_ref(),
        Some(&[]),
    )
}

/// **#10787 AC.** Every top-level key of the default payload belongs to a
/// section. Adding a key without registering it in `StatusSection` fails
/// here, because `--section` could never select it.
#[test]
fn every_top_level_key_belongs_to_a_section() {
    let payload = full_payload();
    for key in payload.as_object().unwrap().keys() {
        assert!(
            StatusSection::all()
                .iter()
                .any(|s| s.json_keys().contains(&key.as_str())),
            "top-level status key `{key}` is not registered in StatusSection"
        );
    }
}

/// Snapshot of the default payload's top-level shape. The three host-local
/// blocks that appear only when this host has one to report are left out.
#[test]
fn the_default_payload_keeps_its_top_level_keys() {
    let payload = full_payload();
    let keys: Vec<&str> = payload
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .filter(|k| !matches!(*k, "fleet_store" | "pending_restart" | "forge_egress"))
        .collect();
    let mut expected: Vec<&str> = StatusSection::all()
        .iter()
        .filter(|s| {
            !matches!(
                s,
                StatusSection::FleetStore
                    | StatusSection::PendingRestart
                    | StatusSection::ForgeEgress
            )
        })
        .flat_map(|s| s.json_keys().iter().copied())
        .collect();
    expected.sort_unstable();
    let mut keys = keys;
    keys.sort_unstable();
    assert_eq!(keys, expected);
}

/// The full selection is the default payload, key for key.
#[test]
fn the_full_selection_is_the_default_payload() {
    let report = sample_report();
    let update = self_update_status(&SectionSet::all());
    let usage = serde_json::json!({"accounts": []});
    let default = build_status_json_value(&report, Some(&usage), &update, None, None, None);
    let all = build_status_json_value_for(
        &report,
        Some(&usage),
        &update,
        None,
        None,
        None,
        &SectionSet::all(),
    );
    let strip = |mut v: serde_json::Value| {
        // `forge_egress.assert` re-reads the host at render time.
        v.as_object_mut().unwrap().remove("forge_egress");
        v
    };
    assert_eq!(strip(all), strip(default));
}

/// Snapshot of one cheap section (`daemon_build`) and one heavy one
/// (`per_repo`): a sectioned payload is exactly those keys of the default
/// payload, with nothing else.
#[test]
fn a_sectioned_payload_is_exactly_those_keys_of_the_default_payload() {
    let mut report = sample_report();
    report.per_repo = vec![loom_daemon::types::RepoStatus {
        root: std::path::PathBuf::from("/repo/a"),
        priority: 100,
        in_flight_count: 1,
        health_gate_halted: false,
        quarantined_issues: vec![101],
        health_gate_not_evaluated: false,
        health_gate_not_evaluated_reason: None,
        health_gate_enabled: Some(true),
        health_gate_verdict_at: None,
        root_missing: false,
        health_gate_deferred: false,
        health_gate_deferred_reason: None,
        health_gate_verdict_tier: Some("full".to_string()),
        role_runner_enabled: true,
        role_runner_roles: vec!["champion".to_string()],
        role_runner_intervals: std::collections::BTreeMap::new(),
        role_runner_on_idle_roles: vec![],
        role_runner_on_idle_promotions: vec![],
        role_runner_env_override: None,
        role_runner_shard: None,
        token_pool_dir: None,
        ranking_present: false,
        ranking_age_secs: None,
        stash_total_count: 2,
        stash_quarantine_count: 1,
        stash_oldest_age_secs: Some(60),
        stash_non_quarantine_unrecoverable_count: 0,
        stash_non_quarantine_unrecoverable_oldest_age_secs: None,
        sweep_command_missing: false,
    }];
    let update = self_update_status(&SectionSet::all());
    let full = build_status_json_value(&report, None, &update, None, None, None);
    for section in [StatusSection::DaemonBuild, StatusSection::PerRepo] {
        let only = SectionSet::only([section]);
        let got = build_status_json_value_for(&report, None, &update, None, None, None, &only);
        let key = section.as_str();
        assert_eq!(got, serde_json::json!({ key: full[key].clone() }), "{section}");
    }
    let daemon_build = &full["daemon_build"];
    assert_eq!(daemon_build["disk_commit"], self_update::BUILT_COMMIT);
    assert_eq!(full["per_repo"][0]["root"], "/repo/a");
}
