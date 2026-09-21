//! Machine-wide **in-flight verification registry** — makes a long-running
//! command *visible* to every other agent on the host, so a sibling or a
//! coordinator can see "that suite is already running against this tree" and
//! skip launching a second copy (Issue #8268).
//!
//! # Why this exists
//!
//! [`crate::build_slot`] already bounds how *many* heavy commands run at once.
//! It deliberately says nothing about *what* they are: a slot is anonymous, and
//! two agents running the byte-identical test suite against the byte-identical
//! tree consume two slots and produce one answer. That is the gap #8268
//! measured in a ~10-agent session — an agent and its coordinator each ran the
//! same ~15-minute suite against the same branch, plus a third run after a
//! rebase, because **neither could see the other**. Each decision was locally
//! correct; the missing input was mutual visibility, not discipline.
//!
//! So this registry is the identity half of the pair:
//!
//! | Module | Question it answers | Primitive |
//! |---|---|---|
//! | [`crate::build_slot`] | "may I run something heavy *now*?" | anonymous counted slot |
//! | this module | "is *this exact command* already running against *this tree*?" | fingerprint-keyed claim |
//!
//! They compose and do not interfere: a caller takes a build slot to be
//! polite about CPU and claims an in-flight fingerprint to avoid duplicating
//! an answer. Neither is required for the other.
//!
//! # The claim closes the check-then-launch race (deliberately)
//!
//! A pure read API ("is it running?") has an unavoidable TOCTOU window: two
//! agents ask simultaneously, both see clear, both launch — reproducing the
//! exact duplication the registry exists to prevent. So the primitive callers
//! are told to use is **[`claim_in`]**, which is atomic: the entry is a
//! directory created with `mkdir` (POSIX-atomic across processes *and*
//! languages, the same primitive [`crate::build_slot`] and the per-issue claim
//! lock use — `flock` is avoided because it is unavailable on stock macOS).
//! Exactly one of N concurrent claimants wins; the losers are handed the
//! winner's identity so they can report "already running" instead of guessing.
//!
//! [`check_in`] exists too, but it is explicitly advisory — for a coordinator
//! *reporting* fleet state, not for deciding to launch. That distinction is
//! the whole of #8268's third acceptance criterion and is repeated in the CLI
//! help text so it cannot be missed at the call site.
//!
//! # Not RAII, on purpose
//!
//! [`crate::tokens_pool::locking::MkdirLock`] releases on drop, which is right
//! when the holder *is* the process holding the guard. Here it would be wrong:
//! the claimant is a shell/agent that invokes `loom-daemon inflight claim`,
//! gets an answer, and then runs the actual 15-minute command in a **different
//! process** after the CLI has already exited. A drop-release would free the
//! claim microseconds before the work it describes even starts. So the entry
//! outlives this process and is released by an explicit
//! `loom-daemon inflight release`, or reaped by staleness.
//!
//! # Staleness: two independent legs, so a crash cannot wedge the host
//!
//! 1. **Dead owner PID** — reaped immediately (the [`crate::live_claim`]
//!    discipline the `loom:building` lease already uses). This is the common
//!    case: an agent is killed mid-suite and never runs `release`.
//! 2. **Age past [`resolve_stale`]** — the backstop for a PID that was never
//!    recorded, or was recycled. The default is deliberately long
//!    ([`DEFAULT_STALE_SECS`], 4h): entries describe whole test suites and
//!    release builds, and a short threshold would let a peer declare a
//!    perfectly healthy 20-minute run "stale" and duplicate it — the precise
//!    failure being prevented. [`crate::build_slot`] reasons the same way about
//!    its own 1h threshold.
//!
//! # Degrades open, always
//!
//! An unusable store (no `$HOME`, unwritable path) yields
//! [`ClaimOutcome::DegradedOpen`] and the caller **runs its command**. Refusing
//! to verify because a bookkeeping directory is broken would convert a
//! wasted-wall-clock problem into a correctness outage. Duplicated work is the
//! failure this module reduces; skipped verification is a worse one it must
//! never cause.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Overrides the machine-wide store directory. Primarily a test seam; also
/// lets an operator relocate the registry off a home directory.
pub const INFLIGHT_DIR_ENV: &str = "LOOM_INFLIGHT_DIR";

/// Overrides the staleness threshold, in seconds. Zero/invalid falls through
/// to [`DEFAULT_STALE_SECS`].
pub const INFLIGHT_STALE_SECS_ENV: &str = "LOOM_INFLIGHT_STALE_SECS";

/// Default staleness threshold: 4 hours. See the module docs — long on
/// purpose, because a too-eager reap recreates the duplication this prevents.
pub const DEFAULT_STALE_SECS: u64 = 14_400;

/// The file recording an entry's owner, inside the fingerprint directory.
const OWNER_FILE: &str = "owner.json";

/// One registered in-flight command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registration {
    /// Stable key derived from (command, tree, branch) — see [`fingerprint`].
    pub fingerprint: String,
    /// The command as the claimant described it, recorded verbatim for the
    /// human/agent who is told "this is already running".
    pub command: String,
    /// The working tree the command runs against (canonicalized when possible).
    pub tree: String,
    /// The branch or ref the command verifies. Empty when the claimant did not
    /// supply one — a deliberately *distinct* key from any named branch.
    #[serde(default)]
    pub branch: String,
    /// Liveness handle. `0` means "not supplied": age-based staleness alone
    /// governs such an entry.
    #[serde(default)]
    pub pid: u32,
    /// Free-form claimant identity (role, session, sweep run id) for reporting.
    #[serde(default)]
    pub agent: String,
    /// When the claim was taken.
    pub started_at: DateTime<Utc>,
}

impl Registration {
    /// Seconds since [`Self::started_at`], saturating at zero for a clock skew
    /// that would otherwise underflow.
    #[must_use]
    pub fn age_secs(&self) -> i64 {
        (Utc::now() - self.started_at).num_seconds().max(0)
    }

    /// One-line human summary, used by every CLI verb that reports a holder.
    #[must_use]
    pub fn summary(&self) -> String {
        let branch = if self.branch.is_empty() {
            "(no branch)".to_string()
        } else {
            self.branch.clone()
        };
        let agent = if self.agent.is_empty() {
            "unknown agent".to_string()
        } else {
            self.agent.clone()
        };
        format!(
            "{} — tree {} @ {} — started {}s ago by {} (pid {}, fingerprint {})",
            self.command,
            self.tree,
            branch,
            self.age_secs(),
            agent,
            self.pid,
            self.fingerprint,
        )
    }
}

/// The outcome of [`claim_in`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// The caller now owns the fingerprint and should run its command.
    Claimed(Registration),
    /// Someone else is already running this command against this tree. The
    /// caller should **not** launch a duplicate; report the holder instead.
    Busy(Registration),
    /// The store is unusable. The caller runs its command unserialized — see
    /// the module docs on degrading open.
    DegradedOpen,
}

/// Collapse a command string to its fingerprintable form: trimmed, with every
/// run of ASCII whitespace folded to a single space.
///
/// This is intentionally **not** shell-aware. `cargo test --all` and
/// `cargo  test   --all` are the same entry; `cargo test --all` and
/// `cargo test --workspace` are not, even when they run identically. An
/// over-clever normalizer that merged genuinely different commands would
/// suppress a verification someone needed — strictly worse than the duplicate
/// run it saved. Callers who want two spellings deduplicated should pass the
/// same spelling.
#[must_use]
pub fn normalize_command(command: &str) -> String {
    command.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Canonicalize `tree` when the path exists, else return it trimmed. Keeps
/// `/repo` and `/repo/` (and a symlinked equivalent) on one fingerprint.
#[must_use]
pub fn normalize_tree(tree: &str) -> String {
    let trimmed = tree.trim();
    std::fs::canonicalize(trimmed)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| trimmed.trim_end_matches('/').to_string())
}

/// The stable key for (command, tree, branch). Unit-separated before hashing
/// so no field boundary can be forged by embedding the separator in a value.
#[must_use]
pub fn fingerprint(command: &str, tree: &str, branch: &str) -> String {
    let joined = format!(
        "{}\x1f{}\x1f{}",
        normalize_command(command),
        normalize_tree(tree),
        branch.trim()
    );
    crate::short_hash::short_sha16(&joined)
}

/// The machine-wide store directory: [`INFLIGHT_DIR_ENV`] when set, else
/// `~/.loom/locks/inflight`. `None` when neither resolves — the caller
/// degrades open.
///
/// Machine-wide, not per-workspace, for the same reason
/// [`crate::build_slot`]'s slots are: the agents that collide here routinely
/// live in *different* checkouts of the same repo (a coordinator in the
/// primary clone, a builder in `.loom/worktrees/issue-N`), so a per-workspace
/// store would make them invisible to each other — the original bug.
#[must_use]
pub fn store_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var(INFLIGHT_DIR_ENV) {
        let trimmed = dir.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    Some(
        dirs::home_dir()?
            .join(".loom")
            .join("locks")
            .join("inflight"),
    )
}

/// Resolve the staleness threshold with precedence **env > default**.
#[must_use]
pub fn resolve_stale() -> Duration {
    let secs = std::env::var(INFLIGHT_STALE_SECS_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .unwrap_or(DEFAULT_STALE_SECS);
    Duration::from_secs(secs)
}

/// Path of one fingerprint's entry directory inside `store`.
#[must_use]
pub fn entry_path(store: &Path, fingerprint: &str) -> PathBuf {
    store.join(sanitize_component(fingerprint))
}

/// Reduce a fingerprint to a safe single path component. [`fingerprint`] only
/// ever produces hex, but `release`/`check` accept an operator-supplied string,
/// so this guarantees no `/` or `..` can escape the store.
fn sanitize_component(value: &str) -> String {
    let mapped: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if mapped.is_empty() {
        "_".to_string()
    } else {
        mapped
    }
}

/// Read the owner record at `entry`, or `None` when absent/unparseable.
fn read_owner(entry: &Path) -> Option<Registration> {
    let raw = std::fs::read_to_string(entry.join(OWNER_FILE)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Whether `entry` is stale: its owner PID is dead, or it has aged past
/// `stale`. An entry with no readable owner record is stale once it has aged
/// past `stale` (never immediately — a claimant that has created the directory
/// but not yet written `owner.json` is mid-claim, not abandoned).
fn is_stale(entry: &Path, stale: Duration) -> bool {
    let aged = std::fs::metadata(entry)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| SystemTime::now().duration_since(m).ok())
        .is_some_and(|age| age > stale);

    match read_owner(entry) {
        Some(owner) => {
            if owner.pid != 0 && !crate::live_claim::pid_is_live_process(owner.pid) {
                return true;
            }
            aged
        }
        None => aged,
    }
}

/// Remove `entry` and its contents, ignoring failure (a racing peer may have
/// reaped it first — an ordinary outcome, not an error).
fn reap(entry: &Path) {
    let _ = std::fs::remove_dir_all(entry);
}

/// Atomically claim `reg`'s fingerprint in `store`.
///
/// This is the primitive a caller that intends to **launch** should use: it
/// answers "is it running?" and "may I run it?" in one indivisible step, so
/// two agents asking simultaneously cannot both proceed.
pub fn claim_in(store: &Path, reg: &Registration, stale: Duration) -> ClaimOutcome {
    if std::fs::create_dir_all(store).is_err() {
        return ClaimOutcome::DegradedOpen;
    }
    let entry = entry_path(store, &reg.fingerprint);

    match std::fs::create_dir(&entry) {
        Ok(()) => write_owner(&entry, reg),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            if !is_stale(&entry, stale) {
                // A live holder with no readable record still blocks — we know
                // *something* is running this fingerprint even if we cannot
                // name it, and duplicating it is exactly what to avoid.
                return match read_owner(&entry) {
                    Some(owner) => ClaimOutcome::Busy(owner),
                    None => ClaimOutcome::Busy(Registration {
                        agent: "unknown (owner record not yet written)".to_string(),
                        pid: 0,
                        ..reg.clone()
                    }),
                };
            }
            reap(&entry);
            // Exactly one more attempt: a racing peer may have reaped and
            // re-taken it first, which is an ordinary Busy, not an error.
            match std::fs::create_dir(&entry) {
                Ok(()) => write_owner(&entry, reg),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    match read_owner(&entry) {
                        Some(owner) => ClaimOutcome::Busy(owner),
                        None => ClaimOutcome::Busy(reg.clone()),
                    }
                }
                Err(_) => ClaimOutcome::DegradedOpen,
            }
        }
        Err(_) => ClaimOutcome::DegradedOpen,
    }
}

/// Persist `reg` into an entry directory this process just created.
///
/// A failed write is **not** downgraded to `DegradedOpen`: the directory now
/// exists and is ours, so the mutual exclusion the caller asked for holds. We
/// simply lose the ability to describe ourselves to a later peer, which
/// [`claim_in`]'s unreadable-owner branch already handles.
fn write_owner(entry: &Path, reg: &Registration) -> ClaimOutcome {
    if let Ok(json) = serde_json::to_string_pretty(reg) {
        let _ = std::fs::write(entry.join(OWNER_FILE), json);
    }
    ClaimOutcome::Claimed(reg.clone())
}

/// Advisory read: the live holder of `fingerprint`, if any, reaping it when
/// stale.
///
/// **Advisory means advisory.** Deciding to launch on the strength of a `None`
/// here reintroduces the check-then-launch race; use [`claim_in`] for that.
/// This verb is for a coordinator *reporting* what is in flight.
pub fn check_in(store: &Path, fingerprint: &str, stale: Duration) -> Option<Registration> {
    let entry = entry_path(store, fingerprint);
    if !entry.exists() {
        return None;
    }
    if is_stale(&entry, stale) {
        reap(&entry);
        return None;
    }
    read_owner(&entry)
}

/// Release `fingerprint`. Returns whether an entry was removed.
///
/// Ownership is checked by PID when `pid` is supplied and the entry records a
/// non-zero one: releasing someone else's in-flight claim would re-open the
/// duplication window for them. `force` skips that check for operator cleanup.
pub fn release_in(store: &Path, fingerprint: &str, pid: Option<u32>, force: bool) -> bool {
    let entry = entry_path(store, fingerprint);
    if !entry.exists() {
        return false;
    }
    if !force {
        if let (Some(mine), Some(owner)) = (pid, read_owner(&entry)) {
            if owner.pid != 0 && owner.pid != mine {
                return false;
            }
        }
    }
    reap(&entry);
    true
}

/// Every live registration in `store` whose `tree` is at or under `dir`
/// (Issue #8413).
///
/// The registry's second consumer: besides "is this command already running?",
/// an entry is *evidence that a live agent is working inside a directory* — the
/// one liveness signal an in-session builder (no sweep record, no claim-lock,
/// no long-lived process) can publish about itself.
/// [`crate::worktree_activity`] turns that into a veto on destructive worktree
/// operations, and [`crate::sweep_registry`]'s mid-build watchdog into
/// `WorktreeUseEvidence::InflightClaim`.
///
/// Containment is compared **component-wise** (`Path::starts_with`), so
/// `.loom/worktrees/issue-84` never matches a claim on
/// `.loom/worktrees/issue-8413`. Stale entries are reaped by [`list_in`] as a
/// side effect, so a dead claimant never fences a directory off forever.
#[must_use]
pub fn holders_for_tree_in(store: &Path, dir: &Path, stale: Duration) -> Vec<Registration> {
    let root = PathBuf::from(normalize_tree(&dir.to_string_lossy()));
    list_in(store, stale)
        .into_iter()
        .filter(|reg| PathBuf::from(normalize_tree(&reg.tree)).starts_with(&root))
        .collect()
}

/// [`holders_for_tree_in`] against the production store ([`store_dir`]) and
/// staleness threshold ([`resolve_stale`]). An unresolvable store yields an
/// empty list — absent evidence, never an error.
#[must_use]
pub fn holders_for_tree(dir: &Path) -> Vec<Registration> {
    match store_dir() {
        Some(store) => holders_for_tree_in(&store, dir, resolve_stale()),
        None => Vec::new(),
    }
}

/// Every live entry in `store`, reaping stale ones as a side effect. Sorted
/// oldest-first so a reader sees the longest-running command at the top.
pub fn list_in(store: &Path, stale: Duration) -> Vec<Registration> {
    let Ok(entries) = std::fs::read_dir(store) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if is_stale(&path, stale) {
            reap(&path);
            continue;
        }
        if let Some(owner) = read_owner(&path) {
            out.push(owner);
        }
    }
    out.sort_by_key(|r| r.started_at);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg(command: &str, tree: &str, branch: &str, pid: u32) -> Registration {
        Registration {
            fingerprint: fingerprint(command, tree, branch),
            command: command.to_string(),
            tree: normalize_tree(tree),
            branch: branch.to_string(),
            pid,
            agent: "test".to_string(),
            started_at: Utc::now(),
        }
    }

    fn tmp_store(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "loom-inflight-test-{name}-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn normalizes_whitespace_but_not_semantics() {
        assert_eq!(normalize_command("  cargo   test  --all "), "cargo test --all");
        assert_ne!(
            normalize_command("cargo test --all"),
            normalize_command("cargo test --workspace")
        );
    }

    #[test]
    fn fingerprint_is_field_separated() {
        // Without the unit separator these two would collide.
        assert_ne!(fingerprint("a b", "/t", ""), fingerprint("a", "b /t", ""));
        // Same inputs, same key; different branch, different key.
        assert_eq!(fingerprint("x", "/t", "main"), fingerprint("x", "/t", "main"));
        assert_ne!(fingerprint("x", "/t", "main"), fingerprint("x", "/t", "dev"));
    }

    #[test]
    fn sanitize_component_cannot_escape_the_store() {
        let store = Path::new("/store");
        let path = entry_path(store, "../../etc/passwd");
        assert_eq!(path.parent(), Some(store));
        assert!(!path.to_string_lossy().contains(".."));
    }

    #[test]
    fn claim_then_second_claim_is_busy() {
        let store = tmp_store("busy");
        let stale = Duration::from_secs(3600);
        let mine = reg("cargo test", "/tree", "main", std::process::id());

        assert!(matches!(claim_in(&store, &mine, stale), ClaimOutcome::Claimed(_)));

        let theirs = reg("cargo test", "/tree", "main", std::process::id());
        match claim_in(&store, &theirs, stale) {
            ClaimOutcome::Busy(owner) => assert_eq!(owner.agent, "test"),
            other => panic!("expected Busy, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn a_different_tree_is_a_different_entry() {
        let store = tmp_store("trees");
        let stale = Duration::from_secs(3600);

        let a = reg("cargo test", "/tree-a", "main", std::process::id());
        let b = reg("cargo test", "/tree-b", "main", std::process::id());
        assert!(matches!(claim_in(&store, &a, stale), ClaimOutcome::Claimed(_)));
        assert!(matches!(claim_in(&store, &b, stale), ClaimOutcome::Claimed(_)));
        assert_eq!(list_in(&store, stale).len(), 2);

        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn dead_owner_pid_is_reaped_immediately() {
        let store = tmp_store("deadpid");
        let stale = Duration::from_secs(86_400);

        // PID 0 means "not supplied"; use a PID that is almost certainly dead
        // but non-zero so the liveness leg (not the age leg) is what fires.
        let dead = reg("pnpm check:ci", "/tree", "main", 4_294_967_294);
        assert!(matches!(claim_in(&store, &dead, stale), ClaimOutcome::Claimed(_)));
        assert!(check_in(&store, &dead.fingerprint, stale).is_none());

        let live = reg("pnpm check:ci", "/tree", "main", std::process::id());
        assert!(matches!(claim_in(&store, &live, stale), ClaimOutcome::Claimed(_)));

        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn age_reaps_an_entry_with_no_pid() {
        let store = tmp_store("aged");
        let held = reg("long suite", "/tree", "main", 0);
        assert!(matches!(
            claim_in(&store, &held, Duration::from_secs(3600)),
            ClaimOutcome::Claimed(_)
        ));
        // Zero-length staleness window: anything already written is past it.
        std::thread::sleep(Duration::from_millis(20));
        assert!(check_in(&store, &held.fingerprint, Duration::from_millis(1)).is_none());

        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn release_is_owner_checked_and_idempotent() {
        let store = tmp_store("release");
        let stale = Duration::from_secs(3600);
        let mine = reg("cargo build --release", "/tree", "main", std::process::id());
        assert!(matches!(claim_in(&store, &mine, stale), ClaimOutcome::Claimed(_)));

        // A different live PID may not release my claim...
        assert!(!release_in(&store, &mine.fingerprint, Some(std::process::id() + 1), false));
        assert!(check_in(&store, &mine.fingerprint, stale).is_some());
        // ...but --force may, and the owner always may.
        assert!(release_in(&store, &mine.fingerprint, Some(std::process::id()), false));
        // Second release is a no-op, not an error.
        assert!(!release_in(&store, &mine.fingerprint, None, true));

        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn unusable_store_degrades_open() {
        // A path whose parent is a FILE can never become a directory.
        let file = std::env::temp_dir().join(format!("loom-inflight-file-{}", std::process::id()));
        std::fs::write(&file, b"x").expect("write temp file");
        let store = file.join("store");
        let mine = reg("cargo test", "/tree", "main", std::process::id());
        assert_eq!(claim_in(&store, &mine, Duration::from_secs(60)), ClaimOutcome::DegradedOpen);
        let _ = std::fs::remove_file(&file);
    }

    /// #8413: containment is component-wise, and a claim on a path *inside* a
    /// worktree still names that worktree.
    #[test]
    fn holders_for_tree_matches_only_real_containment() {
        let store = tmp_store("holders");
        let stale = Duration::from_secs(3600);
        let root = std::env::temp_dir().join(format!("loom-wt-{}", std::process::id()));
        let nested = root.join("issue-8413");
        let twin = root.join("issue-84");
        std::fs::create_dir_all(nested.join("src")).unwrap();
        std::fs::create_dir_all(&twin).unwrap();

        let pid = std::process::id();
        let inside = reg("cargo build", &nested.join("src").to_string_lossy(), "b", pid);
        let sibling = reg("cargo test", &twin.to_string_lossy(), "b", pid);
        assert!(matches!(claim_in(&store, &inside, stale), ClaimOutcome::Claimed(_)));
        assert!(matches!(claim_in(&store, &sibling, stale), ClaimOutcome::Claimed(_)));

        let holders = holders_for_tree_in(&store, &nested, stale);
        assert_eq!(holders.len(), 1, "only the claim inside the worktree counts: {holders:?}");
        assert_eq!(holders[0].command, "cargo build");
        // A path-prefix twin is a different directory, not a holder.
        assert_eq!(holders_for_tree_in(&store, &twin, stale).len(), 1);

        let _ = std::fs::remove_dir_all(&store);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn list_is_oldest_first() {
        let store = tmp_store("order");
        let stale = Duration::from_secs(3600);
        let mut first = reg("a", "/t", "main", std::process::id());
        first.started_at = Utc::now() - chrono::Duration::seconds(600);
        let second = reg("b", "/t", "main", std::process::id());
        assert!(matches!(claim_in(&store, &first, stale), ClaimOutcome::Claimed(_)));
        assert!(matches!(claim_in(&store, &second, stale), ClaimOutcome::Claimed(_)));

        let listed = list_in(&store, stale);
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].command, "a");
        assert!(listed[0].age_secs() >= 600);

        let _ = std::fs::remove_dir_all(&store);
    }
}
