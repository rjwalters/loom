//! #10474: the durable, schema-versioned signature-evidence record.
//!
//! Drives the real fetch through the same fake-`gh` fixtures as `tests.rs`
//! (shared helpers, so `#[serial]` for the same `PATH` reason) and asserts the
//! producer-side contract documented in `defaults/docs/daemon-reference.md`.

use super::tests::{
    linux_inputs, required, sha256_hex, signed_assets, tempdir, with_fake_bin,
    write_checksummed_assets, write_fake_gh, write_script, BIN,
};
use super::*;
use crate::release_fetch::evidence::{
    self, EvidenceFacts, EvidenceOutcome, PolicyMode, SignatureEvidence, EVIDENCE_JOURNAL_PATH_ENV,
    EVIDENCE_LINE_PREFIX, EVIDENCE_SCHEMA_VERSION,
};
use serial_test::serial;

/// Fields the record carries today (the original nine evidence keys).
const AVAILABLE_NOW: &[&str] = &[
    "tag",
    "asset_sha256",
    "signature_state",
    "verification_method",
    "identity",
    "identity_regexp",
    "oidc_issuer",
    "configured_workflow",
    "configured_workflow_applied",
];
/// Fields blocked on #10472 / #10473: always `null` + `not_available`.
const BLOCKED: &[&str] = &[
    "source_revision",
    "policy_revision",
    "approval_provenance",
    "root_domain_scope",
];

fn run_evidence(
    fakebin: &Path,
    inputs: &FetchInputs<'_>,
    policy: &SignaturePolicy,
) -> (FetchOutcome, SignatureEvidence) {
    let mut out = None;
    with_fake_bin(fakebin, || {
        out = Some(evidence::fetch_and_verify_with_evidence(inputs, policy));
    });
    out.unwrap()
}

/// Remove a verified outcome's persisted scratch dir; return its signature line.
fn cleanup(outcome: &FetchOutcome) -> Option<String> {
    match outcome {
        FetchOutcome::Verified {
            artifact,
            signature_line,
            ..
        } => {
            let _ = std::fs::remove_dir_all(&artifact.tmp_dir);
            Some(signature_line.clone())
        }
        _ => None,
    }
}

/// The schema every record must satisfy, whatever its verdict.
fn assert_contract(record: &SignatureEvidence) -> serde_json::Value {
    let v = serde_json::to_value(record).unwrap();
    let obj = v.as_object().unwrap();
    assert_eq!(v["schema_version"], EVIDENCE_SCHEMA_VERSION, "{v}");
    for k in AVAILABLE_NOW.iter().chain(&[
        "recorded_at",
        "host_id",
        "loom",
        "policy_mode",
        "outcome",
        "tamper_evidence",
    ]) {
        assert!(obj.contains_key(*k), "missing {k}: {v}");
    }
    for k in BLOCKED {
        assert!(obj.contains_key(*k), "blocked field {k} must be explicit: {v}");
        assert!(v[*k].is_null(), "{k} must never be fabricated: {v}");
        assert_eq!(v[format!("{k}_status")], "not_available", "{v}");
    }
    for k in ["version", "revision", "tree_state", "complete"] {
        assert!(v["loom"].get(k).is_some(), "provenance {k}: {v}");
    }
    v
}

fn evidence_from_line(signature_line: &str) -> Option<serde_json::Value> {
    signature_line
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{EVIDENCE_LINE_PREFIX} ")))
        .map(|j| serde_json::from_str(j).unwrap())
}

#[test]
#[serial]
fn required_mode_record_has_available_fields_and_not_available_markers() {
    let dir = tempdir();
    let assets = signed_assets(&dir, BIN, true);
    let fakebin = tempdir();
    write_fake_gh(&fakebin, &assets);
    write_script(&fakebin, "cosign", "exit 0\n");
    let policy = SignaturePolicy {
        require_signature: true,
        approved_workflow: Some("release.yml".to_string()),
        ..SignaturePolicy::default()
    };
    let (outcome, record) = run_evidence(&fakebin, &linux_inputs(&dir), &policy);
    let line = cleanup(&outcome).expect("verified");
    let v = assert_contract(&record);
    assert_eq!(v["policy_mode"], "required");
    assert_eq!(v["outcome"], "verified");
    assert_eq!(v["tamper_evidence"], false);
    assert_eq!(v["tag"], "v0.16.0");
    assert_eq!(v["asset_sha256"], sha256_hex(b"fake artifact bytes"));
    assert_eq!(v["signature_state"], "verified");
    assert_eq!(v["verification_method"], "cosign-keyless-identity-regexp");
    assert_eq!(v["oidc_issuer"], "https://token.actions.githubusercontent.com");
    assert_eq!(v["configured_workflow"], "release.yml");
    assert_eq!(v["configured_workflow_applied"], true);
    // The stderr line IS the record: a superset of the legacy keys.
    assert_eq!(evidence_from_line(&line).expect("evidence line"), v);
}

#[test]
#[serial]
fn present_only_mode_records_a_verified_release_without_a_stderr_line() {
    let dir = tempdir();
    let assets = signed_assets(&dir, BIN, true);
    let fakebin = tempdir();
    write_fake_gh(&fakebin, &assets);
    write_script(&fakebin, "cosign", "exit 0\n");
    let (outcome, record) =
        run_evidence(&fakebin, &linux_inputs(&dir), &SignaturePolicy::default());
    let line = cleanup(&outcome).expect("verified");
    let v = assert_contract(&record);
    assert_eq!(record.policy_mode, PolicyMode::PresentOnly);
    assert_eq!(v["policy_mode"], "present-only");
    assert_eq!(v["outcome"], "verified");
    assert_eq!(v["signature_state"], "verified");
    // Present-only stderr is unchanged: no evidence line.
    assert!(evidence_from_line(&line).is_none(), "{line}");

    // An unsigned release in present-only mode: verified on its checksum,
    // and the record says exactly that -- no verifier is claimed.
    let dir = tempdir();
    let assets = write_checksummed_assets(&dir, BIN);
    let fakebin = tempdir();
    write_fake_gh(&fakebin, &assets);
    let (outcome, record) =
        run_evidence(&fakebin, &linux_inputs(&dir), &SignaturePolicy::default());
    cleanup(&outcome).expect("verified");
    let v = assert_contract(&record);
    assert_eq!(v["policy_mode"], "present-only");
    assert_eq!(v["signature_state"], "skipped");
    assert!(v["verification_method"].is_null(), "{v}");
}

#[test]
#[serial]
fn refusals_are_distinguishable_and_unavailable_is_not_tampering() {
    // Unsigned, required.
    let dir = tempdir();
    let assets = write_checksummed_assets(&dir, BIN);
    let fakebin = tempdir();
    write_fake_gh(&fakebin, &assets);
    let (o, unsigned) = run_evidence(&fakebin, &linux_inputs(&dir), &required());
    assert!(cleanup(&o).is_none());

    // Signature present, no cosign on this host, required.
    let dir = tempdir();
    let assets = signed_assets(&dir, BIN, true);
    let fakebin = tempdir();
    write_fake_gh(&fakebin, &assets);
    let mut out = None;
    with_fake_bin(&fakebin, || {
        std::env::set_var("PATH", format!("{}:/usr/bin:/bin", fakebin.display()));
        out = Some(evidence::fetch_and_verify_with_evidence(&linux_inputs(&dir), &required()));
    });
    let (o, unavailable) = out.unwrap();
    assert!(cleanup(&o).is_none());

    // Signature present and checked, does not verify (either mode).
    write_script(&fakebin, "cosign", "if [[ \"$1\" == version ]]; then exit 0; fi\nexit 1\n");
    let (o, invalid) = run_evidence(&fakebin, &linux_inputs(&dir), &SignaturePolicy::default());
    assert!(cleanup(&o).is_none());

    // Checksum mismatch.
    let dir = tempdir();
    let assets = dir.join("assets");
    std::fs::create_dir_all(&assets).unwrap();
    std::fs::write(assets.join(BIN), b"fake artifact bytes").unwrap();
    std::fs::write(assets.join(format!("{BIN}.sha256")), format!("{}  {BIN}\n", "0".repeat(64)))
        .unwrap();
    let fakebin = tempdir();
    write_fake_gh(&fakebin, &assets);
    let (o, mismatch) = run_evidence(&fakebin, &linux_inputs(&dir), &required());
    assert!(cleanup(&o).is_none());

    for r in [&unsigned, &unavailable, &invalid, &mismatch] {
        assert_contract(r);
    }
    assert_eq!(unsigned.outcome, EvidenceOutcome::RefusedUnsigned);
    assert_eq!(unsigned.signature_state.as_deref(), Some("skipped"));
    assert!(!unsigned.tamper_evidence);

    assert_eq!(unavailable.outcome, EvidenceOutcome::RefusedUnavailable);
    assert_eq!(unavailable.signature_state.as_deref(), Some("unavailable"));
    assert!(!unavailable.tamper_evidence, "a tooling gap is not tampering");

    assert_eq!(invalid.outcome, EvidenceOutcome::SignatureInvalid);
    assert_eq!(invalid.policy_mode, PolicyMode::PresentOnly);
    assert!(invalid.tamper_evidence);
    assert!(invalid.verification_method.is_none(), "no verifier succeeded");

    assert_eq!(mismatch.outcome, EvidenceOutcome::ChecksumMismatch);
    assert!(mismatch.tamper_evidence);
    assert!(mismatch.asset_sha256.is_none(), "an unmatched digest is not adopted");

    let v = serde_json::to_value(&unavailable).unwrap();
    assert_eq!(v["outcome"], "refused_unavailable");
}

/// Collect every JSON string value in `v`.
fn strings(v: &serde_json::Value, out: &mut Vec<String>) {
    match v {
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Array(a) => a.iter().for_each(|x| strings(x, out)),
        serde_json::Value::Object(o) => o.values().for_each(|x| strings(x, out)),
        _ => {}
    }
}

#[test]
#[serial]
fn sanitized_record_has_no_token_credential_path_or_env_value() {
    const SECRET: &str = "ghp_SANITIZATIONCANARY0000";
    let saved: Vec<_> = [
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "LOOM_DAEMON_UPDATE_COSIGN_PUBKEY",
    ]
    .iter()
    .map(|k| (*k, std::env::var_os(k)))
    .collect();
    std::env::set_var("GH_TOKEN", SECRET);
    std::env::set_var("GITHUB_TOKEN", SECRET);

    let dir = tempdir();
    let assets = signed_assets(&dir, BIN, false);
    let fakebin = tempdir();
    write_fake_gh(&fakebin, &assets);
    write_script(&fakebin, "cosign", "exit 0\n");
    let pubkey = dir.join("test-cosign.pub");
    std::fs::write(&pubkey, b"fake pubkey").unwrap();
    std::env::set_var("LOOM_DAEMON_UPDATE_COSIGN_PUBKEY", &pubkey);
    let mut inputs = linux_inputs(&dir);
    inputs.cosign_pubkey_env = Some(pubkey.to_string_lossy().into_owned());
    let policy = SignaturePolicy {
        require_signature: true,
        approved_workflow: Some("release.yml".to_string()),
        ..SignaturePolicy::default()
    };
    let (outcome, record) = run_evidence(&fakebin, &inputs, &policy);
    let line = cleanup(&outcome).expect("verified in key mode");

    for (k, v) in saved {
        match v {
            Some(v) => std::env::set_var(k, v),
            None => std::env::remove_var(k),
        }
    }

    let v = assert_contract(&record);
    assert_eq!(v["verification_method"], "cosign-key");
    let serialized = serde_json::to_string(&record).unwrap();
    let mut forbidden = vec![
        SECRET.to_string(),
        dir.display().to_string(),
        pubkey.display().to_string(),
        fakebin.display().to_string(),
        std::env::temp_dir().display().to_string(),
        "fake pubkey".to_string(),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        forbidden.push(home.to_string_lossy().into_owned());
    }
    for f in &forbidden {
        assert!(!serialized.contains(f.as_str()), "record leaks {f:?}: {serialized}");
        assert!(!line.contains(f.as_str()), "stderr line leaks {f:?}: {line}");
    }
    let mut all = Vec::new();
    strings(&v, &mut all);
    for s in all {
        assert!(!s.starts_with('/') && !s.starts_with('~'), "path-shaped value {s:?}");
    }
}

#[test]
#[serial]
fn keyless_regexp_does_not_leak_rejected_workflow() {
    for bad in [
        "ghp_SECRETTOKEN123/release.yml",
        "/Users/me/secret/release.yml",
    ] {
        let dir = tempdir();
        let assets = signed_assets(&dir, BIN, true);
        let fakebin = tempdir();
        write_fake_gh(&fakebin, &assets);
        write_script(&fakebin, "cosign", "exit 0\n");
        let policy = SignaturePolicy {
            require_signature: true,
            approved_workflow: Some(bad.to_string()),
            ..SignaturePolicy::default()
        };
        let (outcome, record) = run_evidence(&fakebin, &linux_inputs(&dir), &policy);
        let line = cleanup(&outcome).expect("verified");
        let v = assert_contract(&record);
        assert_eq!(v["verification_method"], "cosign-keyless-identity-regexp");
        assert!(v["identity_regexp"].is_null(), "{bad:?}");
        assert!(v["configured_workflow"].is_null(), "{bad:?}");
        let needle = bad.trim_start_matches('/');
        assert!(!serde_json::to_string(&v).unwrap().contains(needle), "{bad:?}");
        assert!(!line.contains(needle), "{bad:?}");
    }
    // A legitimate pin keeps its (escaped) regexp.
    let r = SignatureEvidence::assemble(
        "v1.0.0",
        &SignaturePolicy {
            require_signature: true,
            approved_workflow: Some("release.yml".to_string()),
            ..SignaturePolicy::default()
        },
        &EvidenceFacts {
            verified_by: Some(crate::release_fetch::signature::VerifiedBy::KeylessIdentityRegexp {
                regexp:
                    r"^https://github\.com/o/r/\.github/workflows/release\.yml@refs/tags/v1\.0\.0$"
                        .to_string(),
                issuer: "https://token.actions.githubusercontent.com".to_string(),
                workflow_pinned: true,
            }),
            ..EvidenceFacts::default()
        },
        "host-a".to_string(),
        crate::telemetry::provenance::Provenance::current(),
        chrono::Utc::now(),
    );
    assert!(r.identity_regexp.is_some());
}

#[test]
fn sanitize_drops_path_and_token_shaped_public_text() {
    for bad in [
        "/Users/me/.loom/release.yml",
        "~/release.yml",
        r"C:\x\release.yml",
        "ghp_abc",
        "github_pat_abc",
        "",
        "a\nb",
    ] {
        assert!(evidence::public_text(bad).is_none(), "{bad:?}");
    }
    assert_eq!(evidence::public_text(" release.yml ").as_deref(), Some("release.yml"));
    assert!(evidence::public_text("https://token.actions.githubusercontent.com").is_some());

    // A path-shaped workflow pin is dropped, not published.
    let policy = SignaturePolicy {
        require_signature: true,
        approved_workflow: Some("/Users/me/secret/release.yml".to_string()),
        ..SignaturePolicy::default()
    };
    let r = SignatureEvidence::assemble(
        "v1.0.0",
        &policy,
        &EvidenceFacts::default(),
        "host-a".to_string(),
        crate::telemetry::provenance::Provenance::current(),
        chrono::Utc::now(),
    );
    assert!(r.configured_workflow.is_none());
    assert_eq!(r.outcome, EvidenceOutcome::DownloadFailed);
    assert!(!r.tamper_evidence);
}

#[test]
fn identity_and_issuer_reject_credential_urls_and_local_paths() {
    use crate::release_fetch::signature::VerifiedBy;
    let canaries = [
        "https://demo:FAKE_PASSWORD@example.invalid/issuer",
        "https://example.invalid/issuer?access_token=FAKE_CANARY",
        "https://example.invalid/issuer#access_token=FAKE_CANARY",
        "file:///Users/example/private/identity",
        "../private/identity",
        "private/identity",
    ];
    let record = |verified_by| {
        SignatureEvidence::assemble(
            "v1.0.0",
            &SignaturePolicy {
                require_signature: true,
                approved_workflow: None,
                ..SignaturePolicy::default()
            },
            &EvidenceFacts {
                verified_by: Some(verified_by),
                ..EvidenceFacts::default()
            },
            "host-a".to_string(),
            crate::telemetry::provenance::Provenance::current(),
            chrono::Utc::now(),
        )
    };
    for bad in canaries {
        let r = record(VerifiedBy::KeylessExactIdentity {
            identity: bad.to_string(),
            issuer: bad.to_string(),
        });
        assert!(r.identity.is_none() && r.oidc_issuer.is_none(), "{bad:?}");
        let needle = bad.trim_start_matches("../");
        assert!(!serde_json::to_string(&r).unwrap().contains(needle), "{bad:?}");
        assert!(!r.stderr_line().contains(needle), "{bad:?}");
    }
    // Valid public identities and issuers are preserved.
    let r = record(VerifiedBy::KeylessExactIdentity {
        identity: "https://github.com/o/r/.github/workflows/release.yml@refs/tags/v1.0.0"
            .to_string(),
        issuer: "https://token.actions.githubusercontent.com".to_string(),
    });
    assert!(r.identity.is_some() && r.oidc_issuer.is_some());
    let r = record(VerifiedBy::KeylessExactIdentity {
        identity: "releases@example.com".to_string(),
        issuer: "https://accounts.example.com/".to_string(),
    });
    assert!(r.identity.is_some() && r.oidc_issuer.is_some());
}

fn sample(tag: &str) -> SignatureEvidence {
    SignatureEvidence::assemble(
        tag,
        &SignaturePolicy::default(),
        &EvidenceFacts::default(),
        "host-a".to_string(),
        crate::telemetry::provenance::Provenance::current(),
        chrono::Utc::now(),
    )
}

#[test]
#[serial]
fn journal_appends_round_trips_rotates_and_skips_non_loom_checkouts() {
    std::env::remove_var(EVIDENCE_JOURNAL_PATH_ENV);
    let bare = tempdir();
    assert!(evidence::journal_path(&bare).is_none(), "never create .loom/ in a bare dir");
    assert!(evidence::record(&bare, &sample("v1")).is_none());
    assert!(!bare.join(".loom").exists());

    let repo = tempdir();
    std::fs::create_dir_all(repo.join(".loom")).unwrap();
    let path = evidence::record(&repo, &sample("v1")).expect("journaled");
    assert_eq!(path, repo.join(".loom/logs/signature-evidence.jsonl"));
    evidence::record(&repo, &sample("v2")).unwrap();
    let all = evidence::read_all(&path);
    assert_eq!(all.len(), 2);
    assert_eq!(all[1].tag, "v2");

    // Rotation past the size cap.
    std::fs::write(&path, vec![b'x'; usize::try_from(evidence::MAX_JOURNAL_BYTES).unwrap() + 1])
        .unwrap();
    evidence::append(&path, &sample("v3")).unwrap();
    assert!(repo.join(".loom/logs/signature-evidence.jsonl.1").is_file());
    assert_eq!(evidence::read_all(&path).len(), 1);

    // Env override wins.
    let custom = tempdir().join("nested/evidence.jsonl");
    std::env::set_var(EVIDENCE_JOURNAL_PATH_ENV, &custom);
    assert_eq!(evidence::record(&bare, &sample("v4")), Some(custom.clone()));
    std::env::remove_var(EVIDENCE_JOURNAL_PATH_ENV);
    assert_eq!(evidence::read_all(&custom)[0].tag, "v4");
}
