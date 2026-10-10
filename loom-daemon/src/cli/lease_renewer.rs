//! `loom-daemon lease renewer` — single-owner bookkeeping and the per-cycle
//! completion decision for `sweep-lease-renew.sh`'s detached renewal loops
//! (#10229).
//!
//! ## What went wrong
//!
//! A renewal loop's only stop conditions used to be its watched pid dying, its
//! absolute age cap, and the own-yield guard. In-session sweeps watch the
//! operator's long-lived interactive `claude` process, so every issue a
//! session touched left a loop behind that kept PATCHing its lease for up to
//! a day: on joseph-superset 19 of 34 live loops were renewing issues that had
//! already CLOSED, about 600 REST calls an hour from the operator's personal
//! pool. Nothing stopped a second `start` for the same claim either.
//!
//! ## What this owns
//!
//! - **One renewer per (repo, host, sweep, issue).** `claim` records the
//!   loop's pid, its start-time identity and a per-start token in a small JSON
//!   file under [`STATE_DIR_ENV`] (default `~/.loom/lease-renew`, never inside
//!   a worktree), serialised by an exclusive `flock`. A live owner with a
//!   different token wins and its pid is printed, so the caller kills its own
//!   fresh loop. A dead owner, or a recycled pid whose start identity no
//!   longer matches, is taken over. Dead owners' records are swept on every
//!   claim.
//! - **The per-cycle gate.** `check` answers renew (0), stop (3) or skip
//!   this cycle (4). Ownership lost (a release tombstone, or a newer owner's
//!   token) stops. Then the issue state the shell read (`--issue-state`, an
//!   explicit `GET issues/N`, because the comments response carries no state)
//!   decides: `open` renews, `closed` stops for good even though the watched
//!   parent is still alive, and anything else (empty, malformed, a failed
//!   read) skips this cycle's PATCH without ending the loop, so a transient
//!   error is never mistaken for completion and never renews an unverified
//!   target.
//! - **`release`.** Ends this key's loop now: signals the owner (only after
//!   re-verifying its start identity, so a recycled pid is never killed) and
//!   leaves a tombstone the loop's next `check` honours even if the signal
//!   could not be delivered. Idempotent; a peer's key is never touched.
//!
//! ## Why the forge read stays in shell
//!
//! The state read goes through `forge_gh_perm_safe`, whose credential
//! escalation ladder (App 403 recovery, wrong-repo `GH_CONFIG_DIR`, personal
//! fallback) exists only in `lib/forge-helpers.sh`. Reading it there keeps the
//! state read on exactly the credentials and gh-shim attribution the
//! comments read and PATCH already use. This module only interprets it.
//!
//! ## Fail-open
//!
//! Every exit other than 3 and 4 (a binary predating this verb exits 2) means
//! "renew as before #10229", and a missing ownership record means the same: a
//! lost bookkeeping file must never stop a live claim's renewal, because that
//! lets a peer reclaim work that is still being done.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Overrides the ownership-record directory (tests, unusual `HOME`s).
pub(crate) const STATE_DIR_ENV: &str = "LOOM_LEASE_RENEW_STATE_DIR";

/// `check`: renew this cycle.
pub(crate) const EXIT_RENEW: i32 = 0;
/// `check`: stop the loop for good.
pub(crate) const EXIT_STOP: i32 = 3;
/// `check`: skip this cycle's PATCH, keep the loop.
pub(crate) const EXIT_SKIP: i32 = 4;

/// The four-part identity of one renewer. `repo` is resolved here, never
/// passed by the shell, so `claim`, `check` and `release` agree on it.
#[derive(clap::Args, Debug, Clone)]
pub(crate) struct KeyArgs {
    /// The leased issue.
    #[arg(value_name = "ISSUE")]
    pub(crate) issue: u64,

    /// The lease's `host=` value, as `start` resolved it (may be empty).
    #[arg(long)]
    pub(crate) host: Option<String>,

    /// The lease's `sweep=` value, as `start` resolved it (may be empty).
    #[arg(long)]
    pub(crate) sweep_id: Option<String>,
}

#[derive(clap::Subcommand)]
pub(crate) enum RenewerAction {
    /// Record `--pid` as this key's renewer unless a live peer already is.
    /// Prints the owning pid: `--pid` itself, or the live peer's (the caller
    /// then kills its own loop and reports the peer's pid).
    Claim {
        #[command(flatten)]
        key: KeyArgs,
        /// The freshly forked loop's pid.
        #[arg(long)]
        pid: u32,
        /// The token the loop will present to `check`.
        #[arg(long)]
        token: String,
    },
    /// Per-cycle gate: exit 0 renew, 3 stop for good, 4 skip this cycle.
    Check {
        #[command(flatten)]
        key: KeyArgs,
        /// The token this loop was started with.
        #[arg(long)]
        token: String,
        /// The issue's state as the caller read it (`open`/`closed`, or the
        /// raw issue JSON). Empty or unparseable = unverified (exit 4).
        #[arg(long, allow_hyphen_values = true)]
        issue_state: Option<String>,
    },
    /// End this key's renewer now. Without `--sweep-id`, the sweep is taken
    /// from `$LOOM_TERMINAL_ID` (`daemon-<id>`), else empty; without
    /// `--host`, any host on this machine matches.
    Release {
        #[command(flatten)]
        key: KeyArgs,
    },
    /// Stop a renewer safely (#11086): `stop <PID>` signals the pid only if it
    /// is provably a lease renewer (its command line is `<bash|sh|zsh|dash>
    /// sweep-lease-renew.sh start ...` or `loom-daemon lease renewer ...`, with a start identity
    /// that did not change while it was checked); `stop --issue N` ends every
    /// recorded renewer of issue N in this repo (any sweep unless `--sweep-id`
    /// is given) and never a peer issue's. Anything else is refused (exit 1)
    /// with no signal sent.
    Stop {
        /// The renewer loop's pid, as printed by `start`.
        #[arg(
            value_name = "PID",
            conflicts_with = "issue",
            required_unless_present = "issue"
        )]
        pid: Option<String>,
        /// Stop the renewer(s) of this issue instead of naming a pid.
        #[arg(long)]
        issue: Option<u64>,
        /// With `--issue`: only this host's renewer (default: any host).
        #[arg(long, requires = "issue")]
        host: Option<String>,
        /// With `--issue`: only this sweep's renewer (default: any sweep).
        #[arg(long, requires = "issue")]
        sweep_id: Option<String>,
    },
    /// Mark every inherited fd above 2 close-on-exec, then exec `CMD` (#10203).
    ///
    /// `sweep-lease-renew.sh start` re-enters itself through this so its
    /// detached loop holds no descriptor of the caller's (fd 3's saved stdout,
    /// an fd 10+ pipe, ...), which would otherwise keep a `worktree.sh N | tail`
    /// pipe open for the loop's whole lifetime. `--check` only proves this
    /// binary has the subcommand (exit 0), so an older daemon stays fail-open.
    SanitizeExec {
        /// Exit 0 without exec'ing anything.
        #[arg(long)]
        check: bool,
        /// The command to exec, after `--`.
        #[arg(last = true, value_name = "CMD")]
        cmd: Vec<String>,
    },
}

/// One renewer's ownership record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct OwnerRecord {
    pub(crate) repo: String,
    pub(crate) host: String,
    pub(crate) sweep: String,
    pub(crate) issue: u64,
    pub(crate) pid: u32,
    #[serde(default)]
    pub(crate) ident: String,
    pub(crate) token: String,
    /// Set by `release`: the loop must stop at its next `check`.
    #[serde(default)]
    pub(crate) released: bool,
}

/// The `(repo, host, sweep, issue)` key a record is filed under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Key {
    pub(crate) repo: String,
    pub(crate) host: String,
    pub(crate) sweep: String,
    pub(crate) issue: u64,
}

impl Key {
    fn file_stem(&self) -> String {
        use sha2::{Digest, Sha256};
        let raw = format!("{}|{}|{}|{}", self.repo, self.host, self.sweep, self.issue);
        hex::encode(Sha256::digest(raw.as_bytes()))[..32].to_string()
    }
}

/// Is `pid` alive *and* still the process whose start identity is `ident`?
pub(crate) type LiveProbe<'a> = &'a dyn Fn(u32, &str) -> bool;

/// The record directory.
pub(crate) struct Store {
    pub(crate) dir: PathBuf,
}

impl Store {
    fn from_env() -> Self {
        let dir = std::env::var(STATE_DIR_ENV)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
                Path::new(&home).join(".loom").join("lease-renew")
            });
        Self { dir }
    }

    fn path(&self, key: &Key) -> PathBuf {
        self.dir.join(format!("{}.owner", key.file_stem()))
    }

    /// An exclusive `flock` on the directory's lock file, held until dropped
    /// (or the process exits, so a crashed holder never wedges the next one).
    fn lock(&self) -> Result<File> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join(".lock"))?;
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd as _;
            // SAFETY: `flock` on a descriptor this function owns.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
                return Err(std::io::Error::last_os_error()).context("flock");
            }
        }
        Ok(file)
    }

    /// `Ok(None)` for a missing or unparseable record.
    fn read(&self, key: &Key) -> std::io::Result<Option<OwnerRecord>> {
        match std::fs::read_to_string(self.path(key)) {
            Ok(s) => Ok(serde_json::from_str(&s).ok()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Write-then-rename, so a concurrent `check` never reads half a record.
    fn write(&self, key: &Key, rec: &OwnerRecord) -> Result<()> {
        let path = self.path(key);
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec(rec)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    fn all(&self) -> Vec<(PathBuf, OwnerRecord)> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "owner"))
            .filter_map(|p| {
                let rec = serde_json::from_str(&std::fs::read_to_string(&p).ok()?).ok()?;
                Some((p, rec))
            })
            .collect()
    }
}

/// Record `pid` as `key`'s owner unless a live peer already is; returns the
/// owning pid. Caller must not hold the lock.
pub(crate) fn claim(
    store: &Store,
    key: &Key,
    pid: u32,
    token: &str,
    ident_of: &dyn Fn(u32) -> String,
    is_live: LiveProbe<'_>,
) -> Result<u32> {
    let _lock = store.lock()?;
    for (path, rec) in store.all() {
        if !is_live(rec.pid, &rec.ident) {
            let _ = std::fs::remove_file(path);
        }
    }
    if let Some(rec) = store.read(key)? {
        if rec.token != token && !rec.released && is_live(rec.pid, &rec.ident) {
            return Ok(rec.pid);
        }
    }
    let rec = OwnerRecord {
        repo: key.repo.clone(),
        host: key.host.clone(),
        sweep: key.sweep.clone(),
        issue: key.issue,
        pid,
        ident: ident_of(pid),
        token: token.to_string(),
        released: false,
    };
    store.write(key, &rec)?;
    Ok(pid)
}

/// `open`/`closed` out of a bare word or an issue JSON object.
pub(crate) fn parse_state(raw: &str) -> Option<&'static str> {
    let t = raw.trim();
    let word = if t.starts_with('{') {
        let v: serde_json::Value = serde_json::from_str(t).ok()?;
        v.get("state")?.as_str()?.to_ascii_lowercase()
    } else {
        t.trim_matches('"').to_ascii_lowercase()
    };
    match word.as_str() {
        "open" => Some("open"),
        "closed" => Some("closed"),
        _ => None,
    }
}

/// The per-cycle decision: `(exit code, reason to log)`.
pub(crate) fn check(
    store: &Store,
    key: &Key,
    token: &str,
    issue_state: Option<&str>,
) -> (i32, Option<String>) {
    // An unreadable directory or a missing record is fail-open (see module docs).
    if let Ok(Some(rec)) = store.read(key) {
        if rec.token != token {
            let why = format!("ownership passed to renewer pid {} (#10229)", rec.pid);
            return (EXIT_STOP, Some(why));
        }
        if rec.released {
            return (EXIT_STOP, Some("released (#10229)".into()));
        }
    }
    let Some(raw) = issue_state else {
        return (EXIT_RENEW, None);
    };
    match parse_state(raw) {
        Some("open") => (EXIT_RENEW, None),
        Some(_) => (EXIT_STOP, Some("issue is closed (#10229)".into())),
        None => (
            EXIT_SKIP,
            Some("issue state unverified this cycle; skipping renewal (#10229)".into()),
        ),
    }
}

/// Tombstone (and signal, when `is_live` confirms it) every owner of
/// `(repo, issue, sweep)` — and `host`, when given. Returns how many matched.
pub(crate) fn release(
    store: &Store,
    repo: &str,
    issue: u64,
    host: Option<&str>,
    sweep: &str,
    is_live: LiveProbe<'_>,
    signal: &dyn Fn(u32),
) -> Result<usize> {
    end_owners(store, repo, issue, host, Some(sweep), is_live, signal)
}

/// Like [`release`], but `sweep: None` matches every sweep's owner of
/// `(repo, issue)` (#11086). A different issue's owner never matches.
pub(crate) fn end_owners(
    store: &Store,
    repo: &str,
    issue: u64,
    host: Option<&str>,
    sweep: Option<&str>,
    is_live: LiveProbe<'_>,
    signal: &dyn Fn(u32),
) -> Result<usize> {
    let _lock = store.lock()?;
    let mut n = 0;
    for (path, mut rec) in store.all() {
        if rec.repo != repo || rec.issue != issue || sweep.is_some_and(|s| s != rec.sweep) {
            continue;
        }
        if host.is_some_and(|h| h != rec.host) {
            continue;
        }
        n += 1;
        if is_live(rec.pid, &rec.ident) {
            signal(rec.pid);
        }
        rec.released = true;
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec(&rec)?)?;
        std::fs::rename(&tmp, &path)?;
    }
    Ok(n)
}

/// The basename of a path-like argv element.
fn argv_base(a: &str) -> &str {
    a.rsplit('/').next().unwrap_or(a)
}

/// Is this argv a lease renewer this tooling started? Either
/// `<shell> [-flags] .../sweep-lease-renew.sh start ...` (the detached loop is
/// a subshell of that `#!/usr/bin/env bash` script, so it shares its argv) or
/// `.../loom-daemon lease renewer ...`. The program must be a shell
/// (bash/sh/zsh/dash by basename) running the script FILE: any other program
/// (an editor, another interpreter) and command-string / stdin forms (`-c`,
/// `-s`) or option clusters that may consume the script path as their own
/// argument (`-o`, `-O`, any `--long` option) are refused (#11086).
pub(crate) fn is_renewer_argv(argv: &[String]) -> bool {
    let is_shell = argv
        .first()
        .is_some_and(|a| matches!(argv_base(a), "bash" | "sh" | "zsh" | "dash"));
    let script_start = is_shell && {
        let rest = argv.get(1..).unwrap_or_default();
        let first = rest.iter().position(|a| !a.starts_with('-'));
        // Only plain short-flag clusters may precede the script.
        let flags_ok = rest[..first.unwrap_or(rest.len())].iter().all(|f| {
            f.len() > 1
                && !f.starts_with("--")
                && f[1..]
                    .chars()
                    .all(|c| c.is_ascii_alphabetic() && !"csoO".contains(c))
        });
        flags_ok
            && first.is_some_and(|i| {
                argv_base(&rest[i]) == "sweep-lease-renew.sh"
                    && rest.get(i + 1).is_some_and(|a| a == "start")
            })
    };
    let daemon = argv.first().is_some_and(|a| argv_base(a) == "loom-daemon")
        && argv.get(1).is_some_and(|a| a == "lease")
        && argv.get(2).is_some_and(|a| a == "renewer");
    script_start || daemon
}

/// A process's argv: `/proc/<pid>/cmdline` on Linux, `ps -o command=` elsewhere
/// (macOS). `None` when the process is gone or unreadable.
pub(crate) fn process_argv(pid: u32) -> Option<Vec<String>> {
    if let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) {
        let argv: Vec<String> = raw
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect();
        return (!argv.is_empty()).then_some(argv);
    }
    let out = Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let argv: Vec<String> = s.split_whitespace().map(str::to_string).collect();
    (out.status.success() && !argv.is_empty()).then_some(argv)
}

/// Result of [`verify_renewer`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verified {
    Gone,
    Unreadable,
    /// Not a renewer; carries the program's basename only.
    NotRenewer(String),
    IdentityChanged,
    Renewer,
}

/// Is `pid` provably a renewer? Its start identity is read before and after
/// its argv, and must be identical (a pid recycled mid-check is ambiguous).
/// With `expected` (the identity recorded when the renewer was claimed), the
/// first read must also match it, so a pid reused by a different renewer is
/// refused. The one check behind both `stop <PID>` and `stop --issue` (#11086).
pub(crate) fn verify_renewer(
    pid: u32,
    expected: Option<&str>,
    ident_of: &dyn Fn(u32) -> Option<String>,
    argv_of: &dyn Fn(u32) -> Option<Vec<String>>,
) -> Verified {
    let Some(before) = ident_of(pid) else {
        return Verified::Gone;
    };
    if expected.is_some_and(|e| e != before) {
        return Verified::IdentityChanged;
    }
    let Some(argv) = argv_of(pid) else {
        return Verified::Unreadable;
    };
    if !is_renewer_argv(&argv) {
        let program = argv.first().map_or("?", |a| argv_base(a));
        return Verified::NotRenewer(program.to_string());
    }
    if ident_of(pid).as_deref() != Some(before.as_str()) {
        return Verified::IdentityChanged;
    }
    Verified::Renewer
}

/// May `stop --issue` signal the recorded owner `pid`? Only with a nonempty
/// recorded identity that still matches before and after the argv read, and a
/// renewer's argv (#11086).
pub(crate) fn issue_owner_signalable(
    pid: u32,
    recorded_ident: &str,
    ident_of: &dyn Fn(u32) -> Option<String>,
    argv_of: &dyn Fn(u32) -> Option<Vec<String>>,
) -> bool {
    !recorded_ident.is_empty()
        && verify_renewer(pid, Some(recorded_ident), ident_of, argv_of) == Verified::Renewer
}

/// What `stop <PID>` decided.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StopOutcome {
    Stopped,
    /// Nothing is running under that pid; nothing was signalled (reported as a
    /// refusal: the number may be an issue number typed by mistake).
    NotRunning,
    /// Refused, with the reason; nothing was signalled.
    Refused(String),
}

/// Signal `pid_arg` only when it is provably a renewer: a plain number above 1,
/// not this process, whose argv is a renewer's, and whose start identity is the
/// same before and after the argv was read (so a pid recycled mid-check is
/// ambiguous and refused).
pub(crate) fn stop_pid(
    pid_arg: &str,
    self_pid: u32,
    ident_of: &dyn Fn(u32) -> Option<String>,
    argv_of: &dyn Fn(u32) -> Option<Vec<String>>,
    signal: &dyn Fn(u32),
) -> StopOutcome {
    let Ok(pid) = pid_arg.trim().parse::<u32>() else {
        return StopOutcome::Refused(format!("`{pid_arg}` is not a numeric pid"));
    };
    if pid <= 1 || pid == self_pid {
        return StopOutcome::Refused(format!("pid {pid} is not a lease renewer"));
    }
    match verify_renewer(pid, None, ident_of, argv_of) {
        Verified::Gone => return StopOutcome::NotRunning,
        Verified::Unreadable => {
            return StopOutcome::Refused(format!("cannot read the command line of pid {pid}"));
        }
        Verified::NotRenewer(program) => {
            // Only the program's basename: the full argv of an unrelated
            // process may carry credentials and must not reach logs (#11086).
            return StopOutcome::Refused(format!(
                "pid {pid} is not a lease renewer (program: {program}); for an issue number use `stop --issue N`"
            ));
        }
        Verified::IdentityChanged => {
            return StopOutcome::Refused(format!("pid {pid} changed identity while being checked"));
        }
        Verified::Renewer => {}
    }
    signal(pid);
    StopOutcome::Stopped
}

/// The repo half of the key: `$LOOM_REPO`, else origin's GitHub slug (or raw
/// URL), else the checkout root, else `cwd` — lowercased, so a `LOOM_REPO`
/// spelling and the remote URL of the same repository agree.
pub(crate) fn repo_identity(env_repo: Option<&str>, cwd: &Path) -> String {
    if let Some(r) = env_repo.map(str::trim).filter(|r| !r.is_empty()) {
        return r.to_ascii_lowercase();
    }
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .ok()?;
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (out.status.success() && !s.is_empty()).then_some(s)
    };
    if let Some(url) = git(&["remote", "get-url", "origin"]) {
        let slug = loom_daemon::release_resolve::host::slug_from_remote_url(&url);
        return slug.unwrap_or(url).to_ascii_lowercase();
    }
    git(&["rev-parse", "--show-toplevel"])
        .unwrap_or_else(|| cwd.display().to_string())
        .to_ascii_lowercase()
}

/// A process's start-time identity, in the same `starttime:`/`lstart:` shape
/// `sweep-lease-renew.sh`'s `pid_start_identity` uses (#7825).
pub(crate) fn start_identity(pid: u32) -> Option<String> {
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        let rest = &stat[stat.rfind(')')? + 1..];
        return rest
            .split_whitespace()
            .nth(19)
            .map(|t| format!("starttime:{t}"));
    }
    let out = Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    (!s.is_empty()).then(|| format!("lstart:{s}"))
}

fn owner_is_live(pid: u32, ident: &str) -> bool {
    loom_daemon::live_claim::pid_is_live_process(pid)
        && (ident.is_empty() || start_identity(pid).as_deref() == Some(ident))
}

impl RenewerAction {
    pub(crate) fn run(self) -> Result<()> {
        if let Self::SanitizeExec { check, cmd } = &self {
            if *check {
                return Ok(());
            }
            let Some((program, rest)) = cmd.split_first() else {
                anyhow::bail!("sanitize-exec: no command given after `--`");
            };
            super::lease_ensure::mark_inherited_fds_cloexec();
            let err = std::os::unix::process::CommandExt::exec(
                std::process::Command::new(program).args(rest),
            );
            anyhow::bail!("sanitize-exec: cannot exec {program}: {err}");
        }
        let store = Store::from_env();
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let repo = repo_identity(std::env::var("LOOM_REPO").ok().as_deref(), &cwd);
        let key_of = |k: &KeyArgs| Key {
            repo: repo.clone(),
            host: k.host.clone().unwrap_or_default(),
            sweep: k.sweep_id.clone().unwrap_or_default(),
            issue: k.issue,
        };
        match self {
            Self::Claim { key, pid, token } => {
                let ident_of = |p: u32| start_identity(p).unwrap_or_default();
                let owner = claim(&store, &key_of(&key), pid, &token, &ident_of, &owner_is_live)?;
                if owner != pid {
                    eprintln!(
                        "sweep-lease-renew: a renewer (pid {owner}) already owns issue #{}; not \
                         starting a duplicate (#10229)",
                        key.issue
                    );
                }
                println!("{owner}");
                Ok(())
            }
            Self::Check {
                key,
                token,
                issue_state,
            } => {
                let (code, why) = check(&store, &key_of(&key), &token, issue_state.as_deref());
                if let Some(why) = why {
                    eprintln!("sweep-lease-renew: renewal loop for issue #{}: {why}", key.issue);
                }
                std::process::exit(code)
            }
            Self::SanitizeExec { .. } => unreachable!("handled before the store is opened"),
            Self::Stop {
                pid,
                issue,
                host,
                sweep_id,
            } => {
                let signal = |pid: u32| {
                    if let Ok(pid) = i32::try_from(pid) {
                        // SAFETY: plain kill(2) on a pid verified as a renewer by argv + identity.
                        unsafe { libc::kill(pid, libc::SIGTERM) };
                    }
                };
                if let Some(issue) = issue {
                    let live = |p: u32, ident: &str| {
                        loom_daemon::live_claim::pid_is_live_process(p)
                            && issue_owner_signalable(p, ident, &start_identity, &process_argv)
                    };
                    let n = end_owners(
                        &store,
                        &repo,
                        issue,
                        host.as_deref(),
                        sweep_id.as_deref(),
                        &live,
                        &signal,
                    )?;
                    eprintln!("stopped renewal for issue #{issue} ({n} renewer(s) recorded)");
                    return Ok(());
                }
                let arg = pid.unwrap_or_default();
                match stop_pid(&arg, std::process::id(), &start_identity, &process_argv, &signal) {
                    StopOutcome::Stopped => Ok(()),
                    // Not-running is a refusal too: a pid that is not a live renewer
                    // may be an issue number typed by mistake (#11086).
                    StopOutcome::NotRunning => anyhow::bail!(
                        "sweep-lease-renew: no process {arg} is running; nothing signalled \
                         (to stop an issue's renewer use `stop --issue N`)"
                    ),
                    StopOutcome::Refused(why) => {
                        anyhow::bail!("sweep-lease-renew: refusing to stop: {why}")
                    }
                }
            }
            Self::Release { key } => {
                let sweep = key.sweep_id.clone().unwrap_or_else(|| {
                    std::env::var("LOOM_TERMINAL_ID")
                        .ok()
                        .and_then(|t| t.strip_prefix("daemon-").map(str::to_string))
                        .unwrap_or_default()
                });
                let signal = |pid: u32| {
                    if let Ok(pid) = i32::try_from(pid) {
                        // SAFETY: plain kill(2) on a pid just re-verified by identity.
                        unsafe { libc::kill(pid, libc::SIGTERM) };
                    }
                };
                let n = release(
                    &store,
                    &repo,
                    key.issue,
                    key.host.as_deref(),
                    &sweep,
                    &owner_is_live,
                    &signal,
                )?;
                eprintln!("released renewal for issue #{} ({n} renewer(s))", key.issue);
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests;
