//! `$LOOM_HYPERPARAMS` retirement coverage (#11107). Its own integration-test
//! process: `startup_init` now succeeds with the retired env var set, so it
//! captures the process-global `OnceLock`s — which would pin them to this
//! test's empty root and break the capture assertions in
//! `hyperparams_startup.rs`.

use loom_daemon::hyperparams::{startup_init, HYPERPARAMS_ENV};
use serial_test::serial;

/// `$LOOM_HYPERPARAMS` is retired (#11107): set to garbage it only warns and
/// neither startup nor `--validate` fails.
#[test]
#[serial]
fn retired_env_vector_is_ignored_by_startup_and_validate() {
    std::env::set_var(HYPERPARAMS_ENV, "not json");
    let tmp = tempfile::tempdir().unwrap();
    let started = startup_init(tmp.path());
    let args = loom_daemon::hyperparams::HyperparamsArgs {
        json: false,
        validate: true,
        workspace: tmp.path().to_path_buf(),
    };
    let validated = args.run();
    std::env::remove_var(HYPERPARAMS_ENV);
    started.expect("startup ignores the retired env var");
    validated.expect("--validate ignores the retired env var");
}
