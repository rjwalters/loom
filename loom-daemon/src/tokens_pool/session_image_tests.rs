use super::*;
use serde_json::json;

/// Probe double: `Some` output is returned as stdout, `None` fails the probe.
struct FakeProbe(Option<String>);

impl BundleProbe for FakeProbe {
    fn run_image_script(&self, _image: &str, _script: &str) -> Result<String> {
        match &self.0 {
            Some(output) => Ok(output.clone()),
            None => Err(anyhow::anyhow!("docker unavailable")),
        }
    }
}

fn good_manifest() -> Manifest {
    let mut assets = BTreeMap::new();
    for relative in bundle::sealed_assets() {
        assets.insert((*relative).to_owned(), format!("digest-of-{relative}"));
    }
    Manifest {
        protocol: CONTROL_PROTOCOL.into(),
        control_version: CONTROL_VERSION,
        codex_cli: "codex-cli 0.160.0".into(),
        codex_schema_pin: "0.146.0".into(),
        assets,
        policy: bundle::expected_policy(),
        registration: bundle::registration(),
    }
}

/// What [`bundle_probe_script`] prints for an image whose bundle is intact.
fn good_probe_output(manifest: &Manifest) -> String {
    let mut out = serde_json::to_string_pretty(manifest).expect("manifest JSON");
    out.push('\n');
    out.push_str(BUNDLE_PROBE_MARKER);
    out.push('\n');
    let mut assets: Vec<(&String, &String)> = manifest.assets.iter().collect();
    assets.sort_by(|a, b| a.0.cmp(b.0));
    for (path, digest) in assets {
        out.push_str(&format!("{digest}  ./{path}\n"));
    }
    out
}

fn verify_with(output: Option<String>) -> Result<()> {
    let probe = FakeProbe(output);
    verify_override_bundle("ghcr.io/example/session-mine@sha256:deadbeef", &probe)
}

#[test]
fn digest_pin_is_required() {
    assert!(validate_digest_pin("ghcr.io/example/img@sha256:0000").is_err());
    let short = format!("ghcr.io/example/img@sha256:{}", "a".repeat(63));
    assert!(validate_digest_pin(&short).is_err());
    let good = format!("ghcr.io/example/img@sha256:{}", "a".repeat(64));
    assert!(validate_digest_pin(&good).is_ok());
    let upper = format!("ghcr.io/example/img@sha256:{}", "A".repeat(64));
    assert!(validate_digest_pin(&upper).is_ok());
    assert!(validate_digest_pin("ghcr.io/example/img:latest").is_err());
    assert!(validate_digest_pin("ghcr.io/example/img:0.19.1").is_err());
    assert!(validate_digest_pin("@sha256:").is_err());
}

#[test]
fn extra_mounts_parse_and_reject_rw_and_other_keys() {
    let ok = json!([
        {"host": "/opt/pdk", "container": "/opt/pdk"},
    ]);
    let parsed = parse_extra_mounts_config(Some(&ok)).expect("valid mounts");
    assert_eq!(
        parsed,
        vec![ExtraMount {
            host: PathBuf::from("/opt/pdk"),
            container: PathBuf::from("/opt/pdk"),
        }]
    );

    for (case, why) in [
        (json!([{ "host": "/h", "container": "/c", "mode": "rw" }]), "rw mode key"),
        (json!([{ "host": "/h", "container": "/c", "readonly": true }]), "readonly key"),
        (json!([{ "host": "/h", "container": "/c", "rw": true }]), "rw key"),
        (json!([{ "host": "/h" }]), "missing container"),
        (json!([{ "container": "/c" }]), "missing host"),
        (json!([{ "host": "relative", "container": "/c" }]), "relative host"),
        (json!([{ "host": "/h", "container": "" }]), "empty container"),
        (json!([{}]), "empty object"),
        (json!(["/opt/pdk"]), "bare string"),
        (json!({ "host": "/h" }), "not an array"),
    ] {
        let error = parse_extra_mounts_config(Some(&case)).expect_err(why);
        assert!(!error.to_string().is_empty(), "{why} refused");
    }

    assert!(parse_extra_mounts_config(None).unwrap().is_empty());
    assert!(parse_extra_mounts_config(Some(&json!(null)))
        .unwrap()
        .is_empty());
}

#[test]
fn config_reads_when_unset_and_when_set() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    // No config at all: today's behaviour.
    assert_eq!(
        read_session_image_config(root).expect("no config"),
        SessionImageConfig::default()
    );

    std::fs::create_dir_all(root.join(".loom")).expect("mkdir");
    std::fs::write(
        root.join(".loom/config.json"),
        r#"{"accounts": {"session": {"image": "ghcr.io/example/img:latest",
            "extraMounts": [{"host": "/opt/pdk", "container": "/opt/pdk"}]}}}"#,
    )
    .expect("write config");
    let error = read_session_image_config(root).expect_err("tag-only image is refused");
    assert!(error.to_string().contains("digest-pinned"), "{error:#}");

    let good = format!("ghcr.io/example/img@sha256:{}", "a".repeat(64));
    std::fs::write(
        root.join(".loom/config.json"),
        format!(
            r#"{{"accounts": {{"session": {{"image": "{good}",
                "extraMounts": [{{"host": "/opt/pdk", "container": "/opt/pdk"}}]}}}}}}"#
        ),
    )
    .expect("write config");
    let config = read_session_image_config(root).expect("valid config");
    assert_eq!(config.image.as_deref(), Some(good.as_str()));
    assert!(config.has_image_override());
    assert_eq!(config.extra_mounts.len(), 1);
}

#[test]
fn bundle_verification_passes_an_intact_derived_image() {
    let manifest = good_manifest();
    verify_with(Some(good_probe_output(&manifest))).expect("intact image is accepted");
}

#[test]
fn bundle_verification_refuses_a_missing_manifest() {
    let error = verify_with(Some(format!("{}\n\n", BUNDLE_PROBE_MISSING)))
        .expect_err("no manifest is refused");
    assert!(error.to_string().to_lowercase().contains("bundle"), "{error:#}");
}

#[test]
fn bundle_verification_refuses_an_altered_asset() {
    let manifest = good_manifest();
    let output = good_probe_output(&manifest);
    let (head, tail) = output
        .split_once(BUNDLE_PROBE_MARKER)
        .expect("probe output shape");
    let first_asset = manifest.assets.keys().next().expect("assets").clone();
    // Tamper with the RECOMPUTED digest only, so the recomputed content no
    // longer matches what the (untouched) manifest sealed.
    let sealed = &manifest.assets[&first_asset];
    let tampered_tail = tail.replace(sealed, &"0".repeat(sealed.len()));
    assert_ne!(tampered_tail, tail, "fixture tampered something");
    let output = format!("{head}{}{}", BUNDLE_PROBE_MARKER, tampered_tail);
    let error = verify_with(Some(output)).expect_err("altered asset is refused");
    assert!(
        error.to_string().contains(&first_asset),
        "diagnostic names the asset: {error:#}"
    );
}

#[test]
fn bundle_verification_refuses_a_missing_sealed_asset() {
    let mut manifest = good_manifest();
    let dropped = bundle::sealed_assets().next().expect("assets");
    manifest.assets.remove(dropped);
    let error =
        verify_with(Some(good_probe_output(&manifest))).expect_err("missing asset is refused");
    assert!(
        error.to_string().contains(dropped),
        "diagnostic names the missing asset: {error:#}"
    );
}

#[test]
fn bundle_verification_refuses_an_old_control_version() {
    let mut manifest = good_manifest();
    manifest.control_version = CONTROL_VERSION - 1;
    let error =
        verify_with(Some(good_probe_output(&manifest))).expect_err("stale bundle is refused");
    assert!(error.to_string().contains("rebuild"), "diagnostic says to rebuild: {error:#}");
}

#[test]
fn bundle_verification_refuses_a_policy_mismatch() {
    let mut manifest = good_manifest();
    let key = bundle::POLICY[0].0;
    manifest.policy.insert(key.to_owned(), "off".into());
    let error =
        verify_with(Some(good_probe_output(&manifest))).expect_err("policy drift is refused");
    assert!(error.to_string().contains(key), "diagnostic names the differing key: {error:#}");
}

#[test]
fn bundle_verification_refuses_an_unknown_sealed_asset() {
    let mut manifest = good_manifest();
    manifest
        .assets
        .insert("hooks/extra-thing.sh".into(), "digest".into());
    let error =
        verify_with(Some(good_probe_output(&manifest))).expect_err("extra asset is refused");
    assert!(error.to_string().contains("extra-thing.sh"), "{error:#}");
}

#[test]
fn bundle_verification_refuses_a_failed_probe() {
    let error = verify_with(None).expect_err("a failed probe is refused");
    assert!(error.to_string().contains("probing session image"), "{error:#}");
}

#[test]
fn label_json_round_trips() {
    let mounts = vec![ExtraMount {
        host: PathBuf::from("/opt/pdk"),
        container: PathBuf::from("/srv/pdk"),
    }];
    let json = extra_mounts_label_json(&mounts);
    assert_eq!(parse_extra_mounts_label(&json), mounts);
    assert!(parse_extra_mounts_label("not json").is_empty());
}
