//! `loom-daemon health` — the I/O half of the one-shot fleet-vitals command
//! (Issue #4761).
//!
//! Everything opinionated (every verdict rule, the #4694 liveness precedence,
//! the transient-vs-persistent role classifier, the exit-code contract) lives
//! in the pure [`loom_daemon::health`] collector. This module only *collects*:
//! one IPC round-trip, one local install-state probe, one `pgrep`, one
//! `.ranking` stat, one bounded forge fan-out, and one best-effort
//! calibration read — then hands the result to
//! [`loom_daemon::health::assess`] and renders.
//!
//! # Why every probe here is best-effort
//!
//! A health command that fails to produce a report is worse than useless to
//! the watch loop it exists for. Every collection step degrades to "could not
//! determine" (which the collector renders as a non-green `UNKNOWN` section,
//! exit `1`) rather than aborting — the *only* thing that can stop this
//! command from printing a report is a panic.
//!
//! # Daemon-authoritative vs. caller-process sections (#5061)
//!
//! Not every section's verdict is trustworthy from the same vantage point,
//! which matters most exactly when this command is run somewhere other than
//! the machine that ran `loom-daemon health`'s writer (an SSH probe of a
//! remote fleet host, a non-login shell whose `PATH` differs from an
//! interactive login shell's):
//!
//! - **`liveness`, `dispatch`, `tokens`, `roles`, `observability`** are
//!   **daemon-authoritative** — every fact comes from the daemon's own IPC
//!   round-trip ([`DaemonStatusReport`]) or a local probe of *this host's*
//!   process table/filesystem, never a forge call made by this CLI process.
//!   A verdict here reflects the daemon's own state, not this caller's
//!   environment, so it is trustworthy the same way over SSH as locally.
//! - **`queues`, `throughput`, `operator_attention`** execute `gh` calls **in
//!   this CLI process** ([`pipeline_snapshot::GhPipelineSource`]), scoped to
//!   whatever `gh` resolves to and however it is authenticated *here* — which
//!   can differ from the daemon's own (already-verified, see
//!   `credential_preflight` in `status`/`--json`) forge credential. A
//!   missing/non-executable `gh` in *this* process (the common case: a
//!   non-login SSH shell whose `PATH` lacks `~/.local/bin` /
//!   `/opt/homebrew/bin`, #4875's failure class) is reported as a single
//!   distinct fact rather than a per-repo forge-query failure, and
//!   cross-references the daemon's own `credential_preflight` verdict when it
//!   is available — see `health::assess_queues` / `assess_throughput` /
//!   `gh_unavailable_section`. `operator_attention` (#8091) is always
//!   `Verdict::Green` regardless, so a missing `gh` there changes the
//!   rendered text but never the exit code.
//!
//! # `limit_calibration` (#8063, rewired by #8349): collected here, assessed
//! in `health.rs`
//!
//! The calibration reading is collected by this module and *assessed* behind
//! [`health::HealthInputs::limit_calibration`] +
//! [`health::assess_limit_calibration`] (which lives in the
//! `health::calibration_section` sibling module — the file-size-ratchet split
//! every growing `assess_*` function follows). #8366 originally appended the
//! section here instead; #8349 moved it into `assess`'s own roll-up so a
//! `Degraded` calibration verdict promotes `overall` through the same logic
//! every other section uses.
//!
//! The collection itself is
//! [`loom_daemon::limit_calibration::compute_with_fallback`]: claude-monitor's
//! `usage_history` where that companion is installed, else #8347's persisted
//! `weekly_point_samples` joined against #8062's daily cost-equivalent by
//! #8348's pure `calibrate()` — one more best-effort *local* input, never
//! part of the IPC round-trip, degrading to `None` ("not collected") when
//! neither source is readable.
//!
//! It is **conditional**, the same way [`health::assess_observability`]
//! (#4830) is: no line at all renders when the signal is simply not
//! configured on this host (claude-monitor absent *and* no readable
//! sample/cost history). A Loom install with neither would otherwise carry a
//! permanent non-green `limit_calibration` line reporting nothing but its own
//! absence, which is exactly the noise that convention exists to prevent. The
//! consequence is that this section **never** emits
//! [`health::Verdict::Unknown`]: every state it can be in is either a real
//! reading (`Green`/`Degraded`) or no section, so a reader never sees a
//! non-green section line sitting under a `Green` overall.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use loom_daemon::activity::transcript_ingest;
use loom_daemon::daemon_install_state;
use loom_daemon::daemon_pidfile;
use loom_daemon::health::{self, HealthInputs, HealthReport};
use loom_daemon::limit_calibration::{self, CalibrationStatus};
use loom_daemon::pipeline_snapshot::{self, GhPipelineSource, PipelineMetrics};
use loom_daemon::types::{DaemonStatusReport, Request, Response};

use super::common::{query_daemon_bounded, resolve_socket_path};

/// Base per-attempt bound on the IPC round-trip on an *unloaded* host —
/// unchanged from before Issue #6103: `health` still advertises a "< 5s
/// typical" total budget across *all* sections on a quiet host, and a daemon
/// that cannot answer within 2s there is already a finding worth reporting
/// (as `alive-but-unresponsive`) rather than waiting on.
///
/// # This is no longer the whole story (#6103)
///
/// On a *busy* host this fixed 2s budget used to disagree with `status`'s own
/// load-scaled 5-30s budget ([`super::status::resolve_status_timeout`]) and
/// the watchdog's 15s-per-tick / 3-consecutive-failure budget
/// (`loom-daemon-watchdog.sh`'s `PROBE_TIMEOUT_SECS` /
/// `LOOM_WATCHDOG_IPC_PROBE_FAIL_THRESHOLD`) — so `health` alone flagged
/// `overall DEGRADED`/exit `1` against a daemon 29 straight watchdog ticks
/// (and 5/5 immediate manual IPC probes) confirmed was healthy. Reconciled
/// without simply raising this number (which only narrows the false-alarm
/// window, never closes it):
///
/// 1. [`resolve_ipc_timeout`] scales this base by observed host load — the
///    same [`super::status::scale_timeout_for_load`] rule `status` uses — and
///    honors the shared `LOOM_DAEMON_IPC_TIMEOUT_MS` floor
///    ([`super::common::apply_ipc_timeout_env_floor`]), so the two commands
///    can no longer silently disagree about the same busy host.
/// 2. [`query_status`] retries **exactly once** on a *timeout*-classified
///    failure (never a hard one) before ever reporting a failure at all — the
///    single-invocation analog of the watchdog's consecutive-failure
///    debounce, via [`loom_daemon::health::ipc_error_is_probe_timeout`].
/// 3. If both attempts still fail, [`loom_daemon::health::assess_liveness`]
///    reports a lone surviving timeout against a demonstrably-alive daemon as
///    `Verdict::Unknown` ("probe budget exceeded"), not `Verdict::Degraded`
///    ("confirmed unhealthy") — so it does not, by itself, flip `overall` to
///    DEGRADED.
///
/// # Still not the whole story either (#6191)
///
/// #6103 above stopped a lone timeout from being *mislabeled* as a confirmed
/// fault, but every retry still used the same short budget, and the resulting
/// all-`Unknown` report still exited `1` — the same code a genuine
/// degradation uses. Reconciled two more ways, both gated on
/// [`loom_daemon::health::alive_with_fresh_heartbeat`] (a signal collected
/// entirely without IPC, so it costs nothing extra to consult):
///
/// 4. [`query_status`]'s retry uses an **escalated** budget
///    ([`ESCALATED_IPC_TIMEOUT`]) instead of repeating [`resolve_ipc_timeout`]
///    verbatim, when local evidence already corroborates the process as alive
///    with a fresh heartbeat — worth waiting a little longer for, rather than
///    giving up at the same short budget a second time.
/// 5. If the escalated retry also fails, [`loom_daemon::health::assess`]'s
///    roll-up reports `overall` as the distinct `Verdict::IndeterminateBusy`
///    ("busy, not confirmed unhealthy") rather than the ordinary
///    `Verdict::Unknown`, at its own exit code
///    ([`loom_daemon::health::EXIT_INDETERMINATE_BUSY`]) — so a watch loop can
///    tell "try again shortly" apart from "alert" without parsing `--json`.
///
/// # The budget has to fit the work (#8163)
///
/// Both #6103's load scaling and #6191's escalation are keyed on **host
/// load**; neither accounts for the fact that the daemon-side work being
/// waited on ([`loom_daemon::ipc::build_daemon_status`]) is `O(registered
/// workspace roots)`. On a many-workspace host that build outran even the
/// escalated `10s` budget on every call, so `health` reported
/// `indeterminate-busy` on an idle daemon and every section downstream of
/// liveness came back `unknown`:
///
/// 6. [`resolve_retry_timeout`] takes the **larger** of #6191's fixed floor
///    and [`loom_daemon::status_budget::client_probe_budget`], derived from
///    the registered root count read locally before the round-trip. The
///    *first* attempt is deliberately left short (this function's own
///    load-scaled base): a fast miss against a genuinely wedged daemon is
///    still reported fast, and the wider budget is only spent once local
///    evidence says the daemon is alive and worth waiting for.
const BASE_IPC_TIMEOUT: Duration = Duration::from_secs(2);

/// The **floor** under the escalated per-attempt budget [`query_status`]
/// retries at when local, no-IPC evidence already corroborates the daemon as
/// alive with a fresh heartbeat (Issue #6191) — worth waiting longer for
/// rather than repeating [`resolve_ipc_timeout`]'s short budget a second
/// time. `10s` matches #6191's own worked example (`2s -> 10s`).
///
/// # A floor, not the budget (#8163)
///
/// This was the whole escalated budget until #8163, which is exactly why
/// `health` broke on many-workspace hosts: the daemon's
/// [`loom_daemon::ipc::build_daemon_status`] is `O(registered roots)` and
/// measured `13.1s`/`14.3s` on a host with several dozen of them, so both
/// attempts timed out and `health` reported `indeterminate-busy` (exit `3`)
/// on an idle, healthy daemon. [`resolve_retry_timeout`] now takes the
/// **larger** of this floor and
/// [`loom_daemon::status_budget::client_probe_budget`], which is derived from
/// the registered root count — so a single-workspace host keeps this exact
/// `10s` behaviour and a many-workspace host gets a budget that can actually
/// cover the build it is waiting on.
const ESCALATED_IPC_TIMEOUT: Duration = Duration::from_secs(10);

/// Resolve the effective per-attempt IPC timeout for this invocation (#6103
/// AC1): [`BASE_IPC_TIMEOUT`] scaled by observed host load via
/// [`super::status::scale_timeout_for_load`] (the identical rule `status`
/// applies to its own IPC budget), then floored — never lowered — by the
/// shared `LOOM_DAEMON_IPC_TIMEOUT_MS` override.
fn resolve_ipc_timeout() -> Duration {
    let logical_cpus = loom_daemon::cpu_headroom::logical_cpu_count();
    let loadavg_1m = loom_daemon::cpu_headroom::read_loadavg_1m();
    let load_per_core = loom_daemon::cpu_headroom::load_per_core_from(loadavg_1m, logical_cpus);
    resolve_ipc_timeout_for_load(load_per_core)
}

/// The pure core of [`resolve_ipc_timeout`], with the host-load term
/// **injected** rather than read (#6625).
///
/// Split out so the env-floor rule can be tested against a *known* load
/// instead of whatever `/proc/loadavg` happens to say. `/proc/loadavg` is not
/// namespaced: a container on a busy shared host reads the whole host's
/// 1-minute load against only its own visible cores, so the load-scaled term
/// can win the `max` against a test's env floor and turn an equality
/// assertion into a measurement of the *host*, not of this function. Reading
/// the load stays in [`resolve_ipc_timeout`]; the rule lives here.
fn resolve_ipc_timeout_for_load(load_per_core: Option<f64>) -> Duration {
    let scaled = super::status::scale_timeout_for_load(BASE_IPC_TIMEOUT, load_per_core);
    super::common::apply_ipc_timeout_env_floor(scaled)
}

/// Handle `loom-daemon health [--since 30m] [--json]`.
///
/// Never returns `Err` for a *health* problem — an unhealthy fleet is a
/// successful report with a non-zero exit code. `Err` is reserved for the
/// command being unable to run at all (e.g. an unparseable `--since`).
pub(crate) async fn handle_health_command(since: Option<String>, json: bool) -> Result<()> {
    let window = match since.as_deref() {
        Some(raw) => health::parse_since(raw).map_err(|e| anyhow::anyhow!(e))?,
        None => Duration::from_secs(health::DEFAULT_WINDOW_SECS),
    };

    let report = collect(window).await;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", report.render_human());
    }
    std::io::Write::flush(&mut std::io::stdout()).ok();
    std::process::exit(report.exit_code());
}

/// Collect every input and assess. Split out of [`handle_health_command`] so
/// the exit-code path is the only thing that call site adds.
async fn collect(window: Duration) -> HealthReport {
    // 1. Local liveness probes FIRST (Issue #6191). Both are cheap, bounded,
    //    and entirely independent of the IPC round-trip below — moved ahead
    //    of it (previously step 2) so their result can inform *how hard* the
    //    upcoming IPC attempt is worth retrying: process alive + heartbeat
    //    fresh ([`health::alive_with_fresh_heartbeat`]) is corroborating
    //    evidence that an escalated retry is likely to succeed rather than
    //    merely repeat the same short-budget miss. Recorded either way so a
    //    `--json` consumer can see the corroborating evidence regardless of
    //    whether IPC succeeded.
    let install_state = daemon_install_state::probe();
    let pgrep_pids = daemon_install_state::pgrep_daemon_pids();
    let escalate_on_timeout = health::alive_with_fresh_heartbeat(install_state.as_ref());
    // #8163: read the host load BEFORE the IPC attempt, so the reading
    // describes the host the probe actually ran against rather than whatever
    // it settled to afterwards. Corroborates (or refutes) the
    // `indeterminate-busy` roll-up — see `health::busy`.
    let load_per_core = loom_daemon::cpu_headroom::load_per_core();

    // 2. The IPC round-trip — the only source for the dispatch/tokens/roles
    //    sections, and the strongest liveness signal there is.
    let (status, ipc_error) = query_status(escalate_on_timeout).await;

    // 2b. The pid file, observed against the path the DAEMON resolved (#4774)
    //     when it answered — same rule as `.ranking` below and `status`'s token
    //     probe (#4292): never re-derive from this CLI process's own cwd/env
    //     when the daemon has told us which file it actually writes. Falling
    //     back to a local resolution only for an unreachable / pre-#4774
    //     daemon. Observed unconditionally so `--json` carries the corroborating
    //     evidence either way; the collector decides what it means.
    let pid_file = status
        .as_ref()
        .and_then(|r| r.pid_file.clone())
        .or_else(daemon_pidfile::resolve_pid_file_path)
        .map(|path| daemon_pidfile::observe(&path));

    // 3. `.ranking` staleness, against the pool directory the DAEMON resolved
    //    (#4292) rather than one re-derived from this CLI process's own cwd —
    //    the same rule `status`'s client-side token probe follows.
    let (ranking_present, ranking_age_secs) = probe_ranking(status.as_ref());

    // 3b. Per-model-class healthy counts for that same pool (#8058 Phase 3).
    //     One more read of the directory step 3 already resolved, never a new
    //     probe — so the per-class breakdown and the `.ranking` staleness it
    //     is printed next to can never be scoped to different pools.
    let token_class_capacity = probe_class_capacity(status.as_ref());

    // 4. The forge fan-out for queue depth + review pipeline + throughput.
    //    Only the metrics those sections actually read (#4761's
    //    `PipelineMetrics::HEALTH`, widened by #5021 to carry the review-side
    //    axes the `queues` verdict now consumes), over the requested window,
    //    across the roots the daemon reported. Skipped entirely when the daemon
    //    is unreachable: without its root list there is nothing to query, and
    //    the sections honestly report "not collected".
    //
    //    4a. Before fanning out to N repos, check ONCE whether this process
    //    can even run `gh` at all (#5061). A missing/non-executable `gh` — the
    //    common case being a non-login SSH shell whose PATH lacks
    //    `~/.local/bin` / `/opt/homebrew/bin` (the same failure class as
    //    #4875) — would otherwise fail identically for every managed repo,
    //    rendering as "forge query FAILED for: <every repo>" and reading like
    //    a forge outage rather than what it actually is: a fact about this
    //    caller's own environment. `Some` here means the fan-out below is
    //    skipped entirely (there is no value in spawning N `gh` calls already
    //    known to fail the same way), and `queues`/`throughput` render the
    //    single fact instead — see `health::assess_queues`/`assess_throughput`.
    let gh_unavailable =
        pipeline_snapshot::probe_gh_availability(Path::new(pipeline_snapshot::DEFAULT_GH_BIN))
            .err();
    let pipeline = match (&status, &gh_unavailable) {
        (Some(report), None) => {
            let roots = report
                .per_repo
                .iter()
                .map(|r| r.root.clone())
                .collect::<Vec<_>>();
            let source = Arc::new(
                GhPipelineSource::new()
                    .with_metrics(PipelineMetrics::HEALTH)
                    .with_merge_window(
                        chrono::Duration::from_std(window)
                            .unwrap_or_else(|_| chrono::Duration::hours(24)),
                    ),
            );
            Some(pipeline_snapshot::collect_pipeline_snapshots(source, roots).await)
        }
        _ => None,
    };

    // 5. The daemon log's newest `work_finder:` line (#4824) — a *corroborating*
    //    signal, never a derivation: it exists so the collector refuses to call
    //    the work finder dead while the daemon's own log shows it ticking. One
    //    bounded tail read, and only on the path that can consume it (the
    //    daemon reported no tick) — a reported tick is already the stronger
    //    signal, so probing then would be pure I/O for a field nothing reads.
    let work_finder_log_tick_age_secs = status
        .as_ref()
        .is_some_and(|r| r.last_work_finder_tick.is_none())
        .then(health::probe_work_finder_log_tick_age)
        .flatten();

    // 6. This CLI process's own read-only source-vs-built-commit comparison
    //    (Issue #6261) — the same `loom_daemon::self_update::check()` call
    //    `loom-daemon status --json`'s `.self_update` already makes. Feeds
    //    `assess_auto_update`'s staleness magnitude (Issue #7584): no
    //    daemon-side wire change needed since this is cheap enough to run on
    //    every invocation, exactly as `status` already does.
    let self_update = loom_daemon::self_update::check();

    // 7. A configured codesign identity's own non-interactive preflight
    //    (Issue #7605) — client-side, no IPC involved, so it is threaded in
    //    exactly like `self_update` above. Runs a real `codesign` invocation
    //    when (and only when) an identity is actually configured, so this is
    //    deliberately last: every other input above is cheap/local, this one
    //    is the only step in `collect()` that can itself take up to the
    //    preflight cap.
    let codesign_preflight = probe_codesign_identity_preflight(status.as_ref());

    // 8. The limit-calibration reading (#8063, rewired by #8349) — one more
    //    best-effort *local* input, never part of the IPC round-trip:
    //    claude-monitor's `usage_history` (#8366) where that companion is
    //    installed, else #8347's persisted `weekly_point_samples` joined
    //    against the activity DB's daily cost-equivalent (#8062) by #8348's
    //    pure `calibrate()`. Assessed as a conditional section by
    //    `health::assess_limit_calibration`; neither source readable means
    //    `None` ("not collected") — no section, no exit-code impact.
    let since =
        chrono::Utc::now() - chrono::Duration::days(limit_calibration::DEFAULT_LOOKBACK_DAYS);
    let limit_calibration = match limit_calibration::compute_with_fallback(
        &limit_calibration::default_monitor_db_path(),
        &limit_calibration::default_activity_db_path(),
        since,
    ) {
        CalibrationStatus::Unavailable(_) => None,
        status => Some(status),
    };

    // 9. The transcript-ingest health snapshot (#8477) — one more best-effort
    //    *local* input: whether the background pass is enabled
    //    (`autonomous.transcriptIngest`/`LOOM_TRANSCRIPT_INGEST`, resolved
    //    against the same `LOOM_ROOT` / daemon-reported / cwd repo root
    //    `resolve_configured_codesign_identity` uses) and whether the
    //    ledger is keeping up with transcripts actually on disk. Assessed by
    //    `health::assess_transcript_ingest`, which — unlike
    //    `limit_calibration` above — always renders a section: ingestion
    //    being off is the fact this issue exists to surface, not an
    //    unconfigured optional companion tool.
    let transcript_ingest_repo_root = std::env::var_os("LOOM_ROOT")
        .map(PathBuf::from)
        .or_else(|| {
            status
                .as_ref()
                .and_then(|s| s.per_repo.first().map(|r| r.root.clone()))
        })
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let transcript_ingest_config =
        transcript_ingest::read_transcript_ingest_config(&transcript_ingest_repo_root);
    let transcript_ingest_status =
        loom_daemon::transcript_tokens::claude_projects_dir().map(|projects_dir| {
            transcript_ingest::collect_health_status(
                &limit_calibration::default_activity_db_path(),
                &projects_dir,
                &transcript_ingest_config,
            )
        });

    // 10. The tmpfs/`shared`-RAM + kernel OOM-kill snapshot (#8572, split from
    //     #8512) — filesystem-only, no IPC, threaded in exactly like
    //     `transcript_ingest_status` above. `tmpfs_visibility_section` omits
    //     the section entirely when nothing was measurable (e.g. macOS), so
    //     this always collects rather than pre-filtering.
    let tmpfs_visibility_status = Some(loom_daemon::tmpfs_visibility::collect());

    health::assess(&HealthInputs {
        at: chrono::Utc::now(),
        window,
        status,
        ipc_error,
        install_state,
        pgrep_pids,
        pid_file,
        ranking_present,
        ranking_age_secs,
        token_class_capacity,
        pipeline,
        gh_unavailable,
        // This CLI process's own build commit (#4824), compared daemon-side
        // against `DaemonStatusReport::daemon_build_commit` so a newer CLI
        // querying an older daemon reports build skew rather than a phantom
        // dead work finder.
        cli_build_commit: loom_daemon::self_update::BUILT_COMMIT.to_string(),
        work_finder_log_tick_age_secs,
        self_update: Some(self_update),
        codesign_preflight,
        load_per_core,
        limit_calibration,
        codex_accounts: probe_codex_accounts(),
        transcript_ingest: transcript_ingest_status,
        tmpfs_visibility: tmpfs_visibility_status,
        // 11. CI-telemetry poller health (#9014) — local status.json read.
        ci_telemetry: Some(loom_daemon::ci_telemetry::collect_health(&transcript_ingest_repo_root)),
    })
}

/// This host's Codex account reading (#8407): inventory + health counts, one
/// read-only availability pass, and the provider-namespaced ranking file's
/// freshness.
///
/// Filesystem-only and read-only, like every other input this collector
/// gathers — `CheckOptions::default()` writes no ranking file and records no
/// health state, so rendering the health report can never change what the
/// next dispatch selects. `None` on any host with no resolvable workspace or
/// an unreadable registry: an optional signal's absence never becomes a
/// non-green line (the same rule `codesign_preflight` follows).
fn probe_codex_accounts() -> Option<health::codex_accounts::CodexAccountsSnapshot> {
    use loom_daemon::tokens_pool::{codex_check, provider_capacity_at, AccountProvider};

    let workspace = super::tokens::resolve_tokens_workspace(".").ok()?;
    let inventory =
        loom_daemon::tokens_pool::account_inventory(&workspace, AccountProvider::Codex).ok()?;
    if inventory.is_empty() {
        return None;
    }
    let now = chrono::Utc::now();
    let capacity = provider_capacity_at(
        &workspace,
        AccountProvider::Codex,
        &inventory,
        u64::try_from(now.timestamp()).unwrap_or(0),
    )
    .ok()?;
    let statuses = codex_check::run_check(&workspace, codex_check::CheckOptions::default(), now)
        .map(|(report, _)| {
            let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
            for account in &report.accounts {
                *counts.entry(account.status.clone()).or_insert(0) += 1;
            }
            counts
        })
        .unwrap_or_default();
    let (ranking_present, ranking_age_secs) = codex_check::ranking_file_state(&workspace);
    Some(health::codex_accounts::CodexAccountsSnapshot {
        workspace,
        capacity,
        statuses,
        ranking_present,
        ranking_age_secs,
    })
}

/// Issue #7605: probe a configured `codesign.identity` for whether it can
/// sign NON-INTERACTIVELY, so a keychain ACL misconfiguration that would
/// otherwise only surface as a 10+ minute hang inside
/// `sign_daemon_binary` (`scripts/install/provision-daemon.sh`) during the
/// next self-update roll is visible in `loom-daemon health` beforehand.
///
/// `None` (nothing to report) on: any non-Darwin host, no identity
/// configured (env nor resolved repo config), or `security`/`codesign`
/// themselves not spawnable in this process — every one of those is a case
/// `sign_daemon_binary` already silently treats as "use ad-hoc signing",
/// so there is nothing actionable to surface. `Some` covers both "not found
/// in the keychain" and "found but fails the non-interactive preflight" —
/// [`health::assess_codesign_identity`] only renders a section for the
/// latter's failing case (`ok: false`); a passing preflight also returns
/// `Some(.. ok: true ..)` here so a `--json` consumer can see the check ran
/// and passed, but produces no rendered section.
fn probe_codesign_identity_preflight(
    status: Option<&DaemonStatusReport>,
) -> Option<health::CodesignPreflightResult> {
    if !cfg!(target_os = "macos") {
        return None;
    }

    let identity = resolve_configured_codesign_identity(status)?;

    // Mirror sign_daemon_binary's own precedence: an identity absent from
    // the keychain listing is reported as a fact, not attempted.
    let listing = std::process::Command::new("security")
        .args(["find-identity", "-v", "-p", "codesigning"])
        .output()
        .ok()?;
    let keychain_identities = String::from_utf8_lossy(&listing.stdout);
    if !keychain_identities.contains(identity.as_str()) {
        return Some(health::CodesignPreflightResult {
            identity,
            ok: false,
            detail: "not found via 'security find-identity -v -p codesigning'".to_string(),
        });
    }

    let (ok, detail) = codesign_preflight_capped(&identity, CODESIGN_PREFLIGHT_CAP);
    Some(health::CodesignPreflightResult {
        identity,
        ok,
        detail,
    })
}

/// Hard wall-clock cap on the preflight `codesign` invocation — matches
/// `sign_daemon_binary`'s own default (`LOOM_CODESIGN_TIMEOUT_SECS`, default
/// 15s in `scripts/install/provision-daemon.sh`). Not itself overridable by
/// that env var: this CLI check runs at a human's convenience, not inside a
/// self-update loop racing a build-gate budget, so there is no equivalent
/// pressure to shrink it for a test harness here.
const CODESIGN_PREFLIGHT_CAP: Duration = Duration::from_secs(15);

/// `LOOM_CODESIGN_IDENTITY` (env) > `codesign.identity` (resolved repo
/// config) > `None` — the same precedence
/// `_pmd_resolve_codesign_identity` implements in
/// `scripts/install/provision-daemon.sh`, ported here so `health`'s finding
/// and `sign_daemon_binary`'s own resolution can never disagree about which
/// identity is actually configured.
///
/// Repo root for the config lookup: `LOOM_ROOT` (env) when set, else the
/// first repo this daemon has registered ([`DaemonStatusReport::per_repo`]),
/// else this process's own current directory — in that order, matching
/// `sign_daemon_binary`'s own `$LOOM_ROOT` > `git rev-parse
/// --show-toplevel`-via-cwd fallback as closely as a `health` CLI process
/// (which is not necessarily invoked from inside a git checkout at all) can.
fn resolve_configured_codesign_identity(status: Option<&DaemonStatusReport>) -> Option<String> {
    if let Ok(v) = std::env::var("LOOM_CODESIGN_IDENTITY") {
        if !v.trim().is_empty() {
            return Some(v);
        }
    }

    let repo_root = std::env::var_os("LOOM_ROOT")
        .map(std::path::PathBuf::from)
        .or_else(|| status.and_then(|s| s.per_repo.first().map(|r| r.root.clone())))
        .or_else(|| std::env::current_dir().ok())?;

    let effective = loom_daemon::config_resolver::resolve_effective_config(&repo_root);
    loom_daemon::config_resolver::get_path(&effective, "codesign.identity")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Sign a THROWAWAY COPY of this process's own binary with `identity` under
/// `cap` — never `<bin>` itself, and never the real daemon binary this
/// process is running from. Returns `(true, "")` on a non-interactive
/// success, else `(false, <reason>)`.
fn codesign_preflight_capped(identity: &str, cap: Duration) -> (bool, String) {
    let Ok(exe) = std::env::current_exe() else {
        return (
            false,
            "could not resolve this process's own binary to preflight against".to_string(),
        );
    };
    let tmp = std::env::temp_dir().join(format!(
        "loom-codesign-preflight-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    if std::fs::copy(&exe, &tmp).is_err() {
        return (
            false,
            "could not stage a throwaway copy of this binary to preflight".to_string(),
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&tmp) {
            let mut perms = meta.permissions();
            perms.set_mode(0o755);
            let _ = std::fs::set_permissions(&tmp, perms);
        }
    }

    let result = codesign_run_capped(
        &[
            "-f",
            "-s",
            identity,
            "--identifier",
            "com.rjwalters.loom-daemon.preflight",
            tmp.to_string_lossy().as_ref(),
        ],
        cap,
    );
    let _ = std::fs::remove_file(&tmp);
    result
}

/// Run `codesign <args>` under a hard wall-clock cap, killing it if it is
/// still running once the cap elapses (Issue #7605): an identity whose
/// private key lacks `codesign` in its keychain access control list raises
/// a blocking SecurityAgent GUI prompt with no flag to suppress it and no
/// bound on how long it waits — only a wall-clock cap from outside the
/// `codesign` process can detect that.
fn codesign_run_capped(args: &[&str], cap: Duration) -> (bool, String) {
    let mut child = match std::process::Command::new("codesign")
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return (false, format!("could not spawn codesign: {e}")),
    };

    let deadline = std::time::Instant::now() + cap;
    loop {
        match child.try_wait() {
            Ok(Some(exit_status)) => {
                return if exit_status.success() {
                    (true, String::new())
                } else {
                    (false, format!("codesign exited with {exit_status}"))
                };
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return (
                        false,
                        format!(
                            "timed out after {}s — likely cause: this identity's private key is \
                             missing '/usr/bin/codesign' from its keychain access control list, \
                             which raises a blocking Keychain prompt instead of signing \
                             non-interactively",
                            cap.as_secs()
                        ),
                    );
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => return (false, format!("error waiting on codesign: {e}")),
        }
    }
}

/// One bounded `DaemonStatus` round-trip, collapsed to
/// `(Some(report), None)` / `(None, Some(why))`.
///
/// #6103: a first attempt that merely **timed out** (never a hard failure —
/// see [`loom_daemon::health::ipc_error_is_probe_timeout`]) is retried
/// exactly once before this function reports a failure at all. A single
/// bounded miss on a busy host is not, by itself, evidence of an unhealthy
/// daemon; this is the one-shot-CLI-invocation substitute for the watchdog's
/// cross-tick consecutive-failure debounce, which has no history to lean on
/// here.
///
/// #6191: the retry's budget is no longer always the same as the first
/// attempt's. `escalate` — [`health::alive_with_fresh_heartbeat`] against this
/// invocation's own local install-state probe, decided by the caller before
/// the first attempt even starts — selects [`ESCALATED_IPC_TIMEOUT`] instead
/// of repeating [`resolve_ipc_timeout`]'s short budget, when local evidence
/// already corroborates the daemon as alive and recently active. `escalate =
/// false` (no such corroboration) preserves the exact pre-#6191 behavior:
/// retry once, same budget.
async fn query_status(escalate: bool) -> (Option<DaemonStatusReport>, Option<String>) {
    let socket_path = match resolve_socket_path() {
        Ok(p) => p,
        Err(e) => return (None, Some(format!("could not resolve socket path: {e}"))),
    };
    let timeout = resolve_ipc_timeout();
    match query_status_once(&socket_path, timeout).await {
        Ok(report) => (Some(report), None),
        Err(first_err) if health::ipc_error_is_probe_timeout(&first_err) => {
            // #8163: the root count is read from the LOCAL workspace registry
            // (a cheap JSON read), not from the daemon — the client cannot
            // learn it from a round-trip it has not managed to complete.
            let root_count = loom_daemon::status_budget::registered_root_count();
            let retry_timeout = resolve_retry_timeout(timeout, escalate, root_count);
            match query_status_once(&socket_path, retry_timeout).await {
                Ok(report) => (Some(report), None),
                Err(second_err) => (None, Some(second_err)),
            }
        }
        Err(first_err) => (None, Some(first_err)),
    }
}

/// The retry budget [`query_status`] uses on a bounded-timeout-classified
/// first miss (#6191, root-scaled by #8163).
///
/// When `escalate` is set (local evidence already corroborates the daemon as
/// alive with a fresh heartbeat — see [`health::alive_with_fresh_heartbeat`]),
/// the budget is the largest of:
///
/// 1. `base` — so an already-wider first-attempt budget (a heavily
///    load-scaled [`resolve_ipc_timeout`], or an operator's own
///    `LOOM_DAEMON_IPC_TIMEOUT_MS` override) is never *narrowed*;
/// 2. [`ESCALATED_IPC_TIMEOUT`] — #6191's fixed floor;
/// 3. [`loom_daemon::status_budget::client_probe_budget`] over `root_count` —
///    #8163's root-scaled term, which is what makes the escalated retry
///    actually able to cover an `O(roots)` `build_daemon_status` on a
///    many-workspace host. At `root_count == 1` it is well under the `10s`
///    floor, so a single-workspace host is bit-for-bit unchanged.
///
/// `escalate = false` (no corroboration) still preserves the exact pre-#6191
/// behavior: retry once, same budget — a host with many workspaces but no
/// evidence its daemon is alive gets no extra patience.
///
/// Split out of [`query_status`] purely so this decision is unit-testable
/// without a socket.
fn resolve_retry_timeout(base: Duration, escalate: bool, root_count: usize) -> Duration {
    if escalate {
        base.max(ESCALATED_IPC_TIMEOUT)
            .max(loom_daemon::status_budget::client_probe_budget(root_count))
    } else {
        base
    }
}

/// A single connect + `DaemonStatus` attempt, collapsed to the same rendered
/// `Err` string [`query_status`] has always produced. Never itself retried —
/// that decision lives one layer up, in [`query_status`].
async fn query_status_once(
    socket_path: &Path,
    timeout: Duration,
) -> Result<DaemonStatusReport, String> {
    match query_daemon_bounded(socket_path, &Request::DaemonStatus, timeout).await {
        Ok(Response::DaemonStatus(report)) => Ok(*report),
        Ok(Response::Error { message }) => Err(format!("daemon error: {message}")),
        Ok(other) => Err(format!("unexpected response: {other:?}")),
        Err(e) => Err(e.to_string()),
    }
}

/// The token-pool directory every pool-scoped health input is read from.
///
/// The daemon's own [`DaemonStatusReport::token_pool_dir`] (#4292) when
/// available, falling back to this process's cwd resolution only for a
/// pre-#4292 daemon or an unreachable one. `None` when neither resolves, in
/// which case every pool-scoped input reports "absent" rather than guessing.
///
/// Shared by [`probe_ranking`] and [`probe_class_capacity`] (#8058 Phase 3) so
/// the staleness figure and the per-class breakdown printed beside it are
/// structurally incapable of describing different pools.
fn resolve_health_pool_dir(status: Option<&DaemonStatusReport>) -> Option<PathBuf> {
    match status.and_then(|r| r.token_pool_dir.clone()) {
        Some(dir) => Some(dir),
        None => {
            let ws = super::tokens::resolve_tokens_workspace(".").ok()?;
            Some(loom_daemon::tokens_pool::paths::resolve_tokens_dir(&ws))
        }
    }
}

/// Stat the resolved pool's `.ranking`: `(present, age_secs)`.
fn probe_ranking(status: Option<&DaemonStatusReport>) -> (bool, Option<u64>) {
    let Some(dir) = resolve_health_pool_dir(status) else {
        return (false, None);
    };
    ranking_state(&dir)
}

/// Per-model-class healthy counts for the resolved pool (#8058 Phase 3), or
/// `None` when there is no pool directory or no readable `.ranking` there.
///
/// Filesystem-only, like every other input this collector gathers: it reads
/// `.ranking` and `.bad_tokens` and asks
/// [`loom_daemon::tokens_pool::bad_tokens`] the same class-scoped blocking
/// question the selector asks, so the counts cannot drift from what a spawn
/// would actually be handed.
fn probe_class_capacity(
    status: Option<&DaemonStatusReport>,
) -> Option<loom_daemon::capacity::model_class::ClassCapacity> {
    let dir = resolve_health_pool_dir(status)?;
    loom_daemon::capacity::model_class::read_class_capacity_at(&dir)
}

/// `(present, age_secs)` for `<dir>/.ranking`. Delegates to
/// [`loom_daemon::capacity::ranking_file_state`] (#5269) — the same probe
/// `ipc::build_daemon_status` uses to populate each registered repo's own
/// `RepoStatus::ranking_present`/`ranking_age_secs`, so this CLI's single
/// anchored-pool probe and the daemon's per-repo snapshot can never disagree
/// about what "present"/"age" means for the same directory.
fn ranking_state(dir: &Path) -> (bool, Option<u64>) {
    loom_daemon::capacity::ranking_file_state(dir)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use loom_daemon::status_budget;

    #[test]
    fn ranking_state_reports_absent_when_there_is_no_file() {
        let tmp = tempfile::tempdir().unwrap();
        let (present, age) = ranking_state(tmp.path());
        assert!(!present);
        assert_eq!(age, None);
    }

    #[test]
    fn ranking_state_reports_present_and_fresh_for_a_just_written_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".ranking"), "a|available|0.1\n").unwrap();
        let (present, age) = ranking_state(tmp.path());
        assert!(present);
        assert!(age.unwrap() < 60, "a file just written should read as fresh");
    }

    /// Issue #6103 AC1: `health`'s IPC budget must honor the same
    /// `LOOM_DAEMON_IPC_TIMEOUT_MS` floor `status`/`dispatch` already do — one
    /// env var, every client-side IPC round-trip in this binary.
    ///
    /// The property under test is "the env floor is honored", i.e. it *raises*
    /// a lower load-scaled value to itself — NOT "this host is idle". So the
    /// load term is injected as `0.0` (#6625) rather than read from
    /// `/proc/loadavg`, which is not namespaced and therefore reports a busy
    /// shared host's load to every container on it: with 8 sibling jobs on a
    /// 32-core host, a 4-core container read load-per-core ≈ 7.5, the scaled
    /// value (23s) won the `max`, and this assertion failed against a
    /// perfectly correct function. The live-load path is still exercised, as
    /// the `>=` assertion it can actually support.
    #[test]
    #[serial_test::serial]
    fn resolve_ipc_timeout_honors_the_shared_env_floor() {
        std::env::set_var(super::super::common::DAEMON_IPC_TIMEOUT_ENV, "9000");
        let floored = resolve_ipc_timeout_for_load(Some(0.0));
        let live = resolve_ipc_timeout();
        std::env::remove_var(super::super::common::DAEMON_IPC_TIMEOUT_ENV);
        assert_eq!(
            floored,
            Duration::from_secs(9),
            "an idle host's scaled value (the 2s base) must be raised to the 9s env floor"
        );
        assert!(
            live >= Duration::from_secs(9),
            "the env floor is raise-only: whatever this host's load scales to, \
             the result may never fall below the 9s floor (got {live:?})"
        );
    }

    /// The other half of "raise-only" (#6625): a load-scaled value *above* the
    /// env floor must not be dragged back down to it. Injected load, so this
    /// holds on an idle CI VM and a saturated shared host alike.
    #[test]
    #[serial_test::serial]
    fn resolve_ipc_timeout_env_floor_never_lowers_a_load_scaled_value() {
        std::env::set_var(super::super::common::DAEMON_IPC_TIMEOUT_ENV, "3000");
        // 2s base at 8x load-per-core scales to 16s, well above the 3s floor.
        let timeout = resolve_ipc_timeout_for_load(Some(8.0));
        std::env::remove_var(super::super::common::DAEMON_IPC_TIMEOUT_ENV);
        assert_eq!(timeout, Duration::from_secs(16));
    }

    /// With no env override, the resolved timeout must never fall below
    /// [`BASE_IPC_TIMEOUT`] regardless of this test-runner host's actual
    /// load — a real regression here would only ever make it larger, never
    /// smaller.
    #[test]
    #[serial_test::serial]
    fn resolve_ipc_timeout_never_undercuts_the_base_without_an_override() {
        std::env::remove_var(super::super::common::DAEMON_IPC_TIMEOUT_ENV);
        assert!(resolve_ipc_timeout() >= BASE_IPC_TIMEOUT);
    }

    /// Issue #6191: with no corroborating alive-with-fresh-heartbeat evidence
    /// the retry budget is unchanged from the first attempt's — the exact
    /// pre-#6191 "retry once, same budget" behavior. #8163: a large root
    /// count buys no extra patience here either, because the corroboration
    /// that the daemon is even alive is what is missing.
    #[test]
    fn resolve_retry_timeout_is_unchanged_without_escalation() {
        assert_eq!(resolve_retry_timeout(BASE_IPC_TIMEOUT, false, 1), BASE_IPC_TIMEOUT);
        assert_eq!(
            resolve_retry_timeout(BASE_IPC_TIMEOUT, false, status_budget::DOCUMENTED_MAX_ROOTS),
            BASE_IPC_TIMEOUT
        );
    }

    /// Issue #6191 AC1: corroborated evidence escalates the retry to
    /// [`ESCALATED_IPC_TIMEOUT`] rather than repeating a short first-attempt
    /// budget. On a single-workspace host #8163's root term is well under
    /// that floor, so this pins the pre-#8163 behavior as unchanged.
    #[test]
    fn resolve_retry_timeout_escalates_when_corroborated() {
        assert_eq!(resolve_retry_timeout(BASE_IPC_TIMEOUT, true, 1), ESCALATED_IPC_TIMEOUT);
    }

    /// **Issue #8163 AC1.** A many-workspace host must get a retry budget
    /// derived from its registered root count, not the fixed `10s` floor that
    /// the `13.1s`/`14.3s` `build_daemon_status` builds in the issue report
    /// outran on every single call.
    #[test]
    fn resolve_retry_timeout_scales_with_the_registered_root_count() {
        let many = resolve_retry_timeout(BASE_IPC_TIMEOUT, true, 36);
        assert!(
            many > ESCALATED_IPC_TIMEOUT,
            "a 36-root host must be budgeted above the fixed #6191 floor, got {many:?}"
        );
        assert!(
            many > Duration::from_millis(14_300),
            "and above the worst build #8163 measured, got {many:?}"
        );
        assert_eq!(many, status_budget::client_probe_budget(36));
        // Monotonic: registering another workspace never shrinks the budget.
        assert!(resolve_retry_timeout(BASE_IPC_TIMEOUT, true, 37) >= many);
    }

    /// An already-wider base (a heavily load-scaled first attempt, or an
    /// operator's own `LOOM_DAEMON_IPC_TIMEOUT_MS` floor) must never be
    /// *narrowed* by escalation — nor by #8163's root term, which is combined
    /// with `max` for exactly this reason.
    #[test]
    fn resolve_retry_timeout_never_narrows_an_already_wider_base() {
        let wide = status_budget::MAX_ROOT_SCALED_PROBE_TIMEOUT + Duration::from_secs(5);
        assert_eq!(resolve_retry_timeout(wide, true, 1), wide);
        assert_eq!(resolve_retry_timeout(wide, true, status_budget::DOCUMENTED_MAX_ROOTS), wide);
    }

    // ===================================================================
    // Codesign identity resolution (#7605)
    // ===================================================================

    /// `LOOM_CODESIGN_IDENTITY` (env) is the highest-precedence source — the
    /// same rule `_pmd_resolve_codesign_identity` in
    /// `scripts/install/provision-daemon.sh` follows — and must win even
    /// when `LOOM_ROOT` points at a repo config that sets a different value.
    #[test]
    #[serial_test::serial(codesign_identity_env)]
    fn resolve_configured_codesign_identity_prefers_env_over_config() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".loom")).unwrap();
        std::fs::write(
            tmp.path().join(".loom/config.json"),
            r#"{"codesign": {"identity": "Config Identity"}}"#,
        )
        .unwrap();

        std::env::set_var("LOOM_CODESIGN_IDENTITY", "Env Identity");
        std::env::set_var("LOOM_ROOT", tmp.path());
        let resolved = resolve_configured_codesign_identity(None);
        std::env::remove_var("LOOM_CODESIGN_IDENTITY");
        std::env::remove_var("LOOM_ROOT");

        assert_eq!(resolved.as_deref(), Some("Env Identity"));
    }

    /// With no env override, `codesign.identity` is read from the resolved
    /// config at `LOOM_ROOT`.
    #[test]
    #[serial_test::serial(codesign_identity_env)]
    fn resolve_configured_codesign_identity_falls_back_to_config() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".loom")).unwrap();
        std::fs::write(
            tmp.path().join(".loom/config.json"),
            r#"{"codesign": {"identity": "Config Identity"}}"#,
        )
        .unwrap();

        std::env::remove_var("LOOM_CODESIGN_IDENTITY");
        std::env::set_var("LOOM_ROOT", tmp.path());
        let resolved = resolve_configured_codesign_identity(None);
        std::env::remove_var("LOOM_ROOT");

        assert_eq!(resolved.as_deref(), Some("Config Identity"));
    }

    /// Neither env nor a resolvable config with the key set -> `None`, the
    /// same "nothing configured" outcome `sign_daemon_binary` treats as
    /// "use ad-hoc signing" -- never a false positive finding.
    ///
    /// `LOOM_ROOT` alone does not fully sandbox
    /// `resolve_configured_codesign_identity`: `resolve_effective_config`
    /// also merges in a machine-level "private/shared defaults" tier
    /// (`config_resolver::private_defaults_path`, normally
    /// `~/.local/share/loom/config/defaults.json`) that is deliberately
    /// *independent* of `repo_root` — it is meant to apply fleet-wide
    /// regardless of which repo is being resolved. On a host that has
    /// provisioned that file with a `codesign.identity` (the documented
    /// "one file, fleet-wide" setup), this test's empty `LOOM_ROOT` tempdir
    /// still resolves to that real identity instead of `None` (issue #8463).
    /// Disable the tier for the duration of this test the same way
    /// production does — `LOOM_CONFIG_DEFAULTS_FILE` set to an empty string
    /// (see `config_resolver::private_defaults_path`'s doc comment) — rather
    /// than relying on the host happening not to have one provisioned.
    #[test]
    #[serial_test::serial(codesign_identity_env)]
    fn resolve_configured_codesign_identity_is_none_when_unconfigured() {
        let tmp = tempfile::tempdir().unwrap();

        std::env::remove_var("LOOM_CODESIGN_IDENTITY");
        std::env::set_var("LOOM_ROOT", tmp.path());
        std::env::set_var(loom_daemon::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let resolved = resolve_configured_codesign_identity(None);
        std::env::remove_var("LOOM_ROOT");
        std::env::remove_var(loom_daemon::config_resolver::PRIVATE_DEFAULTS_ENV);

        assert_eq!(resolved, None);
    }

    // -- limit_calibration section mapping (#8063/#8349) -------------------
    // Moved to `health::calibration_section`'s own test module when #8349
    // wired the section through `HealthInputs::limit_calibration` — the
    // rendering it exercised now lives next to `assess_limit_calibration`.
}
