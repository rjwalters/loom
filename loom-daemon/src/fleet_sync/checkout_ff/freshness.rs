//! `checkout.freshness` (#12509): one OTLP-only record per managed checkout per
//! fast-forward pass, saying how far the checkout is behind GitHub's tip.
//!
//! The pass has just learned (or reused) the remote head for the checkout, so
//! this adds **no network call** and writes nothing to `.git`: it reads the
//! prompt tree with local `git` children only, and compares it with the head
//! the pass already holds. A pass that could not learn the head says why in
//! `pass_skipped_reason`, so a stale count never looks fresh. Built only when
//! the exporter is configured.

use std::path::Path;

use chrono::{DateTime, Utc};

use super::{Env, Net, UNREACHABLE_STOP};
use crate::role_tick_telemetry::prompt_tree::{self, remote};
use crate::telemetry::kinds::checkout_freshness::CheckoutFreshnessRecord;
use crate::telemetry::TelemetryRecord;

/// Why a remote tip could not be learned for a checkout this pass, in the order
/// the pass would have hit it. `None`: nothing to explain.
fn skipped_reason(env: &Env<'_>, root: &Path, net: &Net, low_disk: bool) -> Option<&'static str> {
    if low_disk {
        Some("low-disk")
    } else if !env.network {
        Some("network-off")
    } else if (env.backing_off)(root) {
        Some("backing-off")
    } else if net.breaker_open {
        Some("breaker-open")
    } else if net.misses >= UNREACHABLE_STOP {
        Some("unreachable-stop")
    } else {
        Some("no-head")
    }
}

/// The head the workspace half heard for `root`'s `branch` within
/// [`remote::HEARD_MAX_AGE`], with its age in seconds: a cache read, so a pass
/// that may not use the network can still name a recent tip (#12565).
fn cached_head(root: &Path, branch: Option<&str>) -> Option<(String, String, u64)> {
    let branch = branch?;
    super::heard_within_aged(root, remote::HEARD_MAX_AGE).filter(|(b, _, _)| b == branch)
}

/// The record for one checkout this pass reached. `head` is the remote head the
/// pass learned for it, `branch` the default branch it resolved.
pub(super) fn reached(
    env: &Env<'_>,
    root: &Path,
    branch: Option<&str>,
    head: Option<&str>,
    low_disk: bool,
    net: &Net,
    now: DateTime<Utc>,
) -> CheckoutFreshnessRecord {
    let mut prompt = prompt_tree::read(root);
    let mut repo = None;
    let mut tip_age_s = None;
    if let Some(tree) = prompt.as_mut() {
        let expected = remote::Expected::for_root(root);
        let cached = head.is_none().then(|| cached_head(root, branch)).flatten();
        let heard = branch
            .zip(head)
            .or_else(|| cached.as_ref().map(|(b, h, _)| (b.as_str(), h.as_str())));
        remote::fill(root, tree, &expected, heard);
        if tree.remote_tip.is_some() {
            tip_age_s = cached.map(|(_, _, age)| age);
        }
        repo = tree
            .remote_repo
            .clone()
            .filter(|_| tree.remote_foreign != Some(true));
    }
    let known = prompt.as_ref().is_some_and(|t| t.remote_tip.is_some());
    let foreign = prompt
        .as_ref()
        .is_some_and(|t| t.remote_foreign == Some(true));
    let reason = if low_disk || (!known && !foreign) {
        skipped_reason(env, root, net, low_disk)
    } else {
        None
    };
    CheckoutFreshnessRecord {
        captured_at: now,
        repo,
        pass_skipped_reason: reason.map(str::to_string),
        remote_tip_age_s: tip_age_s,
        prompt,
    }
}

/// The record for a checkout the pass ran out of budget before reaching: no
/// prompt tree, only the repo (one local read) and the reason.
pub(super) fn deferred(root: &Path, now: DateTime<Utc>) -> CheckoutFreshnessRecord {
    let expected = remote::Expected::for_root(root);
    let url = prompt_tree::git_remote_url(root).unwrap_or_default();
    let origin = remote::classify_origin(&url, &expected);
    CheckoutFreshnessRecord {
        captured_at: now,
        repo: origin.repo.filter(|_| !origin.foreign),
        pass_skipped_reason: Some("budget".to_string()),
        remote_tip_age_s: None,
        prompt: None,
    }
}

/// Emit `record` through the daemon's OTLP exporter.
pub(super) fn publish(record: CheckoutFreshnessRecord) {
    crate::observability::ops::emit_record(TelemetryRecord::CheckoutFreshness(record));
}
