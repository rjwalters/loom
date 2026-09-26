//! Tests for pooled-profile provisioning (issue #8672).
//!
//! The acceptance criteria this file is the evidence for:
//!
//! | Criterion | Test |
//! |---|---|
//! | populates a profile per the sharing table, idempotently | [`provisioning_populates_a_blank_profile`], [`a_second_run_is_a_no_op_by_mtime_and_ledger`] |
//! | a user edit inside a profile survives re-provisioning | [`a_user_edit_to_config_toml_survives_reprovisioning`], [`a_user_edit_to_a_copied_file_survives_reprovisioning`] |
//! | the credential and every denylisted key are never read or written | [`auth_json_is_never_read_or_written`], [`denylisted_keys_are_never_written`] |
//! | the managed `pre_tool_use` bridge lands in the profile's `hooks.json` | [`the_managed_hook_bridge_is_installed_into_the_profile`] |
//! | the design is per-provider data, not Codex-specific code | [`a_synthetic_provider_drives_the_same_provisioner`] |

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::*;
use crate::tokens_pool::profile_sharing::{HookBridgeSpec, SurfaceRule, CODEX_RULES};

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

struct Fixture {
    _root: tempfile::TempDir,
    source: PathBuf,
    profile: PathBuf,
    workspace: PathBuf,
}

impl Fixture {
    /// A realistic operator `~/.codex` next to a freshly created (blank)
    /// pooled profile, both under one tempdir.
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("default-home").join(".codex");
        let profile = root.path().join("codex-profiles").join("pooled-1");
        let workspace = root.path().join("workspace");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&profile).unwrap();
        fs::create_dir_all(&workspace).unwrap();

        fs::create_dir_all(source.join("prompts")).unwrap();
        fs::write(source.join("prompts").join("review.md"), "review prompt").unwrap();
        fs::create_dir_all(source.join("sessions")).unwrap();
        fs::write(source.join("AGENTS.md"), "# repo instructions\n").unwrap();
        fs::write(
            source.join("config.toml"),
            concat!(
                "# the operator's own comment\n",
                "model = \"gpt-5-codex\"\n",
                "approval_policy = \"on-request\"\n",
                "\n",
                "[mcp_servers.loom]\n",
                "command = \"mcp-loom\"\n",
                "\n",
                "[hooks.state.\"abc123\"]\n",
                "trusted_hash = \"SECRET-TRUST-HASH\"\n",
                "\n",
                "[projects.\"/home/operator/private-repo\"]\n",
                "trust_level = \"trusted\"\n",
            ),
        )
        .unwrap();
        // The credential. Present in BOTH profiles so a test can assert the
        // provisioner neither reads the source's nor overwrites the target's.
        fs::write(source.join("auth.json"), "SOURCE-CREDENTIAL").unwrap();
        fs::write(profile.join("auth.json"), "PROFILE-CREDENTIAL").unwrap();

        Self {
            _root: root,
            source,
            profile,
            workspace,
        }
    }

    fn options(&self) -> ProvisionOptions {
        ProvisionOptions {
            source: self.source.clone(),
            workspace: self.workspace.clone(),
            dry_run: false,
            // The bridge is a separate concern with its own dedicated test;
            // leaving it on here would shell out to `bash`+`jq` in every case.
            skip_hook_bridge: true,
        }
    }

    fn run(&self) -> ProvisionReport {
        provision_profile(&CODEX_RULES, &self.profile, &self.options()).unwrap()
    }

    fn config_toml(&self) -> String {
        fs::read_to_string(self.profile.join("config.toml")).unwrap()
    }
}

fn mtime(path: &Path) -> SystemTime {
    fs::symlink_metadata(path).unwrap().modified().unwrap()
}

/// Push every file's mtime into the past so a rewrite is detectable even on a
/// filesystem with coarse timestamp granularity.
fn backdate(dir: &Path) {
    let past = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
    let past = filetime_from(past);
    for entry in walk(dir) {
        let _ = set_mtime(&entry, past);
    }
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let meta = fs::symlink_metadata(&path);
        out.push(path.clone());
        if meta.is_ok_and(|m| m.is_dir()) {
            out.extend(walk(&path));
        }
    }
    out
}

#[cfg(unix)]
fn filetime_from(time: SystemTime) -> libc::time_t {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs() as libc::time_t
}

#[cfg(unix)]
fn set_mtime(path: &Path, secs: libc::time_t) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let times = [
        libc::timeval {
            tv_sec: secs,
            tv_usec: 0,
        },
        libc::timeval {
            tv_sec: secs,
            tv_usec: 0,
        },
    ];
    // `lutimes` so a symlink's own timestamps are set, not its target's.
    let rc = unsafe { libc::lutimes(c_path.as_ptr(), times.as_ptr()) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

// ---------------------------------------------------------------------------
// the sharing table
// ---------------------------------------------------------------------------

#[test]
fn provisioning_populates_a_blank_profile() {
    let fixture = Fixture::new();
    let report = fixture.run();
    assert!(report.changed, "a blank profile must be populated");
    assert_eq!(report.profile, "pooled-1");

    // Capability + session dirs are LIVE shares, not copies.
    let prompts = fixture.profile.join("prompts");
    assert!(fs::symlink_metadata(&prompts)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read_link(&prompts).unwrap(), fixture.source.join("prompts"));
    assert!(fs::symlink_metadata(fixture.profile.join("sessions"))
        .unwrap()
        .file_type()
        .is_symlink());
    // A file added to the default afterwards is visible with no re-provision.
    fs::write(fixture.source.join("prompts").join("later.md"), "later").unwrap();
    assert!(prompts.join("later.md").is_file());

    // Files are COPIED — a symlink here would be replaced by the CLI's
    // tmp-then-rename and silently fork the operator's original.
    let agents = fixture.profile.join("AGENTS.md");
    assert!(!fs::symlink_metadata(&agents)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read_to_string(&agents).unwrap(), "# repo instructions\n");

    // Settings are key-merged.
    let config = fixture.config_toml();
    assert!(config.contains("gpt-5-codex"), "{config}");
    assert!(config.contains("mcp-loom"), "{config}");
}

#[test]
fn a_second_run_is_a_no_op_by_mtime_and_ledger() {
    let fixture = Fixture::new();
    assert!(fixture.run().changed);

    backdate(&fixture.profile);
    let before: Vec<(PathBuf, SystemTime)> = walk(&fixture.profile)
        .into_iter()
        .map(|p| {
            let m = mtime(&p);
            (p, m)
        })
        .collect();
    let ledger_before =
        fs::read_to_string(crate::tokens_pool::profile_ledger::ledger_path(&fixture.profile))
            .unwrap();

    let second = fixture.run();
    assert!(!second.changed, "a second run must change nothing: {second:?}");
    for (path, was) in before {
        assert_eq!(mtime(&path), was, "{} was rewritten", path.display());
    }
    assert_eq!(
        fs::read_to_string(crate::tokens_pool::profile_ledger::ledger_path(&fixture.profile))
            .unwrap(),
        ledger_before,
        "the ledger itself must not be rewritten by a no-op run"
    );
    // Every surface reports a non-mutating action.
    for surface in &second.surfaces {
        assert!(matches!(surface.action, "unchanged" | "preserved" | "skipped"), "{surface:?}");
    }
}

#[test]
fn a_changed_default_propagates_to_a_profile_loom_still_owns() {
    let fixture = Fixture::new();
    fixture.run();
    fs::write(fixture.source.join("AGENTS.md"), "# updated instructions\n").unwrap();
    assert!(fixture.run().changed);
    assert_eq!(
        fs::read_to_string(fixture.profile.join("AGENTS.md")).unwrap(),
        "# updated instructions\n"
    );
}

// ---------------------------------------------------------------------------
// the ledger: a user edit inside a profile wins forever
// ---------------------------------------------------------------------------

#[test]
fn a_user_edit_to_config_toml_survives_reprovisioning() {
    let fixture = Fixture::new();
    fixture.run();
    assert!(fixture.config_toml().contains("gpt-5-codex"));

    // The operator retunes this pooled account by hand, and annotates it.
    let edited = format!(
        "# hand-tuned for the pooled account\n{}",
        fixture
            .config_toml()
            .replace("gpt-5-codex", "gpt-5-codex-mini")
    );
    fs::write(fixture.profile.join("config.toml"), &edited).unwrap();

    // …and the default profile changes too, so a ledger-less provisioner
    // would have a reason to write.
    fs::write(
        fixture.source.join("config.toml"),
        "model = \"gpt-6\"\napproval_policy = \"never\"\n",
    )
    .unwrap();

    fixture.run();
    let after = fixture.config_toml();
    assert!(after.contains("gpt-5-codex-mini"), "the operator's edit must survive: {after}");
    assert!(!after.contains("gpt-6"), "{after}");
    // A key the operator did NOT touch still tracks the default.
    assert!(after.contains("approval_policy = \"never\""), "{after}");
    // And their own comment survived the rewrite byte-for-byte — the whole
    // reason the TOML merge goes through a format-preserving document model.
    assert!(after.contains("# hand-tuned for the pooled account"), "{after}");

    // Re-provisioning again is still a no-op for that key.
    fixture.run();
    assert!(fixture.config_toml().contains("gpt-5-codex-mini"));
}

#[test]
fn a_user_edit_to_a_copied_file_survives_reprovisioning() {
    let fixture = Fixture::new();
    fixture.run();
    fs::write(fixture.profile.join("AGENTS.md"), "# mine, hands off\n").unwrap();
    fs::write(fixture.source.join("AGENTS.md"), "# new default\n").unwrap();
    fixture.run();
    assert_eq!(
        fs::read_to_string(fixture.profile.join("AGENTS.md")).unwrap(),
        "# mine, hands off\n"
    );
}

#[test]
fn a_preexisting_unmanaged_file_is_never_clobbered() {
    let fixture = Fixture::new();
    // Written BEFORE Loom ever provisioned: no ledger entry exists for it.
    fs::write(fixture.profile.join("AGENTS.md"), "# pre-existing\n").unwrap();
    fixture.run();
    assert_eq!(
        fs::read_to_string(fixture.profile.join("AGENTS.md")).unwrap(),
        "# pre-existing\n"
    );
}

#[test]
fn a_real_directory_in_the_profile_is_never_replaced_by_a_link() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.profile.join("prompts")).unwrap();
    fs::write(fixture.profile.join("prompts").join("mine.md"), "mine").unwrap();
    fixture.run();
    assert!(!fs::symlink_metadata(fixture.profile.join("prompts"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(fixture.profile.join("prompts").join("mine.md").is_file());
}

// ---------------------------------------------------------------------------
// credentials and the denylist
// ---------------------------------------------------------------------------

#[test]
fn auth_json_is_never_read_or_written() {
    let fixture = Fixture::new();
    let source_auth = fixture.source.join("auth.json");
    let profile_auth = fixture.profile.join("auth.json");
    backdate(&fixture.source);
    backdate(&fixture.profile);
    let source_before = (fs::read(&source_auth).unwrap(), mtime(&source_auth));
    let profile_before = (fs::read(&profile_auth).unwrap(), mtime(&profile_auth));

    fixture.run();
    fixture.run();

    assert_eq!(fs::read(&source_auth).unwrap(), source_before.0);
    assert_eq!(fs::read(&profile_auth).unwrap(), profile_before.0);
    assert_eq!(
        mtime(&profile_auth),
        profile_before.1,
        "the pooled profile's credential must not be touched"
    );
    assert_eq!(
        mtime(&source_auth),
        source_before.1,
        "the default profile's credential must not be touched"
    );

    // Nothing anywhere in the provisioned profile may quote a credential.
    for path in walk(&fixture.profile) {
        if path == profile_auth || fs::symlink_metadata(&path).is_ok_and(|m| !m.is_file()) {
            continue;
        }
        let body = fs::read_to_string(&path).unwrap_or_default();
        assert!(!body.contains("SOURCE-CREDENTIAL"), "{}", path.display());
        assert!(!body.contains("PROFILE-CREDENTIAL"), "{}", path.display());
    }
}

#[test]
fn denylisted_keys_are_never_written() {
    let fixture = Fixture::new();
    let report = fixture.run();
    let config = fixture.config_toml();

    // Hook trust is established per profile, interactively. Importing another
    // profile's trusted_hash would fake a decision that never happened here.
    assert!(!config.contains("SECRET-TRUST-HASH"), "{config}");
    assert!(!config.contains("hooks.state"), "{config}");
    // Per-project trust state is likewise never shared.
    assert!(!config.contains("private-repo"), "{config}");
    assert!(!config.contains("trust_level"), "{config}");

    assert!(
        report
            .denied_keys
            .iter()
            .any(|k| k == "config.toml:hooks.state"),
        "{:?}",
        report.denied_keys
    );
    assert!(report
        .denied_keys
        .iter()
        .any(|k| k == "config.toml:projects"));

    // …and the ledger never records a denied key either, so a later run can
    // never "restore" one.
    let ledger = crate::tokens_pool::profile_ledger::ProfileLedger::load(
        &fixture.profile,
        "codex",
        &fixture.source,
    );
    for key in ledger.merged.values().flat_map(|f| f.keys()) {
        assert!(!CODEX_RULES.is_denied_key(key), "ledger recorded {key}");
    }
}

#[test]
fn a_denied_key_nested_inside_an_inline_table_is_refused_whole() {
    let fixture = Fixture::new();
    fs::write(
        fixture.source.join("config.toml"),
        "hooks = { enabled = true, state = { abc = \"SECRET\" } }\n",
    )
    .unwrap();
    let report = fixture.run();
    // The default's ONLY key was refused, so nothing was written at all —
    // provisioning never creates an empty settings file just to have one.
    assert!(!fixture.profile.join("config.toml").exists());
    assert!(
        report.denied_keys.iter().any(|k| k == "config.toml:hooks"),
        "{:?}",
        report.denied_keys
    );
}

#[test]
fn a_sibling_of_a_denied_key_is_still_shared() {
    let fixture = Fixture::new();
    fs::write(
        fixture.source.join("config.toml"),
        concat!(
            "[hooks]\n",
            "enabled = true\n",
            "\n",
            "[hooks.state.\"abc\"]\n",
            "trusted_hash = \"SECRET\"\n",
        ),
    )
    .unwrap();
    fixture.run();
    let config = fixture.config_toml();
    assert!(config.contains("enabled = true"), "{config}");
    assert!(!config.contains("SECRET"), "{config}");
}

// ---------------------------------------------------------------------------
// refusals
// ---------------------------------------------------------------------------

#[test]
fn provisioning_a_profile_from_itself_is_refused() {
    let fixture = Fixture::new();
    let mut options = fixture.options();
    options.source = fixture.profile.clone();
    let error = provision_profile(&CODEX_RULES, &fixture.profile, &options).unwrap_err();
    assert!(format!("{error:#}").contains("from itself"), "{error:#}");
}

#[test]
fn a_session_managed_profile_is_left_to_its_container() {
    let fixture = Fixture::new();
    // The marker `accounts session start` writes when it adopts a profile.
    fs::write(
        fixture
            .profile
            .join(crate::tokens_pool::session_lifecycle::SESSION_MARKER_FILE),
        "{\"container\":\"loom-worker-session-pooled-1\"}",
    )
    .unwrap();
    let report = fixture.run();
    assert!(!report.changed);
    assert!(!fixture.profile.join("AGENTS.md").exists());
    assert_eq!(report.surfaces.len(), 1);
    assert_eq!(report.surfaces[0].action, "skipped");
    assert!(report.surfaces[0].detail.contains("session-managed"));
}

#[test]
fn a_dry_run_writes_nothing() {
    let fixture = Fixture::new();
    let mut options = fixture.options();
    options.dry_run = true;
    let report = provision_profile(&CODEX_RULES, &fixture.profile, &options).unwrap();
    assert!(report
        .surfaces
        .iter()
        .any(|s| matches!(s.action, "linked" | "copied" | "merged")));
    assert!(!fixture.profile.join("AGENTS.md").exists());
    assert!(!fixture.profile.join("config.toml").exists());
    assert!(!crate::tokens_pool::profile_ledger::ledger_path(&fixture.profile).exists());
}

#[test]
fn a_malformed_profile_config_is_refused_rather_than_overwritten() {
    let fixture = Fixture::new();
    fs::write(fixture.profile.join("config.toml"), "this is [not valid").unwrap();
    let report = fixture.run();
    assert_eq!(
        fs::read_to_string(fixture.profile.join("config.toml")).unwrap(),
        "this is [not valid"
    );
    let config_surface = report
        .surfaces
        .iter()
        .find(|s| s.path == "config.toml")
        .unwrap();
    assert_eq!(config_surface.action, "skipped");
    assert!(config_surface.detail.contains("not valid TOML"), "{config_surface:?}");
}

// ---------------------------------------------------------------------------
// the managed pre_tool_use hook bridge (AC 3)
// ---------------------------------------------------------------------------

/// Drives the **real** `defaults/scripts/provision-codex-hooks.sh` against a
/// provisioned profile, which is what "the managed bridge is present in every
/// pooled profile's `hooks.json`" actually means. Skipped (not failed) where
/// the script's own dependencies are absent, so the suite stays green on a
/// host without `jq`.
#[test]
#[serial_test::serial]
fn the_managed_hook_bridge_is_installed_into_the_profile() {
    let Some(repo) = repo_root() else {
        eprintln!("skipping: not running inside the Loom checkout");
        return;
    };
    let script = repo.join("defaults/scripts/provision-codex-hooks.sh");
    let bridge = repo.join("defaults/hooks/guard-codex-bridge.sh");
    if !script.is_file() || !bridge.is_file() || which("jq").is_none() {
        eprintln!("skipping: the bridge provisioner or jq is unavailable");
        return;
    }

    let fixture = Fixture::new();
    // A workspace shaped like an installed repo, so the bridge resolves.
    fs::create_dir_all(fixture.workspace.join(".loom/hooks")).unwrap();
    fs::copy(&bridge, fixture.workspace.join(".loom/hooks/guard-codex-bridge.sh")).unwrap();
    fs::create_dir_all(fixture.workspace.join(".loom/scripts")).unwrap();
    fs::copy(
        &script,
        fixture
            .workspace
            .join(".loom/scripts/provision-codex-hooks.sh"),
    )
    .unwrap();

    let options = ProvisionOptions {
        skip_hook_bridge: false,
        ..fixture.options()
    };
    let report = provision_profile(&CODEX_RULES, &fixture.profile, &options).unwrap();
    let hooks = report.hook_bridge.expect("codex declares a hook bridge");
    assert!(hooks.ran, "{hooks:?}");
    assert!(hooks.ok, "{hooks:?}");

    let installed: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(fixture.profile.join("hooks.json")).unwrap())
            .unwrap();
    let commands = installed["hooks"]["PreToolUse"]
        .as_array()
        .expect("a PreToolUse array")
        .iter()
        .flat_map(|group| group["hooks"].as_array().cloned().unwrap_or_default())
        .filter_map(|hook| hook["command"].as_str().map(str::to_string))
        .collect::<Vec<_>>();
    assert!(commands.iter().any(|c| c.contains("guard-codex-bridge.sh")), "{commands:?}");
    // Hook TRUST stays an operator-attested per-profile step: the provisioner
    // must never waive it.
    for command in &commands {
        assert!(!command.contains("--dangerously-bypass-hook-trust"), "{command}");
    }
    assert!(!fs::read_to_string(fixture.profile.join("hooks.json"))
        .unwrap()
        .contains("dangerously-bypass-hook-trust"));
}

#[test]
fn a_missing_hook_bridge_script_is_reported_not_fatal() {
    let fixture = Fixture::new();
    let options = ProvisionOptions {
        skip_hook_bridge: false,
        ..fixture.options()
    };
    // No `.loom/scripts` in this workspace, and the env override is unset.
    let report = provision_profile(&CODEX_RULES, &fixture.profile, &options).unwrap();
    let hooks = report.hook_bridge.expect("codex declares a hook bridge");
    if !hooks.ran {
        assert!(hooks.detail.contains("LOOM_CODEX_HOOKS_SCRIPT"), "{hooks:?}");
    }
    // The surfaces were still provisioned.
    assert!(fixture.profile.join("AGENTS.md").is_file());
}

fn repo_root() -> Option<PathBuf> {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        if dir.join("defaults/scripts").is_dir() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn which(binary: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(binary))
            .find(|candidate| candidate.is_file())
    })
}

// ---------------------------------------------------------------------------
// the table is per-provider DATA, not Codex-specific code (AC 6)
// ---------------------------------------------------------------------------

/// The same entry point, driven by a table that shares nothing with Codex's —
/// different home variable, different surfaces, different denylist, a
/// JSON settings document instead of TOML. This is the structural evidence
/// that adding Kimi (`KIMI_CODE_HOME`, issue #8628) is a new `const` in
/// `profile_sharing.rs` rather than a new code path here.
#[test]
fn a_synthetic_provider_drives_the_same_provisioner() {
    const SYNTHETIC: crate::tokens_pool::profile_sharing::ProviderProfileRules =
        crate::tokens_pool::profile_sharing::ProviderProfileRules {
            provider: "synthetic",
            home_env: "SYNTHETIC_CODE_HOME",
            default_home_relative: ".synthetic",
            default_home_env: "LOOM_SYNTHETIC_DEFAULT_HOME",
            surfaces: &[
                SurfaceRule {
                    path: "toolbox",
                    mode: ShareMode::Symlink,
                },
                SurfaceRule {
                    path: "GUIDE.md",
                    mode: ShareMode::Copy,
                },
                SurfaceRule {
                    path: "settings.json",
                    mode: ShareMode::MergeJson,
                },
            ],
            never_share: &["secrets.json"],
            key_denylist: &["identity", "telemetry.deviceId"],
            hook_bridge: None,
        };

    let root = tempfile::tempdir().unwrap();
    let source = root.path().join(".synthetic");
    let profile = root.path().join("pool").join("acct-2");
    fs::create_dir_all(source.join("toolbox")).unwrap();
    fs::create_dir_all(&profile).unwrap();
    fs::write(source.join("GUIDE.md"), "guide\n").unwrap();
    fs::write(source.join("secrets.json"), "SYNTHETIC-CREDENTIAL").unwrap();
    fs::write(
        source.join("settings.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "theme": "dark",
            "identity": {"email": "operator@example.com"},
            "telemetry": {"enabled": true, "deviceId": "DEVICE-SECRET"},
        }))
        .unwrap(),
    )
    .unwrap();

    let options = ProvisionOptions {
        source: source.clone(),
        workspace: root.path().to_path_buf(),
        dry_run: false,
        skip_hook_bridge: true,
    };
    let report = provision_profile(&SYNTHETIC, &profile, &options).unwrap();
    assert!(report.changed);
    assert!(fs::symlink_metadata(profile.join("toolbox"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read_to_string(profile.join("GUIDE.md")).unwrap(), "guide\n");

    let settings: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(profile.join("settings.json")).unwrap()).unwrap();
    assert_eq!(settings["theme"], "dark");
    assert_eq!(settings["telemetry"]["enabled"], true);
    assert!(settings.get("identity").is_none(), "{settings}");
    assert!(settings["telemetry"].get("deviceId").is_none(), "{settings}");
    assert!(!profile.join("secrets.json").exists());

    // Idempotent on a foreign provider too.
    assert!(
        !provision_profile(&SYNTHETIC, &profile, &options)
            .unwrap()
            .changed
    );
}

#[test]
fn a_hook_bridge_spec_is_optional_per_provider() {
    // Guards the "per-provider data" claim from the other direction: a
    // provider with no bridge simply reports none, with no Codex-shaped
    // special case in the provisioner.
    assert!(CODEX_RULES.hook_bridge.is_some());
    let spec: HookBridgeSpec = CODEX_RULES.hook_bridge.unwrap();
    assert_eq!(spec.home_flag, "--codex-home");
    assert!(spec
        .script_candidates
        .iter()
        .any(|c| c.ends_with("provision-codex-hooks.sh")));
}

// ---------------------------------------------------------------------------
// default source resolution
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial]
fn the_default_home_is_never_inferred_from_the_providers_own_env_var() {
    // A dispatched agent's ambient CODEX_HOME points at a POOLED profile.
    // Trusting it would make one pooled account the source for another.
    std::env::set_var("CODEX_HOME", "/tmp/some-pooled-profile");
    std::env::remove_var(CODEX_RULES.default_home_env);
    assert_eq!(default_profile_home(&CODEX_RULES), None);
    std::env::remove_var("CODEX_HOME");
}

#[test]
#[serial_test::serial]
fn an_explicit_default_home_override_is_honoured() {
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var(CODEX_RULES.default_home_env, tmp.path());
    assert_eq!(default_profile_home(&CODEX_RULES).as_deref(), Some(tmp.path()));
    std::env::set_var(CODEX_RULES.default_home_env, "");
    assert_eq!(default_profile_home(&CODEX_RULES), None);
    std::env::remove_var(CODEX_RULES.default_home_env);
}

#[test]
#[serial_test::serial]
fn the_daemon_start_pass_has_a_kill_switch() {
    std::env::remove_var(PROVISION_ON_START_ENV);
    assert!(provision_on_start_enabled());
    std::env::set_var(PROVISION_ON_START_ENV, "0");
    assert!(!provision_on_start_enabled());
    std::env::set_var(PROVISION_ON_START_ENV, "false");
    assert!(!provision_on_start_enabled());
    std::env::set_var(PROVISION_ON_START_ENV, "1");
    assert!(provision_on_start_enabled());
    std::env::remove_var(PROVISION_ON_START_ENV);
}
