//! Forge egress surface scan (Issue #9988, C5 of #9983).
//!
//! Once the managed `gh` shim is first on `PATH`, plain `gh ...` is routed by
//! construction. What is NOT covered is anything that overrides where `gh`
//! goes or bypasses it: `GH_HOST=` / `GH_CONFIG_DIR=` assignments,
//! `--hostname`, `api_host`, `env -i ... gh`, absolute-path `gh` binaries,
//! `curl|wget ... api.github.com`, and `api.github.com` literals. An LLM copies
//! whatever the prompts/scripts show it, so this scan keeps them out of
//! `defaults/**` and `.github/workflows/**`.
//!
//! - `defaults/scripts/tests/` is excluded: fixtures there legitimately carry
//!   error strings such as `lookup api.github.com: no such host`.
//! - The only allowed `api.github.com` lines are the two in
//!   `defaults/scripts/lib/github-app-token.sh`, each carrying
//!   [`EXCEPTION_MARKER`].
//! - Other hits need a reasoned entry in
//!   `tests/fixtures/forge-egress-surface-allowlist.txt`, which can only shrink.
//! - `hosted-build-gate.yml` must keep exactly one Octokit construction site.
//!
//! Why a Rust test and not a new `.sh`: see `gh_api_repo_flag.rs`
//! (shell-language policy, ADR-0018).

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

const EXCEPTION_MARKER: &str = "loom:egress-exception=github-app-bootstrap";
const EXCEPTION_FILE: &str = "defaults/scripts/lib/github-app-token.sh";
const TESTS_DIR_EXCLUDED: &str = "defaults/scripts/tests/";
const GATE_WORKFLOW: &str = ".github/workflows/hosted-build-gate.yml";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("loom-daemon has a parent dir")
        .to_path_buf()
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// True when `gh` appears as a standalone word in `s`.
fn has_gh_word(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut from = 0;
    while let Some(i) = s[from..].find("gh") {
        let start = from + i;
        let end = start + 2;
        let before_ok = start == 0 || !is_word_byte(bytes[start - 1]);
        let after_ok = end >= bytes.len() || !is_word_byte(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        from = end;
    }
    false
}

/// True when the line names an absolute-path `gh` binary (`/x/bin/gh`).
fn has_absolute_gh(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut from = 0;
    while let Some(i) = line[from..].find("/gh") {
        let start = from + i;
        let end = start + 3;
        let after_ok = end >= bytes.len() || !is_word_byte(bytes[end]);
        let dir_ok = line[..start].ends_with("/bin")
            || line[..start].ends_with("/sbin")
            || line[..start].ends_with("/gh");
        if after_ok && dir_ok {
            return true;
        }
        from = end;
    }
    false
}

/// Pattern classes a single line trips. `api.github.com` lines carrying the
/// bootstrap marker are reported separately by the caller, not here.
fn scan_line(line: &str) -> Vec<&'static str> {
    let mut hits = Vec::new();
    if line.contains("GH_HOST=") {
        hits.push("gh-host-assign");
    }
    if line.contains("GH_CONFIG_DIR=") {
        hits.push("gh-config-dir-assign");
    }
    if line.contains("--hostname") {
        hits.push("hostname-flag");
    }
    if line.contains("api_host") {
        hits.push("api-host");
    }
    if let Some(i) = line.find("env -i") {
        if has_gh_word(&line[i..]) {
            hits.push("env-i-gh");
        }
    }
    if has_absolute_gh(line) {
        hits.push("absolute-gh-binary");
    }
    if line.contains("api.github.com") {
        if line.contains("curl") || line.contains("wget") {
            hits.push("direct-http-api");
        }
        if line.contains("https://api.github.com/") || !line.contains("curl") {
            hits.push("api-github-literal");
        }
    }
    hits
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let p = entry.path();
        let ft = entry.file_type().expect("file type");
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            collect(&p, out);
        } else {
            out.push(p);
        }
    }
}

struct Scan {
    /// (rel path, class) pairs not covered by the in-source exception.
    hits: BTreeSet<(String, String)>,
    /// Lines carrying the bootstrap marker, as (rel path, line number).
    marked: Vec<(String, usize)>,
}

fn scan_tree(root: &Path) -> Scan {
    let mut files = Vec::new();
    collect(&root.join("defaults"), &mut files);
    collect(&root.join(".github/workflows"), &mut files);
    files.sort();
    let mut scan = Scan {
        hits: BTreeSet::new(),
        marked: Vec::new(),
    };
    for f in files {
        let rel = f
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if rel.starts_with(TESTS_DIR_EXCLUDED) {
            continue;
        }
        let Ok(text) = fs::read_to_string(&f) else {
            continue;
        }; // binary
        for (n, line) in text.lines().enumerate() {
            let classes = scan_line(line);
            if classes.is_empty() {
                continue;
            }
            if line.contains(EXCEPTION_MARKER) && classes.iter().all(|c| c.starts_with("api-")) {
                scan.marked.push((rel.clone(), n + 1));
                continue;
            }
            for c in classes {
                scan.hits.insert((rel.clone(), c.to_string()));
            }
        }
    }
    scan
}

fn read_allowlist(root: &Path) -> BTreeSet<(String, String)> {
    let path = root.join("loom-daemon/tests/fixtures/forge-egress-surface-allowlist.txt");
    let text = fs::read_to_string(&path).expect("allowlist fixture");
    let mut set = BTreeSet::new();
    for line in text.lines() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').collect();
        assert!(
            cols.len() == 3 && !cols[2].trim().is_empty(),
            "allowlist entry needs `path<TAB>class<TAB>reason`: {line:?}"
        );
        set.insert((cols[0].to_string(), cols[1].to_string()));
    }
    set
}

#[test]
fn defaults_and_workflows_have_no_unlisted_routing_overrides() {
    let root = repo_root();
    let scan = scan_tree(&root);
    let allow = read_allowlist(&root);

    let new: Vec<_> = scan.hits.difference(&allow).collect();
    assert!(
        new.is_empty(),
        "forge egress routing hazards found (fix them, or for the sole documented \
         bootstrap exception use the `{EXCEPTION_MARKER}` marker): {new:#?}"
    );
    let stale: Vec<_> = allow.difference(&scan.hits).collect();
    assert!(
        stale.is_empty(),
        "stale allowlist entries (the ratchet only shrinks - delete them): {stale:#?}"
    );
}

#[test]
fn api_github_com_only_on_the_two_marked_app_token_lines() {
    let scan = scan_tree(&repo_root());
    assert_eq!(scan.marked.len(), 2, "expected exactly two marked lines: {:?}", scan.marked);
    for (path, _) in &scan.marked {
        assert_eq!(
            path, EXCEPTION_FILE,
            "the bootstrap exception is only valid in {EXCEPTION_FILE}"
        );
    }
}

#[test]
fn hosted_build_gate_has_exactly_one_octokit_construction_site() {
    let text = fs::read_to_string(repo_root().join(GATE_WORKFLOW)).expect("workflow");
    let sites = text
        .lines()
        .filter(|l| l.contains("uses: actions/github-script@"))
        .count();
    assert_eq!(sites, 1, "{GATE_WORKFLOW} must keep exactly one actions/github-script step");
    assert!(!text.contains("pull_request"), "{GATE_WORKFLOW} must stay workflow_call-only");
}

// ── Negative fixtures: one per pattern class ───────────────────────────────

#[test]
fn negative_fixtures_each_pattern_class_is_detected() {
    let cases: &[(&str, &str)] = &[
        ("export GH_HOST=ghe.example.com", "gh-host-assign"),
        ("GH_CONFIG_DIR=/tmp/x gh pr list", "gh-config-dir-assign"),
        ("gh auth status --hostname example.com", "hostname-flag"),
        ("gh config set api_host x", "api-host"),
        ("env -i PATH=/usr/bin gh pr list", "env-i-gh"),
        ("/opt/homebrew/bin/gh pr list", "absolute-gh-binary"),
        ("curl -s https://api.github.com/repos/o/r", "direct-http-api"),
        ("wget -qO- https://api.github.com/user", "direct-http-api"),
        ("URL=\"https://api.github.com/repos\"", "api-github-literal"),
    ];
    for (line, class) in cases {
        assert!(
            scan_line(line).contains(class),
            "{line:?} should trip {class}: {:?}",
            scan_line(line)
        );
    }
}

#[test]
fn negative_fixtures_benign_lines_are_clean() {
    for line in [
        "gh pr list --json number",
        "env -u GH_TOKEN -u GH_CONFIG_DIR \"$@\"",
        "echo ghost; env -i PATH=x ghostwriter",
        "use the `gh` CLI, never /usr/bin/ghost",
    ] {
        assert!(scan_line(line).is_empty(), "{line:?} should be clean: {:?}", scan_line(line));
    }
}
