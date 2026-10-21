//! Containment evidence that can be measured without Docker (#8787).
//!
//! What is covered here: the obligations containment does NOT satisfy and must
//! therefore prove separately (a managed registration exists at all; Codex hook
//! trust has been established), the proof's own shape validation, and the fact
//! that a bare host process cannot produce worker-side evidence.
//!
//! The trust rule is additionally cross-checked against the SHIPPED
//! `provision-codex-hooks.sh verify` (native since #9390) on the same fixture profiles, so the Rust
//! gate and the shell gate can never drift into disagreeing about whether one
//! profile is trusted.
//!
//! Docker-bound evidence — mount topology, exclusive volume/profile ownership,
//! the sealed bundle, the bound control identity and its rechecks — is covered
//! by `tests/private_workspace_docker`.
use super::containment::*;
use super::*;

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
    // Codex keeps ONE trusted_hash per key, and Loom's entry has one key in a
    // private session; the other hashes are recorded for other hooks. Loom's
    // key carries the LAST, most recent decision.
    for (index, hash) in trusted.iter().enumerate() {
        let key = if index + 1 == trusted.len() {
            LOOM_KEY.to_owned()
        } else {
            format!("/elsewhere/profile-{index}/hooks.json:pre_tool_use:0:0")
        };
        config.push_str(&format!("[hooks.state.\"{key}\"]\ntrusted_hash = \"{hash}\"\n"));
    }
    std::fs::write(profile.join("config.toml"), config).unwrap();
    profile
}

/// The key Codex records trust for Loom's entry under in a private session.
const LOOM_KEY: &str = "/home/loom/.codex-profile/hooks.json:pre_tool_use:0:0";

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
/// up under. Each must read as untrusted, with a fresh baseline that would
/// otherwise admit any new hash.
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

#[test]
fn trust_counts_only_under_the_key_codex_looks_loom_up_under() {
    for (what, config) in WRONG_LOCATION_CASES {
        let dir = tempfile::tempdir().unwrap();
        let profile = profile(dir.path(), Some(&[]), &[]);
        std::fs::write(profile.join("config.toml"), config).unwrap();
        assert!(enforcing(&report(true, &profile), &profile).is_err(), "{what}");
    }
    let dir = tempfile::tempdir().unwrap();
    let profile = profile(dir.path(), Some(&[]), &["fresh"]);
    assert!(enforcing(&report(true, &profile), &profile).is_ok());
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

/// Private admission and `provision-codex-hooks.sh verify` (since #9390 the
/// native `codex_hooks::Check`) must agree, profile for profile: one refusing
/// while the other admits is exactly the drift this cross-check exists to
/// prevent. They now share the trust reading, and this pins that they also
/// agree on the verdict built from it.
#[test]
fn the_rust_trust_gate_agrees_with_the_shipped_provisioner() {
    use super::super::codex_hooks::{Check, Registration};
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let bridge = repo.join("defaults/hooks/guard-codex-bridge.sh");
    for (what, baseline, trusted, expected) in TRUST_CASES {
        let dir = tempfile::tempdir().unwrap();
        let profile = profile(dir.path(), *baseline, trusted);
        let verdict = Check {
            codex_home: profile.clone(),
            workspace: Some(PathBuf::from(REPO)),
            registration: Registration::Pinned {
                bridge: bridge.clone(),
            },
            fallback_bridge: None,
            // A private session runs Codex with CODEX_HOME at the container's
            // mount point; that is where its trust is keyed.
            runtime_home: Some(PathBuf::from(PROFILE)),
            sealed: None,
        }
        .verify();
        assert_eq!(verdict.trusted, *expected, "{what}: verify verdict {verdict:?}");
        assert_eq!(
            enforcing(&report(true, &profile), &profile).is_ok(),
            verdict.trusted,
            "{what}: the admission gate disagrees with verify"
        );
    }
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
            in_container("seat", bound, &"b".repeat(40), "https://example.invalid/o/r", REPO)
                .is_err(),
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
        repository: String::new(),
    };
    let config = Config {
        schema_version: 1,
        account: "seat".into(),
        container: "c".into(),
        engine: "e".into(),
        docker_desktop: false,
        repository: "https://example.invalid/repo".into(),
        base: "main".into(),
        repositories: vec![],
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
        assert!(host_proof(&config, &ok, &job, &config.primary()).is_err(), "{what}");
    }
    let proof = host_proof(
        &config,
        &ok,
        &job(&"a".repeat(64), &"c".repeat(64), &"b".repeat(40)),
        &config.primary(),
    )
    .unwrap();
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

/// A proof certifies one clone (#10796): the durable job must name exactly the
/// clone the selection prepared, and a proof never answers for another
/// repository.
#[test]
fn a_proof_is_bound_to_the_clone_it_was_measured_against() {
    let dir = tempfile::tempdir().unwrap();
    let profile = profile(dir.path(), Some(&[]), &["fresh"]);
    let mut config = Config {
        schema_version: 1,
        account: "seat".into(),
        container: "c".into(),
        engine: "e".into(),
        docker_desktop: false,
        repository: "https://example.invalid/o/primary".into(),
        base: "main".into(),
        repositories: vec![],
        volume: "v".into(),
        profile: profile.clone(),
        gh_config: None,
    };
    let second = identify("https://example.invalid/o/second").unwrap();
    config.repositories.push(Registered {
        repository: second.url.clone(),
        base: "main".into(),
        slug: second.slug.clone(),
    });
    let job = |repository: &str| lease::Job {
        owner: "fixture".into(),
        kind: JobKind::Role,
        issue: None,
        branch: None,
        container_id: "a".repeat(64),
        base_revision: "b".repeat(40),
        host_pid: 1,
        control: "c".repeat(64),
        repository: repository.into(),
    };
    let ok = report(true, &profile);
    let primary = config.primary();
    let other = config.target(Some(&second.url)).unwrap();

    // The lease of one clone never certifies the other, in either direction.
    assert!(host_proof(&config, &ok, &job(""), &other).is_err());
    assert!(host_proof(&config, &ok, &job(&second.url), &primary).is_err());
    assert!(host_proof(&config, &ok, &job("https://example.invalid/o/third"), &other).is_err());

    let proof = host_proof(&config, &ok, &job(&second.url), &other).unwrap();
    proof
        .require_repository("https://example.invalid/o/second.git")
        .unwrap();
    assert!(proof
        .require_repository("https://example.invalid/o/primary")
        .is_err());
    let provenance = proof.provenance(vec![], std::collections::BTreeMap::new());
    assert_eq!(provenance.repository, "example.invalid/o/second");
    assert_eq!(provenance.clone, format!("{WORK_ROOT}/{}", second.slug));
    assert!(provenance
        .summary()
        .contains("repository=example.invalid/o/second"));

    let primary_proof = host_proof(&config, &ok, &job(""), &primary).unwrap();
    assert_eq!(primary_proof.provenance(vec![], Default::default()).clone, REPO);
}

/// Only the primary and a slug directly under the work root are clones.
#[test]
fn only_known_clone_shapes_are_accepted() {
    for ok in [REPO, "/workspace/work/o-n-0123abcd"] {
        assert!(valid_clone(ok), "{ok}");
    }
    for bad in [
        "",
        "/workspace",
        "/workspace/repo/x",
        "/workspace/work",
        "/workspace/work/",
        "/workspace/work/../repo",
        "/workspace/work/a/b",
        "/host/repo",
    ] {
        assert!(!valid_clone(bad), "{bad}");
    }
}

/// The worker looks the identity record up by the repository's own slug, so an
/// additional clone cannot be answered for by another clone's record.
#[test]
fn identity_record_is_derived_from_the_repository_not_chosen() {
    let id = identify("https://example.invalid/o/second").unwrap();
    assert_eq!(
        identity_record("https://example.invalid/o/second", &format!("{WORK_ROOT}/{}", id.slug))
            .unwrap(),
        Path::new(WORK_ROOT)
            .join(".identity")
            .join(format!("{}.json", id.slug))
    );
    assert_eq!(
        identity_record("https://example.invalid/o/primary", REPO).unwrap(),
        Path::new(ROOT).join("identity.json")
    );
    // A clone directory that is not this repository's slug is another clone.
    assert!(identity_record(
        "https://example.invalid/o/other",
        &format!("{WORK_ROOT}/{}", id.slug)
    )
    .is_err());
}

/// A seat holding a primary and one additional clone, on `profile`.
fn multi_clone_config(profile: &Path) -> (Config, RepoId) {
    let mut config = Config {
        schema_version: 2,
        account: "seat".into(),
        container: "c".into(),
        engine: "e".into(),
        docker_desktop: false,
        repository: "https://example.invalid/o/primary".into(),
        base: "main".into(),
        repositories: vec![],
        volume: "v".into(),
        profile: profile.to_owned(),
        gh_config: None,
    };
    let second = identify("https://example.invalid/o/second").unwrap();
    config.repositories.push(Registered {
        repository: second.url.clone(),
        base: "main".into(),
        slug: second.slug.clone(),
    });
    (config, second)
}

/// AC3 of #10796 on the selected-clone path: the host proof for an additional
/// clone is gated by the SAME hook-trust rule as the primary's — trust that
/// Codex records for Loom's entry under the session's `CODEX_HOME` mount
/// point, `/home/loom/.codex-profile/hooks.json` — and never by anything
/// about the clone. Trust recorded under a clone's own path, the host-side
/// profile path, another profile or another position refuses BOTH clones;
/// trust at the session key admits both, each bound to its own clone; and a
/// boundary with no managed registration refuses both. A container label or
/// capability flag is never consulted: the only inputs are the observed
/// report and the profile's own control files.
#[test]
fn hook_trust_is_proven_at_the_session_key_for_every_selected_clone() {
    let dir = tempfile::tempdir().unwrap();
    let profile = profile(dir.path(), Some(&[]), &[]);
    let (config, second) = multi_clone_config(&profile);
    let primary = config.primary();
    let other = config.target(Some(&second.url)).unwrap();
    assert_eq!(other.path, format!("{WORK_ROOT}/{}", second.slug));
    let job = |target: &Target| lease::Job {
        owner: "fixture".into(),
        kind: JobKind::Role,
        issue: None,
        branch: None,
        container_id: "a".repeat(64),
        base_revision: "b".repeat(40),
        host_pid: 1,
        control: "c".repeat(64),
        repository: dispatch::job_repository(target),
    };
    let ok = report(true, &profile);

    // Trust keyed by the additional clone's own path: a plausible mistake once
    // a seat runs Codex from more than one directory, and still not where
    // Codex looks Loom's entry up (CODEX_HOME is the profile, not the clone).
    let clone_keyed = format!(
        "[hooks.state.\"{}/hooks.json:pre_tool_use:0:0\"]\ntrusted_hash = \"fresh\"\n",
        other.path
    );
    let wrong: Vec<(&str, &str)> = WRONG_LOCATION_CASES
        .iter()
        .copied()
        .chain([("the additional clone's own path", clone_keyed.as_str())])
        .collect();
    for (what, text) in &wrong {
        std::fs::write(profile.join("config.toml"), text).unwrap();
        for target in [&primary, &other] {
            let error = host_proof(&config, &ok, &job(target), target).unwrap_err();
            let text = error.to_string();
            assert!(
                text.contains("protected remote operations"),
                "{what} / {}: {text}",
                target.path
            );
            assert!(!text.contains(&*profile.to_string_lossy()), "{text}");
            assert!(!text.contains("fresh"), "{text}");
        }
    }
    // No trust anywhere: the same refusal for both clones.
    std::fs::write(profile.join("config.toml"), "model = \"fixture\"\n").unwrap();
    for target in [&primary, &other] {
        assert!(host_proof(&config, &ok, &job(target), target).is_err(), "{}", target.path);
    }

    // Trust at the session key admits each clone — bound to that clone, never
    // to the other — with the same registration and receipt as before.
    std::fs::write(
        profile.join("config.toml"),
        format!("[hooks.state.\"{LOOM_KEY}\"]\ntrusted_hash = \"fresh\"\n"),
    )
    .unwrap();
    for target in [&primary, &other] {
        let proof = host_proof(&config, &ok, &job(target), target).unwrap();
        let provenance = proof.provenance(vec![], Default::default());
        assert_eq!(provenance.clone, target.path);
        assert_eq!(provenance.policy, POLICY);
        assert!(provenance.policy.contains("managed-hook-trusted"), "{}", provenance.policy);
    }
    let proof = host_proof(&config, &ok, &job(&other), &other).unwrap();
    assert!(proof.require_repository(&config.repository).is_err());
    proof.require_repository(&second.url).unwrap();

    // Trust alone is not enough: with no managed registration the additional
    // clone is refused exactly as the primary is.
    let unmanaged = report(false, &profile);
    for target in [&primary, &other] {
        let error = host_proof(&config, &unmanaged, &job(target), target).unwrap_err();
        assert!(error.to_string().contains("no Loom hook provisioner"), "{error}");
    }
}
