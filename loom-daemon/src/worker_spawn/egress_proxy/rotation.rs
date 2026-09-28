//! Host-side account rotation for a proxied launch (issue #8818, follow-up to
//! #8697).
//!
//! With the Claude credential proxy on, the container never sees the token
//! pool — `.loom/tokens` is masked and the shared pool is not mounted — so
//! `claude-wrapper.sh` cannot bad-mark an exhausted account or select another
//! one from inside. The proxy process, on the host, already holds the launch
//! record and has pool access, so rotation happens here instead:
//!
//! ```text
//!   container                                   host (this process)
//!   ─────────                                   ───────────────────
//!   POST /.loom-egress-proxy/v1/rotate    ──▶   placeholder -> launch record
//!   Authorization: Bearer loom-placeholder-…    evidence check, rotation cap
//!   {"reason":"usage-limit"}                    mark-bad(current account)
//!                                               select() -> swap credential
//!                                         ◀──   {"rotated":true,"account":"b"}
//! ```
//!
//! # What the request can and cannot say (the trust boundary)
//!
//! The request body is a closed, `deny_unknown_fields` schema: a [`Reason`]
//! from a fixed vocabulary and one boolean. It **cannot** name an account, a
//! credential, a model, an upstream or a free-text reason — the launch is
//! identified by the placeholder it already holds, the account being rotated
//! away from is whatever the HOST recorded for that launch, the model class a
//! scoped mark narrows to is the host's own `LOOM_MODEL`, and the `.bad_tokens`
//! reason text is composed here from the enum. Nothing the container sends is
//! ever written into the pool.
//!
//! The response carries only the new account's NAME (already non-secret: the
//! same value `LOOM_TOKEN_NAME` forwards into the container at launch) and
//! whether a mark was written. Neither credential ever leaves this process.
//!
//! # Why the container's word is not enough to bad-mark
//!
//! A compromised container could otherwise walk the pool, permanently
//! auth-dead-marking every account in it. Two bounds make that expensive:
//!
//! - **Upstream evidence.** A marking rotation needs the proxy itself to have
//!   seen the upstream refuse the CURRENT credential: a 401/403 for
//!   `auth-dead` (a permanent mark), any 4xx for an exhaustion mark (a TTL
//!   mark). `concurrent-session` writes no mark and needs no evidence.
//! - **A per-launch cap** on swaps ([`DEFAULT_MAX_ROTATIONS`], overridable with
//!   `LOOM_EGRESS_PROXY_MAX_ROTATIONS` on the host).
//!
//! # Relationship to #8699
//!
//! #8699 (429-driven bad-marking at the proxy) would let the proxy mark on its
//! own. The [`Evidence`](super::registry) this module keys on is the same
//! per-record observation that work needs, so it can reuse it rather than add
//! a second observer.

use super::registry::{Refusal, Registry};
use super::PLACEHOLDER_PREFIX;
use std::path::PathBuf;
use std::sync::Arc;

/// Every path under this prefix is handled by the proxy itself and is NEVER
/// forwarded upstream, whatever follows it.
pub const CONTROL_PREFIX: &str = "/.loom-egress-proxy/";
/// The one control endpoint.
pub const ROTATE_PATH: &str = "/.loom-egress-proxy/v1/rotate";
/// Cap on a control request body. The whole schema fits in well under 100
/// bytes; anything bigger is not a rotation request.
pub const MAX_CONTROL_BODY: usize = 1024;
/// Swaps one launch may perform before further requests are refused.
pub const DEFAULT_MAX_ROTATIONS: u32 = 8;

/// Why the launch's current account failed — the ONLY thing a rotation request
/// can say.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Reason {
    /// Weekly / monthly / per-model / credits exhaustion. TTL mark.
    UsageLimit,
    /// Claude's rolling 5h session window. TTL mark capped at the window.
    SessionWindow,
    /// 401 / revoked / invalid credential. Permanent mark.
    AuthDead,
    /// Healthy account, no free concurrent-session slot. No mark, swap only.
    ConcurrentSession,
}

impl Reason {
    /// Stable token for logs and the wire.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::UsageLimit => "usage-limit",
            Self::SessionWindow => "session-window",
            Self::AuthDead => "auth-dead",
            Self::ConcurrentSession => "concurrent-session",
        }
    }

    /// The `.bad_tokens` reason text, composed entirely host-side. Each prefix
    /// is chosen so the pool's own classifiers read it the same way they read
    /// the wrapper's direct marks: `auth-dead: … 401 …` is `Auth` (permanent),
    /// `exhausted: …` is `Exhaustion` (TTL), and only the session-window text
    /// contains "session limit" (the 5h cap, #7522). `None` = no mark.
    fn mark_text(self, launch_id: &str) -> Option<String> {
        let what = match self {
            Self::UsageLimit => "exhausted: usage limit",
            Self::SessionWindow => "exhausted: hit your session limit",
            Self::AuthDead => "auth-dead: 401/invalid credential",
            Self::ConcurrentSession => return None,
        };
        Some(format!("{what} (egress-proxy rotation, launch {launch_id})"))
    }

    fn has_evidence(self, evidence: super::registry::Evidence) -> bool {
        match self {
            Self::AuthDead => evidence.auth_failure,
            Self::UsageLimit | Self::SessionWindow => evidence.refused,
            Self::ConcurrentSession => true,
        }
    }
}

/// A parsed control request. `deny_unknown_fields` is the AC2 boundary: a
/// body that tries to carry an `account`, `credential`, `upstream`, `model`
/// or anything else is a 400, not a silently ignored extra.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RotateRequest {
    pub reason: Reason,
    /// Narrow an exhaustion mark to the model class the HOST says this launch
    /// runs (#8058). Ignored for `auth-dead` (an auth death is account-wide).
    #[serde(default)]
    pub model_scoped: bool,
}

impl RotateRequest {
    /// Strict parse. Never echoes the body.
    ///
    /// Only a JSON object is accepted: serde's derived struct deserializer
    /// would otherwise also take the positional array form
    /// (`["usage-limit"]`), which `deny_unknown_fields` does not police.
    pub fn parse(body: &[u8]) -> Result<Self, ControlRefusal> {
        if body.len() > MAX_CONTROL_BODY {
            return Err(ControlRefusal::PayloadTooLarge);
        }
        match serde_json::from_slice::<serde_json::Value>(body) {
            Ok(value @ serde_json::Value::Object(_)) => {
                serde_json::from_value(value).map_err(|_| ControlRefusal::BadRequest)
            }
            _ => Err(ControlRefusal::BadRequest),
        }
    }
}

/// Why a control request was not honored. Like [`Refusal`], no variant ever
/// carries anything the request presented.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlRefusal {
    /// The placeholder did not authorize (same semantics as a proxied call).
    Auth(Refusal),
    NotFound,
    MethodNotAllowed,
    BadRequest,
    PayloadTooLarge,
    /// This launch has no host-side pool or no known account to rotate.
    RotationUnavailable,
    /// The upstream never refused the current credential in the way `reason`
    /// claims, so nothing is marked.
    NoUpstreamEvidence,
    /// The per-launch swap cap is spent.
    RotationLimit,
    /// No eligible account is left in the host pool.
    PoolExhausted,
}

impl ControlRefusal {
    #[must_use]
    pub fn status(self) -> u16 {
        match self {
            Self::Auth(refusal) => refusal.status().0,
            Self::NotFound => 404,
            Self::MethodNotAllowed => 405,
            Self::BadRequest => 400,
            Self::PayloadTooLarge => 413,
            Self::RotationUnavailable | Self::NoUpstreamEvidence => 409,
            Self::RotationLimit => 429,
            Self::PoolExhausted => 503,
        }
    }

    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::Auth(refusal) => refusal.token(),
            Self::NotFound => "not_found",
            Self::MethodNotAllowed => "method_not_allowed",
            Self::BadRequest => "bad_request",
            Self::PayloadTooLarge => "payload_too_large",
            Self::RotationUnavailable => "rotation_unavailable",
            Self::NoUpstreamEvidence => "no_upstream_evidence",
            Self::RotationLimit => "rotation_limit",
            Self::PoolExhausted => "pool_exhausted",
        }
    }
}

/// A freshly selected account. `Debug` redacts the credential.
pub struct Selected {
    pub name: String,
    credential: String,
}

impl Selected {
    #[must_use]
    pub fn new(name: impl Into<String>, credential: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            credential: credential.into(),
        }
    }
}

impl std::fmt::Debug for Selected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Selected")
            .field("name", &self.name)
            .field("credential", &"<redacted>")
            .finish()
    }
}

/// The host pool a rotation marks and selects against. A trait so the
/// trust-boundary tests can count calls without a filesystem, while the
/// production [`HostPool`] wraps the exact primitives `tokens mark-bad` /
/// `tokens select` use.
pub trait AccountPool: Send + Sync {
    /// Append a `.bad_tokens` entry for `account`.
    fn mark_bad(&self, account: &str, reason: &str, model_scoped: bool) -> Result<(), String>;
    /// Select an eligible account (the just-marked one is no longer eligible).
    fn select(&self) -> Result<Selected, String>;
}

/// The real host pool: `<workspace>/.loom/tokens`, falling back to the shared
/// pool exactly as `tokens select --workspace` does.
pub struct HostPool {
    pub workspace: PathBuf,
    /// The host's `LOOM_MODEL`: the class a scoped mark narrows to and the
    /// class selection skips marks for (#8058). Never taken from the request.
    pub model: Option<String>,
}

impl AccountPool for HostPool {
    fn mark_bad(&self, account: &str, reason: &str, model_scoped: bool) -> Result<(), String> {
        let model = if model_scoped {
            self.model.as_deref()
        } else {
            None
        };
        crate::tokens_pool::bad_tokens::mark_bad_for_model(&self.workspace, account, reason, model)
    }

    fn select(&self) -> Result<Selected, String> {
        // Role deliberately `None`: prompt-cache affinity names the account
        // being rotated AWAY from (#8146 — the wrapper unsets LOOM_ROLE for the
        // same reason).
        crate::tokens_pool::select::select_token_for_model_and_role(
            &self.workspace,
            None,
            self.model.as_deref(),
            None,
        )
        .map(|sel| Selected::new(sel.name, sel.key))
        .map_err(|e| e.to_string())
    }
}

/// Installed on a [`Registry`] to turn rotation on.
pub struct Control {
    pub pool: Arc<dyn AccountPool>,
    pub max_rotations: u32,
}

/// A completed rotation — non-secret by construction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rotated {
    pub launch_id: String,
    pub from: String,
    pub account: String,
    pub marked: bool,
}

impl Rotated {
    /// Secret-free one-line record for the per-sweep log.
    #[must_use]
    pub fn marker(&self, reason: Reason) -> String {
        format!(
            "# LOOM_EGRESS_PROXY_ROTATION launch={} reason={} from={} to={} marked={}",
            self.launch_id,
            reason.token(),
            self.from,
            self.account,
            self.marked
        )
    }
}

/// Account names cross into the container (response body) and into a shell
/// `eval` there, so only a conservative charset is ever returned.
#[must_use]
pub fn is_account_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'@' | b'+'))
}

impl Registry {
    /// Turn rotation on for this registry. Only the first call has effect.
    pub fn enable_rotation(&self, pool: Arc<dyn AccountPool>, max_rotations: u32) {
        let _ = self.rotation.set(Control {
            pool,
            max_rotations,
        });
    }

    /// Set the host-side account name on every record (one per launch).
    pub fn set_account(&self, account: &str) {
        for record in self.lock().values_mut() {
            record.account = Some(account.to_string());
        }
    }

    /// Bad-mark the current account of the launch `presented` authorizes (per
    /// `request.reason`), select a fresh one and swap it in behind the SAME
    /// placeholder. Blocking (pool I/O): call from `spawn_blocking`.
    pub fn rotate(
        &self,
        presented: &[String],
        request: RotateRequest,
    ) -> Result<Rotated, ControlRefusal> {
        let control = self
            .rotation
            .get()
            .ok_or(ControlRefusal::RotationUnavailable)?;
        let _serial = self
            .rotation_serial
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Snapshot under the map lock; pool I/O happens without it so proxied
        // traffic is never stalled behind a `.bad_tokens` lock.
        let (key, launch_id, from, generation) = {
            let guard = self.lock();
            let (key, record) = presented
                .iter()
                .find_map(|value| guard.get_key_value(value))
                .ok_or(ControlRefusal::Auth(Refusal::UnknownPlaceholder))?;
            if !record.open {
                return Err(ControlRefusal::Auth(Refusal::ClosedLaunch));
            }
            let from = record
                .account
                .clone()
                .ok_or(ControlRefusal::RotationUnavailable)?;
            if record.rotations >= control.max_rotations {
                return Err(ControlRefusal::RotationLimit);
            }
            if !request.reason.has_evidence(record.evidence) {
                return Err(ControlRefusal::NoUpstreamEvidence);
            }
            (key.clone(), record.launch_id.clone(), from, record.generation)
        };

        let marked = match request.reason.mark_text(&launch_id) {
            Some(text) => {
                let scoped = request.model_scoped && request.reason != Reason::AuthDead;
                match control.pool.mark_bad(&from, &text, scoped) {
                    Ok(()) => true,
                    Err(_) => {
                        // Mirrors the wrapper: a failed mark still re-selects.
                        eprintln!(
                            "egress-proxy: could not record '{from}' in .bad_tokens \
                             (launch {launch_id}); re-selecting anyway"
                        );
                        false
                    }
                }
            }
            None => false,
        };

        let selected = control
            .pool
            .select()
            .map_err(|_| ControlRefusal::PoolExhausted)?;
        // A pool entry that is itself a placeholder, or blank, or whose name
        // cannot be returned safely is not an account this proxy will send.
        if selected.credential.trim().is_empty()
            || selected.credential.starts_with(PLACEHOLDER_PREFIX)
            || !is_account_name(&selected.name)
        {
            return Err(ControlRefusal::PoolExhausted);
        }

        let mut guard = self.lock();
        let record = guard
            .get_mut(&key)
            .ok_or(ControlRefusal::Auth(Refusal::UnknownPlaceholder))?;
        if !record.open || record.generation != generation {
            return Err(ControlRefusal::Auth(Refusal::ClosedLaunch));
        }
        record.credential = selected.credential;
        record.account = Some(selected.name.clone());
        record.generation = record.generation.wrapping_add(1);
        record.rotations += 1;
        record.evidence = super::registry::Evidence::default();
        Ok(Rotated {
            launch_id,
            from,
            account: selected.name,
            marked,
        })
    }
}

#[cfg(test)]
#[path = "rotation_tests.rs"]
mod tests;
