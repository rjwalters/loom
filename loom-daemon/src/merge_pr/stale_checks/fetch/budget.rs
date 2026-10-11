//! How fast the freshness guard may ask GitHub (#11418).
//!
//! GitHub's secondary limits trip on bursts, not on hourly totals. So the
//! guard spaces its own requests ([`before_request`]) and, once GitHub
//! answers with a secondary limit, sends nothing more until it clears
//! ([`note_secondary`]). The hold is host-wide: it is written to the shared
//! ETag store directory, so the next `merge-pr` pass on this host fails closed
//! at once instead of retrying into the same limit.

use std::path::PathBuf;
use std::time::Duration;

/// Default spacing between two of the guard's requests.
pub(crate) const DEFAULT_GAP: Duration = Duration::from_millis(200);

/// Overrides [`DEFAULT_GAP`], in milliseconds (`0` disables the spacing).
pub(crate) const GAP_ENV: &str = "LOOM_STALE_CHECKS_MIN_GAP_MS";

/// The host-wide hold, inside the shared store directory.
const HOLD_FILE: &str = "stale-checks-secondary-hold.json";

#[derive(serde::Serialize, serde::Deserialize)]
struct Hold {
    until: i64,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn hold_path() -> Option<PathBuf> {
    crate::forge_etag_store::daemon_store_dir().map(|d| d.join(HOLD_FILE))
}

/// When the current secondary-limit hold ends, if one is in force at `now`.
pub(crate) fn hold_until(now: i64) -> Option<i64> {
    let on_disk = hold_path()
        .and_then(|p| crate::forge_etag_store::read_private_json::<Hold>(&p))
        .map(|h| h.until);
    on_disk
        .into_iter()
        .chain(state::held())
        .max()
        .filter(|&until| until > now)
}

/// Refuse while a hold is in force; otherwise wait out the spacing.
///
/// # Errors
/// A secondary-limit hold is in force. Nothing is sent.
pub(crate) fn before_request() -> Result<(), String> {
    let now = now();
    if let Some(until) = hold_until(now) {
        return Err(format!(
            "GitHub answered a secondary rate limit; the freshness guard sends no request \
             for another {}s (until epoch {until}) rather than retrying into it (#11418)",
            until - now
        ));
    }
    state::pace(gap());
    Ok(())
}

/// GitHub refused with a secondary limit that clears at `retry_at` (epoch
/// seconds; `None` = a minute from now). Holds every later request.
pub(crate) fn note_secondary(retry_at: Option<i64>) {
    let now = now();
    let until = retry_at.unwrap_or(now + 60).clamp(now + 1, now + 3600);
    let until = hold_until(now).map_or(until, |held| held.max(until));
    state::hold(until);
    if let Some(path) = hold_path() {
        crate::forge_etag_store::write_private_json(&path, &Hold { until });
    }
}

fn gap() -> Duration {
    std::env::var(GAP_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(DEFAULT_GAP, Duration::from_millis)
}

#[cfg(test)]
pub(crate) use state::{reset as reset_for_test, slept as slept_for_test};

/// Process state. Test builds keep it per thread and never really sleep.
#[cfg(not(test))]
mod state {
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    static HELD: AtomicI64 = AtomicI64::new(0);
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);

    pub(super) fn held() -> Option<i64> {
        Some(HELD.load(Ordering::Relaxed)).filter(|&t| t > 0)
    }

    pub(super) fn hold(until: i64) {
        HELD.fetch_max(until, Ordering::Relaxed);
    }

    pub(super) fn pace(gap: Duration) {
        let Ok(mut last) = LAST.lock() else { return };
        if let Some(prev) = *last {
            let wait = gap.saturating_sub(prev.elapsed());
            if !wait.is_zero() {
                std::thread::sleep(wait);
            }
        }
        *last = Some(Instant::now());
    }
}

#[cfg(test)]
mod state {
    use std::cell::{Cell, RefCell};
    use std::time::{Duration, Instant};

    thread_local! {
        static HELD: Cell<i64> = const { Cell::new(0) };
        static LAST: Cell<Option<Instant>> = const { Cell::new(None) };
        static SLEPT: RefCell<Vec<Duration>> = const { RefCell::new(Vec::new()) };
    }

    pub(super) fn held() -> Option<i64> {
        Some(HELD.with(Cell::get)).filter(|&t| t > 0)
    }

    pub(super) fn hold(until: i64) {
        HELD.with(|h| h.set(h.get().max(until)));
    }

    /// Records the wait instead of sleeping (see [`slept`]).
    pub(super) fn pace(gap: Duration) {
        if let Some(prev) = LAST.with(Cell::get) {
            let wait = gap.saturating_sub(prev.elapsed());
            if !wait.is_zero() {
                SLEPT.with(|s| s.borrow_mut().push(wait));
            }
        }
        LAST.with(|l| l.set(Some(Instant::now())));
    }

    /// The waits this thread's pacing asked for so far.
    pub(crate) fn slept() -> Vec<Duration> {
        SLEPT.with(|s| s.borrow().clone())
    }

    /// Forget this thread's hold and pacing history.
    pub(crate) fn reset() {
        HELD.with(|h| h.set(0));
        LAST.with(|l| l.set(None));
        SLEPT.with(|s| s.borrow_mut().clear());
    }
}
