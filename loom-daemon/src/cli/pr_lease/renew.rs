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
//! two requests: one `GET issues/<pr>` (state and labels) and one PATCH of
//! the lease comment — 24/h per held lease, the issue renewer's dispatched
//! budget. It stops when the watched session dies, at the age cap (4 h, like
//! `lease ensure`), on `release`, when the PR closes, when the lease comment
//! is gone, or when the role's claim label has been absent for two
//! consecutive cycles. That last one is the backstop for a claimant that
//! finished without calling `release`: the label is the projection the role
//! removes when it is done, and two cycles tolerate a reclaim's
//! remove-then-add.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::super::lease_renewer::{self, Key, Store};
use super::{lease_body, provenance_line, Ctx, Gh, Identity, Role};

/// Renewal cadence, shared with the issue renewer's knob.
const INTERVAL_ENV: &str = "SWEEP_LEASE_RENEW_INTERVAL_SECS";
pub(crate) const DEFAULT_INTERVAL_SECS: u64 = 300;
/// Absolute lifetime cap (4 h), as `lease ensure` uses for one builder.
pub(crate) const DEFAULT_MAX_AGE_SECS: u64 = 14_400;
/// Consecutive cycles without the claim label before the loop stops.
pub(crate) const LABEL_MISS_LIMIT: u32 = 2;
/// How often a sleeping loop re-checks the watched session.
const POLL: Duration = Duration::from_secs(5);

#[derive(clap::Args, Debug, Clone)]
pub(crate) struct RenewLoopArgs {
    #[arg(value_name = "PR")]
    pub(crate) pr: u64,
    #[arg(long, value_enum)]
    pub(crate) role: Role,
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

/// The per-cycle decision. Pure; `misses` carries the label-absent streak.
pub(crate) fn decide(
    watch_alive: bool,
    elapsed_secs: u64,
    cap_secs: u64,
    owner_code: i32,
    pr_state: Option<&(bool, Vec<String>)>,
    label: &str,
    misses: &mut u32,
) -> Step {
    if !watch_alive {
        return Step::Stop("the watched session ended");
    }
    if elapsed_secs >= cap_secs {
        return Step::Stop("the age cap passed");
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
pub(crate) fn spawn(ctx: &Ctx, comment_id: u64, watch_pid: u32, story: Option<u32>) -> String {
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
        .args(["--host", &ctx.me.host, "--sweep-id", &ctx.me.sweep])
        .args(["--comment-id", &comment_id.to_string()])
        .args(["--watch-pid", &watch_pid.to_string()])
        .args(["--interval", &interval.to_string()])
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
        &sweep_key(&ctx.me),
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
    let started = Instant::now();
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
        let step = decide(
            watch_alive,
            started.elapsed().as_secs(),
            args.max_age,
            owner_code,
            state.as_ref(),
            me.role.claim_label(),
            &mut misses,
        );
        match step {
            Step::Stop(why) => {
                say(&format!("renewer stopping: {why}"));
                return;
            }
            Step::Skip => say("cycle skipped (PR state unreadable, or claim label absent)"),
            Step::Renew => {
                let body = lease_body(&me, args.pr, Some(chrono::Utc::now()), &provenance);
                match gh.patch(args.comment_id, &body) {
                    Ok(()) => say("renewed"),
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
