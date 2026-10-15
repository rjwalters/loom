//! The batched open-PR pre-filter at the tick seam (Issue #8865), with a fake
//! dispatcher. Its own file with its own fakes because `work_finder/tests.rs`
//! is over the file-size ratchet. The production dispatcher and a fake `gh`
//! are driven in `open_pr_prefilter_forge_tests.rs`.

use std::collections::HashMap;
use std::path::Path;

use super::log_changes;

fn skip_lines(root: &Path, blocked: &[(u32, u32)]) -> Vec<String> {
    let blocked: HashMap<u32, u32> = blocked.iter().copied().collect();
    crate::test_log_capture::capture_logs(|| log_changes(root, &blocked))
        .into_iter()
        .filter(|(level, _)| *level == log::Level::Info)
        .map(|(_, msg)| msg)
        .collect()
}

/// The skip line is logged when an issue becomes blocked (or its PR changes),
/// not on every tick it stays blocked.
#[test]
fn the_skip_line_is_logged_once_per_state_change() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let first = skip_lines(root, &[(8136, 8131), (8200, 8201)]);
    assert_eq!(first.len(), 2, "{first:?}");
    assert!(first[0].contains("skipping issue #8136"), "{first:?}");
    assert!(first[0].contains("open linked PR #8131"), "{first:?}");
    assert!(first[0].contains("#4123 open-PR guard"), "{first:?}");

    assert!(
        skip_lines(root, &[(8136, 8131), (8200, 8201)]).is_empty(),
        "steady state is quiet"
    );

    // #8200 gets a different PR; #8136 is unchanged.
    let changed = skip_lines(root, &[(8136, 8131), (8200, 8300)]);
    assert_eq!(changed.len(), 1, "{changed:?}");
    assert!(changed[0].contains("#8200") && changed[0].contains("#8300"), "{changed:?}");

    // #8136 is freed, then blocked again: that is a new state.
    assert!(skip_lines(root, &[(8200, 8300)]).is_empty());
    assert_eq!(skip_lines(root, &[(8136, 8131), (8200, 8300)]).len(), 1);

    // Another workspace keeps its own state.
    let other = tempfile::tempdir().unwrap();
    assert_eq!(skip_lines(other.path(), &[(8136, 8131)]).len(), 1);
}
