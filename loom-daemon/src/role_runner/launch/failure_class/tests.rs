//! #10640: the class, exit code and status message a launched-then-failed
//! role tick reports on its `loom.role_attempt` span.
#![allow(clippy::unwrap_used)]

use super::*;

const ANCHOR: &str = "2026-10-06T03:00:00.123Z";
const CLI_START: &str = "# LOOM_CLI_START runtime=codex";

/// One tick's region of a role log: its header line (carrying the anchor),
/// then `body`.
fn tick(body: &str) -> String {
    format!("\n==== loom-daemon role_runner: {ANCHOR} role=judge model=default ====\n{body}\n")
}

fn terminal(category: &str, code: i32) -> String {
    format!(
        "# LOOM_TERMINAL_RESULT v=2 provider=codex account=acct-1 category={category} \
         exit_code={code} model=gpt-5"
    )
}

#[cfg(unix)]
fn exit_with(code: i32) -> std::process::ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    std::process::ExitStatus::from_raw(code << 8)
}

#[test]
fn every_classifier_rung_has_its_class() {
    let cases: Vec<(String, Option<TerminalClassification>, &str)> = vec![
        // 1. The wrapper's auth pre-flight, which never reaches the CLI.
        ("# AUTH_PREFLIGHT_FAILED".into(), None, "preflight-auth-failed"),
        // 2. The sweep's pre-flight classifier.
        ("# MCP_PREFLIGHT_FAILED".into(), None, "preflight-mcp-failed"),
        (
            "spawn-claude: Token selection failed (exit 78)".into(),
            None,
            "preflight-token-selection-failed",
        ),
        ("spawn-codex: resolving account".into(), None, "preflight-no-cli-start"),
        // 3. Mid-run rotation exhaustion.
        (format!("{CLI_START}\n# ACCOUNT_POOL_EXHAUSTED"), None, "account-pool-exhausted"),
        // 4. The adapter's own verdict.
        (
            CLI_START.into(),
            Some(TerminalClassification::TokenExpired),
            "credential-expired",
        ),
        (
            CLI_START.into(),
            Some(TerminalClassification::TokenExhausted),
            "account-exhausted:token-exhausted",
        ),
        (CLI_START.into(), Some(TerminalClassification::SessionLimit), "session-limit"),
        (CLI_START.into(), Some(TerminalClassification::Fatal), "runtime-fatal"),
        // 5. The sweep's crash classifier, ending in the bare code.
        (
            format!("{CLI_START}\nYou've hit your usage limit"),
            None,
            "account-exhausted:rate-limited",
        ),
        (format!("{CLI_START}\nExecution error"), None, "execution-error"),
        (CLI_START.into(), None, "exit-1"),
        // RECOVERABLE is the adapter classifier's catch-all for an exit it did
        // not recognize: it names nothing more specific than the code.
        (CLI_START.into(), Some(TerminalClassification::Recoverable), "exit-1"),
    ];
    for (region, adapter, expected) in cases {
        assert_eq!(classify_exit(&region, adapter, 1), expected, "{region:?} / {adapter:?}");
    }
}

/// The pre-flight rungs outrank the adapter: a record that says
/// `RECOVERABLE` after an adapter that never started its CLI is still a
/// pre-flight death.
#[test]
fn a_pre_flight_death_outranks_the_adapter_verdict() {
    assert_eq!(
        classify_exit("# AUTH_PREFLIGHT_FAILED", Some(TerminalClassification::Recoverable), 1),
        "preflight-auth-failed"
    );
    assert_eq!(
        classify_exit("no cli here", Some(TerminalClassification::TokenExpired), 1),
        "preflight-no-cli-start"
    );
}

/// The 2026-10-05 loom-worker-2 shape behind most of the issue's 849
/// failures: Codex started, exited 1 in ~380 ms, and its adapter reported the
/// `RECOVERABLE` catch-all. The span now says all three facts.
#[cfg(unix)]
#[test]
fn a_fast_codex_exit_reports_its_code_and_the_adapter_verdict() {
    let log = tick(&format!(
        "{CLI_START}\nError: something codex said\n{}",
        terminal("RECOVERABLE", 1)
    ));
    let failure = exited(exit_with(1), &log, ANCHOR);
    assert_eq!(failure.class(), "exit-1");
    assert_eq!(failure.exit_code(), Some(1));
    assert_eq!(
        failure.message(),
        "role child exited with code 1; runtime adapter reported RECOVERABLE"
    );
}

/// Only this tick's own region counts: a sentinel or terminal record from an
/// earlier tick in the append-only log classifies nothing.
#[cfg(unix)]
#[test]
fn an_earlier_ticks_markers_do_not_classify_this_one() {
    let earlier = format!(
        "==== earlier tick ====\n# AUTH_PREFLIGHT_FAILED\n{}\n",
        terminal("TOKEN_EXPIRED", 1)
    );
    let log = format!("{earlier}{}", tick(&format!("{CLI_START}\nboom")));
    let failure = exited(exit_with(2), &log, ANCHOR);
    assert_eq!(failure.class(), "exit-2");
    assert_eq!(failure.message(), "role child exited with code 2");
}

/// Nothing the child wrote reaches the class or the message.
#[cfg(unix)]
#[test]
fn no_log_text_reaches_the_span() {
    let leak = "LEAK-fake-credential-SECRET --model opus HOME=/Users/someone";
    let log = tick(&format!("{CLI_START}\n{leak}\n{}", terminal("FATAL", 3)));
    let failure = exited(exit_with(3), &log, ANCHOR);
    assert_eq!(failure.class(), "runtime-fatal");
    for value in failure.attributes().values() {
        for fragment in ["LEAK", "SECRET", "--model", "HOME", "/Users"] {
            assert!(!value.contains(fragment), "{fragment} leaked into {value:?}");
        }
    }
}

#[cfg(unix)]
#[test]
fn a_signal_death_names_the_signal_and_no_exit_code() {
    use std::os::unix::process::ExitStatusExt;
    let failure = exited(std::process::ExitStatus::from_raw(9), &tick(CLI_START), ANCHOR);
    assert_eq!(failure.class(), "killed-by-signal");
    assert_eq!(failure.exit_code(), None);
    assert_eq!(failure.message(), "role child was killed by signal 9");
}

#[test]
fn the_launch_side_failures_have_fixed_classes() {
    let error = std::io::Error::from(std::io::ErrorKind::NotFound);
    let launch = launch_failed(&error);
    assert_eq!(launch.class(), "launch-failed");
    assert_eq!(launch.exit_code(), None);
    assert_eq!(launch.message(), "role launcher could not be spawned (NotFound)");
    assert_eq!(wait_failed(&error).class(), "wait-failed");
    let timeout = timed_out(Duration::from_secs(1800));
    assert_eq!(timeout.class(), "timeout-ceiling");
    assert_eq!(timeout.exit_code(), None);
    assert_eq!(
        timeout.message(),
        "role child ran past the 1800 s role timeout and was terminated"
    );
    assert_eq!(toolless_launch().class(), "toolless-launch");
    assert_eq!(toolless_launch().exit_code(), Some(0));
    assert_eq!(sandbox_unavailable().class(), "sandbox-unavailable");
    assert_eq!(sandbox_unavailable().exit_code(), Some(0));
}

/// Every adapter category the parser accepts has a wire name that parses
/// back to itself, so the message quotes exactly what the adapter wrote.
#[test]
fn adapter_wire_names_round_trip() {
    use TerminalClassification as C;
    for category in [
        C::Success,
        C::TokenExpired,
        C::TokenExhausted,
        C::ModelCreditsExhausted,
        C::Recoverable,
        C::Timeout,
        C::Fatal,
        C::CwdDeleted,
        C::ModelRefusal,
        C::SessionLimit,
        C::SandboxUnavailable,
        C::SessionDown,
        C::SessionMountStale,
    ] {
        let parsed: TerminalClassification = adapter_wire_name(category).parse().unwrap();
        assert_eq!(parsed, category);
    }
}

/// #10455 / #10364: a `session-exec` refusal happens before the CLI starts,
/// so its region has no `# LOOM_CLI_START`. Its named cause must still win
/// over the "no CLI start" pre-flight inference, and agree with the span's
/// `loom.admission.reason`.
#[test]
fn a_session_refusal_outranks_the_no_cli_start_inference() {
    let region = "# LOOM_ACCOUNT name=alice";
    assert_eq!(
        classify_exit(region, Some(TerminalClassification::SessionDown), 78),
        "session-down"
    );
    assert_eq!(
        classify_exit(region, Some(TerminalClassification::SessionMountStale), 78),
        "session-mount-stale"
    );
    // Without the refusal the same region is still a pre-flight death.
    assert_eq!(classify_exit(region, None, 78), "preflight-no-cli-start");
}

/// The mount-stale refusal is announced by `session-exec`, and the adapter
/// packages only a generic `RECOVERABLE`; the record parser applies the
/// announcement, so the span still names the refusal and quotes it.
#[cfg(unix)]
#[test]
fn an_announced_mount_stale_refusal_classifies_from_the_log() {
    let log = tick(&format!(
        "# LOOM_ACCOUNT name=alice\n# LOOM_SESSION_REFUSAL v=1 category=SESSION_MOUNT_STALE\n{}",
        terminal("RECOVERABLE", 78)
    ));
    let failure = exited(exit_with(78), &log, ANCHOR);
    let attrs = failure.attributes();
    assert_eq!(attrs["loom.failure_class"], "session-mount-stale");
    assert_eq!(attrs["loom.exit_code"], "78");
    assert_eq!(
        attrs[crate::telemetry::trace::STATUS_MESSAGE],
        "role child exited with code 78; runtime adapter reported SESSION_MOUNT_STALE"
    );
}
