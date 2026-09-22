#![allow(clippy::unwrap_used)]
use super::*;

fn workspace(parent: &Path, name: &str) -> PathBuf {
    let root = parent.join(name);
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join(".git"), "fixture git marker").unwrap();
    root
}

#[test]
fn defaults_and_external_container_base_are_private_and_isolated() {
    let temp = tempfile::tempdir().unwrap();
    let root = workspace(temp.path(), "repo");
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let state = create(&root, None, Some(&home), None).unwrap();
    assert!(state.directory.starts_with(
        home.canonicalize()
            .unwrap()
            .join(".local/state/loom/native-tools")
    ));
    let container_base = temp.path().join("container-runtime/native-tools");
    let one = create(&root, Some(&container_base), None, None).unwrap();
    let two = create(&root, Some(&container_base), None, None).unwrap();
    assert_ne!(one.directory, two.directory);
    assert_eq!(one.directory.parent(), two.directory.parent());
    assert!(!root.join(".loom/native-tools").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for dir in [
            &state.directory,
            &one.directory,
            one.directory.parent().unwrap(),
        ] {
            assert_eq!(fs::metadata(dir).unwrap().permissions().mode() & 0o777, 0o700);
        }
    }
}

#[test]
fn concurrent_launches_and_workspaces_never_share_mutable_state() {
    let temp = tempfile::tempdir().unwrap();
    let a = workspace(temp.path(), "one");
    let b = workspace(temp.path(), "two");
    let base = temp.path().join("external");
    let threads: Vec<_> = (0..12)
        .map(|index| {
            let root = if index % 2 == 0 { a.clone() } else { b.clone() };
            let base = base.clone();
            std::thread::spawn(move || create(&root, Some(&base), None, None).unwrap().directory)
        })
        .collect();
    let paths: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(
        paths
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        12
    );
    assert_eq!(
        paths
            .iter()
            .map(|p| p.parent().unwrap())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        2
    );
}

#[test]
fn checkout_overrides_other_repositories_and_home_checkout_fail_before_creation() {
    let temp = tempfile::tempdir().unwrap();
    let root = workspace(temp.path(), "repo");
    let other = workspace(temp.path(), "other");
    for destination in [root.join("ignored/auth"), other.join("ignored/auth")] {
        assert!(create(&root, Some(&destination), None, None).is_err());
        assert!(!destination.exists());
    }
    assert!(create(&root, None, Some(&root), None).is_err());
    assert!(!root.join(".local").exists());
    assert!(create(&root, Some(Path::new("relative-state")), None, None).is_err());
    assert!(create(&root, Some(&temp.path().join("safe/../unsafe")), None, None).is_err());
}

#[cfg(unix)]
#[test]
fn symlink_aliases_into_checkouts_are_rejected_but_external_aliases_work() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let root = workspace(temp.path(), "repo");
    let alias = temp.path().join("alias");
    symlink(&root, &alias).unwrap();
    assert!(create(&root, Some(&alias.join("ignored")), None, None).is_err());
    assert!(!root.join("ignored").exists());
    let external = temp.path().join("external");
    fs::create_dir(&external).unwrap();
    let inside_alias = root.join("secret-alias");
    symlink(&external, &inside_alias).unwrap();
    assert!(create(&root, Some(&inside_alias), None, None).is_err());
    assert!(validate_override(&root.canonicalize().unwrap(), &inside_alias).is_err());
    let good = temp.path().join("external-alias");
    symlink(&external, &good).unwrap();
    assert!(create(&root, Some(&good), None, None)
        .unwrap()
        .directory
        .starts_with(external.canonicalize().unwrap()));
}

#[test]
fn harness_auth_and_session_paths_are_private_and_auth_snapshot_never_changes_source() {
    let temp = tempfile::tempdir().unwrap();
    let root = workspace(temp.path(), "repo");
    let private = temp.path().join("credentials");
    private_directory(&private).unwrap();
    let source = private.join("external-auth.json");
    fs::write(&source, "{}").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&source, fs::Permissions::from_mode(0o600)).unwrap();
    }
    for runtime in ["pi", "opencode"] {
        let bytes = serde_json::to_vec(&serde_json::json!({"fixture": {
            "type": if runtime == "pi" { "api_key" } else { "api" },
            "key": "fixture-only-not-a-real-key"
        }}))
        .unwrap();
        fs::write(&source, &bytes).unwrap();
        let state = create(&root, Some(&temp.path().join("state")), None, Some(&source)).unwrap();
        let mut command = Command::new("fixture-harness");
        state.configure(&mut command, runtime).unwrap();
        for (name, value) in command.get_envs() {
            if name.to_string_lossy().ends_with("DIR") || name.to_string_lossy().starts_with("XDG_")
            {
                assert!(Path::new(value.unwrap()).starts_with(&state.directory));
            }
        }
        let auth = state.directory.join(if runtime == "pi" {
            "pi-agent/auth.json"
        } else {
            "data/opencode/auth.json"
        });
        assert_eq!(fs::read(&auth).unwrap(), bytes);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&auth).unwrap().permissions().mode() & 0o777, 0o600);
        }
        fs::write(auth, "fixture-token-refresh").unwrap();
        assert_eq!(fs::read(&source).unwrap(), bytes);
    }
}

#[test]
fn unsafe_or_malformed_auth_source_has_no_provisioning_side_effects_or_content_in_error() {
    let temp = tempfile::tempdir().unwrap();
    let root = workspace(temp.path(), "repo");
    let base = temp.path().join("external-state");
    let source = root.join("auth.json");
    fs::write(&source, "fixture-secret-sentinel").unwrap();
    let error = create(&root, Some(&base), None, Some(&source))
        .err()
        .unwrap()
        .to_string();
    assert!(!base.exists());
    assert!(!error.contains("fixture-secret-sentinel"));
    let private = temp.path().join("credentials");
    private_directory(&private).unwrap();
    let external = private.join("bad-auth.json");
    fs::write(&external, "fixture-secret-sentinel").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&external, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let error = create(&root, Some(&base), None, Some(&external))
        .err()
        .unwrap()
        .to_string();
    assert!(!base.exists());
    assert!(!error.contains("fixture-secret-sentinel"));
}

#[cfg(unix)]
#[test]
fn non_private_auth_and_preexisting_workspace_state_fail_closed() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let root = workspace(temp.path(), "repo");
    let base = temp.path().join("state");
    let private = temp.path().join("credentials");
    private_directory(&private).unwrap();
    let source = private.join("auth.json");
    fs::write(&source, "{}").unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(create(&root, Some(&base), None, Some(&source)).is_err());
    assert!(!base.exists());
    let state = create(&root, Some(&base), None, None).unwrap();
    fs::set_permissions(state.directory.parent().unwrap(), fs::Permissions::from_mode(0o755))
        .unwrap();
    assert!(create(&root, Some(&base), None, None).is_err());
}

#[test]
fn ambient_directory_overrides_cannot_redirect_state_into_repositories() {
    let temp = tempfile::tempdir().unwrap();
    let root = workspace(temp.path(), "repo").canonicalize().unwrap();
    let other = workspace(temp.path(), "other");
    for destination in [root.join("auth"), other.join("auth")] {
        assert!(validate_override(&root, &destination).is_err());
        assert!(!destination.exists());
    }
    assert!(validate_override(&root, &temp.path().join("outside")).is_ok());
    assert!(validate_override(&root, Path::new("relative")).is_err());
}

#[test]
fn auth_format_is_harness_specific_and_diagnostics_never_contain_secrets() {
    let pi = br#"{"provider":{"type":"api_key","key":"fixture-secret"}}"#;
    let oc = br#"{"provider":{"type":"api","key":"fixture-secret"}}"#;
    assert!(validate_auth_format(pi, "pi").is_ok());
    assert!(validate_auth_format(oc, "opencode").is_ok());
    for (bytes, runtime) in [(pi.as_slice(), "opencode"), (oc.as_slice(), "pi")] {
        let error = validate_auth_format(bytes, runtime)
            .unwrap_err()
            .to_string();
        assert!(!error.contains("fixture-secret"));
    }
    let oauth = br#"{"provider":{"type":"oauth","refresh":"fixture-refresh","access":"fixture-access","expires":100}}"#;
    for runtime in ["pi", "opencode"] {
        assert!(validate_auth_format(oauth, runtime).is_ok());
        assert!(validate_auth_format(br#"{"provider":{"type":"oauth"}}"#, runtime).is_err());
    }
}
