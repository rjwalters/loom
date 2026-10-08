#![allow(clippy::unwrap_used)]

use super::*;
use crate::install_compat_harness::{
    hard_floors, invoked_files_from_source, optional_subs, parse_detector_list,
    subcommand_unrecognized,
};
use std::path::PathBuf;

/// Today's `.loom/install-metadata.json` shape (this repo, 2026-10-07): no
/// `requires_daemon`, plus keys the contract does not read.
const TODAY: &str = r#"{
  "loom_version": "0.19.876",
  "loom_commit": "5956544463c35b9708afe5750451deb68f5e6628",
  "install_date": "2026-04-21",
  "installed_files": [
    ".claude/README.md",
    "CLAUDE.md"
  ],
  "last_resync": "2026-10-07",
  "loom_source_remote": "https://github.com/rjwalters/loom.git"
}"#;

fn v(s: &str) -> Version {
    Version::parse(s).unwrap()
}

fn meta(loom_version: Option<&str>, requires_daemon: Option<&str>) -> InstallMeta {
    InstallMeta {
        loom_version: loom_version.map(str::to_string),
        requires_daemon: requires_daemon.map(str::to_string),
    }
}

/// running 0.19.876, supports_installed 0.19.500, no floor.
fn daemon() -> DaemonCompat {
    DaemonCompat {
        running: v("0.19.876"),
        supports_installed: v("0.19.500"),
        floor: None,
    }
}

// ---- Version ----------------------------------------------------------------

#[test]
fn version_parses_strict_major_minor_patch() {
    assert_eq!(
        Version::parse("0.19.876"),
        Some(Version {
            major: 0,
            minor: 19,
            patch: 876
        })
    );
    assert_eq!(Version::parse(" v1.2.3\n"), Some(v("1.2.3")));
    for bad in [
        "",
        "unknown",
        "1.2",
        "1.2.3.4",
        "1.2.x",
        "1.2.3-rc1",
        "1..3",
        "-1.2.3",
    ] {
        assert_eq!(Version::parse(bad), None, "{bad:?}");
    }
}

#[test]
fn version_orders_numerically_not_lexically() {
    assert!(v("0.19.100") > v("0.19.99"));
    assert!(v("1.0.0") > v("0.99.999"));
    assert_eq!(v("0.19.876").next_patch(), v("0.19.877"));
    assert_eq!(v("0.19.876").to_string(), "0.19.876");
}

// ---- InstallMeta ------------------------------------------------------------

#[test]
fn todays_metadata_parses_and_owes_a_resync() {
    let m = InstallMeta::parse(TODAY).unwrap();
    assert_eq!(m, meta(Some("0.19.876"), None));
    assert_eq!(classify(&m, &DaemonCompat::this_binary(None).unwrap()), Compat::ResyncOwed);
}

#[test]
fn metadata_parses_new_fields_nulls_and_empty_object() {
    let m =
        InstallMeta::parse(r#"{"loom_version":"0.19.877","requires_daemon":"0.19.772"}"#).unwrap();
    assert_eq!(m, meta(Some("0.19.877"), Some("0.19.772")));
    let m = InstallMeta::parse(r#"{"loom_version":null,"requires_daemon":null}"#).unwrap();
    assert_eq!(m, InstallMeta::default());
    assert_eq!(InstallMeta::parse("{}").unwrap(), InstallMeta::default());
    assert!(InstallMeta::parse("[]").is_err());
    assert!(InstallMeta::parse(r#"{"requires_daemon": 5}"#).is_err());
}

// ---- classify: one test per variant, then the boundaries --------------------

#[test]
fn classify_compatible() {
    assert_eq!(
        classify(&meta(Some("0.19.870"), Some("0.19.772")), &daemon()),
        Compat::Compatible
    );
}

#[test]
fn classify_resync_owed() {
    // Absent requires_daemon (every repo before #10716).
    assert_eq!(classify(&meta(Some("0.19.870"), None), &daemon()), Compat::ResyncOwed);
    // Absent or unparseable loom_version.
    assert_eq!(classify(&meta(None, Some("0.19.772")), &daemon()), Compat::ResyncOwed);
    assert_eq!(
        classify(&meta(Some("unknown"), Some("0.19.772")), &daemon()),
        Compat::ResyncOwed
    );
    // Unparseable requires_daemon.
    assert_eq!(
        classify(&meta(Some("0.19.870"), Some("garbage")), &daemon()),
        Compat::ResyncOwed
    );
    assert_eq!(classify(&InstallMeta::default(), &daemon()), Compat::ResyncOwed);
}

#[test]
fn classify_installed_too_old() {
    assert_eq!(
        classify(&meta(Some("0.19.400"), Some("0.19.300")), &daemon()),
        Compat::InstalledTooOld
    );
    // Too old wins over the migration rule: no requires_daemon does not excuse it.
    assert_eq!(classify(&meta(Some("0.19.400"), None), &daemon()), Compat::InstalledTooOld);
}

#[test]
fn classify_needs_newer_daemon() {
    assert_eq!(
        classify(&meta(Some("0.19.900"), Some("0.19.890")), &daemon()),
        Compat::NeedsNewerDaemon
    );
    // Wins even without a usable loom_version, and over "too old".
    assert_eq!(classify(&meta(None, Some("0.19.877")), &daemon()), Compat::NeedsNewerDaemon);
    assert_eq!(
        classify(&meta(Some("0.19.1"), Some("0.20.0")), &daemon()),
        Compat::NeedsNewerDaemon
    );
}

#[test]
fn classify_boundary_loom_version_equal_to_supports_installed_is_compatible() {
    assert_eq!(
        classify(&meta(Some("0.19.500"), Some("0.19.400")), &daemon()),
        Compat::Compatible
    );
    assert_eq!(
        classify(&meta(Some("0.19.499"), Some("0.19.400")), &daemon()),
        Compat::InstalledTooOld
    );
}

#[test]
fn classify_boundary_requires_daemon_equal_to_running_is_compatible() {
    assert_eq!(
        classify(&meta(Some("0.19.876"), Some("0.19.876")), &daemon()),
        Compat::Compatible
    );
    assert_eq!(
        classify(&meta(Some("0.19.877"), Some("0.19.877")), &daemon()),
        Compat::NeedsNewerDaemon
    );
}

#[test]
fn classify_boundary_floor_applies_to_installed_versions() {
    let floored = DaemonCompat {
        floor: Some(v("0.19.800")),
        ..daemon()
    };
    assert_eq!(
        classify(&meta(Some("0.19.800"), Some("0.19.772")), &floored),
        Compat::Compatible
    );
    assert_eq!(
        classify(&meta(Some("0.19.799"), Some("0.19.772")), &floored),
        Compat::InstalledTooOld
    );
    // A floor below supports_installed does not loosen it.
    let low = DaemonCompat {
        floor: Some(v("0.1.0")),
        ..daemon()
    };
    assert_eq!(
        classify(&meta(Some("0.19.499"), Some("0.19.400")), &low),
        Compat::InstalledTooOld
    );
}

#[test]
fn classify_identical_versions_everywhere_is_compatible() {
    let same = DaemonCompat {
        running: v("0.19.876"),
        supports_installed: v("0.19.876"),
        floor: Some(v("0.19.876")),
    };
    assert_eq!(classify(&meta(Some("0.19.876"), Some("0.19.876")), &same), Compat::Compatible);
}

// ---- this binary's claims ---------------------------------------------------

#[test]
fn this_binarys_claims_are_versions_and_not_from_the_future() {
    let d = DaemonCompat::this_binary(None).unwrap();
    assert!(d.supports_installed <= d.running.next_patch());
    assert!(v(REQUIRES_DAEMON) <= d.running.next_patch());
}

#[test]
fn requires_daemon_line_keeps_the_shape_the_installer_reads() {
    // scripts/install/loom-source-path.sh reads this line with sed.
    let line = format!("\npub const REQUIRES_DAEMON: &str = \"{REQUIRES_DAEMON}\";\n");
    assert!(include_str!("../install_compat.rs").contains(&line));
}

#[test]
fn invoked_file_markers_parse_back_to_the_constant() {
    let parsed = invoked_files_from_source(include_str!("../install_compat.rs")).unwrap();
    assert_eq!(parsed, DAEMON_INVOKED_INSTALLED_FILES);
    assert_eq!(invoked_files_from_source("pub const X: u8 = 1;"), None);
}

#[test]
fn every_invoked_file_ships_in_defaults() {
    let defaults = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../defaults");
    for f in DAEMON_INVOKED_INSTALLED_FILES {
        let rel = f.strip_prefix(".loom/").unwrap();
        assert!(defaults.join(rel).is_file(), "{f} is not shipped as defaults/{rel}");
    }
}

// ---- harness parsing --------------------------------------------------------

#[test]
fn markers_split_into_hard_floors_and_optional() {
    let text = "\
# requires-daemon: forge >= 0.19.707   note
    # requires-daemon: merge-pr >= 0.19.763
# requires-daemon: release-explain optional  degrades
# requires-daemon: <subcommand> >= <version>   template line
echo '# requires-daemon: not-a-marker >= 9.9.9'
";
    assert_eq!(
        hard_floors(text),
        vec![
            ("forge".to_string(), v("0.19.707")),
            ("merge-pr".to_string(), v("0.19.763"))
        ]
    );
    assert_eq!(optional_subs(text).into_iter().collect::<Vec<_>>(), vec!["release-explain"]);
}

#[test]
fn detector_list_lines_parse_to_file_and_subcommand() {
    let out = "/tmp/x/defaults/scripts/a.sh:12\tforge\ndefaults/hooks/b.sh:3\tsecret-scan\njunk\n";
    assert_eq!(
        parse_detector_list(out),
        vec![
            (PathBuf::from("/tmp/x/defaults/scripts/a.sh"), "forge".to_string()),
            (PathBuf::from("defaults/hooks/b.sh"), "secret-scan".to_string()),
        ]
    );
}

#[test]
fn only_claps_refusal_counts_as_a_missing_subcommand() {
    assert!(subcommand_unrecognized(
        "error: unrecognized subcommand 'gh-shim'\n\nUsage: loom-daemon [COMMAND]\n"
    ));
    // A pre-clap subcommand's own usage, exit 2, is present.
    assert!(!subcommand_unrecognized("usage: loom-daemon gh-shim path|session-env|status\n"));
    assert!(!subcommand_unrecognized(""));
}

// ---- reader -----------------------------------------------------------------

fn git(dir: &Path, args: &[&str]) {
    let st = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .unwrap();
    assert!(st.success(), "git {args:?}");
}

fn repo_with_origin_main(metadata: Option<&str>) -> tempfile::TempDir {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("README"), "x\n").unwrap();
    if let Some(m) = metadata {
        std::fs::create_dir(dir.join(".loom")).unwrap();
        std::fs::write(dir.join(INSTALL_METADATA_PATH), m).unwrap();
    }
    git(dir, &["add", "-A"]);
    git(
        dir,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "init",
        ],
    );
    git(dir, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    tmp
}

#[test]
fn reader_reads_the_default_branch_not_the_working_tree() {
    let repo = repo_with_origin_main(Some(TODAY));
    // A local edit the default branch does not carry is invisible to the reader.
    std::fs::write(repo.path().join(INSTALL_METADATA_PATH), r#"{"loom_version":"9.9.9"}"#).unwrap();
    let m = read_default_branch(repo.path()).unwrap().unwrap();
    assert_eq!(m, meta(Some("0.19.876"), None));
}

#[test]
fn reader_prefers_origin_head() {
    let repo = repo_with_origin_main(Some(TODAY));
    let dir = repo.path();
    std::fs::write(
        dir.join(INSTALL_METADATA_PATH),
        r#"{"loom_version":"0.19.900","requires_daemon":"0.19.880"}"#,
    )
    .unwrap();
    git(
        dir,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qam",
            "trunk",
        ],
    );
    git(dir, &["update-ref", "refs/remotes/origin/trunk", "HEAD"]);
    git(
        dir,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/trunk",
        ],
    );
    assert_eq!(
        read_default_branch(dir).unwrap().unwrap(),
        meta(Some("0.19.900"), Some("0.19.880"))
    );
}

#[test]
fn reader_returns_none_without_metadata_and_errors_without_a_branch() {
    let repo = repo_with_origin_main(None);
    assert_eq!(read_default_branch(repo.path()).unwrap(), None);
    git(repo.path(), &["update-ref", "-d", "refs/remotes/origin/main"]);
    assert!(read_default_branch(repo.path()).is_err());
    assert!(read_at_ref(repo.path(), "no-such-ref").is_err());
}

#[test]
fn reader_rejects_malformed_metadata() {
    let repo = repo_with_origin_main(Some("{not json"));
    assert!(read_default_branch(repo.path()).is_err());
}
