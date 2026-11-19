//! Owner-drift detection (#14223, part of the #14170 org-transfer family).
//!
//! The daemon resolves each checkout's credential from the checkout's owner
//! at startup: [`cross_owner::establish_for_workspace`] reads every managed
//! root's `origin`, mints one per-owner credential per distinct owner, and
//! registers root → dir in process-global maps. A checkout re-pointed at
//! ANOTHER owner after startup (the #14170 incident: an org moved, the
//! consumer re-pointed, the daemon never restarted) keeps that startup
//! mapping, so its reads keep running under the OLD owner's credential and
//! are shed — silently, until now.
//!
//! This module is the detection and cure, driven by the primary refresh
//! tick (`daemon_service/github_app_refresh.rs`) and reported by
//! `loom-daemon health`:
//!
//! - **Owner drift** (the re-pointed checkout): the tick compares each
//!   root's current `origin` owner with the one its credential was resolved
//!   for (the tick's [`DriftWatch`] baseline). On drift it repairs — a root
//!   re-pointed back at the workspace-root owner only needs its stale
//!   registration dropped ([`super::unregister_root_gh_config_dir`]; no
//!   plan covers a root-owner repo, so the establish pass alone would leave
//!   the old mapping in place) — and any other new owner is covered by one
//!   [`cross_owner::establish_for_workspace`] pass over the current
//!   remotes. A root the pass could not cover gets ONE actionable `warn!`
//!   per (root, from-owner, to-owner) pair naming the repo and the cure,
//!   rate-limited by that pair — never per tick. A cured root's key is
//!   un-marked, so a recurrence warns again.
//!
//! - **Canonical-name drift** (the transfer the remote was never re-pointed
//!   after): the forge answers `GET repos/{old}` with the new canonical
//!   `full_name` via redirect. Only when no owner drifted (that repair is
//!   in motion and supersedes this check), the tick asks
//!   [`canonical_full_name`] — bounded, cached per repo — and reports roots
//!   whose canonical owner differs from the remote's. A lookup failure is
//!   no verdict, never a false drift. The cure is an operator step (re-point
//!   the remote), so this case only ever warns.
//!
//! The pure comparison cores — [`detect_owner_drift`],
//! [`canonical_owner_differs`], [`detect_canonical_drift_with`] and the
//! [`DriftWatch`] warn bookkeeping — are tested without git or network in
//! `owner_drift_tests.rs`.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::{cross_owner, nwo_from_git_remote, owner_of_nwo};

/// One drifted checkout: the owner its credential was resolved for at
/// startup vs the owner its `origin` names now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerDrift {
    /// The checkout root.
    pub root: PathBuf,
    /// The `owner/repo` the credential was resolved for.
    pub startup_nwo: String,
    /// The `owner/repo` the checkout's `origin` names now.
    pub current_nwo: String,
}

/// One canonically-renamed checkout: the owner its `origin` names vs the
/// owner the forge answers under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalDrift {
    /// The checkout root.
    pub root: PathBuf,
    /// The `owner/repo` the checkout's `origin` names.
    pub remote_nwo: String,
    /// The `owner/repo` the forge answers under.
    pub canonical_nwo: String,
}

/// The key one warn is rate-limited by: `(root, from_owner, to_owner)`,
/// owners lowercased so a case-only rename dedups against the original.
pub type WarnKey = (PathBuf, String, String);

/// Whether two owner slugs name different owners. GitHub owner slugs are
/// case-insensitive (and the read route's `target_owner` lowercases its
/// answer), so a case-only difference is the SAME owner.
#[must_use]
pub fn owners_differ(a: &str, b: &str) -> bool {
    !a.eq_ignore_ascii_case(b)
}

/// Pure: which of `current`'s roots drifted from the owner their credential
/// was resolved for at `startup`. A root with no startup entry is new since
/// startup — not drift; a root that lost its remote never reaches this (the
/// caller's [`nwo_from_git_remote`] filter drops it first) — also not drift.
#[must_use]
pub fn detect_owner_drift(
    startup: &[(PathBuf, String)],
    current: &[(PathBuf, String)],
) -> Vec<OwnerDrift> {
    current
        .iter()
        .filter_map(|(root, current_nwo)| {
            let (_, startup_nwo) = startup.iter().find(|(r, _)| r == root)?;
            owners_differ(owner_of_nwo(startup_nwo), owner_of_nwo(current_nwo)).then(|| {
                OwnerDrift {
                    root: root.clone(),
                    startup_nwo: startup_nwo.clone(),
                    current_nwo: current_nwo.clone(),
                }
            })
        })
        .collect()
}

/// Pure: does the forge's canonical `full_name` for a repo resolve under a
/// different owner than the checkout's remote names? `canonical_full_name`
/// is what a transfer redirect answers with.
#[must_use]
pub fn canonical_owner_differs(remote_nwo: &str, canonical_full_name: &str) -> bool {
    owners_differ(owner_of_nwo(remote_nwo), owner_of_nwo(canonical_full_name))
}

/// The forge's canonical `owner/repo` for `nwo`:
/// `crate::telemetry::repo_identity`'s bounded, process-cached
/// `GET repos/{nwo}` — a transfer redirect answers the NEW canonical name.
/// `None` when the forge could not answer (no verdict — never a false
/// drift).
#[must_use]
pub fn canonical_full_name(nwo: &str) -> Option<String> {
    crate::telemetry::repo_identity::resolve(nwo).map(|identity| identity.full_name)
}

/// Pure modulo the injected canonical-name fetch: which of `pairs` the forge
/// answers under a different owner than the remote names. One fetch per
/// distinct nwo; a `None` answer is no verdict for that root.
#[must_use]
pub fn detect_canonical_drift_with(
    pairs: &[(PathBuf, String)],
    canonical: impl Fn(&str) -> Option<String>,
) -> Vec<CanonicalDrift> {
    let mut answered: HashMap<&str, Option<String>> = HashMap::new();
    pairs
        .iter()
        .filter_map(|(root, nwo)| {
            let canonical_nwo = match answered.entry(nwo.as_str()) {
                Entry::Occupied(e) => e.get().clone(),
                Entry::Vacant(e) => {
                    let answer = canonical(nwo);
                    e.insert(answer.clone());
                    answer
                }
            }?;
            canonical_owner_differs(nwo, &canonical_nwo).then_some(CanonicalDrift {
                root: root.clone(),
                remote_nwo: nwo.clone(),
                canonical_nwo,
            })
        })
        .collect()
}

/// [`detect_canonical_drift_with`] over the real forge.
#[must_use]
pub fn detect_canonical_drift(pairs: &[(PathBuf, String)]) -> Vec<CanonicalDrift> {
    detect_canonical_drift_with(pairs, canonical_full_name)
}

/// The warn key for an owner drift: `(root, from_owner, to_owner)`, owners
/// lowercased so a case-only rename dedups against the original.
#[must_use]
pub fn owner_drift_warn_key(d: &OwnerDrift) -> WarnKey {
    (
        d.root.clone(),
        owner_of_nwo(&d.startup_nwo).to_ascii_lowercase(),
        owner_of_nwo(&d.current_nwo).to_ascii_lowercase(),
    )
}

/// The ONE actionable warn for an owner drift the repair could not cover:
/// names the checkout, both owners, and the cure.
#[must_use]
pub fn owner_drift_warn_line(d: &OwnerDrift) -> String {
    format!(
        "credential_preflight: checkout {} now names {} but its credential was resolved for {} \
         at startup — reads/writes are shed under the stale credential; fix the checkout's \
         `origin` URL (the next refresh tick re-resolves it) or restart the daemon — #14223",
        d.root.display(),
        d.current_nwo,
        d.startup_nwo
    )
}

/// The warn key for a canonical-name drift.
#[must_use]
pub fn canonical_drift_warn_key(d: &CanonicalDrift) -> WarnKey {
    (
        d.root.clone(),
        owner_of_nwo(&d.remote_nwo).to_ascii_lowercase(),
        owner_of_nwo(&d.canonical_nwo).to_ascii_lowercase(),
    )
}

/// The ONE actionable warn for a canonical-name drift: names the checkout,
/// both names, and the operator cure (this case is never self-repairable —
/// the remote itself names the stale owner).
#[must_use]
pub fn canonical_drift_warn_line(d: &CanonicalDrift) -> String {
    format!(
        "credential_preflight: checkout {}'s `origin` names {} but the forge answers under {} \
         (the repo moved and the remote was never re-pointed) — `git remote set-url origin` \
         to the canonical name and restart the daemon so its credential re-resolves — #14223",
        d.root.display(),
        d.remote_nwo,
        d.canonical_nwo
    )
}

/// The state one primary-refresh tick task keeps across ticks (#14223): the
/// per-root `owner/repo` snapshot the credentials were resolved against
/// (captured at startup by `daemon_service::credential_startup`), and the
/// warn keys already logged once.
#[derive(Debug, Clone, Default)]
pub struct DriftWatch {
    startup_roots: Vec<(PathBuf, String)>,
    warned: HashSet<WarnKey>,
}

impl DriftWatch {
    /// The tick's baseline: the startup capture.
    #[must_use]
    pub fn new(startup_roots: Vec<(PathBuf, String)>) -> Self {
        Self {
            startup_roots,
            warned: HashSet::new(),
        }
    }

    /// The lines to log for `(key, line)` entries not warned yet, marking
    /// each — one warn per repo+owner pair, never per tick (#14223 AC).
    pub fn take_warn_lines(&mut self, entries: Vec<(WarnKey, String)>) -> Vec<String> {
        let mut lines = Vec::new();
        for (key, line) in entries {
            if self.warned.insert(key) {
                lines.push(line);
            }
        }
        lines
    }

    /// Adopt each cured root's current `owner/repo` as its baseline (the
    /// repair re-resolved its credential) and un-mark its warn key, so a
    /// recurrence of the same drift warns again. Uncured roots keep their
    /// startup baseline and keep being detected.
    pub fn cure_roots(&mut self, cured: &[OwnerDrift]) {
        for d in cured {
            if let Some(entry) = self.startup_roots.iter_mut().find(|(r, _)| r == &d.root) {
                entry.1 = d.current_nwo.clone();
            }
            self.warned.remove(&owner_drift_warn_key(d));
        }
    }
}

/// The blocking per-tick check and cure, against the real registries and
/// git. Called on a blocking thread by [`check_off_thread`].
fn check_blocking(
    workspace_root: &Path,
    root_owner_repo: &str,
    script_path: &Path,
    watch: &mut DriftWatch,
) {
    let current = cross_owner::managed_roots(workspace_root);
    let drift = detect_owner_drift(&watch.startup_roots, &current);
    if drift.is_empty() {
        // Canonical-name case, only when no owner drifted — a re-point
        // repair in motion supersedes this check for the drifted roots, and
        // re-running it every tick would add forge calls the repair makes
        // redundant.
        let canonical = detect_canonical_drift(&current);
        let entries = canonical
            .iter()
            .map(|d| (canonical_drift_warn_key(d), canonical_drift_warn_line(d)))
            .collect();
        for line in watch.take_warn_lines(entries) {
            log::warn!("{line}");
        }
        return;
    }
    // Owner drift. A root re-pointed back at the workspace-root owner rides
    // the process credential again: drop its stale registration first — no
    // plan covers a root-owner repo, so the establish pass below would
    // leave the old owner's mapping in place and the root unreachable.
    let root_owner = owner_of_nwo(root_owner_repo);
    for d in &drift {
        if owner_of_nwo(&d.current_nwo).eq_ignore_ascii_case(root_owner) {
            super::unregister_root_gh_config_dir(&d.root);
        }
    }
    // One establish pass over the current remotes covers every other
    // drifted root: new owners get minted and published, their roots
    // re-registered, and the refusals cached under the previous credential
    // expired (`register_plan`'s #5431 stale sweep). Owners with an
    // existing dir are not re-minted, so an unchanged fleet costs only the
    // registration lookups.
    let pass = cross_owner::establish_for_workspace(workspace_root, root_owner_repo, script_path);
    let (mut cured, mut broke) = (Vec::new(), Vec::new());
    for d in drift {
        let new_owner = owner_of_nwo(&d.current_nwo);
        let repaired = new_owner.eq_ignore_ascii_case(root_owner)
            || pass
                .established
                .iter()
                .any(|o| o.eq_ignore_ascii_case(new_owner));
        if repaired {
            cured.push(d);
        } else {
            broke.push(d);
        }
    }
    if !cured.is_empty() {
        log::info!(
            "credential_preflight: re-resolved the credential of {} drifted checkout(s) — #14223",
            cured.len()
        );
        watch.cure_roots(&cured);
    }
    let entries = broke
        .iter()
        .map(|d| (owner_drift_warn_key(d), owner_drift_warn_line(d)))
        .collect();
    for line in watch.take_warn_lines(entries) {
        log::warn!("{line}");
    }
}

/// [`check_blocking`] on a blocking thread, returning the advanced watch.
/// A join error keeps the previous watch (the check simply retries next
/// tick) and logs one warn.
pub async fn check_off_thread(
    workspace_root: PathBuf,
    root_owner_repo: String,
    script_path: PathBuf,
    watch: DriftWatch,
) -> DriftWatch {
    let fallback = watch.clone();
    tokio::task::spawn_blocking(move || {
        let mut watch = watch;
        check_blocking(&workspace_root, &root_owner_repo, &script_path, &mut watch);
        watch
    })
    .await
    .unwrap_or_else(|e| {
        log::warn!("credential_preflight: owner-drift check join error: {e} — #14223");
        fallback
    })
}

/// The `loom-daemon health` collection (#14223): canonical-name drift over
/// `roots` (the daemon-reported managed checkouts). The CLI process cannot
/// see the daemon's startup baseline, so this is the canonical comparison
/// only — visible from any process. A root with no GitHub remote is
/// dropped; a forge lookup failure is no verdict for that root, not a false
/// drift (#14223 AC).
#[must_use]
pub fn collect_canonical_drift(roots: &[PathBuf]) -> Vec<CanonicalDrift> {
    let pairs: Vec<(PathBuf, String)> = roots
        .iter()
        .filter_map(|root| nwo_from_git_remote(root).map(|nwo| (root.clone(), nwo)))
        .collect();
    detect_canonical_drift(&pairs)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#[path = "owner_drift_tests.rs"]
mod tests;
