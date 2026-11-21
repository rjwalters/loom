//! `$LOOM_HYPERPARAMS` retirement coverage (#11107). Its own integration-test
//! process: `startup_init` now succeeds with the retired env var set, so it
//! captures the process-global `OnceLock`s — which would pin them to this
//! test's empty root and break the capture assertions in
//! `hyperparams_startup.rs`.

use loom_daemon::hyperparams::{startup_init, HYPERPARAMS_ENV};
use serial_test::serial;
use std::sync::atomic::{AtomicUsize, Ordering};

struct HyperparameterWarnings(AtomicUsize);

impl log::Log for HyperparameterWarnings {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() == log::Level::Warn && metadata.target() == "loom_daemon::hyperparams"
    }

    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn flush(&self) {}
}

static WARNINGS: HyperparameterWarnings = HyperparameterWarnings(AtomicUsize::new(0));

/// The retired variable warns once, and startup and validation still succeed.
#[test]
#[serial]
fn retired_env_vector_is_ignored_by_startup_and_validate() {
    log::set_logger(&WARNINGS).unwrap();
    log::set_max_level(log::LevelFilter::Warn);

    std::env::set_var(HYPERPARAMS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    let started = startup_init(tmp.path());
    let startup_warnings = WARNINGS.0.load(Ordering::Relaxed);

    std::env::set_var(HYPERPARAMS_ENV, "not json");
    let args = loom_daemon::hyperparams::HyperparamsArgs {
        json: false,
        validate: true,
        workspace: tmp.path().to_path_buf(),
    };
    let validated = args.run();
    std::env::remove_var(HYPERPARAMS_ENV);
    started.expect("startup ignores the retired env var");
    validated.expect("--validate ignores the retired env var");
    assert_eq!(startup_warnings, 1);
    assert_eq!(WARNINGS.0.load(Ordering::Relaxed), 1);
}

// Fail this test binary on any spawn of the real `gh` (#10225, extends #10138).
#[allow(dead_code)]
#[path = "../src/live_gh_guard.rs"]
mod live_gh_guard;
