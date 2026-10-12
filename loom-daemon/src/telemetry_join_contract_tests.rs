//! Contract regressions use unmistakably fake secrets and temporary external
//! paths. Never contact SSM, Access or the production reader.
use super::*;

const PAIR: &str = "CF-Access-Client-Id: FAKE-client-id-for-tests\nCF-Access-Client-Secret: FAKE-client-secret-for-tests\n";

#[tokio::test]
#[serial]
async fn existing_access_pair_reaches_the_live_sender_without_bearer() {
    let _guard = HostGuard::at("this-host");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("otlp-headers");
    write_credential_headers(&path, PAIR).unwrap();
    let spec = parse_credential_spec(&format!("headers-file:{}", path.display())).unwrap();
    let credential = credential_source(&spec, None);
    let sink = AuthSink::answering(200);
    let mut plan = plan_at(dir.path(), "this-host");
    plan.endpoint = sink.base_url();
    let sender = OtlpProbeSender::new(plan.endpoint.clone(), path.clone());
    let mut reader = FakeReader::always(vec![ProbeRow {
        host_id: Some("this-host".into()),
        ingest_principal: Some("this-host".into()),
    }]);
    join(
        &plan,
        JoinDeps {
            credential: credential.as_ref(),
            sender: &sender,
            reader: &mut reader,
        },
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), PAIR);
    assert_eq!(sink.auth_headers(), vec![None], "file auth must not add an empty Bearer");
    assert_eq!(
        *sink.access.lock().unwrap(),
        vec![(
            Some("FAKE-client-id-for-tests".into()),
            Some("FAKE-client-secret-for-tests".into())
        )]
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        // SAFETY: geteuid has no side effects.
        assert_eq!(meta.uid(), unsafe { libc::geteuid() });
    }
}

#[test]
fn invalid_or_partial_headers_leave_existing_pair_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("otlp-headers");
    write_credential_headers(&path, PAIR).unwrap();
    for invalid in [
        "CF-Access-Client-Id: FAKE-only-id\n",
        "CF-Access-Client-Secret: FAKE-only-secret\n",
        "not-a-header-FAKE-secret",
        "Host: fake.example\n",
    ] {
        let error = write_credential_headers(&path, invalid).unwrap_err();
        assert!(!error.to_string().contains("FAKE"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), PAIR);
    }
    assert!(write_headers_file(&path, "FAKE-token\nInjected: FAKE-secret").is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), PAIR);
    assert!(!format!("{:?}", Credential::Headers(PAIR.into())).contains("FAKE"));
}

#[test]
fn repository_and_symlink_destinations_are_refused_without_clobbering() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".git")).unwrap();
    let path = dir.path().join("credentials");
    assert!(write_credential_headers(&path, PAIR).is_err());
    assert!(!path.exists());
    #[cfg(unix)]
    {
        let external = tempfile::tempdir().unwrap();
        let target = external.path().join("target");
        write_credential_headers(&target, PAIR).unwrap();
        let link = external.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(write_headers_file(&link, "FAKE-replacement").is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), PAIR);
    }
}

#[cfg(unix)]
#[test]
fn loose_header_source_is_refused_before_copying() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source");
    std::fs::write(&path, PAIR).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let spec = parse_credential_spec(&format!("headers-file:{}", path.display())).unwrap();
    let error = credential_source(&spec, None).credential().unwrap_err();
    assert!(!error.to_string().contains("FAKE"));
}

#[test]
fn access_default_and_legacy_scalar_specs_remain_distinct() {
    let spec = parse_credential_spec(&default_credential_spec("this-host")).unwrap();
    assert_eq!(
        spec,
        CredentialSpec::SsmAccess {
            prefix: "/2am/cloudflare/otlp-host/this-host".into()
        }
    );
    assert!(matches!(
        parse_credential_spec("ssm:///fake/scalar").unwrap(),
        CredentialSpec::Ssm { .. }
    ));
    assert!(matches!(
        parse_credential_spec("file:/external/FAKE-token").unwrap(),
        CredentialSpec::File { .. }
    ));
    assert!(matches!(
        parse_credential_spec("age-file:/external/FAKE-token.age").unwrap(),
        CredentialSpec::AgeFile { .. }
    ));
    for bad in ["ssm-access://", "headers-file:"] {
        assert!(parse_credential_spec(bad).is_err());
    }
}

#[test]
fn missing_parent_traversal_cannot_create_a_repository_secret() {
    let sandbox = tempfile::tempdir().unwrap();
    let outside = sandbox.path().join("outside");
    let repo = sandbox.path().join("repo");
    std::fs::create_dir(&outside).unwrap();
    std::fs::create_dir(&repo).unwrap();
    std::fs::create_dir(repo.join(".git")).unwrap();
    let route = outside.join("missing/../../repo/new-secret");
    assert!(write_credential_headers(&route, PAIR).is_err());
    assert!(!outside.join("missing").exists(), "refused before mkdir");
    assert!(!repo.join("new-secret").exists());
}

#[cfg(unix)]
#[test]
fn new_parents_are_private_and_shared_parent_is_not_changed() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("new/nested/headers");
    write_credential_headers(&path, PAIR).unwrap();
    for parent in [dir.path().join("new"), dir.path().join("new/nested")] {
        assert_eq!(std::fs::metadata(parent).unwrap().permissions().mode() & 0o777, 0o700);
    }
    let shared = dir.path().join("shared");
    std::fs::create_dir(&shared).unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(write_credential_headers(&shared.join("headers"), PAIR).is_err());
    assert!(!shared.join("headers").exists());
    assert_eq!(std::fs::metadata(&shared).unwrap().permissions().mode() & 0o777, 0o755);
}
