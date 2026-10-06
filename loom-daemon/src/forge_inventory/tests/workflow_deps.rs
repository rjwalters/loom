//! Tests for the workflow delivery-dependency audit (Issue #9790).

#![allow(clippy::unwrap_used)]

use std::path::Path;

use super::{audit, is_workflow_file, scan, Plane};

const SAMPLE: &str = r#"
name: sample
on: [push]
jobs:
  build:
    runs-on: ubuntu-latest
    container: ghcr.io/acme/builder:1.2
    steps:
      # uses: commented/out@v1 is prose, not a dependency
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
      - uses: dtolnay/rust-toolchain@stable
      - uses: ./.github/actions/local
      - uses: docker://ghcr.io/acme/tool:2
      - uses: https://gitea.example.test/mirror/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1
      - name: tool
        run: |
          curl -fsSL "https://github.com/koalaman/shellcheck/releases/download/v0.10.0/sc.tar.xz" -o sc.tar.xz
          curl -fsSL https://raw.githubusercontent.com/acme/x/main/install.sh
          curl -fsSL https://api.github.com/repos/acme/x/releases/latest
          docker push ghcr.io/acme/out:${{ github.sha }}
          gh release upload v1 dist/*
          echo "see https://docs.example.test/page"
"#;

fn planes(text: &str) -> Vec<(Plane, String)> {
    scan("w.yml", text)
        .into_iter()
        .map(|d| (d.plane, d.reference))
        .collect()
}

#[test]
fn each_reference_lands_on_its_own_plane() {
    let got = planes(SAMPLE);
    let has = |p: Plane, r: &str| got.iter().any(|(gp, gr)| *gp == p && gr == r);

    assert!(has(Plane::PackageRegistry, "ghcr.io/acme/builder:1.2"), "{got:#?}");
    assert!(has(
        Plane::ActionSource,
        "actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1"
    ));
    assert!(has(Plane::ActionSource, "dtolnay/rust-toolchain@stable"));
    assert!(has(Plane::NoGithubFetch, "./.github/actions/local"));
    assert!(has(Plane::PackageRegistry, "docker://ghcr.io/acme/tool:2"));
    assert!(has(
        Plane::NoGithubFetch,
        "https://gitea.example.test/mirror/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1"
    ));
    assert!(has(
        Plane::ReleaseDownload,
        "https://github.com/koalaman/shellcheck/releases/download/v0.10.0/sc.tar.xz"
    ));
    assert!(has(
        Plane::ReleaseDownload,
        "https://raw.githubusercontent.com/acme/x/main/install.sh"
    ));
    assert!(has(Plane::ForgeApi, "https://api.github.com/repos/acme/x/releases/latest"));
    // `${{ … }}` ends the reference rather than swallowing the expression.
    assert!(has(Plane::PackageRegistry, "ghcr.io/acme/out"));
    assert!(has(Plane::ForgeApi, "gh release upload v1 dist/*"));

    // A comment line is never a dependency, and a non-GitHub URL is not tracked.
    assert!(!got.iter().any(|(_, r)| r.contains("commented/out")));
    assert!(!got.iter().any(|(_, r)| r.contains("docs.example.test")));
}

#[test]
fn sha_pinning_is_recorded_for_uses_references_only() {
    let deps = scan("w.yml", SAMPLE);
    let pin = |r: &str| deps.iter().find(|d| d.reference == r).unwrap().sha_pinned;
    assert_eq!(pin("actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1"), Some(true));
    assert_eq!(pin("dtolnay/rust-toolchain@stable"), Some(false));
    assert_eq!(pin("ghcr.io/acme/builder:1.2"), None);
}

#[test]
fn the_audit_never_claims_network_independence() {
    let a = audit(&[("w.yml".into(), SAMPLE.into())]);
    assert!(a.static_only);
    assert_eq!(
        a.distinct_actions,
        vec![
            "actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1".to_string(),
            "dtolnay/rust-toolchain@stable".to_string(),
        ]
    );
    let action = &a.planes["action-source"];
    assert_eq!(action.references, 2);
    assert!(action.integration_estimate.contains("DEFAULT_ACTIONS_URL"));
    // An empty scan is an empty audit, not an all-clear with invented rows.
    let empty = audit(&[]);
    assert!(empty.planes.is_empty() && empty.static_only);
}

#[test]
fn workflow_file_selection_covers_github_and_gitea_dirs_only() {
    assert!(is_workflow_file(".github/workflows/ci.yml"));
    assert!(is_workflow_file(".gitea/workflows/qual.yaml"));
    assert!(!is_workflow_file(".github/labels.yml"));
    assert!(!is_workflow_file(
        "defaults/forge/qualification/ci-fixture/.gitea/workflows/qual-ci.yml"
    ));
}

/// Over the real tree: the production workflows DO fetch from GitHub on every
/// plane #9790 names. If this ever reads empty the scan broke, not the tree —
/// the same guard ci-daily's pin check carries.
#[test]
fn the_production_workflows_report_every_github_delivery_plane() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let dir = root.join(".github/workflows");
    let mut files = Vec::new();
    for e in std::fs::read_dir(&dir).unwrap() {
        let p = e.unwrap().path();
        let name = format!(".github/workflows/{}", p.file_name().unwrap().to_string_lossy());
        if is_workflow_file(&name) {
            files.push((name, std::fs::read_to_string(&p).unwrap()));
        }
    }
    let a = audit(&files);
    for plane in [
        "action-source",
        "package-registry",
        "release-download",
        "forge-api",
    ] {
        assert!(
            a.planes.get(plane).is_some_and(|p| p.references > 0),
            "expected the production workflows to reference plane {plane}: {:#?}",
            a.planes
        );
    }
    assert!(a
        .distinct_actions
        .iter()
        .any(|x| x.starts_with("actions/checkout@")));
}
