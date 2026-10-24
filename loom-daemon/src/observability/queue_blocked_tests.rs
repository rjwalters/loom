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
    assert!(matches!(snapshot_rows(fixture.path()), Rows::Snapshot(_)));

    set(Vec::new());
    assert!(matches!(snapshot_rows(fixture.path()), Rows::Skip));
    assert_eq!(fixture.gh_calls(), "");

    let standalone = tempfile::tempdir().unwrap();
    assert!(matches!(snapshot_rows(standalone.path()), Rows::List));
}
