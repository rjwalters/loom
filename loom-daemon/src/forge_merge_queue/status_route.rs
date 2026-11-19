//! Which checkout may configure the merge-queue status route (#14632).
//!
//! `forge.mergeQueue.statusCommand` (#12679) is per-target configuration, but
//! the handoff used to resolve it from whatever root the invocation happened
//! to run in. On a fleet worker that root can be an isolated checkout of the
//! target — a clone whose git-ignored `.loom-local/` tiers (including the
//! fleet-synced `fleet-repo` tier that carries the broker command) were never
//! materialized — or a checkout of a *different* repository entirely, with the
//! target named only by `--repo`. The broker then silently disappeared: the
//! post fell back to the writer credential, which the operator had
//! deliberately kept without `statuses: write`, and the handoff failed after
//! the grant with a 403 (2AMLogic/loom#14632).
//!
//! # The rule
//!
//! The command may be authorized only by a root that is verifiably **of the
//! exact target**:
//!
//! 1. a workspace registered on this machine whose `origin` is the target —
//!    the root the fleet sync materializes `.loom-local/fleet-repo.json` into,
//!    and the one the write-scope gate (#9548) already treats as managed;
//! 2. otherwise, the invocation's own checkout, when its `origin` is the
//!    target (a developer's clone, a CI checkout, a non-fleet host).
//!
//! A registered root wins over the invocation checkout even when the latter is
//! also a clone of the target: an isolated clone's untracked local tiers are
//! not the fleet's configuration for that repository. With no verifiable root,
//! a command configured in the invocation root **refuses** the route — a bare
//! `--repo` string cannot authorize a broker — while an unset one keeps the
//! ordinary direct status path for repositories that never configured a
//! broker. Global config precedence is untouched: this selects the root the
//! existing resolution reads, not a new tier order.
//!
//! # Diagnosis
//!
//! Every construction records one bounded observation
//! ([`ROUTE_LOG_RELATIVE_PATH`]): target, route, root class, an opaque root
//! identity, and the config tier that supplied the command. Never command
//! argv, credentials, environment, URLs or raw stderr (#14632 acceptance
//! criterion 1 — the facts the 2026-11-19 incident could not establish after
//! the fact).

use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};

use super::status_command::{self, StatusCommand, CONFIG_KEY};

/// Where route observations are appended, relative to the invocation root.
pub const ROUTE_LOG_RELATIVE_PATH: &str = ".loom/logs/merge-queue-route.jsonl";

/// How the root the status route was resolved from relates to the invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootClass {
    /// The invocation's own checkout, and it is the registered workspace for
    /// the target — the ordinary fleet case.
    RegisteredMatch,
    /// A registered workspace for the target, distinct from the invocation's
    /// checkout (the #14632 shape: the handoff ran from an isolated clone).
    RegisteredOther,
    /// No registered workspace for the target; the invocation's own checkout
    /// is one (a developer clone, a CI checkout, a non-fleet host).
    InvocationCheckout,
    /// No verifiable root for the target: the invocation root is neither.
    Unverified,
}

impl RootClass {
    /// Stable token for the observation record.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            RootClass::RegisteredMatch => "registered-match",
            RootClass::RegisteredOther => "registered-other",
            RootClass::InvocationCheckout => "invocation-checkout",
            RootClass::Unverified => "unverified",
        }
    }
}

/// Why a configured command refused the route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The command is configured in a root that cannot authorize the route
    /// for this target.
    UntrustedCommand,
    /// The setting is present but unusable (same failure class as #12679).
    Malformed,
}

impl Refusal {
    /// Stable token for the observation record.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Refusal::UntrustedCommand => "untrusted-command",
            Refusal::Malformed => "malformed",
        }
    }
}

/// The resolved status route for one target from one invocation root.
#[derive(Debug)]
pub struct ResolvedRoute {
    /// The root `command` was resolved from; `None` when unverified.
    pub trusted_root: Option<PathBuf>,
    /// How that root relates to the invocation.
    pub root_class: RootClass,
    /// `forge.mergeQueue.statusCommand` from the trusted root: `Ok(None)` is
    /// the direct route, `Err` refuses every post (fail closed, no fallback).
    pub command: Result<Option<StatusCommand>, String>,
    /// Typed token when `command` is `Err`.
    pub refusal: Option<Refusal>,
}

impl ResolvedRoute {
    /// The route this resolution selected.
    #[must_use]
    pub fn route(&self) -> &'static str {
        match &self.command {
            Ok(Some(_)) => "command",
            Ok(None) => "direct",
            Err(_) => "refused",
        }
    }
}

/// The `origin` `owner/repo` of the git checkout at `root`, when readable.
fn origin_nwo_of(root: &Path) -> Option<String> {
    crate::write_scope::target::origin_nwo(&crate::write_scope::target::read_remotes(root))
}

/// Canonical path for identity and comparison; the literal path when the
/// filesystem cannot canonicalize it.
fn canonical(root: &Path) -> PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}

/// The registered workspace roots on this machine: the production input of
/// [`resolve`]. A registry that cannot be read answers none (fail closed —
/// no root is then verifiable except the invocation's own checkout).
#[must_use]
pub fn default_registered_roots() -> Vec<PathBuf> {
    crate::workspace_registry::WorkspaceRegistry::load_default()
        .map(|r| r.roots())
        .unwrap_or_default()
}

/// Resolve the status route for `target` as invoked from `invocation_root`,
/// given the machine's registered workspace roots.
///
/// Pure with respect to the arguments (filesystem reads and one `git config`
/// per candidate root); the production caller passes
/// [`default_registered_roots`], tests pass explicit roots.
#[must_use]
pub fn resolve(invocation_root: &Path, target: &str, registered: &[PathBuf]) -> ResolvedRoute {
    // A linked worktree resolves to its main checkout, whose remotes and
    // local config tiers it shares (the same deref `Env::current` performs).
    let invocation_checkout = crate::repo_root::find_repo_root(invocation_root)
        .unwrap_or_else(|| invocation_root.to_path_buf());
    let is_target =
        |root: &Path| origin_nwo_of(root).is_some_and(|o| o.eq_ignore_ascii_case(target));

    let registered_match = registered
        .iter()
        .find(|r| is_target(r))
        .map(|r| canonical(r));

    let (trusted_root, root_class) = match &registered_match {
        Some(r) if *r == canonical(&invocation_checkout) => {
            (registered_match.clone(), RootClass::RegisteredMatch)
        }
        Some(_) => (registered_match, RootClass::RegisteredOther),
        None => {
            if is_target(&invocation_checkout) {
                (Some(invocation_checkout.clone()), RootClass::InvocationCheckout)
            } else {
                (None, RootClass::Unverified)
            }
        }
    };

    match trusted_root {
        Some(root) => {
            let command = status_command::resolve(&root);
            // A malformed setting in a trusted root keeps its #12679 class.
            let refusal = command.is_err().then_some(Refusal::Malformed);
            ResolvedRoute {
                command,
                refusal,
                root_class,
                trusted_root: Some(root),
            }
        }
        None => {
            // Nothing verifiable is of the target. The invocation root's own
            // value may not authorize a broker for it (#14632); an unset one
            // keeps the direct route, and a malformed one still refuses.
            let (command, refusal) = match status_command::resolve(invocation_root) {
                Ok(Some(_)) => (
                    Err(format!(
                        "{CONFIG_KEY} is configured in this root, but the root is neither a \
                         registered workspace for {target} nor a checkout of it, so it cannot \
                         authorize the status route for {target}; nothing was granted, posted \
                         or enqueued (#14632)"
                    )),
                    Some(Refusal::UntrustedCommand),
                ),
                Ok(None) => (Ok(None), None),
                Err(e) => (Err(e), Some(Refusal::Malformed)),
            };
            ResolvedRoute {
                command,
                refusal,
                root_class,
                trusted_root: None,
            }
        }
    }
}

/// Whether the invocation root's effective config carries a non-null
/// [`CONFIG_KEY`] at all (present-but-malformed still counts).
fn command_is_configured_in(root: &Path) -> bool {
    crate::config_resolver::get_path(
        &crate::config_resolver::resolve_effective_config(root),
        CONFIG_KEY,
    )
    .is_some_and(|v| !v.is_null())
}

/// The tier that supplied [`CONFIG_KEY`] in `root`, when any did.
fn command_tier(root: &Path) -> Option<&'static str> {
    crate::config_resolver::source_of(root, CONFIG_KEY).map(|s| tier_name(root, &s))
}

/// One bounded route observation (#14632). Field names are the contract; the
/// values never include command argv, credentials, environment, URLs, absolute
/// paths or raw stderr.
#[derive(Debug, Serialize)]
pub struct RouteRecord {
    /// RFC 3339.
    pub at: String,
    /// The `owner/repo` the route was resolved for.
    pub target: String,
    /// `command` | `direct` | `refused`.
    pub route: &'static str,
    /// [`RootClass::as_str`].
    pub root_class: &'static str,
    /// Whether the invocation's own checkout is the root the route came from.
    pub root_matches_trusted: bool,
    /// Opaque identity of the invocation checkout: `sha256:<12 hex>` of its
    /// canonical path (the path itself stays private evidence).
    pub invocation_root_id: String,
    /// Opaque identity of the trusted root, when one was found.
    pub trusted_root_id: Option<String>,
    /// Which config tier supplied the command (`local` | `fleet-repo` |
    /// `project` | `committed` | `defaults`), when one did.
    pub command_tier: Option<&'static str>,
    /// [`Refusal::as_str`] when the route is `refused`.
    pub refusal: Option<&'static str>,
}

/// `sha256:<12 hex>` of a root's canonical path — a stable opaque identity.
#[must_use]
pub fn opaque_root_id(root: &Path) -> String {
    let mut h = Sha256::new();
    h.update(canonical(root).to_string_lossy().as_bytes());
    format!("sha256:{}", &hex::encode(h.finalize())[..12])
}

/// Name the tier a config source file belongs to — the same mapping
/// `loom-daemon config get` reports.
fn tier_name(repo_root: &Path, source: &Path) -> &'static str {
    use crate::config_resolver::{
        FLEET_REPO_CONFIG_REL, LEGACY_CONFIG_REL, LOCAL_CONFIG_REL, PROJECT_CONFIG_REL,
    };
    let is = |rel: &str| source == repo_root.join(rel);
    if is(LOCAL_CONFIG_REL) {
        "local"
    } else if is(FLEET_REPO_CONFIG_REL) {
        "fleet-repo"
    } else if is(PROJECT_CONFIG_REL) {
        "project"
    } else if is(LEGACY_CONFIG_REL) {
        "committed"
    } else {
        "defaults"
    }
}

/// Record one observation for a resolved route: an info line always, and an
/// append to [`ROUTE_LOG_RELATIVE_PATH`] under the invocation root when that
/// root is a Loom surface (`.loom/` present). Consecutive duplicate
/// observations (same fields except `at`) are not re-appended, so a daemon
/// tick that constructs the forge every pass cannot grow the log unboundedly.
/// Best effort: a write failure is logged, never an error of the handoff.
pub fn record(invocation_root: &Path, resolved: &ResolvedRoute, target: &str) {
    let rec = RouteRecord {
        at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        target: target.to_string(),
        route: resolved.route(),
        root_class: resolved.root_class.as_str(),
        root_matches_trusted: resolved.trusted_root.as_ref().is_some_and(|t| {
            *t == canonical(
                &crate::repo_root::find_repo_root(invocation_root)
                    .unwrap_or_else(|| invocation_root.to_path_buf()),
            )
        }),
        invocation_root_id: opaque_root_id(invocation_root),
        trusted_root_id: resolved.trusted_root.as_deref().map(opaque_root_id),
        command_tier: resolved
            .trusted_root
            .as_deref()
            .and_then(command_tier)
            .or_else(|| {
                (resolved.root_class == RootClass::Unverified
                    && command_is_configured_in(invocation_root))
                .then(|| command_tier(invocation_root))
                .flatten()
            }),
        refusal: resolved.refusal.map(Refusal::as_str),
    };
    log::info!(
        "merge-queue: status route for {} is {} (root class {}, command tier {})",
        rec.target,
        rec.route,
        rec.root_class,
        rec.command_tier.unwrap_or("none")
    );
    if !invocation_root.join(".loom").is_dir() {
        return;
    }
    let path = invocation_root.join(ROUTE_LOG_RELATIVE_PATH);
    let line = match serde_json::to_string(&rec) {
        Ok(l) => l,
        Err(e) => {
            log::warn!("merge-queue: could not serialize the route record: {e}");
            return;
        }
    };
    let write = || -> std::io::Result<()> {
        if let Some(prev) = last_record(&path) {
            if record_equivalent(&prev, &line) {
                return Ok(());
            }
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(f, "{line}")
    };
    if let Err(e) = write() {
        log::warn!("merge-queue: could not record the status route at {}: {e}", path.display());
    }
}

/// The last complete line of the route log, parsed, when any.
fn last_record(path: &Path) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(path).ok()?;
    let line = text.lines().rev().find(|l| !l.trim().is_empty())?;
    serde_json::from_str(line).ok()
}

/// Two records are equivalent when every field except `at` matches.
fn record_equivalent(a: &serde_json::Value, line: &str) -> bool {
    let mut b = match serde_json::from_str::<serde_json::Value>(line) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let mut a = a.clone();
    if !a.is_object() || !b.is_object() {
        return false;
    }
    a.as_object_mut().unwrap().remove("at");
    b.as_object_mut().unwrap().remove("at");
    a == b
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_git_helpers::run as git;

    fn checkout_of(nwo: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        git(
            dir.path(),
            &[
                "remote",
                "add",
                "origin",
                &format!("https://github.com/{nwo}.git"),
            ],
        );
        std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
        std::fs::write(dir.path().join(".loom/config.json"), "{}").unwrap();
        dir
    }

    fn with_command(dir: &tempfile::TempDir, cfg: &str) {
        std::fs::write(dir.path().join(".loom/config.json"), cfg).unwrap();
    }

    const CMD: &str = r#"{"forge":{"mergeQueue":{"statusCommand":["true"]}}}"#;

    #[test]
    fn an_unverified_root_without_a_command_keeps_the_direct_route() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
        let r = resolve(dir.path(), "o/r", &[]);
        assert_eq!(r.root_class, RootClass::Unverified);
        assert!(matches!(r.command, Ok(None)), "{:?}", r.command);
        assert_eq!(r.route(), "direct");
    }

    #[test]
    fn an_unverified_root_with_a_command_refuses_the_route() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
        with_command(&dir, CMD);
        let r = resolve(dir.path(), "o/r", &[]);
        assert_eq!(r.root_class, RootClass::Unverified);
        let err = r.command.clone().unwrap_err();
        assert!(err.contains("cannot authorize"), "{err}");
        assert_eq!(r.refusal, Some(Refusal::UntrustedCommand));
        assert_eq!(r.route(), "refused");
    }

    #[test]
    fn a_malformed_command_in_an_unverified_root_still_refuses() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
        with_command(&dir, r#"{"forge":{"mergeQueue":{"statusCommand":"post-it"}}}"#);
        let r = resolve(dir.path(), "o/r", &[]);
        assert!(r.command.is_err());
        assert_eq!(r.refusal, Some(Refusal::Malformed));
    }

    #[test]
    fn the_invocation_checkout_is_trusted_when_it_is_of_the_target() {
        let dir = checkout_of("o/r");
        with_command(&dir, CMD);
        let r = resolve(dir.path(), "o/r", &[]);
        assert_eq!(r.root_class, RootClass::InvocationCheckout);
        assert!(matches!(r.command, Ok(Some(_))));
        assert_eq!(r.trusted_root.as_deref(), Some(dir.path()));
    }

    #[test]
    fn origin_matching_is_case_insensitive() {
        let dir = checkout_of("O/R");
        with_command(&dir, CMD);
        let r = resolve(dir.path(), "o/r", &[]);
        assert_eq!(r.root_class, RootClass::InvocationCheckout);
    }

    #[test]
    fn the_invocation_checkout_of_another_repo_is_unverified_for_the_target() {
        let dir = checkout_of("other/repo");
        with_command(&dir, CMD);
        // Registered for its own sake, not for the target.
        let r = resolve(dir.path(), "o/r", &[dir.path().to_path_buf()]);
        assert_eq!(r.root_class, RootClass::Unverified);
        assert!(r.command.is_err());
    }

    #[test]
    fn a_registered_root_for_the_target_wins_over_the_invocation_checkout() {
        let registered = checkout_of("o/r");
        with_command(&registered, CMD);
        let isolated = checkout_of("o/r"); // no command of its own
        let r = resolve(isolated.path(), "o/r", &[registered.path().to_path_buf()]);
        assert_eq!(r.root_class, RootClass::RegisteredOther);
        assert!(matches!(r.command, Ok(Some(_))), "the command came from the registered root");
        assert_eq!(r.trusted_root.as_deref(), Some(registered.path()));
    }

    #[test]
    fn the_registered_root_matching_the_invocation_is_a_match() {
        let dir = checkout_of("o/r");
        with_command(&dir, CMD);
        let r = resolve(dir.path(), "o/r", &[dir.path().to_path_buf()]);
        assert_eq!(r.root_class, RootClass::RegisteredMatch);
        assert!(matches!(r.command, Ok(Some(_))));
    }

    #[test]
    fn an_untrusted_local_override_in_the_isolated_root_is_not_the_route() {
        let registered = checkout_of("o/r");
        with_command(&registered, CMD);
        let isolated = checkout_of("o/r");
        // A local-tier command only the isolated clone carries: ignored,
        // because the registered root (which configures no such override)
        // owns the route. Distinguish the source by leaving the registered
        // command absent and the local one present: the route must then be
        // direct, proving the local value was never consulted.
        std::fs::create_dir_all(isolated.path().join(".loom-local")).unwrap();
        std::fs::write(isolated.path().join(".loom-local/local.json"), CMD).unwrap();
        std::fs::write(registered.path().join(".loom/config.json"), "{}").unwrap();
        let r = resolve(isolated.path(), "o/r", &[registered.path().to_path_buf()]);
        assert_eq!(r.root_class, RootClass::RegisteredOther);
        assert!(matches!(r.command, Ok(None)), "{:?}", r.command);
        assert_eq!(r.route(), "direct");
    }

    #[test]
    fn a_malformed_registered_command_refuses() {
        let registered = checkout_of("o/r");
        with_command(&registered, r#"{"forge":{"mergeQueue":{"statusCommand":"post-it"}}}"#);
        let isolated = checkout_of("o/r");
        let r = resolve(isolated.path(), "o/r", &[registered.path().to_path_buf()]);
        assert!(r.command.is_err());
        assert_eq!(r.refusal, Some(Refusal::Malformed));
    }

    #[test]
    fn the_record_carries_bounded_fields_and_no_command_text() {
        let registered = checkout_of("o/r");
        with_command(
            &registered,
            r#"{"forge":{"mergeQueue":{"statusCommand":["sh","-c","echo SECRET-ARGV","stub"]}}}"#,
        );
        let isolated = checkout_of("o/r");
        let r = resolve(isolated.path(), "o/r", &[registered.path().to_path_buf()]);
        record(isolated.path(), &r, "o/r");
        let text = std::fs::read_to_string(isolated.path().join(ROUTE_LOG_RELATIVE_PATH)).unwrap();
        assert!(!text.contains("SECRET-ARGV"), "no argv in the record: {text}");
        assert!(
            !text.contains(isolated.path().display().to_string().as_str()),
            "no path: {text}"
        );
        let v: serde_json::Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert_eq!(v["target"], "o/r");
        assert_eq!(v["route"], "command");
        assert_eq!(v["root_class"], "registered-other");
        assert_eq!(v["root_matches_trusted"], false);
        assert_eq!(v["command_tier"], "committed");
        assert_eq!(v["trusted_root_id"], v["trusted_root_id"]); // present
        assert!(v["invocation_root_id"]
            .as_str()
            .unwrap()
            .starts_with("sha256:"));
    }

    #[test]
    fn repeated_identical_observations_are_not_reappended() {
        let dir = checkout_of("o/r");
        with_command(&dir, CMD);
        let r = resolve(dir.path(), "o/r", &[]);
        record(dir.path(), &r, "o/r");
        record(dir.path(), &r, "o/r");
        let text = std::fs::read_to_string(dir.path().join(ROUTE_LOG_RELATIVE_PATH)).unwrap();
        assert_eq!(text.lines().count(), 1, "{text}");
    }

    #[test]
    fn nothing_is_recorded_outside_a_loom_surface() {
        let dir = tempfile::tempdir().unwrap();
        let r = resolve(dir.path(), "o/r", &[]);
        record(dir.path(), &r, "o/r");
        assert!(!dir
            .path()
            .join(".loom/logs")
            .join("merge-queue-route.jsonl")
            .exists());
    }

    #[test]
    fn a_fleet_repo_tier_command_reports_its_tier() {
        let registered = checkout_of("o/r");
        std::fs::create_dir_all(registered.path().join(".loom-local")).unwrap();
        std::fs::write(registered.path().join(".loom-local/fleet-repo.json"), CMD).unwrap();
        let isolated = checkout_of("o/r");
        let r = resolve(isolated.path(), "o/r", &[registered.path().to_path_buf()]);
        assert!(matches!(r.command, Ok(Some(_))));
        let tier = command_tier(registered.path()).unwrap();
        assert_eq!(tier, "fleet-repo");
    }
}
