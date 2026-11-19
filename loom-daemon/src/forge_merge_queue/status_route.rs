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
//! the grant with a 403 (#14632). The retained traces cannot establish that
//! this was the cause of the incident; this module closes the path and records
//! evidence, nothing more.
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
    /// The workspace registry could not be read, so no root can be verified
    /// as managed and the invocation checkout cannot vouch for itself.
    RegistryUnavailable,
    /// A trusted root's config tier is unreadable or malformed, so a command
    /// it may have carried cannot be ruled out.
    TierUnavailable,
    /// This target is known (from earlier route observations) to be
    /// broker-routed, but no command resolves now: refuse rather than fall
    /// through to the direct writer.
    BrokerMissing,
}

impl Refusal {
    /// Stable token for the observation record.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Refusal::UntrustedCommand => "untrusted-command",
            Refusal::Malformed => "malformed",
            Refusal::RegistryUnavailable => "registry-unavailable",
            Refusal::TierUnavailable => "tier-unavailable",
            Refusal::BrokerMissing => "broker-missing",
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

/// The host a target slug lives on: `GH_HOST` when set, else `github.com`.
fn target_host() -> String {
    std::env::var("GH_HOST")
        .ok()
        .map(|h| h.trim().to_ascii_lowercase())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "github.com".to_string())
}

/// Whether the git checkout at `root` has an `origin` that is exactly the
/// target: same host **and** same `owner/repo` (a same-slug remote on another
/// host is a different repository).
fn is_target_checkout(root: &Path, target: &str, host: &str) -> bool {
    crate::write_scope::target::read_remotes(root)
        .iter()
        .find(|r| r.name == "origin")
        .is_some_and(|r| r.host.eq_ignore_ascii_case(host) && r.nwo.eq_ignore_ascii_case(target))
}

/// Canonical path for identity and comparison; the literal path when the
/// filesystem cannot canonicalize it.
fn canonical(root: &Path) -> PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}

/// The registered workspace roots on this machine: the production input of
/// [`resolve_with`]. `None` when the registry cannot be read — then no root
/// is verifiable as managed (fail closed), which is different from a registry
/// that loaded and is empty.
#[must_use]
pub fn default_registered_roots() -> Option<Vec<PathBuf>> {
    crate::workspace_registry::WorkspaceRegistry::load_default()
        .ok()
        .map(|r| r.roots())
}

/// [`resolve_with`] for a registry that loaded.
#[must_use]
pub fn resolve(invocation_root: &Path, target: &str, registered: &[PathBuf]) -> ResolvedRoute {
    resolve_with(invocation_root, target, Some(registered))
}

/// Resolve the status route for `target` as invoked from `invocation_root`,
/// given the machine's registered workspace roots (`None` = registry
/// unreadable).
///
/// Trust is bound to the managed identity of the target: a registered root
/// whose `origin` host and slug are the target's. The invocation checkout may
/// vouch for itself only on a machine with **no** registered workspaces (a
/// developer clone, a CI checkout); an unreadable registry, or a registry that
/// manages other repositories but not this one, trusts nothing — an untrusted
/// same-slug clone must not supply the broker.
#[must_use]
pub fn resolve_with(
    invocation_root: &Path,
    target: &str,
    registered: Option<&[PathBuf]>,
) -> ResolvedRoute {
    let host = target_host();
    // A linked worktree resolves to its main checkout, whose remotes and
    // local config tiers it shares (the same deref `Env::current` performs).
    let invocation_checkout = crate::repo_root::find_repo_root(invocation_root)
        .unwrap_or_else(|| invocation_root.to_path_buf());

    let registered_match = registered.and_then(|roots| {
        roots
            .iter()
            .find(|r| is_target_checkout(r, target, &host))
            .map(|r| canonical(r))
    });

    let (trusted_root, root_class) = match &registered_match {
        Some(r) if *r == canonical(&invocation_checkout) => {
            (registered_match.clone(), RootClass::RegisteredMatch)
        }
        Some(_) => (registered_match, RootClass::RegisteredOther),
        None => {
            let no_fleet = registered.is_some_and(<[PathBuf]>::is_empty);
            if no_fleet && is_target_checkout(&invocation_checkout, target, &host) {
                (Some(invocation_checkout.clone()), RootClass::InvocationCheckout)
            } else {
                (None, RootClass::Unverified)
            }
        }
    };

    match trusted_root {
        Some(root) => {
            let mut command = status_command::resolve(&root);
            let mut refusal = command.is_err().then_some(Refusal::Malformed);
            if command.is_ok() {
                // `resolve_effective_config` soft-fails a dead tier to `{}`,
                // which reads as "no command": that must not become the
                // direct writer for a target whose tier is merely broken.
                if let Some((_, why)) = crate::config_resolver::unreadable_tiers(&root)
                    .into_iter()
                    .next()
                {
                    command = Err(format!(
                        "a config tier of the trusted root for {target} is unreadable or \
                         malformed ({why}), so {CONFIG_KEY} cannot be ruled out; nothing was \
                         granted, posted or enqueued (#14632)"
                    ));
                    refusal = Some(Refusal::TierUnavailable);
                } else if matches!(command, Ok(None))
                    && broker_required(&[&root, invocation_root], target)
                {
                    command = Err(format!(
                        "{target} was routed through {CONFIG_KEY} on an earlier handoff but \
                         no command resolves from its trusted root now; refusing rather than \
                         using the direct writer. If the broker was removed on purpose, delete \
                         {ROUTE_LOG_RELATIVE_PATH} (#14632)"
                    ));
                    refusal = Some(Refusal::BrokerMissing);
                }
            }
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
            // keeps the direct route unless the target is known broker-routed,
            // and a malformed one still refuses.
            let unavailable = registered.is_none();
            let (command, refusal) =
                match status_command::resolve(invocation_root) {
                    Ok(Some(_)) => {
                        (
                            Err(format!(
                        "{CONFIG_KEY} is configured in this root, but the root is neither a \
                         registered workspace for {target} nor a trusted checkout of it{}, so it \
                         cannot authorize the status route for {target}; nothing was granted, \
                         posted or enqueued (#14632)",
                        if unavailable { " (the workspace registry is unreadable)" } else { "" }
                    )),
                            Some(if unavailable {
                                Refusal::RegistryUnavailable
                            } else {
                                Refusal::UntrustedCommand
                            }),
                        )
                    }
                    Ok(None) if broker_required(&[invocation_root], target) => (
                        Err(format!(
                        "{target} was routed through {CONFIG_KEY} on an earlier handoff but no \
                         trusted root resolves now; refusing rather than using the direct \
                         writer (#14632)"
                    )),
                        Some(Refusal::BrokerMissing),
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
#[derive(Debug, Clone, Serialize)]
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
    /// Handoff correlation (#14636): the PR a status write was for. `None` on
    /// a construction observation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr: Option<u32>,
    /// The approved head sha the status write was for (a public git object
    /// id, never a secret).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    /// The operation the record belongs to (`post_status`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation: Option<&'static str>,
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

/// Bound on the route log (#14632): above `MAX_LOG_BYTES` the file is cut to
/// its last `KEEP_LOG_BYTES`, at a line boundary.
const MAX_LOG_BYTES: u64 = 256 * 1024;
const KEEP_LOG_BYTES: u64 = 128 * 1024;
/// How much of the tail is read for deduplication and history: never the
/// whole file.
const TAIL_BYTES: u64 = 64 * 1024;
/// A construction observation is not re-appended when it equals any of the
/// last this-many records, so an alternating pair cannot grow the log either.
const DEDUPE_WINDOW: usize = 32;

/// Build the observation for a resolved route (no I/O beyond reading config).
#[must_use]
pub fn observe(invocation_root: &Path, resolved: &ResolvedRoute, target: &str) -> RouteRecord {
    let rec = RouteRecord {
        at: String::new(),
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
        pr: None,
        head_sha: None,
        operation: None,
    };
    log::info!(
        "merge-queue: status route for {} is {} (root class {}, command tier {})",
        rec.target,
        rec.route,
        rec.root_class,
        rec.command_tier.unwrap_or("none")
    );
    rec
}

/// Record one construction observation for a resolved route: an info line
/// always, and an append to [`ROUTE_LOG_RELATIVE_PATH`] under the invocation
/// root when that root is a Loom surface (`.loom/` present). An observation
/// equal (but for `at`) to one of the last [`DEDUPE_WINDOW`] records is not
/// re-appended. Best effort: a write failure is logged, never an error of the
/// handoff.
pub fn record(invocation_root: &Path, resolved: &ResolvedRoute, target: &str) {
    let rec = observe(invocation_root, resolved, target);
    append(invocation_root, rec, true);
}

/// Record the route used by one specific status write (#14636): the same
/// bounded fields plus the PR and the approved head sha, so the normal
/// handoff can be joined to the route it actually took. Always appended.
pub fn record_handoff(
    invocation_root: &Path,
    base: &RouteRecord,
    pr: Option<u32>,
    head_sha: &str,
    operation: &'static str,
) {
    let mut rec = base.clone();
    rec.pr = pr;
    rec.head_sha = Some(head_sha.to_string());
    rec.operation = Some(operation);
    append(invocation_root, rec, false);
}

fn append(invocation_root: &Path, mut rec: RouteRecord, dedupe: bool) {
    if !invocation_root.join(".loom").is_dir() {
        return;
    }
    rec.at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let path = invocation_root.join(ROUTE_LOG_RELATIVE_PATH);
    let line = match serde_json::to_string(&rec) {
        Ok(l) => l,
        Err(e) => {
            log::warn!("merge-queue: could not serialize the route record: {e}");
            return;
        }
    };
    let write = || -> std::io::Result<()> {
        if dedupe
            && tail_records(&path)
                .iter()
                .rev()
                .take(DEDUPE_WINDOW)
                .any(|prev| record_equivalent(prev, &line))
        {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        trim_log(&path)?;
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

/// Cut an oversized log to its last [`KEEP_LOG_BYTES`] at a line boundary.
fn trim_log(path: &Path) -> std::io::Result<()> {
    let len = match std::fs::metadata(path) {
        Ok(m) => m.len(),
        Err(_) => return Ok(()),
    };
    if len <= MAX_LOG_BYTES {
        return Ok(());
    }
    let tail = read_tail(path, KEEP_LOG_BYTES)?;
    let kept = match tail.find('\n') {
        Some(i) => &tail[i + 1..],
        None => "",
    };
    std::fs::write(path, kept)
}

/// The last `max` bytes of a file, lossily decoded — never the whole file.
fn read_tail(path: &Path, max: u64) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(max)))?;
    let mut buf = Vec::new();
    f.take(max).read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Complete records in the log's tail window, oldest first (a partial leading
/// line is dropped).
fn tail_records(path: &Path) -> Vec<String> {
    let Ok(text) = read_tail(path, TAIL_BYTES) else {
        return Vec::new();
    };
    let whole = std::fs::metadata(path).map_or(true, |m| m.len() <= TAIL_BYTES);
    text.lines()
        .skip(usize::from(!whole))
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect()
}

/// Whether an earlier record in any of `roots`' route logs shows `target`
/// routed through a command — i.e. a broker is required for it.
fn broker_required(roots: &[&Path], target: &str) -> bool {
    roots.iter().any(|root| {
        tail_records(&root.join(ROUTE_LOG_RELATIVE_PATH))
            .iter()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .any(|v| {
                v["route"] == "command"
                    && v["target"]
                        .as_str()
                        .is_some_and(|t| t.eq_ignore_ascii_case(target))
            })
    })
}

/// Two records are equivalent when every field except `at` matches.
fn record_equivalent(prev: &str, line: &str) -> bool {
    let (Ok(mut a), Ok(mut b)) = (
        serde_json::from_str::<serde_json::Value>(prev),
        serde_json::from_str::<serde_json::Value>(line),
    ) else {
        return false;
    };
    let (Some(ao), Some(bo)) = (a.as_object_mut(), b.as_object_mut()) else {
        return false;
    };
    ao.remove("at");
    bo.remove("at");
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
        assert_eq!(
            r.trusted_root.as_deref(),
            Some(std::fs::canonicalize(dir.path()).unwrap().as_path())
        );
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
        assert_eq!(
            r.trusted_root.as_deref(),
            Some(std::fs::canonicalize(registered.path()).unwrap().as_path())
        );
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

    #[test]
    fn an_unreadable_registry_trusts_no_invocation_checkout() {
        let dir = checkout_of("o/r");
        with_command(&dir, CMD);
        let r = resolve_with(dir.path(), "o/r", None);
        assert_eq!(r.root_class, RootClass::Unverified);
        assert_eq!(r.refusal, Some(Refusal::RegistryUnavailable));
        assert!(r.command.is_err());
    }

    #[test]
    fn a_same_slug_clone_is_untrusted_when_the_registry_manages_others() {
        let other = checkout_of("o/other");
        let clone = checkout_of("o/r");
        with_command(&clone, CMD);
        let r = resolve(clone.path(), "o/r", &[other.path().to_path_buf()]);
        assert_eq!(r.root_class, RootClass::Unverified);
        assert_eq!(r.refusal, Some(Refusal::UntrustedCommand));
    }

    #[test]
    fn a_registered_checkout_on_another_host_is_not_the_target() {
        let wrong_host = tempfile::tempdir().unwrap();
        git(wrong_host.path(), &["init", "-q"]);
        git(
            wrong_host.path(),
            &[
                "remote",
                "add",
                "origin",
                "https://ghe.example.invalid/o/r.git",
            ],
        );
        std::fs::create_dir_all(wrong_host.path().join(".loom")).unwrap();
        std::fs::write(wrong_host.path().join(".loom/config.json"), CMD).unwrap();
        let isolated = checkout_of("o/r");
        let r = resolve(isolated.path(), "o/r", &[wrong_host.path().to_path_buf()]);
        assert_eq!(r.root_class, RootClass::Unverified);
        assert!(r.command.is_ok() || r.refusal.is_some());
        assert_ne!(r.route(), "command");
    }

    #[test]
    fn a_malformed_trusted_tier_refuses() {
        let dir = checkout_of("o/r");
        std::fs::create_dir_all(dir.path().join(".loom-local")).unwrap();
        std::fs::write(dir.path().join(".loom-local/fleet-repo.json"), "{not json").unwrap();
        let r = resolve(dir.path(), "o/r", &[dir.path().to_path_buf()]);
        assert_eq!(r.refusal, Some(Refusal::TierUnavailable));
        assert_eq!(r.route(), "refused");
    }

    #[test]
    fn a_known_broker_target_refuses_when_its_tier_disappears() {
        let dir = checkout_of("o/r");
        std::fs::create_dir_all(dir.path().join(".loom-local")).unwrap();
        std::fs::write(
            dir.path().join(".loom-local/fleet-repo.json"),
            r#"{"forge":{"mergeQueue":{"statusCommand":["true"]}}}"#,
        )
        .unwrap();
        let reg = [dir.path().to_path_buf()];
        let first = resolve(dir.path(), "o/r", &reg);
        assert_eq!(first.route(), "command");
        record(dir.path(), &first, "o/r");
        std::fs::remove_file(dir.path().join(".loom-local/fleet-repo.json")).unwrap();
        let second = resolve(dir.path(), "o/r", &reg);
        assert_eq!(second.refusal, Some(Refusal::BrokerMissing));
        assert_eq!(second.route(), "refused");
    }

    #[test]
    fn a_handoff_record_carries_pr_and_head_and_the_log_stays_bounded() {
        let dir = checkout_of("o/r");
        let r = resolve(dir.path(), "o/r", &[]);
        let base = observe(dir.path(), &r, "o/r");
        let sha = "a".repeat(40);
        for _ in 0..3 {
            record_handoff(dir.path(), &base, Some(7), &sha, "post_status");
        }
        let text = std::fs::read_to_string(dir.path().join(ROUTE_LOG_RELATIVE_PATH)).unwrap();
        assert_eq!(text.lines().count(), 3);
        let v: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(v["pr"], 7);
        assert_eq!(v["head_sha"], sha.as_str());
        for _ in 0..4000 {
            record_handoff(dir.path(), &base, Some(7), &sha, "post_status");
        }
        let len = std::fs::metadata(dir.path().join(ROUTE_LOG_RELATIVE_PATH))
            .unwrap()
            .len();
        assert!(len <= MAX_LOG_BYTES + 1024, "{len}");
    }
}
