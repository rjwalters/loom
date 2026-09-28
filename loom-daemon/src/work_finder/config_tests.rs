//! Coverage for the `hostClass` / `allowHeavyLocal` fields added to
//! [`super::WorkFinderConfig`] / [`super::read_work_finder_config`] by Issue
//! #9034. The pre-existing fields' own coverage stays in
//! `work_finder/tests.rs` (unmoved — this file is new, not a relocation).

use std::path::Path;

use super::{read_work_finder_config, WorkFinderConfig};
use crate::work_finder::host_class::HostClass;

fn write_config(dir: &Path, body: &str) {
    let loom_dir = dir.join(".loom");
    std::fs::create_dir_all(&loom_dir).unwrap();
    std::fs::write(loom_dir.join("config.json"), body).unwrap();
}

#[test]
fn read_work_finder_config_parses_host_class() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"autonomous": {"workFinder": {"hostClass": "local-dev"}}}"#);
    assert_eq!(read_work_finder_config(tmp.path()).host_class, Some(HostClass::LocalDev));
}

#[test]
fn read_work_finder_config_drops_an_unrecognized_host_class() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"autonomous": {"workFinder": {"hostClass": "laptop"}}}"#);
    assert_eq!(read_work_finder_config(tmp.path()).host_class, None);
}

#[test]
fn read_work_finder_config_host_class_absent_by_default() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"autonomous": {"workFinder": {}}}"#);
    assert_eq!(read_work_finder_config(tmp.path()).host_class, None);
    assert_eq!(read_work_finder_config(tmp.path()), WorkFinderConfig::default());
}

#[test]
fn read_work_finder_config_parses_allow_heavy_local() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"autonomous": {"workFinder": {"allowHeavyLocal": true}}}"#);
    assert_eq!(read_work_finder_config(tmp.path()).allow_heavy_local, Some(true));
}
