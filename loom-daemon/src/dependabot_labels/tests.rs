use super::*;

const RENDERED: &str = r#"# header
version: 2
updates:
  - package-ecosystem: "github-actions"
    directories:
      - "/"
    labels: ["dependencies", "loom:review-requested"]
  - package-ecosystem: "npm"
    directories:
      - "/a"
      - "/b"
    schedule:
      interval: "weekly"
    labels: ["dependencies", "loom:review-requested"]
    groups:
      npm-minor-patch:
        patterns: ["*"]
"#;

const BLOCK: &str = r#"version: 2
updates:
  - package-ecosystem: "npm"
    directory: "/"
    labels:
      - "dependencies"
      - "loom:review-requested"
    groups:
      all:
        patterns:
          - "*"
"#;

fn repo(config: &str, files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".github")).unwrap();
    std::fs::write(dir.path().join(CONFIG), config).unwrap();
    for (path, body) in files {
        let p = dir.path().join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }
    dir
}

fn run(dir: &tempfile::TempDir, manifests: &[&str]) -> Report {
    let m: Vec<String> = manifests.iter().map(|s| s.to_string()).collect();
    check_with_manifests(dir.path(), &m)
}

const DEPS: &str = r#"{"dependencies":{"left-pad":"1.0.0"}}"#;

#[test]
fn parses_block_and_flow_shapes_one_row_per_directory() {
    let e = parse_entries(RENDERED);
    let dirs: Vec<_> = e
        .iter()
        .map(|e| (e.ecosystem.as_str(), e.directory.as_str()))
        .collect();
    assert_eq!(dirs, vec![("github-actions", "/"), ("npm", "/a"), ("npm", "/b")]);
    assert!(e
        .iter()
        .all(|e| e.labels == vec!["dependencies", REQUIRED_LABEL]));

    let b = parse_entries(BLOCK);
    assert_eq!(b.len(), 1);
    assert_eq!(b[0].directory, "/");
    assert_eq!(b[0].labels, vec!["dependencies", REQUIRED_LABEL]);

    let inline =
        parse_entries("updates:\n  - package-ecosystem: npm\n    directories: ['/x', \"/y\"]\n");
    assert_eq!(
        inline
            .iter()
            .map(|e| e.directory.as_str())
            .collect::<Vec<_>>(),
        vec!["/x", "/y"]
    );
}

#[test]
fn rendered_shape_passes_when_covered() {
    let d = repo(
        RENDERED,
        &[
            ("a/package.json", DEPS),
            ("a/package-lock.json", "{}"),
            ("b/package.json", DEPS),
            ("b/pnpm-lock.yaml", ""),
        ],
    );
    let r = run(&d, &["a/package.json", "b/package.json"]);
    assert!(r.errors.is_empty(), "{:?}", r.errors);
}

#[test]
fn missing_label_fails_per_directory() {
    let cfg = RENDERED.replace(
        r#"labels: ["dependencies", "loom:review-requested"]
    groups"#,
        "labels: [\"dependencies\"]\n    groups",
    );
    let d = repo(&cfg, &[("a/package.json", DEPS), ("b/package.json", DEPS)]);
    let r = run(&d, &[]);
    assert_eq!(r.errors.len(), 2, "{:?}", r.errors);
    assert!(r.errors[0].contains("'npm' (directory '/a')"));
    assert!(r.errors[1].contains("'npm' (directory '/b')"));
}

#[test]
fn entry_without_directory_fails() {
    let d = repo(
        "updates:\n  - package-ecosystem: \"cargo\"\n    labels: [\"loom:review-requested\"]\n",
        &[],
    );
    assert!(run(&d, &[]).errors[0].contains("has no 'directory:'"));
}

#[test]
fn zero_entries_fails_loudly() {
    let d = repo("version: 2\nupdates: []\n", &[]);
    assert!(run(&d, &[]).errors[0].contains("parsed zero entries"));
}

#[test]
fn no_config_is_a_no_op() {
    let d = tempfile::tempdir().unwrap();
    let r = check_with_manifests(d.path(), &["package.json".into()]);
    assert!(!r.config_present && r.errors.is_empty());
}

#[test]
fn uncovered_lockfile_manifest_fails() {
    let d = repo(
        BLOCK,
        &[
            ("package.json", "{}"),
            ("mcp-loom/package.json", DEPS),
            ("mcp-loom/package-lock.json", "{}"),
        ],
    );
    let r = run(&d, &["package.json", "mcp-loom/package.json"]);
    assert_eq!(r.errors.len(), 1, "{:?}", r.errors);
    assert!(r.errors[0].contains("mcp-loom/package.json"));
}

#[test]
fn stale_npm_entry_fails() {
    let d = repo(&BLOCK.replace("directory: \"/\"", "directory: \"/moved-away\""), &[]);
    assert!(run(&d, &[]).errors[0].contains("\"/moved-away\""));
}

#[test]
fn lockfile_less_scaffold_is_a_note() {
    let d = repo(
        BLOCK,
        &[
            ("package.json", "{}"),
            ("quickstarts/api/package.json", DEPS),
        ],
    );
    let r = run(&d, &["package.json", "quickstarts/api/package.json"]);
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert!(r.notes[0].starts_with("note: quickstarts/api/package.json"));
}

fn no_npm_entry() -> String {
    RENDERED
        .split("  - package-ecosystem: \"npm\"")
        .next()
        .unwrap()
        .to_string()
}

#[test]
fn dependency_free_non_workspace_root_is_a_note() {
    let d = repo(
        &no_npm_entry(),
        &[
            ("package.json", r#"{"name":"x","scripts":{}}"#),
            ("pnpm-lock.yaml", ""),
        ],
    );
    let r = run(&d, &["package.json"]);
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert!(
        r.notes[0].starts_with("note: package.json has a lockfile but declares no dependencies")
    );
}

/// Judge repro on #11828: a pnpm workspace root declares nothing itself but
/// its lockfile carries the members' dependencies.
#[test]
fn pnpm_workspace_root_is_not_exempt() {
    let d = repo(
        &no_npm_entry(),
        &[
            ("package.json", r#"{"private":true}"#),
            ("pnpm-lock.yaml", ""),
            ("pnpm-workspace.yaml", "packages:\n  - 'pkgs/*'\n"),
            ("pkgs/m/package.json", DEPS),
        ],
    );
    let r = run(&d, &["package.json", "pkgs/m/package.json"]);
    assert_eq!(r.errors.len(), 1, "{:?}", r.errors);
    assert!(r.errors[0].starts_with("ERROR: package.json has a committed lockfile"));
}

/// Judge repro on #11828: an npm workspace root (`workspaces` key).
#[test]
fn npm_workspace_root_is_not_exempt() {
    let d = repo(
        &no_npm_entry(),
        &[
            ("package.json", r#"{"private":true,"workspaces":["pkgs/*"]}"#),
            ("package-lock.json", "{}"),
            ("pkgs/m/package.json", DEPS),
        ],
    );
    let r = run(&d, &["package.json", "pkgs/m/package.json"]);
    assert_eq!(r.errors.len(), 1, "{:?}", r.errors);
    assert!(r.errors[0].starts_with("ERROR: package.json has a committed lockfile"));
}

#[test]
fn unreadable_manifest_with_lockfile_is_not_exempt() {
    let d = repo(&no_npm_entry(), &[("package.json", "{ not json"), ("package-lock.json", "{}")]);
    assert_eq!(run(&d, &["package.json"]).errors.len(), 1);
}

/// The regression guard: this repository's own rendered dependabot.yml passes.
#[test]
fn this_repositorys_dependabot_config_passes() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let r = check(&root);
    assert!(r.config_present);
    assert!(r.errors.is_empty(), "{:?}", r.errors);
}
