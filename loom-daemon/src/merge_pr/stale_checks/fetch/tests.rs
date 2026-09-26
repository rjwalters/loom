//! `fetch_job_log` against stub `gh` binaries (#9057): a current `gh` that
//! refuses escape-bearing output without the flag, and one predating it.
#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use super::fetch_job_log;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const LOG: &str = "HEAD is now at 8536533 Merge f8f004b17 into c3db84070";

fn stub(dir: &Path, name: &str, with_flag: &str, without_flag: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nfor a in \"$@\"; do [ \"$a\" = --allow-escape-sequences ] && {{ {with_flag}; }}; done\n{without_flag}\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn a_current_gh_is_asked_to_allow_the_logs_escape_sequences() {
    let dir = tempfile::tempdir().unwrap();
    let gh = stub(
        dir.path(),
        "gh",
        &format!("printf '\\033[36m%s\\033[0m\\n' '{LOG}'; exit 0"),
        "echo 'the response contains terminal escape sequences; pass --allow-escape-sequences to output it anyway' >&2; exit 1",
    );
    let log = fetch_job_log(gh.to_str().unwrap(), "o/r", 7).unwrap();
    assert!(log.contains(LOG), "{log:?}");
}

#[test]
fn a_gh_without_the_flag_is_retried_plainly() {
    let dir = tempfile::tempdir().unwrap();
    let gh = stub(
        dir.path(),
        "gh",
        "echo 'unknown flag: --allow-escape-sequences' >&2; exit 1",
        &format!("echo '{LOG}'"),
    );
    assert_eq!(
        fetch_job_log(gh.to_str().unwrap(), "o/r", 7)
            .unwrap()
            .trim(),
        LOG
    );
}

#[test]
fn any_other_failure_is_reported_not_retried() {
    let dir = tempfile::tempdir().unwrap();
    let gh = stub(
        dir.path(),
        "gh",
        "echo 'HTTP 404: Not Found' >&2; exit 1",
        &format!("echo '{LOG}'"),
    );
    let error = fetch_job_log(gh.to_str().unwrap(), "o/r", 7).unwrap_err();
    assert!(error.contains("404"), "{error}");
}
