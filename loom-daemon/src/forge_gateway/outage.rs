//! A gateway outage is not a credential failure: fall back, don't fail.
//!
//! The reader chain withdraws a reader only for what it understands as the
//! credential's fault (a rate limit, a refused token, a coverage miss), and
//! returns any other failure as the answer, since another reader would not
//! help. That is wrong for the gateway: when the relay itself is down or
//! unreachable, the host-held readers and the writer still reach GitHub. So a
//! gateway attempt that fails with a transport error, a timeout or a
//! `502`/`503`/`504` withdraws the gateway everywhere for
//! [`crate::forge_read_pool::DEFAULT_WITHDRAWAL`], and the read continues to
//! the readers, then the writer.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::GATEWAY_APP_ID;

/// `gh` / Go transport error text that means no HTTP answer arrived.
const TRANSPORT_MARKERS: &[&str] = &[
    "connection refused",
    "connection reset",
    "dial tcp",
    "no such host",
    "i/o timeout",
    "tls handshake",
    "context deadline exceeded",
    "timeout awaiting",
    "unexpected eof",
    "server misbehaving",
    "network is unreachable",
];

/// The marker the agent front leaves beside the profiles: no front routes
/// through the gateway until the epoch second it holds.
const FRONT_OUTAGE: &str = "outage-until";

/// The status `gh` printed as `HTTP <nnn>` on stderr, if any.
#[must_use]
pub fn http_status_in(stderr: &str) -> Option<u16> {
    stderr.match_indices("HTTP ").find_map(|(i, _)| {
        let digits = stderr.get(i + 5..i + 8)?;
        digits
            .bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| digits.parse().ok())
            .flatten()
    })
}

/// Whether a failed gateway attempt is the gateway's outage. `status` is the
/// parsed response status when the call printed headers (`--include`), and
/// `expects_headers` says it did: then no status at all means no response
/// arrived. Without headers, only a `502`/`503`/`504` or a transport error
/// on stderr counts, so a GraphQL "not found" never withdraws the gateway.
#[must_use]
pub fn is_outage(status: Option<u16>, stderr: &str, expects_headers: bool) -> bool {
    match status.or_else(|| http_status_in(stderr)) {
        Some(s) => matches!(s, 502..=504),
        None if expects_headers => true,
        None => {
            let e = stderr.to_ascii_lowercase();
            TRANSPORT_MARKERS.iter().any(|m| e.contains(m))
        }
    }
}

/// Withdraw the gateway App-wide for the default window. A test build
/// records the call on its own thread instead of touching the process-wide
/// table, so parallel tests never see each other's outages.
pub fn withdraw_for_outage(why: &str) {
    #[cfg(test)]
    {
        TEST_OUTAGES.with(|o| o.borrow_mut().push(why.to_string()));
    }
    #[cfg(not(test))]
    withdraw_globally(why);
}

#[cfg(test)]
thread_local! {
    static TEST_OUTAGES: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// The outages this test thread recorded, cleared.
#[cfg(test)]
#[must_use]
pub fn take_test_outages() -> Vec<String> {
    TEST_OUTAGES.with(|o| std::mem::take(&mut *o.borrow_mut()))
}

#[cfg_attr(test, allow(dead_code))]
fn withdraw_globally(why: &str) {
    let window = crate::forge_read_pool::DEFAULT_WITHDRAWAL;
    crate::forge_read_pool::withdraw_until(GATEWAY_APP_ID, SystemTime::now() + window);
    log::warn!(
        "forge_gateway: the gateway did not answer ({why}); reads use the readers and the writer \
         for {}s — 2am#4367",
        window.as_secs()
    );
}

/// The front's half: remember an outage beside the profiles so the next
/// agent calls go direct at once instead of each paying a failed attempt.
pub fn note_front_outage(base: &Path) {
    let until = SystemTime::now() + crate::forge_read_pool::DEFAULT_WITHDRAWAL;
    let secs = until.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let _ = super::publish::write_private(&base.join(FRONT_OUTAGE), secs.to_string().as_bytes());
}

/// Whether a front-recorded outage is still in force at `now`.
#[must_use]
pub fn front_outage_active(base: &Path, now: SystemTime) -> bool {
    std::fs::read_to_string(base.join(FRONT_OUTAGE))
        .ok()
        .and_then(|t| t.trim().parse::<u64>().ok())
        .is_some_and(|until| UNIX_EPOCH + Duration::from_secs(until) > now)
}
