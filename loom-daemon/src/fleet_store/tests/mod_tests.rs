use serde_json::json;

use super::*;

fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
    move |k| {
        pairs
            .iter()
            .find(|(n, _)| *n == k)
            .map(|(_, v)| (*v).to_string())
    }
}

#[test]
fn unset_means_off() {
    assert_eq!(resolve_location(&json!({}), &env_of(&[])).unwrap(), None);
    assert_eq!(resolve_location(&json!({"fleet": {"repo": "  "}}), &env_of(&[])).unwrap(), None);
}

#[test]
fn config_key_with_default_ref() {
    let loc = resolve_location(&json!({"fleet": {"repo": "acme/.fleet"}}), &env_of(&[]))
        .unwrap()
        .unwrap();
    assert_eq!(loc.repo, "acme/.fleet");
    assert_eq!(loc.reference, "main");
    assert_eq!(loc.owner(), "acme");
}

#[test]
fn env_overrides_config() {
    let cfg = json!({"fleet": {"repo": "acme/a", "ref": "stable"}});
    let loc = resolve_location(
        &cfg,
        &env_of(&[("LOOM_FLEET_REPO", "other/b"), ("LOOM_FLEET_REF", "v2")]),
    )
    .unwrap()
    .unwrap();
    assert_eq!((loc.repo.as_str(), loc.reference.as_str()), ("other/b", "v2"));
    let loc = resolve_location(&cfg, &env_of(&[])).unwrap().unwrap();
    assert_eq!(loc.reference, "stable");
}

#[test]
fn malformed_locations_are_refused() {
    for repo in ["acme", "acme/", "/x", "a/b/c", "acme/..", "acme/x y"] {
        let cfg = json!({"fleet": {"repo": repo}});
        assert!(resolve_location(&cfg, &env_of(&[])).is_err(), "{repo}");
    }
    let cfg = json!({"fleet": {"repo": "acme/x", "ref": "../../etc"}});
    assert!(resolve_location(&cfg, &env_of(&[])).is_err());
}

#[test]
fn contract_paths() {
    for p in [
        "repos.yml",
        "fleet/state.yml",
        "fleet/defaults.json",
        "fleet/hosts/h1/defaults.json",
        "fleet/hosts/h1/local.json",
    ] {
        assert!(is_contract_path(p), "{p}");
    }
    for p in [
        "hosts.yml",
        "README.md",
        "fleet/hosts/h1/notes.json",
        "fleet/hosts//local.json",
        "fleet/hosts/a/b/local.json",
    ] {
        assert!(!is_contract_path(p), "{p}");
    }
}

#[test]
fn host_ids_must_be_plain_names() {
    assert!(validate_host("worker-1").is_ok());
    for bad in ["", "..", "../x", "a/b", ".hidden"] {
        assert!(validate_host(bad).is_err(), "{bad}");
    }
}
