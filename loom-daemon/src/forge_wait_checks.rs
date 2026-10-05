//! `loom-daemon forge wait-checks <PR|SHA>` — an ETag'd, backed-off CI wait
//! for agents (#10330, part of the GitHub API reduction epic #10332).
//!
//! # Why
//!
//! Role prompts waited on CI with a copied `gh pr checks` loop every 60s for
//! up to 30 minutes: a GraphQL read per poll, never conditional, never backed
//! off. This command does the same wait with REST reads that are revalidated
//! by ETag (an unchanged poll is a free `304`, see [`reads`]) and an interval
//! that backs off from 30s to 120s.
//!
//! # Contract
//!
//! Exactly one sentinel line on stdout, then (RED only) one
//! `<name>\t<url>\t<run_id>` line per failing check on stderr. Callers MUST
//! branch on the sentinel, never on the exit code — clap's own usage error is
//! also exit 2, and an older binary has no `wait-checks` at all. Nothing of
//! this module's is written to stderr before the sentinel.
//!
//! | Sentinel | Exit |
//! |---|---|
//! | `LOOM-CHECKS-GREEN <sha>` | 0 |
//! | `LOOM-CHECKS-NONE <sha>` | 0 |
//! | `LOOM-CHECKS-RED <sha> <name>[,<name>…]` | 1 |
//! | `LOOM-CHECKS-TIMEOUT <sha> <pending,…>` | 2 |
//! | `LOOM-CHECKS-ERROR <reason>` | 3 |
//! | `LOOM-CHECKS-HEAD-MOVED <old> <new>` | 4 |
//!
//! `--timeout 0` takes exactly one poll (a snapshot). A RED is reported as
//! soon as a failing check is terminal, without waiting for the rest.
//!
//! # Zero rows
//!
//! An empty rollup is ambiguous (#6169), so it goes through the same bounded
//! settle `merge-pr.sh --auto` uses ([`crate::merge_pr::zero_checks`]):
//! `NONE` only after several empty reads AND a successful lookup that the base
//! branch requires no contexts; with required contexts (or a failed lookup)
//! the wait runs to `TIMEOUT` — never `NONE`. That also covers a fork PR whose
//! workflow awaits approval (#9257).
//!
//! # Backoff, and why it does not reset
//!
//! The first poll is immediate; then 30s, ×1.5 per poll, capped at 120s
//! (`--min-interval`/`--max-interval`, `LOOM_WAIT_CHECKS_MIN`/`_MAX`). The
//! curated design also reset to the minimum whenever the rollup changed; that
//! was dropped because it breaks the issue's own request budget — on a
//! 15-minute run with 6 rollup changes a reset policy takes 15–20 polls
//! against a cap of 12, while plain geometric backoff takes 11.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::merge_pr::zero_checks::{self, Action, Required};

pub mod reads;
pub mod verdict;

#[cfg(test)]
mod tests;

pub use verdict::{Failed, Outcome};

use reads::{GhReads, ReadError};
use verdict::Decision;

/// Default `--timeout`, seconds (the retired prompt loops' 30-minute cap).
pub const DEFAULT_TIMEOUT: u64 = 1800;
/// Default first interval between polls, seconds.
pub const DEFAULT_MIN_INTERVAL: u64 = 30;
/// Default interval cap, seconds.
pub const DEFAULT_MAX_INTERVAL: u64 = 120;
/// Consecutive transient read failures tolerated before `ERROR`.
const MAX_TRANSIENT_FAILURES: u32 = 3;

/// What to wait on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selector {
    /// A PR number: its head is re-read every poll (HEAD-MOVED detection).
    Pr(u32),
    /// A commit SHA (7–40 hex characters).
    Sha(String),
}

/// `<PR|SHA>`: all digits is a PR number; otherwise 7–40 hex characters.
///
/// # Errors
///
/// Anything else.
pub fn parse_selector(raw: &str) -> Result<Selector, String> {
    let raw = raw.trim();
    if !raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit()) {
        return match raw.parse::<u32>() {
            Ok(n) if n > 0 => Ok(Selector::Pr(n)),
            _ => Err(format!("bad-selector {raw}")),
        };
    }
    if (7..=40).contains(&raw.len()) && raw.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(Selector::Sha(raw.to_ascii_lowercase()));
    }
    Err(format!("bad-selector {raw}"))
}

/// The geometric poll interval: `min`, then ×1.5 per poll, capped at `max`.
#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    next: u64,
    max: u64,
}

impl Backoff {
    #[must_use]
    pub fn new(min: u64, max: u64) -> Self {
        let min = min.max(1);
        Self {
            next: min,
            max: max.max(min),
        }
    }

    /// The interval to sleep now; advances the schedule.
    pub fn take(&mut self) -> u64 {
        let now = self.next;
        self.next = (self.next * 3 / 2).max(self.next + 1).min(self.max);
        now
    }
}

/// Time source for the wait loop; tests drive a fake one.
pub trait Clock {
    fn elapsed(&self) -> Duration;
    fn sleep(&mut self, d: Duration);
}

struct RealClock(Instant);

impl Clock for RealClock {
    fn elapsed(&self) -> Duration {
        self.0.elapsed()
    }
    fn sleep(&mut self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// The wait's knobs (everything but the reads and the clock).
#[derive(Debug, Clone)]
pub struct Opts {
    pub timeout: Duration,
    pub required_only: bool,
    /// Base branch for the required-context lookup in SHA mode; `None` =
    /// the repository's default branch.
    pub base: Option<String>,
    pub min_interval: u64,
    pub max_interval: u64,
    pub settle_polls: u64,
    pub settle_interval: u64,
}

/// CLI arguments, as parsed by `cli::forge_action`.
#[derive(Debug, Clone)]
pub struct WaitArgs {
    pub selector: String,
    pub repo: Option<String>,
    pub base: Option<String>,
    pub timeout: u64,
    pub required_only: bool,
    pub min_interval: Option<u64>,
    pub max_interval: Option<u64>,
}

/// `forge wait-checks` entry point: print the sentinel (and detail), exit.
pub fn cli_entrypoint(args: WaitArgs) -> ! {
    let (outcome, notes) = run_cli(&args);
    finish(&outcome, &notes)
}

fn run_cli(args: &WaitArgs) -> (Outcome, Vec<String>) {
    if crate::forge_cmd::detect_forge(None) == crate::forge_cmd::ForgeType::Gitea {
        return (Outcome::Error("gitea-unsupported".into()), Vec::new());
    }
    let selector = match parse_selector(&args.selector) {
        Ok(s) => s,
        Err(e) => return (Outcome::Error(e), Vec::new()),
    };
    let env_repo = env("LOOM_REPO");
    let repo = args.repo.clone().or(env_repo);
    let mut reads = match GhReads::new(
        PathBuf::from(crate::gh_invocation::gh_bin()),
        std::env::current_dir().ok(),
        crate::forge_etag_store::disk_cache_dir(),
        repo.as_deref(),
    ) {
        Ok(r) => r,
        Err(e) => return (Outcome::Error(e), Vec::new()),
    };
    let env_secs = |k: &str| env(k).and_then(|v| v.trim().parse::<u64>().ok());
    let min_interval = args
        .min_interval
        .or_else(|| env_secs("LOOM_WAIT_CHECKS_MIN"))
        .unwrap_or(DEFAULT_MIN_INTERVAL);
    let opts = Opts {
        timeout: Duration::from_secs(args.timeout),
        required_only: args.required_only,
        base: args.base.clone(),
        min_interval,
        max_interval: args
            .max_interval
            .or_else(|| env_secs("LOOM_WAIT_CHECKS_MAX"))
            .unwrap_or(DEFAULT_MAX_INTERVAL),
        settle_polls: zero_checks::settle_polls(env("LOOM_ZERO_CHECKS_SETTLE_POLLS").as_deref()),
        settle_interval: zero_checks::settle_interval(
            env("LOOM_ZERO_CHECKS_SETTLE_INTERVAL").as_deref(),
            min_interval,
        ),
    };
    wait(&mut reads, &selector, &opts, &mut RealClock(Instant::now()))
}

fn finish(outcome: &Outcome, notes: &[String]) -> ! {
    use std::io::Write;
    println!("{}", outcome.sentinel());
    let _ = std::io::stdout().flush();
    for line in outcome.detail().iter().chain(notes) {
        eprintln!("{line}");
    }
    std::process::exit(outcome.exit_code())
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// What one poll concluded.
enum Poll {
    Done(Outcome),
    /// Not settled: these are still pending. `sleep` overrides the backoff
    /// (the zero-row settle's short spacing).
    Pending {
        sha: String,
        names: Vec<String>,
        sleep: Option<u64>,
    },
}

/// Per-wait state carried across polls.
struct State {
    pinned_head: Option<String>,
    base_ref: Option<String>,
    /// `None` = not looked up yet; `Some(None)` = the lookup failed.
    required: Option<Option<Vec<String>>>,
    zero_polls: u64,
    notes: Vec<String>,
}

/// The wait loop. Returns the outcome and any notes for stderr.
pub fn wait<C: Clock>(
    reads: &mut GhReads,
    selector: &Selector,
    opts: &Opts,
    clock: &mut C,
) -> (Outcome, Vec<String>) {
    let mut st = State {
        pinned_head: None,
        base_ref: opts.base.clone(),
        required: None,
        zero_polls: 0,
        notes: Vec::new(),
    };
    let mut backoff = Backoff::new(opts.min_interval, opts.max_interval);
    let mut transient: u32 = 0;
    loop {
        let deadline_reached = clock.elapsed() >= opts.timeout;
        let (sha, names, sleep) = match poll(reads, selector, opts, &mut st, deadline_reached) {
            Ok(Poll::Done(o)) => return (o, st.notes),
            Err(ReadError::Fatal(why)) => return (Outcome::Error(why), st.notes),
            Err(ReadError::Transient(why)) => {
                transient += 1;
                if transient >= MAX_TRANSIENT_FAILURES || deadline_reached {
                    return (Outcome::Error(format!("read-failed: {why}")), st.notes);
                }
                (None, Vec::new(), None)
            }
            Ok(Poll::Pending { sha, names, sleep }) => {
                transient = 0;
                (Some(sha), names, sleep)
            }
        };
        if deadline_reached {
            let sha = sha.unwrap_or_default();
            return (
                Outcome::Timeout {
                    sha,
                    pending: names,
                },
                st.notes,
            );
        }
        let remaining = opts.timeout.saturating_sub(clock.elapsed());
        let secs = sleep.unwrap_or_else(|| backoff.take());
        clock.sleep(Duration::from_secs(secs).min(remaining));
    }
}

fn poll(
    reads: &mut GhReads,
    selector: &Selector,
    opts: &Opts,
    st: &mut State,
    deadline_reached: bool,
) -> Result<Poll, ReadError> {
    let sha = match selector {
        Selector::Sha(s) => s.clone(),
        Selector::Pr(n) => {
            let head = reads.pull(*n)?;
            st.base_ref.get_or_insert(head.base_ref);
            match &st.pinned_head {
                Some(old) if *old != head.sha => {
                    return Ok(Poll::Done(Outcome::HeadMoved {
                        old: old.clone(),
                        new: head.sha,
                    }));
                }
                Some(_) => {}
                None => st.pinned_head = Some(head.sha.clone()),
            }
            head.sha
        }
    };
    let runs = reads.check_runs(&sha)?;
    let status = reads.statuses(&sha)?;
    let rollup = verdict::fold(&runs, &status).map_err(ReadError::Fatal)?;

    if rollup.total == 0 {
        st.zero_polls += 1;
        let required = resolve_required(reads, st)?;
        let req_state = match &required {
            None => Required::LookupFailed,
            Some(r) if r.is_empty() => Required::None,
            Some(_) => Required::Present,
        };
        let d = zero_checks::decide(&zero_checks::Inputs {
            pr: sha.clone(),
            base_ref: st.base_ref.clone().unwrap_or_default(),
            polls: st.zero_polls,
            required: req_state,
            deadline_reached,
            settle_polls: opts.settle_polls,
            settle_interval: opts.settle_interval,
            poll_interval: opts.min_interval,
            timeout: opts.timeout.as_secs(),
        });
        let names = required.unwrap_or_default();
        return Ok(match d.action {
            Action::Settle => Poll::Done(Outcome::NoChecks { sha }),
            Action::TimedOut => Poll::Done(Outcome::Timeout {
                sha,
                pending: names,
            }),
            Action::Wait => Poll::Pending {
                sha,
                names,
                sleep: (req_state == Required::None).then_some(d.sleep_secs),
            },
        });
    }
    st.zero_polls = 0;

    // Required contexts are looked up only when a verdict needs them.
    let needs_required =
        opts.required_only || (rollup.failing.is_empty() && rollup.pending.is_empty());
    let required = if needs_required {
        resolve_required(reads, st)?
    } else {
        Some(Vec::new())
    };
    if opts.required_only && required.is_none() {
        return Err(ReadError::Fatal("required-lookup-failed".into()));
    }
    Ok(match verdict::decide(&rollup, required.as_deref(), opts.required_only) {
        Decision::Green => Poll::Done(Outcome::Green { sha }),
        Decision::Red(names) => {
            let failed = rollup
                .failed_detail
                .into_iter()
                .filter(|f| names.contains(&f.name))
                .collect();
            Poll::Done(Outcome::Red { sha, failed })
        }
        Decision::Pending(names) => Poll::Pending {
            sha,
            names,
            sleep: None,
        },
    })
}

/// The required-context set, looked up once per wait. `Ok(None)` = the
/// lookup failed (recorded as a note; never read as "nothing required").
fn resolve_required(reads: &mut GhReads, st: &mut State) -> Result<Option<Vec<String>>, ReadError> {
    if st.required.is_none() {
        let base = match &st.base_ref {
            Some(b) => b.clone(),
            None => {
                let b = reads.default_branch()?;
                st.base_ref = Some(b.clone());
                b
            }
        };
        st.required = Some(match reads.required(&base) {
            Ok((contexts, notices)) => {
                st.notes.extend(notices);
                Some(contexts)
            }
            Err(why) => {
                st.notes
                    .push(format!("wait-checks: required-context lookup for {base} failed: {why}"));
                None
            }
        });
    }
    Ok(st.required.clone().flatten())
}
