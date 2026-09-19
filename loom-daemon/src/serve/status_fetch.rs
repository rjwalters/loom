//! The dashboard's `DaemonStatus` round-trip, and the root-scaled budget it
//! waits under (Issue #8224).
//!
//! # Why this is not just [`super::FETCH_TIMEOUT`]
//!
//! [`crate::ipc::build_daemon_status`] walks **every registered workspace
//! root** on every round-trip, so the reply this function waits for costs
//! `O(roots)`. A fixed `5s` budget is blind to that: on the same many-root
//! host #8163 measured `13.1s`/`14.3s` builds on, `/api/status` and
//! `/api/health` returned `503 daemon unreachable` against a demonstrably
//! healthy daemon — the dashboard-side face of the false `indeterminate-busy`
//! verdict #8163 fixed for `loom-daemon health`, and the same class of bug
//! #8224 fixes for `loom-daemon status`.
//!
//! The floor is applied with [`status_budget::apply_client_probe_floor`],
//! the same raise-only combinator `cli::health::resolve_retry_timeout` and
//! `cli::status::resolve_status_timeout` use, so all three clients derive from
//! one cost model instead of three constants. At `root_count == 1` the budget
//! is `1.4s` — under `FETCH_TIMEOUT` — so a single-workspace host is
//! bit-for-bit unchanged.
//!
//! # Why [`super::open_event_subscription`] is deliberately NOT scaled
//!
//! It is the other [`super::FETCH_TIMEOUT`] user, and it is left alone on
//! purpose rather than by omission (#8224 asks for this to be decided, not
//! skipped). Its bounded region is `connect` + one `SubscribeEvents` request
//! write — it never awaits a response inside the timeout, and
//! `SubscribeEvents` is intercepted in `ipc::handle_client` *before* any
//! request dispatch, so it never reaches `build_daemon_status`. Nor can a
//! concurrent slow build delay it: the IPC accept loop `tokio::spawn`s every
//! connection, so a `status` build in flight on another connection does not
//! block a new `connect`. The cost is `O(1)` in the root count, which makes a
//! root-scaled floor there pure noise — and a *harmful* kind of noise, since
//! widening it would slow the 503 an unreachable daemon should return fast.
//! If that region ever grows a response await, it should be revisited.

use anyhow::{anyhow, Result};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::status_budget;
use crate::types::{DaemonStatusReport, Request, Response};

/// The budget [`fetch_report`] waits under on a host with `root_count`
/// registered workspace roots: [`super::FETCH_TIMEOUT`] as a floor, raised by
/// the `O(roots)` cost of the [`crate::ipc::build_daemon_status`] it is
/// waiting for (Issue #8224).
///
/// Split out of [`fetch_report`] purely so the decision is unit-testable
/// without a socket — the same reason `cli::health::resolve_retry_timeout`
/// exists separately from `cli::health::query_status`.
#[must_use]
pub(super) fn fetch_budget(root_count: usize) -> Duration {
    status_budget::apply_client_probe_floor(super::FETCH_TIMEOUT, root_count)
}

/// Fetch the live [`DaemonStatusReport`] from the running daemon over its
/// Unix socket — the exact same `Request::DaemonStatus` request
/// `loom-daemon status --json` sends, so the aggregation itself (dynamic
/// caps, per-repo breakdown, capacity, drain state, …) is computed exactly
/// once, in [`crate::ipc::build_daemon_status`], never re-derived here.
///
/// Bounded at [`super::FETCH_TIMEOUT`] raised by this host's registered root
/// count — see the module docs for why.
pub(super) async fn fetch_report(socket_path: &Path) -> Result<DaemonStatusReport> {
    let roundtrip = async {
        let stream = UnixStream::connect(socket_path)
            .await
            .map_err(|e| anyhow!("connect to daemon socket failed: {e}"))?;
        let (reader, mut writer) = stream.into_split();

        let request_json = serde_json::to_string(&Request::DaemonStatus)?;
        writer.write_all(request_json.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;

        let mut lines = BufReader::new(reader).lines();
        let line = lines
            .next_line()
            .await?
            .ok_or_else(|| anyhow!("daemon closed the connection without responding"))?;
        let response: Response = serde_json::from_str(&line)?;
        match response {
            Response::DaemonStatus(report) => Ok(*report),
            Response::Error { message } => Err(anyhow!("daemon error: {message}")),
            other => Err(anyhow!("unexpected response: {other:?}")),
        }
    };

    // Read from the LOCAL workspace registry, never from the daemon: the
    // client cannot learn the root count from the very round-trip it is
    // trying to budget for.
    let root_count = status_budget::registered_root_count();
    let budget = fetch_budget(root_count);
    tokio::time::timeout(budget, roundtrip).await.map_err(|_| {
        anyhow!(
            "status round-trip timed out after {}s ({root_count} registered workspace root(s))",
            budget.as_secs()
        )
    })?
}

#[cfg(test)]
mod tests;
