//! Tests for the `.github/labels.yml` managed-block refresh (#11033): the
//! pure splice, then the daemon resync over a temp workspace (the fixtures are
//! `payload_tests`'s), then `resync-payload`'s exit code.

use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use super::super::standalone::{run_with, EXIT_FAILED, EXIT_OK};
use super::super::surfaces::SkipReason;
use super::super::{materialize_with, resync_workspace_with, Payload, ResyncOutcome};
use super::*;
use crate::init::payload_tests::{fake_defaults, installed_workspace, stamp, write};

const UPSTREAM: &str = "# BEGIN LOOM LABELS\n# LOOM-MANAGED BLOCK\n- name: loom:issue\n  \
                        color: \"3B82F6\"\n- name: loom:needs-revision\n  color: \"D97706\"\n\
                        # END LOOM LABELS\n";
const STALE: &str = "# BEGIN LOOM LABELS\n# LOOM-MANAGED BLOCK\n- name: loom:issue\n  \
                     color: \"3B82F6\"\n- name: loom:urgent\n  color: \"DC2626\"\n\
                     # END LOOM LABELS";
const ABOVE: &str = "# consumer labels\n- name: team:frontend\n  color: \"00ff00\"\n\n";
const BELOW: &str = "\n\n- name: team:backend   # trailing comment\n  color: \"0000ff\"\n";
const REL: &str = ".github/labels.yml";

fn upstream_block() -> &'static str {
    UPSTREAM.trim_end_matches('\n')
}

// ---- the splice ----------------------------------------------------------

#[test]
fn a_retired_label_leaves_the_block_and_every_byte_outside_it_stays() {
    let existing = format!("{ABOVE}{STALE}{BELOW}");
    let new = refresh(Some(&existing), UPSTREAM).unwrap();
    assert_eq!(new, format!("{ABOVE}{}{BELOW}", upstream_block()));
    assert!(!new.contains("loom:urgent"));
    assert!(new.contains("loom:needs-revision"));
    assert_eq!(refresh(Some(&new), UPSTREAM).unwrap(), new, "idempotent");
}

#[test]
fn an_end_marker_on_the_last_line_keeps_its_missing_newline() {
    let existing = format!("{ABOVE}{STALE}");
    assert_eq!(
        refresh(Some(&existing), UPSTREAM).unwrap(),
        format!("{ABOVE}{}", upstream_block())
    );
}

#[test]
fn markers_with_trailing_whitespace_or_crlf_are_markers() {
    let existing =
        "a\r\n# BEGIN LOOM LABELS \r\n- name: loom:urgent\r\n# END LOOM LABELS\t\r\nb\r\n";
    assert_eq!(
        refresh(Some(existing), UPSTREAM).unwrap(),
        format!("a\r\n{}\nb\r\n", upstream_block())
    );
}

#[test]
fn no_markers_appends_the_block_and_keeps_the_old_content_as_a_prefix() {
    let old = "- name: team:frontend\n  color: \"00ff00\"";
    assert_eq!(refresh(Some(old), UPSTREAM).unwrap(), format!("{old}\n\n{UPSTREAM}"));
    let old_nl = format!("{old}\n");
    assert_eq!(refresh(Some(&old_nl), UPSTREAM).unwrap(), format!("{old_nl}\n{UPSTREAM}"));
    assert_eq!(refresh(Some(""), UPSTREAM).unwrap(), UPSTREAM);
}

#[test]
fn no_file_gets_the_shipped_file() {
    assert_eq!(refresh(None, UPSTREAM).unwrap(), UPSTREAM);
}

#[test]
fn malformed_markers_are_refused() {
    for (case, text) in [
        ("duplicate BEGIN", format!("# BEGIN LOOM LABELS\n{STALE}\n")),
        ("duplicate block", format!("{STALE}\n{STALE}\n")),
        ("END before BEGIN", "# END LOOM LABELS\nx\n# BEGIN LOOM LABELS\n".to_string()),
        ("lone BEGIN", "# BEGIN LOOM LABELS\n- name: loom:urgent\n".to_string()),
        ("lone END", "- name: loom:urgent\n# END LOOM LABELS\n".to_string()),
        ("indented", "  # BEGIN LOOM LABELS\nx\n# END LOOM LABELS\n".to_string()),
        ("no space", "#BEGIN LOOM LABELS\nx\n# END LOOM LABELS\n".to_string()),
        ("suffix", "# BEGIN LOOM LABELS v2\nx\n# END LOOM LABELS\n".to_string()),
    ] {
        assert!(matches!(classify(&text), Shape::Malformed(_)), "{case}: {:?}", classify(&text));
        assert!(refresh(Some(&text), UPSTREAM).is_err(), "{case}");
    }
}

#[test]
fn prose_naming_the_markers_is_not_a_marker() {
    let text = "# everything between the BEGIN/END LOOM LABELS markers\n# BEGIN of team list\n";
    assert_eq!(classify(text), Shape::None);
}

#[test]
fn a_payload_without_a_block_is_refused() {
    assert!(refresh(Some(STALE), "- name: x\n").is_err());
}

#[test]
fn the_real_labels_file_has_one_block_without_retired_labels() {
    let real = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../defaults/.github/labels.yml"),
    )
    .unwrap();
    let Shape::Block { start, end } = classify(&real) else {
        panic!("defaults/.github/labels.yml must have exactly one managed block");
    };
    let block = &real[start..end];
    for retired in ["- name: loom:urgent", "- name: tier:"] {
        assert!(!block.contains(retired), "{retired} is retired");
    }
}

// ---- the daemon resync ---------------------------------------------------

/// A workspace and payload whose only label difference is `existing` (or no
/// file at all) against [`UPSTREAM`].
fn workspace(tmp: &TempDir, existing: Option<&str>) -> (PathBuf, Payload) {
    let defaults = fake_defaults(tmp.path());
    write(&defaults.join(REL), UPSTREAM);
    let ws = installed_workspace(tmp.path(), &defaults, "0.19.880");
    if let Some(text) = existing {
        write(&ws.join(REL), text);
    }
    (ws, Payload::from_defaults(defaults, stamp("0.19.881")))
}

fn installed_files(ws: &Path) -> Vec<String> {
    let meta: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(ws.join(crate::install_compat::INSTALL_METADATA_PATH)).unwrap(),
    )
    .unwrap();
    meta["installed_files"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn one_daemon_resync_drops_a_retired_label_and_a_second_writes_nothing() {
    let tmp = TempDir::new().unwrap();
    let existing = format!("{ABOVE}{STALE}{BELOW}");
    let (ws, payload) = workspace(&tmp, Some(&existing));

    let diff = materialize_with(&payload, &ws).unwrap();
    assert!(diff.changed.contains(&REL.to_string()), "{:?}", diff.changed);
    resync_workspace_with(&payload, &ws).unwrap();
    assert_eq!(
        fs::read_to_string(ws.join(REL)).unwrap(),
        format!("{ABOVE}{}{BELOW}", upstream_block())
    );
    assert!(!installed_files(&ws).contains(&REL.to_string()), "co-owned, never recorded");
    assert_eq!(resync_workspace_with(&payload, &ws).unwrap(), ResyncOutcome::Unchanged);
}

#[test]
fn a_missing_labels_file_is_created() {
    let tmp = TempDir::new().unwrap();
    let (ws, payload) = workspace(&tmp, None);
    let diff = materialize_with(&payload, &ws).unwrap();
    assert!(diff.added.contains(&REL.to_string()), "{:?}", diff.added);
    resync_workspace_with(&payload, &ws).unwrap();
    assert_eq!(fs::read_to_string(ws.join(REL)).unwrap(), UPSTREAM);
}

#[test]
fn malformed_markers_are_skipped_reported_and_the_file_kept() {
    let tmp = TempDir::new().unwrap();
    let bad = format!("{STALE}\n{STALE}\n");
    let (ws, payload) = workspace(&tmp, Some(&bad));

    let diff = materialize_with(&payload, &ws).unwrap();
    assert!(!diff.changed.contains(&REL.to_string()));
    assert!(
        diff.skipped_surfaces
            .iter()
            .any(|s| s.path == REL && matches!(s.why, SkipReason::MalformedMarkers(_))),
        "{:?}",
        diff.skipped_surfaces
    );

    let report = run_with(&payload, &ws, false).unwrap();
    assert_eq!(report.exit_code(), EXIT_FAILED, "a malformed block is a failed run");
    assert!(report
        .render(&ws)
        .contains("skipped .github/labels.yml (NOT refreshed"));
    assert_eq!(fs::read_to_string(ws.join(REL)).unwrap(), bad);
}

#[test]
fn a_clean_standalone_resync_exits_ok() {
    let tmp = TempDir::new().unwrap();
    let existing = format!("{ABOVE}{STALE}{BELOW}");
    let (ws, payload) = workspace(&tmp, Some(&existing));
    assert_eq!(run_with(&payload, &ws, false).unwrap().exit_code(), EXIT_OK);
    assert!(!fs::read_to_string(ws.join(REL))
        .unwrap()
        .contains("loom:urgent"));
}

#[test]
fn a_pinned_labels_file_is_never_written() {
    let tmp = TempDir::new().unwrap();
    let existing = format!("{ABOVE}{STALE}{BELOW}");
    let (ws, payload) = workspace(&tmp, Some(&existing));
    write(&ws.join(".loom/resync-ignore"), ".github/labels.yml\n");
    let diff = materialize_with(&payload, &ws).unwrap();
    assert!(!diff.changed.contains(&REL.to_string()), "{:?}", diff.changed);
    resync_workspace_with(&payload, &ws).unwrap();
    assert_eq!(fs::read_to_string(ws.join(REL)).unwrap(), existing);
}
