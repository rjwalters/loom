//! How fast the freshness guard may ask GitHub (#11418).
//!
//! GitHub's secondary limits trip on bursts, not on hourly totals. So the
//! guard spaces its own requests ([`before_request`]) and, once GitHub
//! answers with a secondary limit, sends nothing more on that credential
//! until it clears ([`note_secondary`]). Secondary limits are per user or
//! installation token, so a hold is scoped like the ETag entries: forge
//! host, credential ([`crate::forge_etag_store::credential_scope`]) and repo
//! owner (a reader App's installation is per owner). It is written to the
//! shared store directory, so the next `merge-pr` pass on this host with the
//! same credential fails closed at once instead of retrying into the limit.

use std::path::PathBuf;
use std::time::Duration;

/// Default spacing between two of the guard's requests.
pub(crate) const DEFAULT_GAP: Duration = Duration::from_millis(200);

/// Overrides [`DEFAULT_GAP`], in milliseconds (`0` disables the spacing).
pub(crate) const GAP_ENV: &str = "LOOM_STALE_CHECKS_MIN_GAP_MS";

/// The longest hold, also applied when one is read back.
const MAX_HOLD_SECS: i64 = 3600;

/// A hold with no `retry-after` (and no exhausted primary window).
const DEFAULT_HOLD_SECS: i64 = 60;

#[derive(serde::Serialize, serde::Deserialize)]
struct Hold {
    until: i64,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// The identity a hold on reads of `nwo` belongs to.
pub(crate) fn scope(nwo: &str) -> String {
    let target = crate::forge_etag_store::resolve_target(None, Some(nwo));
    scope_with(
        &crate::forge_etag_store::host_scope(&target),
        &crate::forge_etag_store::credential_scope(None, &target),
        nwo,
    )
}

/// [`scope`] with the host and credential given.
pub(crate) fn scope_with(host: &str, credential: &str, nwo: &str) -> String {
    let owner = nwo
        .split('/')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    format!("{host}|{credential}|{owner}")
}

/// Where the hold on `scope` is stored (tests plant values here).
#[cfg(test)]
pub(crate) fn hold_file_for_test(scope: &str) -> Option<PathBuf> {
    hold_path(scope)
}

/// Store a raw `until` for `scope`, bypassing the write-side cap.
#[cfg(test)]
pub(crate) fn plant_hold_for_test(scope: &str, until: i64) {
    state::hold(scope, until);
    if let Some(path) = hold_path(scope) {
        crate::forge_etag_store::write_private_json(&path, &Hold { until });
    }
}

fn hold_path(scope: &str) -> Option<PathBuf> {
    crate::forge_etag_store::daemon_store_dir()
        .map(|d| crate::forge_etag_store::entry_path_with_prefix(&d, "stale-checks-hold-", scope))
}

/// When the hold on `scope` ends, if one is in force at `now`. A stored value
/// past the cap (a clock step, a hand-edited file) counts as the cap.
pub(crate) fn hold_until(scope: &str, now: i64) -> Option<i64> {
    let on_disk = hold_path(scope)
        .and_then(|p| crate::forge_etag_store::read_private_json::<Hold>(&p))
        .map(|h| h.until);
    on_disk
        .into_iter()
        .chain(state::held(scope))
        .max()
        .map(|until| until.min(now + MAX_HOLD_SECS))
        .filter(|&until| until > now)
}

/// Refuse while `scope` is held; otherwise wait out the spacing.
///
/// # Errors
/// A secondary-limit hold is in force. Nothing is sent.
pub(crate) fn before_request(scope: &str) -> Result<(), String> {
    let now = now();
    if let Some(until) = hold_until(scope, now) {
        return Err(format!(
            "GitHub answered a secondary rate limit; the freshness guard sends no request \
             on this credential for another {}s (until epoch {until}) rather than retrying \
             into it (#11418)",
            until - now
        ));
    }
    pace();
    Ok(())
}

/// Wait out the spacing only (a later page of a read already admitted).
pub(crate) fn pace() {
    state::pace(gap());
}

/// GitHub refused a read on `scope` with a secondary limit that clears at
/// `retry_at` (epoch seconds; `None` = a minute from now). Holds every later
/// request on that scope.
pub(crate) fn note_secondary(scope: &str, retry_at: Option<i64>) {
    let now = now();
    let until = retry_at
        .unwrap_or(now + DEFAULT_HOLD_SECS)
        .clamp(now + 1, now + MAX_HOLD_SECS);
    let until = hold_until(scope, now).map_or(until, |held| held.max(until));
    state::hold(scope, until);
    if let Some(path) = hold_path(scope) {
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
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    static HELD: Mutex<Option<HashMap<String, i64>>> = Mutex::new(None);
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);

    pub(super) fn held(scope: &str) -> Option<i64> {
        HELD.lock().ok()?.as_ref()?.get(scope).copied()
    }

    pub(super) fn hold(scope: &str, until: i64) {
        if let Ok(mut held) = HELD.lock() {
            let entry = held
                .get_or_insert_with(HashMap::new)
                .entry(scope.to_string());
            let at = entry.or_insert(until);
            *at = (*at).max(until);
        }
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
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    thread_local! {
        static HELD: RefCell<HashMap<String, i64>> = RefCell::new(HashMap::new());
        static LAST: Cell<Option<Instant>> = const { Cell::new(None) };
        static SLEPT: RefCell<Vec<Duration>> = const { RefCell::new(Vec::new()) };
    }

    pub(super) fn held(scope: &str) -> Option<i64> {
        HELD.with(|h| h.borrow().get(scope).copied())
    }

    pub(super) fn hold(scope: &str, until: i64) {
        HELD.with(|h| {
            let mut h = h.borrow_mut();
            let at = h.entry(scope.to_string()).or_insert(until);
            *at = (*at).max(until);
        });
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
        HELD.with(|h| h.borrow_mut().clear());
        LAST.with(|l| l.set(None));
        SLEPT.with(|s| s.borrow_mut().clear());
    }
}
