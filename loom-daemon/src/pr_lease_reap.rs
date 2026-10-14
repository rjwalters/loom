//! Releasing the PR leases of a role run the daemon had to terminate
//! (#11931).
//!
//! ## The gap
//!
//! A Judge (or Doctor) that wins `loom-daemon lease pr claim` is expected to
//! end with `lease pr release`, and a Judge's release is what records a pass
//! that ended without a verdict for the no-verdict backstop
//! ([`crate::judge_backstop`], #11855). A run the role runner kills at its
//! timeout ceiling never gets there: its lease simply ages out at the TTL,
//! nothing records the pass, and the stale `loom:reviewing` it leaves behind
//! is reclaimed by the next host, which hits the same wall. One PR took five
//! Judge runs in two hours across three hosts that way.
//!
//! ## The record
//!
//! The daemon gives each role child a per-run file in [`LEASE_FILE_ENV`].
//! `lease pr claim` appends one [`Held`] line to it for every lease it wins,
//! naming the exact identity (PR, role, published host, sweep) the lease
//! comment carries; `lease pr release` drops the line again. When the run
//! ends by timeout, the daemon runs the hidden `lease pr reap` verb once per
//! line still there ([`Reaper::reap`]). `reap` is `release` on the dead
//! claimant's behalf: it stops its renewer, rewrites its lease marker to
//! `loom:pr-lease-released`, and for a Judge whose PR is still waiting for a
//! verdict it writes the same no-verdict record a release would, plus
//! `reaped=timeout`. The record is written unconditionally on CI state: a
//! pass that burned the whole ceiling is exactly the pass the backstop exists
//! to count.
//!
//! The reap runs immediately after the termination, while the dead lease is
//! still live (renewed at most one renewal interval ago), so no peer can have
//! claimed the PR in between: the record is on the PR before any other host
//! can take it, and every host's next `lease pr claim`, `pr-queue --role
//! judge` and lane assignment reads it.
//!
//! The file is removed when the run ends, on every outcome. Everything here
//! is best-effort: a missing file, an unreadable line or a failed reap leaves
//! the lease to age out at the TTL, as before.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The child's env var naming the file its won PR leases are listed in.
pub const LEASE_FILE_ENV: &str = "LOOM_ROLE_PR_LEASE_FILE";

/// Why a lease was reaped, as written to the record's `reaped=` field.
pub const REASON_TIMEOUT: &str = "timeout";

/// The deadline for one `lease pr reap` child: a bounded lease read (three
/// attempts), one PR read and one PATCH, each with its own `gh` deadline.
pub const REAP_TIMEOUT: Duration = Duration::from_secs(150);

const POLL: Duration = Duration::from_millis(200);

/// One PR lease a role run won: the identity its lease comment names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held {
    pub pr: u64,
    /// `judge` or `doctor`.
    pub role: String,
    /// The published host id the lease marker carries.
    pub host: String,
    pub sweep: String,
}

impl Held {
    /// `<pr> <role> <host> <sweep>`.
    #[must_use]
    pub fn line(&self) -> String {
        format!("{} {} {} {}", self.pr, self.role, self.host, self.sweep)
    }

    /// Parse one [`line`](Self::line). Anything else (a role other than
    /// `judge`/`doctor`, a missing or extra field) is `None`.
    #[must_use]
    pub fn parse(line: &str) -> Option<Self> {
        let mut tokens = line.split_whitespace();
        let pr = tokens.next()?.parse().ok()?;
        let role = tokens.next()?;
        if !matches!(role, "judge" | "doctor") {
            return None;
        }
        let host = tokens.next()?;
        let sweep = tokens.next()?;
        if tokens.next().is_some() {
            return None;
        }
        Some(Self {
            pr,
            role: role.to_string(),
            host: host.to_string(),
            sweep: sweep.to_string(),
        })
    }

    /// Whether every field is a single non-empty token (a line round-trips).
    fn writable(&self) -> bool {
        [&self.role, &self.host, &self.sweep]
            .iter()
            .all(|f| !f.is_empty() && !f.contains(char::is_whitespace))
    }

    /// The `lease pr reap` argv (after the binary) for this lease.
    #[must_use]
    pub fn reap_args(&self, root: &Path, reason: &str) -> Vec<String> {
        vec![
            "lease".into(),
            "pr".into(),
            "reap".into(),
            self.pr.to_string(),
            "--role".into(),
            self.role.clone(),
            "--host".into(),
            self.host.clone(),
            "--sweep-id".into(),
            self.sweep.clone(),
            "--reason".into(),
            reason.into(),
            "--workspace".into(),
            root.to_string_lossy().into_owned(),
        ]
    }
}

/// Every [`Held`] line in `text`, in order, duplicates dropped.
#[must_use]
pub fn parse_file(text: &str) -> Vec<Held> {
    let mut out: Vec<Held> = Vec::new();
    for held in text.lines().filter_map(Held::parse) {
        if !out.contains(&held) {
            out.push(held);
        }
    }
    out
}

/// Where run `item` of `root` lists its won PR leases.
#[must_use]
pub fn path_for(root: &Path, item: &str) -> PathBuf {
    let name: String = item
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    root.join(".loom/state/pr-lease-held")
        .join(format!("{name}.leases"))
}

/// The lease file the launching role run named in [`LEASE_FILE_ENV`], if
/// any (an attended session, or a sweep child, has none).
#[must_use]
pub fn file_from_env() -> Option<PathBuf> {
    std::env::var_os(LEASE_FILE_ENV)
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
}

/// `lease pr claim`, on a won claim: list `held` in the run's lease file.
/// Best-effort; never fails a claim.
pub fn record_in(path: &Path, held: &Held) {
    if !held.writable() {
        return;
    }
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    if parse_file(&existing).contains(held) {
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{}", held.line());
    }
}

/// `lease pr release`: the claimant released `held` itself, so the daemon
/// has nothing to reap for it. Best-effort.
pub fn forget_in(path: &Path, held: &Held) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let kept: Vec<String> = parse_file(&text)
        .into_iter()
        .filter(|h| h != held)
        .map(|h| h.line() + "\n")
        .collect();
    let _ = std::fs::write(path, kept.concat());
}

/// One role run's lease file: handed to the child at launch, reaped if the
/// run is terminated, removed when the run ends (on drop).
#[derive(Debug)]
pub struct Reaper {
    root: PathBuf,
    path: PathBuf,
}

impl Reaper {
    /// The lease file for run `item` of `root`. Any file left at that path by
    /// an earlier run is cleared, so a reap only ever sees this run's leases.
    #[must_use]
    pub fn plan(root: &Path, item: &str) -> Self {
        let path = path_for(root, item);
        let _ = std::fs::remove_file(&path);
        Self {
            root: root.to_path_buf(),
            path,
        }
    }

    /// Give the launching child its lease-file path.
    pub fn apply(&self, cmd: &mut Command) {
        cmd.env(LEASE_FILE_ENV, &self.path);
    }

    /// The file's path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The leases this run still holds by its own account.
    #[must_use]
    pub fn held(&self) -> Vec<Held> {
        parse_file(&std::fs::read_to_string(&self.path).unwrap_or_default())
    }

    /// Reap every lease still listed, running `bin lease pr reap …` for each
    /// with `prepare` applied to the command (the per-owner `gh` config, in
    /// production). Returns how many reaps exited 0. A child past
    /// [`REAP_TIMEOUT`] is killed.
    pub fn reap(&self, bin: &Path, reason: &str, prepare: impl Fn(&mut Command)) -> usize {
        let mut ok = 0;
        for held in self.held() {
            let mut cmd = Command::new(bin);
            cmd.args(held.reap_args(&self.root, reason))
                .current_dir(&self.root)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            prepare(&mut cmd);
            if run_bounded(&mut cmd, REAP_TIMEOUT) {
                ok += 1;
            } else {
                log::warn!(
                    "pr_lease_reap: could not reap the {} lease on PR #{} ({}); it ages out at \
                     the TTL",
                    held.role,
                    held.pr,
                    held.sweep
                );
            }
        }
        ok
    }

    /// [`reap`](Self::reap) through this daemon's own binary, with the
    /// per-owner `gh` config the role child had. A no-op when nothing is
    /// listed (no binary is resolved).
    pub fn reap_with_daemon(&self, reason: &str) -> usize {
        if self.held().is_empty() {
            return 0;
        }
        let bin = match crate::daemon_bin_resolve::resolve_daemon_bin() {
            Ok(bin) => bin,
            Err(e) => {
                log::warn!("pr_lease_reap: {e}; the run's PR leases age out at the TTL");
                return 0;
            }
        };
        let root = self.root.clone();
        self.reap(&bin, reason, |cmd| {
            crate::credential_preflight::apply_gh_config_for_root(cmd, &root);
        })
    }
}

impl Drop for Reaper {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn run_bounded(cmd: &mut Command, limit: Duration) -> bool {
    let Ok(mut child) = cmd.spawn() else {
        return false;
    };
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if start.elapsed() < limit => std::thread::sleep(POLL),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

#[cfg(test)]
#[path = "pr_lease_reap/tests.rs"]
mod tests;
