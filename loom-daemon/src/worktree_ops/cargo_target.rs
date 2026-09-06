//! Reclaim a worktree's **redirected** Cargo target directory when the
//! worktree is removed (issue #7239).
//!
//! # The leak
//!
//! Cargo's build output is not always `<workspace>/target`. `CARGO_TARGET_DIR`
//! or `build.target-dir` in any `config.toml` on the lookup path redirects it
//! anywhere — commonly onto a large external volume, one directory per
//! worktree. Every worktree-removal path in Loom (this crate's
//! [`super::clean::cleanup_worktree`], driven by `loom-daemon clean` and by the
//! periodic [`crate::worktree_reaper`]; and `worktree.sh remove` on the bash
//! side) only ever removed the worktree directory itself, so a redirected
//! target dir outlived its worktree forever. One multi-agent host accumulated
//! tens of orphaned directories and hundreds of GB before the volume started
//! pressuring other workloads.
//!
//! # What this module adds
//!
//! Two steps, deliberately split, because they must happen on opposite sides
//! of the removal:
//!
//! 1. [`resolve_for_worktree`] — run **before** the worktree is removed:
//!    `cargo metadata` needs the worktree's manifest, which is gone the instant
//!    `git worktree remove` runs.
//! 2. [`reclaim`] — run **after** the worktree is off disk, so the worktree
//!    being removed can never count as a live referent of its own target dir.
//!
//! # Never-delete gates
//!
//! Deleting a build cache is cheap to redo and catastrophic to get wrong, so
//! [`plan_reclaim`] refuses unless *all* of these hold:
//!
//! - The resolved path is **outside** the worktree (an in-worktree `target/`
//!   already disappears with the worktree — nothing to do).
//! - It is not a path this pass must never touch: the repository itself or any
//!   ancestor of it, the primary checkout's own `target/` (that belongs to
//!   [`crate::deep_clean`], which gates on disk pressure and the machine build
//!   slot), `$HOME`, or a suspiciously shallow path.
//! - **The path is attributable to this worktree.** Only a redirect derived
//!   from the worktree *itself* — `build.target-dir` in a `.cargo/config.toml`
//!   **inside** it, the `host-optimize` shape this module exists for — can be
//!   evidence that a directory belongs to it. Two inputs look like evidence and
//!   are not, because both resolve identically for every path on the host:
//!   the remover's own ambient `CARGO_TARGET_DIR` (read from this process's
//!   environment), and a `build.target-dir` in `$CARGO_HOME/config.toml` or in
//!   an ancestor `.cargo/config.toml` above the worktree. [`redirect_possible_with`]
//!   therefore refuses to resolve *through* an out-of-worktree config at all,
//!   and gate 2f additionally refuses a resolved path that merely equals one of
//!   those machine-global values.
//! - **No other live worktree resolves to it.** The `host-optimize` convention
//!   is a *single shared* `target-dir` for the whole machine; deleting that on
//!   one worktree's removal would destroy a sibling's cache mid-build.
//!   Containment counts as sharing in both directions, and an *unanswerable*
//!   sharing question refuses too — "nobody else uses it" and "we could not
//!   find out" must not look alike. That applies to both of the scan's inputs:
//!   git could not list the worktrees at all, or a sibling has a redirect
//!   configured that `cargo metadata` could not read (a degraded sibling
//!   silently falls back to `<sibling>/target`, which is exactly how a real
//!   sharer stops looking like one).
//! - **No running process is detectably using it** — the same evidence-based
//!   gate [`super::clean::sweep_primary_checkout_artifacts`] applies to the
//!   primary checkout's artifacts (issue #6127). Like that gate, it matches on
//!   process *cwd* and *executable image*, not open file descriptors: an
//!   in-flight `cargo build` whose cwd is the worktree and whose exe is
//!   `~/.cargo/bin/cargo` is invisible to it. The sharing gate above is what
//!   actually covers that case (the building worktree is still live); this one
//!   catches a program *running out of* the target dir.
//!
//! # Relationship to the bash implementation
//!
//! `defaults/scripts/lib/cargo-target-dir.sh` implements the same rules for
//! `worktree.sh remove`. The two are parallel implementations on purpose: the
//! daemon reaps worktrees in repositories where Loom's bash library may not be
//! installed at all, so shelling out to it would silently no-op exactly where
//! the leak was observed. Both mirror the resolution order of the standalone
//! `scripts/cargo-target-dir.sh`, and both have a parity test against it
//! ([`tests::resolution_matches_the_standalone_bash_resolver`] here;
//! `defaults/scripts/tests/test-cargo-target-dir-reclaim.sh` there) so a change
//! to one that is not made to the others fails CI.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Best-effort physical path for comparison purposes. A path that does not
/// exist is returned unchanged rather than dropped — containment checks still
/// need something to compare.
fn realish(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Real (canonicalized) system roots that are exactly as shallow and shared
/// as their literal, un-resolved names suggest, but land one path component
/// *deeper* than the generic component-count heuristic expects because a
/// symlink hop sits between the literal name and its real location — most
/// notably macOS's `/tmp -> /private/tmp` and `/var -> /private/var`
/// (issue #7279). The generic `components().count() < 3` check in
/// [`plan_reclaim`] assumes canonicalization never *adds* depth to a shallow
/// path; this denylist is the explicit backstop for the OSes where it does.
fn is_known_shallow_real_root(real: &Path) -> bool {
    const KNOWN_SHALLOW_REAL_ROOTS: &[&str] = &["/private/tmp", "/private/var", "/private/etc"];
    KNOWN_SHALLOW_REAL_ROOTS
        .iter()
        .any(|root| real == Path::new(root))
}

/// Resolve a possibly-relative Cargo path against a workspace root, without
/// requiring it to exist (the target dir is created by the build itself).
fn absolutize(value: &str, workspace_root: &Path) -> PathBuf {
    let candidate = Path::new(value);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        workspace_root.join(candidate)
    }
}

/// Cargo's resolution order for a workspace's target directory, with both
/// external inputs injected so it is unit-testable without mutating this
/// process's environment (`set_var` is process-global and racy under a
/// multi-threaded test runner):
///
/// 1. `CARGO_TARGET_DIR` when set and non-empty — env beats config in Cargo.
/// 2. `cargo metadata`'s `target_directory`, which applies the full
///    `config.toml` hierarchy (including `build.target-dir`).
/// 3. `<workspace_root>/target` — Cargo's default, i.e. the assumption every
///    caller made before this existed.
///
/// Mirrors `scripts/cargo-target-dir.sh` step for step; see the module docs.
#[must_use]
pub fn resolve_target_dir_with(
    workspace_root: &Path,
    env_override: Option<&str>,
    metadata: &dyn Fn(&Path) -> Option<String>,
) -> PathBuf {
    if let Some(value) = env_override {
        if !value.is_empty() {
            return absolutize(value, workspace_root);
        }
    }
    if let Some(resolved) = metadata(workspace_root) {
        if !resolved.is_empty() && resolved != "null" {
            return absolutize(&resolved, workspace_root);
        }
    }
    workspace_root.join("target")
}

/// `cargo metadata --format-version 1 --no-deps`'s `target_directory`, or
/// `None` when cargo is unavailable / the invocation failed / the field is
/// missing. Parsed with a targeted string extraction (cargo emits compact
/// single-line JSON), matching the `sed` fallback in the bash twin — no serde
/// round-trip of a large document just to read one field.
#[must_use]
pub fn cargo_metadata_target_directory(workspace_root: &Path) -> Option<String> {
    let out = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(workspace_root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let key = "\"target_directory\":\"";
    let start = stdout.find(key)? + key.len();
    let rest = &stdout[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Config files that can attribute a redirect to *this* workspace:
/// `.cargo/config.toml` (and the legacy extension-less `config`) **inside**
/// `workspace_root`, and nothing else.
///
/// Deliberately narrower than Cargo's own lookup path, which also reads every
/// ancestor directory and `$CARGO_HOME`. This function does not answer "what
/// would Cargo do" — it answers "is this directory attributable to this one
/// worktree", and an out-of-worktree config cannot make it so. See
/// [`redirect_possible_with`].
fn worktree_cargo_config_candidates(workspace_root: &Path) -> [PathBuf; 2] {
    [
        workspace_root.join(".cargo").join("config.toml"),
        workspace_root.join(".cargo").join("config"),
    ]
}

/// Extract `build.target-dir`'s value from a `config.toml` body, or `None`.
/// Deliberately minimal (TOML *string* values only, first match wins): a value
/// Cargo would reject is not one this module needs to compare against, and the
/// only consequence of not recognizing one is a reclaim that would have been
/// refused — never a deletion that should not have happened.
fn parse_target_dir_value(body: &str) -> Option<String> {
    body.lines()
        .map(str::trim_start)
        .filter(|line| line.starts_with("target-dir"))
        .find_map(|line| {
            let rhs = line.split_once('=')?.1.trim();
            let quote = rhs.chars().next().filter(|c| *c == '"' || *c == '\'')?;
            let rest = &rhs[quote.len_utf8()..];
            let end = rest.find(quote)?;
            Some(rest[..end].to_string())
        })
        .filter(|v| !v.is_empty())
}

/// Cheap pre-check: is a redirect even conceivable *for this worktree*?
///
/// When it returns `false` this pass treats the target dir as
/// `<workspace_root>/target` and the caller can skip the `cargo metadata`
/// subprocess entirely — which keeps an ordinary (unredirected) host at a few
/// small file reads per worktree removal instead of a cargo invocation per
/// removal.
///
/// # Why the config candidates stop at the worktree boundary
///
/// This is an *attribution* question, not a reimplementation of Cargo's config
/// lookup. A `build.target-dir` in `$CARGO_HOME/config.toml`, or in an ancestor
/// `.cargo/config.toml` above the worktree, is exactly as machine- or
/// session-global as `CARGO_TARGET_DIR`: it resolves identically for every path
/// on the host, so it can never establish that a directory belongs to the one
/// worktree being removed. Consulting those files made a worktree resolve
/// straight to the machine-global cache, which the sharing scan then found no
/// referent for (manifest-less trees are skipped, and the primary checkout's
/// own resolution can fail) — and the shared cache was deleted. The
/// per-worktree redirect this pass exists to reclaim can only come from a
/// config *inside* the worktree, so nothing reclaimable is lost.
#[must_use]
pub fn redirect_possible_with(workspace_root: &Path, env_override: Option<&str>) -> bool {
    // No manifest ⇒ cargo never built here ⇒ nothing to redirect, and in
    // particular an ambient `CARGO_TARGET_DIR` is NOT evidence that this tree
    // owns the directory it names. This test comes FIRST, before the env
    // override, so the worktree being removed is judged by exactly the same
    // rule [`TargetDirProbes::has_manifest`] already applies to every *other*
    // worktree in the sharing scan. When it came second, a manifest-less
    // worktree resolved to the machine-global env path while every sibling was
    // skipped as a referent — so nothing looked shared and the shared cache was
    // deleted (the regression `a_manifestless_worktree_never_resolves_to_the_
    // ambient_env_dir` pins).
    if !workspace_root.join("Cargo.toml").is_file() {
        return false;
    }
    // An ambient `CARGO_TARGET_DIR` still makes a redirect *possible* (Cargo
    // honors it), so resolution must not skip it — but the reclaim step refuses
    // to delete a path that is only ever that env value. See gate 2f.
    if env_override.is_some_and(|v| !v.is_empty()) {
        return true;
    }
    worktree_cargo_config_candidates(workspace_root)
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .any(|body| {
            body.lines()
                .any(|line| line.trim_start().starts_with("target-dir"))
        })
}

fn env_cargo_target_dir() -> Option<String> {
    std::env::var("CARGO_TARGET_DIR")
        .ok()
        .filter(|v| !v.is_empty())
}

fn cargo_home() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("CARGO_HOME") {
        if !explicit.is_empty() {
            return Some(PathBuf::from(explicit));
        }
    }
    std::env::var("HOME")
        .ok()
        .filter(|v| !v.is_empty())
        .map(|home| PathBuf::from(home).join(".cargo"))
}

/// Every `target-dir` value that comes from a machine- or session-global source
/// rather than from the worktree itself, paired with a description of where it
/// came from: the remover's own `CARGO_TARGET_DIR`, and `build.target-dir` in
/// any `.cargo/config.toml` *outside* the worktree (an ancestor directory, or
/// `$CARGO_HOME`).
///
/// Both resolve identically for every path on the host, so neither can ever be
/// evidence that a directory belongs to the one worktree being removed — gate
/// 2f in [`plan_reclaim`] refuses a path that is only one of these. Runs after
/// the worktree is off disk, which is exactly why it reads the config files
/// directly instead of asking cargo: every file it reads lives outside the
/// worktree and is therefore still there.
fn machine_global_target_dirs_with(
    worktree_path: &Path,
    env_override: Option<&str>,
    cargo_home: Option<&Path>,
) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    if let Some(value) = env_override.filter(|v| !v.is_empty()) {
        out.push((absolutize(value, worktree_path), "the ambient CARGO_TARGET_DIR".to_string()));
    }

    // Ancestors ABOVE the worktree only: a config inside the worktree is the
    // one legitimate form of per-worktree attribution and must not appear here.
    let mut files: Vec<PathBuf> = Vec::new();
    let mut dir = worktree_path.parent();
    while let Some(current) = dir {
        files.push(current.join(".cargo").join("config.toml"));
        files.push(current.join(".cargo").join("config"));
        dir = current.parent();
    }
    if let Some(home) = cargo_home {
        files.push(home.join("config.toml"));
        files.push(home.join("config"));
    }

    for file in files {
        let Some(value) = std::fs::read_to_string(&file)
            .ok()
            .as_deref()
            .and_then(parse_target_dir_value)
        else {
            continue;
        };
        // Cargo resolves a relative config path against the directory holding
        // the `.cargo` directory. Guessing wrong costs at most a refusal we
        // would not otherwise have made, never a deletion.
        let base = file
            .parent()
            .and_then(Path::parent)
            .unwrap_or(worktree_path)
            .to_path_buf();
        out.push((absolutize(&value, &base), format!("the target-dir in {}", file.display())));
    }
    out
}

/// The pre-check + resolution composition, reporting whether the answer is
/// **trustworthy**. Every external input is injected so the *production*
/// composition — not a test-local restatement of it — can be exercised without
/// mutating this process's environment.
///
/// `None` means a redirect is configured for this tree but could not be read
/// (`cargo metadata` is missing, exited non-zero — a mid-edit manifest, a
/// conflicted merge — or emitted no `target_directory`). Callers that are about
/// to delete something must fail closed on it: silently degrading to
/// `<root>/target` is how a sibling stops looking like a sharer of the very
/// directory being removed, and "builds somewhere else" must not be
/// indistinguishable from "we could not find out where it builds".
#[must_use]
pub fn resolve_for_worktree_checked_with(
    worktree_path: &Path,
    env_override: Option<&str>,
    metadata: &dyn Fn(&Path) -> Option<String>,
) -> Option<PathBuf> {
    if !redirect_possible_with(worktree_path, env_override) {
        return Some(worktree_path.join("target"));
    }
    // Env beats config in Cargo, and needs no subprocess: a definite answer.
    if let Some(value) = env_override.filter(|v| !v.is_empty()) {
        return Some(absolutize(value, worktree_path));
    }
    metadata(worktree_path)
        .filter(|resolved| !resolved.is_empty() && resolved != "null")
        .map(|resolved| absolutize(&resolved, worktree_path))
}

/// The same composition, degrading a missing answer to `<worktree>/target`.
///
/// Correct for the worktree **being removed** — that fallback is `Inside`, i.e.
/// a silent no-op, so a resolution failure costs a missed reclaim rather than a
/// wrong deletion. The sharing scan, where a degraded answer would instead
/// license a deletion, uses [`resolve_for_worktree_checked_with`].
#[must_use]
pub fn resolve_for_worktree_with(
    worktree_path: &Path,
    env_override: Option<&str>,
    metadata: &dyn Fn(&Path) -> Option<String>,
) -> PathBuf {
    resolve_for_worktree_checked_with(worktree_path, env_override, metadata)
        .unwrap_or_else(|| worktree_path.join("target"))
}

/// Production resolution for one worktree. **Must be called while the worktree
/// still exists on disk** — `cargo metadata` needs its manifest.
#[must_use]
pub fn resolve_for_worktree(worktree_path: &Path) -> PathBuf {
    resolve_for_worktree_with(
        worktree_path,
        env_cargo_target_dir().as_deref(),
        &cargo_metadata_target_directory,
    )
}

/// Production resolution that distinguishes "no redirect" from "a redirect we
/// could not read" — see [`resolve_for_worktree_checked_with`].
#[must_use]
pub fn resolve_for_worktree_checked(worktree_path: &Path) -> Option<PathBuf> {
    resolve_for_worktree_checked_with(
        worktree_path,
        env_cargo_target_dir().as_deref(),
        &cargo_metadata_target_directory,
    )
}

/// What [`plan_reclaim`] decided about one worktree's resolved target dir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetDirOutcome {
    /// Not redirected: it lives inside the worktree and is removed with it.
    Inside(PathBuf),
    /// Nothing on disk at the resolved path (never built, or already gone).
    Absent(PathBuf),
    /// A path this pass must never delete. `reason` says which rule fired.
    Refused { path: PathBuf, reason: String },
    /// Another live worktree resolves to the same directory.
    Shared { path: PathBuf, by: PathBuf },
    /// Live process(es) are using it.
    Protected { path: PathBuf, holders: Vec<String> },
    /// Dry run: this is what a real pass would remove.
    WouldReclaim { path: PathBuf, size_human: String },
    /// Removed.
    Reclaimed { path: PathBuf, size_human: String },
    /// Removal was attempted and failed.
    Failed { path: PathBuf, error: String },
}

impl TargetDirOutcome {
    /// One operator-facing line, or `None` for the two uninteresting outcomes
    /// that describe every unredirected host (`Inside` / `Absent`). Rendered
    /// identically by the interactive `clean` pass and the unattended reaper's
    /// log, so "why did disk not get freed?" has the same answer in both.
    #[must_use]
    pub fn report_line(&self) -> Option<String> {
        match self {
            Self::Inside(_) | Self::Absent(_) => None,
            Self::Refused { path, reason } => {
                Some(format!("Refusing to reclaim cargo target dir {} — {reason}", path.display()))
            }
            Self::Shared { path, by } => Some(format!(
                "Keeping redirected cargo target dir {} — still used by {}",
                path.display(),
                by.display()
            )),
            Self::Protected { path, holders } => Some(format!(
                "Keeping redirected cargo target dir {} — {} live process(es) [{}] still using \
                 it; the reclaim is deferred, not lost",
                path.display(),
                holders.len(),
                holders.join(", ")
            )),
            Self::WouldReclaim { path, size_human } => Some(format!(
                "Would reclaim redirected cargo target dir: {} ({size_human})",
                path.display()
            )),
            Self::Reclaimed { path, size_human } => Some(format!(
                "Reclaimed redirected cargo target dir: {} ({size_human})",
                path.display()
            )),
            Self::Failed { path, error } => Some(format!(
                "Could not reclaim redirected cargo target dir {} — {error}",
                path.display()
            )),
        }
    }
}

/// Every external input [`plan_reclaim`] consults, injected so the whole
/// decision — including the destructive step — is unit-testable without a real
/// process table, a real `git worktree list`, or a real `rm -rf`.
pub struct TargetDirProbes<'a> {
    /// Worktrees (and the primary checkout) git currently knows about, or
    /// `None` when that could not be determined at all (git missing, not a
    /// repo, I/O error). `None` is **not** the same as "no other worktrees":
    /// it is the one input whose absence makes the sharing gate unanswerable,
    /// so [`plan_reclaim`] refuses rather than deleting on an empty answer.
    pub live_worktrees: &'a dyn Fn() -> Option<Vec<PathBuf>>,
    /// True when a path is a workspace cargo actually builds in. A tree with
    /// no manifest cannot depend on a target dir, and counting it would make
    /// an ambient absolute `CARGO_TARGET_DIR` — which resolves identically for
    /// every path — report every redirected dir as shared, reclaiming nothing.
    pub has_manifest: &'a dyn Fn(&Path) -> bool,
    /// One live worktree's resolved target dir, or `None` when that worktree
    /// has a redirect configured that could not be read. `None` is **not** the
    /// same as `<root>/target`: it is an unanswered question, and gate 4 fails
    /// closed on it exactly as it does on an unanswerable `live_worktrees`.
    pub resolve: &'a dyn Fn(&Path) -> Option<PathBuf>,
    /// Every `target-dir` value reaching this removal from a machine- or
    /// session-global source — the remover's own `CARGO_TARGET_DIR`, and
    /// `build.target-dir` in any `.cargo/config.toml` outside the worktree —
    /// paired with a description of where it came from. All of them resolve
    /// identically for every path on the host, so none can ever be evidence
    /// that a directory belongs to *one* worktree, which is the only thing that
    /// would justify deleting it — see gate 2f in [`plan_reclaim`].
    pub machine_global_target_dirs: &'a dyn Fn(&Path) -> Vec<(PathBuf, String)>,
    /// Descriptions of live processes using a directory (empty ⇒ nobody).
    pub holders: &'a dyn Fn(&Path) -> Vec<String>,
    /// Human-readable directory size, for the report.
    pub size_human: &'a dyn Fn(&Path) -> String,
    /// Recursive removal.
    pub remove: &'a dyn Fn(&Path) -> std::io::Result<()>,
}

/// Decide (and, unless `dry_run`, act on) one worktree's resolved target dir.
///
/// `worktree_path` is the worktree that was removed; it is excluded from the
/// sharing scan by path, so this is correct whether it is still on disk (dry
/// run) or already gone (the real pass).
#[must_use]
pub fn plan_reclaim(
    repo_root: &Path,
    worktree_path: &Path,
    resolved: &Path,
    dry_run: bool,
    probes: &TargetDirProbes,
) -> TargetDirOutcome {
    let resolved_real = realish(resolved);
    let worktree_real = realish(worktree_path);
    let repo_real = realish(repo_root);

    // 1. The default, in-worktree location: it goes away with the worktree.
    if resolved_real == worktree_real || resolved_real.starts_with(&worktree_real) {
        return TargetDirOutcome::Inside(resolved.to_path_buf());
    }

    // 2. Paths that must never be deleted by this pass, however they resolved.
    let refuse = |reason: &str| TargetDirOutcome::Refused {
        path: resolved.to_path_buf(),
        reason: reason.to_string(),
    };
    if resolved_real.components().count() < 3
        || resolved.components().count() < 3
        || is_known_shallow_real_root(&resolved_real)
    {
        // `/`, `/tmp`, `C:\` — one component for the root prefix plus at most
        // one name. Nothing Loom provisions is ever that shallow.
        //
        // The component count is checked on *both* the canonicalized
        // (`resolved_real`) and literal (`resolved`) forms because
        // canonicalization can grow OR hide shallowness depending on which
        // direction a symlink hop runs:
        //   - A shallow literal input (e.g. `/tmp` passed directly) is caught
        //     by the `resolved` check even when `realish` fails to resolve it
        //     (e.g. it does not exist) and returns it unchanged.
        //   - A literal path many components deep whose *final* segment is a
        //     symlink into a shallow real location (e.g. some `redirect ->
        //     /tmp`) canonicalizes down to something shallow; that is caught
        //     by the `resolved_real` check.
        //   - Neither count check catches macOS, where `/tmp` and `/var` are
        //     themselves symlinks to `/private/tmp` and `/private/var` — one
        //     *extra* real component from the symlink hop lands exactly on 3,
        //     not under it. `is_known_shallow_real_root` is the explicit
        //     denylist backstop for that one-hop-deeper case (issue #7279).
        return refuse("suspiciously shallow path");
    }
    if repo_real.starts_with(&resolved_real) {
        return refuse("contains the repository itself");
    }
    if resolved_real == repo_real.join("target") {
        // Regenerable, but it belongs to the pressure-gated deep-clean pass,
        // never to a single worktree's removal.
        return refuse("the primary checkout's own target/");
    }
    if std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .is_some_and(|home| realish(Path::new(&home)) == resolved_real)
    {
        return refuse("resolves to $HOME");
    }

    // 2f. The resolved path is nothing but a MACHINE-GLOBAL redirect value —
    //     the remover's own `CARGO_TARGET_DIR` (read from THIS process's
    //     environment), or a `build.target-dir` declared by a `.cargo/config.toml`
    //     outside the worktree. Neither comes from anything belonging to the
    //     worktree, and both resolve identically for every path on the host, so
    //     neither can establish that this directory is exclusive to the worktree
    //     we are removing — while the sharing scan below deliberately skips
    //     manifest-less trees, leaving a shared cache with no visible referent at
    //     all. Refuse instead, and say which source it was.
    //
    //     This costs the feature nothing: the per-worktree redirect this pass
    //     exists to reclaim comes from `build.target-dir` in a `.cargo/config.toml`
    //     INSIDE the worktree (the host-optimize shape), whose value is
    //     per-worktree by construction. A genuinely per-worktree
    //     `CARGO_TARGET_DIR` exported into the remover's environment merely gets
    //     reported instead of deleted — the safe direction.
    //
    //     Backstop, not the primary defense: `redirect_possible_with` already
    //     declines to resolve THROUGH an out-of-worktree config. This gate
    //     additionally catches a worktree-local config that names the very
    //     directory a machine-global one names.
    for (path, source) in (probes.machine_global_target_dirs)(worktree_path) {
        if realish(&path) == resolved_real {
            return refuse(&format!("{source} is machine-global, not exclusive to this worktree"));
        }
    }

    // 3. Nothing there.
    if !resolved_real.is_dir() {
        return TargetDirOutcome::Absent(resolved.to_path_buf());
    }

    // 4. Shared with a still-live worktree. Containment counts in BOTH
    //    directions: a sibling building into a parent of this path, or into a
    //    subtree of it, is equally destroyed by an `rm -rf` here.
    //
    //    Fail CLOSED when the worktree list is unavailable: an empty answer
    //    and "git could not tell us" are indistinguishable downstream, and
    //    treating the latter as "nobody else uses it" is precisely how a
    //    sibling's cache gets deleted mid-build. Mirrors the same rule
    //    `registered_worktree_paths`' own doc states for orphan removal.
    let Some(others) = (probes.live_worktrees)() else {
        return refuse("could not enumerate live worktrees (git worktree list failed)");
    };
    for other in others {
        let other_real = realish(&other);
        if other_real == worktree_real {
            continue;
        }
        if !(probes.has_manifest)(&other_real) {
            continue;
        }
        // FAIL CLOSED on a degraded answer, for the same reason as the
        // enumeration above. A sibling whose configured redirect could not be
        // read falls back to `<sibling>/target`, which is indistinguishable
        // from a sibling that genuinely builds elsewhere — so one transient
        // `cargo metadata` failure (a mid-edit Cargo.toml, a conflicted merge)
        // would stop it counting as a sharer of the directory about to be
        // deleted.
        let Some(other_target) = (probes.resolve)(&other_real) else {
            return refuse(
                "could not resolve a live worktree's target dir (cargo metadata failed)",
            );
        };
        let other_target = realish(&other_target);
        if other_target == resolved_real
            || resolved_real.starts_with(&other_target)
            || other_target.starts_with(&resolved_real)
        {
            return TargetDirOutcome::Shared {
                path: resolved.to_path_buf(),
                by: other,
            };
        }
    }

    // 5. A running process is using it. Checked under `dry_run` too: a preview
    //    that claims it "would remove" a live build's output is a preview an
    //    operator would act on.
    let holders = (probes.holders)(&resolved_real);
    if !holders.is_empty() {
        return TargetDirOutcome::Protected {
            path: resolved.to_path_buf(),
            holders,
        };
    }

    let size_human = (probes.size_human)(&resolved_real);
    if dry_run {
        return TargetDirOutcome::WouldReclaim {
            path: resolved.to_path_buf(),
            size_human,
        };
    }
    match (probes.remove)(&resolved_real) {
        Ok(()) => TargetDirOutcome::Reclaimed {
            path: resolved.to_path_buf(),
            size_human,
        },
        Err(e) => TargetDirOutcome::Failed {
            path: resolved.to_path_buf(),
            error: e.to_string(),
        },
    }
}

/// Production wiring for [`plan_reclaim`]: real `git worktree list`, real
/// resolution, the real process table, real `du`-equivalent sizing, and a real
/// `remove_dir_all`.
#[must_use]
pub fn reclaim(
    repo_root: &Path,
    worktree_path: &Path,
    resolved: &Path,
    dry_run: bool,
) -> TargetDirOutcome {
    let live_worktrees = || {
        super::clean::registered_worktree_paths(repo_root)
            .map(|set| set.into_iter().collect::<Vec<_>>())
    };
    let has_manifest = |p: &Path| p.join("Cargo.toml").is_file();
    let resolve = |p: &Path| resolve_for_worktree_checked(p);
    let machine_global_target_dirs = |p: &Path| {
        machine_global_target_dirs_with(
            p,
            env_cargo_target_dir().as_deref(),
            cargo_home().as_deref(),
        )
    };
    let holders = |p: &Path| {
        let mut out: Vec<String> = super::safety::find_processes_executing_within(p)
            .into_iter()
            .map(|exe| exe.to_string())
            .collect();
        for pid in super::safety::find_processes_using_directory(p) {
            let line = format!("pid {pid}");
            if !out.iter().any(|held| held.starts_with(&line)) {
                out.push(line);
            }
        }
        out
    };
    let size_human = |p: &Path| super::clean::dir_size_human(p);
    let remove = |p: &Path| std::fs::remove_dir_all(p);
    let probes = TargetDirProbes {
        live_worktrees: &live_worktrees,
        has_manifest: &has_manifest,
        resolve: &resolve,
        machine_global_target_dirs: &machine_global_target_dirs,
        holders: &holders,
        size_human: &size_human,
        remove: &remove,
    };
    plan_reclaim(repo_root, worktree_path, resolved, dry_run, &probes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A repo root + one worktree + an external target dir, all real paths so
    /// the `is_dir` / canonicalize gates behave exactly as in production.
    struct Fixture {
        _tmp: tempfile::TempDir,
        repo_root: PathBuf,
        worktree: PathBuf,
        external: PathBuf,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let repo_root = base.join("repo");
        let worktree = repo_root.join(".loom/worktrees/issue-7239");
        let external = base.join("cargo-target/issue-7239");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::create_dir_all(external.join("debug")).unwrap();
        std::fs::write(external.join("debug/artifact.bin"), vec![0u8; 2048]).unwrap();
        Fixture {
            _tmp: tmp,
            repo_root,
            worktree,
            external,
        }
    }

    /// A worktree that really does build with cargo (the production
    /// `has_manifest` probe, verbatim).
    fn make_cargo_worktree(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(path.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
    }

    fn has_manifest(p: &Path) -> bool {
        p.join("Cargo.toml").is_file()
    }

    /// The default for every case except the machine-global regressions below:
    /// no ambient `CARGO_TARGET_DIR`, no out-of-worktree config redirect — so
    /// the resolved path came from something belonging to the worktree.
    fn no_machine_global_dirs(_: &Path) -> Vec<(PathBuf, String)> {
        Vec::new()
    }

    /// The remover's environment carries `value` as `CARGO_TARGET_DIR`, exactly
    /// as the production probe would report it.
    fn ambient_env_only(value: &str) -> impl Fn(&Path) -> Vec<(PathBuf, String)> + '_ {
        move |worktree: &Path| machine_global_target_dirs_with(worktree, Some(value), None)
    }

    fn size_stub(_: &Path) -> String {
        "12.3G".to_string()
    }

    #[test]
    fn reclaims_an_external_target_dir_no_other_worktree_references() {
        let f = fixture();
        // The primary checkout has no manifest here, so it is correctly not a
        // referent — the shape the daemon sees on a repo it merely hosts.
        let live = vec![f.repo_root.clone(), f.worktree.clone()];
        let live_worktrees = || Some(live.clone());
        let resolve = |_: &Path| Some(PathBuf::from("/nowhere/else"));
        let holders = |_: &Path| Vec::new();
        let removed: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
        let remove = |p: &Path| {
            removed.borrow_mut().push(p.to_path_buf());
            std::fs::remove_dir_all(p)
        };
        let probes = TargetDirProbes {
            live_worktrees: &live_worktrees,
            has_manifest: &has_manifest,
            resolve: &resolve,
            machine_global_target_dirs: &no_machine_global_dirs,
            holders: &holders,
            size_human: &size_stub,
            remove: &remove,
        };

        let outcome = plan_reclaim(&f.repo_root, &f.worktree, &f.external, false, &probes);

        assert_eq!(
            outcome,
            TargetDirOutcome::Reclaimed {
                path: f.external.clone(),
                size_human: "12.3G".to_string()
            }
        );
        assert!(!f.external.exists(), "the external target dir was not removed");
        assert!(outcome.report_line().unwrap().contains("Reclaimed"));
    }

    #[test]
    fn never_removes_a_target_dir_a_live_worktree_still_resolves_to() {
        let f = fixture();
        let sibling = f.repo_root.join(".loom/worktrees/issue-999");
        make_cargo_worktree(&sibling);
        let live = vec![f.worktree.clone(), sibling.clone()];
        let live_worktrees = || Some(live.clone());
        // The host-optimize shape: every worktree resolves to ONE shared dir.
        let shared = f.external.clone();
        let resolve = move |_: &Path| Some(shared.clone());
        let holders = |_: &Path| Vec::new();
        let removed: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
        let remove = |p: &Path| {
            removed.borrow_mut().push(p.to_path_buf());
            std::fs::remove_dir_all(p)
        };
        let probes = TargetDirProbes {
            live_worktrees: &live_worktrees,
            has_manifest: &has_manifest,
            resolve: &resolve,
            machine_global_target_dirs: &no_machine_global_dirs,
            holders: &holders,
            size_human: &size_stub,
            remove: &remove,
        };

        let outcome = plan_reclaim(&f.repo_root, &f.worktree, &f.external, false, &probes);

        assert_eq!(
            outcome,
            TargetDirOutcome::Shared {
                path: f.external.clone(),
                by: sibling
            }
        );
        assert!(
            f.external.join("debug/artifact.bin").is_file(),
            "a shared target dir and its contents must survive intact"
        );
        assert!(removed.borrow().is_empty(), "nothing may be removed");
        assert!(outcome.report_line().unwrap().contains("still used by"));
    }

    #[test]
    fn a_sibling_building_into_a_parent_of_this_dir_also_counts_as_shared() {
        let f = fixture();
        let sibling = f.repo_root.join(".loom/worktrees/issue-999");
        make_cargo_worktree(&sibling);
        let live = vec![f.worktree.clone(), sibling];
        let live_worktrees = || Some(live.clone());
        let parent = f.external.parent().unwrap().to_path_buf();
        let resolve = move |_: &Path| Some(parent.clone());
        let holders = |_: &Path| Vec::new();
        let remove = |p: &Path| std::fs::remove_dir_all(p);
        let probes = TargetDirProbes {
            live_worktrees: &live_worktrees,
            has_manifest: &has_manifest,
            resolve: &resolve,
            machine_global_target_dirs: &no_machine_global_dirs,
            holders: &holders,
            size_human: &size_stub,
            remove: &remove,
        };

        assert!(matches!(
            plan_reclaim(&f.repo_root, &f.worktree, &f.external, false, &probes),
            TargetDirOutcome::Shared { .. }
        ));
        assert!(f.external.exists());
    }

    #[test]
    fn a_manifestless_live_worktree_is_not_treated_as_a_referent() {
        // Regression guard for the failure mode that would make this feature
        // reclaim nothing, ever: an ambient absolute CARGO_TARGET_DIR resolves
        // identically for EVERY path, so a tree that never builds with cargo
        // must not count as sharing the dir.
        let f = fixture();
        let bystander = f.repo_root.join(".loom/worktrees/issue-888");
        std::fs::create_dir_all(&bystander).unwrap(); // no Cargo.toml
        let live = vec![f.worktree.clone(), bystander];
        let live_worktrees = || Some(live.clone());
        let shared = f.external.clone();
        let resolve = move |_: &Path| Some(shared.clone());
        let holders = |_: &Path| Vec::new();
        let remove = |p: &Path| std::fs::remove_dir_all(p);
        let probes = TargetDirProbes {
            live_worktrees: &live_worktrees,
            has_manifest: &has_manifest,
            resolve: &resolve,
            machine_global_target_dirs: &no_machine_global_dirs,
            holders: &holders,
            size_human: &size_stub,
            remove: &remove,
        };

        assert!(matches!(
            plan_reclaim(&f.repo_root, &f.worktree, &f.external, false, &probes),
            TargetDirOutcome::Reclaimed { .. }
        ));
        assert!(!f.external.exists());
    }

    /// Regression, half 1 of 2: the shape that deleted a machine-global cargo
    /// cache. A worktree with **no manifest** (every non-Rust consumer repo)
    /// on a host that exports `CARGO_TARGET_DIR` for a single shared build
    /// cache used to resolve straight to that shared path — while the sharing
    /// scan skipped every manifest-less sibling, so nothing looked shared and
    /// the whole cache was `rm -rf`'d. The manifest test now runs BEFORE the
    /// env override, so such a tree resolves to its own `<worktree>/target`.
    #[test]
    fn a_manifestless_worktree_never_resolves_to_the_ambient_env_dir() {
        let f = fixture();
        // No Cargo.toml anywhere: not in the worktree, not in the repo root.
        let ambient = f.external.to_string_lossy().to_string();

        let resolved = resolve_for_worktree_with(&f.worktree, Some(&ambient), &|_| {
            panic!("cargo metadata must not even be consulted")
        });
        assert_eq!(
            resolved,
            f.worktree.join("target"),
            "a manifest-less tree must resolve to its own in-worktree target/, \
             never to the machine-global CARGO_TARGET_DIR"
        );

        let live = vec![f.repo_root.clone(), f.worktree.clone()];
        let live_worktrees = || Some(live.clone());
        let resolve = |_: &Path| Some(PathBuf::from("/nowhere/else"));
        let ambient_probe = ambient_env_only(&ambient);
        let holders = |_: &Path| Vec::new();
        let removed: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
        let remove = |p: &Path| {
            removed.borrow_mut().push(p.to_path_buf());
            std::fs::remove_dir_all(p)
        };
        let probes = TargetDirProbes {
            live_worktrees: &live_worktrees,
            has_manifest: &has_manifest,
            resolve: &resolve,
            machine_global_target_dirs: &ambient_probe,
            holders: &holders,
            size_human: &size_stub,
            remove: &remove,
        };

        let outcome = plan_reclaim(&f.repo_root, &f.worktree, &resolved, false, &probes);
        assert_eq!(outcome, TargetDirOutcome::Inside(resolved));
        assert!(
            f.external.join("debug/artifact.bin").is_file(),
            "the shared cargo cache must survive untouched"
        );
        assert!(removed.borrow().is_empty(), "nothing may be removed");
    }

    /// Regression, half 2 of 2: reordering the manifest check alone is NOT
    /// enough. A worktree that *does* carry a manifest, inside a repo whose
    /// root and siblings do not, passes the reordered pre-check and resolves to
    /// the ambient `CARGO_TARGET_DIR` — and the sharing scan still sees no
    /// referent, because it skips manifest-less trees. The reclaim step itself
    /// therefore refuses any path that is merely the remover's own ambient env
    /// value: it is machine-global by construction and can never be evidence
    /// that the directory belongs to this one worktree.
    #[test]
    fn refuses_a_path_that_is_only_the_removers_ambient_cargo_target_dir() {
        let f = fixture();
        make_cargo_worktree(&f.worktree); // this tree really does build
        let bystander = f.repo_root.join(".loom/worktrees/issue-888");
        std::fs::create_dir_all(&bystander).unwrap(); // manifest-less, as is the repo root
        let ambient = f.external.to_string_lossy().to_string();

        // With a manifest, the env override IS honored by resolution...
        let resolved = resolve_for_worktree_with(&f.worktree, Some(&ambient), &|_| None);
        assert_eq!(resolved, f.external, "env beats config, exactly as in Cargo");

        // ...and the sharing scan finds nobody, exactly as in the reproduction.
        let live = vec![f.repo_root.clone(), f.worktree.clone(), bystander];
        let live_worktrees = || Some(live.clone());
        let resolve = |_: &Path| Some(PathBuf::from("/nowhere/else"));
        let ambient_probe = ambient_env_only(&ambient);
        let holders = |_: &Path| Vec::new();
        let removed: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
        let remove = |p: &Path| {
            removed.borrow_mut().push(p.to_path_buf());
            std::fs::remove_dir_all(p)
        };
        let probes = TargetDirProbes {
            live_worktrees: &live_worktrees,
            has_manifest: &has_manifest,
            resolve: &resolve,
            machine_global_target_dirs: &ambient_probe,
            holders: &holders,
            size_human: &size_stub,
            remove: &remove,
        };

        for dry_run in [false, true] {
            match plan_reclaim(&f.repo_root, &f.worktree, &resolved, dry_run, &probes) {
                TargetDirOutcome::Refused { reason, .. } => {
                    assert!(reason.contains("machine-global"), "reason: {reason}");
                }
                other => panic!("expected a refusal, got {other:?}"),
            }
        }
        assert!(
            f.external.join("debug/artifact.bin").is_file(),
            "the shared cargo cache must survive untouched"
        );
        assert!(removed.borrow().is_empty(), "nothing may be removed");
    }

    /// Blocker D: a `build.target-dir` in `$CARGO_HOME/config.toml` — or in any
    /// ancestor `.cargo/config.toml` above the worktree — is exactly as
    /// machine-global as `CARGO_TARGET_DIR`. It used to be accepted as this
    /// worktree's own redirect, so the worktree resolved straight to the shared
    /// machine cache, the sharing scan found no manifest-bearing referent, and
    /// the cache was deleted. The pre-check now consults only configs INSIDE
    /// the worktree, so such a tree resolves to `<worktree>/target`.
    #[test]
    fn a_cargo_home_redirect_is_not_this_worktrees_own_target_dir() {
        let f = fixture();
        make_cargo_worktree(&f.worktree); // this tree really does build
        let cargo_home = f.repo_root.parent().unwrap().join("cargo-home");
        std::fs::create_dir_all(&cargo_home).unwrap();
        std::fs::write(
            cargo_home.join("config.toml"),
            format!("[build]\ntarget-dir = \"{}\"\n", f.external.display()),
        )
        .unwrap();
        // The same shape one level down, as a real filesystem input the
        // pre-check could still read if it walked ancestors: a config ABOVE the
        // worktree redirects every tree in the repo to the same place.
        std::fs::create_dir_all(f.repo_root.join(".cargo")).unwrap();
        std::fs::write(
            f.repo_root.join(".cargo/config.toml"),
            format!("[build]\ntarget-dir = \"{}\"\n", f.external.display()),
        )
        .unwrap();

        // Real cargo WOULD honor both and answer with the shared cache; the
        // pre-check must decline to ask it at all.
        let metadata = |_: &Path| Some(f.external.to_string_lossy().to_string());
        let resolved = resolve_for_worktree_with(&f.worktree, None, &metadata);
        assert_eq!(
            resolved,
            f.worktree.join("target"),
            "a $CARGO_HOME or ancestor redirect must not be read as this worktree's own"
        );

        // ...and even had it resolved there, gate 2f refuses the same path.
        let live = vec![f.repo_root.clone(), f.worktree.clone()];
        let live_worktrees = || Some(live.clone());
        let resolve = |_: &Path| Some(PathBuf::from("/nowhere/else"));
        let machine_global =
            |wt: &Path| machine_global_target_dirs_with(wt, None, Some(&cargo_home));
        assert_eq!(
            machine_global(&f.worktree).len(),
            2,
            "both out-of-worktree sources are recognized as machine-global"
        );
        let holders = |_: &Path| Vec::new();
        let removed: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
        let remove = |p: &Path| {
            removed.borrow_mut().push(p.to_path_buf());
            std::fs::remove_dir_all(p)
        };
        let probes = TargetDirProbes {
            live_worktrees: &live_worktrees,
            has_manifest: &has_manifest,
            resolve: &resolve,
            machine_global_target_dirs: &machine_global,
            holders: &holders,
            size_human: &size_stub,
            remove: &remove,
        };

        assert_eq!(
            plan_reclaim(&f.repo_root, &f.worktree, &resolved, false, &probes),
            TargetDirOutcome::Inside(resolved)
        );
        match plan_reclaim(&f.repo_root, &f.worktree, &f.external, false, &probes) {
            TargetDirOutcome::Refused { reason, .. } => {
                assert!(reason.contains("machine-global"), "reason: {reason}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(
            f.external.join("debug/artifact.bin").is_file(),
            "the machine-global cargo cache must survive untouched"
        );
        assert!(removed.borrow().is_empty(), "nothing may be removed");
    }

    /// Blocker E: the sharing gate's *other* input can fail open too. A sibling
    /// with a configured redirect that `cargo metadata` cannot read (a mid-edit
    /// `Cargo.toml`, a conflicted merge) used to degrade silently to
    /// `<sibling>/target` — so the one worktree that really was building into
    /// this directory stopped counting as a sharer and the shared cache was
    /// deleted. Same rule as an unanswerable `git worktree list`: refuse.
    #[test]
    fn refuses_when_a_live_worktrees_target_dir_cannot_be_resolved() {
        let f = fixture();
        let sibling = f.repo_root.join(".loom/worktrees/issue-999");
        make_cargo_worktree(&sibling);
        let live = vec![f.worktree.clone(), sibling.clone()];
        let live_worktrees = || Some(live.clone());
        // The sibling has a redirect configured, but cargo could not answer.
        let resolve = |_: &Path| None;
        let holders = |_: &Path| Vec::new();
        let removed: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
        let remove = |p: &Path| {
            removed.borrow_mut().push(p.to_path_buf());
            std::fs::remove_dir_all(p)
        };
        let probes = TargetDirProbes {
            live_worktrees: &live_worktrees,
            has_manifest: &has_manifest,
            resolve: &resolve,
            machine_global_target_dirs: &no_machine_global_dirs,
            holders: &holders,
            size_human: &size_stub,
            remove: &remove,
        };

        for dry_run in [false, true] {
            match plan_reclaim(&f.repo_root, &f.worktree, &f.external, dry_run, &probes) {
                TargetDirOutcome::Refused { reason, .. } => {
                    assert!(reason.contains("cargo metadata failed"), "reason: {reason}");
                }
                other => panic!("expected a refusal, got {other:?}"),
            }
        }
        assert!(f.external.join("debug/artifact.bin").is_file());
        assert!(removed.borrow().is_empty(), "nothing may be removed");
    }

    /// The `None` gate 4 fails closed on is produced by the production
    /// resolution itself, not only by a hand-written probe: a worktree whose
    /// own `.cargo/config.toml` redirects, whose `cargo metadata` then fails,
    /// is "unknown" — while a worktree with no redirect at all is a definite
    /// `<worktree>/target`.
    #[test]
    fn a_configured_redirect_that_cargo_cannot_read_is_unknown_not_default() {
        let f = fixture();
        make_cargo_worktree(&f.worktree);
        let failing = |_: &Path| None;

        assert_eq!(
            resolve_for_worktree_checked_with(&f.worktree, None, &failing),
            Some(f.worktree.join("target")),
            "no redirect configured ⇒ a definite answer"
        );

        std::fs::create_dir_all(f.worktree.join(".cargo")).unwrap();
        std::fs::write(
            f.worktree.join(".cargo/config.toml"),
            format!("[build]\ntarget-dir = \"{}\"\n", f.external.display()),
        )
        .unwrap();
        assert_eq!(
            resolve_for_worktree_checked_with(&f.worktree, None, &failing),
            None,
            "a configured redirect cargo could not read is not `<worktree>/target`"
        );
        assert_eq!(
            resolve_for_worktree_with(&f.worktree, None, &failing),
            f.worktree.join("target"),
            "the unchecked form still degrades — correct for the tree being removed"
        );
    }

    /// The refusal is scoped to the ambient path *itself*, not to "an ambient
    /// `CARGO_TARGET_DIR` exists": a directory that is genuinely this
    /// worktree's own is still reclaimed while some unrelated env value points
    /// elsewhere. Pins the gate against being "simplified" into a blanket
    /// `if ambient.is_some() { refuse }`, which would quietly turn the whole
    /// feature off on every host that exports one.
    #[test]
    fn a_config_derived_redirect_is_still_reclaimed_under_an_unrelated_ambient_env() {
        let f = fixture();
        let live = vec![f.worktree.clone()];
        let live_worktrees = || Some(live.clone());
        let resolve = |_: &Path| Some(PathBuf::from("/nowhere/else"));
        let ambient_probe = ambient_env_only("/some/other/shared/cache");
        let holders = |_: &Path| Vec::new();
        let removed: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
        let remove = |p: &Path| {
            removed.borrow_mut().push(p.to_path_buf());
            std::fs::remove_dir_all(p)
        };
        let probes = TargetDirProbes {
            live_worktrees: &live_worktrees,
            has_manifest: &has_manifest,
            resolve: &resolve,
            machine_global_target_dirs: &ambient_probe,
            holders: &holders,
            size_human: &size_stub,
            remove: &remove,
        };

        assert!(matches!(
            plan_reclaim(&f.repo_root, &f.worktree, &f.external, false, &probes),
            TargetDirOutcome::Reclaimed { .. }
        ));
        assert!(!f.external.exists());
    }

    #[test]
    fn refuses_when_the_live_worktree_list_is_unavailable() {
        // Fail CLOSED: `git worktree list` failing is not evidence that
        // nothing else uses this directory. Treating an unanswerable sharing
        // question as "unshared" is exactly how a sibling's cache would get
        // deleted mid-build on a host with a broken/absent git.
        let f = fixture();
        let live_worktrees = || None;
        let resolve = |_: &Path| Some(PathBuf::from("/nowhere/else"));
        let holders = |_: &Path| Vec::new();
        let removed: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
        let remove = |p: &Path| {
            removed.borrow_mut().push(p.to_path_buf());
            std::fs::remove_dir_all(p)
        };
        let probes = TargetDirProbes {
            live_worktrees: &live_worktrees,
            has_manifest: &has_manifest,
            resolve: &resolve,
            machine_global_target_dirs: &no_machine_global_dirs,
            holders: &holders,
            size_human: &size_stub,
            remove: &remove,
        };

        for dry_run in [false, true] {
            match plan_reclaim(&f.repo_root, &f.worktree, &f.external, dry_run, &probes) {
                TargetDirOutcome::Refused { reason, .. } => {
                    assert!(reason.contains("enumerate"), "reason: {reason}");
                }
                other => panic!("expected a refusal, got {other:?}"),
            }
        }
        assert!(f.external.join("debug/artifact.bin").is_file());
        assert!(removed.borrow().is_empty());
    }

    #[test]
    fn never_removes_a_target_dir_a_live_process_is_using() {
        let f = fixture();
        let live = vec![f.worktree.clone()];
        let live_worktrees = || Some(live.clone());
        let resolve = |_: &Path| Some(PathBuf::from("/nowhere/else"));
        let holders = |_: &Path| vec!["pid 4242 → /vol/cargo-target/debug/safehoused".to_string()];
        let removed: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
        let remove = |p: &Path| {
            removed.borrow_mut().push(p.to_path_buf());
            std::fs::remove_dir_all(p)
        };
        let probes = TargetDirProbes {
            live_worktrees: &live_worktrees,
            has_manifest: &has_manifest,
            resolve: &resolve,
            machine_global_target_dirs: &no_machine_global_dirs,
            holders: &holders,
            size_human: &size_stub,
            remove: &remove,
        };

        let outcome = plan_reclaim(&f.repo_root, &f.worktree, &f.external, false, &probes);

        assert!(matches!(outcome, TargetDirOutcome::Protected { .. }));
        assert!(f.external.exists(), "a dir backing a live process survives");
        assert!(removed.borrow().is_empty());
        assert!(outcome.report_line().unwrap().contains("4242"));
    }

    #[test]
    fn the_live_process_gate_applies_under_dry_run_too() {
        // A preview that claims it "would remove" a live build's output is a
        // preview an operator would act on.
        let f = fixture();
        let live = vec![f.worktree.clone()];
        let live_worktrees = || Some(live.clone());
        let resolve = |_: &Path| Some(PathBuf::from("/nowhere/else"));
        let holders = |_: &Path| vec!["pid 7 → /vol/cargo-target/debug/thing".to_string()];
        let remove = |p: &Path| std::fs::remove_dir_all(p);
        let probes = TargetDirProbes {
            live_worktrees: &live_worktrees,
            has_manifest: &has_manifest,
            resolve: &resolve,
            machine_global_target_dirs: &no_machine_global_dirs,
            holders: &holders,
            size_human: &size_stub,
            remove: &remove,
        };

        assert!(matches!(
            plan_reclaim(&f.repo_root, &f.worktree, &f.external, true, &probes),
            TargetDirOutcome::Protected { .. }
        ));
    }

    #[test]
    fn dry_run_reports_a_size_and_removes_nothing() {
        let f = fixture();
        let live = vec![f.worktree.clone()];
        let live_worktrees = || Some(live.clone());
        let resolve = |_: &Path| Some(PathBuf::from("/nowhere/else"));
        let holders = |_: &Path| Vec::new();
        let removed: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
        let remove = |p: &Path| {
            removed.borrow_mut().push(p.to_path_buf());
            std::fs::remove_dir_all(p)
        };
        let probes = TargetDirProbes {
            live_worktrees: &live_worktrees,
            has_manifest: &has_manifest,
            resolve: &resolve,
            machine_global_target_dirs: &no_machine_global_dirs,
            holders: &holders,
            size_human: &size_stub,
            remove: &remove,
        };

        let outcome = plan_reclaim(&f.repo_root, &f.worktree, &f.external, true, &probes);

        assert_eq!(
            outcome,
            TargetDirOutcome::WouldReclaim {
                path: f.external.clone(),
                size_human: "12.3G".to_string()
            }
        );
        assert!(f.external.exists(), "a dry run must not delete");
        assert!(removed.borrow().is_empty());
        let line = outcome.report_line().unwrap();
        assert!(line.contains("Would reclaim") && line.contains("12.3G"));
    }

    #[test]
    fn an_in_worktree_target_dir_is_a_silent_no_op() {
        let f = fixture();
        let inside = f.worktree.join("target");
        std::fs::create_dir_all(&inside).unwrap();
        let live = vec![f.worktree.clone()];
        let live_worktrees = || Some(live.clone());
        let resolve = |_: &Path| Some(PathBuf::from("/nowhere/else"));
        let holders = |_: &Path| Vec::new();
        let removed: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
        let remove = |p: &Path| {
            removed.borrow_mut().push(p.to_path_buf());
            std::fs::remove_dir_all(p)
        };
        let probes = TargetDirProbes {
            live_worktrees: &live_worktrees,
            has_manifest: &has_manifest,
            resolve: &resolve,
            machine_global_target_dirs: &no_machine_global_dirs,
            holders: &holders,
            size_human: &size_stub,
            remove: &remove,
        };

        let outcome = plan_reclaim(&f.repo_root, &f.worktree, &inside, false, &probes);

        assert_eq!(outcome, TargetDirOutcome::Inside(inside.clone()));
        assert!(outcome.report_line().is_none(), "must not be chatty");
        assert!(removed.borrow().is_empty());
    }

    #[test]
    fn refuses_the_primary_checkouts_own_target_and_any_ancestor_of_the_repo() {
        let f = fixture();
        let live = vec![f.worktree.clone()];
        let live_worktrees = || Some(live.clone());
        let resolve = |_: &Path| Some(PathBuf::from("/nowhere/else"));
        let holders = |_: &Path| Vec::new();
        let removed: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
        let remove = |p: &Path| {
            removed.borrow_mut().push(p.to_path_buf());
            std::fs::remove_dir_all(p)
        };
        let probes = TargetDirProbes {
            live_worktrees: &live_worktrees,
            has_manifest: &has_manifest,
            resolve: &resolve,
            machine_global_target_dirs: &no_machine_global_dirs,
            holders: &holders,
            size_human: &size_stub,
            remove: &remove,
        };

        let primary_target = f.repo_root.join("target");
        std::fs::create_dir_all(&primary_target).unwrap();
        match plan_reclaim(&f.repo_root, &f.worktree, &primary_target, false, &probes) {
            TargetDirOutcome::Refused { reason, .. } => {
                assert!(reason.contains("primary checkout"), "reason: {reason}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(primary_target.exists());

        let ancestor = f.repo_root.parent().unwrap().to_path_buf();
        match plan_reclaim(&f.repo_root, &f.worktree, &ancestor, false, &probes) {
            TargetDirOutcome::Refused { reason, .. } => {
                assert!(reason.contains("repository itself"), "reason: {reason}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(f.repo_root.exists(), "the repository must still be there");

        match plan_reclaim(&f.repo_root, &f.worktree, Path::new("/tmp"), false, &probes) {
            TargetDirOutcome::Refused { reason, .. } => {
                assert!(reason.contains("shallow"), "reason: {reason}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }

        assert!(removed.borrow().is_empty(), "no refusal may delete anything");
    }

    /// Issue #7279 regression: a literal input that is many components deep
    /// (nothing about it looks shallow) but whose *final* segment is a
    /// symlink into a shallow real system directory must still be refused as
    /// shallow. This is the general shape of the bug the component-count-only
    /// guard missed — `/tmp` itself only reproduces it on an OS where `/tmp`
    /// is a symlink (macOS); this test reproduces it on any OS by
    /// constructing the symlink explicitly.
    #[test]
    fn refuses_a_deep_literal_path_whose_final_symlink_hop_resolves_to_a_shallow_real_root() {
        let f = fixture();
        let live = vec![f.worktree.clone()];
        let live_worktrees = || Some(live.clone());
        let resolve = |_: &Path| Some(PathBuf::from("/nowhere/else"));
        let holders = |_: &Path| Vec::new();
        let removed: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
        let remove = |p: &Path| {
            removed.borrow_mut().push(p.to_path_buf());
            std::fs::remove_dir_all(p)
        };
        let probes = TargetDirProbes {
            live_worktrees: &live_worktrees,
            has_manifest: &has_manifest,
            resolve: &resolve,
            machine_global_target_dirs: &no_machine_global_dirs,
            holders: &holders,
            size_human: &size_stub,
            remove: &remove,
        };

        // A deep, unremarkable-looking literal path whose last component is a
        // symlink to `/tmp` — canonicalizing it lands on `/tmp` (Linux, where
        // `/tmp` is not itself a symlink) or `/private/tmp` (macOS, where it
        // is): both must be caught, the first by the pre-existing
        // `resolved_real` component-count check, the second by the
        // `is_known_shallow_real_root` denylist added for this issue.
        let deep_redirect = f
            .repo_root
            .join("some/deeply/nested/looking/cargo-target-redirect");
        std::fs::create_dir_all(deep_redirect.parent().unwrap()).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/tmp", &deep_redirect).unwrap();
        #[cfg(not(unix))]
        panic!("this test requires symlink support");

        match plan_reclaim(&f.repo_root, &f.worktree, &deep_redirect, false, &probes) {
            TargetDirOutcome::Refused { reason, .. } => {
                assert!(reason.contains("shallow"), "reason: {reason}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(removed.borrow().is_empty(), "no refusal may delete anything");
    }

    #[test]
    fn known_shallow_real_roots_denylist_covers_the_macos_symlinked_system_dirs() {
        assert!(is_known_shallow_real_root(Path::new("/private/tmp")));
        assert!(is_known_shallow_real_root(Path::new("/private/var")));
        assert!(is_known_shallow_real_root(Path::new("/private/etc")));
        assert!(!is_known_shallow_real_root(Path::new("/private/tmp/extra")));
        assert!(!is_known_shallow_real_root(Path::new("/Users/someone/workspace")));
    }

    #[test]
    fn absent_target_dir_is_a_silent_no_op() {
        let f = fixture();
        let live = vec![f.worktree.clone()];
        let live_worktrees = || Some(live.clone());
        let resolve = |_: &Path| Some(PathBuf::from("/nowhere/else"));
        let holders = |_: &Path| Vec::new();
        let remove = |p: &Path| std::fs::remove_dir_all(p);
        let probes = TargetDirProbes {
            live_worktrees: &live_worktrees,
            has_manifest: &has_manifest,
            resolve: &resolve,
            machine_global_target_dirs: &no_machine_global_dirs,
            holders: &holders,
            size_human: &size_stub,
            remove: &remove,
        };

        let never_built = f.external.parent().unwrap().join("issue-does-not-exist");
        let outcome = plan_reclaim(&f.repo_root, &f.worktree, &never_built, false, &probes);
        assert_eq!(outcome, TargetDirOutcome::Absent(never_built));
        assert!(outcome.report_line().is_none());
    }

    #[test]
    fn resolution_precedence_env_then_metadata_then_default() {
        let root = Path::new("/w/root");
        let no_metadata = |_: &Path| None;
        let metadata = |_: &Path| Some("/from/metadata".to_string());

        assert_eq!(
            resolve_target_dir_with(root, Some("/abs/target"), &no_metadata),
            PathBuf::from("/abs/target"),
            "env wins outright"
        );
        assert_eq!(
            resolve_target_dir_with(root, Some("rel-target"), &no_metadata),
            PathBuf::from("/w/root/rel-target"),
            "a relative env value resolves against the workspace root"
        );
        assert_eq!(
            resolve_target_dir_with(root, Some("/abs/target"), &metadata),
            PathBuf::from("/abs/target"),
            "env beats config, exactly as in Cargo"
        );
        assert_eq!(
            resolve_target_dir_with(root, None, &metadata),
            PathBuf::from("/from/metadata"),
            "config.toml's build.target-dir, via cargo metadata"
        );
        assert_eq!(
            resolve_target_dir_with(root, Some(""), &no_metadata),
            PathBuf::from("/w/root/target"),
            "an empty env value is not a redirect"
        );
        assert_eq!(
            resolve_target_dir_with(root, None, &no_metadata),
            PathBuf::from("/w/root/target"),
            "Cargo's default is the last resort"
        );
    }

    /// Anti-drift inside this module: [`resolve_for_worktree_checked_with`]
    /// restates the resolution order so it can report "unknown", so whenever it
    /// *has* an answer that answer must be exactly what the parity-tested
    /// [`resolve_target_dir_with`] produces. Without this, a change to one
    /// order would silently change what gets deleted.
    #[test]
    fn the_checked_resolution_agrees_with_the_parity_tested_order() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        std::fs::create_dir_all(root.join(".cargo")).unwrap();
        std::fs::write(
            root.join(".cargo/config.toml"),
            "[build]\ntarget-dir = \"/volume/cargo-target\"\n",
        )
        .unwrap();

        type Metadata<'a> = &'a dyn Fn(&Path) -> Option<String>;
        let metadatas: [Metadata; 3] = [
            &|_: &Path| Some("/from/metadata".to_string()),
            &|_: &Path| Some("rel-from-metadata".to_string()),
            &|_: &Path| None,
        ];
        for metadata in metadatas {
            for env in [None, Some("/abs/env"), Some("rel-env"), Some("")] {
                if let Some(checked) = resolve_for_worktree_checked_with(&root, env, metadata) {
                    assert_eq!(
                        checked,
                        resolve_target_dir_with(&root, env, metadata),
                        "checked resolution drifted for env={env:?}"
                    );
                }
                assert_eq!(
                    resolve_for_worktree_with(&root, env, metadata),
                    resolve_target_dir_with(&root, env, metadata),
                    "unchecked resolution drifted for env={env:?}"
                );
            }
        }
    }

    #[test]
    fn redirect_precheck_skips_the_cargo_invocation_when_nothing_can_redirect() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let root = base.join("nest/workspace");
        std::fs::create_dir_all(&root).unwrap();

        // No manifest at all — and NOT even with an env override. The manifest
        // test comes first on purpose (see `redirect_possible_with`): an
        // ambient CARGO_TARGET_DIR must not make a tree that never built with
        // cargo resolve to the machine-global cache.
        assert!(!redirect_possible_with(&root, None));
        assert!(!redirect_possible_with(&root, Some("/elsewhere")));

        // A manifest, but no config anywhere mentions target-dir.
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        assert!(!redirect_possible_with(&root, None));

        // WITH a manifest, the env override alone is enough.
        assert!(redirect_possible_with(&root, Some("/elsewhere")));
        assert!(!redirect_possible_with(&root, Some("")));

        // A workspace-local .cargo/config.toml that redirects — the ONE form of
        // per-worktree attribution this module accepts.
        std::fs::create_dir_all(root.join(".cargo")).unwrap();
        std::fs::write(
            root.join(".cargo/config.toml"),
            "[build]\ntarget-dir = \"/volume/cargo-target\"\n",
        )
        .unwrap();
        assert!(redirect_possible_with(&root, None));
        std::fs::remove_file(root.join(".cargo/config.toml")).unwrap();
        assert!(!redirect_possible_with(&root, None));

        // ...but NOT one in an ancestor directory, and not one in $CARGO_HOME.
        // Both are machine-global: they resolve identically for every path on
        // the host, so they can never attribute a directory to this worktree,
        // and honoring them here is what deleted a shared cache (blocker D).
        // The consequence is a resolution of `<root>/target` ⇒ `Inside` ⇒ a
        // silent no-op, which is the safe direction.
        std::fs::create_dir_all(base.join("nest/.cargo")).unwrap();
        std::fs::write(
            base.join("nest/.cargo/config.toml"),
            "[build]\ntarget-dir = \"/volume/cargo-target\"\n",
        )
        .unwrap();
        assert!(
            !redirect_possible_with(&root, None),
            "an ancestor .cargo/config.toml is not this worktree's redirect"
        );
    }

    /// `$CARGO_HOME` and ancestor configs are excluded from *attribution* — but
    /// they are still recognized as machine-global sources, so gate 2f can
    /// refuse a worktree-local redirect that names the very same directory.
    #[test]
    fn machine_global_sources_are_the_env_var_and_every_config_outside_the_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let worktree = base.join("repo/.loom/worktrees/issue-1");
        let cargo_home = base.join("cargo-home");
        std::fs::create_dir_all(worktree.join(".cargo")).unwrap();
        std::fs::create_dir_all(&cargo_home).unwrap();

        // A config INSIDE the worktree is attribution, so it must NOT appear.
        std::fs::write(
            worktree.join(".cargo/config.toml"),
            "[build]\ntarget-dir = \"/volume/mine\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(base.join("repo/.cargo")).unwrap();
        std::fs::write(
            base.join("repo/.cargo/config.toml"),
            "[build]\ntarget-dir = \"/volume/repo-wide\"\n",
        )
        .unwrap();
        std::fs::write(
            cargo_home.join("config.toml"),
            "[build]\ntarget-dir = '/volume/machine-wide'\n",
        )
        .unwrap();

        let found =
            machine_global_target_dirs_with(&worktree, Some("/volume/from-env"), Some(&cargo_home));
        let paths: Vec<&Path> = found.iter().map(|(p, _)| p.as_path()).collect();
        assert!(paths.contains(&Path::new("/volume/from-env")));
        assert!(paths.contains(&Path::new("/volume/repo-wide")));
        assert!(paths.contains(&Path::new("/volume/machine-wide")));
        assert!(
            !paths.contains(&Path::new("/volume/mine")),
            "the worktree's OWN redirect is attribution, not a machine-global value"
        );
        assert!(found[0].1.contains("ambient CARGO_TARGET_DIR"));

        // A relative value resolves against the directory holding `.cargo`.
        std::fs::write(
            base.join("repo/.cargo/config.toml"),
            "[build]\ntarget-dir = \"shared-target\"\n",
        )
        .unwrap();
        let found = machine_global_target_dirs_with(&worktree, None, None);
        assert!(found
            .iter()
            .any(|(p, _)| p == &base.join("repo/shared-target")));
    }

    /// Anti-drift: this module and `scripts/cargo-target-dir.sh` must resolve
    /// identically, because `worktree.sh remove` and the daemon reaper both
    /// decide what to DELETE from their answers. Skipped when the standalone
    /// script is absent (a consumer-repo checkout of the crate).
    #[test]
    fn resolution_matches_the_standalone_bash_resolver() {
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .map(|p| p.join("scripts/cargo-target-dir.sh"));
        let Some(script) = script.filter(|p| p.is_file()) else {
            eprintln!("skipping: scripts/cargo-target-dir.sh not present");
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        let ask_script = |env_value: Option<&str>| -> String {
            let mut cmd = Command::new("bash");
            cmd.arg(&script).arg(&root);
            match env_value {
                Some(v) => {
                    cmd.env("CARGO_TARGET_DIR", v);
                }
                None => {
                    cmd.env_remove("CARGO_TARGET_DIR");
                }
            }
            let out = cmd.output().expect("the bash resolver ran");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };

        for env_value in [Some("/abs/target-7239"), Some("rel-target-7239")] {
            let ours = resolve_target_dir_with(&root, env_value, &|_| None);
            assert_eq!(
                ours.to_string_lossy(),
                ask_script(env_value),
                "resolver drift for CARGO_TARGET_DIR={env_value:?}"
            );
        }
        // No manifest under `root`, so both fall through to `<root>/target`
        // without a network call or a real build.
        let ours = resolve_target_dir_with(&root, None, &cargo_metadata_target_directory);
        assert_eq!(
            ours.to_string_lossy(),
            ask_script(None),
            "resolver drift for the unredirected default"
        );
    }
}
