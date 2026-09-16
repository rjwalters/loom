use super::super::PlanEntry;
use super::*;

pub(super) fn base_config() -> AddWorkerConfig {
    AddWorkerConfig {
        ssh_host: "worker-1".to_string(),
        repos: vec!["rjwalters/anvil".to_string()],
        priority: 50,
        dry_run: false,
        loom_repo_url: DEFAULT_LOOM_REPO_URL.to_string(),
        pat_file: None,
        accounts_env_file: None,
        safehouse_enabled: false,
        idle_shutdown_minutes: None,
        safehouse_tailnet_auth_key_file: None,
        safehouse_secrets_file: None,
        safehouse_repo_url: DEFAULT_SAFEHOUSE_REPO_URL.to_string(),
        safehouse_homeserver_url: None,
        safehouse_room: None,
        safehouse_personas: Vec::new(),
        safehouse_invite_exec: None,
        feed_egress_enabled: false,
        feed_egress_ingest_key_file: None,
        // Both left at the command's own defaults — no sink URL, no scrub
        // patterns (#7814): neither carries a compiled-in operator identity.
        feed_egress_sink_url: None,
        feed_egress_deny_patterns: Vec::new(),
        feed_egress_delay_seconds: DEFAULT_FEED_EGRESS_DELAY_SECONDS,
    }
}

/// A full, valid `--safehouse` config — every required input present and
/// well-formed. Individual tests mutate a field to exercise a specific
/// failure.
pub(super) fn safehouse_config() -> AddWorkerConfig {
    let mut config = base_config();
    config.safehouse_enabled = true;
    config.safehouse_tailnet_auth_key_file = Some(PathBuf::from("/does/not/matter"));
    config.safehouse_secrets_file = Some(PathBuf::from("/does/not/matter"));
    config.safehouse_homeserver_url = Some("matrix.internal.example".to_string());
    config.safehouse_room = Some("!fleet:matrix.internal.example".to_string());
    config.safehouse_personas = vec!["loom_daemon".to_string()];
    config
}

pub(super) fn safehouse_secrets() -> Secrets {
    Secrets {
        pat: None,
        accounts_env: None,
        safehouse_tailnet_auth_key: Some("tskey-auth-ephemeral-tagged".to_string()),
        safehouse_secrets: Some(
            "SAFEHOUSE_MATRIX_USER_ID=@safehoused-worker1:matrix.internal.example\n\
                 SAFEHOUSE_MATRIX_PASSWORD=hunter2-matrix-pw\n\
                 SAFEHOUSE_STORE_PASSPHRASE=store-pass-xyz\n\
                 SAFEHOUSE_RECOVERY_PASSPHRASE=recovery-pass-xyz\n"
                .to_string(),
        ),
        feed_egress_ingest_key: None,
    }
}

// ---- validation / preflight ----------------------------------------

#[test]
fn validate_repo_accepts_slug_and_rejects_injection() {
    assert!(validate_repo("rjwalters/anvil").is_ok());
    assert!(validate_repo("owner/name.with-dots_and-dashes").is_ok());
    assert!(validate_repo("no-slash").is_err());
    assert!(validate_repo("owner/name; rm -rf /").is_err());
    assert!(validate_repo("owner/$(whoami)").is_err());
    assert!(validate_repo("").is_err());
}

#[test]
fn repo_dir_name_takes_last_segment() {
    assert_eq!(repo_dir_name("rjwalters/anvil"), "anvil");
    assert_eq!(repo_dir_name("a/b/c"), "c");
}

#[test]
fn preflight_requires_a_repo() {
    let mut config = base_config();
    config.repos.clear();
    assert!(preflight(&config).is_err());
}

#[test]
fn preflight_rejects_empty_host() {
    let mut config = base_config();
    config.ssh_host = "   ".to_string();
    assert!(preflight(&config).is_err());
}

#[test]
fn preflight_missing_pat_file_fails_before_remote() {
    let mut config = base_config();
    config.pat_file = Some(PathBuf::from("/nonexistent/pat-file-xyz"));
    let err = preflight(&config).unwrap_err().to_string();
    assert!(err.contains("pat-file"), "err: {err}");
}

#[test]
fn preflight_missing_accounts_env_fails_before_remote() {
    let mut config = base_config();
    config.accounts_env_file = Some(PathBuf::from("/nonexistent/accounts-xyz.env"));
    assert!(preflight(&config).is_err());
}

#[test]
fn preflight_reads_secret_files_and_trims_pat() {
    let dir = tempfile::tempdir().unwrap();
    let pat = dir.path().join("pat");
    let accounts = dir.path().join("accounts.env");
    std::fs::write(&pat, "  github_pat_abc123\n").unwrap();
    std::fs::write(&accounts, "ACCOUNT_EMAIL_1=a@b.c\n").unwrap();
    let mut config = base_config();
    config.pat_file = Some(pat);
    config.accounts_env_file = Some(accounts);
    let secrets = preflight(&config).unwrap();
    assert_eq!(secrets.pat.as_deref(), Some("github_pat_abc123"));
    assert!(secrets
        .accounts_env
        .as_deref()
        .unwrap()
        .contains("ACCOUNT_EMAIL_1"));
}

#[test]
fn preflight_empty_pat_file_fails() {
    let dir = tempfile::tempdir().unwrap();
    let pat = dir.path().join("pat");
    std::fs::write(&pat, "   \n").unwrap();
    let mut config = base_config();
    config.pat_file = Some(pat);
    assert!(preflight(&config).is_err());
}

// ---- platform fast-fail (#5395) --------------------------------------

/// A runner that answers every command with a scripted [`CommandOutput`]
/// and records the shell it was asked to run — so a test can assert both
/// the outcome and that *nothing beyond the probe* was ever executed.
struct ScriptedRunner {
    out: CommandOutput,
    seen: std::cell::RefCell<Vec<String>>,
}

impl ScriptedRunner {
    fn new(code: i32, stdout: &str, stderr: &str) -> Self {
        Self {
            out: CommandOutput {
                code,
                stdout: stdout.to_string(),
                stderr: stderr.to_string(),
            },
            seen: std::cell::RefCell::new(Vec::new()),
        }
    }
}

impl CommandRunner for ScriptedRunner {
    fn run(&self, shell: &str, _stdin: Option<&str>) -> Result<CommandOutput> {
        self.seen.borrow_mut().push(shell.to_string());
        Ok(self.out.clone())
    }
}

#[test]
fn platform_probe_accepts_linux_and_runs_only_the_probe() {
    let runner = ScriptedRunner::new(0, "Linux\n", "");
    assert!(ensure_supported_platform(&runner, "worker-1").is_ok());
    let seen = runner.seen.borrow();
    assert_eq!(seen.len(), 1, "the probe must be a single command");
    assert_eq!(seen[0], PLATFORM_PROBE_SHELL);
    assert!(
        !seen[0].contains("apt-get") && !seen[0].contains("systemctl"),
        "the probe must not touch the host's package manager or supervisor"
    );
}

#[test]
fn platform_probe_accepts_case_insensitive_linux() {
    let runner = ScriptedRunner::new(0, "linux\n", "");
    assert!(ensure_supported_platform(&runner, "worker-1").is_ok());
}

#[test]
fn platform_probe_rejects_darwin_naming_platform_and_runbook() {
    let runner = ScriptedRunner::new(0, "Darwin\n", "");
    let err = ensure_supported_platform(&runner, "mac-mini")
        .expect_err("a Darwin target must fail fast")
        .to_string();

    // AC 1: names the platform ...
    assert!(err.contains("macOS"), "error must name the platform: {err}");
    assert!(err.contains("Darwin"), "error must carry the raw uname: {err}");
    assert!(err.contains("mac-mini"), "error must name the host: {err}");
    assert!(
        err.contains("Linux targets only"),
        "error must state the supported platform: {err}"
    );
    // ... and points at the manual route.
    assert!(err.contains("--dry-run"), "error must offer the manual checklist: {err}");
    assert!(err.contains("daemon-reference.md"), "error must point at the runbook: {err}");

    // AC 1: fails *before* any apt-get/systemd step is attempted.
    assert_eq!(
        runner.seen.borrow().len(),
        1,
        "nothing beyond the probe may run on an unsupported target"
    );
}

#[test]
fn platform_probe_reports_an_unglossed_platform_verbatim() {
    let runner = ScriptedRunner::new(0, "SunOS\n", "");
    let err = ensure_supported_platform(&runner, "solaris-box")
        .expect_err("a non-Linux target must fail fast")
        .to_string();
    assert!(err.contains("SunOS"), "error must name the platform: {err}");
}

#[test]
fn platform_probe_failure_is_reported_as_an_undetermined_platform() {
    let runner = ScriptedRunner::new(255, "", "ssh: connect to host worker-9: No route to host");
    let err = ensure_supported_platform(&runner, "worker-9")
        .expect_err("an unreachable host must fail fast")
        .to_string();
    assert!(
        err.contains("could not determine the platform"),
        "error must explain what failed: {err}"
    );
    assert!(err.contains("255"), "error must carry the exit code: {err}");
    assert!(err.contains("No route to host"), "error must carry the stderr: {err}");
}

#[test]
fn platform_probe_empty_output_fails_rather_than_assuming_linux() {
    let runner = ScriptedRunner::new(0, "   \n", "");
    let err = ensure_supported_platform(&runner, "worker-1")
        .expect_err("an empty probe answer must not be assumed Linux")
        .to_string();
    assert!(err.contains("printed nothing"), "{err}");
}

#[test]
fn platform_label_glosses_known_platforms_and_caps_garbage() {
    assert_eq!(platform_label("Darwin"), "macOS (Darwin)");
    assert_eq!(platform_label("FreeBSD"), "FreeBSD (BSD)");
    assert!(platform_label("MINGW64_NT-10.0").starts_with("Windows ("));
    assert_eq!(platform_label("SunOS"), "SunOS");
    // Arbitrary remote output is capped to one line and 40 chars so a
    // hostile/garbled answer cannot flood the operator's terminal.
    let garbage = platform_label(&format!("{}\nsecond line", "x".repeat(200)));
    assert_eq!(garbage.chars().count(), 40);
}

// ---- plan shape ----------------------------------------------------

#[test]
fn plan_step_ordering_matches_the_eight_pilot_steps() {
    let config = base_config();
    // Full secrets so the token/forge steps are executable, not skipped.
    let secrets = Secrets {
        pat: Some("pat".to_string()),
        accounts_env: Some("ACCOUNT_EMAIL_1=a@b.c".to_string()),
        ..Secrets::default()
    };
    let plan = build_plan(&config, &secrets);
    let names: Vec<&str> = plan
        .entries
        .iter()
        .map(super::super::PlanEntry::name)
        .collect();
    assert_eq!(
        names,
        vec![
            "base-deps",
            "machine-layout",
            "claude-code",
            "forge-auth",
            "token-accounts",
            "token-pool",
            "token-ranking",
            "workspace-clone",
            "workspace-register",
            "daemon-unit",
            "daemon-watchdog",
            "idle-shutdown",
            "safehouse",
            "verify",
        ]
    );
}

#[test]
fn workspace_clone_only_inits_an_unconfigured_workspace() {
    // #4641: `loom-daemon init` used to run unconditionally here, so every
    // re-run of provisioning re-entered the `.loom/config.json` merge on a
    // workspace an operator had since hand-tuned. The init call must now sit
    // behind a `.loom/config.json` existence guard, the same way the clone
    // sits behind a `.git` guard.
    let script = render_workspace_clone(&["rjwalters/anvil".to_string()]);

    assert!(
        script.contains(r#"if [ ! -f "$HOME/loom-workspaces/anvil/.loom/config.json" ]; then"#),
        "init must be guarded on the workspace being unconfigured:\n{script}"
    );

    // The guard must actually enclose the init call: the only `loom-daemon
    // init` line has to appear after the guard and before its `else`.
    let guard = script
        .find(r#"if [ ! -f "$HOME/loom-workspaces/anvil/.loom/config.json" ]"#)
        .expect("guard present");
    let init = script.find("loom-daemon init").expect("init present");
    let else_branch = script.find("\nelse\n").expect("else present");
    assert!(guard < init && init < else_branch, "init is outside the guard:\n{script}");
    assert_eq!(
        script.matches("loom-daemon init").count(),
        1,
        "no second, unguarded init call may remain:\n{script}"
    );

    // The clone itself stays guarded on .git (unchanged behavior).
    assert!(
        script.contains(r#"if [ ! -d "$HOME/loom-workspaces/anvil/.git" ]; then"#),
        "clone guard regressed:\n{script}"
    );
}

#[test]
fn base_deps_check_includes_libsqlite3_dev() {
    // safehouse#38: libsqlite3-dev must be part of the base deps.
    let plan = build_plan(&base_config(), &Secrets::default());
    let base = plan
        .entries
        .iter()
        .find_map(|e| match e {
            super::super::PlanEntry::Step(s) if s.name == "base-deps" => Some(s),
            _ => None,
        })
        .unwrap();
    assert!(base.check.as_ref().unwrap().contains("libsqlite3-dev"));
    assert!(base.apply.contains("libsqlite3-dev"));
}

// ---- machine-layout: artifact-first provisioning (#5067, Epic #4990 Phase 4) ----

fn machine_layout_step() -> Step {
    let plan = build_plan(&base_config(), &Secrets::default());
    plan.entries
        .iter()
        .find_map(|e| match e {
            super::super::PlanEntry::Step(s) if s.name == "machine-layout" => Some(s.clone()),
            _ => None,
        })
        .unwrap()
}

#[test]
fn base_deps_no_longer_installs_or_requires_a_rust_toolchain() {
    // AC: base-deps no longer requires rustup/a Rust toolchain on the
    // happy path (an artifact resolves for this host's platform); it is
    // only pulled in — by machine-layout, reactively — as a fallback
    // dependency when the build path is actually taken.
    let plan = build_plan(&base_config(), &Secrets::default());
    let base = plan
        .entries
        .iter()
        .find_map(|e| match e {
            super::super::PlanEntry::Step(s) if s.name == "base-deps" => Some(s),
            _ => None,
        })
        .unwrap();
    assert!(
        !base.apply.contains("rustup"),
        "base-deps must not install rustup unconditionally:\n{}",
        base.apply
    );
    assert!(
        !base.check.as_ref().unwrap().contains("cargo"),
        "base-deps' idempotency check must not require cargo (it no longer installs it):\n{}",
        base.check.as_ref().unwrap()
    );
    // gh remains required (needed for artifact resolution + downloads).
    assert!(base.check.as_ref().unwrap().contains("command -v gh"));
}

#[test]
fn machine_layout_attempts_artifact_fetch_before_any_toolchain_fallback() {
    // AC: machine-layout attempts a release-artifact fetch first (reusing
    // Phase 3's already-tested fetch/verify/checksum logic by shelling
    // out to loom-daemon-update.sh's own "auto" resolution), falling back
    // to `cargo build -p loom-daemon --release` (installing rustup first)
    // only when that invocation fails.
    let step = machine_layout_step();
    let script = &step.apply;

    // Delegates to loom-daemon-update.sh (Phase 3, #5020) rather than
    // duplicating the fetch/verify/checksum implementation.
    let update_call = script
        .find(r#"defaults/scripts/cli/loom-daemon-update.sh""#)
        .expect(
            "machine-layout must invoke loom-daemon-update.sh to reuse Phase 3's \
                 fetch/verify/checksum logic",
        );

    // The toolchain fallback (rustup install + `cargo build` retry) must
    // sit strictly INSIDE the `if ! "$UPDATE_SCRIPT" ...; then` failure
    // branch — i.e. after the first invocation and gated on `cargo`
    // being absent — so it is never reached on the artifact-available
    // happy path.
    let toolchain_fallback = script
        .find("installing rustup as a fallback dependency")
        .expect("a rustup-install fallback must exist for when no artifact resolves");
    let cargo_guard = script
        .find("if ! command -v cargo")
        .expect("the toolchain fallback must be gated on cargo being absent");
    assert!(
        update_call < cargo_guard && cargo_guard < toolchain_fallback,
        "artifact-fetch attempt must precede the cargo-absent guard, which must precede \
             the rustup install:\n{script}"
    );

    // No literal `cargo build` shell invocation: the actual build now
    // happens inside loom-daemon-update.sh's own (already-tested)
    // fallback path, not duplicated here.
    assert!(
        !script.contains("cargo build --release")
            && !script
                .lines()
                .any(|l| l.trim_start().starts_with("cargo build")),
        "machine-layout must not duplicate the source-build invocation; it delegates to \
             loom-daemon-update.sh instead:\n{script}"
    );

    // --no-restart: fleet add-worker has not installed the loom-daemon
    // systemd unit yet at this point in the plan, so there is never a
    // running daemon for this invocation to try to restart.
    assert!(
        script.contains(r#""$UPDATE_SCRIPT" --no-restart"#),
        "loom-daemon-update.sh must be invoked with --no-restart:\n{script}"
    );
}

#[test]
fn machine_layout_toolchain_fallback_retries_the_same_update_script() {
    // The retry after installing rustup must call the SAME update-script
    // invocation (not a hand-rolled `cargo build`), so it goes through
    // the identical fetch/verify/checksum + build logic a second time
    // (now with cargo available) rather than a second implementation.
    let step = machine_layout_step();
    let script = &step.apply;
    assert_eq!(
        script.matches(r#""$UPDATE_SCRIPT" --no-restart"#).count(),
        2,
        "expected exactly two invocations (initial attempt + one retry after installing \
             rustup):\n{script}"
    );
}

#[test]
fn machine_layout_idempotency_check_unchanged() {
    // AC: the plan's idempotency check (test -x .../loom-daemon) and
    // existing re-run semantics are preserved — a re-run of `fleet
    // add-worker` against an already-provisioned host still no-ops this
    // step.
    let step = machine_layout_step();
    assert_eq!(step.check.as_deref(), Some(r#"test -x "$HOME/.local/bin/loom-daemon""#));
}

#[test]
fn machine_layout_rendered_script_is_valid_shell() {
    // Sanity-check the generated script actually parses as shell (bash -n
    // — syntax check only, no execution) — catches an unbalanced
    // if/fi or quoting mistake in the artifact-fetch/fallback wiring
    // that a pure string-content assertion would miss.
    let step = machine_layout_step();
    let output = std::process::Command::new("bash")
        .arg("-n")
        .arg("-c")
        .arg(&step.apply)
        .output();
    match output {
        Ok(out) => assert!(
            out.status.success(),
            "rendered machine-layout script failed `bash -n`:\n{}\nscript:\n{}",
            String::from_utf8_lossy(&out.stderr),
            step.apply
        ),
        Err(e) => {
            // bash not available in this environment — skip rather than fail.
            eprintln!("skipping bash -n check: could not launch bash ({e})");
        }
    }
}

#[test]
fn forge_auth_and_token_accounts_carry_secret_stdin() {
    let config = base_config();
    let secrets = Secrets {
        pat: Some("the-pat".to_string()),
        accounts_env: Some("ACCOUNT_EMAIL_1=a@b.c".to_string()),
        ..Secrets::default()
    };
    let plan = build_plan(&config, &secrets);
    for name in ["forge-auth", "token-accounts"] {
        let step = plan
            .entries
            .iter()
            .find_map(|e| match e {
                super::super::PlanEntry::Step(s) if s.name == name => Some(s),
                _ => None,
            })
            .unwrap();
        let stdin = step.stdin.as_ref().expect("secret step must carry stdin");
        assert!(stdin.secret, "{name} stdin must be marked secret");
    }
    // The apply strings must NOT embed the secret values (stdin only).
    let dry = plan.render_dry_run("fleet add-worker", "worker-1");
    assert!(!dry.contains("the-pat"));
}

#[test]
fn missing_secrets_become_skips_not_failures() {
    // No PAT, no accounts.env → those steps are skip-with-notice.
    let plan = build_plan(&base_config(), &Secrets::default());
    for name in [
        "forge-auth",
        "token-accounts",
        "token-pool",
        "token-ranking",
    ] {
        let entry = plan.entries.iter().find(|e| e.name() == name).unwrap();
        assert!(
            matches!(entry, super::super::PlanEntry::Skip { .. }),
            "{name} should be a skip when its secret is absent"
        );
    }
}

// ---- verify: daemon-readiness retry + strict workspace cwd (#5334) ----

#[test]
fn verify_cd_into_workspace_root_fails_loudly_instead_of_falling_through() {
    // A missing workspace root used to be swallowed by `|| true` and the
    // rest of the script ran from whatever cwd the SSH session started
    // in (never a registered workspace). It must now fail loudly, naming
    // the expected root.
    let script = render_verify("loom-workspaces/anvil", &["rjwalters/anvil".to_string()]);
    assert!(
        !script.contains("|| true"),
        "verify must not silently swallow a failed cd:\n{script}"
    );
    assert!(script.contains(r#"cd "$WORKSPACE_ROOT""#), "verify:\n{script}");
    assert!(script.contains("workspace root $WORKSPACE_ROOT not found"), "verify:\n{script}");
}

#[test]
fn verify_retries_daemon_status_with_a_bounded_wait() {
    let script = render_verify("loom-workspaces/anvil", &["rjwalters/anvil".to_string()]);
    assert!(
        script.contains("for _attempt in $(seq 1 15)"),
        "verify must bound its daemon-readiness retry loop:\n{script}"
    );
    assert!(script.contains("sleep 2"), "verify:\n{script}");
    assert!(
        script.contains("loom-daemon status did not become ready within ~30s"),
        "verify:\n{script}"
    );
}

#[test]
fn verify_rendered_script_is_valid_shell() {
    let script = render_verify("loom-workspaces/anvil", &["rjwalters/anvil".to_string()]);
    let output = Command::new("bash")
        .arg("-n")
        .arg("-c")
        .arg(&script)
        .output();
    match output {
        Ok(out) => assert!(
            out.status.success(),
            "rendered verify script failed `bash -n`:\n{}\nscript:\n{}",
            String::from_utf8_lossy(&out.stderr),
            script
        ),
        Err(e) => eprintln!("skipping bash -n check: could not launch bash ({e})"),
    }
}

#[test]
fn daemon_watchdog_step_reruns_loom_daemon_start_from_the_workspace_root() {
    // #5343: the step must cd into the SAME workspace root `daemon-unit`
    // pinned as WorkingDirectory=, then re-run the already-cloned
    // loom-daemon-start.sh (never a second hand-rolled watchdog
    // unit/timer renderer duplicating the shell version).
    let script = render_daemon_watchdog("loom-workspaces/anvil");
    assert!(
        script.contains(r#"cd "$HOME/loom-workspaces/anvil""#),
        "daemon-watchdog must cd into the primary workspace root:\n{script}"
    );
    assert!(
        script.contains("$HOME/.local/share/loom/defaults/scripts/cli/loom-daemon-start.sh"),
        "daemon-watchdog must re-run the already-cloned loom-daemon-start.sh:\n{script}"
    );
}

#[test]
fn daemon_watchdog_step_has_an_idempotent_check_and_follows_daemon_unit() {
    let config = base_config();
    let secrets = Secrets {
        pat: Some("pat".to_string()),
        accounts_env: Some("ACCOUNT_EMAIL_1=a@b.c".to_string()),
        ..Secrets::default()
    };
    let plan = build_plan(&config, &secrets);
    let names: Vec<&str> = plan.entries.iter().map(PlanEntry::name).collect();
    let unit_pos = names.iter().position(|n| *n == "daemon-unit").unwrap();
    let watchdog_pos = names.iter().position(|n| *n == "daemon-watchdog").unwrap();
    assert_eq!(
        watchdog_pos,
        unit_pos + 1,
        "daemon-watchdog must immediately follow daemon-unit"
    );

    let step = plan
        .entries
        .iter()
        .find_map(|e| match e {
            PlanEntry::Step(s) if s.name == "daemon-watchdog" => Some(s),
            _ => None,
        })
        .expect("daemon-watchdog step present");
    assert!(
        step.check
            .as_deref()
            .is_some_and(|c| c.contains("loom-daemon-watchdog.timer")),
        "daemon-watchdog needs an idempotent check gated on the timer unit"
    );
}

#[test]
fn daemon_watchdog_rendered_script_is_valid_shell() {
    let script = render_daemon_watchdog("loom-workspaces/anvil");
    let output = Command::new("bash")
        .arg("-n")
        .arg("-c")
        .arg(&script)
        .output();
    match output {
        Ok(out) => assert!(
            out.status.success(),
            "rendered daemon-watchdog script failed `bash -n`:\n{}\nscript:\n{}",
            String::from_utf8_lossy(&out.stderr),
            script
        ),
        Err(e) => eprintln!("skipping bash -n check: could not launch bash ({e})"),
    }
}

#[test]
fn verify_status_retry_loop_eventually_succeeds_past_early_failures() {
    // Functional check (not just string content): a `loom-daemon` that
    // fails its first two `status` calls (the exact startup race #5334
    // reports) and succeeds from the third must let the retry loop pass,
    // without needing to reach the full 15-attempt bound.
    let Some(bash) = which_bash() else {
        eprintln!("skipping: bash not available");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    // Stub `loom-daemon` at `${HOME}/.local/bin/loom-daemon` -- the
    // FIRST entry in `path_bootstrap::CANONICAL_PATH_DIRS`, which the
    // rendered script's own `export PATH=...` line always prepends
    // ahead of both the other canonical dirs (e.g. `/usr/local/bin`)
    // and the inherited `$PATH`. Placing the stub anywhere else lets a
    // real `loom-daemon` installed at a canonical path on the host
    // shadow it (#5577).
    let home = dir.path().join("home");
    let local_bin = home.join(".local/bin");
    std::fs::create_dir_all(&local_bin).unwrap();
    write_executable(
        &local_bin.join("loom-daemon"),
        r#"#!/bin/sh
if [ "$1" = "status" ]; then
  COUNTER_FILE="$STUB_STATE_DIR/status-calls"
  n=0
  [ -f "$COUNTER_FILE" ] && n="$(cat "$COUNTER_FILE")"
  n=$((n + 1))
  echo "$n" > "$COUNTER_FILE"
  if [ "$n" -lt 3 ]; then
    exit 1
  fi
  exit 0
fi
if [ "$1" = "workspace" ]; then
  echo '{"workspaces":["anvil"]}'
  exit 0
fi
exit 0
"#,
    );
    let repos = vec!["rjwalters/anvil".to_string()];
    let script = render_verify("loom-workspaces/anvil", &repos);
    // A tiny substitution so the test does not actually sleep 2s per
    // retry (still exercises the identical loop/branch structure).
    let fast_script = script.replace("sleep 2", "sleep 0.05");
    std::fs::create_dir_all(home.join("loom-workspaces/anvil")).unwrap();
    std::fs::create_dir_all(home.join(".loom/tokens")).unwrap();
    std::fs::write(home.join(".loom/tokens/.ranking"), "x").unwrap();
    let out = Command::new(bash)
        .arg("-c")
        .arg(&fast_script)
        .env("HOME", &home)
        .env("STUB_STATE_DIR", dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "verify should pass once status recovers:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

// ---- token-ranking: health gate + checklist note (#5334) --------------

#[test]
fn token_ranking_marks_the_checklist_note_with_the_prefix_extract_checklist_note_expects() {
    let script = render_token_ranking();
    assert!(
        script.contains(CHECKLIST_NOTE_PREFIX),
        "token-ranking must use the shared checklist-note marker:\n{script}"
    );
}

#[test]
fn token_ranking_rendered_script_is_valid_shell() {
    let script = render_token_ranking();
    let output = Command::new("bash")
        .arg("-n")
        .arg("-c")
        .arg(&script)
        .output();
    match output {
        Ok(out) => assert!(
            out.status.success(),
            "rendered token-ranking script failed `bash -n`:\n{}\nscript:\n{}",
            String::from_utf8_lossy(&out.stderr),
            script
        ),
        Err(e) => eprintln!("skipping bash -n check: could not launch bash ({e})"),
    }
}

#[test]
fn token_ranking_reports_the_available_split_and_succeeds_when_some_available() {
    let Some(bash) = which_bash() else {
        eprintln!("skipping: bash not available");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    // See `verify_status_retry_loop_eventually_succeeds_past_early_failures`
    // above: stub at `${HOME}/.local/bin/loom-daemon` so it wins over
    // any real `loom-daemon` reachable at a canonical PATH dir (#5577).
    let local_bin = dir.path().join(".local/bin");
    std::fs::create_dir_all(&local_bin).unwrap();
    write_executable(
            &local_bin.join("loom-daemon"),
            "#!/bin/sh\ncat <<'EOF'\nToken pool ranking (probed at 2026-08-04T00:00:00Z)\n====\nAccount  5h util  7d util  Status\n----\na-1  0.10  0.10  available\nb-2  0.20  0.20  available\nc-3  1.00  1.00  blocked\nd-4  1.00  1.00  exhausted\n\nTotal 4: 2 available, 1 blocked, 1 exhausted\nEOF\nexit 0\n",
        );
    let out = Command::new(bash)
        .arg("-c")
        .arg(render_token_ranking())
        .env("HOME", dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "a pool with some available accounts must not fail:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(&format!("{CHECKLIST_NOTE_PREFIX}token pool 2/4 accounts available")),
        "stdout: {stdout}"
    );
}

#[test]
fn token_ranking_fails_loudly_when_every_account_is_blocked() {
    // The exact 2026-08-04 loom-worker-2 incident: `tokens check
    // --ranking` itself exits 0 (nothing is `error`/`skipped`), but every
    // account is `blocked` — zero `available`. This must halt the run,
    // not report a green "changed".
    let Some(bash) = which_bash() else {
        eprintln!("skipping: bash not available");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    // See `verify_status_retry_loop_eventually_succeeds_past_early_failures`
    // above: stub at `${HOME}/.local/bin/loom-daemon` so it wins over
    // any real `loom-daemon` reachable at a canonical PATH dir (#5577).
    let local_bin = dir.path().join(".local/bin");
    std::fs::create_dir_all(&local_bin).unwrap();
    write_executable(
            &local_bin.join("loom-daemon"),
            "#!/bin/sh\ncat <<'EOF'\nToken pool ranking (probed at 2026-08-04T00:00:00Z)\n====\nAccount  5h util  7d util  Status\n----\na-1  1.00  1.00  blocked\nb-2  1.00  1.00  blocked\nc-3  1.00  1.00  blocked\nd-4  1.00  1.00  blocked\n\nTotal 4: 4 blocked\nEOF\nexit 0\n",
        );
    let out = Command::new(bash)
        .arg("-c")
        .arg(render_token_ranking())
        .env("HOME", dir.path())
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "an all-blocked pool must fail the step, not bootstrap silently"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("WARNING"), "stderr: {stderr}");
    assert!(stderr.contains("0/4"), "stderr: {stderr}");
}

/// Resolve a `bash` binary for the functional (actually-executes-shell)
/// tests above, or `None` when the test environment has no bash — mirrors
/// `machine_layout_rendered_script_is_valid_shell`'s skip-not-fail
/// posture for a bash-less CI image.
fn which_bash() -> Option<PathBuf> {
    // Resolve an *absolute* path up front (rather than the bare name
    // "bash") so that setting `HOME` on the child process (below, to
    // point the rendered script's canonical PATH lookup at the stub
    // `loom-daemon` under `${HOME}/.local/bin`) cannot also change
    // which `bash` binary gets exec'd.
    let out = Command::new("sh")
        .arg("-c")
        .arg("command -v bash")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
    }
}

/// Write an executable shell-script stub at `path` (mode 0755).
fn write_executable(path: &std::path::Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, contents).unwrap();
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).unwrap();
}

#[test]
fn safehouse_disabled_stays_a_single_skip_and_plain_worker_unchanged() {
    // AC: "Without --safehouse, behavior is unchanged: skip-with-notice,
    // plain worker, zero safehouse provisioning."
    let plan = build_plan(&base_config(), &Secrets::default());
    let names: Vec<&str> = plan.entries.iter().map(PlanEntry::name).collect();
    assert_eq!(names.iter().filter(|n| n.starts_with("safehouse")).count(), 1);
    let entry = plan
        .entries
        .iter()
        .find(|e| e.name() == "safehouse")
        .unwrap();
    match entry {
        PlanEntry::Skip { reason, .. } => assert!(
            reason.contains("not requested"),
            "reason should explain safehouse was not requested: {reason}"
        ),
        other => panic!("expected safehouse skip, got {other:?}"),
    }
}

// ---- safehouse enabled: real steps (#3998) --------------------------

fn safehouse_step_names() -> Vec<&'static str> {
    vec![
        "safehouse-tailscale-install",
        "safehouse-tailscale-join",
        "safehouse-build",
        "safehouse-config",
        "safehouse-room-invite",
        "safehouse-supervise",
        "safehouse-daemon-restart",
    ]
}

#[test]
fn safehouse_enabled_renders_full_step_sequence_in_order_between_idle_shutdown_and_verify() {
    let config = safehouse_config();
    let plan = build_plan(&config, &safehouse_secrets());
    let names: Vec<&str> = plan.entries.iter().map(PlanEntry::name).collect();

    // No bare "safehouse" skip entry remains once real steps render.
    assert!(!names.contains(&"safehouse"));

    let idle_pos = names.iter().position(|n| *n == "idle-shutdown").unwrap();
    let verify_pos = names.iter().position(|n| *n == "verify").unwrap();
    let safehouse_positions: Vec<usize> = safehouse_step_names()
        .iter()
        .map(|want| {
            names
                .iter()
                .position(|n| n == want)
                .unwrap_or_else(|| panic!("missing step {want}"))
        })
        .collect();

    // Exact ordering, and the whole block sits between idle-shutdown and verify.
    let mut sorted = safehouse_positions.clone();
    sorted.sort_unstable();
    assert_eq!(
        safehouse_positions, sorted,
        "safehouse steps must render in the documented order"
    );
    assert!(safehouse_positions
        .iter()
        .all(|&p| p > idle_pos && p < verify_pos));
}

#[test]
fn safehouse_config_step_precedes_supervise_step_boot_time_ordering_constraint() {
    // AC: the persona allowlist is written before safehoused's first
    // start (boot-time-only, no reload) — asserted directly on the
    // rendered plan's step order, not just by construction.
    let config = safehouse_config();
    let plan = build_plan(&config, &safehouse_secrets());
    let names: Vec<&str> = plan.entries.iter().map(PlanEntry::name).collect();
    let config_pos = names.iter().position(|n| *n == "safehouse-config").unwrap();
    let supervise_pos = names
        .iter()
        .position(|n| *n == "safehouse-supervise")
        .unwrap();
    assert!(
        config_pos < supervise_pos,
        "safehouse-config ({config_pos}) must precede safehouse-supervise ({supervise_pos})"
    );
}

#[test]
fn safehouse_every_step_has_a_check_for_idempotent_rerun() {
    // AC: a re-run on an already-provisioned host reports AlreadyDone —
    // that classification is driven entirely by a present `check`.
    let config = safehouse_config();
    let plan = build_plan(&config, &safehouse_secrets());
    for want in safehouse_step_names() {
        let step = plan
            .entries
            .iter()
            .find_map(|e| match e {
                PlanEntry::Step(s) if s.name == want => Some(s),
                _ => None,
            })
            .unwrap_or_else(|| panic!("missing step {want}"));
        assert!(step.check.is_some(), "{want} must have a check phase (idempotency)");
    }
}

#[test]
fn safehouse_full_plan_reruns_idempotently_when_every_check_passes() {
    struct AlwaysOkRunner;
    impl CommandRunner for AlwaysOkRunner {
        fn run(&self, _shell: &str, _stdin: Option<&str>) -> Result<CommandOutput> {
            Ok(CommandOutput {
                code: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }
    let config = safehouse_config();
    let plan = build_plan(&config, &safehouse_secrets());
    let reports = super::super::execute_plan(&AlwaysOkRunner, &plan);
    for name in safehouse_step_names() {
        let report = reports.iter().find(|r| r.name == name).unwrap();
        assert_eq!(
            report.status,
            StepStatus::AlreadyDone,
            "{name} should be AlreadyDone on a passing check"
        );
    }
}

#[test]
fn safehouse_tailscale_join_and_config_steps_carry_secret_stdin_not_in_rendered_text() {
    let config = safehouse_config();
    let secrets = safehouse_secrets();
    let plan = build_plan(&config, &secrets);

    for name in ["safehouse-tailscale-join", "safehouse-config"] {
        let step = plan
            .entries
            .iter()
            .find_map(|e| match e {
                PlanEntry::Step(s) if s.name == name => Some(s),
                _ => None,
            })
            .unwrap();
        let stdin = step.stdin.as_ref().expect("secret step must carry stdin");
        assert!(stdin.secret, "{name} stdin must be marked secret");
    }

    let dry = plan.render_dry_run("fleet add-worker", "worker-1");
    assert!(!dry.contains("tskey-auth-ephemeral-tagged"));
    assert!(!dry.contains("hunter2-matrix-pw"));
    assert!(!dry.contains("store-pass-xyz"));
    assert!(!dry.contains("recovery-pass-xyz"));
}

#[test]
fn safehouse_config_step_apply_references_secret_vars_not_values() {
    let config = safehouse_config();
    let secrets = safehouse_secrets();
    let plan = build_plan(&config, &secrets);
    let step = plan
        .entries
        .iter()
        .find_map(|e| match e {
            PlanEntry::Step(s) if s.name == "safehouse-config" => Some(s),
            _ => None,
        })
        .unwrap();
    // Non-secret operator values ARE interpolated.
    assert!(step.apply.contains("matrix.internal.example"));
    assert!(step.apply.contains("!fleet:matrix.internal.example"));
    assert!(step.apply.contains("loom_daemon"));
    // Secret values are referenced only via their sourced variable names.
    assert!(step.apply.contains("$SAFEHOUSE_MATRIX_USER_ID"));
    assert!(step.apply.contains("$SAFEHOUSE_MATRIX_PASSWORD"));
    assert!(step.apply.contains("$SAFEHOUSE_STORE_PASSPHRASE"));
    assert!(step.apply.contains("$SAFEHOUSE_RECOVERY_PASSPHRASE"));
    assert!(!step.apply.contains("hunter2-matrix-pw"));
}

#[test]
fn safehouse_room_invite_uses_the_daemon_side_op_not_raw_cs_api() {
    let config = safehouse_config();
    let plan = build_plan(&config, &safehouse_secrets());
    let step = plan
        .entries
        .iter()
        .find_map(|e| match e {
            PlanEntry::Step(s) if s.name == "safehouse-room-invite" => Some(s),
            _ => None,
        })
        .unwrap();
    assert!(step.apply.contains("safehoused invite"));
    assert!(!step.apply.to_lowercase().contains("client-server"));
    assert!(!step.apply.contains("/_matrix/client"));

    // An operator override replaces the default invocation.
    let mut overridden = safehouse_config();
    overridden.safehouse_invite_exec = Some("safehoused invite --room-override x".to_string());
    let plan2 = build_plan(&overridden, &safehouse_secrets());
    let step2 = plan2
        .entries
        .iter()
        .find_map(|e| match e {
            PlanEntry::Step(s) if s.name == "safehouse-room-invite" => Some(s),
            _ => None,
        })
        .unwrap();
    assert!(step2.apply.contains("--room-override x"));
}

#[test]
fn safehouse_supervise_step_installs_via_the_shared_service_script_and_lingers() {
    let config = safehouse_config();
    let plan = build_plan(&config, &safehouse_secrets());
    let step = plan
        .entries
        .iter()
        .find_map(|e| match e {
            PlanEntry::Step(s) if s.name == "safehouse-supervise" => Some(s),
            _ => None,
        })
        .unwrap();
    assert!(step.apply.contains("safehoused-service.sh"));
    assert!(step.apply.contains("install"));
    assert!(step.apply.contains("enable-linger"));
}

#[test]
fn safehouse_daemon_restart_step_wires_env_and_restarts() {
    let config = safehouse_config();
    let plan = build_plan(&config, &safehouse_secrets());
    let step = plan
        .entries
        .iter()
        .find_map(|e| match e {
            PlanEntry::Step(s) if s.name == "safehouse-daemon-restart" => Some(s),
            _ => None,
        })
        .unwrap();
    assert!(step.apply.contains("LOOM_SAFEHOUSE_ENABLED=true"));
    assert!(step.apply.contains("LOOM_SAFEHOUSE_SOCKET"));
    assert!(step
        .apply
        .contains("LOOM_SAFEHOUSE_ROOM=!fleet:matrix.internal.example"));
    assert!(step
        .apply
        .contains("systemctl --user restart loom-daemon.service"));
}

// ---- safehouse preflight ---------------------------------------------

#[test]
fn preflight_safehouse_enabled_requires_every_input() {
    let mut config = base_config();
    config.safehouse_enabled = true;
    let err = preflight(&config).unwrap_err().to_string();
    assert!(err.contains("--safehouse-tailnet-auth-key-file"), "err: {err}");
    assert!(err.contains("--safehouse-secrets-file"), "err: {err}");
    assert!(err.contains("--safehouse-homeserver-url"), "err: {err}");
    assert!(err.contains("--safehouse-room"), "err: {err}");
    assert!(err.contains("--safehouse-persona"), "err: {err}");
}

#[test]
fn preflight_safehouse_enabled_with_full_inputs_reads_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let key_file = dir.path().join("tailnet.key");
    let secrets_file = dir.path().join("safehouse.env");
    std::fs::write(&key_file, "tskey-ephemeral-tagged\n").unwrap();
    std::fs::write(
        &secrets_file,
        "SAFEHOUSE_MATRIX_USER_ID=@w1:example\nSAFEHOUSE_MATRIX_PASSWORD=pw\n\
             SAFEHOUSE_STORE_PASSPHRASE=sp\nSAFEHOUSE_RECOVERY_PASSPHRASE=rp\n",
    )
    .unwrap();

    let mut config = safehouse_config();
    config.safehouse_tailnet_auth_key_file = Some(key_file);
    config.safehouse_secrets_file = Some(secrets_file);

    let secrets = preflight(&config).unwrap();
    assert_eq!(secrets.safehouse_tailnet_auth_key.as_deref(), Some("tskey-ephemeral-tagged"));
    assert!(secrets
        .safehouse_secrets
        .as_ref()
        .unwrap()
        .contains("SAFEHOUSE_MATRIX_USER_ID"));
}

#[test]
fn preflight_rejects_invalid_persona_name() {
    let mut config = safehouse_config();
    config.safehouse_personas = vec!["Not-Valid!".to_string()];
    let err = preflight(&config).unwrap_err().to_string();
    assert!(err.contains("safehouse-persona"), "err: {err}");
}

#[test]
fn preflight_rejects_unsafe_homeserver_url_and_room() {
    let mut config = safehouse_config();
    config.safehouse_homeserver_url = Some("https://evil; rm -rf /".to_string());
    assert!(preflight(&config).is_err());

    let mut config2 = safehouse_config();
    config2.safehouse_room = Some("!room`whoami`:example".to_string());
    assert!(preflight(&config2).is_err());
}

#[test]
fn preflight_safehouse_disabled_ignores_missing_safehouse_inputs() {
    // AC: without --safehouse, nothing about safehouse gates preflight.
    let config = base_config();
    assert!(preflight(&config).is_ok());
}

#[test]
fn idle_shutdown_step_present_only_when_configured() {
    // Absent → skip.
    let plan = build_plan(&base_config(), &Secrets::default());
    let entry = plan
        .entries
        .iter()
        .find(|e| e.name() == "idle-shutdown")
        .unwrap();
    assert!(matches!(entry, super::super::PlanEntry::Skip { .. }));

    // Present → executable step whose apply renders the minute limit.
    let mut config = base_config();
    config.idle_shutdown_minutes = Some(45);
    let plan = build_plan(&config, &Secrets::default());
    let step = plan
        .entries
        .iter()
        .find_map(|e| match e {
            super::super::PlanEntry::Step(s) if s.name == "idle-shutdown" => Some(s),
            _ => None,
        })
        .unwrap();
    assert!(step.apply.contains("LIMIT=45"));
}

// ---- idle-shutdown guard: ask the daemon, don't veto on bare presence
// (#5565) ---------------------------------------------------------------

#[test]
fn idle_shutdown_guard_no_longer_vetoes_on_bare_daemon_process_presence() {
    // The whole point of #5565: a `pgrep -f loom-daemon` (or equivalent)
    // veto is essentially always true under the fleet's own
    // `Restart=on-success` systemd supervision, making
    // `--idle-shutdown-minutes` a no-op. It must be gone.
    let script = render_idle_shutdown(60);
    assert!(
        !script.contains("pgrep -f"),
        "guard must not veto on bare daemon-process presence any more:\n{script}"
    );
    assert!(
        script.contains(r#""eligible""#),
        "guard must query the daemon's own idle-exit eligibility:\n{script}"
    );
}

#[test]
fn idle_shutdown_rendered_script_is_valid_shell() {
    let script = render_idle_shutdown(60);
    let output = Command::new("bash")
        .arg("-n")
        .arg("-c")
        .arg(&script)
        .output();
    match output {
        Ok(out) => assert!(
            out.status.success(),
            "rendered idle-shutdown script failed `bash -n`:\n{}\nscript:\n{}",
            String::from_utf8_lossy(&out.stderr),
            script
        ),
        Err(e) => eprintln!("skipping bash -n check: could not launch bash ({e})"),
    }
}

/// Extract the body of the `<<'GUARD' ... GUARD` heredoc so the
/// functional tests below exercise only the guard's OWN busy/idle logic
/// — never the mkdir/cat/chmod/crontab installer wrapped around it.
fn idle_guard_body(minutes: u32) -> String {
    let rendered = render_idle_shutdown(minutes);
    let start_marker = "<<'GUARD'\n";
    let start = rendered.find(start_marker).expect("GUARD heredoc start") + start_marker.len();
    let end = rendered[start..]
        .find("\nGUARD\n")
        .expect("GUARD heredoc end");
    rendered[start..start + end].to_string()
}

/// Run the extracted guard body under a fully-stubbed PATH (`pgrep`,
/// `loom-daemon`, `sudo`) so the poweroff decision is driven only by the
/// canned `loom-daemon status --json` response and the pre-seeded
/// `$STAMP` age — never by real processes on the test host (a genuine
/// `claude`/`loom-daemon` process IS running while this very sweep
/// executes, which would otherwise false-positive a real `pgrep`).
/// Returns whether the stubbed `sudo systemctl poweroff` was invoked.
fn run_idle_guard(
    body: &str,
    status_exit_ok: bool,
    status_json: &str,
    stamp_age_minutes: i64,
) -> bool {
    let Some(bash) = which_bash() else {
        eprintln!("skipping: bash not available");
        return false;
    };
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let stub_bin = home.join(".local/bin");
    std::fs::create_dir_all(home.join(".loom")).unwrap();
    std::fs::create_dir_all(&stub_bin).unwrap();

    // Pre-seed the "last active" stamp so the cron guard's OWN elapsed-
    // idle window is already satisfied — isolates the test from real
    // wall-clock sleeps (this is a separate timer from the daemon's own
    // `idle_minutes`, see `render_idle_shutdown`'s doc comment).
    let stamp = chrono::Utc::now().timestamp() - stamp_age_minutes * 60;
    std::fs::write(home.join(".loom/last-active"), stamp.to_string()).unwrap();

    // The guard's own `{export_line}` rewrites `$PATH` to put
    // `"${HOME}/.local/bin"` FIRST (`path_bootstrap::canonical_path_export_line`),
    // ahead of whatever the test process's own `$PATH` already contained
    // — so stubs must live THERE to actually shadow the real system
    // `pgrep`/`loom-daemon`/`sudo` (a bare tempdir prepended to the
    // process `$PATH` gets pushed behind `/usr/bin` etc. by that
    // rewrite and is never consulted).
    write_executable(&stub_bin.join("pgrep"), "#!/bin/sh\nexit 1\n");
    let daemon_exit = if status_exit_ok { 0 } else { 1 };
    write_executable(
            &stub_bin.join("loom-daemon"),
            &format!(
                "#!/bin/sh\nif [ \"$1\" = \"status\" ]; then cat <<'JSON'\n{status_json}\nJSON\nexit {daemon_exit}\nfi\nexit 1\n"
            ),
        );
    write_executable(
        &stub_bin.join("sudo"),
        &format!("#!/bin/sh\necho \"$@\" >> \"{}/sudo-calls\"\nexit 0\n", dir.path().display()),
    );

    let out = Command::new(bash)
        .arg("-c")
        .arg(body)
        .env("PATH", format!("{}:{}", stub_bin.display(), std::env::var("PATH").unwrap()))
        .env("HOME", &home)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "guard script itself must exit 0:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    dir.path().join("sudo-calls").exists()
}

#[test]
fn idle_guard_powers_off_when_daemon_reports_eligible_even_though_daemon_is_up() {
    // Regression test (#5565 AC1): the daemon process IS reachable/alive
    // (the stub answers `status`), which under the OLD `pgrep -f
    // loom-daemon` veto would always have blocked poweroff. With that
    // veto replaced by an eligibility query, `"eligible": true` must let
    // the guard proceed once its own elapsed-window check also passes.
    let body = idle_guard_body(1);
    let powered_off = run_idle_guard(
        &body,
        true,
        r#"{"in_flight_count":0,"idle_exit":{"enabled":true,"eligible":true,"trigger":"idle"}}"#,
        10,
    );
    assert!(powered_off, "guard must power off when the daemon reports eligible");
}

#[test]
fn idle_guard_vetoes_when_daemon_reports_not_eligible() {
    // Regression test (#5565 AC2): daemon reachable and explicitly
    // reports busy (not yet eligible) — the guard must still veto.
    let body = idle_guard_body(1);
    let powered_off = run_idle_guard(
        &body,
        true,
        r#"{"in_flight_count":2,"idle_exit":{"enabled":true,"eligible":false}}"#,
        10,
    );
    assert!(!powered_off, "guard must veto while the daemon reports not-eligible");
}

#[test]
fn idle_guard_falls_back_to_in_flight_count_for_a_daemon_too_old_to_report_eligibility() {
    // A daemon predating #5565 answers `status --json` with no
    // `idle_exit`/`eligible` field at all. The guard must fall back to
    // the raw in-flight-sweep count rather than either always-veto or
    // (dangerously) always-allow.
    let body = idle_guard_body(1);
    let powered_off_while_busy = run_idle_guard(&body, true, r#"{"in_flight_count":3}"#, 10);
    assert!(!powered_off_while_busy, "must veto: in_flight_count > 0");

    let powered_off_while_idle = run_idle_guard(&body, true, r#"{"in_flight_count":0}"#, 10);
    assert!(powered_off_while_idle, "must allow poweroff: in_flight_count == 0, old stamp");
}

#[test]
fn idle_guard_falls_back_to_claude_check_when_daemon_is_genuinely_unreachable() {
    // Regression test (#5565 AC3): the daemon is genuinely down (status
    // call fails, e.g. socket gone) — not idle, just absent. The guard
    // has nothing to ask, so it must fall back to the existing `claude`
    // process check rather than hanging or defaulting either way on the
    // daemon's own say-so.
    let body = idle_guard_body(1);

    // No claude running, daemon unreachable, old stamp -> poweroff.
    let powered_off = run_idle_guard(&body, false, "", 10);
    assert!(powered_off, "an unreachable daemon must not itself veto poweroff");
}

#[test]
fn idle_shutdown_step_check_and_crontab_wiring_unchanged() {
    // #5565 must not touch the OUTER installer wrapper (idempotency
    // check, crontab install line) — only the guard body's own
    // busy/idle logic.
    let mut config = base_config();
    config.idle_shutdown_minutes = Some(45);
    let plan = build_plan(&config, &Secrets::default());
    let step = plan
        .entries
        .iter()
        .find_map(|e| match e {
            super::super::PlanEntry::Step(s) if s.name == "idle-shutdown" => Some(s),
            _ => None,
        })
        .unwrap();
    assert!(step.apply.contains("crontab -"));
    assert!(step.apply.contains("loom-idle-shutdown.sh"));
}

// ---- build_worker_record / #4697 registry fields ----------------------

#[test]
fn worker_record_carries_configured_idle_shutdown_window() {
    // #4697 AC 3's prerequisite: `fleet status` can only tell an EXPECTED
    // power-off from an outage if the window the guard was installed with
    // is persisted on the record at bootstrap time.
    let mut config = base_config();
    config.idle_shutdown_minutes = Some(45);
    let now = chrono::Utc::now();

    let record = build_worker_record(&config, Some(true), now);
    assert_eq!(record.idle_shutdown_minutes, Some(45));
    // A successful bootstrap observed the host up over SSH moments ago, so
    // the heuristic has a reference point from the very first poll.
    assert_eq!(record.last_seen_up_at.as_deref(), Some(now.to_rfc3339()).as_deref());
}

#[test]
fn worker_record_leaves_idle_shutdown_absent_when_not_configured() {
    // Mirrors `render_idle_shutdown()`'s gate: no --idle-shutdown-minutes
    // => no guard installed => nothing for the heuristic to compare
    // against, so the host stays UNREACHABLE (never "expected") when
    // silent.
    let config = base_config();
    assert!(config.idle_shutdown_minutes.is_none());
    let record = build_worker_record(&config, Some(true), chrono::Utc::now());
    assert_eq!(record.idle_shutdown_minutes, None);
}

#[test]
fn worker_record_roundtrips_idle_shutdown_fields_through_the_registry() {
    // The registry file is the only carrier between `add-worker` (write)
    // and a LATER `fleet status` process (read), so the #4697 fields must
    // survive a real save/load — and a record written before they existed
    // must still load (the `#[serde(default)]` backward-compat pattern).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fleet.json");

    let mut config = base_config();
    config.idle_shutdown_minutes = Some(30);
    let record = build_worker_record(&config, Some(true), chrono::Utc::now());
    let expected_last_seen = record.last_seen_up_at.clone();

    let mut registry = FleetRegistry::default();
    registry.upsert(record);
    registry.save(&path).unwrap();

    let loaded = FleetRegistry::load(&path).unwrap();
    let w = loaded.get(&config.ssh_host).unwrap();
    assert_eq!(w.idle_shutdown_minutes, Some(30));
    assert_eq!(w.last_seen_up_at, expected_last_seen);

    // Pre-#4697 record: both fields absent from the JSON entirely.
    std::fs::write(
        &path,
        r#"{ "version": 1, "workers": [ { "ssh_host": "legacy", "bootstrapped_at": "t" } ] }"#,
    )
    .unwrap();
    let legacy = FleetRegistry::load(&path).unwrap();
    let w = legacy.get("legacy").unwrap();
    assert_eq!(w.idle_shutdown_minutes, None);
    assert_eq!(w.last_seen_up_at, None);
}

#[test]
fn daemon_unit_pins_workingdirectory_with_4292_marker() {
    // AC 4: the #4292 token-pool cwd workaround must be marked with its
    // tracking issue so it is removed when #4292 lands.
    let config = base_config();
    let plan = build_plan(&config, &Secrets::default());
    let step = plan
        .entries
        .iter()
        .find_map(|e| match e {
            super::super::PlanEntry::Step(s) if s.name == "daemon-unit" => Some(s),
            _ => None,
        })
        .unwrap();
    assert!(step
        .apply
        .contains("WorkingDirectory=%h/loom-workspaces/anvil"));
    assert!(step.apply.contains("#4292"), "workaround must be marked with #4292");
    assert!(step.apply.contains("Restart=on-success"));
    assert!(step.apply.contains("enable-linger"));
}

#[test]
fn daemon_unit_sets_supervisor_env_and_correct_restart_policy() {
    // #4640: without LOOM_DAEMON_SUPERVISOR=systemd, detect_supervisor()
    // (ipc.rs) can't tell the fleet daemon is systemd-supervised, so
    // `restart --drain` refuses on every fleet worker. Restart=on-failure
    // additionally inverts the exit-code contract: it never relaunches on
    // the restart primitive's clean exit 0, and (had it been changed to
    // `always` instead) would incorrectly relaunch on EXIT_SHUTDOWN (143).
    // Restart=on-success mirrors the canonical `render_systemd_unit()` in
    // loom-daemon-start.sh (#4268) and gets both right.
    let config = base_config();
    let plan = build_plan(&config, &Secrets::default());
    let step = plan
        .entries
        .iter()
        .find_map(|e| match e {
            super::super::PlanEntry::Step(s) if s.name == "daemon-unit" => Some(s),
            _ => None,
        })
        .unwrap();
    assert!(
        step.apply
            .contains("Environment=LOOM_DAEMON_SUPERVISOR=systemd"),
        "rendered unit must set LOOM_DAEMON_SUPERVISOR=systemd so detect_supervisor() \
             recognizes the fleet daemon as supervised"
    );
    assert!(
        step.apply.contains("Restart=on-success"),
        "rendered unit must use Restart=on-success (the EXIT_RESTART/EXIT_SIGINT/\
             EXIT_SHUTDOWN contract), not Restart=on-failure or Restart=always"
    );
    assert!(
        !step.apply.contains("Restart=on-failure"),
        "the old Restart=on-failure policy must be fully replaced"
    );
    assert!(
        step.summary.contains("Restart=on-success"),
        "step summary must match the rendered policy"
    );
}

/// #5119: `Restart=on-success` is necessary but NOT sufficient — it can only
/// fire on `Result=success`. Under the systemd default
/// `KillMode=control-group`, a clean exit(0) with lingering sweep/role-run
/// children in the unit's cgroup is reclassified `Result=timeout` after the
/// full `TimeoutStopSec`, the unit lands in `failed`, and the relaunch never
/// happens (the 2026-08-03 loom-worker-1 outage). The canonical renderer
/// (`render_systemd_unit()` in loom-daemon-start.sh) has carried
/// `KillMode=mixed` since #4862 and `TimeoutStopSec=20` since #4950; the
/// fleet-worker template silently did not, so every provisioned worker got
/// the broken shape.
#[test]
fn daemon_unit_carries_the_killmode_and_stop_timeout_fixes() {
    let config = base_config();
    let plan = build_plan(&config, &Secrets::default());
    let step = plan
        .entries
        .iter()
        .find_map(|e| match e {
            super::super::PlanEntry::Step(s) if s.name == "daemon-unit" => Some(s),
            _ => None,
        })
        .unwrap();
    assert!(
        step.apply.contains("KillMode=mixed"),
        "fleet worker unit must carry #4862's KillMode=mixed, or Restart=on-success \
             never fires when the cgroup still holds sweep/role-run children"
    );
    assert!(
        step.apply.contains("TimeoutStopSec=20"),
        "fleet worker unit must carry #4950's TimeoutStopSec=20 fast-failure backstop \
             instead of dragging out systemd's 90s default"
    );
}

/// #4831: `daemon-unit`'s `Environment=PATH=` used to be a THIRD,
/// narrower hand-hardcoded set
/// (`%h/.local/bin:/usr/local/bin:/usr/bin:/bin`, missing
/// `%h/.cargo/bin` and Homebrew) that disagreed with both
/// `resolve_plist_path()` (loom-daemon-start.sh) and this file's own
/// provisioning `export PATH=` lines. It must now render the FULL
/// shared canonical superset (`path_bootstrap::canonical_path_systemd`),
/// byte-for-byte, not a hand-picked subset.
#[test]
fn daemon_unit_path_is_the_full_canonical_systemd_superset() {
    let config = base_config();
    let plan = build_plan(&config, &Secrets::default());
    let step = plan
        .entries
        .iter()
        .find_map(|e| match e {
            super::super::PlanEntry::Step(s) if s.name == "daemon-unit" => Some(s),
            _ => None,
        })
        .unwrap();
    let want =
        format!("Environment=PATH={}", super::super::path_bootstrap::canonical_path_systemd());
    assert!(
        step.apply.contains(&want),
        "daemon-unit must render the full canonical systemd PATH ({want}), got: {}",
        step.apply
    );
    assert!(
        step.apply.contains("%h/.cargo/bin"),
        "the pre-#4831 narrower systemd PATH omitted %h/.cargo/bin -- must be present now"
    );
    assert!(
        step.apply.contains("/opt/homebrew/bin"),
        "the pre-#4831 narrower systemd PATH omitted Homebrew -- must be present now"
    );
}

/// #4831: every one of the ~12 duplicated `export PATH="$HOME/.local/bin:
/// $PATH"` provisioning lines omitted `${HOME}/.cargo/bin` and
/// `/opt/homebrew/bin`. Spot-check a representative sample of rendered
/// steps (spanning machine layout, forge auth, token bootstrap, and
/// workspace registration) to prove they all now render the FULL
/// canonical export line, not just one fixed-up call site.
#[test]
fn provisioning_steps_export_the_full_canonical_path_not_a_narrower_subset() {
    let config = base_config();
    // forge-auth/token-pool/token-ranking only render as Steps (not
    // Skips) when their secrets are present -- mirrors
    // forge_auth_and_token_accounts_carry_secret_stdin above.
    let secrets = Secrets {
        pat: Some("the-pat".to_string()),
        accounts_env: Some("ACCOUNT_EMAIL_1=a@b.c".to_string()),
        ..Secrets::default()
    };
    let plan = build_plan(&config, &secrets);
    let export_line = super::super::path_bootstrap::canonical_path_export_line();
    for name in [
        "machine-layout",
        "forge-auth",
        "token-pool",
        "token-ranking",
        "workspace-clone",
        "workspace-register",
    ] {
        let step = plan
            .entries
            .iter()
            .find_map(|e| match e {
                super::super::PlanEntry::Step(s) if s.name == name => Some(s),
                _ => None,
            })
            .unwrap_or_else(|| panic!("expected a step named {name}"));
        assert!(
            step.apply.contains(export_line.trim_end()),
            "step {name} must export the full canonical PATH ({}), got: {}",
            export_line.trim_end(),
            step.apply
        );
        assert!(
            step.apply.contains("${HOME}/.cargo/bin"),
            "step {name} is missing ${{HOME}}/.cargo/bin -- the pre-#4831 duplicated \
                 export lines omitted this"
        );
        assert!(
            step.apply.contains("/opt/homebrew/bin"),
            "step {name} is missing /opt/homebrew/bin -- the pre-#4831 duplicated export \
                 lines omitted this"
        );
    }
}

#[test]
fn no_python_loom_tools_or_pip_step_anywhere() {
    // #4228 landed — no interim Python install may appear (AC 4).
    let config = base_config();
    let secrets = Secrets {
        pat: Some("pat".to_string()),
        accounts_env: Some("ACCOUNT_EMAIL_1=a@b.c".to_string()),
        ..Secrets::default()
    };
    let plan = build_plan(&config, &secrets);
    for entry in &plan.entries {
        if let super::super::PlanEntry::Step(s) = entry {
            let hay = format!("{}\n{}", s.apply, s.check.clone().unwrap_or_default());
            assert!(!hay.contains("break-system-packages"), "step {} has pip step", s.name);
            assert!(!hay.contains("loom_tools"), "step {} references python loom_tools", s.name);
            assert!(!hay.contains("pip install"), "step {} has pip install", s.name);
        }
    }
}

#[test]
fn workspace_register_uses_the_priority() {
    let mut config = base_config();
    config.priority = 7;
    config.repos = vec!["a/anvil".to_string(), "b/repo2".to_string()];
    let plan = build_plan(&config, &Secrets::default());
    let step = plan
        .entries
        .iter()
        .find_map(|e| match e {
            super::super::PlanEntry::Step(s) if s.name == "workspace-register" => Some(s),
            _ => None,
        })
        .unwrap();
    assert!(step.apply.contains("--priority 7"));
    assert!(step.apply.contains("loom-workspaces/anvil"));
    assert!(step.apply.contains("loom-workspaces/repo2"));
}

#[test]
fn dry_run_render_is_stable_and_lists_all_steps() {
    let config = base_config();
    let secrets = Secrets {
        pat: Some("pat".to_string()),
        accounts_env: Some("env".to_string()),
        ..Secrets::default()
    };
    let plan = build_plan(&config, &secrets);
    let out = plan.render_dry_run("fleet add-worker", &config.ssh_host);
    assert!(out.contains("14 steps"));
    assert!(out.contains("base-deps"));
    assert!(out.contains("verify"));
    assert!(out.contains("feeds a secret via stdin"));
}
