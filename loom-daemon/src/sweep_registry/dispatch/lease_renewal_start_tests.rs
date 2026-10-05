//! Issue #10348: a dispatched lease-renewal start carries the marker that
//! lets `sweep-lease-renew.sh` skip its per-cycle issue-state read.
use super::*;

#[test]
fn dispatched_start_sets_source_marker() {
    let cmd = lease_renewal_start_command(
        Path::new("/w/.loom/scripts/sweep-lease-renew.sh"),
        42,
        "sweep-issue-42-1",
        999,
        "h",
        Path::new("/w"),
    );
    let marker = cmd
        .get_envs()
        .find(|(k, _)| *k == "LOOM_SWEEP_LEASE_RENEW_SOURCE")
        .and_then(|(_, v)| v);
    assert_eq!(marker, Some(std::ffi::OsStr::new("dispatch")));
    let args: Vec<_> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(args[..4], ["start", "42", "--watch-pid", "999"]);
}
