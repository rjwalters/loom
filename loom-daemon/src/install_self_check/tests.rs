//! Unit tests for `install_self_check` — a sibling file so the frozen parent
//! (file-size ratchet) has room for the `ForgeEgressAligned` invariant (#9984).

#![allow(clippy::unwrap_used)]

use super::*;
use serial_test::serial;
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn write_config(root: &Path, contents: &str) {
    fs::create_dir_all(root.join(".loom")).unwrap();
    fs::write(root.join(".loom").join("config.json"), contents).unwrap();
}

fn write_fake_bin(dir: &Path, name: &str, body: &str) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
    }
    path
}

// ===================================================================
// Invariant registry — single source of truth
// ===================================================================

#[test]
fn test_invariant_ids_are_unique_and_stable() {
    let ids: BTreeSet<&str> = Invariant::ALL.iter().map(|i| i.id()).collect();
    assert_eq!(ids.len(), Invariant::ALL.len(), "invariant ids must be unique");
    // The three priority invariants from the 2026-08-03 incident.
    assert!(ids.contains("mcp-bundle-health"));
    assert!(ids.contains("runtimes-present"));
    assert!(ids.contains("token-ranking-fresh"));
}

#[test]
fn test_issue_marker_encodes_id() {
    for inv in Invariant::ALL {
        let marker = inv.issue_marker();
        assert!(marker.contains(inv.id()));
        assert!(marker.starts_with("<!-- loom:install-self-check:"));
    }
}

// ===================================================================
// Condition #1 — MCP bundle health
// ===================================================================

fn make_mcp(root: &Path, dist_bytes: Option<&str>, lockfile: bool, sdk_entries: bool) {
    let mcp = root.join("mcp-loom");
    fs::create_dir_all(mcp.join("dist")).unwrap();
    if let Some(b) = dist_bytes {
        fs::write(mcp.join("dist").join("index.js"), b).unwrap();
    }
    if lockfile {
        fs::write(mcp.join("package-lock.json"), "{}").unwrap();
    }
    let sdk = mcp
        .join("node_modules")
        .join("@modelcontextprotocol")
        .join("sdk");
    fs::create_dir_all(&sdk).unwrap();
    if sdk_entries {
        fs::write(sdk.join("package.json"), "{}").unwrap();
    }
}

#[test]
fn test_mcp_skipped_when_no_bundle() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(matches!(check_mcp_bundle(tmp.path()), InvariantStatus::Skipped(_)));
}

#[test]
fn test_mcp_ok_when_dist_and_sdk_present() {
    let tmp = tempfile::tempdir().unwrap();
    make_mcp(tmp.path(), Some("console.log(1)"), true, true);
    assert_eq!(check_mcp_bundle(tmp.path()), InvariantStatus::Ok);
}

#[test]
fn test_mcp_violation_when_dist_missing() {
    let tmp = tempfile::tempdir().unwrap();
    make_mcp(tmp.path(), None, true, true);
    let status = check_mcp_bundle(tmp.path());
    let InvariantStatus::Violation(detail) = status else {
        panic!("expected violation, got {status:?}");
    };
    assert!(detail.contains("index.js"), "{detail}");
}

/// The exact 2026-08-03 condition #1: a lockfile exists but the SDK
/// directory is empty (`node_modules` incomplete) — the state that produced
/// zero work for hours (#5016).
#[test]
fn test_mcp_violation_when_sdk_dir_empty_with_lockfile() {
    let tmp = tempfile::tempdir().unwrap();
    make_mcp(tmp.path(), Some("console.log(1)"), true, false);
    let status = check_mcp_bundle(tmp.path());
    let InvariantStatus::Violation(detail) = status else {
        panic!("expected violation, got {status:?}");
    };
    assert!(detail.contains("sdk"), "{detail}");
    assert!(detail.contains("npm ci"), "{detail}");
}

/// No lockfile ⇒ the node_modules-completeness check is not applied (a
/// dependency-free bundle is legitimately without node_modules).
#[test]
fn test_mcp_ok_when_no_lockfile_even_if_sdk_empty() {
    let tmp = tempfile::tempdir().unwrap();
    make_mcp(tmp.path(), Some("console.log(1)"), false, false);
    assert_eq!(check_mcp_bundle(tmp.path()), InvariantStatus::Ok);
}

// ===================================================================
// Condition #4 — .loom/runtimes/ presence
// ===================================================================

#[test]
fn test_runtimes_default_claude_when_no_config() {
    let tmp = tempfile::tempdir().unwrap();
    let set = configured_runtimes(tmp.path());
    assert!(set.contains("claude"), "default runtime should be claude");
}

#[test]
fn test_runtimes_reads_default_and_roles() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(
        tmp.path(),
        r#"{"runtimes": {"default": "codex", "roles": {"builder": "aider", "judge": "claude"}}}"#,
    );
    let set = configured_runtimes(tmp.path());
    assert!(set.contains("codex"));
    assert!(set.contains("aider"));
    assert!(set.contains("claude"));
}

/// The exact 2026-08-03 condition #4: `.loom/runtimes/` absent.
#[test]
fn test_runtimes_violation_when_dir_absent() {
    let tmp = tempfile::tempdir().unwrap();
    // No .loom/runtimes/ at all; default runtime is claude.
    let status = check_runtimes_present(tmp.path());
    let InvariantStatus::Violation(detail) = status else {
        panic!("expected violation, got {status:?}");
    };
    assert!(detail.contains("absent"), "{detail}");
    assert!(detail.contains("claude"), "{detail}");
}

#[test]
fn test_runtimes_ok_when_configured_files_present() {
    let tmp = tempfile::tempdir().unwrap();
    let rt = tmp.path().join(".loom").join("runtimes");
    fs::create_dir_all(&rt).unwrap();
    fs::write(rt.join("claude.json"), "{}").unwrap();
    assert_eq!(check_runtimes_present(tmp.path()), InvariantStatus::Ok);
}

#[test]
fn test_runtimes_violation_when_a_configured_runtime_file_missing() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(
        tmp.path(),
        r#"{"runtimes": {"default": "claude", "roles": {"builder": "codex"}}}"#,
    );
    let rt = tmp.path().join(".loom").join("runtimes");
    fs::create_dir_all(&rt).unwrap();
    fs::write(rt.join("claude.json"), "{}").unwrap();
    // codex.json missing.
    let status = check_runtimes_present(tmp.path());
    let InvariantStatus::Violation(detail) = status else {
        panic!("expected violation, got {status:?}");
    };
    assert!(detail.contains("codex"), "{detail}");
}

// ===================================================================
// Condition #4 — runtimes repair idempotency
// ===================================================================

fn make_defaults_runtimes(root: &Path, names: &[&str]) {
    let d = root.join("defaults").join("runtimes");
    fs::create_dir_all(&d).unwrap();
    for n in names {
        fs::write(d.join(format!("{n}.json")), format!("{{\"runtime\":\"{n}\"}}")).unwrap();
    }
}

#[test]
fn test_repair_runtimes_converges_missing_file() {
    let tmp = tempfile::tempdir().unwrap();
    make_defaults_runtimes(tmp.path(), &["claude", "codex", "aider"]);
    // configured = claude (default)
    assert!(matches!(check_runtimes_present(tmp.path()), InvariantStatus::Violation(_)));

    let outcome = repair_runtimes(tmp.path());
    assert!(outcome.is_repaired(), "{outcome:?}");
    // Now the check passes.
    assert_eq!(check_runtimes_present(tmp.path()), InvariantStatus::Ok);
    assert!(tmp
        .path()
        .join(".loom")
        .join("runtimes")
        .join("claude.json")
        .is_file());
}

#[test]
fn test_repair_runtimes_is_idempotent_noop_when_current() {
    let tmp = tempfile::tempdir().unwrap();
    make_defaults_runtimes(tmp.path(), &["claude"]);
    // First repair converges.
    assert!(repair_runtimes(tmp.path()).is_repaired());
    // Second repair is a no-op (nothing missing).
    let second = repair_runtimes(tmp.path());
    assert!(
        matches!(second, RepairOutcome::NotAttempted(_)),
        "second repair should be a no-op, got {second:?}"
    );
}

#[test]
fn test_repair_runtimes_reports_when_defaults_absent() {
    let tmp = tempfile::tempdir().unwrap();
    // No defaults/runtimes/ — a consumer clone.
    let outcome = repair_runtimes(tmp.path());
    assert!(matches!(outcome, RepairOutcome::NotAttempted(_)), "{outcome:?}");
}

// ===================================================================
// Condition #5 — token .ranking staleness
// ===================================================================

fn bootstrap_pool(root: &Path) -> PathBuf {
    let dir = root.join(".loom").join("tokens");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("agent-1.token"), "x").unwrap();
    dir
}

#[test]
// `LOOM_SHARED_TOKENS_DIR`-mutating tests here MUST use
// `#[serial(loom_shared_tokens_dir_env)]`, the key `role_runner::tests`
// already uses for the same var — never bare `#[serial]` (or, as here,
// no lock at all), or a `role_runner` test's own mutation of this
// process-global can race in unserialized (#8480 audit).
#[serial(loom_shared_tokens_dir_env)]
fn test_ranking_skipped_when_no_pool() {
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("LOOM_SHARED_TOKENS_DIR", "");
    let status = check_token_ranking_fresh(tmp.path(), Duration::from_secs(3600));
    std::env::remove_var("LOOM_SHARED_TOKENS_DIR");
    assert!(matches!(status, InvariantStatus::Skipped(_)), "{status:?}");
}

#[test]
#[serial(loom_shared_tokens_dir_env)]
fn test_ranking_violation_when_missing_with_pool() {
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("LOOM_SHARED_TOKENS_DIR", "");
    bootstrap_pool(tmp.path());
    // No .ranking file written.
    let status = check_token_ranking_fresh(tmp.path(), Duration::from_secs(3600));
    std::env::remove_var("LOOM_SHARED_TOKENS_DIR");
    let InvariantStatus::Violation(detail) = status else {
        panic!("expected violation, got {status:?}");
    };
    assert!(detail.contains(".ranking"), "{detail}");
}

#[test]
#[serial(loom_shared_tokens_dir_env)]
fn test_ranking_ok_when_fresh() {
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("LOOM_SHARED_TOKENS_DIR", "");
    let dir = bootstrap_pool(tmp.path());
    fs::write(dir.join(".ranking"), "agent-1\n").unwrap();
    let status = check_token_ranking_fresh(tmp.path(), Duration::from_secs(3600));
    std::env::remove_var("LOOM_SHARED_TOKENS_DIR");
    assert_eq!(status, InvariantStatus::Ok);
}

/// The exact 2026-08-03 condition #5: a `.ranking` old enough to have
/// drifted from live rate-limit state. We backdate the file's mtime and use
/// a tight threshold.
#[test]
#[serial(loom_shared_tokens_dir_env)]
fn test_ranking_violation_when_stale() {
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("LOOM_SHARED_TOKENS_DIR", "");
    // `bootstrap_pool`'s return is joined inline rather than bound: this
    // file is at its `scripts/check-file-size-budget.sh` ratchet ceiling,
    // so the `#[serial(...)]` line added above had to be paid for here.
    let ranking = bootstrap_pool(tmp.path()).join(".ranking");
    fs::write(&ranking, "agent-1\n").unwrap();
    // Backdate mtime by two hours.
    let two_hours_ago = SystemTime::now() - Duration::from_secs(7200);
    let f = fs::File::options().write(true).open(&ranking).unwrap();
    f.set_modified(two_hours_ago).unwrap();
    drop(f);

    let status = check_token_ranking_fresh(tmp.path(), Duration::from_secs(3600));
    std::env::remove_var("LOOM_SHARED_TOKENS_DIR");
    let InvariantStatus::Violation(detail) = status else {
        panic!("expected violation, got {status:?}");
    };
    assert!(detail.contains("old"), "{detail}");
}

// ===================================================================
// Repair — token ranking via injected daemon bin
// ===================================================================

#[test]
fn test_repair_token_ranking_success_via_fake_bin() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = write_fake_bin(tmp.path(), "fake-daemon.sh", "exit 0");
    let ctx = RepairContext {
        daemon_bin: Some(bin),
        ..RepairContext::default()
    };
    let outcome = repair_token_ranking(tmp.path(), &ctx);
    assert!(outcome.is_repaired(), "{outcome:?}");
}

#[test]
fn test_repair_token_ranking_failure_via_fake_bin() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = write_fake_bin(tmp.path(), "fake-daemon.sh", "echo boom; exit 1");
    let ctx = RepairContext {
        daemon_bin: Some(bin),
        ..RepairContext::default()
    };
    let outcome = repair_token_ranking(tmp.path(), &ctx);
    assert!(matches!(outcome, RepairOutcome::Failed(_)), "{outcome:?}");
}

// ===================================================================
// Repair — MCP live-sweep guard
// ===================================================================

#[test]
fn test_mcp_repair_refused_under_live_sweep() {
    let tmp = tempfile::tempdir().unwrap();
    make_mcp(tmp.path(), Some("x"), true, false);
    // Simulate a live sweep: a worktree with a .loom-in-use marker.
    let wt = tmp.path().join(".loom").join("worktrees").join("issue-1");
    fs::create_dir_all(&wt).unwrap();
    fs::write(wt.join(".loom-in-use"), "").unwrap();
    assert!(live_sweep_in_progress(tmp.path()));

    let outcome = repair_mcp_bundle(tmp.path(), &RepairContext::default());
    let RepairOutcome::NotAttempted(reason) = outcome else {
        panic!("expected NotAttempted, got {outcome:?}");
    };
    assert!(reason.contains("live sweep"), "{reason}");
}

#[test]
fn test_live_sweep_false_when_no_worktrees() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(!live_sweep_in_progress(tmp.path()));
}

// ===================================================================
// Issue filing — exactly one per condition (dedup)
// ===================================================================

struct FakeReporter {
    already_open: bool,
    lookup_calls: Arc<AtomicUsize>,
    file_calls: Arc<AtomicUsize>,
    next_issue: u64,
}

impl ViolationReporter for FakeReporter {
    fn has_open_issue(&self, _marker: &str) -> Result<bool, String> {
        self.lookup_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.already_open)
    }
    fn file_issue(&self, _title: &str, _body: &str) -> Result<u64, String> {
        self.file_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.next_issue)
    }
}

fn violation_outcome() -> InvariantOutcome {
    InvariantOutcome {
        invariant: Invariant::RuntimesPresent,
        status: InvariantStatus::Violation("runtimes dir absent".to_string()),
    }
}

#[test]
fn test_report_files_one_issue_when_none_open() {
    let tmp = tempfile::tempdir().unwrap();
    let reporter = FakeReporter {
        already_open: false,
        lookup_calls: Arc::new(AtomicUsize::new(0)),
        file_calls: Arc::new(AtomicUsize::new(0)),
        next_issue: 4242,
    };
    let outcome = report_violation(&reporter, tmp.path(), &violation_outcome());
    assert_eq!(outcome, ReportOutcome::Filed(4242));
    assert_eq!(reporter.file_calls.load(Ordering::SeqCst), 1);
}

/// The #4736 failure mode this dedup exists to prevent: a repeat pass with
/// an already-open issue must NOT file (or comment) again.
#[test]
fn test_report_does_not_duplicate_when_already_open() {
    let tmp = tempfile::tempdir().unwrap();
    let reporter = FakeReporter {
        already_open: true,
        lookup_calls: Arc::new(AtomicUsize::new(0)),
        file_calls: Arc::new(AtomicUsize::new(0)),
        next_issue: 1,
    };
    let outcome = report_violation(&reporter, tmp.path(), &violation_outcome());
    assert_eq!(outcome, ReportOutcome::AlreadyOpen);
    assert_eq!(reporter.file_calls.load(Ordering::SeqCst), 0, "must not file a duplicate");
}

#[test]
fn test_report_body_carries_dedup_marker() {
    // Prove the filed body contains the marker the next pass dedupes on.
    struct CapturingReporter {
        body: std::sync::Mutex<String>,
    }
    impl ViolationReporter for CapturingReporter {
        fn has_open_issue(&self, _marker: &str) -> Result<bool, String> {
            Ok(false)
        }
        fn file_issue(&self, _title: &str, body: &str) -> Result<u64, String> {
            *self.body.lock().unwrap() = body.to_string();
            Ok(7)
        }
    }
    let tmp = tempfile::tempdir().unwrap();
    let reporter = CapturingReporter {
        body: std::sync::Mutex::new(String::new()),
    };
    let out = report_violation(&reporter, tmp.path(), &violation_outcome());
    assert_eq!(out, ReportOutcome::Filed(7));
    let body = reporter.body.lock().unwrap().clone();
    assert!(body.contains(&Invariant::RuntimesPresent.issue_marker()), "{body}");
}

// ===================================================================
// Config surface + precedence (env > config > default)
// ===================================================================

#[test]
fn test_config_missing_file_is_default() {
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(read_config(tmp.path()), InstallSelfCheckConfig::default());
}

#[test]
fn test_config_reads_all_fields() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(
        tmp.path(),
        r#"{"autonomous": {"installSelfCheck": {"enabled": true, "intervalSecs": 900, "repair": true, "tokenRankingMaxAgeSecs": 120}}}"#,
    );
    assert_eq!(
        read_config(tmp.path()),
        InstallSelfCheckConfig {
            enabled: Some(true),
            interval_secs: Some(900),
            repair: Some(true),
            ranking_max_age_secs: Some(120),
        }
    );
}

#[test]
#[serial]
fn test_resolve_enabled_default_is_false() {
    std::env::remove_var(INSTALL_SELF_CHECK_ENABLE_ENV);
    assert!(
        !resolve_enabled(&InstallSelfCheckConfig::default()),
        "absent config + unset env ⇒ default OFF (FLAGS-OFF)"
    );
}

#[test]
#[serial]
fn test_resolve_enabled_env_overrides_config() {
    std::env::set_var(INSTALL_SELF_CHECK_ENABLE_ENV, "1");
    assert!(resolve_enabled(&InstallSelfCheckConfig {
        enabled: Some(false),
        ..Default::default()
    }));
    std::env::set_var(INSTALL_SELF_CHECK_ENABLE_ENV, "0");
    assert!(!resolve_enabled(&InstallSelfCheckConfig {
        enabled: Some(true),
        ..Default::default()
    }));
    std::env::remove_var(INSTALL_SELF_CHECK_ENABLE_ENV);
}

#[test]
#[serial]
fn test_resolve_repair_default_is_report_only() {
    std::env::remove_var(INSTALL_SELF_CHECK_REPAIR_ENV);
    assert!(
        !resolve_repair(&InstallSelfCheckConfig::default()),
        "report-only is the default"
    );
    assert!(resolve_repair(&InstallSelfCheckConfig {
        repair: Some(true),
        ..Default::default()
    }));
}

#[test]
#[serial]
fn test_resolve_interval_precedence() {
    std::env::remove_var(INSTALL_SELF_CHECK_INTERVAL_ENV);
    assert_eq!(
        resolve_interval(&InstallSelfCheckConfig::default()),
        Duration::from_secs(DEFAULT_INTERVAL_SECS)
    );
    assert_eq!(
        resolve_interval(&InstallSelfCheckConfig {
            interval_secs: Some(300),
            ..Default::default()
        }),
        Duration::from_secs(300)
    );
    std::env::set_var(INSTALL_SELF_CHECK_INTERVAL_ENV, "45");
    assert_eq!(
        resolve_interval(&InstallSelfCheckConfig {
            interval_secs: Some(300),
            ..Default::default()
        }),
        Duration::from_secs(45)
    );
    std::env::remove_var(INSTALL_SELF_CHECK_INTERVAL_ENV);
}

// ===================================================================
// run_pass — report-only does not act; repair mode repairs
// ===================================================================

#[test]
#[serial(loom_shared_tokens_dir_env)]
fn test_run_pass_report_only_does_not_repair_or_file() {
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("LOOM_SHARED_TOKENS_DIR", "");
    make_defaults_runtimes(tmp.path(), &["claude"]);
    // runtimes violation present (no .loom/runtimes yet).
    let reporter = FakeReporter {
        already_open: false,
        lookup_calls: Arc::new(AtomicUsize::new(0)),
        file_calls: Arc::new(AtomicUsize::new(0)),
        next_issue: 1,
    };
    let report = run_pass(
        tmp.path(),
        false, // report-only
        CheckOptions::hermetic(),
        &RepairContext::default(),
        &reporter,
    );
    std::env::remove_var("LOOM_SHARED_TOKENS_DIR");
    assert!(!report.is_all_ok(), "runtimes violation expected");
    // report-only: no repair happened, and the .loom/runtimes dir is still absent.
    assert!(!tmp
        .path()
        .join(".loom")
        .join("runtimes")
        .join("claude.json")
        .exists());
    assert_eq!(reporter.file_calls.load(Ordering::SeqCst), 0, "report-only must not file");
}

#[test]
#[serial(loom_shared_tokens_dir_env)]
fn test_run_pass_repair_mode_converges_runtimes() {
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("LOOM_SHARED_TOKENS_DIR", "");
    make_defaults_runtimes(tmp.path(), &["claude"]);
    let reporter = FakeReporter {
        already_open: false,
        lookup_calls: Arc::new(AtomicUsize::new(0)),
        file_calls: Arc::new(AtomicUsize::new(0)),
        next_issue: 1,
    };
    run_pass(tmp.path(), true, CheckOptions::hermetic(), &RepairContext::default(), &reporter);
    std::env::remove_var("LOOM_SHARED_TOKENS_DIR");
    // repair mode converged the runtimes file.
    assert!(tmp
        .path()
        .join(".loom")
        .join("runtimes")
        .join("claude.json")
        .is_file());
}

// ===================================================================
// resolve_tool_path
// ===================================================================

/// The search path is **injected**, never `set_var`'d (#5961): this test
/// previously replaced the process-global `PATH` with a tempdir and never
/// restored it, so every test that ran afterwards in the same process lost
/// its ability to spawn a bare-name `git`/`gh`.
#[test]
fn test_resolve_tool_path_finds_on_path() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = write_fake_bin(tmp.path(), "mytool", "exit 0");
    let resolved = resolve_tool_path_in("mytool", Some(tmp.path().as_os_str()));
    assert_eq!(resolved.as_deref(), Some(bin.as_path()));
}

/// A tool absent from the injected search path falls through to the
/// hard-coded Homebrew / usr-local fallbacks and is not found there
/// either — the "explicitly resolve rather than trust `PATH`" contract
/// (#4875) with no process-global mutation.
#[test]
fn test_resolve_tool_path_returns_none_when_absent_everywhere() {
    let tmp = tempfile::tempdir().unwrap();
    let resolved =
        resolve_tool_path_in("definitely-not-a-real-tool-5961", Some(tmp.path().as_os_str()));
    assert_eq!(resolved, None);
}

/// The public entry point still resolves a real tool end-to-end — the
/// production wiring [`resolve_tool_path_in`] must not silently drop. Not
/// asserted against a synthetic `PATH`, because injecting one into this
/// process is exactly the process-global mutation this change removes
/// (#5961).
#[test]
fn test_resolve_tool_path_resolves_a_real_tool_via_the_process_env() {
    let sh = resolve_tool_path("sh").expect("`sh` resolves on any supported host");
    assert!(sh.ends_with("sh"), "resolved an unexpected path: {}", sh.display());
    assert!(is_executable(&sh), "resolved a non-executable path: {}", sh.display());
}

// ===================================================================
// #10516 — interactive sessions reach the agent gh front
// ===================================================================

#[cfg(unix)]
#[test]
fn test_gh_front_wired_check_with() {
    use gh_front_invariant::check_with;
    let tmp = tempfile::tempdir().unwrap();
    let daemon = write_fake_bin(&tmp.path().join("bin"), "loom-daemon", "exit 0");
    let front = tmp.path().join("front");
    fs::create_dir_all(&front).unwrap();
    std::os::unix::fs::symlink(&daemon, front.join("gh")).unwrap();
    let launcher = write_fake_bin(&tmp.path().join("managed"), "gh", "exit 0");
    let plain = write_fake_bin(&tmp.path().join("plain"), "gh", "exit 0");
    let dir = |p: &Path| Some(p.parent().unwrap().as_os_str().to_os_string());
    let wired = Some(r#"{"command": "… $HOME/.local/share/loom/defaults/hooks/gh-front-env.sh"}"#);
    let loom_only =
        Some(r#"{"command": "… $HOME/.local/share/loom/defaults/hooks/guard-destructive.sh"}"#);
    let project = Some(r#"{"command": "\"${CLAUDE_PROJECT_DIR}/.loom/hooks/gh-front-env.sh\""}"#);
    let skipped = |s: &InvariantStatus| matches!(s, InvariantStatus::Skipped(_));

    // Never provisioned: not this check's business.
    assert!(skipped(&check_with(None, None, None, None)));
    assert!(skipped(&check_with(Some("{}"), Some("{}"), None, None)));
    // Provisioned but stale (no SessionStart entry): the one actionable miss.
    assert!(check_with(loom_only, None, dir(&front.join("gh")), None).is_violation());
    // Wired (either scope) and the prefix resolves gh to the front.
    assert_eq!(check_with(wired, None, dir(&front.join("gh")), None), InvariantStatus::Ok);
    assert_eq!(
        check_with(loom_only, project, dir(&front.join("gh")), None),
        InvariantStatus::Ok
    );
    // Under a policy, the launcher first is equally correct.
    assert_eq!(check_with(wired, None, dir(&launcher), Some(&launcher)), InvariantStatus::Ok);
    // Wired but ineffective: no shim dir, or a prefix whose gh is neither.
    assert!(check_with(wired, None, None, None).is_violation());
    assert!(check_with(wired, None, dir(&plain), Some(&launcher)).is_violation());
    // The live check never reads this host's ~/.claude in a unit-test build.
    assert!(skipped(&check(Invariant::GhFrontWired, tmp.path(), &CheckOptions::hermetic())));
}
