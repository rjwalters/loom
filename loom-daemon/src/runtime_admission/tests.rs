use super::*;
use std::process::Command;
use std::time::Duration;

/// RAII guard that clears the ambient `LOOM_RUNTIME` env var for the
/// scope of a test and restores whatever value (if any) it previously
/// had — including across a mid-test assertion panic, since Rust
/// unwinds through `Drop`. Some host/dev-container shells export
/// `LOOM_RUNTIME` (as the `spawn-worker.sh` runtime selector), and
/// without this guard that ambient value silently outranks the
/// `runtimes.default` / `runtimes.roles` config precedence this test
/// exercises (#4739).
struct ClearedLoomRuntimeEnv(Option<String>);

impl ClearedLoomRuntimeEnv {
    fn new() -> Self {
        let prior = std::env::var("LOOM_RUNTIME").ok();
        std::env::remove_var("LOOM_RUNTIME");
        Self(prior)
    }
}

impl Drop for ClearedLoomRuntimeEnv {
    fn drop(&mut self) {
        match self.0.take() {
            Some(v) => std::env::set_var("LOOM_RUNTIME", v),
            None => std::env::remove_var("LOOM_RUNTIME"),
        }
    }
}

fn fixture() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    for sub in ["defaults/roles", "defaults/runtimes", "defaults/scripts"] {
        fs::create_dir_all(d.path().join(sub)).unwrap();
    }
    fs::write(d.path().join("defaults/roles/curator.json"), "{}").unwrap();
    fs::write(d.path().join("defaults/roles/judge.json"), r#"{"runtimeRequirements":["mcp"]}"#)
        .unwrap();
    fs::write(
        d.path().join("defaults/roles/builder.json"),
        r#"{"runtimeRequirements":["worktreeIsolation","mcp"]}"#,
    )
    .unwrap();
    for (name, isolation) in [("claude", "yes"), ("codex", "partial")] {
        fs::write(
                d.path().join(format!("defaults/runtimes/{name}.json")),
                format!(r#"{{"runtime":"{name}","capabilities":{{"mcp":"yes","worktreeIsolation":"{isolation}"}}}}"#),
            ).unwrap();
        let adapter = d.path().join(format!("defaults/scripts/spawn-{name}.sh"));
        fs::write(&adapter, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(adapter, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    d
}

#[test]
fn precedence_and_empty_values() {
    assert_eq!(
        choose_runtime(Some("codex"), Some("claude"), Some("claude"), None, None),
        ("codex".into(), RuntimeSource::Explicit)
    );
    assert_eq!(
        choose_runtime(Some(" "), Some("codex"), Some("claude"), None, None),
        ("codex".into(), RuntimeSource::RoleEnvironment)
    );
    assert_eq!(
        choose_runtime(None, Some(""), Some("codex"), None, None),
        ("codex".into(), RuntimeSource::GlobalEnvironment)
    );
    assert_eq!(
        choose_runtime(None, None, None, Some("codex".into()), Some("claude".into())),
        ("codex".into(), RuntimeSource::RoleConfig)
    );
    assert_eq!(
        choose_runtime(None, None, None, Some("".into()), Some("codex".into())),
        ("codex".into(), RuntimeSource::DefaultConfig)
    );
    assert_eq!(
        choose_runtime(None, None, None, None, None),
        ("claude".into(), RuntimeSource::BuiltIn)
    );
}

/// #6201: [`suggested_worker_type_mismatch_warning`] fires only when the
/// role manifest declares a preference, the admitted runtime diverges
/// from it, AND something actually overrode the built-in default — never
/// when there is nothing declared, never when they already agree, and
/// never for the honest zero-config default (the `RuntimeSource::BuiltIn`
/// carve-out that keeps Builder's aspirational `"codex"` hint silent on
/// every ordinary `claude`-runtime dispatch — see the doc comment on
/// [`suggested_worker_type_mismatch_warning`] for why).
#[test]
fn suggested_worker_type_mismatch_warning_fires_only_on_real_divergence() {
    let admitted =
        |suggested: Option<&str>, runtime: &str, source: RuntimeSource| ResolvedRuntime {
            role: "curator".into(),
            runtime: runtime.into(),
            source,
            adapter: PathBuf::from("/adapter"),
            role_manifest: PathBuf::from("/role.json"),
            runtime_manifest: PathBuf::from("/runtime.json"),
            suggested_worker_type: suggested.map(str::to_string),
        };
    // No declared suggestion at all -> nothing to warn about.
    assert!(suggested_worker_type_mismatch_warning(&admitted(
        None,
        "codex",
        RuntimeSource::DefaultConfig
    ))
    .is_none());
    // Declared and matches admitted -> silent.
    assert!(suggested_worker_type_mismatch_warning(&admitted(
        Some("claude"),
        "claude",
        RuntimeSource::BuiltIn
    ))
    .is_none());
    // The zero-config built-in default diverging from an aspirational
    // suggestion (Builder's real-world "codex" hint while it actually
    // runs on "claude") is NOT a redirect -> silent, regardless of the
    // mismatch, because nothing overrode anything.
    assert!(suggested_worker_type_mismatch_warning(&admitted(
        Some("codex"),
        "claude",
        RuntimeSource::BuiltIn
    ))
    .is_none());
    // Declared "claude" but admitted onto "codex" via a broad default ->
    // loud, names the role, both runtimes, and the winning source.
    let msg = suggested_worker_type_mismatch_warning(&admitted(
        Some("claude"),
        "codex",
        RuntimeSource::DefaultConfig,
    ))
    .expect("mismatch must warn");
    assert!(msg.contains("curator"), "{msg}");
    assert!(msg.contains("suggestedWorkerType=\"claude\""), "{msg}");
    assert!(msg.contains("runtime=\"codex\""), "{msg}");
    assert!(msg.contains("default-config"), "{msg}");
    // Even an EXPLICIT override away from the declared suggestion still
    // warns — an operator deliberately overriding should still see the
    // divergence named, not just an unintentional broad-default redirect.
    assert!(suggested_worker_type_mismatch_warning(&admitted(
        Some("claude"),
        "codex",
        RuntimeSource::Explicit
    ))
    .is_some());
}

#[test]
fn sweep_is_one_runtime_gated_by_builder() {
    let d = fixture();
    let e = resolve_and_admit(d.path(), "sweep-lifecycle", Some("codex")).unwrap_err();
    assert_eq!(e.role, "sweep-lifecycle");
    assert_eq!(e.unmet_capabilities, vec!["worktreeIsolation"]);
}

#[test]
fn shipped_safe_codex_roles_are_admitted() {
    let d = fixture();
    assert_eq!(
        resolve_and_admit(d.path(), "issue-curator", Some("codex"))
            .unwrap()
            .role,
        "curator"
    );
    assert_eq!(
        resolve_and_admit(d.path(), "judge", Some("codex"))
            .unwrap()
            .runtime,
        "codex"
    );
}

/// Issue #8561: `defaults/runtimes/kimi.json` declares every capability
/// `"no"` (no on-disk copy is written by [`fixture`], so this exercises
/// the compiled-in [`BUNDLED_RUNTIME_MANIFESTS`] fallback). Curator
/// declares no `runtimeRequirements` at all, so it is trivially
/// compatible; Builder and Judge each require at least one capability
/// Kimi declares `"no"`, so both fail closed at 78 — the same mechanism
/// that already keeps Codex's `worktreeIsolation: "partial"` and the
/// aider tier-3 example out of those roles.
#[test]
fn kimi_is_admitted_only_for_roles_with_no_runtime_requirements() {
    let d = fixture();
    assert_eq!(
        resolve_and_admit(d.path(), "curator", Some("kimi"))
            .unwrap()
            .runtime,
        "kimi"
    );
    let e = resolve_and_admit(d.path(), "builder", Some("kimi")).unwrap_err();
    assert_eq!(e.reason, "unmet capabilities: worktreeIsolation, mcp");
    let e = resolve_and_admit(d.path(), "judge", Some("kimi")).unwrap_err();
    assert_eq!(e.unmet_capabilities, vec!["mcp"]);
}

#[test]
fn malformed_missing_and_unknown_values_fail_closed() {
    let d = fixture();
    fs::write(
        d.path().join("defaults/runtimes/bad.json"),
        r#"{"runtime":"bad","capabilities":{"mcp":"maybe"}}"#,
    )
    .unwrap();
    let adapter = d.path().join("defaults/scripts/spawn-bad.sh");
    fs::write(&adapter, "#!/bin/sh\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&adapter, fs::Permissions::from_mode(0o755)).unwrap();
    }
    assert!(resolve_and_admit(d.path(), "judge", Some("bad"))
        .unwrap_err()
        .unmet_capabilities
        .contains(&"mcp".to_string()));
    assert!(resolve_and_admit(d.path(), "judge", Some("missing"))
        .unwrap_err()
        .reason
        .contains("adapter"));
    assert!(resolve_and_admit(d.path(), "not-a-role", Some("claude"))
        .unwrap_err()
        .reason
        .contains("unknown role"));
}

/// The shell checker's `--json` decision object (#4494), parsed so the
/// conformance matrix can compare the exact unmet-capability SET rather
/// than just pass/fail.
#[derive(Debug, Deserialize)]
struct ShellDecision {
    decision: String,
    unmet: Vec<String>,
}

/// Run `check-runtime-capabilities.sh --json` and return its parsed
/// decision plus its exit code. Panics (rather than degrading) if the
/// script does not produce parseable JSON — a silent parse failure is
/// exactly the drift blindness this checker exists to prevent.
///
/// **Timing sensitivity (issue #8532).**
/// `native_and_shell_conformance_for_every_shipped_pair` calls this once
/// per shipped (role, runtime) pair — dozens of real `bash` spawns in a
/// single test. That makes it, per run, the most fork/exec-dependent test
/// in the `--lib` suite, and the two ways a saturated host breaks it are
/// *both* host conditions rather than admission drift:
///
/// 1. **`Command::output()` returns `Err`** — the fork/exec never
///    happened (`EAGAIN`/`ENOMEM` under concurrent `cargo build`s on a
///    host with no swap). Retried below, bounded; this cannot mask drift,
///    because a checker that *did* run and answered wrongly still fails
///    immediately via the caller's own assertions.
/// 2. **The child ran but produced no parseable JSON** — e.g. it was
///    OOM-killed, or `bash` itself failed to start up (`ENOSPC` on the
///    filesystem holding the temp/worktree). Deliberately **not** retried
///    (silence there is exactly the drift blindness the checker exists to
///    prevent), but the panic below reports the exit status and stderr,
///    not just stdout, so a repeat flake report is triageable from the
///    failure text alone instead of being re-investigated from scratch.
///
/// The original #8532 report captured only the four test names, with no
/// failure messages — which is why (2) exists.
fn shell_decision(checker: &Path, dir: &Path, role: &str, runtime: &str) -> (ShellDecision, i32) {
    const SPAWN_ATTEMPTS: u32 = 3;
    let mut last_err = None;
    let mut out = None;
    for attempt in 0..SPAWN_ATTEMPTS {
        match Command::new("bash")
            .arg(checker)
            .args(["--role", role, "--runtime", runtime, "--json", "--dir"])
            .arg(dir)
            .output()
        {
            Ok(o) => {
                out = Some(o);
                break;
            }
            Err(e) if attempt + 1 < SPAWN_ATTEMPTS => {
                last_err = Some(e);
                std::thread::sleep(Duration::from_millis(100 * u64::from(attempt + 1)));
            }
            Err(e) => last_err = Some(e),
        }
    }
    let out = out.unwrap_or_else(|| {
        panic!(
            "could not spawn checker for role={role} runtime={runtime} after \
                 {SPAWN_ATTEMPTS} attempts: {last_err:?}"
        )
    });
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json = stdout.lines().last().unwrap_or_default();
    let parsed: ShellDecision = serde_json::from_str(json).unwrap_or_else(|e| {
        // Issue #8532: status + stderr, not just stdout. A signal-killed
        // child (`status.code() == None`, e.g. the OOM killer) and a
        // checker that genuinely emitted malformed JSON are
        // indistinguishable from stdout alone, and the difference is the
        // whole triage decision: host condition vs. real drift.
        let stderr = String::from_utf8_lossy(&out.stderr);
        panic!(
            "checker --json output for role={role} runtime={runtime} unparseable ({e}): \
                 status={status:?} (code={code:?}) stdout={stdout:?} stderr={stderr:?}",
            status = out.status,
            code = out.status.code(),
        )
    });
    (parsed, out.status.code().unwrap_or(-1))
}

fn sorted(mut values: Vec<String>) -> Vec<String> {
    values.sort();
    values
}

/// One matrix drives both implementations: every shipped role/runtime pair
/// is evaluated natively and by the installed shell checker, and BOTH the
/// decision and the exact named unmet-capability set must be identical.
/// This prevents two independent assertion lists from silently drifting —
/// comparing only pass/fail would let the two paths agree on "reject" while
/// naming different capabilities.
#[test]
fn native_and_shell_conformance_for_every_shipped_pair() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let defaults = repo.join("defaults");
    let checker = defaults.join("scripts/check-runtime-capabilities.sh");
    let roles = fs::read_dir(defaults.join("roles")).unwrap();
    let runtimes: Vec<String> = fs::read_dir(defaults.join("runtimes"))
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|e| e.path().file_stem()?.to_str().map(str::to_owned))
        .collect();
    assert!(!runtimes.is_empty(), "no shipped runtime manifests found");
    let mut compared = 0_usize;
    let mut rejections = 0_usize;

    for role_entry in roles.filter_map(Result::ok) {
        if role_entry.path().extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Some(role) = role_entry
            .path()
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_owned)
        else {
            continue;
        };
        // Roles outside the daemon's canonical launch set are not
        // schedulable here; the shell checker remains a generic tool.
        let Some(canonical) = canonical_role(&role) else {
            continue;
        };
        for runtime in &runtimes {
            let native = resolve_and_admit(repo, &role, Some(runtime));
            // A full sweep is one runtime admitted against Builder's
            // (strongest lifecycle) requirements — that is the role the
            // shell checker must be asked about for the `loom`/sweep entry.
            let shell_role = if canonical == "sweep-lifecycle" {
                "builder"
            } else {
                canonical
            };
            let (shell, code) = shell_decision(&checker, &defaults, shell_role, runtime);

            // 1. Identical decisions.
            assert_eq!(
                native.is_ok(),
                shell.decision == "admit",
                "native/shell admission drift for role={role} runtime={runtime}: \
                     native={native:?}, shell={shell:?} (exit {code})"
            );
            // 2. Identical unmet-capability SETS (the named capabilities,
            //    not merely the pass/fail verdict).
            let native_unmet = native
                .as_ref()
                .err()
                .map(|r| r.unmet_capabilities.clone())
                .unwrap_or_default();
            assert_eq!(
                sorted(native_unmet),
                sorted(shell.unmet.clone()),
                "native/shell unmet-capability drift for role={role} runtime={runtime}: \
                     native={native:?}, shell={shell:?}"
            );
            // 3. The checker's EX_CONFIG(78)-vs-error(1) distinction is
            //    preserved and matches the decision it reported.
            match shell.decision.as_str() {
                "admit" => assert_eq!(code, 0, "role={role} runtime={runtime}"),
                "reject" => {
                    assert_eq!(code, 78, "role={role} runtime={runtime}");
                    rejections += 1;
                }
                other => {
                    panic!("unexpected decision {other:?} for role={role} runtime={runtime}")
                }
            }
            compared += 1;
        }
    }

    // The matrix must be non-degenerate: it has to actually contain the
    // shipped `builder`/sweep + `codex` refusals whose named unmet set is
    // the contract under test (`worktreeIsolation`).
    assert!(compared >= 2, "conformance matrix compared only {compared} pair(s)");
    assert!(rejections > 0, "conformance matrix contained no rejection to compare");
    let (builder_codex, code) = shell_decision(&checker, &defaults, "builder", "codex");
    assert_eq!(code, 78);
    assert_eq!(builder_codex.unmet, vec!["worktreeIsolation"]);
    assert_eq!(
        resolve_and_admit(repo, "sweep-lifecycle", Some("codex"))
            .unwrap_err()
            .unmet_capabilities,
        builder_codex.unmet,
        "the full-sweep gate must name the same unmet set as the shell checker"
    );
}

/// Fail-closed `runtimes.roles` key validation (#4494): an unknown role key
/// is rejected rather than silently ignored, while an empty *value* keeps
/// its established "unset, fall through" meaning.
#[test]
#[serial_test::serial]
fn unknown_configured_role_keys_fail_closed() {
    let _env_guard = ClearedLoomRuntimeEnv::new();
    let d = fixture();
    let write_config = |contents: &str| {
        fs::create_dir_all(d.path().join(".loom")).unwrap();
        fs::write(d.path().join(".loom/config.json"), contents).unwrap();
    };

    // Unknown key -> fail closed, for the requested role AND for any other.
    write_config(r#"{"runtimes":{"roles":{"not-a-role":"codex"}}}"#);
    let rejection = resolve_and_admit(d.path(), "judge", None).unwrap_err();
    assert_eq!(rejection.source, RuntimeSource::RoleConfig);
    assert!(rejection.reason.contains("not-a-role"), "{}", rejection.reason);
    assert!(rejection.unmet_capabilities.is_empty());
    // Even an explicit per-dispatch runtime does not rescue a broken map.
    assert!(resolve_and_admit(d.path(), "curator", Some("claude"))
        .unwrap_err()
        .reason
        .contains("unknown role name"));

    // A misspelled real role is caught by the same check.
    write_config(r#"{"runtimes":{"roles":{"buidler":"claude"}}}"#);
    assert!(resolve_and_admit(d.path(), "judge", None)
        .unwrap_err()
        .reason
        .contains("buidler"));

    // Non-string values fail closed too.
    write_config(r#"{"runtimes":{"roles":{"curator":true}}}"#);
    assert!(resolve_and_admit(d.path(), "curator", None)
        .unwrap_err()
        .reason
        .contains("must be strings"));

    // A non-object `runtimes.roles` fails closed.
    write_config(r#"{"runtimes":{"roles":"codex"}}"#);
    assert!(resolve_and_admit(d.path(), "curator", None)
        .unwrap_err()
        .reason
        .contains("must be an object"));

    // Known keys (canonical + alias) with an EMPTY value stay "unset":
    // resolution falls through to `runtimes.default`, unchanged semantics.
    write_config(
        r#"{"runtimes":{"default":"codex","roles":{"curator":"","issue-curator":"","sweep-lifecycle":""}}}"#,
    );
    let admitted = resolve_and_admit(d.path(), "curator", None).unwrap();
    assert_eq!(admitted.runtime, "codex");
    assert_eq!(admitted.source, RuntimeSource::DefaultConfig);

    // And a valid per-role binding still wins over the default.
    write_config(r#"{"runtimes":{"default":"claude","roles":{"curator":"codex"}}}"#);
    let admitted = resolve_and_admit(d.path(), "curator", None).unwrap();
    assert_eq!(admitted.runtime, "codex");
    assert_eq!(admitted.source, RuntimeSource::RoleConfig);

    // No `runtimes` block at all remains the zero-config Claude path.
    write_config("{}");
    let admitted = resolve_and_admit(d.path(), "curator", None).unwrap();
    assert_eq!(admitted.runtime, "claude");
    assert_eq!(admitted.source, RuntimeSource::BuiltIn);
}

/// End-to-end #6201 regression, the exact incident shape: a role
/// manifest declares `"suggestedWorkerType": "claude"`, but a fleet-wide
/// `runtimes.default` runtime-selection experiment (e.g. testing Codex
/// broadly) is in effect with no role-targeted override at all.
/// `resolve_and_admit` still resolves the runtime exactly as before
/// (selection is unchanged — see `role_suggested_worker_type`'s doc
/// comment for why a role's own hint must not become a binding
/// override), but [`ResolvedRuntime`] now carries the declared
/// suggestion, and [`suggested_worker_type_mismatch_warning`] is the
/// loud, at-selection diagnostic the original incident lacked entirely.
#[test]
#[serial_test::serial]
fn suggested_worker_type_is_observable_but_never_overrides_selection() {
    let _env_guard = ClearedLoomRuntimeEnv::new();
    let d = fixture();
    // curator declares a preference the zero-config fixture didn't have.
    fs::write(
        d.path().join("defaults/roles/curator.json"),
        r#"{"suggestedWorkerType":"claude"}"#,
    )
    .unwrap();
    let write_config = |contents: &str| {
        fs::create_dir_all(d.path().join(".loom")).unwrap();
        fs::write(d.path().join(".loom/config.json"), contents).unwrap();
    };

    // Zero config: curator resolves onto its own declared "claude" via
    // the honest built-in default -> no divergence, no warning.
    let admitted = resolve_and_admit(d.path(), "curator", None).unwrap();
    assert_eq!(admitted.runtime, "claude");
    assert_eq!(admitted.source, RuntimeSource::BuiltIn);
    assert_eq!(admitted.suggested_worker_type.as_deref(), Some("claude"));
    assert!(suggested_worker_type_mismatch_warning(&admitted).is_none());

    // A blanket experiment redirects every role to codex, with no
    // curator-specific override — the exact incident shape (#6201):
    // selection is UNCHANGED (curator still ends up on codex, matching
    // pre-#6201 behavior — this fix does not silently rescue the role),
    // but the mismatch is now observable and loudly warned about.
    write_config(r#"{"runtimes":{"default":"codex"}}"#);
    let admitted = resolve_and_admit(d.path(), "curator", None).unwrap();
    assert_eq!(admitted.runtime, "codex");
    assert_eq!(admitted.source, RuntimeSource::DefaultConfig);
    assert_eq!(admitted.suggested_worker_type.as_deref(), Some("claude"));
    let msg = suggested_worker_type_mismatch_warning(&admitted).expect("must warn on divergence");
    assert!(msg.contains("curator"), "{msg}");
    assert!(msg.contains("default-config"), "{msg}");

    // A role-targeted override (`runtimes.roles.curator`) is a
    // deliberate, per-role decision — selection still lands on codex,
    // and the warning still fires (an operator's own explicit override
    // is still worth naming loudly), but its wording differs by source.
    write_config(r#"{"runtimes":{"default":"codex","roles":{"curator":"codex"}}}"#);
    let admitted = resolve_and_admit(d.path(), "curator", None).unwrap();
    assert_eq!(admitted.runtime, "codex");
    assert_eq!(admitted.source, RuntimeSource::RoleConfig);
    assert_eq!(admitted.suggested_worker_type.as_deref(), Some("claude"));
    assert!(suggested_worker_type_mismatch_warning(&admitted).is_some());

    // judge has no declared suggestion in this fixture (still `"{}"`):
    // no divergence is even representable, so no warning either way.
    let judge_admitted = resolve_and_admit(d.path(), "judge", None).unwrap();
    assert_eq!(judge_admitted.runtime, "codex");
    assert!(suggested_worker_type_mismatch_warning(&judge_admitted).is_none());
}

/// `check_runtimes_roles_config` (#5006) must report exactly the same
/// fail-closed problems `config_runtime` rejects at admission time —
/// unknown role key, non-string value, non-object shape — so
/// `loom-daemon validate` can catch a bad `runtimes.roles` before any
/// role's tick actually runs into it.
#[test]
fn check_runtimes_roles_config_reports_unknown_key() {
    let config = serde_json::json!({"runtimes": {"roles": {"buidler": "claude"}}});
    let errors = check_runtimes_roles_config(&config);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("buidler"), "{}", errors[0]);
    assert!(errors[0].contains("unknown role name"), "{}", errors[0]);
}

#[test]
fn check_runtimes_roles_config_reports_non_string_value() {
    let config = serde_json::json!({"runtimes": {"roles": {"curator": true}}});
    let errors = check_runtimes_roles_config(&config);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("must be strings"), "{}", errors[0]);
    assert!(errors[0].contains("curator"), "{}", errors[0]);
}

#[test]
fn check_runtimes_roles_config_reports_non_object_shape() {
    let config = serde_json::json!({"runtimes": {"roles": "codex"}});
    let errors = check_runtimes_roles_config(&config);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("must be an object"), "{}", errors[0]);
}

/// Well-formed or absent `runtimes.roles` produces no findings — no
/// regression for the common (zero-config, or valid config) case.
#[test]
fn check_runtimes_roles_config_is_silent_on_valid_or_absent_config() {
    assert!(check_runtimes_roles_config(&serde_json::json!({})).is_empty());
    assert!(check_runtimes_roles_config(&serde_json::json!({"terminals": []})).is_empty());
    assert!(check_runtimes_roles_config(
        &serde_json::json!({"runtimes": {"default": "codex", "roles": {"curator": "claude"}}})
    )
    .is_empty());
    // Empty-value semantics (an unset per-role binding) are preserved.
    assert!(check_runtimes_roles_config(
        &serde_json::json!({"runtimes": {"roles": {"curator": ""}}})
    )
    .is_empty());
}

#[test]
fn installed_layout_has_identical_resolution_and_rejection() {
    let source = fixture();
    let installed = tempfile::tempdir().unwrap();
    fs::create_dir_all(installed.path().join(".loom")).unwrap();
    for sub in ["roles", "runtimes", "scripts"] {
        fs::rename(
            source.path().join("defaults").join(sub),
            installed.path().join(".loom").join(sub),
        )
        .unwrap();
    }
    let admitted = resolve_and_admit(installed.path(), "judge", Some("codex")).unwrap();
    assert!(admitted
        .role_manifest
        .starts_with(installed.path().join(".loom")));
    let rejected =
        resolve_and_admit(installed.path(), "sweep-lifecycle", Some("codex")).unwrap_err();
    assert_eq!(rejected.unmet_capabilities, vec!["worktreeIsolation"]);
}

/// `roots()` must resolve each subdirectory's source independently
/// (#4688): a workspace with `.loom/roles/` present but `.loom/runtimes/`
/// absent must still resolve `roles` from `.loom/roles`, not fall through
/// to `defaults/roles` just because `runtimes` is missing.
#[test]
fn roots_resolves_roles_and_runtimes_independently() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join(".loom/roles")).unwrap();
    fs::create_dir_all(root.path().join("defaults/runtimes")).unwrap();
    fs::create_dir_all(root.path().join("defaults/scripts")).unwrap();
    // `.loom/roles` exists but `.loom/runtimes` and `.loom/scripts` do not.
    let (roles, runtimes, scripts) = roots(root.path());
    assert_eq!(roles, root.path().join(".loom/roles"));
    assert_eq!(runtimes, root.path().join("defaults/runtimes"));
    assert_eq!(scripts, root.path().join("defaults/scripts"));
}

/// #4688 regression: a consumer repo can have `.loom/roles/` fully
/// provisioned while `.loom/runtimes/` is still missing (the exact live
/// incident layout — a provisioning gap on installs that predate
/// `defaults/runtimes/`). The per-directory `roots()` fallback resolves
/// roles from `.loom/roles` even though runtimes falls through, and the
/// builtin `claude` runtime — whose manifest is absent from BOTH
/// `.loom/runtimes/` and any `defaults/runtimes/` fallback, since this
/// fixture has no `defaults/` at all — degrades open rather than
/// rejecting the dispatch.
#[test]
fn consumer_layout_missing_runtimes_dir_still_admits_claude() {
    let root = tempfile::tempdir().unwrap();
    let loom_roles = root.path().join(".loom/roles");
    fs::create_dir_all(&loom_roles).unwrap();
    fs::write(
        loom_roles.join("builder.json"),
        r#"{"runtimeRequirements":["worktreeIsolation","mcp"]}"#,
    )
    .unwrap();
    fs::write(loom_roles.join("judge.json"), "{}").unwrap();
    let loom_scripts = root.path().join(".loom/scripts");
    fs::create_dir_all(&loom_scripts).unwrap();
    for name in ["claude", "codex"] {
        let adapter = loom_scripts.join(format!("spawn-{name}.sh"));
        fs::write(&adapter, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&adapter, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    // No `.loom/runtimes/` and no `defaults/` at all — the exact live
    // incident layout.
    assert!(!root.path().join(".loom/runtimes").exists());
    assert!(!root.path().join("defaults").exists());

    let admitted = resolve_and_admit(root.path(), "sweep-lifecycle", None).unwrap();
    assert_eq!(admitted.runtime, "claude");
    // Roles resolved from `.loom/roles` — the per-directory fallback
    // means the absence of `.loom/runtimes/` does NOT drag roles down
    // to `defaults/roles` too.
    assert!(admitted
        .role_manifest
        .starts_with(root.path().join(".loom")));
    // Runtimes independently fall through to `defaults/runtimes` (which
    // does not exist in this fixture either) since `.loom/runtimes/` is
    // absent — and admission still succeeds because the missing
    // manifest degrades open for the builtin runtime instead of
    // rejecting the dispatch.
    assert!(admitted
        .runtime_manifest
        .starts_with(root.path().join("defaults/runtimes")));

    // #5002: a non-builtin runtime the daemon ships an adapter for
    // (codex) ALSO admits in this exact layout — via the bundled
    // fallback manifest compiled into the binary, not the degrade-open
    // path exercised above (which is scoped to the builtin runtime
    // only). `judge` declares no `runtimeRequirements`, so it is
    // satisfied by codex's real bundled capabilities.
    let admitted_codex = resolve_and_admit(root.path(), "judge", Some("codex")).unwrap();
    assert_eq!(admitted_codex.runtime, "codex");
    assert_eq!(admitted_codex.source, RuntimeSource::Explicit);
}

/// The degrade-open behaviour is scoped to the builtin `claude` runtime
/// only, and the #5002 bundled-fallback addition is scoped to the
/// runtimes the daemon binary actually ships a manifest for. A runtime
/// with NO on-disk manifest anywhere AND no bundled fallback (simulated
/// here with an operator-defined custom runtime name the binary was
/// never built with) must still fail closed — a regression guard
/// against over-widening either fix beyond its intended scope. It also
/// pins the #5002 corrected error text: the message must name a path
/// reachable from THIS repo (`.loom/runtimes/<name>.json`), not the
/// unreachable `defaults/runtimes/<name>.json` fallback path `roots()`
/// computed (`defaults/` only exists in the Loom source checkout).
#[test]
fn consumer_layout_missing_runtimes_dir_non_builtin_still_fails_closed() {
    let root = tempfile::tempdir().unwrap();
    let loom_roles = root.path().join(".loom/roles");
    fs::create_dir_all(&loom_roles).unwrap();
    fs::write(loom_roles.join("judge.json"), "{}").unwrap();
    let loom_scripts = root.path().join(".loom/scripts");
    fs::create_dir_all(&loom_scripts).unwrap();
    for name in ["codex", "acme-runtime"] {
        let adapter = loom_scripts.join(format!("spawn-{name}.sh"));
        fs::write(&adapter, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&adapter, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    // No on-disk manifest anywhere (no `.loom/runtimes/`, no
    // `defaults/`) AND no bundled fallback for this made-up runtime
    // name -- still fails closed exactly as before.
    let rejected = resolve_and_admit(root.path(), "judge", Some("acme-runtime")).unwrap_err();
    assert!(rejected.reason.contains("runtime manifest"), "{}", rejected.reason);
    // #5002: the corrected message names a path reachable from a
    // consumer repo, not the unreachable `defaults/runtimes/...` path.
    assert!(
        rejected.reason.contains(".loom/runtimes/acme-runtime.json"),
        "{}",
        rejected.reason
    );
    assert!(!rejected.reason.contains("defaults/runtimes"), "{}", rejected.reason);

    // #5002: a runtime the daemon DOES ship a bundled manifest for
    // (codex) is no longer stuck here -- it now admits via the bundled
    // fallback instead of failing closed, since the compiled-in
    // manifest is a reachable source of truth even with no on-disk
    // `.loom/runtimes/` at all. This is the fix under test, not a
    // weakening of the fail-closed guarantee asserted above.
    let admitted = resolve_and_admit(root.path(), "judge", Some("codex")).unwrap();
    assert_eq!(admitted.runtime, "codex");
}

#[test]
fn rejection_serialization_is_structured_backward_compatible_and_secret_free() {
    let rejection = RuntimeRejection {
        role: "builder".into(),
        runtime: "codex".into(),
        source: RuntimeSource::RoleConfig,
        unmet_capabilities: vec!["worktreeIsolation".into()],
        reason: "unmet capabilities: worktreeIsolation".into(),
    };
    let json = serde_json::to_string(&rejection).unwrap();
    assert!(json.contains("\"role\":\"builder\""));
    assert!(json.contains("\"source\":\"role-config\""));
    assert!(!json.contains("TOKEN"));
    assert_eq!(serde_json::from_str::<RuntimeRejection>(&json).unwrap(), rejection);
}

/// Admission fixture extended with the repo's real `opencode` runtime
/// manifest (the `fixture()` baseline ships only claude/codex), so a
/// native runtime resolves exactly as a fleet host would. `curator`
/// declares no `runtimeRequirements`, which opencode's real manifest
/// satisfies (it declares `mcp: "no"`, so judge/builder would reject on
/// capabilities — not the axis under test here).
fn native_fixture() -> tempfile::TempDir {
    let d = fixture();
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    fs::copy(
        repo.join("defaults/runtimes/opencode.json"),
        d.path().join("defaults/runtimes/opencode.json"),
    )
    .unwrap();
    d
}

fn touch_executable_file(path: &Path) {
    fs::write(path, b"#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }
}

/// #8707: a native-routed dispatch must survive a mid-`auto_update` roll —
/// new binary staged, drain-restart not yet landed, `current_exe()`
/// reporting the unlinked inode's path with the kernel's ` (deleted)`
/// suffix. The fixture mirrors `daemon_bin_resolve`'s own
/// `test_deleted_suffix_stripped_when_replacement_exists`: the on-disk
/// replacement exists; only the readlink target carries the suffix.
#[test]
fn native_adapter_admits_through_a_deleted_current_exe_mid_roll() {
    let d = native_fixture();
    let roll = tempfile::tempdir().unwrap();
    let replacement = roll.path().join("loom-daemon");
    touch_executable_file(&replacement);
    let deleted = roll.path().join("loom-daemon (deleted)");

    // The defect's mechanism, pinned first: the raw readlink target — what
    // the pre-#8707 native branch consumed verbatim — names no existing
    // file, so the old strategy rejected every native dispatch on it for
    // the whole roll window.
    assert!(!deleted.is_file());

    let admitted = resolve_and_admit_with(d.path(), "curator", Some("opencode"), || {
        crate::daemon_bin_resolve::resolve_from_current_exe(&deleted, |_| None)
    })
    .unwrap();
    assert_eq!(admitted.runtime, "opencode");
    assert_eq!(admitted.adapter, replacement);
}

/// …and the ordinary (non-deleted) case is unchanged: admission resolves
/// exactly the executable the raw `current_exe()` would have named.
#[test]
fn native_adapter_ordinary_current_exe_admits_unchanged() {
    let d = native_fixture();
    let roll = tempfile::tempdir().unwrap();
    let exe = roll.path().join("loom-daemon");
    touch_executable_file(&exe);

    let admitted = resolve_and_admit_with(d.path(), "curator", Some("opencode"), || {
        crate::daemon_bin_resolve::resolve_from_current_exe(&exe, |_| None)
    })
    .unwrap();
    assert_eq!(admitted.runtime, "opencode");
    assert_eq!(admitted.adapter, exe);
}

/// #8707: fail closed with a clear reason when the on-disk replacement is
/// ALSO gone and no `$PATH` fallback resolves — the resolver's error names
/// both attempts (correctness during a roll, never masking a genuine
/// missing-binary condition).
#[test]
fn native_adapter_fails_closed_naming_both_attempts_when_replacement_is_gone() {
    let d = native_fixture();
    let roll = tempfile::tempdir().unwrap();
    // Nothing at `roll/loom-daemon`, and the injected PATH lookup finds
    // nothing — the resolver's error path is the rejection reason.
    let deleted = roll.path().join("loom-daemon (deleted)");

    let rejected = resolve_and_admit_with(d.path(), "curator", Some("opencode"), || {
        crate::daemon_bin_resolve::resolve_from_current_exe(&deleted, |_| None)
    })
    .unwrap_err();
    assert_eq!(rejected.role, "curator");
    assert_eq!(rejected.runtime, "opencode");
    assert!(rejected.reason.contains("deleted inode"), "{}", rejected.reason);
    assert!(
        rejected
            .reason
            .contains(roll.path().join("loom-daemon").to_str().unwrap()),
        "{}",
        rejected.reason
    );
}
