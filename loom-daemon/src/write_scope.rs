//! Loom writes only to repositories it manages (#9548).
//!
//! # Why
//!
//! Loom has to be able to work on any public repository, and GitHub lets any
//! account comment on any public issue. So an installation that resolves the
//! wrong repository does not fail: its label edits are refused, but its
//! comments land, carrying well-formed Loom control markers onto a repository
//! whose own control plane is somebody else's. That is what #9548 observed.
//!
//! The wrong repository comes from `gh`'s base-repo resolution, not from any
//! explicit choice (see [`target`]): every daemon pass runs `gh` in the
//! checkout without `--repo`, and the shell helpers use `{owner}/{repo}` or
//! `gh repo view`, all of which prefer an `upstream` remote over `origin`. A
//! fork checkout therefore reads and writes the upstream project. And nothing
//! asked whether the resolved repository was one this installation manages or
//! could write to.
//!
//! # The rule
//!
//! A write to `OWNER/REPO` is allowed only when all of these hold:
//!
//! 1. **The target is what the caller meant.** With no explicit repository,
//!    the one `gh` will resolve from the checkout must be that checkout's
//!    `origin`. A checkout whose `gh` target is an `upstream` (or a
//!    `gh repo set-default` pin elsewhere, or a foreign `GH_REPO`) is refused
//!    rather than silently redirected, because its *reads* go there too: a
//!    verdict computed from upstream PR #N must not be posted to origin #N.
//! 2. **The repository is managed.** It is the `origin` of a workspace in this
//!    daemon's registry, or of the Loom-installed checkout the call runs in.
//! 3. **The credential can write it.** The credential that would carry the
//!    write has WRITE or better ([`probe`]), probed once per repository per
//!    [`probe::ttl`].
//!
//! Anything unverifiable is a refusal. Reads are never gated.
//!
//! # Where it is enforced
//!
//! - [`gate_root`] at the top of every per-workspace daemon pass that writes
//!   (claim and quarantine reconciliation, star liveness, sweep dispatch, and
//!   every scheduled role tick), which is also where the refusal is logged,
//!   once per change of reason.
//! - `loom-daemon forge may-write` for shell, wrapped by `loom_write_repo` in
//!   `defaults/scripts/lib/forge-helpers.sh`; the `forge issue|pr` write
//!   passthroughs and the auto-merge verbs vet with it before they run.
//! - `tests::daemon_write_paths_are_scoped` fails when a new daemon file
//!   issues a forge write without being reviewed into its list, and
//!   `tests::shell_write_paths_are_vetted` does the same for `defaults/scripts`.
//!
//! See `defaults/docs/comment-trust.md` § "Loom writes only to repos it
//! manages".

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

pub(crate) mod probe;
pub(crate) mod target;

use probe::{Permission, PermissionProbe};
use target::GhTarget;

/// The answer to "may this installation write here?".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Yes; the `owner/repo` the write must name explicitly.
    Allow(String),
    /// No; why, for the log line or stderr.
    Deny(String),
}

impl Verdict {
    #[must_use]
    pub fn is_allowed(&self) -> bool {
        matches!(self, Verdict::Allow(_))
    }
}

fn eq(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// The inputs a decision needs, already gathered.
pub(crate) struct Inputs<'a> {
    /// The repository the write would land on.
    pub(crate) target: Option<GhTarget>,
    /// Whether the target was named explicitly (`--repo`) rather than resolved
    /// from the checkout.
    pub(crate) explicit: bool,
    /// The checkout's `origin`, when it has one.
    pub(crate) origin: Option<&'a str>,
    /// Is this repository managed? Lazily evaluated: most calls never need
    /// the registry scan.
    pub(crate) managed: &'a dyn Fn(&str) -> bool,
}

/// The pure decision (rules 1-3 in the module docs).
pub(crate) fn decide(inputs: &Inputs<'_>, probe: &dyn PermissionProbe) -> Verdict {
    let Some(target) = &inputs.target else {
        return Verdict::Deny(
            "no target repository: the checkout has no GitHub remote and none was named".into(),
        );
    };
    if !inputs.explicit {
        match inputs.origin {
            Some(o) if eq(o, &target.nwo) => {}
            Some(o) => {
                return Verdict::Deny(format!(
                    "gh resolves this checkout to {} (via {}), not its origin {o}; Loom will not \
                     read from one repository and write to another. Pin it with \
                     `gh repo set-default {o}` if origin is the repository Loom manages",
                    target.nwo, target.via
                ))
            }
            None => {
                return Verdict::Deny(format!(
                    "the checkout has no origin remote, so {} (via {}) cannot be confirmed as \
                     the repository it manages",
                    target.nwo, target.via
                ))
            }
        }
    }
    if !(inputs.managed)(&target.nwo) {
        return Verdict::Deny(format!(
            "{} is not a repository this installation manages (not the origin of a registered \
             workspace or of this Loom checkout)",
            target.nwo
        ));
    }
    match probe.permission(&target.nwo) {
        Permission::Write => Verdict::Allow(target.nwo.clone()),
        Permission::Insufficient(what) => Verdict::Deny(format!(
            "the credential in use cannot write to {} ({what}); Loom writes need WRITE",
            target.nwo
        )),
        Permission::Unknown(why) => Verdict::Deny(format!(
            "could not verify write permission on {} ({why}); refusing rather than guessing",
            target.nwo
        )),
    }
}

/// `origin`'s `owner/repo` for each root, cached for the process (remotes
/// do not move under a running daemon often enough to re-read per call).
fn origin_of(root: &Path) -> Option<String> {
    static M: OnceLock<Mutex<HashMap<PathBuf, Option<String>>>> = OnceLock::new();
    let m = M.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(v) = m.lock().ok().and_then(|m| m.get(root).cloned()) {
        return v;
    }
    let v = target::origin_nwo(&target::read_remotes(root));
    if let Ok(mut m) = m.lock() {
        m.insert(root.to_path_buf(), v.clone());
    }
    v
}

/// A root's remotes, re-read at most once a minute: [`gate_root`] runs on
/// every dispatch tick for every workspace.
fn remotes_cached(root: &Path) -> Vec<target::Remote> {
    type Entry = (Vec<target::Remote>, std::time::Instant);
    static M: OnceLock<Mutex<HashMap<PathBuf, Entry>>> = OnceLock::new();
    let m = M.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some((r, at)) = m.lock().ok().and_then(|m| m.get(root).cloned()) {
        if at.elapsed() < std::time::Duration::from_secs(60) {
            return r;
        }
    }
    let r = target::read_remotes(root);
    if let Ok(mut m) = m.lock() {
        m.insert(root.to_path_buf(), (r.clone(), std::time::Instant::now()));
    }
    r
}

/// Is `repo` the origin of a registered workspace, or of `checkout` when that
/// checkout has Loom installed?
pub(crate) fn is_managed(checkout: &Path, checkout_origin: Option<&str>, repo: &str) -> bool {
    if checkout.join(".loom").is_dir() && checkout_origin.is_some_and(|o| eq(o, repo)) {
        return true;
    }
    let registry = crate::workspace_registry::WorkspaceRegistry::load_default().unwrap_or_default();
    registry
        .effective_roots(checkout)
        .iter()
        .filter_map(|r| origin_of(r))
        .any(|o| eq(&o, repo))
}

/// The Loom checkout containing `dir` (a linked worktree resolves to its main
/// checkout, which carries the same remotes), or `dir` itself.
fn checkout_root(dir: &Path) -> PathBuf {
    crate::repo_root::find_repo_root(dir).unwrap_or_else(|| dir.to_path_buf())
}

fn gh_bin() -> PathBuf {
    std::env::var_os("LOOM_GH_BIN").map_or_else(|| PathBuf::from("gh"), PathBuf::from)
}

/// `loom-daemon forge may-write`: may a write from `cwd` go to `repo` (or,
/// with no `repo`, to whatever `gh` would resolve there)? The credential is
/// this process's own environment, which is what the calling script's `gh`
/// will use.
#[must_use]
pub fn may_write_from(cwd: &Path, repo: Option<&str>) -> Verdict {
    let root = checkout_root(cwd);
    let remotes = target::read_remotes(&root);
    let origin = target::origin_nwo(&remotes);
    let explicit = repo.map(str::trim).filter(|r| !r.is_empty());
    let target = match explicit {
        Some(r) => match target::nwo_from_repo_arg(r) {
            Some(nwo) => Some(GhTarget {
                nwo,
                via: "--repo".into(),
            }),
            None => return Verdict::Deny(format!("`{r}` is not an OWNER/REPO")),
        },
        None => target::gh_target(&remotes, std::env::var("GH_REPO").ok().as_deref()),
    };
    let managed = |r: &str| is_managed(&root, origin.as_deref(), r);
    let probe = probe_for(&root, None);
    decide(
        &Inputs {
            target,
            explicit: explicit.is_some(),
            origin: origin.as_deref(),
            managed: &managed,
        },
        probe.as_ref(),
    )
}

/// Gitea: rules 1 and 2 (origin, managed) still apply, but the permission leg
/// is a GitHub API probe with no Gitea counterpart yet, so it answers WRITE
/// rather than refusing every Gitea workspace.
struct GiteaUnprobed;

impl PermissionProbe for GiteaUnprobed {
    fn permission(&self, _repo: &str) -> Permission {
        Permission::Write
    }
}

/// The production probe for writes made from `root` under `config_dir` (a
/// per-owner `GH_CONFIG_DIR`; `None` is this process's own credential).
fn probe_for(root: &Path, config_dir: Option<PathBuf>) -> Box<dyn PermissionProbe> {
    if crate::forge_cmd::detect_forge(Some(root)) == crate::forge_cmd::ForgeType::Gitea {
        return Box::new(GiteaUnprobed);
    }
    Box::new(probe::Cached {
        inner: probe::GhProbe::new(gh_bin(), config_dir.clone()),
        key_dir: config_dir,
    })
}

/// [`probe_for`] under `root`'s own credential.
fn probe_for_root(root: &Path) -> Box<dyn PermissionProbe> {
    probe_for(root, crate::credential_preflight::gh_config_dir_for_root(root))
}

/// May the daemon's per-workspace passes write from `root`? The target is
/// what their `gh` calls resolve (never an explicit repo), under `root`'s own
/// credential; a machine-wide `LOOM_REPO` override, which their `gh api`
/// calls honour, must pass on its own too.
#[must_use]
pub fn root_writable(root: &Path) -> Verdict {
    #[cfg(test)]
    if let Some(v) = test_override::get(root) {
        return v;
    }
    let remotes = remotes_cached(root);
    let origin = target::origin_nwo(&remotes);
    let probe = probe_for_root(root);
    let managed = |r: &str| is_managed(root, origin.as_deref(), r);
    let gh_repo = std::env::var("GH_REPO").ok();
    let verdict = decide(
        &Inputs {
            target: target::gh_target(&remotes, gh_repo.as_deref()),
            explicit: false,
            origin: origin.as_deref(),
            managed: &managed,
        },
        probe.as_ref(),
    );
    match (&verdict, std::env::var("LOOM_REPO").ok()) {
        (Verdict::Allow(t), Some(o)) if !o.trim().is_empty() && !eq(t, o.trim()) => {
            repo_writable(root, &o)
        }
        _ => verdict,
    }
}

/// May the daemon write to the explicitly named `repo` (a configured roster
/// issue, a `--repo` argument) on behalf of workspace `root`, under `root`'s
/// credential? No origin match is required, since nothing was resolved from
/// remotes; the repository must still be managed and writable.
#[must_use]
pub fn repo_writable(root: &Path, repo: &str) -> Verdict {
    #[cfg(test)]
    if let Some(v) = test_override::get(root) {
        return v;
    }
    let Some(nwo) = target::nwo_from_repo_arg(repo) else {
        return Verdict::Deny(format!("`{repo}` is not an OWNER/REPO"));
    };
    let origin = target::origin_nwo(&remotes_cached(root));
    let managed = |r: &str| is_managed(root, origin.as_deref(), r);
    decide(
        &Inputs {
            target: Some(GhTarget {
                nwo,
                via: "an explicit repository".into(),
            }),
            explicit: true,
            origin: origin.as_deref(),
            managed: &managed,
        },
        probe_for_root(root).as_ref(),
    )
}

/// Log a refusal at `warn` once per change of reason per `key`, and its
/// clearing at `info`, so a denied workspace costs one log line, not one per
/// tick. Returns whether to proceed.
fn gate(key: String, verdict: &Verdict, what: &str) -> bool {
    static LAST: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    let last = LAST.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut last) = last.lock() else {
        return verdict.is_allowed();
    };
    match verdict {
        Verdict::Allow(_) => {
            if last.remove(&key).is_some() {
                log::info!("write_scope: {what} for {key} may write again (#9548)");
            }
            true
        }
        Verdict::Deny(why) => {
            if last.get(&key) != Some(why) {
                log::warn!(
                    "write_scope: skipping {what} for {key}: {why} (#9548; reads are unaffected)"
                );
                last.insert(key, why.clone());
            }
            false
        }
    }
}

/// [`root_writable`] as a gate: `true` to proceed with `what` (a short pass
/// name for the log).
pub fn gate_root(root: &Path, what: &str) -> bool {
    gate(root.display().to_string(), &root_writable(root), what)
}

/// [`repo_writable`] as a gate.
pub fn gate_repo(root: &Path, repo: &str, what: &str) -> bool {
    gate(format!("{} -> {repo}", root.display()), &repo_writable(root, repo), what)
}

/// Per-thread verdict overrides so other modules' tests, whose temp roots
/// have no remotes, keep exercising their own logic: unset means "allow".
#[cfg(test)]
pub(crate) mod test_override {
    use super::Verdict;
    use std::cell::RefCell;
    use std::path::Path;

    thread_local! {
        static DENY: RefCell<Option<String>> = const { RefCell::new(None) };
        static REAL: RefCell<bool> = const { RefCell::new(false) };
    }

    /// Deny every root on this thread with `reason` (`None` restores allow).
    pub(crate) fn deny(reason: Option<&str>) {
        DENY.with(|d| *d.borrow_mut() = reason.map(str::to_string));
    }

    /// Run the real decision on this thread (write_scope's own tests).
    pub(crate) fn real(on: bool) {
        REAL.with(|r| *r.borrow_mut() = on);
    }

    pub(super) fn get(_root: &Path) -> Option<Verdict> {
        if REAL.with(|r| *r.borrow()) {
            return None;
        }
        Some(DENY.with(|d| {
            d.borrow()
                .clone()
                .map_or_else(|| Verdict::Allow("test/allow".into()), Verdict::Deny)
        }))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests;
