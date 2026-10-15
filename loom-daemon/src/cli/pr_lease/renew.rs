//! The detached renewal loop behind a won `lease pr claim` (#9401).
//!
//! The issue path's renewer is `sweep-lease-renew.sh` (#6180), a shell
//! `contract` file whose portable growth the shell budget refuses outright
//! (see `cli::lease_ensure`'s module docs), and whose readers all assume the
//! `loom:lease host=` marker. So the PR lease gets a small Rust loop, reusing
//! the issue renewer's single-owner bookkeeping (`cli::lease_renewer`, #10229):
//! one renewer per `(repo, host, sweep#pr-<role>, pr)`, a `release` tombstone
//! the loop honours at its next cycle, and a signal only to a pid whose start
//! identity still matches.
//!
//! Each cycle (default every 300 s, `SWEEP_LEASE_RENEW_INTERVAL_SECS`) makes
//! up to three requests: one `GET issues/<pr>` (state and labels), one read
//! of the PR's `loom:pr-lease*` comments, and one PATCH of the lease comment
//! — 36/h per held lease. It stops when the watched session dies, at the age
//! cap (4 h, like `lease ensure`), on `release`, when the PR closes, when the
//! lease comment is gone, or when the role's claim label has been absent for
//! two consecutive cycles. That last one is the backstop for a claimant that
//! finished without calling `release`: the label is the projection the role
//! removes when it is done, and two cycles tolerate a reclaim's
//! remove-then-add.
//!
//! ## A renewal never revives an expired lease
//!
//! A PATCH refreshes `updated_at`, and the lease's comment `id` is older than
//! any claim a peer made after it expired, so renewing a lease that has
//! already aged out would make it the earliest live lease again and take the
//! claim from that peer mid-flight. Two guards run before every PATCH:
//!
//! - **Wall clock.** The loop stops once the last successful renewal (or the
//!   claim itself) is within [`RENEW_MARGIN_SECS`] of the TTL. Measured with
//!   `chrono::Utc::now()`, never `Instant`: on macOS `Instant` does not
//!   advance while the machine sleeps, which is exactly the lid-closed case.
//!   The 4 h cap uses the same wall clock. It is checked at the top of the
//!   cycle and again immediately before the PATCH ([`pre_patch_guard`]), so a
//!   slow forge read cannot carry a just-inside check past the margin.
//! - **The forge.** It re-reads the role's leases and renews only while its
//!   own comment is still this role's earliest live lease *with the margin to
//!   spare* ([`forge_state`]: live against `TTL - margin`, not the full TTL).
//!   A peer holding the claim, or its own lease no longer live, stops the
//!   loop. An unreadable list falls back to the wall-clock guard alone.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use super::super::lease_renewer::{self, Key, Store};
use super::{
    claim_state, lease_body, provenance_line, ClaimState, Ctx, Gh, Identity, Role, Row, GH_TIMEOUT,
};

/// Renewal cadence, shared with the issue renewer's knob.
const INTERVAL_ENV: &str = "SWEEP_LEASE_RENEW_INTERVAL_SECS";
pub(crate) const DEFAULT_INTERVAL_SECS: u64 = 300;
/// Absolute lifetime cap (4 h), as `lease ensure` uses for one builder.
pub(crate) const DEFAULT_MAX_AGE_SECS: u64 = 14_400;
/// Consecutive cycles without the claim label before the loop stops.
pub(crate) const LABEL_MISS_LIMIT: u32 = 2;
/// Slack on top of the `gh` timeouts the margin must outlast.
const MARGIN_SLACK_SECS: u64 = 10;
/// Stop this far short of the TTL: a renewal checked just inside it must
/// still land before the lease ages out. Derived from the `gh` timeout so the
/// two cannot drift apart: two calls (the forge read and the PATCH) plus
/// slack, although the pre-PATCH re-check leaves only the PATCH inside it.
/// Clamped to a quarter of the TTL for short TTLs ([`renew_margin_secs`]).
pub(crate) const RENEW_MARGIN_SECS: u64 = 2 * GH_TIMEOUT.as_secs() + MARGIN_SLACK_SECS;
/// How often a sleeping loop re-checks the watched session.
const POLL: Duration = Duration::from_secs(5);

#[derive(clap::Args, Debug, Clone)]
pub(crate) struct RenewLoopArgs {
    #[arg(value_name = "PR")]
    pub(crate) pr: u64,
    #[arg(long, value_enum)]
    pub(crate) role: Role,
    /// The lease's step (#11898), when it names one.
    #[arg(long)]
    pub(crate) step: Option<loom_daemon::step_role::StepRole>,
    #[arg(long)]
    pub(crate) host: String,
    #[arg(long)]
    pub(crate) sweep_id: String,
    #[arg(long)]
    pub(crate) comment_id: u64,
    #[arg(long)]
    pub(crate) watch_pid: u32,
    #[arg(long)]
    pub(crate) story_issue: Option<u32>,
    #[arg(long, default_value_t = DEFAULT_MAX_AGE_SECS)]
    pub(crate) max_age: u64,
    #[arg(long, default_value_t = DEFAULT_INTERVAL_SECS)]
    pub(crate) interval: u64,
    /// Wall-clock time (RFC 3339) at or before the lease's last write — the
    /// claim's post, or the re-attached lease's `updated_at`. The renewal
    /// guard ages from here. Default: the loop's own start.
    #[arg(long)]
    pub(crate) leased_at: Option<DateTime<Utc>>,
    #[arg(long)]
    pub(crate) workspace: PathBuf,
}

/// The renewer-ownership key: the issue renewer's store, with the role folded
/// into the sweep half so a Doctor's and a Judge's renewer never collide.
pub(crate) fn key(repo: &str, me: &Identity, pr: u64) -> Key {
    Key {
        repo: repo.to_string(),
        host: me.host.clone(),
        sweep: sweep_key(me),
        issue: pr,
    }
}

fn sweep_key(me: &Identity) -> String {
    format!("{}#pr-{}", me.sweep, me.role.as_str())
}

/// What one cycle does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    Renew,
    /// Skip this cycle's PATCH, keep the loop.
    Skip,
    Stop(&'static str),
}

/// The clocks one cycle reads, all wall-clock seconds.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Clocks {
    /// Since the renewer started (the 4 h cap).
    pub(crate) elapsed_secs: i64,
    pub(crate) cap_secs: u64,
    /// Since the last successful renewal, or the claim.
    pub(crate) since_renewal_secs: i64,
    pub(crate) ttl_secs: u64,
}

impl Clocks {
    /// The lease is too old to renew: within the margin of the TTL.
    fn lease_aged_out(&self) -> bool {
        aged_out(self.since_renewal_secs, self.ttl_secs)
    }
}

/// The renewal margin for a TTL: [`RENEW_MARGIN_SECS`], clamped to a quarter
/// of the TTL.
pub(crate) fn renew_margin_secs(ttl_secs: u64) -> u64 {
    RENEW_MARGIN_SECS.min(ttl_secs / 4)
}

/// A lease last written `since_secs` ago is within the margin of `ttl_secs`.
fn aged_out(since_secs: i64, ttl_secs: u64) -> bool {
    let limit = ttl_secs.saturating_sub(renew_margin_secs(ttl_secs));
    i64::try_from(limit).is_ok_and(|limit| since_secs >= limit)
}

/// The wall-clock re-check taken immediately before the PATCH, at `attempt`
/// (after the forge read): the top-of-cycle check ran before up to two `gh`
/// calls, so it is repeated here. Pure.
pub(crate) fn pre_patch_guard(
    attempt: DateTime<Utc>,
    last_renewed: DateTime<Utc>,
    ttl_secs: u64,
) -> Step {
    if aged_out(wall_secs(attempt, last_renewed), ttl_secs) {
        Step::Stop("the lease aged into the renewal margin during the forge read; renewing could revive it")
    } else {
        Step::Renew
    }
}

/// The forge state the renewal guard reads: [`claim_state`] at the full TTL
/// (peers are judged exactly as a fence would judge them), except that this
/// claimant's own earliest lease must also be live against `TTL - margin`,
/// or it reads [`ClaimState::Free`]. Pure.
pub(crate) fn forge_state(
    rows: &[Row],
    me: &Identity,
    now: DateTime<Utc>,
    ttl: chrono::Duration,
) -> ClaimState {
    let full = claim_state(Some(rows), me, now, ttl);
    if let ClaimState::HeldBySelf { id } = full {
        let ttl_secs = u64::try_from(ttl.num_seconds()).unwrap_or(0);
        let margin = i64::try_from(renew_margin_secs(ttl_secs)).unwrap_or(i64::MAX);
        let inner = ttl - chrono::Duration::seconds(margin);
        if claim_state(Some(rows), me, now, inner) != (ClaimState::HeldBySelf { id }) {
            return ClaimState::Free;
        }
    }
    full
}

/// Seconds from `then` to `now` on the wall clock.
pub(crate) fn wall_secs(now: DateTime<Utc>, then: DateTime<Utc>) -> i64 {
    now.signed_duration_since(then).num_seconds()
}

/// The per-cycle decision. Pure; `misses` carries the label-absent streak.
pub(crate) fn decide(
    watch_alive: bool,
    clocks: Clocks,
    owner_code: i32,
    pr_state: Option<&(bool, Vec<String>)>,
    label: Option<&str>,
    misses: &mut u32,
) -> Step {
    if !watch_alive {
        return Step::Stop("the watched session ended");
    }
    if i64::try_from(clocks.cap_secs).is_ok_and(|cap| clocks.elapsed_secs >= cap) {
        return Step::Stop("the age cap passed");
    }
    if clocks.lease_aged_out() {
        return Step::Stop(
            "the lease went a TTL without a renewal; it may have expired and been claimed by a \
             peer, so renewing could revive it",
        );
    }
    if owner_code == lease_renewer::EXIT_STOP {
        return Step::Stop("released, or ownership passed to another renewer");
    }
    let Some((open, labels)) = pr_state else {
        return Step::Skip;
    };
    if !open {
        return Step::Stop("the PR is closed");
    }
    // A claim with no label (the Champion merge step, #11898) has no
    // label-absence backstop: the session, the cap and the item's state stop it.
    let Some(label) = label else {
        return Step::Renew;
    };
    if labels.iter().any(|l| l == label) {
        *misses = 0;
        return Step::Renew;
    }
    *misses += 1;
    if *misses >= LABEL_MISS_LIMIT {
        Step::Stop("the claim label is gone (claimant finished without release)")
    } else {
        Step::Skip
    }
}

/// The forge guard before a PATCH: renew only while `comment_id` is still
/// this role's earliest live lease. `None` (unreadable list) defers to the
/// wall-clock guard [`decide`] already applied. Pure.
pub(crate) fn forge_guard(state: Option<&ClaimState>, comment_id: u64) -> Step {
    match state {
        None | Some(ClaimState::Unknown) => Step::Renew,
        Some(ClaimState::HeldBySelf { id }) if *id == comment_id => Step::Renew,
        Some(ClaimState::HeldBySelf { .. }) => {
            Step::Stop("another lease of this claimant is the earliest; not this comment")
        }
        Some(ClaimState::HeldByPeer { .. }) => {
            Step::Stop("a peer holds this role's earliest live lease")
        }
        Some(ClaimState::Free) => {
            Step::Stop("this lease is no longer live (or is within the renewal margin); renewing could revive it")
        }
    }
}

fn log_path(root: &Path, pr: u64, role: Role) -> PathBuf {
    root.join(".loom/logs/pr-lease")
        .join(format!("pr-{pr}-{}.log", role.as_str()))
}

fn note(root: &Path, pr: u64, role: Role, line: &str) {
    let path = log_path(root, pr, role);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "{} {line}", chrono::Utc::now().to_rfc3339());
    }
}

fn repo_of(root: &Path) -> String {
    lease_renewer::repo_identity(std::env::var("LOOM_REPO").ok().as_deref(), root)
}

/// Start the detached loop; returns the line to report.
pub(crate) fn spawn(
    ctx: &Ctx,
    comment_id: u64,
    watch_pid: u32,
    story: Option<u32>,
    leased_at: DateTime<Utc>,
) -> String {
    let interval = std::env::var(INTERVAL_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_INTERVAL_SECS);
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => return format!("renewer not started ({e}); the lease ages out at the TTL"),
    };
    let mut command = Command::new(exe);
    command
        .args(["lease", "pr", "renew-loop", &ctx.common.pr.to_string()])
        .args(["--role", ctx.me.role.as_str()])
        .args(ctx.me.step.map(|s| format!("--step={s}")))
        .args(["--host", &ctx.me.host, "--sweep-id", &ctx.me.sweep])
        .args(["--comment-id", &comment_id.to_string()])
        .args(["--watch-pid", &watch_pid.to_string()])
        .args(["--interval", &interval.to_string()])
        .args([
            "--leased-at",
            &leased_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        ])
        .arg("--workspace")
        .arg(&ctx.gh.root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(story) = story {
        command.args(["--story-issue", &story.to_string()]);
    }
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    super::super::lease_ensure::mark_inherited_fds_cloexec();
    match command.spawn() {
        Ok(child) => format!(
            "renewer {} keeps the lease fresh every {interval}s while pid {watch_pid} lives (log: \
             .loom/logs/pr-lease/pr-{}-{}.log)",
            child.id(),
            ctx.common.pr,
            ctx.me.role.as_str()
        ),
        Err(e) => format!("renewer not started ({e}); the lease ages out at the TTL"),
    }
}

/// End this claimant's renewer (tombstone + identity-checked SIGTERM).
/// Returns how many ownership records matched.
pub(crate) fn stop(ctx: &Ctx) -> usize {
    let store = Store::from_env();
    let signal = |pid: u32| {
        if let Ok(pid) = i32::try_from(pid) {
            // SAFETY: plain kill(2) on a pid `release` just re-verified by
            // its start identity.
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
    };
    lease_renewer::release(
        &store,
        &repo_of(&ctx.gh.root),
        ctx.common.pr,
        Some(&ctx.me.host),
        Some(&sweep_key(&ctx.me)),
        &lease_renewer::owner_is_live,
        &signal,
    )
    .unwrap_or(0)
}

/// The `renew-loop` entry point.
pub(crate) fn run_loop(args: &RenewLoopArgs) {
    let root = args.workspace.clone();
    let me = Identity {
        role: args.role,
        host: args.host.clone(),
        sweep: args.sweep_id.clone(),
        step: args.step,
    };
    let say = |line: &str| note(&root, args.pr, args.role, line);
    let store = Store::from_env();
    let key = key(&repo_of(&root), &me, args.pr);
    let own = std::process::id();
    let token = format!(
        "{own}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    );
    let ident_of = |p: u32| lease_renewer::start_identity(p).unwrap_or_default();
    match lease_renewer::claim(&store, &key, own, &token, &ident_of, &lease_renewer::owner_is_live)
    {
        Ok(owner) if owner != own => {
            say(&format!("renewer {owner} already owns this lease; exiting"));
            return;
        }
        Ok(_) => {}
        Err(e) => say(&format!("renewer bookkeeping unavailable ({e}); renewing unowned")),
    }
    let watch = args.watch_pid;
    let watch_ident = lease_renewer::start_identity(watch);
    let alive = || {
        loom_daemon::live_claim::pid_is_live_process(watch)
            && (watch_ident.is_none() || lease_renewer::start_identity(watch) == watch_ident)
    };
    let gh = Gh {
        bin: PathBuf::from(std::env::var("LOOM_GH_BIN").unwrap_or_else(|_| "gh".into())),
        root: root.clone(),
        write_repo: super::write_target(&root),
    };
    if let Err(why) = &gh.write_repo {
        say(&format!("renewer stopping: refusing to write here (#9548): {why}"));
        return;
    }
    let provenance = provenance_line(&root, &me, args.story_issue);
    let policy = loom_daemon::comment_trust::TrustPolicy::for_root(&root);
    let ttl_minutes = loom_daemon::claim_reconciliation::resolve_lease_ttl_minutes();
    let ttl = chrono::Duration::seconds((ttl_minutes * 60.0) as i64);
    // Wall clock throughout (#9401 review): `Instant` stands still while a
    // Mac sleeps, so a lid-closed renewer would wake believing its lease fresh.
    let started = Utc::now();
    let mut last_renewed = args.leased_at.unwrap_or(started).min(started);
    let mut misses = 0;
    say(&format!("renewer {own} started for comment {} (pid {watch})", args.comment_id));
    loop {
        sleep_while(Duration::from_secs(args.interval.max(1)), &alive);
        let watch_alive = alive();
        let (owner_code, _) = lease_renewer::check(&store, &key, &token, None);
        let state = if watch_alive {
            gh.pr_state(args.pr)
        } else {
            None
        };
        let now = Utc::now();
        let clocks = Clocks {
            elapsed_secs: wall_secs(now, started),
            cap_secs: args.max_age,
            since_renewal_secs: wall_secs(now, last_renewed),
            ttl_secs: u64::try_from(ttl.num_seconds()).unwrap_or(0),
        };
        let mut step =
            decide(watch_alive, clocks, owner_code, state.as_ref(), me.claim_label(), &mut misses);
        if step == Step::Renew {
            let rows = gh.read_rows(args.pr, &policy);
            let state = rows
                .as_deref()
                .map(|rows| forge_state(rows, &me, Utc::now(), ttl));
            step = forge_guard(state.as_ref(), args.comment_id);
        }
        match step {
            Step::Stop(why) => {
                say(&format!("renewer stopping: {why}"));
                return;
            }
            Step::Skip => say("cycle skipped (PR state unreadable, or claim label absent)"),
            Step::Renew => {
                // Taken BEFORE the PATCH: the forge's `updated_at` is no
                // earlier, so the guard never overestimates the lease's life.
                let attempt = Utc::now();
                // Re-check after the forge read (#9401 review): only the
                // PATCH's own timeout may now sit inside the margin.
                if let Step::Stop(why) = pre_patch_guard(attempt, last_renewed, clocks.ttl_secs) {
                    say(&format!("renewer stopping: {why}"));
                    return;
                }
                let body = lease_body(&me, args.pr, Some(attempt), &provenance);
                match gh.patch(args.comment_id, &body) {
                    Ok(()) => {
                        last_renewed = attempt;
                        say("renewed");
                    }
                    Err(true) => {
                        say("renewer stopping: the lease comment is gone");
                        return;
                    }
                    Err(false) => say("renewal PATCH failed; retrying next cycle"),
                }
            }
        }
    }
}

/// Sleep `total`, waking every [`POLL`] to stop early once `alive` fails.
fn sleep_while(total: Duration, alive: &impl Fn() -> bool) {
    let deadline = Instant::now() + total;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        if left.is_zero() || !alive() {
            return;
        }
        std::thread::sleep(left.min(POLL));
    }
}
