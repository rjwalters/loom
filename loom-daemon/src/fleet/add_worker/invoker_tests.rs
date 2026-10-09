//! #11044: `fleet add-worker` marks its `loom-daemon-update.sh` calls.
//!
//! A sibling of `tests.rs`, which is over the file-size ratchet's threshold
//! (`.loom/docs/file-size-policy.md`).

use super::*;

/// The bootstrap is unattended, so both update-script calls run marked and
/// the fleet floor confirmation never refuses them.
#[test]
fn machine_layout_marks_the_update_script_as_add_worker() {
    let script = render_machine_layout(DEFAULT_LOOM_REPO_URL);
    let marker = script
        .find("export LOOM_DAEMON_UPDATE_INVOKER=add-worker")
        .expect("machine-layout must mark its update-script calls");
    let first_call = script
        .find(r#"if ! "$UPDATE_SCRIPT" --no-restart"#)
        .expect("machine-layout invokes the update script");
    assert!(marker < first_call, "the marker must be exported before the first call");
}
