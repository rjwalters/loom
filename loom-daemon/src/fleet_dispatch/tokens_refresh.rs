//! The worker's side of the on-demand fleet token probe (#13943, part 3 of
//! #13893): `loom-daemon tokens refresh --fleet`.
//!
//! After an operator buys account resets the fleet should learn of it now,
//! not at the next scheduled probe. This module sends one request to the
//! captain over the fleet-dispatch channel ([`proto::TOKENS_REFRESH_PATH`]);
//! the captain runs its own token-pool probe immediately (concurrent
//! requests coalesce into the one probe) and publishes the result through
//! the heartbeat snapshot (#13942, [`proto::HeartbeatResponse::token_probe`]),
//! and every worker applies it on its next heartbeat (at most
//! [`super::worker::HEARTBEAT_GAP`]).
//!
//! Unlike the dispatch paths this is an operator's explicit command, so it
//! neither consults nor feeds the worker's fail-open hysteresis
//! ([`super::fallback`]): there is nothing to fail open to — a local probe
//! would be a different command pretending to be a fleet refresh — and the
//! failure is reported to the operator directly.

use std::path::Path;

use chrono::{DateTime, Utc};

use super::client::{self, CallError, CallFacts};
use super::proto::{self, TokensRefreshRequest, TokensRefreshResponse};
use super::worker;

/// What `tokens refresh --fleet` got back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FleetRefresh {
    /// The captain ran (or joined) a probe: `probed_at` is the probe
    /// instant the fleet's snapshots now carry, and `coalesced` says
    /// whether a probe was already running and this request joined it.
    Acked {
        probed_at: Option<DateTime<Utc>>,
        coalesced: bool,
    },
    /// No fleet refresh happened — no captain declared, the captain could
    /// not be asked, or its probe failed. `reason` is the one-line why;
    /// nothing was probed locally.
    Refused { reason: String },
}

/// Ask the captain at this host's `fleetDispatch.url` to probe the pool now
/// (#13943). Blocking and bounded by the captain's own probe run: the same
/// safe-from-any-thread contract [`client::post_blocking_probe_wait`] holds.
#[must_use]
pub fn refresh(root: &Path) -> FleetRefresh {
    let Some(url) = worker::host_endpoint(root) else {
        return FleetRefresh::Refused {
            reason: "no captain is declared for this host (fleetDispatch.enabled / \
                     fleetDispatch.url); a fleet refresh needs one — a local probe is `tokens \
                     check --ranking`"
                .to_string(),
        };
    };
    let request = TokensRefreshRequest {
        host_id: worker::host_id(),
    };
    let started = std::time::Instant::now();
    let mut facts = CallFacts {
        endpoint: proto::TOKENS_REFRESH_PATH,
        ..CallFacts::default()
    };
    match client::post_blocking_probe_wait::<_, TokensRefreshResponse>(
        &url,
        proto::TOKENS_REFRESH_PATH,
        &request,
    ) {
        Ok(answer) => {
            facts.status = Some(200);
            facts.server_epoch = Some(answer.server_epoch.clone());
            facts.result = client::RESULT_OK;
            facts.reason = answer.coalesced.then(|| "coalesced".to_string());
            facts.latency = started.elapsed();
            client::record(&facts);
            match answer.error {
                Some(error) => FleetRefresh::Refused {
                    reason: format!("the captain's probe failed: {error}"),
                },
                None => FleetRefresh::Acked {
                    probed_at: answer.probed_at,
                    coalesced: answer.coalesced,
                },
            }
        }
        Err(e) => {
            facts.status = e.status();
            facts.result = client::RESULT_ERROR;
            facts.reason = Some(e.token().to_string());
            facts.latency = started.elapsed();
            client::record(&facts);
            FleetRefresh::Refused {
                reason: match &e {
                    CallError::Status(404) => format!(
                        "the captain at {url} does not serve a token refresh (HTTP 404; it \
                         predates #13943)"
                    ),
                    _ => format!("captain unreachable at {url}: {e}"),
                },
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::test_server::TestServer;
    use super::*;
    use serde_json::json;
    use serial_test::serial;
    use tempfile::tempdir;

    /// Point the worker's host tier at `url` (or nowhere when `url` is
    /// `None`), so [`worker::host_endpoint`] resolves deterministically
    /// whatever an earlier test left in the shared host-root state.
    fn host_tier(url: Option<&str>) -> tempfile::TempDir {
        let host = tempdir().unwrap();
        let path = host.path().join(crate::config_resolver::LOCAL_CONFIG_REL);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let config = match url {
            Some(url) => json!({"fleetDispatch": {"enabled": true, "url": url}}),
            None => json!({}),
        };
        std::fs::write(path, config.to_string()).unwrap();
        worker::set_host_root(host.path());
        host
    }

    /// No captain declared: refused with a one-line reason, and no request
    /// is even attempted (#13943 acceptance).
    #[test]
    #[serial]
    fn without_a_captain_the_refresh_is_refused() {
        let _host = host_tier(None);
        let repo = tempdir().unwrap();
        let FleetRefresh::Refused { reason } = refresh(repo.path()) else {
            panic!("must refuse");
        };
        assert!(reason.contains("no captain"), "{reason}");
        assert!(reason.contains("tokens check --ranking"), "{reason}");
    }

    /// An unreachable captain: refused with the transport failure named,
    /// never a local probe (#13943 acceptance).
    #[test]
    #[serial]
    fn an_unreachable_captain_is_refused_with_its_reason() {
        let _host = host_tier(Some(&TestServer::dead_url()));
        let repo = tempdir().unwrap();
        let FleetRefresh::Refused { reason } = refresh(repo.path()) else {
            panic!("must refuse");
        };
        assert!(reason.contains("unreachable"), "{reason}");
    }

    /// A captain that predates #13943 answers 404: a precise refusal, not
    /// a generic transport line.
    #[test]
    #[serial]
    fn an_old_captain_is_refused_by_name() {
        let old = TestServer::start(|_, _| (404, json!({"server_epoch": "e0", "error": "nf"})));
        let _host = host_tier(Some(&old.url()));
        let repo = tempdir().unwrap();
        let FleetRefresh::Refused { reason } = refresh(repo.path()) else {
            panic!("must refuse");
        };
        assert!(reason.contains("predates #13943"), "{reason}");
    }

    /// The acknowledged probe: one request to the refresh path, carrying
    /// this host's host id, answered with the probe instant.
    #[test]
    #[serial]
    fn an_acknowledged_probe_carries_its_instant() {
        let server = TestServer::start(|path, body| {
            assert_eq!(path, "/v1/tokens-refresh");
            assert_eq!(body["host_id"], crate::host_identity::host_identity());
            (
                200,
                json!({"server_epoch":"e1","probed_at":"2026-11-19T23:30:00Z","coalesced":true}),
            )
        });
        let _host = host_tier(Some(&server.url()));
        let repo = tempdir().unwrap();
        let FleetRefresh::Acked {
            probed_at,
            coalesced,
        } = refresh(repo.path())
        else {
            panic!("must ack");
        };
        assert_eq!(probed_at, Some("2026-11-19T23:30:00Z".parse::<DateTime<Utc>>().unwrap()));
        assert!(coalesced);
        assert_eq!(server.requests().len(), 1, "one request, no retries");
    }

    /// The captain's own probe failure rides the answer: refused with the
    /// captain's reason.
    #[test]
    #[serial]
    fn a_captains_failed_probe_is_refused() {
        let server = TestServer::start(|_, _| {
            (
                200,
                json!({"server_epoch":"e1","coalesced":false,"error":"every account errored"}),
            )
        });
        let _host = host_tier(Some(&server.url()));
        let repo = tempdir().unwrap();
        let FleetRefresh::Refused { reason } = refresh(repo.path()) else {
            panic!("must refuse");
        };
        assert!(reason.contains("every account errored"), "{reason}");
    }
}
