//! Containment evidence that can be measured without Docker (#8787).
//!
//! What is covered here: the obligations containment does NOT satisfy and must
//! therefore prove separately (a managed registration exists at all; Codex hook
//! trust has been established), the proof's own shape validation, and the fact
//! that a bare host process cannot produce worker-side evidence.
//!
//! The trust rule is additionally cross-checked against the SHIPPED
//! `provision-codex-hooks.sh verify` on the same fixture profiles, so the Rust
//! gate and the shell gate can never drift into disagreeing about whether one
//! profile is trusted.
//!
//! Docker-bound evidence — mount topology, exclusive volume/profile ownership,
//! the sealed bundle, the bound control identity and its rechecks — is covered
//! by `tests/private_workspace_docker`.
use super::containment::*;
use super::*;
use std::process::Command;

/// A profile shaped the way a provisioned private session's profile is: the
/// managed registration naming the image-owned bridge, Loom's receipt pinning
/// it, and a `config.toml` carrying `trusted` hashes plus the receipt's
/// install-time `baseline`.
fn profile(dir: &Path, baseline: Option<&[&str]>, trusted: &[&str]) -> PathBuf {
    let profile = dir.join("profile");
    std::fs::create_dir_all(&profile).unwrap();
    let command = bundle::registration();
    let mut receipt = serde_json::json!({
        "loomManagedHook": {
            "version": bundle::HOOK_VERSION,
            "command": command,
            "commandSha256": sha256(command.as_bytes()),
            "matcher": "*",
            "codexSchemaPin": bundle::CODEX_FLOOR,
            "workspace": REPO,
        }
    });
    if let Some(baseline) = baseline {
        receipt["loomManagedHook"]["trustBaselineHashes"] = serde_json::json!(baseline);
    }
    std::fs::write(profile.join("loom-codex-hooks.json"), serde_json::to_string(&receipt).unwrap())
        .unwrap();
    std::fs::write(
        profile.join("hooks.json"),
        serde_json::json!({
            "hooks": {"PreToolUse": [{"matcher": "*", "hooks": [
                {"type": "command", "command": command, "timeout": 30}
            ]}]}
        })
        .to_string(),
    )
    .unwrap();
    let mut config = String::from("model = \"fixture\"\n# trusted_hash = \"commented-out\"\n");
    // Codex keeps ONE trusted_hash per key, and Loom's entry has one key; the
    // other hashes are recorded for other hooks (an operator's, or this same
    // entry at another path). Loom's key carries the LAST hash: the most
    // recent decision, which is what the baseline diff asks about.
    for (index, hash) in trusted.iter().enumerate() {
        let key = if index + 1 == trusted.len() {
            loom_key()
        } else {
            format!("/elsewhere/profile-{index}/hooks.json:pre_tool_use:0:0")
        };
        config.push_str(&format!("[hooks.state.\"{key}\"]\ntrusted_hash = \"{hash}\"\n"));
    }
    std::fs::write(profile.join("config.toml"), config).unwrap();
    profile
}

/// The key Codex records trust for Loom's entry under in a private session.
fn loom_key() -> String {
    "/home/loom/.codex-profile/hooks.json:pre_tool_use:0:0".to_owned()
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

fn report(managed: bool, profile: &Path) -> bundle::Report {
    bundle::Report {
        protocol: bundle::CONTROL_PROTOCOL.into(),
        control_version: bundle::CONTROL_VERSION,
        status: bundle::Status::Ready,
        codex_cli: "0.149.1".into(),
        identity: "a".repeat(64),
        profile: bundle::profile_digests(profile),
        managed,
    }
}

/// `(description, install-time baseline, current trusted hashes, is trusted)`.
/// A `None` baseline is a legacy receipt from before the field existed.
type TrustCase = (&'static str, Option<&'static [&'static str]>, &'static [&'static str], bool);

/// Every trust shape the shipped shell gate distinguishes, and what the Rust
/// gate must answer for it.
const TRUST_CASES: &[TrustCase] = &[
    ("no trust at all", Some(&[]), &[], false),
    ("a new decision after install", Some(&[]), &["fresh"], true),
    ("only the pre-install baseline", Some(&["old"]), &["old"], false),
    ("a new decision beside the baseline", Some(&["old"]), &["old", "fresh"], true),
    ("legacy receipt, some trust", None, &["whenever"], true),
    ("legacy receipt, no trust", None, &[], false),
];

/// Trust recorded under a key Codex never looks Loom's private-session entry
/// up under: `(description, config.toml body)`. Each must read as untrusted to
/// both gates, with a fresh baseline that would otherwise admit any new hash.
const WRONG_LOCATION_CASES: &[(&str, &str)] = &[
    (
        "host-side path of the same profile",
        "[hooks.state.\"/Users/op/.loom/codex-profiles/a/hooks.json:pre_tool_use:0:0\"]\ntrusted_hash = \"fresh\"\n",
    ),
    (
        "another profile's path (the robb-studio shape)",
        "[hooks.state.\"/Users/op/.loom/codex-profiles/r.j.walters/hooks.json:pre_tool_use:0:0\"]\ntrusted_hash = \"fresh\"\n",
    ),
    (
        "another position in the right file",
        "[hooks.state.\"/home/loom/.codex-profile/hooks.json:pre_tool_use:3:0\"]\ntrusted_hash = \"fresh\"\n",
    ),
    (
        "a commented-out entry for the right key",
        "# [hooks.state.\"/home/loom/.codex-profile/hooks.json:pre_tool_use:0:0\"]\n# trusted_hash = \"fresh\"\n",
    ),
];

/// Spellings of the RIGHT key that both gates must read as trusted.
const KEYED_SPELLINGS: &[&str] = &[
    "hooks.state.\"/home/loom/.codex-profile/hooks.json:pre_tool_use:0:0\".trusted_hash = \"fresh\"\n",
    "[hooks.state]\n\"/home/loom/.codex-profile/hooks.json:pre_tool_use:0:0\" = { trusted_hash = \"fresh\", enabled = true }\n",
    "[hooks]\nstate.\"/home/loom/.codex-profile/hooks.json:pre_tool_use:0:0\".trusted_hash = \"fresh\"\n",
    "[hooks.state.\"/home/loom/.codex-profile/hooks.json:pre_tool_use:0:0\"]\nenabled = true\ntrusted_hash = \"fresh\"\n",
];

#[test]
fn trust_counts_only_under_the_key_codex_looks_loom_up_under() {
    for (what, config) in WRONG_LOCATION_CASES {
        let dir = tempfile::tempdir().unwrap();
        let profile = profile(dir.path(), Some(&[]), &[]);
        std::fs::write(profile.join("config.toml"), config).unwrap();
        assert!(enforcing(&report(true, &profile), &profile).is_err(), "{what}");
    }
    for config in KEYED_SPELLINGS {
        let dir = tempfile::tempdir().unwrap();
        let profile = profile(dir.path(), Some(&[]), &[]);
        std::fs::write(profile.join("config.toml"), config).unwrap();
        assert!(enforcing(&report(true, &profile), &profile).is_ok(), "{config}");
    }
}

#[test]
fn hook_trust_follows_the_install_time_baseline_diff() {
    for (what, baseline, trusted, expected) in TRUST_CASES {
        let dir = tempfile::tempdir().unwrap();
        let profile = profile(dir.path(), *baseline, trusted);
        assert_eq!(enforcing(&report(true, &profile), &profile).is_ok(), *expected, "{what}");
    }
    // A profile with no config.toml at all carries no trust.
    let dir = tempfile::tempdir().unwrap();
    let profile = profile(dir.path(), Some(&[]), &["fresh"]);
    std::fs::remove_file(profile.join("config.toml")).unwrap();
    assert!(enforcing(&report(true, &profile), &profile).is_err());
}

/// `(description, baseline, config.toml override, is trusted)` for the cross-check.
type CrossCase = (String, Option<&'static [&'static str]>, Option<&'static str>, bool);

/// The Rust gate and `provision-codex-hooks.sh verify` must agree, profile for
/// profile: one of them refusing while the other admits is exactly the drift
/// this cross-check exists to prevent. Skipped where the shell's own
/// dependencies are unavailable.
#[test]
fn the_rust_trust_gate_agrees_with_the_shipped_provisioner() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let provisioner = repo.join("defaults/scripts/provision-codex-hooks.sh");
    let bridge = repo.join("defaults/hooks/guard-codex-bridge.sh");
    if !provisioner.is_file() || !bridge.is_file() || which("jq").is_none() {
        eprintln!("skipping: provision-codex-hooks.sh or jq unavailable");
        return;
    }
    let mut cases: Vec<CrossCase> = TRUST_CASES
        .iter()
        .map(|(what, baseline, _, expected)| ((*what).to_owned(), *baseline, None, *expected))
        .collect();
    for (what, config) in WRONG_LOCATION_CASES {
        cases.push(((*what).to_owned(), Some(&[]), Some(*config), false));
    }
    for config in KEYED_SPELLINGS {
        cases.push(((*config).to_owned(), Some(&[]), Some(*config), true));
    }
    for (what, baseline, config, expected) in &cases {
        let dir = tempfile::tempdir().unwrap();
        let trusted: &[&str] = TRUST_CASES
            .iter()
            .find(|(name, ..)| name == what)
            .map_or(&[], |(_, _, trusted, _)| *trusted);
        let profile = profile(dir.path(), *baseline, trusted);
        if let Some(config) = config {
            std::fs::write(profile.join("config.toml"), config).unwrap();
        }
        // A private session runs Codex with CODEX_HOME at the container's
        // mount point; that is where its trust is keyed.
        let output = Command::new("bash")
            .arg(&provisioner)
            .args(["verify", "--codex-home"])
            .arg(&profile)
            .args([
                "--workspace",
                REPO,
                "--runtime-codex-home",
                PROFILE,
                "--bridge",
            ])
            .arg(&bridge)
            .arg("--json")
            .output()
            .unwrap();
        let verdict: serde_json::Value =
            serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
                panic!(
                    "verify emitted no JSON for {what}: {}",
                    String::from_utf8_lossy(&output.stderr)
                )
            });
        assert_eq!(verdict["trusted"], *expected, "{what}: shell verdict {verdict}");
        assert_eq!(
            enforcing(&report(true, &profile), &profile).is_ok(),
            verdict["trusted"] == true,
            "{what}: the Rust gate disagrees with the shipped provisioner"
        );
    }
}

fn which(bin: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(bin))
            .find(|candidate| candidate.is_file())
    })
}

/// A clone with no Loom surface has no managed bridge to enforce with, so it
/// can never carry a mutable role however well contained it is.
#[test]
fn an_unmanaged_clone_is_refused_even_when_the_boundary_is_ready() {
    let dir = tempfile::tempdir().unwrap();
    let profile = profile(dir.path(), Some(&[]), &["fresh"]);
    let error = enforcing(&report(false, &profile), &profile).unwrap_err();
    assert!(error.to_string().contains("no Loom hook provisioner"), "{error}");
    assert!(enforcing(&report(true, &profile), &profile).is_ok());
}

/// Obligation strings reach host logs: they must be fixed text that names the
/// requirement, never worker output, configuration or a profile's contents.
#[test]
fn refusals_are_fixed_secret_free_obligation_text() {
    let dir = tempfile::tempdir().unwrap();
    let untrusted = profile(dir.path(), Some(&["old"]), &["old"]);
    for error in [
        enforcing(&report(false, &untrusted), &untrusted).unwrap_err(),
        enforcing(&report(true, &untrusted), &untrusted).unwrap_err(),
    ] {
        let text = error.to_string();
        assert!(!text.is_empty());
        assert!(!text.contains(&*untrusted.to_string_lossy()), "{text}");
        assert!(!text.contains("fresh") && !text.contains("old"), "{text}");
        assert!(text.contains("protected remote operations"), "{text}");
    }
}

/// Exactly the roles `spawn-codex.sh` treats as mutable, plus the full sweep
/// that runs them in-process.
#[test]
fn only_repository_mutating_roles_need_the_managed_policy() {
    for role in [
        "builder",
        "doctor",
        "sweep-lifecycle",
        "sweep",
        "loom",
        "development-worker",
        "pr-fixer",
    ] {
        assert!(mutable(role), "{role}");
    }
    for role in [
        "judge",
        "curator",
        "champion",
        "guide",
        "architect",
        "",
        "unknown",
    ] {
        assert!(!mutable(role), "{role}");
    }
}

/// The worker-side constructor measures the running system. On a host there is
/// no bound control identity, no `/workspace` identity record and no
/// container hostname, so it can never produce a proof — with or without an
/// invented environment.
#[test]
#[serial_test::serial(private_workspace_fork)]
fn a_bare_host_process_cannot_produce_worker_evidence() {
    let restore = std::env::var("LOOM_PRIVATE_CONTROL").ok();
    for bound in ["", "not-hex", &"a".repeat(64)] {
        std::env::set_var("LOOM_PRIVATE_CONTROL", bound);
        assert!(
            in_container("seat", bound, &"b".repeat(40)).is_err(),
            "a host produced a proof for bound={bound:?}"
        );
    }
    match restore {
        Some(value) => std::env::set_var("LOOM_PRIVATE_CONTROL", value),
        None => std::env::remove_var("LOOM_PRIVATE_CONTROL"),
    }
}

/// A malformed container ID, control identity or base revision is refused
/// before anything else: a lease written before `loom-private-control-v1`
/// deserializes with an empty identity, and that must never read as proven.
#[test]
fn malformed_or_absent_identity_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let profile = profile(dir.path(), Some(&[]), &["fresh"]);
    let job = |container: &str, control: &str, revision: &str| lease::Job {
        owner: "fixture".into(),
        kind: JobKind::Sweep,
        issue: Some(7),
        branch: None,
        container_id: container.into(),
        base_revision: revision.into(),
        host_pid: 1,
        control: control.into(),
    };
    let config = Config {
        schema_version: 1,
        account: "seat".into(),
        container: "c".into(),
        engine: "e".into(),
        docker_desktop: false,
        repository: "https://example.invalid/repo".into(),
        base: "main".into(),
        volume: "v".into(),
        profile: profile.clone(),
        gh_config: None,
    };
    let ok = report(true, &profile);
    for (what, job) in [
        ("empty control identity", job(&"a".repeat(64), "", &"b".repeat(40))),
        ("short control identity", job(&"a".repeat(64), "c", &"b".repeat(40))),
        ("non-hex container", job("not-a-container", &"c".repeat(64), &"b".repeat(40))),
        ("symbolic revision", job(&"a".repeat(64), &"c".repeat(64), "HEAD")),
    ] {
        assert!(host_proof(&config, &ok, &job).is_err(), "{what}");
    }
    let proof =
        host_proof(&config, &ok, &job(&"a".repeat(64), &"c".repeat(64), &"b".repeat(40))).unwrap();
    assert_eq!(proof.runtime(), "codex");
    assert_eq!(proof.control(), &"c".repeat(64));
    let provenance = proof.provenance(
        vec![crate::runtime_admission::CONTAINMENT_SATISFIES.into()],
        std::collections::BTreeMap::new(),
    );
    assert_eq!(provenance.mode, MODE);
    assert_eq!(provenance.protocol, PROTOCOL);
    assert_eq!(provenance.container.len(), 12);
    assert_eq!(provenance.control.len(), 12);
    assert_eq!(provenance.account, "seat");
    // Secret-free: a full container ID, control identity or profile path is
    // never carried into a record that reaches `session status` and logs.
    let rendered = provenance.summary();
    assert!(!rendered.contains(&"a".repeat(64)), "{rendered}");
    assert!(!rendered.contains(&"c".repeat(64)), "{rendered}");
    assert!(!rendered.contains(&*profile.to_string_lossy()), "{rendered}");
}
