//! Snapshot-or-list choice (Issue #12307).

use super::{snapshot_rows, Rows};

/// #12307: in mirror mode the rows come from the tick's snapshot; the listing
/// is not made, and a repo the snapshot has not read adds no rows.
#[test]
#[serial_test::serial]
fn mirror_mode_reads_the_tick_snapshot() {
    use crate::discovery::published::test_support::{repo, set, MirrorRoot};

    let fixture = MirrorRoot::new("acme/widgets");
    let mut snapshot = repo("acme/widgets");
    snapshot.rows = Vec::new();
    set(vec![snapshot]);
    assert!(matches!(snapshot_rows(fixture.path(), "acme/widgets", None), Rows::Snapshot(_)));

    set(Vec::new());
    assert!(matches!(snapshot_rows(fixture.path(), "acme/widgets", None), Rows::Skip));
    assert_eq!(fixture.gh_calls(), "");

    let standalone = tempfile::tempdir().unwrap();
    assert!(matches!(snapshot_rows(standalone.path(), "acme/widgets", None), Rows::Skip));
}

fn git(root: &std::path::Path, args: &[&str]) {
    assert!(std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .status()
        .unwrap()
        .success());
}

fn authority(host: &str, configured: &str, canonical: &str) -> crate::forge_repo_facts::Fact {
    let (owner, name) = canonical.split_once('/').unwrap();
    crate::forge_repo_facts::Fact {
        host: host.to_owned(),
        configured_nwo: configured.to_owned(),
        owner: owner.to_owned(),
        name: name.to_owned(),
        repo_id: Some(42),
        verified_at: 0,
        fresh: true,
    }
}

#[test]
#[serial_test::serial]
fn retargeted_roots_cannot_supply_rows_for_the_old_stage_identity() {
    use crate::discovery::published::test_support::{repo, set, MirrorRoot};
    crate::forge_repo_facts::set_test_enabled(true);

    let mirror = MirrorRoot::new("acme/old-mirror");
    let mirror_fact = authority("github.com", "acme/old-mirror", "acme/canonical-mirror");
    set(vec![
        repo("acme/old-mirror"),
        repo("acme/canonical-mirror"),
        repo("other/new-mirror"),
    ]);
    assert!(matches!(
        snapshot_rows(mirror.path(), "acme/canonical-mirror", Some(&mirror_fact)),
        Rows::Skip
    ));
    git(
        mirror.path(),
        &[
            "remote",
            "set-url",
            "origin",
            "https://github.com/acme/canonical-mirror",
        ],
    );
    assert!(matches!(
        snapshot_rows(mirror.path(), "acme/canonical-mirror", Some(&mirror_fact)),
        Rows::Snapshot(_)
    ));
    git(
        mirror.path(),
        &[
            "remote",
            "set-url",
            "origin",
            "https://github.com/other/new-mirror",
        ],
    );
    assert!(matches!(
        snapshot_rows(mirror.path(), "acme/canonical-mirror", Some(&mirror_fact)),
        Rows::Skip
    ));
    assert!(matches!(
        snapshot_rows(mirror.path(), "other/new-mirror", None),
        Rows::Snapshot(_)
    ));
    assert_eq!(mirror.gh_calls(), "");
    set(Vec::new());

    let direct = tempfile::tempdir().unwrap();
    git(direct.path(), &["init", "-q"]);
    git(
        direct.path(),
        &[
            "remote",
            "add",
            "origin",
            "https://forge-one.invalid/acme/old-direct",
        ],
    );
    let original = authority("forge-one.invalid", "acme/old-direct", "acme/new-direct");
    let Rows::List(legacy) = snapshot_rows(direct.path(), "acme/old-direct", None) else {
        panic!("legacy exact slug should keep the resolved nondefault host");
    };
    assert_eq!(legacy.host.as_deref(), Some("forge-one.invalid"));
    assert!(matches!(snapshot_rows(direct.path(), "acme/new-direct", None), Rows::Skip));
    let Rows::List(pin) = snapshot_rows(direct.path(), "acme/new-direct", Some(&original)) else {
        panic!("proved alias should select the canonical direct listing");
    };
    assert_eq!(pin.repo.as_deref(), Some("acme/new-direct"));
    assert_eq!(pin.host.as_deref(), Some("forge-one.invalid"));
    git(
        direct.path(),
        &[
            "remote",
            "set-url",
            "origin",
            "https://forge-two.invalid/acme/old-direct",
        ],
    );
    assert!(matches!(
        snapshot_rows(direct.path(), "acme/new-direct", Some(&original)),
        Rows::Skip
    ));
    let current = authority("forge-two.invalid", "acme/old-direct", "acme/old-direct");
    let Rows::List(pin) = snapshot_rows(direct.path(), "acme/old-direct", Some(&current)) else {
        panic!("the next verified identity should select the new direct listing");
    };
    assert_eq!(pin.repo.as_deref(), Some("acme/old-direct"));
    assert_eq!(pin.host.as_deref(), Some("forge-two.invalid"));
    crate::forge_repo_facts::set_test_enabled(false);
}
