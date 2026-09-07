//! Host-wide **Docker image retention** pass (issue #7332) — the Docker
//! counterpart of [`crate::deep_clean`] for `target/`/`node_modules/`.
//!
//! # What was leaking
//!
//! The session-container CI/smoke/audit flows (`worker-image-smoke` /
//! `session-image-smoke` in `.github/workflows/ci.yml`, plus a fleet-side
//! `audit-smoke`/`audit-test` flow outside this repo) build and tag images
//! under a **fixed** tag (`loom-worker:ci-smoke`,
//! `ghcr.io/rjwalters/loom-worker:ci-smoke`, `loom-worker-session:ci-smoke`,
//! …) on every run. Docker re-points that tag at the new image on each build
//! and leaves the *previous* image dangling (untagged, unreferenced by any
//! name) — it never disappears on its own. A host that runs these flows daily
//! measured **26.9GB across 25 images with only 2 active** before this pass
//! existed: none of that is visible to workspace-level disk accounting
//! (`du` under the unprivileged fleet user), because it lives in root-owned
//! `/var/lib/docker` — only `docker system df` reveals it.
//!
//! # Shape: mirrors `deep_clean`, not a from-scratch mechanism
//!
//! Like [`crate::deep_clean`], this is a pure-function [`plan_retention`] core
//! (fully unit-testable against a fixture, no real `docker` binary required)
//! wrapped by an I/O shell (`list_images` / `remove_image`, both injectable)
//! and invoked from the same [`crate::worktree_reaper`] tick, right after
//! [`crate::deep_clean::run_for`] — the "existing scheduled-clean hook" this
//! issue asks to extend. Unlike `deep_clean`, this pass is **not**
//! disk-pressure-gated: images accumulate independently of `target/`
//! regrowth, and dangling-image removal is safe to run on every tick (it is
//! the same guarantee `docker image prune` gives — an image that is
//! unreferenced by any tag or container can never be "in use"). A
//! `minIntervalSecs` cooldown still exists, purely to bound how often the
//! (cheap but non-zero) `docker image inspect` shell-out runs on a host with
//! several registered repos ticking on the same schedule.
//!
//! # Two-part retention policy
//!
//! 1. **Dangling images are always removable.** No tag points at them, so by
//!    definition nothing is currently using that name — this is
//!    [`RetentionPlan::remove_dangling`], the practical fix for *this* repo's
//!    fixed-tag build sites (Option 3 in the issue, the floor of the fix).
//! 2. **Tracked repositories keep only the newest N tagged images.** For any
//!    repository name in the configured `trackedRepos` allowlist (default:
//!    the `loom-worker` / `loom-worker-session` family and their `ghcr.io`
//!    aliases), only the `keepLastN` most recently created images survive;
//!    older ones are removed by image ID (not by tag) so **every** alias tag
//!    pointing at that ID goes with it in one `docker rmi` call — this is
//!    [`RetentionPlan::remove_stale_tracked`] (Option 2), a generalization
//!    that also bounds a future build site that tags per-run (`ci-smoke-123`)
//!    rather than reusing one fixed tag.
//!
//! **Long-lived base images are never touched.** A configurable `allowlist`
//! of repository-name substrings (e.g. `"eda"`) is checked *before* either
//! rule above — an allowlisted image is skipped outright, whether or not it
//! is dangling or in a tracked repository. Everything not dangling, not
//! tracked, and not allowlisted is left alone by construction: this pass only
//! ever acts on images it explicitly recognizes.
//!
//! # Safety
//!
//! Mirrors `deep_clean`'s gates: the removal half holds the machine-wide
//! [`crate::build_slot`] for its duration (so an in-progress `docker build`
//! — which itself briefly holds dangling intermediate layers — is never
//! targeted mid-build) and defers to the next tick if the slot cannot be
//! taken. `docker rmi` additionally refuses (a soft per-image failure, not
//! fatal to the pass) to remove an image backing a running container, so a
//! currently-executing smoke/audit container is never pulled out from under
//! itself even if this pass's own bookkeeping were somehow wrong.
//!
//! # Default-on
//!
//! Like `deep_clean`, this is default-on: an unbounded per-run image leak is
//! not a behavior anyone opts into. Opt out with `LOOM_DOCKER_IMAGE_RETENTION=0`
//! or `autonomous.dockerImageRetention.enabled=false`.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Deserialize;

// ============================================================================
// Constants
// ============================================================================

/// Master on/off env override. Default-on: `0`/`false`/`no`/`off` disables,
/// `1`/`true`/`yes`/`on` force-enables even when config disables it.
pub const DOCKER_RETENTION_ENABLE_ENV: &str = "LOOM_DOCKER_IMAGE_RETENTION";

/// Env override for how many of the newest tagged images per tracked
/// repository survive a pass.
pub const DOCKER_RETENTION_KEEP_N_ENV: &str = "LOOM_DOCKER_IMAGE_RETENTION_KEEP_N";

/// Env override for the cooldown between passes (seconds).
pub const DOCKER_RETENTION_MIN_INTERVAL_ENV: &str = "LOOM_DOCKER_IMAGE_RETENTION_MIN_INTERVAL_SECS";

/// Default number of newest tagged images kept per tracked repository.
pub const DEFAULT_KEEP_LAST_N: usize = 2;

/// Default cooldown between passes: 30 minutes. Much shorter than
/// `deep_clean`'s 6h — this pass is not disk-pressure-gated and cheap, the
/// cooldown exists only to avoid re-shelling to `docker` on every repo in a
/// multi-repo host's same reaper tick.
pub const DEFAULT_MIN_INTERVAL_SECS: u64 = 1_800;

/// The repositories this pass manages by default — the exact build sites
/// `.github/workflows/ci.yml`'s `worker-image-smoke` / `session-image-smoke`
/// jobs tag, plus their `ghcr.io` aliases (verified against `origin/main`,
/// 2026-09-07, issue #7332).
pub const DEFAULT_TRACKED_REPOS: &[&str] = &[
    "loom-worker",
    "loom-worker-session",
    "ghcr.io/rjwalters/loom-worker",
    "ghcr.io/rjwalters/loom-worker-session",
];

/// How long the pass waits for the machine-wide build slot before deferring —
/// mirrors [`crate::deep_clean::DEEP_CLEAN_SLOT_WAIT_SECS`].
pub const DOCKER_RETENTION_SLOT_WAIT_SECS: u64 = 5;

const DOCKER_RETENTION_SLOT_POLL: Duration = Duration::from_millis(500);

/// docker CLI binary name — overridable only for tests (production always
/// uses the system `docker`).
const DOCKER_BIN: &str = "docker";

// ============================================================================
// Config (.loom/config.json → autonomous.dockerImageRetention)
// ============================================================================

/// The subset of `.loom/config.json → autonomous.dockerImageRetention` this
/// module consumes. Every field is `Option`, matching every other
/// `autonomous.*` surface's env > config > default precedence.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DockerRetentionConfig {
    /// `…dockerImageRetention.enabled` (default **true**).
    pub enabled: Option<bool>,
    /// `…dockerImageRetention.keepLastN` (default [`DEFAULT_KEEP_LAST_N`]).
    pub keep_last_n: Option<usize>,
    /// `…dockerImageRetention.minIntervalSecs` (default
    /// [`DEFAULT_MIN_INTERVAL_SECS`]; a zero/invalid value drops to `None`).
    pub min_interval_secs: Option<u64>,
    /// `…dockerImageRetention.trackedRepos` — repository names this pass
    /// enforces `keepLastN` over (default [`DEFAULT_TRACKED_REPOS`]).
    pub tracked_repos: Option<Vec<String>>,
    /// `…dockerImageRetention.allowlist` — repo:tag substrings that are never
    /// touched, whether or not they are dangling or tracked (default empty —
    /// a host with a shared long-lived image, e.g. an EDA toolchain image,
    /// must opt it in explicitly).
    pub allowlist: Option<Vec<String>>,
}

/// Read `.loom/config.json → autonomous.dockerImageRetention`, soft-failing
/// every field to `None` (env/default resolution) on a missing file,
/// malformed JSON, or a missing block.
#[must_use]
pub fn read_docker_retention_config(repo_root: &Path) -> DockerRetentionConfig {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let Some(block) =
        crate::config_resolver::get_path(&effective, "autonomous.dockerImageRetention")
    else {
        return DockerRetentionConfig::default();
    };

    let str_vec = |key: &str| -> Option<Vec<String>> {
        block
            .get(key)
            .and_then(serde_json::Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
    };

    DockerRetentionConfig {
        enabled: block.get("enabled").and_then(serde_json::Value::as_bool),
        keep_last_n: block
            .get("keepLastN")
            .and_then(serde_json::Value::as_u64)
            .map(|n| n as usize),
        min_interval_secs: block
            .get("minIntervalSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
        tracked_repos: str_vec("trackedRepos"),
        allowlist: str_vec("allowlist"),
    }
}

/// Resolve whether the pass runs — precedence **env > config > default(true)**.
#[must_use]
pub fn resolve_enabled(config: &DockerRetentionConfig) -> bool {
    if let Ok(v) = std::env::var(DOCKER_RETENTION_ENABLE_ENV) {
        return matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
    }
    config.enabled.unwrap_or(true)
}

/// Resolve how many newest tagged images survive per tracked repository —
/// precedence **env > config > default**.
#[must_use]
pub fn resolve_keep_last_n(config: &DockerRetentionConfig) -> usize {
    std::env::var(DOCKER_RETENTION_KEEP_N_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .or(config.keep_last_n)
        .unwrap_or(DEFAULT_KEEP_LAST_N)
}

/// Resolve the cooldown — precedence **env > config > default**.
#[must_use]
pub fn resolve_min_interval_secs(config: &DockerRetentionConfig) -> u64 {
    std::env::var(DOCKER_RETENTION_MIN_INTERVAL_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or(config.min_interval_secs)
        .unwrap_or(DEFAULT_MIN_INTERVAL_SECS)
}

/// Resolve the tracked-repository list — precedence **config > default**
/// (no env override: this is a set, not a scalar, and the default already
/// covers this repo's own build sites).
#[must_use]
pub fn resolve_tracked_repos(config: &DockerRetentionConfig) -> Vec<String> {
    config.tracked_repos.clone().unwrap_or_else(|| {
        DEFAULT_TRACKED_REPOS
            .iter()
            .map(|s| (*s).to_string())
            .collect()
    })
}

/// Resolve the allowlist — precedence **config > default(empty)**.
#[must_use]
pub fn resolve_allowlist(config: &DockerRetentionConfig) -> Vec<String> {
    config.allowlist.clone().unwrap_or_default()
}

// ============================================================================
// Image model
// ============================================================================

/// One Docker image, as one unit — every tag alias pointing at the same image
/// ID (e.g. a local `loom-worker:ci-smoke` and its `ghcr.io/...` mirror of
/// the identical digest) is collapsed into one [`DockerImageRecord`], because
/// `docker image inspect` already groups `RepoTags` by ID for us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerImageRecord {
    /// Full `sha256:...` image ID.
    pub id: String,
    /// Every `repository:tag` alias pointing at this ID. Empty (or
    /// `["<none>:<none>"]`, the pre-`--no-trunc`-JSON shape) for a dangling
    /// image.
    pub repo_tags: Vec<String>,
    /// When this image was built.
    pub created_at: DateTime<Utc>,
    /// On-disk size in bytes, for reporting only.
    pub size_bytes: u64,
}

impl DockerImageRecord {
    /// True when no tag currently points at this image.
    #[must_use]
    pub fn is_dangling(&self) -> bool {
        self.repo_tags.is_empty() || self.repo_tags.iter().all(|t| t == "<none>:<none>")
    }

    /// Human-readable size, matching [`crate::worktree_ops::clean`]'s report
    /// style (`"34.1G"`, `"512.0M"`, …).
    #[must_use]
    pub fn size_human(&self) -> String {
        human_size(self.size_bytes)
    }
}

fn human_size(bytes: u64) -> String {
    let b = bytes as f64;
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1}G", b / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1}M", b / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1}K", b / 1024.0)
    } else {
        format!("{bytes}B")
    }
}

/// The repository half of a `"repo:tag"` string (everything before the last
/// `:`) — tolerant of a `ghcr.io/owner/name:tag` repo containing colons only
/// in a port-qualified registry host, which none of this repo's tracked
/// repos do, so `rsplit_once` is sufficient.
#[must_use]
pub fn repo_of(repo_tag: &str) -> &str {
    repo_tag
        .rsplit_once(':')
        .map_or(repo_tag, |(repo, _tag)| repo)
}

// ============================================================================
// Retention plan (pure)
// ============================================================================

/// The disposition of one `docker image ls` snapshot — every input image ends
/// up in exactly one bucket.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetentionPlan {
    /// Untagged images with no name pointing at them — always removable.
    pub remove_dangling: Vec<DockerImageRecord>,
    /// Tagged images in a tracked repository, past the newest `keepLastN`.
    pub remove_stale_tracked: Vec<DockerImageRecord>,
    /// Everything left alone: the newest `keepLastN` per tracked repository,
    /// plus every non-dangling image outside every tracked repository.
    pub kept: Vec<DockerImageRecord>,
    /// Explicitly exempted by the allowlist — never evaluated against either
    /// removal rule.
    pub allowlisted: Vec<DockerImageRecord>,
}

impl RetentionPlan {
    /// Every image this plan would remove, dangling first.
    #[must_use]
    pub fn to_remove(&self) -> Vec<&DockerImageRecord> {
        self.remove_dangling
            .iter()
            .chain(self.remove_stale_tracked.iter())
            .collect()
    }

    /// `"3 dangling (7.6G), 2 stale tracked (2.4G)"`, or `"nothing"`.
    #[must_use]
    pub fn summary(&self) -> String {
        if self.remove_dangling.is_empty() && self.remove_stale_tracked.is_empty() {
            return "nothing".to_string();
        }
        let mut parts = Vec::new();
        if !self.remove_dangling.is_empty() {
            let bytes: u64 = self.remove_dangling.iter().map(|i| i.size_bytes).sum();
            parts.push(format!("{} dangling ({})", self.remove_dangling.len(), human_size(bytes)));
        }
        if !self.remove_stale_tracked.is_empty() {
            let bytes: u64 = self.remove_stale_tracked.iter().map(|i| i.size_bytes).sum();
            parts.push(format!(
                "{} stale tracked ({})",
                self.remove_stale_tracked.len(),
                human_size(bytes)
            ));
        }
        parts.join(", ")
    }
}

/// Whether `record` matches an allowlist entry — a substring match against
/// every `repo:tag` alias, so `"eda"` exempts `my-registry.example/eda:latest`
/// without the caller having to spell out the exact tag.
#[must_use]
pub fn matches_allowlist(record: &DockerImageRecord, allowlist: &[String]) -> bool {
    if allowlist.is_empty() {
        return false;
    }
    record.repo_tags.iter().any(|rt| {
        allowlist
            .iter()
            .any(|pat| !pat.is_empty() && rt.contains(pat.as_str()))
    })
}

/// Decide what to remove, purely from the current image snapshot and policy —
/// no `docker` shell-out, fully unit-testable against a fixture.
///
/// # Ordering
///
/// The allowlist is checked **first**, ahead of both the dangling check and
/// tracked-repository membership — a long-lived base image that happens to
/// share a repository name is exempt outright, and the (structurally
/// impossible in practice, since an allowlisted image is tagged) case of an
/// allowlisted *and* dangling image still favors the allowlist.
#[must_use]
pub fn plan_retention(
    images: &[DockerImageRecord],
    tracked_repos: &[String],
    allowlist: &[String],
    keep_last_n: usize,
) -> RetentionPlan {
    let mut plan = RetentionPlan::default();
    let mut tracked: BTreeMap<String, Vec<DockerImageRecord>> = BTreeMap::new();

    for image in images {
        if matches_allowlist(image, allowlist) {
            plan.allowlisted.push(image.clone());
            continue;
        }
        if image.is_dangling() {
            plan.remove_dangling.push(image.clone());
            continue;
        }
        let tracked_repo = image
            .repo_tags
            .iter()
            .map(|rt| repo_of(rt))
            .find(|repo| tracked_repos.iter().any(|tr| tr == repo));
        match tracked_repo {
            Some(repo) => tracked
                .entry(repo.to_string())
                .or_default()
                .push(image.clone()),
            None => plan.kept.push(image.clone()),
        }
    }

    for (_repo, mut group) in tracked {
        // Newest first; ties (identical `created_at`, e.g. a rebuild inside
        // the same second) break on ID so the ordering — and therefore which
        // half of the tie survives — is deterministic rather than
        // sort-stability-dependent on insertion order.
        group.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        for (i, image) in group.into_iter().enumerate() {
            if i < keep_last_n {
                plan.kept.push(image);
            } else {
                plan.remove_stale_tracked.push(image);
            }
        }
    }

    plan
}

// ============================================================================
// I/O: listing and removal (injected, so the pass is testable without a real
// `docker` binary)
// ============================================================================

#[derive(Debug, Deserialize)]
struct InspectImage {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "RepoTags")]
    repo_tags: Option<Vec<String>>,
    #[serde(rename = "Created")]
    created: String,
    #[serde(rename = "Size")]
    size: Option<u64>,
}

/// List every image on the host, one [`DockerImageRecord`] per unique image
/// ID (multi-tag aliases already collapsed by `docker image inspect` into a
/// single `RepoTags` array). `None` on any failure to run or parse `docker`
/// output — treated identically to "docker unavailable" by [`run_pass`],
/// never as "zero images" (the same unknown-!=-zero discipline
/// [`crate::deep_clean::evaluate`] applies to an unmeasurable `df`).
#[must_use]
pub fn list_images() -> Option<Vec<DockerImageRecord>> {
    let ids_output = Command::new(DOCKER_BIN)
        .args(["image", "ls", "-aq", "--no-trunc"])
        .output()
        .ok()?;
    if !ids_output.status.success() {
        return None;
    }
    let mut ids: Vec<String> = String::from_utf8_lossy(&ids_output.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    ids.sort();
    ids.dedup();
    if ids.is_empty() {
        return Some(Vec::new());
    }

    let mut cmd = Command::new(DOCKER_BIN);
    cmd.arg("image").arg("inspect");
    cmd.args(&ids);
    let inspect_output = cmd.output().ok()?;
    if !inspect_output.status.success() {
        return None;
    }
    let parsed: Vec<InspectImage> = serde_json::from_slice(&inspect_output.stdout).ok()?;
    Some(
        parsed
            .into_iter()
            .filter_map(|img| {
                let created_at = DateTime::parse_from_rfc3339(&img.created)
                    .ok()?
                    .with_timezone(&Utc);
                Some(DockerImageRecord {
                    id: img.id,
                    repo_tags: img.repo_tags.unwrap_or_default(),
                    created_at,
                    size_bytes: img.size.unwrap_or(0),
                })
            })
            .collect(),
    )
}

/// Remove one image **by ID**, taking every alias tag pointing at it with it
/// in the same call — the "multi-tag aliasing handled as one unit"
/// requirement. A failure (most commonly: the image backs a running
/// container) is logged and treated as a soft per-image failure, not fatal to
/// the rest of the pass.
pub fn remove_image(id: &str) -> bool {
    match Command::new(DOCKER_BIN).args(["rmi", id]).output() {
        Ok(out) if out.status.success() => true,
        Ok(out) => {
            log::warn!(
                "docker_image_clean: could not remove image {id}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
            false
        }
        Err(e) => {
            log::warn!("docker_image_clean: could not invoke `docker rmi {id}`: {e}");
            false
        }
    }
}

// ============================================================================
// The pass
// ============================================================================

/// One retention pass's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerRetentionReport {
    /// Whether the pass was enabled for this evaluation.
    pub enabled: bool,
    /// `None` when disabled, in cooldown, or `docker` was unqueryable.
    pub plan: Option<RetentionPlan>,
    /// Images [`plan`]'s removal set that were actually removed (a subset of
    /// `plan.to_remove()` — `docker rmi` failures are excluded).
    pub removed: Vec<DockerImageRecord>,
    /// Present when a safety gate deferred removal (build slot unavailable).
    pub deferred: Option<String>,
    /// When this evaluation ran.
    pub at: DateTime<Utc>,
}

impl DockerRetentionReport {
    /// One-line rationale for logging, mirroring
    /// [`crate::deep_clean::DeepCleanTrigger::reason`]'s style.
    #[must_use]
    pub fn reason(&self) -> String {
        if !self.enabled {
            return "disabled (autonomous.dockerImageRetention.enabled=false or \
                     LOOM_DOCKER_IMAGE_RETENTION unset-falsy)"
                .to_string();
        }
        if let Some(reason) = &self.deferred {
            return format!("deferred: {reason}");
        }
        match &self.plan {
            None => "docker was unqueryable — skipping (unknown != empty)".to_string(),
            Some(plan) => format!(
                "removed {} of planned {} ({} kept, {} allowlisted)",
                self.removed.len(),
                plan.to_remove().len(),
                plan.kept.len(),
                plan.allowlisted.len()
            ),
        }
    }
}

/// Everything a fully-injected [`run_pass`] needs.
pub struct DockerRetentionInputs<'a> {
    pub enabled: bool,
    pub tracked_repos: &'a [String],
    pub allowlist: &'a [String],
    pub keep_last_n: usize,
    pub now: DateTime<Utc>,
}

/// Run one pass, with listing and removal both injected — mirrors
/// [`crate::deep_clean::run_pass`]'s shape.
pub fn run_pass(
    inputs: &DockerRetentionInputs<'_>,
    lister: &dyn Fn() -> Option<Vec<DockerImageRecord>>,
    remover: &dyn Fn(&str) -> bool,
    take_build_slot: &dyn Fn() -> Option<String>,
) -> DockerRetentionReport {
    if !inputs.enabled {
        return DockerRetentionReport {
            enabled: false,
            plan: None,
            removed: Vec::new(),
            deferred: None,
            at: inputs.now,
        };
    }

    let Some(images) = lister() else {
        return DockerRetentionReport {
            enabled: true,
            plan: None,
            removed: Vec::new(),
            deferred: None,
            at: inputs.now,
        };
    };

    let plan = plan_retention(&images, inputs.tracked_repos, inputs.allowlist, inputs.keep_last_n);
    let to_remove = plan.to_remove();
    if to_remove.is_empty() {
        return DockerRetentionReport {
            enabled: true,
            plan: Some(plan),
            removed: Vec::new(),
            deferred: None,
            at: inputs.now,
        };
    }

    // Hold the machine-wide build slot for the removal, exactly like
    // `deep_clean::production_sweep` — an in-progress `docker build` (which
    // itself briefly produces dangling intermediate layers before the final
    // tag lands) is never targeted mid-build.
    if let Some(reason) = take_build_slot() {
        return DockerRetentionReport {
            enabled: true,
            plan: Some(plan),
            removed: Vec::new(),
            deferred: Some(reason),
            at: inputs.now,
        };
    }

    let removed: Vec<DockerImageRecord> = to_remove
        .into_iter()
        .filter(|img| remover(&img.id))
        .cloned()
        .collect();

    DockerRetentionReport {
        enabled: true,
        plan: Some(plan),
        removed,
        deferred: None,
        at: inputs.now,
    }
}

/// The production build-slot seam: `Some(reason)` when the slot could not be
/// held (defer), `None` when held (the lease is released when this returns,
/// which is fine — the slot only needs to be *held during the decision*, the
/// same way `deep_clean::production_sweep` holds it only across its own
/// removal call; a second daemon's build cannot observe a gap here because
/// `docker rmi` itself is the only thing racing a `docker build`, and it is
/// the `docker build` invocation, not this lease, that Docker itself
/// serializes for a given image reference).
fn production_take_build_slot() -> Option<String> {
    let Some(slot_dir) = crate::build_slot::slot_dir() else {
        return Some(
            "no machine build-slot directory is resolvable (no home directory)".to_string(),
        );
    };
    let lease = crate::build_slot::acquire_in(
        &slot_dir,
        crate::build_slot::resolve_slots(),
        Duration::from_secs(DOCKER_RETENTION_SLOT_WAIT_SECS),
        DOCKER_RETENTION_SLOT_POLL,
        crate::build_slot::resolve_stale(),
        "docker-image-retention",
    );
    if !lease.holds_slot() {
        let why = match lease.kind() {
            crate::build_slot::LeaseKind::DegradedOpen { reason } => reason.clone(),
            other => other.as_str().to_string(),
        };
        return Some(format!(
            "could not hold the machine build slot ({why}) — deferring so this can never remove an \
             image mid-build"
        ));
    }
    None
}

/// Log one pass's outcome — `WARN` when it removed anything or deferred
/// (an operator should see multi-GB reclaims and stuck deferrals at default
/// verbosity), `DEBUG` otherwise.
pub fn log_report(report: &DockerRetentionReport) {
    if report.deferred.is_some() || !report.removed.is_empty() {
        log::warn!("docker_image_clean: {}", report.reason());
    } else {
        log::debug!("docker_image_clean: {}", report.reason());
    }
}

// ============================================================================
// Host-wide cooldown state
// ============================================================================

/// Process-global "when did a pass last actually evaluate `docker`" —
/// deliberately host-wide (not per-repo, unlike `deep_clean`'s state map):
/// Docker images are not scoped to any one registered repo, and a host with
/// several repos ticking on the same reaper cadence must not re-shell to
/// `docker` once per repo per tick.
static LAST_EVALUATED_AT: OnceLock<Mutex<Option<DateTime<Utc>>>> = OnceLock::new();

fn last_evaluated_slot() -> &'static Mutex<Option<DateTime<Utc>>> {
    LAST_EVALUATED_AT.get_or_init(|| Mutex::new(None))
}

/// Whether enough time has passed since the last evaluation to run another —
/// the cooldown gate `run_for` applies before touching `docker` at all.
#[must_use]
fn cooldown_elapsed(now: DateTime<Utc>, min_interval_secs: u64) -> bool {
    let guard = last_evaluated_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match *guard {
        None => true,
        Some(last) => {
            let since = (now - last).num_seconds();
            since < 0 || since >= i64::try_from(min_interval_secs).unwrap_or(i64::MAX)
        }
    }
}

fn record_evaluated(now: DateTime<Utc>) {
    let mut guard = last_evaluated_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = Some(now);
}

/// Drop cooldown state. Test-only seam (the process-global would otherwise
/// leak between `#[serial]` tests in the same binary).
#[doc(hidden)]
pub fn reset_state_for_test() {
    *last_evaluated_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

/// Run one production pass, honoring the host-wide cooldown, and log the
/// result. Called once per registered repo per reaper tick (see
/// `worktree_reaper::reap_repo`) — every call after the first inside the
/// cooldown window is a cheap no-op (a clock read, no `docker` shell-out).
pub fn run_for(repo_root: &Path) {
    let config = read_docker_retention_config(repo_root);
    let enabled = resolve_enabled(&config);
    let now = Utc::now();

    if enabled && !cooldown_elapsed(now, resolve_min_interval_secs(&config)) {
        return;
    }

    let inputs = DockerRetentionInputs {
        enabled,
        tracked_repos: &resolve_tracked_repos(&config),
        allowlist: &resolve_allowlist(&config),
        keep_last_n: resolve_keep_last_n(&config),
        now,
    };
    let report = run_pass(&inputs, &list_images, &remove_image, &production_take_build_slot);
    if enabled {
        record_evaluated(now);
    }
    log_report(&report);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serial_test::serial;

    fn t(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_800_000_000 + secs, 0).unwrap()
    }

    fn image(id: &str, tags: &[&str], created_secs: i64, size_gb: f64) -> DockerImageRecord {
        DockerImageRecord {
            id: id.to_string(),
            repo_tags: tags.iter().map(|s| (*s).to_string()).collect(),
            created_at: t(created_secs),
            size_bytes: (size_gb * 1024.0 * 1024.0 * 1024.0) as u64,
        }
    }

    fn tracked() -> Vec<String> {
        DEFAULT_TRACKED_REPOS
            .iter()
            .map(|s| (*s).to_string())
            .collect()
    }

    // ===================================================================
    // repo_of / is_dangling
    // ===================================================================

    #[test]
    fn repo_of_splits_on_the_last_colon() {
        assert_eq!(repo_of("loom-worker:ci-smoke"), "loom-worker");
        assert_eq!(
            repo_of("ghcr.io/rjwalters/loom-worker:ci-smoke"),
            "ghcr.io/rjwalters/loom-worker"
        );
        assert_eq!(repo_of("untagged"), "untagged");
    }

    #[test]
    fn empty_repo_tags_is_dangling() {
        assert!(image("sha256:a", &[], 0, 1.0).is_dangling());
    }

    #[test]
    fn none_none_repo_tag_is_dangling() {
        assert!(image("sha256:a", &["<none>:<none>"], 0, 1.0).is_dangling());
    }

    #[test]
    fn a_real_tag_is_not_dangling() {
        assert!(!image("sha256:a", &["loom-worker:ci-smoke"], 0, 1.0).is_dangling());
    }

    // ===================================================================
    // plan_retention — the pure core
    // ===================================================================

    #[test]
    fn dangling_images_are_always_planned_for_removal() {
        let images = vec![
            image("sha256:a", &[], 0, 1.0),
            image("sha256:b", &["<none>:<none>"], 0, 2.0),
        ];
        let plan = plan_retention(&images, &tracked(), &[], 2);
        assert_eq!(plan.remove_dangling.len(), 2);
        assert!(plan.remove_stale_tracked.is_empty());
        assert!(plan.kept.is_empty());
    }

    #[test]
    fn a_tracked_repo_keeps_only_the_newest_n() {
        // 4 tagged images in loom-worker, newest-first by construction:
        // sha256:d (t=30) > sha256:c (t=20) > sha256:b (t=10) > sha256:a (t=0)
        let images = vec![
            image("sha256:a", &["loom-worker:old-1"], 0, 1.0),
            image("sha256:b", &["loom-worker:old-2"], 10, 1.0),
            image("sha256:c", &["loom-worker:ci-smoke-prev"], 20, 1.0),
            image("sha256:d", &["loom-worker:ci-smoke"], 30, 1.0),
        ];
        let plan = plan_retention(&images, &tracked(), &[], 2);
        let kept_ids: Vec<&str> = plan.kept.iter().map(|i| i.id.as_str()).collect();
        let removed_ids: Vec<&str> = plan
            .remove_stale_tracked
            .iter()
            .map(|i| i.id.as_str())
            .collect();
        assert_eq!(kept_ids, vec!["sha256:d", "sha256:c"], "newest 2 survive");
        assert_eq!(
            removed_ids,
            vec!["sha256:b", "sha256:a"],
            "the older 2 are removed, newest-first"
        );
    }

    #[test]
    fn fewer_than_n_tracked_images_are_all_kept() {
        let images = vec![image("sha256:a", &["loom-worker:ci-smoke"], 0, 1.0)];
        let plan = plan_retention(&images, &tracked(), &[], 2);
        assert_eq!(plan.kept.len(), 1);
        assert!(plan.remove_stale_tracked.is_empty());
    }

    #[test]
    fn multi_tag_aliasing_is_one_unit_not_double_counted() {
        // The exact scenario from the issue: a local tag and a ghcr.io mirror
        // of the identical digest — one DockerImageRecord, one decision.
        let images = vec![
            image(
                "sha256:newest",
                &[
                    "loom-worker:ci-smoke",
                    "ghcr.io/rjwalters/loom-worker:ci-smoke",
                ],
                20,
                1.0,
            ),
            image("sha256:older", &["loom-worker:ci-smoke-old"], 0, 1.0),
        ];
        let plan = plan_retention(&images, &tracked(), &[], 1);
        assert_eq!(plan.kept.len(), 1);
        assert_eq!(plan.kept[0].repo_tags.len(), 2, "both aliases travel together");
        assert_eq!(plan.remove_stale_tracked.len(), 1);
        assert_eq!(plan.remove_stale_tracked[0].id, "sha256:older");
    }

    #[test]
    fn two_tracked_repos_are_retained_independently() {
        // loom-worker and loom-worker-session each get their own keepLastN=1
        // budget rather than sharing one global budget.
        let images = vec![
            image("sha256:w1", &["loom-worker:ci-smoke"], 20, 1.0),
            image("sha256:w0", &["loom-worker:old"], 0, 1.0),
            image("sha256:s1", &["loom-worker-session:ci-smoke"], 20, 1.0),
            image("sha256:s0", &["loom-worker-session:old"], 0, 1.0),
        ];
        let plan = plan_retention(&images, &tracked(), &[], 1);
        let kept_ids: Vec<&str> = plan.kept.iter().map(|i| i.id.as_str()).collect();
        assert!(kept_ids.contains(&"sha256:w1"));
        assert!(kept_ids.contains(&"sha256:s1"));
        assert_eq!(plan.remove_stale_tracked.len(), 2);
    }

    #[test]
    fn allowlisted_long_lived_base_image_is_never_swept() {
        let images = vec![image(
            "sha256:eda",
            &["shared/eda-toolchain:2026.1"],
            0,
            6.5,
        )];
        let plan = plan_retention(&images, &tracked(), &["eda".to_string()], 1);
        assert_eq!(plan.allowlisted.len(), 1);
        assert!(plan.remove_dangling.is_empty());
        assert!(plan.remove_stale_tracked.is_empty());
    }

    #[test]
    fn allowlisted_image_survives_even_when_it_would_be_stale_tracked() {
        let images = vec![
            image("sha256:new", &["loom-worker:ci-smoke"], 20, 1.0),
            image("sha256:shared", &["loom-worker:shared-base"], 0, 6.5),
        ];
        // Without the allowlist, keepLastN=1 would remove sha256:shared.
        let plan = plan_retention(&images, &tracked(), &["shared-base".to_string()], 1);
        assert_eq!(plan.allowlisted.len(), 1);
        assert_eq!(plan.allowlisted[0].id, "sha256:shared");
        assert!(plan.remove_stale_tracked.is_empty());
    }

    #[test]
    fn untracked_non_dangling_images_are_never_touched() {
        let images = vec![image("sha256:other", &["ubuntu:22.04"], 0, 0.1)];
        let plan = plan_retention(&images, &tracked(), &[], 2);
        assert_eq!(plan.kept.len(), 1);
        assert!(plan.remove_dangling.is_empty());
        assert!(plan.remove_stale_tracked.is_empty());
    }

    #[test]
    fn plan_summary_reports_counts_and_size() {
        let images = vec![
            image("sha256:a", &[], 0, 2.0),
            image("sha256:b", &["loom-worker:old"], 0, 1.0),
            image("sha256:c", &["loom-worker:ci-smoke"], 20, 1.0),
        ];
        let plan = plan_retention(&images, &tracked(), &[], 1);
        assert!(plan.summary().contains("1 dangling"));
        assert!(plan.summary().contains("1 stale tracked"));
    }

    #[test]
    fn empty_plan_summarizes_as_nothing() {
        assert_eq!(RetentionPlan::default().summary(), "nothing");
    }

    // ===================================================================
    // run_pass — the injected I/O shell
    // ===================================================================

    #[test]
    fn disabled_never_lists_or_removes() {
        let inputs = DockerRetentionInputs {
            enabled: false,
            tracked_repos: &tracked(),
            allowlist: &[],
            keep_last_n: 2,
            now: t(0),
        };
        let listed = std::sync::atomic::AtomicBool::new(false);
        let report = run_pass(
            &inputs,
            &|| {
                listed.store(true, std::sync::atomic::Ordering::SeqCst);
                Some(Vec::new())
            },
            &|_| panic!("must never remove while disabled"),
            &|| None,
        );
        assert!(!listed.load(std::sync::atomic::Ordering::SeqCst));
        assert!(report.removed.is_empty());
        assert_eq!(report.plan, None);
    }

    #[test]
    fn docker_unavailable_removes_nothing_and_is_distinguishable_from_empty() {
        let inputs = DockerRetentionInputs {
            enabled: true,
            tracked_repos: &tracked(),
            allowlist: &[],
            keep_last_n: 2,
            now: t(0),
        };
        let report = run_pass(&inputs, &|| None, &|_| panic!("must never remove"), &|| None);
        assert!(report.plan.is_none());
        assert!(report.reason().contains("unqueryable"));
    }

    #[test]
    fn a_build_in_progress_holding_the_slot_defers_removal_entirely() {
        let images = vec![image("sha256:a", &[], 0, 1.0)];
        let inputs = DockerRetentionInputs {
            enabled: true,
            tracked_repos: &tracked(),
            allowlist: &[],
            keep_last_n: 2,
            now: t(0),
        };
        let report = run_pass(
            &inputs,
            &move || Some(images.clone()),
            &|_| panic!("must never remove while the build slot is held elsewhere"),
            &|| Some("held by another build".to_string()),
        );
        assert!(report.removed.is_empty());
        assert_eq!(report.deferred.as_deref(), Some("held by another build"));
        // The plan was still computed (for observability) even though
        // nothing was actually removed.
        assert!(report.plan.is_some());
    }

    #[test]
    fn nothing_to_remove_never_takes_the_build_slot() {
        let images = vec![image("sha256:a", &["loom-worker:ci-smoke"], 0, 1.0)];
        let inputs = DockerRetentionInputs {
            enabled: true,
            tracked_repos: &tracked(),
            allowlist: &[],
            keep_last_n: 2,
            now: t(0),
        };
        let took_slot = std::sync::atomic::AtomicBool::new(false);
        let report = run_pass(
            &inputs,
            &move || Some(images.clone()),
            &|_| panic!("nothing planned for removal"),
            &|| {
                took_slot.store(true, std::sync::atomic::Ordering::SeqCst);
                None
            },
        );
        assert!(!took_slot.load(std::sync::atomic::Ordering::SeqCst));
        assert!(report.removed.is_empty());
    }

    #[test]
    fn removal_failures_are_soft_and_excluded_from_the_removed_list() {
        let images = vec![
            image("sha256:a", &[], 0, 1.0),
            image("sha256:b", &[], 0, 1.0),
        ];
        let inputs = DockerRetentionInputs {
            enabled: true,
            tracked_repos: &tracked(),
            allowlist: &[],
            keep_last_n: 2,
            now: t(0),
        };
        let report = run_pass(
            &inputs,
            &move || Some(images.clone()),
            &|id| id == "sha256:a", // sha256:b "fails" (e.g. backs a running container)
            &|| None,
        );
        assert_eq!(report.removed.len(), 1);
        assert_eq!(report.removed[0].id, "sha256:a");
        assert!(report.deferred.is_none());
    }

    #[test]
    fn reason_mentions_removed_kept_and_allowlisted_counts() {
        let images = vec![
            image("sha256:a", &[], 0, 1.0),
            image("sha256:b", &["loom-worker:ci-smoke"], 0, 1.0),
        ];
        let inputs = DockerRetentionInputs {
            enabled: true,
            tracked_repos: &tracked(),
            allowlist: &[],
            keep_last_n: 2,
            now: t(0),
        };
        let report = run_pass(&inputs, &move || Some(images.clone()), &|_| true, &|| None);
        assert!(report.reason().contains("removed 1 of planned 1"));
        assert!(report.reason().contains("1 kept"));
    }

    // ===================================================================
    // Cooldown (host-wide, not per-repo)
    // ===================================================================

    #[test]
    #[serial]
    fn cooldown_elapsed_is_true_before_any_evaluation() {
        reset_state_for_test();
        assert!(cooldown_elapsed(t(0), 1_800));
        reset_state_for_test();
    }

    #[test]
    #[serial]
    fn a_recent_evaluation_holds_the_cooldown() {
        reset_state_for_test();
        record_evaluated(t(0));
        assert!(!cooldown_elapsed(t(60), 1_800));
        reset_state_for_test();
    }

    #[test]
    #[serial]
    fn the_cooldown_expires() {
        reset_state_for_test();
        record_evaluated(t(0));
        assert!(cooldown_elapsed(t(1_801), 1_800));
        reset_state_for_test();
    }

    // ===================================================================
    // Config resolution defaults
    // ===================================================================

    #[test]
    fn default_config_resolves_to_documented_defaults() {
        let config = DockerRetentionConfig::default();
        assert!(resolve_enabled(&config));
        assert_eq!(resolve_keep_last_n(&config), DEFAULT_KEEP_LAST_N);
        assert_eq!(resolve_min_interval_secs(&config), DEFAULT_MIN_INTERVAL_SECS);
        assert_eq!(resolve_tracked_repos(&config), tracked());
        assert!(resolve_allowlist(&config).is_empty());
    }

    #[test]
    fn config_values_override_defaults() {
        let config = DockerRetentionConfig {
            enabled: Some(false),
            keep_last_n: Some(5),
            min_interval_secs: Some(3_600),
            tracked_repos: Some(vec!["my-repo".to_string()]),
            allowlist: Some(vec!["shared".to_string()]),
        };
        assert!(!resolve_enabled(&config));
        assert_eq!(resolve_keep_last_n(&config), 5);
        assert_eq!(resolve_min_interval_secs(&config), 3_600);
        assert_eq!(resolve_tracked_repos(&config), vec!["my-repo".to_string()]);
        assert_eq!(resolve_allowlist(&config), vec!["shared".to_string()]);
    }

    #[test]
    fn read_config_soft_fails_to_defaults_on_a_repo_with_no_block() {
        let tmp = tempfile::tempdir().unwrap();
        let config = read_docker_retention_config(tmp.path());
        assert_eq!(config, DockerRetentionConfig::default());
    }
}
