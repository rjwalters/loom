//! Roster `autoApply`: clone a missing desired repo, then register it (#11218).
//!
//! The roster plan ([`crate::fleet_store::roster::plan`]) registers a desired
//! repo only when it is already cloned under `root`; a missing clone is a
//! [`Change::MissingClone`]. Before this module nothing in the fleet cloned
//! one, so admitting a repo still needed a login on every dispatcher. Here a
//! **timer** pass with `fleet.autoApply` on clones it and registers it in the
//! same pass, through the same [`super::apply_change`] `Add` an existing clone
//! takes.
//!
//! # What it keeps
//!
//! - **Fail closed.** It acts only on a [`Plan`] built by the fail-closed
//!   roster pass ([`super::roster_pass`]), so a firewall record, a both-flags
//!   roster or an unconfirmable snapshot never reaches it. With `autoApply`
//!   off ([`summarize`]'s `auto_apply == false`) it does nothing at all, and
//!   the report is byte-identical to the pre-#11218 one.
//! - **HTTPS with the daemon's own credential.** The record's `remote` only
//!   names the repo ([`github_slug`]); the clone URL is always
//!   `https://github.com/<owner>/<name>.git`, never the record's `git@…` form.
//!   [`GitCloner`] runs git the way every other daemon git network call does
//!   (no prompt, the host's credential helper), with `GH_CONFIG_DIR` pointed
//!   at that owner's per-owner credential when it is a cross-owner one
//!   ([`crate::credential_preflight::apply_gh_config_for_owner_slug`]).
//!   The clone runs with `protocol.allow=never`, `protocol.https.allow=always`
//!   and `protocol.ssh.allow=never`, so a host `url.*.insteadOf` rewrite to
//!   SSH (or any other transport) makes it fail rather than go over SSH.
//! - **Never over something.** A path that exists and is not an empty
//!   directory is refused and reported, never touched. The clone goes to a
//!   uniquely-named, marked staging sibling ([`staging`]) and is renamed into
//!   place only once it is complete, so a failed or killed clone never leaves
//!   a half-repo that the next plan's `is_cloned` (a `.git` check) would
//!   register. A leftover sibling is reclaimed only when it carries the marker.
//! - **Bounded.** At most [`CloneConfig::max_per_pass`] clones per pass, each
//!   under [`CloneConfig::timeout`]; the rest are deferred to the next pass. A
//!   failure is recorded in the pass ([`CloneAttempt`]) and retried after a
//!   per-repo backoff ([`memory`]), never-failed and least-recently-failed
//!   repos first, so a repo that always fails cannot starve the others. It
//!   never panics and never becomes a roster *error*.
//! - **No scaffolding.** Registration is `add_and_trust`: nothing is written
//!   into the working tree (the `--no-init` semantics, #6636).
//!
//! # Where it runs, and why there
//!
//! On the fleet-sync **timer** pass, inside the roster half, on the blocking
//! thread that pass already runs on. That is the one place a fail-closed plan
//! exists, so the clone and the registration share it ("in the same pass").
//! The startup pass never clones (boot must not wait on a transfer; the first
//! timer pass does it), and neither does a pass whose desired run state holds
//! or stops this host, so a clone never delays a `paused`/`stopped` order read
//! in the same pass. The cost is that a slow clone delays the *next* timer
//! tick by at most `max_per_pass × timeout`; the defaults keep that small, and
//! a repo is cloned once. A `paused`/`stopped` order that lands *during* a
//! clone is likewise read one such delay later.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{FleetSyncStatus, RosterPass};
use crate::fleet_store::roster::{self, Change, Plan};

pub mod memory;
pub mod staging;

pub use memory::CloneMemory;

/// Config key: clones per timer pass. `0` turns cloning off.
pub const MAX_PER_PASS_KEY: &str = "fleet.cloneMaxPerPass";
/// Env override for [`MAX_PER_PASS_KEY`].
pub const MAX_PER_PASS_ENV: &str = "LOOM_FLEET_CLONE_MAX_PER_PASS";
/// Config key: the wall-clock cap on one clone.
pub const TIMEOUT_KEY: &str = "fleet.cloneTimeoutSecs";
/// Env override for [`TIMEOUT_KEY`].
pub const TIMEOUT_ENV: &str = "LOOM_FLEET_CLONE_TIMEOUT_SECS";
/// Clones per pass when nothing configures it.
pub const DEFAULT_MAX_PER_PASS: usize = 2;
/// Per-clone cap when nothing configures it.
pub const DEFAULT_TIMEOUT_SECS: u64 = 300;
/// Floor on the per-clone cap: below this a real clone could never finish.
pub const MIN_TIMEOUT_SECS: u64 = 30;
/// Event-bus topic one record per clone attempt is published on.
pub const TOPIC: &str = "fleet_sync.clone";

/// How much cloning one pass may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CloneConfig {
    /// Clones per pass; `0` = never clone.
    pub max_per_pass: usize,
    /// Wall-clock cap on one clone.
    pub timeout: Duration,
}

impl Default for CloneConfig {
    fn default() -> Self {
        Self {
            max_per_pass: DEFAULT_MAX_PER_PASS,
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        }
    }
}

/// Resolve [`CloneConfig`]: env over `effective_config` over the defaults.
/// An unparseable value reads as unset. The timeout is clamped up to
/// [`MIN_TIMEOUT_SECS`].
#[must_use]
pub fn resolve_config(
    effective_config: &Value,
    env: &dyn Fn(&str) -> Option<String>,
) -> CloneConfig {
    let read = |env_key: &str, key: &str| {
        env(env_key)
            .and_then(|s| s.trim().parse::<u64>().ok())
            .or_else(|| {
                crate::config_resolver::get_path(effective_config, key).and_then(|v| {
                    v.as_u64()
                        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
                })
            })
    };
    let max = read(MAX_PER_PASS_ENV, MAX_PER_PASS_KEY)
        .map_or(DEFAULT_MAX_PER_PASS, |n| usize::try_from(n).unwrap_or(usize::MAX));
    let secs = read(TIMEOUT_ENV, TIMEOUT_KEY).unwrap_or(DEFAULT_TIMEOUT_SECS);
    CloneConfig {
        max_per_pass: max,
        timeout: Duration::from_secs(secs.max(MIN_TIMEOUT_SECS)),
    }
}

/// The seam: one clone of `url` (the repo `slug`, `owner/name`) into `dest`,
/// which does not exist. Production is [`GitCloner`]; tests pass a fake, so
/// no unit test runs git or touches the network. Leaving a partial `dest`
/// behind on failure is allowed: the caller removes it.
pub trait Cloner {
    /// Clone, or say why not.
    fn clone_into(
        &self,
        slug: &str,
        url: &str,
        dest: &Path,
        timeout: Duration,
    ) -> Result<(), String>;
}

/// `git clone` over HTTPS, bounded by `timeout` (the child's process group is
/// killed when it fires).
pub struct GitCloner;

impl Cloner for GitCloner {
    fn clone_into(
        &self,
        slug: &str,
        url: &str,
        dest: &Path,
        timeout: Duration,
    ) -> Result<(), String> {
        use crate::proc_exec::{run_bounded, Completion};
        // A clone on a nearly full volume aborts mid-pack (#10995).
        if let Some(why) = dest.parent().and_then(crate::fetch_headroom::skip_reason) {
            return Err(why.replace("git fetch", "git clone"));
        }
        match run_bounded(clone_command(slug, url, dest), timeout) {
            Ok(Completion::Exited(out)) if out.status.success() => Ok(()),
            Ok(Completion::Exited(out)) => {
                let err = String::from_utf8_lossy(&out.stderr);
                let line = err
                    .lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty())
                    .unwrap_or("");
                Err(format!("git clone failed ({}): {line}", out.status))
            }
            Ok(Completion::TimedOut { .. }) => {
                Err(format!("git clone did not finish within {}s", timeout.as_secs()))
            }
            Err(e) => Err(format!("git clone could not run: {e}")),
        }
    }
}

/// `-c` settings every clone runs with: hooks off (a template hook must not
/// run unattended in the daemon), and HTTPS as the only transport, so a host
/// `url.*.insteadOf` rewrite to SSH fails the clone instead of using SSH.
pub const CLONE_CONFIG: &[&str] = &[
    "core.hooksPath=/dev/null",
    "protocol.allow=never",
    "protocol.https.allow=always",
    "protocol.ssh.allow=never",
];

/// The `git clone` [`GitCloner`] runs: no prompt, no inherited repository
/// override, and `slug`'s owner's per-owner `GH_CONFIG_DIR` when it has one.
#[must_use]
pub fn clone_command(slug: &str, url: &str, dest: &Path) -> Command {
    let mut cmd = Command::new("git");
    for pair in CLONE_CONFIG {
        cmd.arg("-c").arg(pair);
    }
    cmd.args(["clone", "--quiet", "--no-recurse-submodules", "--", url])
        .arg(dest)
        .stdin(Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .env("LC_ALL", "C");
    for var in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"] {
        cmd.env_remove(var);
    }
    crate::credential_preflight::apply_gh_config_for_owner_slug(&mut cmd, slug);
    cmd
}

/// What [`summarize`] may clone with this pass.
pub struct Clones<'a> {
    cloner: Option<&'a dyn Cloner>,
    config: CloneConfig,
    memory: Option<&'a std::sync::Mutex<CloneMemory>>,
    now: Instant,
}

impl<'a> Clones<'a> {
    /// Never clone (the startup pass, a held host, `roster --apply`).
    #[must_use]
    pub fn off() -> Self {
        Self {
            cloner: None,
            config: CloneConfig::default(),
            memory: None,
            now: Instant::now(),
        }
    }

    /// Clone through `cloner`, within `config`, ordering and backing off by
    /// `memory` as of `now`.
    #[must_use]
    pub fn on(
        cloner: &'a dyn Cloner,
        config: CloneConfig,
        memory: &'a std::sync::Mutex<CloneMemory>,
        now: Instant,
    ) -> Self {
        Self {
            cloner: Some(cloner).filter(|_| config.max_per_pass > 0),
            config,
            memory: Some(memory),
            now,
        }
    }

    fn with_memory<T>(&self, f: impl FnOnce(&mut CloneMemory) -> T) -> Option<T> {
        let mut guard = self.memory?.lock().ok()?;
        Some(f(&mut guard))
    }
}

/// What happened to one missing clone this pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    /// Cloned into place (see [`CloneAttempt::registered`]).
    Cloned,
    /// The clone was tried and did not complete; retried next pass.
    Failed,
    /// Not tried: no GitHub remote, or the path is occupied.
    Refused,
    /// Not tried: the per-pass cap was reached, or the repo is backing off
    /// after a failure; a later pass.
    Deferred,
}

impl Outcome {
    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cloned => "cloned",
            Self::Failed => "failed",
            Self::Refused => "refused",
            Self::Deferred => "deferred",
        }
    }
}

/// One missing clone, as this pass handled it. Recorded on
/// [`RosterPass::clones`] (so `loom-daemon status` shows it) and published on
/// [`TOPIC`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloneAttempt {
    /// Record name.
    pub name: String,
    /// `owner/name`, when the record's remote named a GitHub repo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// The clone path.
    pub path: PathBuf,
    /// What happened.
    pub outcome: Outcome,
    /// Wall-clock time spent, milliseconds.
    pub duration_ms: u64,
    /// Whether the clone was then registered.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub registered: bool,
    /// Why, for anything but a clean clone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// `owner/name` from a GitHub remote: `git@github.com:o/n(.git)`,
/// `ssh://git@github.com/o/n(.git)` or `https://github.com/o/n(.git)`.
/// `None` for anything else (another host, extra path segments, odd
/// characters), which is then reported rather than guessed at.
#[must_use]
pub fn github_slug(remote: &str) -> Option<String> {
    let r = remote.trim();
    let rest = [
        "git@github.com:",
        "ssh://git@github.com/",
        "https://github.com/",
    ]
    .iter()
    .find_map(|p| r.strip_prefix(p))?;
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let (owner, name) = rest.split_once('/')?;
    let ok = |s: &str| {
        !s.is_empty()
            && !s.starts_with('.')
            && !s.starts_with('-')
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    (ok(owner) && ok(name)).then(|| format!("{owner}/{name}"))
}

/// The URL a clone uses, whatever form the record's remote took.
#[must_use]
pub fn https_url(slug: &str) -> String {
    format!("https://github.com/{slug}.git")
}

/// Turn a [`Plan`] into a [`RosterPass`], applying it through `apply` when
/// `auto_apply` is set and cloning a missing clone through `clones` first.
/// [`super::summarize_roster`] is this with [`Clones::off`].
pub fn summarize(
    plan: &Plan,
    auto_apply: bool,
    clones: &Clones<'_>,
    apply: &mut dyn FnMut(&Change) -> Result<()>,
) -> RosterPass {
    let mut out = RosterPass {
        drift: plan.changes.iter().map(roster::describe).collect(),
        ..RosterPass::default()
    };
    if !auto_apply {
        return out;
    }
    let mut missing = Vec::new();
    for (i, change) in plan.changes.iter().enumerate() {
        match change {
            Change::MissingClone { .. } if clones.cloner.is_some() => missing.push(i),
            Change::MissingClone { .. } => out.unapplied += 1,
            other => {
                record(&mut out, other, apply);
            }
        }
    }
    let Some(cloner) = clones.cloner else {
        return out;
    };
    // Never-failed first, then least recently failed (stable: plan order
    // within each group), so a repo that keeps failing cannot starve others.
    missing.sort_by_key(|&i| {
        let (_, path, _) = fields(&plan.changes[i]);
        clones
            .with_memory(|m| m.order_key(path))
            .unwrap_or((false, None))
    });
    let mut tried = 0usize;
    for i in missing {
        let change = &plan.changes[i];
        let (_, path, _) = fields(change);
        let waiting = clones
            .with_memory(|m| m.waiting(path, clones.now))
            .flatten();
        let mut attempt = if let Some((count, left)) = waiting {
            deferred(
                change,
                format!(
                    "backing off after {count} failed clone(s); next try in about {}m",
                    left.as_secs().div_ceil(60)
                ),
            )
        } else if tried >= clones.config.max_per_pass {
            let cap = clones.config.max_per_pass;
            deferred(change, format!("the per-pass cap of {cap} clone(s) was reached; next pass"))
        } else {
            let a = clone_one(change, cloner, clones.config.timeout);
            tried += usize::from(matches!(a.outcome, Outcome::Cloned | Outcome::Failed));
            match a.outcome {
                Outcome::Failed => clones.with_memory(|m| m.failed(path, clones.now)),
                _ => clones.with_memory(|m| m.forget(path)),
            };
            a
        };
        if attempt.outcome != Outcome::Cloned {
            out.unapplied += 1;
            out.clones.push(attempt);
            continue;
        }
        let add = as_add(change);
        out.drift[i] = format!("{} — cloned this pass", roster::describe(&add));
        attempt.registered = record(&mut out, &add, apply);
        out.clones.push(attempt);
    }
    out
}

/// Apply one change, counting it; `true` when it applied.
fn record(
    out: &mut RosterPass,
    change: &Change,
    apply: &mut dyn FnMut(&Change) -> Result<()>,
) -> bool {
    match apply(change) {
        Ok(()) => {
            out.applied += 1;
            true
        }
        Err(e) => {
            out.unapplied += 1;
            let detail = format!("{}: {e:#}", roster::describe(change));
            out.error = Some(match out.error.take() {
                Some(prev) => format!("{prev}; {detail}"),
                None => detail,
            });
            false
        }
    }
}

/// The [`Change::Add`] a cloned [`Change::MissingClone`] becomes.
fn as_add(change: &Change) -> Change {
    match change.clone() {
        Change::MissingClone {
            name,
            path,
            priority,
            maintain_only,
            ..
        } => Change::Add {
            name,
            path,
            priority,
            maintain_only,
        },
        other => other,
    }
}

fn fields(change: &Change) -> (&str, &Path, Option<&str>) {
    match change {
        Change::MissingClone {
            name, path, remote, ..
        } => (name, path, remote.as_deref()),
        _ => ("", Path::new(""), None),
    }
}

fn deferred(change: &Change, why: String) -> CloneAttempt {
    let (name, path, remote) = fields(change);
    CloneAttempt {
        name: name.to_string(),
        repo: remote.and_then(github_slug),
        path: path.to_path_buf(),
        outcome: Outcome::Deferred,
        duration_ms: 0,
        registered: false,
        detail: Some(why),
    }
}

/// Clone one [`Change::MissingClone`] into place, or say why not.
fn clone_one(change: &Change, cloner: &dyn Cloner, timeout: Duration) -> CloneAttempt {
    let (name, path, remote) = fields(change);
    let started = Instant::now();
    let slug = remote.and_then(github_slug);
    let (outcome, detail) = match &slug {
        None => (
            Outcome::Refused,
            Some(match remote {
                Some(r) => {
                    format!("remote `{}` is not a github.com repo; not cloned", redact_userinfo(r))
                }
                None => "the record has no `remote`; not cloned".to_string(),
            }),
        ),
        Some(slug) => match place(slug, path, cloner, timeout) {
            Ok(()) => (Outcome::Cloned, None),
            Err((outcome, why)) => (outcome, Some(why)),
        },
    };
    CloneAttempt {
        name: name.to_string(),
        repo: slug,
        path: path.to_path_buf(),
        outcome,
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        registered: false,
        detail,
    }
}

/// Is `path` free to clone into? `Ok(true)` when it is an empty directory
/// (removed just before the rename), `Ok(false)` when absent.
fn vacant(path: &Path) -> Result<bool, (Outcome, String)> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err((Outcome::Failed, format!("cannot inspect {}: {e}", path.display()))),
        Ok(m) if m.is_dir() && std::fs::read_dir(path).is_ok_and(|mut d| d.next().is_none()) => {
            Ok(true)
        }
        Ok(m) => {
            Err((
                Outcome::Refused,
                format!(
                "{} already exists and is not a git clone ({}); left untouched — move it aside \
                 or put the clone there by hand",
                path.display(),
                if m.is_dir() { "a non-empty directory with no .git" } else { "not a directory" }
            ),
            ))
        }
    }
}

/// `remote` with any URL userinfo (`scheme://user:secret@host`) replaced by
/// `***`, for messages that echo it.
#[must_use]
pub fn redact_userinfo(remote: &str) -> String {
    let Some((scheme, rest)) = remote.split_once("://") else {
        return remote.to_string();
    };
    let authority_end = rest.find('/').unwrap_or(rest.len());
    match rest[..authority_end].rfind('@') {
        Some(at) => format!("{scheme}://***{}", &rest[at..]),
        None => remote.to_string(),
    }
}

/// Clone into a marked staging sibling, then rename into `path`.
fn place(
    slug: &str,
    path: &Path,
    cloner: &dyn Cloner,
    timeout: Duration,
) -> Result<(), (Outcome, String)> {
    let failed = |why: String| (Outcome::Failed, why);
    let empty_dir = vacant(path)?;
    let (Some(parent), Some(dir)) = (path.parent(), path.file_name()) else {
        return Err((Outcome::Refused, format!("{} has no parent directory", path.display())));
    };
    std::fs::create_dir_all(parent)
        .map_err(|e| failed(format!("creating {}: {e}", parent.display())))?;
    // Leftovers from a clone this daemon did not finish: only marked ones go.
    staging::sweep(parent, dir).map_err(|why| (Outcome::Refused, why))?;
    // Removed, with whatever is in it, when it goes out of scope.
    let stage = staging::Staging::create(parent, dir).map_err(failed)?;
    let tmp = stage.dest();
    cloner
        .clone_into(slug, &https_url(slug), &tmp, timeout)
        .and_then(|()| {
            if tmp.join(".git").is_dir() {
                Ok(())
            } else {
                Err("the clone finished without a .git directory".to_string())
            }
        })
        .and_then(|()| {
            if empty_dir {
                std::fs::remove_dir(path)
                    .map_err(|e| format!("removing empty {}: {e}", path.display()))?;
            }
            std::fs::rename(&tmp, path)
                .map_err(|e| format!("moving the clone into {}: {e}", path.display()))
        })
        .map_err(failed)
}

/// The `Fleet store:` status lines for this pass's clone attempts.
#[must_use]
pub fn lines(attempts: &[CloneAttempt]) -> Vec<String> {
    attempts
        .iter()
        .map(|a| {
            let secs = a.duration_ms as f64 / 1000.0;
            let from = a.repo.as_deref().unwrap_or("?");
            let path = a.path.display();
            let why = a.detail.as_deref().unwrap_or("");
            match a.outcome {
                Outcome::Cloned if a.registered => {
                    format!("  roster: CLONED {} {path} from {from} in {secs:.1}s, registered", a.name)
                }
                Outcome::Cloned => format!(
                    "  roster: CLONED {} {path} from {from} in {secs:.1}s, NOT registered (see the \
                     roster error)",
                    a.name
                ),
                Outcome::Failed => format!(
                    "  roster: clone FAILED — {} {path} ({from}, {secs:.1}s): {why} — retried next pass",
                    a.name
                ),
                Outcome::Refused => format!("  roster: clone REFUSED — {} {path}: {why}", a.name),
                Outcome::Deferred => format!("  roster: clone deferred — {}: {why}", a.name),
            }
        })
        .collect()
}

/// Log each clone attempt and publish it on [`TOPIC`]. The payload carries no
/// absolute path (the clone directory is its last component, and `root` in a
/// detail becomes `$ROOT`), so it can ship as telemetry as-is.
pub(super) fn announce(status: &FleetSyncStatus, bus: Option<&crate::event_bus::EventBus>) {
    for a in status
        .roster
        .clones
        .iter()
        .filter(|a| a.outcome != Outcome::Deferred)
    {
        let line = lines(std::slice::from_ref(a)).join("");
        if a.outcome == Outcome::Cloned && a.registered {
            log::info!("fleet_sync:{line}");
        } else {
            log::warn!("fleet_sync:{line}");
        }
        if let Some(bus) = bus {
            let _ = bus.publish_generic(TOPIC, payload(a, &status.host));
        }
    }
}

/// The [`TOPIC`] payload for one attempt.
#[must_use]
pub fn payload(a: &CloneAttempt, host: &str) -> Value {
    let root = a.path.parent().map(|p| p.display().to_string());
    let detail = a.detail.as_ref().map(|d| match &root {
        Some(r) if !r.is_empty() => d.replace(r.as_str(), "$ROOT"),
        _ => d.clone(),
    });
    serde_json::json!({
        "host": host,
        "repo": a.repo,
        "name": a.name,
        "dir": a.path.file_name().map(|d| d.to_string_lossy().into_owned()),
        "outcome": a.outcome.as_str(),
        "durationMs": a.duration_ms,
        "registered": a.registered,
        "detail": detail,
    })
}

#[cfg(test)]
#[path = "tests/roster_clone.rs"]
mod tests;
