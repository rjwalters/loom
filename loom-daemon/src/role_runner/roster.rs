//! Roster heartbeat task (Issue #7690 Phase A + #7691 Phase B of #6704),
//! split out of `role_runner.rs` (#7852).
//!
//! One per daemon, not per workspace: this publishes/refreshes a SINGLE
//! comment (this host's own) on the fleet-wide roster issue, advertising the
//! union of every registered workspace's shard key this host currently rotates
//! role ticks for, and caches the read-back for both `status` and — with
//! `roster.enabled` — `crate::role_shard::decide`'s fence. It is the ONLY
//! writer of this host's record, and the only producer of the snapshot the
//! fence reads; a host that cannot run this loop therefore self-fences within
//! `ttl` instead of acting on a stale ring.

use super::*;

/// Resolve this host's `serves` digest set: the [`crate::role_shard::hash_key`]
/// digest of every registered workspace's resolved shard key, for every
/// workspace whose role runner is actually enabled (an unregistered /
/// role-runner-off workspace has no rotation for a roster to describe).
///
/// Re-derived every heartbeat cycle rather than cached — the registered-
/// workspace set changes rarely (per the design record's own assumption),
/// and a fresh derivation costs a handful of cheap config reads on a
/// `heartbeatSecs`-scale cadence (default 300s), never on any tick's hot path.
fn resolve_this_host_serves(fallback_root: &Path) -> std::collections::BTreeSet<u64> {
    let registry = WorkspaceRegistry::load_default().unwrap_or_default();
    let roots = registry.effective_roots(fallback_root);
    let mut serves = std::collections::BTreeSet::new();
    for root in &roots {
        let config = read_role_runner_config(root);
        if !resolve_enabled(&config) {
            continue;
        }
        let effective = crate::config_resolver::resolve_effective_config(root);
        let block = crate::config_resolver::get_path(&effective, "autonomous.roleRunner");
        let explicit_key = block
            .and_then(|b| b.get(crate::role_shard::SHARD_KEY_KEY))
            .and_then(serde_json::Value::as_str);
        let resolved = crate::role_shard::resolve_shard_key(root, explicit_key);
        serves.insert(crate::role_shard::hash_key(&resolved.key));
    }
    serves
}

/// This host's opaque roster-publishing id — the same transform lease records
/// use (`opaque_host_id(host_identity())`), unless
/// [`sweep_registry::LEASE_PUBLISH_HOSTNAME_ENV`] opts into publishing the raw
/// hostname instead (kept consistent with every other lease-shaped forge
/// comment this daemon publishes).
fn roster_host_id() -> String {
    let host = sweep_registry::host_identity();
    let raw_opt_in = std::env::var(sweep_registry::LEASE_PUBLISH_HOSTNAME_ENV)
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false);
    if raw_opt_in {
        host
    } else {
        sweep_registry::opaque_host_id(&host)
    }
}

/// Read back every roster-marker comment on `owner/repo#issue` (REST, never
/// GraphQL — Issue #5047). `None` on any transport failure; the caller treats
/// that as "try again next cycle", never as "no comments exist".
fn read_roster_comments(
    gh: &Path,
    cwd: &Path,
    owner: &str,
    repo: &str,
    issue: u32,
) -> Option<Vec<crate::role_shard::roster::RosterComment>> {
    let mut cmd = Command::new(gh);
    cmd.arg("api")
        .arg(format!("repos/{owner}/{repo}/issues/{issue}/comments"))
        .arg("--paginate")
        .arg("--jq")
        .arg(format!(
            r#".[] | select(.body | startswith("{prefix}")) | {{id: .id, created_at: .created_at, updated_at: .updated_at, body: .body}}"#,
            prefix = crate::role_shard::roster::ROSTER_MARKER_PREFIX,
        ));
    cmd.current_dir(cwd);
    // #5401: cross-owner managed repo -> its own owner's installation-token
    // GH_CONFIG_DIR (no-op for single-owner fleets / the root owner).
    crate::credential_preflight::apply_gh_config_for_root(&mut cmd, cwd);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(crate::role_shard::roster::parse_roster_comments_json(&output.stdout))
}

/// `POST` a brand-new roster comment for this host (its first heartbeat ever
/// on this issue). `false` on any failure — best-effort, like every other
/// forge mutation this daemon publishes on its own initiative.
fn create_roster_comment(
    gh: &Path,
    cwd: &Path,
    owner: &str,
    repo: &str,
    issue: u32,
    body: &str,
) -> bool {
    let mut cmd = Command::new(gh);
    cmd.arg("api")
        .arg(format!("repos/{owner}/{repo}/issues/{issue}/comments"))
        .arg("--method")
        .arg("POST")
        .arg("-f")
        .arg(format!("body={body}"));
    cmd.current_dir(cwd);
    crate::credential_preflight::apply_gh_config_for_root(&mut cmd, cwd);
    cmd.stdout(Stdio::null()).stderr(Stdio::piped());
    match cmd.output() {
        Ok(out) if out.status.success() => true,
        Ok(out) => {
            log::warn!(
                "role_runner: roster comment create on {owner}/{repo}#{issue} exited {:?}: {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
            false
        }
        Err(e) => {
            log::warn!("role_runner: roster comment create on {owner}/{repo}#{issue} failed: {e}");
            false
        }
    }
}

/// `DELETE` a roster comment (this host's own stale record, when it is being
/// replaced rather than patched — see
/// [`crate::role_shard::roster::resolve_publish_action`]). `false` on any
/// failure; the caller then keeps the old record and retries next cycle
/// rather than leaving two records for one host.
fn delete_roster_comment(gh: &Path, cwd: &Path, owner: &str, repo: &str, comment_id: u64) -> bool {
    let mut cmd = Command::new(gh);
    cmd.arg("api")
        .arg(format!("repos/{owner}/{repo}/issues/comments/{comment_id}"))
        .arg("--method")
        .arg("DELETE");
    cmd.current_dir(cwd);
    crate::credential_preflight::apply_gh_config_for_root(&mut cmd, cwd);
    cmd.stdout(Stdio::null()).stderr(Stdio::piped());
    match cmd.output() {
        Ok(out) if out.status.success() => true,
        Ok(out) => {
            log::warn!(
                "role_runner: roster comment delete {comment_id} on {owner}/{repo} exited {:?}: {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
            false
        }
        Err(e) => {
            log::warn!(
                "role_runner: roster comment delete {comment_id} on {owner}/{repo} failed: {e}"
            );
            false
        }
    }
}

/// `PATCH` this host's existing roster comment. Idempotent: the body is
/// regenerated wholesale every cycle (see
/// [`crate::role_shard::roster::build_roster_comment_body`]), so the only content
/// this ever needs to preserve is the comment's own `id`.
fn patch_roster_comment(
    gh: &Path,
    cwd: &Path,
    owner: &str,
    repo: &str,
    comment_id: u64,
    body: &str,
) -> bool {
    let mut cmd = Command::new(gh);
    cmd.arg("api")
        .arg(format!("repos/{owner}/{repo}/issues/comments/{comment_id}"))
        .arg("--method")
        .arg("PATCH")
        .arg("-f")
        .arg(format!("body={body}"));
    cmd.current_dir(cwd);
    crate::credential_preflight::apply_gh_config_for_root(&mut cmd, cwd);
    cmd.stdout(Stdio::null()).stderr(Stdio::piped());
    match cmd.output() {
        Ok(out) if out.status.success() => true,
        Ok(out) => {
            log::warn!(
                "role_runner: roster comment patch {comment_id} on {owner}/{repo} exited {:?}: {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
            false
        }
        Err(e) => {
            log::warn!(
                "role_runner: roster comment patch {comment_id} on {owner}/{repo} failed: {e}"
            );
            false
        }
    }
}

/// One heartbeat cycle: read the roster issue's comments, locate (or create)
/// this host's own comment, and refresh it with this host's current `serves`
/// set. Best-effort throughout — like [`sweep_registry`]'s
/// `write_lease_comment`, a `gh` failure here only logs; it must never crash
/// the daemon or block any other loop. Runs on a blocking thread (spawned by
/// the caller via [`tokio::task::spawn_blocking`]).
fn roster_heartbeat_once(
    fallback_root: &Path,
    issue: &crate::role_shard::roster::RosterIssueRef,
    ttl_secs: u64,
    settle_secs: u64,
) {
    let gh = Path::new("gh");
    let host = roster_host_id();
    let serves = resolve_this_host_serves(fallback_root);

    let Some(comments) =
        read_roster_comments(gh, fallback_root, &issue.owner, &issue.repo, issue.number)
    else {
        log::warn!(
            "role_runner: roster heartbeat could not read comments on {} (Issue #6704 Phase A) \
             -- will retry next cycle",
            issue.display()
        );
        return;
    };

    let existing = crate::role_shard::roster::own_comment(&comments, &host);
    let body = crate::role_shard::roster::build_roster_comment_body(&host, &serves);
    // A record is PATCHed in place while it is live and its `serves` set is
    // unchanged; otherwise it is REPLACED so its fresh `created_at` becomes a
    // membership boundary every host observes identically (Issue #7691). See
    // `resolve_publish_action` for why an unfenced membership change is the
    // one thing the generation fence cannot absorb.
    let action = crate::role_shard::roster::resolve_publish_action(
        existing,
        &serves,
        chrono::Utc::now(),
        ttl_secs,
    );
    let ok = match action {
        crate::role_shard::roster::RosterPublish::Patch { id } => {
            patch_roster_comment(gh, fallback_root, &issue.owner, &issue.repo, id, &body)
        }
        crate::role_shard::roster::RosterPublish::Create => {
            create_roster_comment(gh, fallback_root, &issue.owner, &issue.repo, issue.number, &body)
        }
        crate::role_shard::roster::RosterPublish::Republish { id, reason } => {
            log::info!(
                "role_runner: replacing this host's roster record on {} — {} (a fresh created_at \
                 is the membership boundary peers fence on, Issue #6704)",
                issue.display(),
                reason.label(),
            );
            // Delete-then-create, and create EVEN IF the delete failed. The
            // fallback is deliberately not "PATCH it instead": patching
            // resurrects this host into every peer's ring with no boundary
            // and no settle window, so peers would switch rings at whatever
            // instant each happened to read — the two-owner race the fence
            // exists to rule out. A leftover record is harmless by
            // comparison: `members`/`ring` key on the host id (deduped),
            // `own_comment` takes the freshest, and every boundary the
            // orphan contributes is already in the past. It only shows up as
            // an extra EXPIRED row in `status`.
            if !delete_roster_comment(gh, fallback_root, &issue.owner, &issue.repo, id) {
                log::warn!(
                    "role_runner: could not delete this host's stale roster comment {id} on {} — \
                     publishing the replacement anyway and leaving the stale record behind \
                     (correct fencing matters more; delete it by hand to tidy up)",
                    issue.display(),
                );
            }
            create_roster_comment(gh, fallback_root, &issue.owner, &issue.repo, issue.number, &body)
        }
    };
    if !ok {
        log::warn!(
            "role_runner: roster heartbeat failed to publish this host's comment on {} (Issue \
             #6704 Phase A)",
            issue.display()
        );
        return;
    }

    // Re-read so the `status` cache below is at most one heartbeat interval
    // stale, never the pre-publish view (in particular, so a brand-new host's
    // very first heartbeat is immediately visible in its own `status`).
    if let Some(fresh) =
        read_roster_comments(gh, fallback_root, &issue.owner, &issue.repo, issue.number)
    {
        crate::role_shard::roster::set_roster_snapshot(crate::role_shard::roster::RosterSnapshot {
            issue: issue.clone(),
            host,
            comments: fresh,
            ttl_secs,
            settle_secs,
            fetched_at: chrono::Utc::now(),
        });
    }
}

/// Spawn the per-daemon roster heartbeat loop, gated on
/// `autonomous.roleRunner.roster.enabled` resolved from `fallback_root` (the
/// daemon's primary/default workspace — the roster is a fleet-wide, host-level
/// concern, not a per-workspace one, exactly like the static shard posture
/// [`crate::ipc::build_daemon_status`] resolves once from the same root).
///
/// Returns `None` (spawns nothing, makes zero forge calls) when the roster is
/// disabled — the default — or misconfigured (`enabled: true` with no valid
/// `roster.issue`, which additionally logs one `error!` here so the
/// misconfiguration is never silent).
pub fn spawn_roster_heartbeat_task(fallback_root: PathBuf) -> Option<tokio::task::JoinHandle<()>> {
    let config = crate::role_shard::roster::resolve_roster_config(&fallback_root);
    match config.state.clone() {
        crate::role_shard::roster::RosterState::Disabled => {
            log::debug!(
                "role_runner: roster heartbeat disabled (set {}=1 and {} to enable, Issue #6704 \
                 Phase A)",
                crate::role_shard::roster::ROSTER_ENABLED_ENV,
                crate::role_shard::roster::ROSTER_ISSUE_ENV,
            );
            None
        }
        crate::role_shard::roster::RosterState::MisconfiguredNoIssue => {
            log::error!(
                "role_runner: autonomous.roleRunner.roster.enabled=true but no valid `issue` \
                 (owner/repo#N) is configured via autonomous.roleRunner.roster.issue / {} -- \
                 roster disabled (Issue #6704 Phase A)",
                crate::role_shard::roster::ROSTER_ISSUE_ENV,
            );
            None
        }
        crate::role_shard::roster::RosterState::Active(issue) => {
            let interval = Duration::from_secs(config.heartbeat_secs.max(1));
            let ttl_secs = config.ttl_secs;
            let settle_secs = config.settle_secs;
            log::info!(
                "role_runner: roster heartbeat enabled -- publishing to {} every {}s (ttl={}s, \
                 settle={}s; Issue #6704). This host's role-runner ring is now derived from the \
                 live roster and fenced by generation+settle: a dead host's slice is reassigned \
                 within ttl+settle+one interval, and any membership disagreement YIELDS role \
                 ticks here rather than duplicating them. Set {}/{} to pin a static ring instead.",
                issue.display(),
                config.heartbeat_secs,
                ttl_secs,
                settle_secs,
                crate::role_shard::SHARD_INDEX_ENV,
                crate::role_shard::SHARD_COUNT_ENV,
            );
            Some(tokio::spawn(async move {
                loop {
                    let root = fallback_root.clone();
                    let issue = issue.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        roster_heartbeat_once(&root, &issue, ttl_secs, settle_secs);
                    })
                    .await;
                    tokio::time::sleep(interval).await;
                }
            }))
        }
    }
}
