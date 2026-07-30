//! Test-only capturing `log::Log` implementation shared across the crate.
//!
//! Several test modules need to assert on the **severity** of a production log
//! line (not merely that some side effect happened) — e.g. the main-health-gate
//! green path (#4083) and `init::merge_config_file`'s config-rewrite branch
//! logging (#4641).
//!
//! `log::set_boxed_logger` can only succeed **once per process**, and every
//! `#[cfg(test)]` module in this crate compiles into the *same* test binary. If
//! each module installed its own capturing logger, whichever module's test ran
//! first would win and every other module's capture would silently return an
//! empty record list — a flaky, order-dependent false pass. Single-sourcing the
//! logger here guarantees exactly one installation for the whole binary.
//!
//! Records are routed into a **thread-local** buffer that is only collected
//! while [`capture_logs`] is active on the calling thread, so logs emitted by a
//! concurrently-running test land in that other thread's (inactive) buffer and
//! are dropped. Capture is therefore race-free without serializing the suite.

use log::{Level, Log, Metadata, Record};
use std::cell::RefCell;
use std::sync::Once;

thread_local! {
    static RECORDS: RefCell<Vec<(Level, String)>> = const { RefCell::new(Vec::new()) };
    static ACTIVE: RefCell<bool> = const { RefCell::new(false) };
}

struct CaptureLogger;

impl Log for CaptureLogger {
    fn enabled(&self, _: &Metadata) -> bool {
        true
    }
    fn log(&self, record: &Record) {
        ACTIVE.with(|a| {
            if *a.borrow() {
                RECORDS.with(|r| {
                    r.borrow_mut()
                        .push((record.level(), record.args().to_string()));
                });
            }
        });
    }
    fn flush(&self) {}
}

static INIT: Once = Once::new();

/// Run `f`, returning every `(Level, message)` it logged on this thread.
pub fn capture_logs<F: FnOnce()>(f: F) -> Vec<(Level, String)> {
    INIT.call_once(|| {
        // `set_max_level(Trace)` is required — without it the log macros
        // short-circuit before reaching the logger. Because it is set to the
        // most permissive level, a regression of a line from `warn!`/`info!`
        // down to `debug!` is still *captured* (as `Debug`), so the level
        // assertion — not mere presence — is what guards the severity.
        let _ = log::set_boxed_logger(Box::new(CaptureLogger));
        log::set_max_level(log::LevelFilter::Trace);
    });
    RECORDS.with(|r| r.borrow_mut().clear());
    ACTIVE.with(|a| *a.borrow_mut() = true);
    f();
    ACTIVE.with(|a| *a.borrow_mut() = false);
    RECORDS.with(|r| r.borrow_mut().clone())
}
