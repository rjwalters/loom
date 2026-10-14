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
//!    the `loom-worker` / `loom-worker-session` / `loom-worker-native`
//!    family and their `ghcr.io` aliases), only the `keepLastN` most
//!    recently created images survive;
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
//! 3. **Unused images under disk pressure (#11195).** When free space on the
//!    Docker data volume is below the floor (`diskWarnFreeGb`), images that
//!    no container (running or stopped) references and that are older than
//!    `unusedMaxAgeDays` (default 7) are removed **largest first, stopping
//!    once the free-space deficit is covered** —
//!    [`RetentionPlan::remove_unused_aged`]. Docker exposes no portable
//!    "last used" timestamp, so the conservative age source is the image's
//!    **creation time** (an image pulled recently but built long ago counts as
//!    old; this errs toward removing, but only ever under pressure and never
//!    for a container-referenced or allowlisted image). Above the floor this
//!    rule never fires.
//!
//! **Untagged includes digest-only refs (#11195).** On containerd-store hosts a
//! superseded image keeps a digest ref (`openroad/orfs@sha256:…`) in
//! `RepoTags` rather than becoming `<none>:<none>`; an image whose every
//! `RepoTags` entry is a digest ref or `<none>:<none>` is untagged. An image
//! with any real `repo:tag` alias alongside a digest ref is not.
//!
//! **Container-referenced images are never planned** (any removal rule),
//! whether the container is running or stopped.
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

/// Env override for the unused-image age floor (days) applied under pressure.
pub const DOCKER_RETENTION_UNUSED_MAX_AGE_DAYS_ENV: &str =
    "LOOM_DOCKER_IMAGE_RETENTION_UNUSED_MAX_AGE_DAYS";

/// Default age (days since creation) past which an unreferenced image may be
/// removed when the host is below the free-space floor (#11195).
pub const DEFAULT_UNUSED_MAX_AGE_DAYS: u64 = 7;

/// Default cooldown between passes: 30 minutes. Much shorter than
/// `deep_clean`'s 6h — this pass is not disk-pressure-gated and cheap, the
/// cooldown exists only to avoid re-shelling to `docker` on every repo in a
/// multi-repo host's same reaper tick.
pub const DEFAULT_MIN_INTERVAL_SECS: u64 = 1_800;

/// The repositories this pass manages by default — the exact build sites
/// `.github/workflows/ci.yml`'s `worker-image-smoke` / `session-image-smoke`
/// / `native-image-smoke` jobs tag, plus their `ghcr.io` aliases (verified
/// against `origin/main`, 2026-09-07, issue #7332; `loom-worker-native`
/// added with the image itself, issue #8403).
///
/// Membership is **exact repository-name equality** (see
/// [`plan_retention`]), not a substring test — `loom-worker-native` is a
/// distinct entry precisely because `loom-worker` does not cover it.
pub const DEFAULT_TRACKED_REPOS: &[&str] = &[
    "loom-worker",
    "loom-worker-session",
    "loom-worker-native",
    "ghcr.io/rjwalters/loom-worker",
    "ghcr.io/rjwalters/loom-worker-session",
    "ghcr.io/rjwalters/loom-worker-native",
];

/// How long the pass waits for the machine-wide build slot before deferring —
/// mirrors [`crate::deep_clean::DEEP_CLEAN_SLOT_WAIT_SECS`].
pub const DOCKER_RETENTION_SLOT_WAIT_SECS: u64 = 5;

const DOCKER_RETENTION_SLOT_POLL: Duration = Duration::from_millis(500);

/// docker CLI binary name — overridable only for tests (production always
/// uses the system `docker`).
const DOCKER_BIN: &str = "docker";

/// Default Docker data root; its nearest existing ancestor's filesystem is the
/// volume the free-space floor is measured against (#11195).
const DOCKER_DATA_ROOT: &str = "/var/lib/docker";

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
    /// `…dockerImageRetention.unusedMaxAgeDays` (default
    /// [`DEFAULT_UNUSED_MAX_AGE_DAYS`]).
    pub unused_max_age_days: Option<u64>,
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
        unused_max_age_days: block
            .get("unusedMaxAgeDays")
            .and_then(serde_json::Value::as_u64),
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

/// Resolve the unused-image age floor (days) — precedence
/// **env > config > default**.
#[must_use]
pub fn resolve_unused_max_age_days(config: &DockerRetentionConfig) -> u64 {
    std::env::var(DOCKER_RETENTION_UNUSED_MAX_AGE_DAYS_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .or(config.unused_max_age_days)
        .unwrap_or(DEFAULT_UNUSED_MAX_AGE_DAYS)
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
    /// True when any container (running **or** stopped) references this image
    /// ID. Such an image is never planned for removal (#11195).
    pub in_use: bool,
}

/// True for a digest reference (`repo@sha256:…`) as opposed to `repo:tag`.
fn is_digest_ref(tag: &str) -> bool {
    tag.contains("@sha256:")
}

impl DockerImageRecord {
    /// True when no real `repo:tag` name points at this image: `RepoTags` is
    /// empty, or every entry is `<none>:<none>` or a digest ref
    /// (`repo@sha256:…`, the shape a superseded image keeps on
    /// containerd-store hosts — #11195).
    #[must_use]
    pub fn is_dangling(&self) -> bool {
        self.repo_tags
            .iter()
            .all(|t| t == "<none>:<none>" || is_digest_ref(t))
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
    /// Unreferenced, old, otherwise-unmanaged images, removed only while the
    /// host is below the free-space floor (#11195), largest first.
    pub remove_unused_aged: Vec<DockerImageRecord>,
    /// Bytes of unreferenced images this plan leaves behind (kept or
    /// allowlisted, not container-referenced) — surfaced so a pass that
    /// removed nothing next to a large reclaimable pool shows as an anomaly.
    pub reclaimable_left_bytes: u64,
}

/// Disk-pressure inputs for the unused-image rule (#11195).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PressurePolicy {
    /// Free GB on the Docker data volume.
    pub free_gb: u64,
    /// The floor (`diskWarnFreeGb`).
    pub floor_gb: u64,
    /// Minimum age in days (since creation) for an unused image.
    pub unused_max_age_days: u64,
    /// Evaluation time.
    pub now: DateTime<Utc>,
}

impl PressurePolicy {
    /// Whether free space is strictly below the floor.
    #[must_use]
    pub fn below_floor(&self) -> bool {
        self.free_gb < self.floor_gb
    }
}

const GIB: u64 = 1024 * 1024 * 1024;

impl RetentionPlan {
    /// Every image this plan would remove, dangling first.
    #[must_use]
    pub fn to_remove(&self) -> Vec<&DockerImageRecord> {
        self.remove_dangling
            .iter()
            .chain(self.remove_stale_tracked.iter())
            .chain(self.remove_unused_aged.iter())
            .collect()
    }

    /// `"3 dangling (7.6G), 2 stale tracked (2.4G)"`, or `"nothing"`.
    #[must_use]
    pub fn summary(&self) -> String {
        if self.to_remove().is_empty() {
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
        if !self.remove_unused_aged.is_empty() {
            let bytes: u64 = self.remove_unused_aged.iter().map(|i| i.size_bytes).sum();
            parts.push(format!(
                "{} unused aged ({})",
                self.remove_unused_aged.len(),
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
    plan_retention_with_pressure(images, tracked_repos, allowlist, keep_last_n, None)
}

/// [`plan_retention`] plus the disk-pressure unused-image rule (#11195).
///
/// Order: allowlist, then container-referenced (never removed), then
/// untagged, then tracked family, then — only when `pressure` says the host is
/// below the floor — unused images older than `unused_max_age_days`, largest
/// first, until the bytes already planned plus these cover the free-space
/// deficit.
#[must_use]
pub fn plan_retention_with_pressure(
    images: &[DockerImageRecord],
    tracked_repos: &[String],
    allowlist: &[String],
    keep_last_n: usize,
    pressure: Option<&PressurePolicy>,
) -> RetentionPlan {
    let mut plan = RetentionPlan::default();
    let mut tracked: BTreeMap<String, Vec<DockerImageRecord>> = BTreeMap::new();
    let mut aged_candidates: Vec<DockerImageRecord> = Vec::new();

    for image in images {
        if matches_allowlist(image, allowlist) {
            plan.allowlisted.push(image.clone());
            continue;
        }
        if image.in_use && !tracked_member(image, tracked_repos) {
            plan.kept.push(image.clone());
            continue;
        }
        if image.is_dangling() {
            plan.remove_dangling.push(image.clone());
            continue;
        }
        let tracked_repo = image
            .repo_tags
            .iter()
            .filter(|rt| !is_digest_ref(rt))
            .map(|rt| repo_of(rt))
            .find(|repo| tracked_repos.iter().any(|tr| tr == repo));
        match tracked_repo {
            Some(repo) => tracked
                .entry(repo.to_string())
                .or_default()
                .push(image.clone()),
            None => {
                let old_enough = pressure.is_some_and(|p| {
                    let age_days = (p.now - image.created_at).num_days();
                    age_days >= 0 && age_days as u64 >= p.unused_max_age_days
                });
                if old_enough {
                    aged_candidates.push(image.clone());
                } else {
                    plan.kept.push(image.clone());
                }
            }
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
            // A container-referenced image is never removed, whatever its rank.
            if i < keep_last_n || image.in_use {
                plan.kept.push(image);
            } else {
                plan.remove_stale_tracked.push(image);
            }
        }
    }

    if let Some(p) = pressure.filter(|p| p.below_floor()) {
        let mut remaining = p.floor_gb.saturating_sub(p.free_gb).saturating_mul(GIB);
        let planned: u64 = plan
            .remove_dangling
            .iter()
            .chain(plan.remove_stale_tracked.iter())
            .map(|i| i.size_bytes)
            .sum();
        remaining = remaining.saturating_sub(planned);
        // Largest first; ID breaks ties deterministically.
        aged_candidates.sort_by(|a, b| {
            b.size_bytes
                .cmp(&a.size_bytes)
                .then_with(|| a.id.cmp(&b.id))
        });
        for image in aged_candidates {
            if remaining > 0 {
                remaining = remaining.saturating_sub(image.size_bytes);
                plan.remove_unused_aged.push(image);
            } else {
                plan.kept.push(image);
            }
        }
    } else {
        plan.kept.append(&mut aged_candidates);
    }

    plan.reclaimable_left_bytes = plan
        .kept
        .iter()
        .chain(plan.allowlisted.iter())
        .filter(|i| !i.in_use)
        .map(|i| i.size_bytes)
        .sum();

    plan
}

/// Whether any non-digest alias of `image` is in a tracked repository.
fn tracked_member(image: &DockerImageRecord, tracked_repos: &[String]) -> bool {
    image
        .repo_tags
        .iter()
        .filter(|rt| !is_digest_ref(rt))
        .any(|rt| tracked_repos.iter().any(|tr| tr == repo_of(rt)))
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

    let in_use_ids = list_container_image_ids()?;

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
                let in_use = in_use_ids.contains(&img.id);
                Some(DockerImageRecord {
                    id: img.id,
                    repo_tags: img.repo_tags.unwrap_or_default(),
                    created_at,
                    size_bytes: img.size.unwrap_or(0),
                    in_use,
                })
            })
            .collect(),
    )
}

/// Image IDs referenced by any container, running **or** stopped (#11195).
/// `None` on any failure — unknown must never read as "no container uses it".
fn list_container_image_ids() -> Option<std::collections::BTreeSet<String>> {
    let ps = Command::new(DOCKER_BIN)
        .args(["ps", "-aq", "--no-trunc"])
        .output()
        .ok()?;
    if !ps.status.success() {
        return None;
    }
    let containers: Vec<String> = String::from_utf8_lossy(&ps.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    if containers.is_empty() {
        return Some(std::collections::BTreeSet::new());
    }
    let inspect = Command::new(DOCKER_BIN)
        .args(["inspect", "--format", "{{.Image}}"])
        .args(&containers)
        .output()
        .ok()?;
    if !inspect.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&inspect.stdout)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
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

/// The build-slot seam [`run_pass`] takes.
///
/// **Deliberately a "run this work while the slot is held" callback, not a
/// stateless `-> Option<String>` probe.** A probe can only report whether a
/// slot *was* free at the instant it was asked; the lease it took to find out
/// is released the moment the probe returns, so removal would run outside the
/// slot. Passing the work *in* is what lets the production implementation
/// ([`production_with_build_slot`]) keep its [`crate::build_slot::BuildSlotLease`]
/// alive across every `docker rmi` — the same lease lifetime
/// [`crate::deep_clean::production_sweep`] gives its own deletion call.
///
/// Contract: run `work` exactly once while the slot is held and return `None`,
/// **or** return `Some(reason)` without running `work` at all.
pub type WithBuildSlot<'a> = &'a dyn Fn(&mut dyn FnMut()) -> Option<String>;

/// Everything a fully-injected [`run_pass`] needs.
pub struct DockerRetentionInputs<'a> {
    pub enabled: bool,
    pub tracked_repos: &'a [String],
    pub allowlist: &'a [String],
    pub keep_last_n: usize,
    pub now: DateTime<Utc>,
    /// Disk-pressure inputs for the unused-image rule; `None` disables it
    /// (free space unmeasurable, or not applicable).
    pub pressure: Option<PressurePolicy>,
}

/// Run one pass, with listing and removal both injected — mirrors
/// [`crate::deep_clean::run_pass`]'s shape.
pub fn run_pass(
    inputs: &DockerRetentionInputs<'_>,
    lister: &dyn Fn() -> Option<Vec<DockerImageRecord>>,
    remover: &dyn Fn(&str) -> bool,
    with_build_slot: WithBuildSlot<'_>,
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

    let plan = plan_retention_with_pressure(
        &images,
        inputs.tracked_repos,
        inputs.allowlist,
        inputs.keep_last_n,
        inputs.pressure.as_ref(),
    );
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

    // Hold the machine-wide build slot **for the whole removal**, exactly like
    // `deep_clean::production_sweep` — an in-progress `docker build` (which
    // itself briefly produces dangling intermediate layers before the final
    // tag lands) is never targeted mid-build. The removal loop runs *inside*
    // the seam so the lease cannot be released before the `docker rmi` calls
    // it is meant to protect; see [`WithBuildSlot`].
    let mut removed: Vec<DockerImageRecord> = Vec::new();
    let deferred = with_build_slot(&mut || {
        removed = to_remove
            .iter()
            .filter(|img| remover(&img.id))
            .map(|img| (*img).clone())
            .collect();
    });

    if let Some(reason) = deferred {
        return DockerRetentionReport {
            enabled: true,
            plan: Some(plan),
            removed: Vec::new(),
            deferred: Some(reason),
            at: inputs.now,
        };
    }

    DockerRetentionReport {
        enabled: true,
        plan: Some(plan),
        removed,
        deferred: None,
        at: inputs.now,
    }
}

/// The production build-slot seam: take the machine-wide build slot, run
/// `work` (the `docker rmi` loop) **while still holding it**, and release only
/// afterwards. Returns `Some(reason)` — without running `work` — when the slot
/// could not be taken, so the pass defers to the next tick.
///
/// The lease must outlive the `work()` call, not merely the acquire: releasing
/// it first would prove only "no build was running at the instant we checked",
/// while a `docker build` started microseconds later could still have its
/// intermediate layers removed out from under it. This mirrors
/// [`crate::deep_clean::production_sweep`], which keeps its own lease bound
/// across `clean::sweep_primary_checkout_artifacts` and drops it only once the
/// deletion has finished.
fn production_with_build_slot(work: &mut dyn FnMut()) -> Option<String> {
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

    work();
    // Explicit (rather than end-of-scope) so the ordering — every removal
    // first, release second — is visible at the call site, as in `deep_clean`.
    drop(lease);
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

/// Arm the host-wide cooldown as if a pass had just evaluated at `now`.
/// Test-only seam (#7512): lets [`crate::eager_reclaim`]'s own test module
/// assert that an eagerly-triggered call short-circuits on *this* module's
/// cooldown **without** ever shelling out to `docker` — which is what makes
/// that test safe to run on a developer machine holding real images.
#[doc(hidden)]
pub fn record_evaluated_for_test(now: DateTime<Utc>) {
    record_evaluated(now);
}

/// Run one production pass, honoring the host-wide cooldown, and log the
/// result. Called once per registered repo per reaper tick (see
/// `worktree_reaper::reap_repo`) — every call after the first inside the
/// cooldown window is a cheap no-op (a clock read, no `docker` shell-out).
///
/// Returns the evaluated [`DockerRetentionReport`] (#7512) so a caller that
/// needs to know *what this call actually did* — the eager, out-of-cycle
/// reclaim trigger in [`crate::eager_reclaim`] — does not have to re-derive it
/// from logs. A cooldown-skipped call returns a report with an empty `removed`,
/// no `plan`, and a `deferred` reason naming the cooldown, so the caller can
/// tell "we were inside the window" apart from "`docker` was unqueryable"
/// (both of which produce `plan: None`). Unlike the past-cooldown paths that
/// skip is **never logged**, preserving the pre-#7512 silence — a cooldown skip
/// on a multi-repo host must not spam once per repo per tick.
///
/// The early return is what makes the eager trigger safe: an eagerly-triggered
/// call inside the window costs one clock read and never reaches `docker`.
pub fn run_for(repo_root: &Path) -> DockerRetentionReport {
    let config = read_docker_retention_config(repo_root);
    let enabled = resolve_enabled(&config);
    let now = Utc::now();

    let min_interval_secs = resolve_min_interval_secs(&config);
    if enabled && !cooldown_elapsed(now, min_interval_secs) {
        return DockerRetentionReport {
            enabled,
            plan: None,
            removed: Vec::new(),
            deferred: Some(format!(
                "host-wide cooldown active (min interval {min_interval_secs}s) — \
                 `docker` was not queried"
            )),
            at: now,
        };
    }

    // #11195: the unused-image rule needs the floor and the free space on the
    // volume Docker actually stores images on. Unmeasurable free space means
    // no pressure rule (unknown != low).
    let reaper_config = crate::worktree_reaper::read_worktree_reaper_config(repo_root);
    let floor_gb = crate::worktree_reaper::resolve_disk_warn_free_gb(&reaper_config);
    let pressure = crate::disk_headroom::path_free_gb(Path::new(DOCKER_DATA_ROOT)).map(|free_gb| {
        PressurePolicy {
            free_gb,
            floor_gb,
            unused_max_age_days: resolve_unused_max_age_days(&config),
            now,
        }
    });

    let inputs = DockerRetentionInputs {
        enabled,
        tracked_repos: &resolve_tracked_repos(&config),
        allowlist: &resolve_allowlist(&config),
        keep_last_n: resolve_keep_last_n(&config),
        now,
        pressure,
    };
    let report = run_pass(&inputs, &list_images, &remove_image, &production_with_build_slot);
    if enabled {
        record_evaluated(now);
    }
    log_report(&report);
    // #11195: below the floor, always say what the pass left behind at INFO,
    // so "removed 0" next to tens of GB reclaimable reads as an anomaly.
    if let (Some(p), Some(plan)) = (pressure.filter(PressurePolicy::below_floor), &report.plan) {
        log::info!(
            "docker_image_clean: below floor ({} GB free < {} GB): planned {}, removed {},              {} left reclaimable",
            p.free_gb,
            p.floor_gb,
            plan.summary(),
            report.removed.len(),
            human_size(plan.reclaimable_left_bytes)
        );
    }
    report
}

#[cfg(test)]
mod tests;
