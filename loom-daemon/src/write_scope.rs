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
//! - [`repo_writable`] before the workspace resync (#10718) takes a repo's
//!   claim: its target is the workspace's `origin`, named explicitly.
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
mod probe_snapshot;
pub(crate) mod target;

use probe::{Permission, PermissionProbe};
use target::GhTarget;

use crate::telemetry::kinds::write_scope_refused::{RefusalClass, WriteScopeRefusedRecord};

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

/// A verdict with the closed refusal class behind a `Deny`, so telemetry never
/// parses the reason sentence (#12168).
pub(crate) struct Decision {
    pub(crate) verdict: Verdict,
    /// `Some` exactly when `verdict` is a `Deny`.
    pub(crate) class: Option<RefusalClass>,
    /// The `owner/repo` the decision was about, when known.
    pub(crate) repo: Option<String>,
}

impl Decision {
    fn deny(class: RefusalClass, repo: Option<&str>, why: String) -> Self {
        Decision {
            verdict: Verdict::Deny(why),
            class: Some(class),
            repo: repo.map(str::to_owned),
        }
    }
}

/// The pure decision (rules 1-3 in the module docs).
pub(crate) fn decide(inputs: &Inputs<'_>, probe: &dyn PermissionProbe) -> Verdict {
    decide_classified(inputs, probe).verdict
}

/// [`decide`] plus the refusal class and the repository the decision is about.
pub(crate) fn decide_classified(inputs: &Inputs<'_>, probe: &dyn PermissionProbe) -> Decision {
    let Some(target) = &inputs.target else {
        return Decision::deny(
            RefusalClass::NoTarget,
            None,
            "no target repository: the checkout has no GitHub remote and none was named".into(),
        );
    };
    let repo = Some(target.nwo.as_str());
    if !inputs.explicit {
        match inputs.origin {
            Some(o) if eq(o, &target.nwo) => {}
            Some(o) => {
                return Decision::deny(
                    RefusalClass::TargetMismatch,
                    repo,
                    format!(
                        "gh resolves this checkout to {} (via {}), not its origin {o}; Loom will \
                         not read from one repository and write to another. Pin it with \
                         `gh repo set-default {o}` if origin is the repository Loom manages",
                        target.nwo, target.via
                    ),
                )
            }
            None => {
                return Decision::deny(
                    RefusalClass::TargetMismatch,
                    repo,
                    format!(
                        "the checkout has no origin remote, so {} (via {}) cannot be confirmed \
                         as the repository it manages",
                        target.nwo, target.via
                    ),
                )
            }
        }
    }
    if !(inputs.managed)(&target.nwo) {
        return Decision::deny(
            RefusalClass::UnmanagedRepo,
            repo,
            format!(
                "{} is not a repository this installation manages (not the origin of a \
                 registered workspace or of this Loom checkout)",
                target.nwo
            ),
        );
    }
    match probe.permission(&target.nwo) {
        Permission::Write => Decision {
            verdict: Verdict::Allow(target.nwo.clone()),
            class: None,
            repo: Some(target.nwo.clone()),
        },
        Permission::Insufficient(what) => {
            let class = if what == probe::NOT_IN_INSTALLATION {
                RefusalClass::NotInInstallation
            } else {
                RefusalClass::InsufficientPermission
            };
            Decision::deny(
                class,
                repo,
                format!(
                    "the credential in use cannot write to {} ({what}); Loom writes need WRITE",
                    target.nwo
                ),
            )
        }
        Permission::Unknown(why) => Decision::deny(
            RefusalClass::Unverifiable,
            repo,
            format!(
                "could not verify write permission on {} ({why}); refusing rather than guessing",
                target.nwo
            ),
        ),
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

/// The `gh` a write runs when its caller names none: `LOOM_GH_BIN`, else
/// `gh` on `PATH`. Callers that carry their own `gh` (a pass's `gh_bin`, a
/// sweep config's) use the `_with` gates, so the permission probe runs the
/// same binary, and so the same credential, as the writes it vets.
#[must_use]
pub fn default_gh() -> PathBuf {
    PathBuf::from(crate::gh_invocation::gh_bin())
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
    let probe = probe_for(&root, None, &default_gh());
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

/// The production probe for writes made from `root` under `config_dir` (a
/// per-owner `GH_CONFIG_DIR`; `None` is this process's own credential). On
/// Gitea the same cache wraps a [`probe::GiteaProbe`] (rule 3 against Gitea's
/// own API, #9699); when its connection cannot even resolve, the probe
/// answers `Unknown` and the decision refuses — the fail-closed rule.
fn probe_for(root: &Path, config_dir: Option<PathBuf>, gh: &Path) -> Box<dyn PermissionProbe> {
    if crate::forge_cmd::detect_forge(Some(root)) == crate::forge_cmd::ForgeType::Gitea {
        // The Gitea probe names its own key space (forge + base URL +
        // connection digest); an unresolved connection has none and is never
        // cached or graced.
        return Box::new(probe::Cached {
            inner: probe::GiteaProbe::for_root(root),
            key_dir: config_dir,
        });
    }
    Box::new(probe::Cached {
        inner: probe::GhProbe::new(gh.to_path_buf(), config_dir.clone()),
        key_dir: config_dir,
    })
}

/// [`probe_for`] under `root`'s own credential, run through `gh`.
fn probe_for_root(root: &Path, gh: &Path) -> Box<dyn PermissionProbe> {
    probe_for(root, crate::credential_preflight::gh_config_dir_for_root(root), gh)
}

/// May the daemon's per-workspace passes write from `root`? The target is
/// what their `gh` calls resolve (never an explicit repo), under `root`'s own
/// credential; a machine-wide `LOOM_REPO` override, which their `gh api`
/// calls honour, must pass on its own too. The probe runs [`default_gh`].
#[must_use]
pub fn root_writable(root: &Path) -> Verdict {
    root_writable_with(root, &default_gh())
}

/// [`root_writable`] with the permission probe run through `gh`, the binary
/// the caller's own writes use.
#[must_use]
pub fn root_writable_with(root: &Path, gh: &Path) -> Verdict {
    root_decision_with(root, gh).verdict
}

/// [`root_writable_with`], keeping the refusal class for [`gate`].
fn root_decision_with(root: &Path, gh: &Path) -> Decision {
    let remotes = remotes_cached(root);
    let origin = target::origin_nwo(&remotes);
    let probe = probe_for_root(root, gh);
    let managed = |r: &str| is_managed(root, origin.as_deref(), r);
    let gh_repo = std::env::var("GH_REPO").ok();
    let decision = decide_classified(
        &Inputs {
            target: target::gh_target(&remotes, gh_repo.as_deref()),
            explicit: false,
            origin: origin.as_deref(),
            managed: &managed,
        },
        probe.as_ref(),
    );
    match (&decision.verdict, std::env::var("LOOM_REPO").ok()) {
        (Verdict::Allow(t), Some(o)) if !o.trim().is_empty() && !eq(t, o.trim()) => {
            repo_decision_with(root, &o, gh)
        }
        _ => decision,
    }
}

/// May the daemon write to the explicitly named `repo` (a configured roster
/// issue, a `--repo` argument) on behalf of workspace `root`, under `root`'s
/// credential? No origin match is required, since nothing was resolved from
/// remotes; the repository must still be managed and writable.
#[must_use]
pub fn repo_writable(root: &Path, repo: &str) -> Verdict {
    repo_writable_with(root, repo, &default_gh())
}

/// [`repo_writable`] with the permission probe run through `gh`.
#[must_use]
pub fn repo_writable_with(root: &Path, repo: &str, gh: &Path) -> Verdict {
    repo_decision_with(root, repo, gh).verdict
}

/// [`repo_writable_with`], keeping the refusal class for [`gate`].
fn repo_decision_with(root: &Path, repo: &str, gh: &Path) -> Decision {
    let Some(nwo) = target::nwo_from_repo_arg(repo) else {
        return Decision::deny(
            RefusalClass::NoTarget,
            None,
            format!("`{repo}` is not an OWNER/REPO"),
        );
    };
    let origin = target::origin_nwo(&remotes_cached(root));
    let managed = |r: &str| is_managed(root, origin.as_deref(), r);
    decide_classified(
        &Inputs {
            target: Some(GhTarget {
                nwo,
                via: "an explicit repository".into(),
            }),
            explicit: true,
            origin: origin.as_deref(),
            managed: &managed,
        },
        probe_for_root(root, gh).as_ref(),
    )
}

/// A non-secret identity for the credential a write from `root` would carry:
/// its kind, or the registered per-owner config directory's name. Never a
/// token or any part of one (#12168).
fn credential_identity(root: &Path) -> String {
    if crate::forge_cmd::detect_forge(Some(root)) == crate::forge_cmd::ForgeType::Gitea {
        return "gitea".into();
    }
    match crate::credential_preflight::gh_config_dir_for_root(root)
        .as_deref()
        .and_then(Path::file_name)
    {
        Some(dir) => format!("gh-config:{}", dir.to_string_lossy()),
        None => "gh-process-default".into(),
    }
}

/// Emit one `write_scope.refused` record for a refused daemon-pass write
/// (#12168). Not deduplicated: the record is the count. A no-op without a
/// registered ops sink, and skipped when the build provenance is not valid.
fn emit_refused(root: &Path, decision: &Decision, what: &str) {
    let Some(reason) = decision.class else {
        return;
    };
    let loom = crate::telemetry::provenance::Provenance::current();
    if !loom.is_valid() {
        return;
    }
    crate::observability::ops::emit_record(crate::telemetry::TelemetryRecord::WriteScopeRefused(
        WriteScopeRefusedRecord {
            repo: decision.repo.clone(),
            credential: credential_identity(root),
            call_site: what.to_owned(),
            reason,
            refused_at: chrono::Utc::now(),
            loom,
        },
    ));
}

/// Log a refusal at `warn` once per change of reason per `key`, and its
/// clearing at `info`, so a denied workspace costs one log line, not one per
/// tick. Every refusal, however, emits a `write_scope.refused` record (#12168).
/// Returns whether to proceed.
fn gate(key: String, root: &Path, decision: &Decision, what: &str) -> bool {
    let verdict = &decision.verdict;
    if !verdict.is_allowed() {
        emit_refused(root, decision, what);
    }
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
    gate_root_with(root, &default_gh(), what)
}

/// [`root_writable_with`] as a gate, for a pass that writes through `gh`.
pub fn gate_root_with(root: &Path, gh: &Path, what: &str) -> bool {
    gate(root.display().to_string(), root, &root_decision_with(root, gh), what)
}

/// [`repo_writable_with`] as a gate, for a pass that writes through `gh`.
pub fn gate_repo_with(root: &Path, repo: &str, gh: &Path, what: &str) -> bool {
    gate(
        format!("{} -> {repo}", root.display()),
        root,
        &repo_decision_with(root, repo, gh),
        what,
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod gitea_tests;
