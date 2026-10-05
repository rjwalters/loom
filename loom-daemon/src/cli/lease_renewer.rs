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
    let _lock = store.lock()?;
    let mut n = 0;
    for (path, mut rec) in store.all() {
        if rec.repo != repo || rec.issue != issue || rec.sweep != sweep {
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
