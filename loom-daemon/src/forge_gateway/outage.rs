//! A failed gateway attempt is not a credential failure: rerun, and withdraw
//! only on the gateway's own evidence.
//!
//! The reader chain withdraws a reader only for what it understands as the
//! credential's fault (a rate limit, a refused token, a coverage miss), and
//! returns any other failure as the answer, since another reader would not
//! help. That is wrong for the gateway: when the relay itself is broken, the
//! host-held readers and the writer still reach GitHub. Two decisions follow
//! ([`fault`]):
//!
//! - **Rerun** ([`Fault::Rerun`]): any gateway attempt that times out, gets
//!   a transport error, or fails with a `5xx` is run once more on the
//!   readers, then the writer. A non-`api` read (`gh pr view`, …) shows only
//!   `HTTP 5xx` on stderr, so whose `5xx` it was cannot be told. A `5xx`
//!   GitHub relayed then costs one repeated read, and a real gateway fault
//!   (a `500 misconfigured`, a Worker crash) is still served.
//! - **Withdraw** ([`Fault::Withdraw`], App-wide for
//!   [`crate::forge_read_pool::DEFAULT_WITHDRAWAL`], and the front's
//!   `outage-until`): only on evidence the gateway itself gives (see
//!   [`is_outage`]). A `5xx` GitHub returns through the relay never takes
//!   the gateway out.

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
pub(super) const FRONT_OUTAGE: &str = "outage-until";

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

/// Whether a failed gateway attempt is the gateway's own outage, judged only
/// from what the gateway itself signals, so a `5xx` GitHub returned through
/// the relay never takes the gateway out (the reader and the writer would
/// get the same answer).
///
/// - With headers (`--include`, `response`): a `5xx` the gateway marked as
///   its own (`x-github-proxy-refusal`), or one that never came from the
///   gateway (no `x-github-proxy-version`: the edge in front of a crashed
///   Worker).
/// - Without headers: a `5xx` on stderr whose body on stdout has the
///   gateway's opaque `{error, request_id}` shape (GitHub's carry
///   `message`), or a Cloudflare edge status (`520`–`530`), which GitHub
///   never sends.
/// - Neither: a transport error on stderr. A `gh` that failed before
///   sending a request (a bad flag, a config error) shows none, so it is
///   never an outage, headers or not.
///
/// A timeout is the caller's to judge (it has no output to read).
#[must_use]
pub fn is_outage(
    response: Option<&crate::forge_listing::HttpResponse>,
    stdout: &str,
    stderr: &str,
) -> bool {
    if let Some(r) = response {
        return r.status >= 500 && (r.gateway.refusal.is_some() || !r.gateway.answered);
    }
    if let Some(s) = http_status_in(stderr) {
        return s >= 500 && ((520..=530).contains(&s) || is_gateway_body(stdout));
    }
    let e = stderr.to_ascii_lowercase();
    TRANSPORT_MARKERS.iter().any(|m| e.contains(m))
}

/// The gateway's opaque 5xx body: `{"error": <code>, "request_id": …}` and
/// no `message` (errors.js, SECURITY.md rule 6 on the gateway side). It
/// depends on the gateway keeping `request_id` in that body. If it is ever
/// dropped, an unmarked fault still reruns ([`Fault::Rerun`]); it just stops
/// withdrawing the gateway. With headers the `x-github-proxy-refusal` mark
/// decides instead.
fn is_gateway_body(stdout: &str) -> bool {
    let Ok(serde_json::Value::Object(m)) = serde_json::from_str::<serde_json::Value>(stdout.trim())
    else {
        return false;
    };
    m.get("error").is_some_and(serde_json::Value::is_string)
        && m.contains_key("request_id")
        && !m.contains_key("message")
}

/// What a failed gateway attempt calls for (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// An answer: the chain's usual rules apply.
    None,
    /// Run it once more on the readers, then the writer.
    Rerun,
    /// Rerun, and withdraw the gateway: it signalled the fault itself.
    Withdraw,
}

impl Fault {
    /// Whether the read goes on past the gateway.
    #[must_use]
    pub fn reruns(self) -> bool {
        self != Self::None
    }
}

/// Classify a failed gateway attempt from its output. A timeout has none;
/// callers treat it as [`Fault::Rerun`].
#[must_use]
pub fn fault(
    response: Option<&crate::forge_listing::HttpResponse>,
    stdout: &str,
    stderr: &str,
) -> Fault {
    if is_outage(response, stdout, stderr) {
        return Fault::Withdraw;
    }
    let status = response
        .map(|r| r.status)
        .or_else(|| http_status_in(stderr));
    if status.is_some_and(|s| s >= 500) {
        Fault::Rerun
    } else {
        Fault::None
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
