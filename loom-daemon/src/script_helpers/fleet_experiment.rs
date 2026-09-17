//! Fleet-wide, repo-stratified model A/B — `plan` and `start`/`stop`
//! (Issue #8055, phases 1–2 of 6).
//!
//! # What this is
//!
//! [`sweep_experiment`](super::sweep_experiment) randomizes **per issue** by
//! parity and only moves the work-finder dispatch path. Deciding whether Opus
//! earns its per-token weight across a managed fleet needs the other unit of
//! assignment: the **workspace**. This module is that surface —
//! `loom-daemon sweep-experiment plan | start | stop` — added as sub-actions on
//! the existing verb rather than a second top-level `experiment` verb so the
//! two cannot drift on what "an arm" means.
//!
//! # The three commands
//!
//! * **`plan`** reads the machine-level workspace registry
//!   (`~/.loom/workspaces.json`), measures each workspace, assigns an arm
//!   deterministically from `--seed`, and prints the table. It **writes
//!   nothing** except the optional `--out` plan file.
//! * **`start --plan <file>`** is the only thing that mutates a workspace: it
//!   deep-merges the arm's model into each workspace's
//!   [`LOCAL_CONFIG_REL`](crate::config_resolver::LOCAL_CONFIG_REL) overlay —
//!   the ungitted, highest-precedence, hot-reloaded tier — and records the plan
//!   under `~/.loom/experiments/<id>.json`.
//! * **`stop --id <id>`** reverses exactly the keys `start` wrote, and nothing
//!   else.
//!
//! # Why `stop` cannot be "delete the overlay file"
//!
//! `.loom-local/local.json` is the **operator's** host-local override tier. It
//! may already carry unrelated keys, and it may already carry a *different*
//! value for one of the keys the experiment writes. So `start` records, in a
//! `_loom_experiment` marker inside the file itself, both the pointers it wrote
//! and the prior value at each pointer (present or absent). `stop` restores
//! exactly that, prunes containers it emptied, and deletes the file only when
//! the whole document becomes `{}`. A pointer whose current value is no longer
//! what `start` wrote is **drift**: left in place and reported, unless
//! `--force`.
//!
//! # Determinism
//!
//! [`build_plan`] is a pure function of `(seed, sorted workspace list, strata,
//! arms, now)`. No wall clock beyond the `now` it is handed, no map-iteration
//! order (strata are grouped in a [`BTreeMap`]), no randomness: the same inputs
//! produce a byte-identical plan, which is the property the fixture tests lean
//! on.
//!
//! # Out of scope here (phases 3–6)
//!
//! `status` / drift reporting, `--scope dispatch|pipeline` (this phase always
//! writes both the dispatch and role-runner pointers), explicit arm stamping in
//! outcome records, and any change to the per-issue mode.

use anyhow::{bail, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub mod lifecycle;

#[cfg(test)]
mod tests;

/// Key of the ownership marker `start` writes into each overlay. Named with a
/// leading underscore so it sorts away from real config keys and reads as
/// metadata; `resolve_effective_config` merges it like any other key, and no
/// consumer looks at it.
pub const MARKER_KEY: &str = "_loom_experiment";

/// Environment override for the host-level experiment-state directory
/// (mirrors [`crate::workspace_registry::REGISTRY_PATH_ENV`]'s role for the
/// registry). Primarily a test seam.
pub const STATE_DIR_ENV: &str = "LOOM_EXPERIMENTS_DIR";

/// Dispatch-path model pointer (`resolve_dispatch_model`).
pub const DISPATCH_MODEL_POINTER: &str = "/autonomous/model";

/// Role-runner tick model pointer (`resolve_role_runner_model`).
pub const ROLE_RUNNER_MODEL_POINTER: &str = "/autonomous/roleRunner/model";

/// Every pointer `start` writes and `stop` reverses, highest-level first.
pub const EXPERIMENT_POINTERS: [&str; 2] = [DISPATCH_MODEL_POINTER, ROLE_RUNNER_MODEL_POINTER];

/// Stratification dimensions `--stratify` accepts.
pub const STRATIFY_DIMENSIONS: [&str; 2] = ["merges14d", "kind"];

/// Merge-count window, in days, behind the `merges14d` dimension.
pub const MERGE_WINDOW_DAYS: i64 = 14;

// ---------------------------------------------------------------------------
// Plan types
// ---------------------------------------------------------------------------

/// One workspace's assignment in a plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanWorkspace {
    /// Absolute workspace root, as the registry records it.
    pub path: String,
    /// `owner/name` when it could be resolved from the git remote, else the
    /// directory basename. Reporting only — `path` is the key.
    pub repo: String,
    /// The assigned arm. An arm's name **is** the model alias written into the
    /// overlay (`--arms opus,sonnet` ⇒ `autonomous.model = "opus"`).
    pub arm: String,
    /// The stratum this workspace was paired within, e.g.
    /// `merges14d=high,kind=rust`.
    pub stratum: String,
}

/// A complete assignment. This document — not the filesystem — is the source
/// of truth for `start`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    /// Stable identity, `exp-<YYYYMMDD>-<hash8>`. The hash covers the seed,
    /// arms, strata and the full assignment, so two plans that assign
    /// differently can never share an id.
    pub experiment_id: String,
    /// When the plan was built (`%Y-%m-%dT%H:%M:%SZ`).
    pub created_at: String,
    /// The `--seed` the assignment was derived from.
    pub seed: u64,
    /// Arms, in the order given on the command line.
    pub arms: Vec<String>,
    /// Dimensions used to stratify, in the order given on the command line.
    pub stratify: Vec<String>,
    /// Assignments, ordered by `path`.
    pub workspaces: Vec<PlanWorkspace>,
}

/// One workspace as **measured**, before any assignment happens. Separating
/// measurement (filesystem + `gh`) from assignment (pure) is what makes the
/// determinism test possible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceInput {
    /// Absolute workspace root.
    pub path: PathBuf,
    /// `owner/name`, or the basename when no remote resolves.
    pub repo: String,
    /// Merges in the last [`MERGE_WINDOW_DAYS`] days, or `None` when the count
    /// could not be measured (no `gh`, no network, not a forge repo). One
    /// `None` degrades the whole `merges14d` dimension — see
    /// [`merges_labels`].
    pub merges14d: Option<u32>,
    /// The [`repo_kind`] heuristic's verdict.
    pub kind: String,
}

// ---------------------------------------------------------------------------
// Measurement
// ---------------------------------------------------------------------------

/// Classify a repo by the first build manifest present at its root, in this
/// fixed order:
///
/// | check (repo root) | label |
/// |---|---|
/// | `Cargo.toml` | `rust` |
/// | `package.json` | `node` |
/// | `pyproject.toml`, `setup.py`, `requirements.txt` | `python` |
/// | `go.mod` | `go` |
/// | a `scripts/` directory | `shell` |
/// | none of the above | `docs` |
///
/// This is a **heuristic**, deliberately: it exists to keep a Rust daemon repo
/// from being paired against a prose repo, not to be a language census. It is
/// first-match-wins (a Rust repo with a `package.json` is `rust`), reads only
/// the workspace root, and never touches the network — so it is stable across
/// re-plans and identical on every host.
#[must_use]
pub fn repo_kind(root: &Path) -> String {
    let has = |name: &str| root.join(name).exists();
    if has("Cargo.toml") {
        return "rust".to_string();
    }
    if has("package.json") {
        return "node".to_string();
    }
    if has("pyproject.toml") || has("setup.py") || has("requirements.txt") {
        return "python".to_string();
    }
    if has("go.mod") {
        return "go".to_string();
    }
    if root.join("scripts").is_dir() {
        return "shell".to_string();
    }
    "docs".to_string()
}

/// `owner/name` for `root`, from `git remote get-url origin`; the directory
/// basename when there is no remote to parse. Never calls the forge API — a
/// plan must be buildable offline.
#[must_use]
pub fn repo_slug(root: &Path) -> String {
    let basename = || {
        root.file_name()
            .map_or_else(|| root.display().to_string(), |n| n.to_string_lossy().into_owned())
    };
    let out = super::run_git(root, &["remote", "get-url", "origin"]);
    let Some(url) = out.ok_stdout_trimmed() else {
        return basename();
    };
    crate::forge_cmd::parse_nwo_from_remote_url(&url).unwrap_or_else(basename)
}

/// Merged PRs in `root`'s forge repo within the last [`MERGE_WINDOW_DAYS`]
/// days, or `None` when the question could not be answered.
///
/// `None` is **"unknown", never "zero"** (the `cmd_out::Query` contract): a
/// missing `gh`, an unauthenticated one, or a rate-limited API must not be
/// silently recorded as a quiet repo, because that would put every unreachable
/// workspace in the same stratum.
#[must_use]
pub fn merges_in_window(root: &Path, now: DateTime<Utc>) -> Option<u32> {
    #[derive(Deserialize)]
    struct PrRow {
        #[allow(dead_code)]
        number: i64,
    }
    let cutoff = (now - Duration::days(MERGE_WINDOW_DAYS))
        .format("%Y-%m-%d")
        .to_string();
    let search = format!("merged:>={cutoff}");
    let q = super::gh_query::<Vec<PrRow>, _>(
        &[
            "pr", "list", "--state", "merged", "--search", &search, "--limit", "200", "--json",
            "number",
        ],
        root,
        true,
        Vec::is_empty,
    );
    match q {
        crate::cmd_out::Query::Populated(rows) => {
            Some(u32::try_from(rows.len()).unwrap_or(u32::MAX))
        }
        // A definite, successful "no merged PRs in the window" is a measurement.
        crate::cmd_out::Query::Empty => Some(0),
        _ => None,
    }
}

/// Measure every root. Returns the inputs **sorted by path** (the canonical
/// order every downstream step assumes) plus any human-readable notes about
/// measurements that could not be taken.
///
/// `offline` skips the `gh` call entirely, which is both the test seam and the
/// honest option on a host with no forge credentials.
#[must_use]
pub fn collect_inputs(
    roots: &[PathBuf],
    offline: bool,
    now: DateTime<Utc>,
) -> (Vec<WorkspaceInput>, Vec<String>) {
    let mut notes = Vec::new();
    let mut inputs: Vec<WorkspaceInput> = roots
        .iter()
        .map(|root| {
            let merges14d = if offline {
                None
            } else {
                merges_in_window(root, now)
            };
            if merges14d.is_none() && !offline {
                notes.push(format!(
                    "{}: could not measure merges in the last {MERGE_WINDOW_DAYS}d",
                    root.display()
                ));
            }
            WorkspaceInput {
                path: root.clone(),
                repo: repo_slug(root),
                merges14d,
                kind: repo_kind(root),
            }
        })
        .collect();
    inputs.sort_by(|a, b| a.path.cmp(&b.path));
    (inputs, notes)
}

// ---------------------------------------------------------------------------
// Stratification + assignment (pure)
// ---------------------------------------------------------------------------

/// 64 bits of SHA-256 over `seed` and `key` — a stable, cross-host,
/// cross-release shuffle key. `DefaultHasher` would not do: its output is
/// explicitly not stable across Rust releases, so a re-plan on an upgraded
/// host would silently reassign arms.
#[must_use]
pub fn shuffle_key(seed: u64, key: &str) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(seed.to_string().as_bytes());
    hasher.update([0x1f]);
    hasher.update(key.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(bytes)
}

/// Per-workspace labels for the `merges14d` dimension.
///
/// With a count for every workspace this is a median split: rank by merge
/// count descending (ties broken by path), the busier half is `high`, the rest
/// `low`. If **any** count is missing the dimension degrades wholesale to the
/// documented offline fallback — an alphabetical split, `alpha-a` for the
/// first half of the sorted path list and `alpha-b` for the rest. Degrading
/// wholesale rather than per workspace keeps the stratum labels comparable:
/// a mix of measured and unmeasured labels would pair a busy repo against an
/// unreachable one and call it a stratum.
///
/// `inputs` must already be sorted by path.
#[must_use]
pub fn merges_labels(inputs: &[WorkspaceInput]) -> Vec<String> {
    let n = inputs.len();
    if inputs.iter().any(|i| i.merges14d.is_none()) {
        return (0..n)
            .map(|i| {
                if i < n.div_ceil(2) {
                    "alpha-a".to_string()
                } else {
                    "alpha-b".to_string()
                }
            })
            .collect();
    }
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| {
        inputs[b]
            .merges14d
            .cmp(&inputs[a].merges14d)
            .then_with(|| inputs[a].path.cmp(&inputs[b].path))
    });
    let mut labels = vec![String::new(); n];
    for (rank, &idx) in order.iter().enumerate() {
        labels[idx] = if rank < n.div_ceil(2) {
            "high".to_string()
        } else {
            "low".to_string()
        };
    }
    labels
}

/// The stratum label for every workspace, e.g. `merges14d=high,kind=rust`.
/// An empty `dims` puts every workspace in the single stratum `all`.
pub fn stratum_labels(inputs: &[WorkspaceInput], dims: &[String]) -> Result<Vec<String>> {
    if dims.is_empty() {
        return Ok(vec!["all".to_string(); inputs.len()]);
    }
    let mut per_dim: Vec<Vec<String>> = Vec::new();
    for dim in dims {
        match dim.as_str() {
            "merges14d" => per_dim.push(merges_labels(inputs)),
            "kind" => per_dim.push(inputs.iter().map(|i| i.kind.clone()).collect()),
            other => bail!(
                "unknown --stratify dimension {other:?} (known: {})",
                STRATIFY_DIMENSIONS.join(", ")
            ),
        }
    }
    Ok((0..inputs.len())
        .map(|i| {
            dims.iter()
                .zip(per_dim.iter())
                .map(|(dim, labels)| format!("{dim}={}", labels[i]))
                .collect::<Vec<_>>()
                .join(",")
        })
        .collect())
}

/// Build the plan. Pure: same `(inputs, arms, dims, seed, now)` ⇒ byte-identical
/// document.
///
/// Within each stratum the members are ordered by [`shuffle_key`] (ties broken
/// by path) and dealt round-robin into the arms. The deal counter **continues
/// across strata** (strata themselves are visited in [`BTreeMap`] order, so
/// that too is deterministic) rather than restarting in each one: restarting
/// balances each stratum but lets the leftover member of every odd-sized
/// stratum land on the same arm, which is exactly how a fleet of mostly
/// singleton strata ends up lopsided. Continuing the counter bounds arm sizes
/// to differ by at most one both *within* each stratum and *overall*. The
/// counter's starting offset is seed-derived, so a different seed does not hand
/// the odd workspace to the same arm.
pub fn build_plan(
    inputs: &[WorkspaceInput],
    arms: &[String],
    dims: &[String],
    seed: u64,
    now: DateTime<Utc>,
) -> Result<Plan> {
    if arms.len() < 2 {
        bail!("--arms needs at least two arms (got {})", arms.len());
    }
    if inputs.is_empty() {
        bail!("no workspaces to assign — is the workspace registry empty?");
    }
    let strata = stratum_labels(inputs, dims)?;

    let mut groups: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, stratum) in strata.iter().enumerate() {
        groups.entry(stratum.as_str()).or_default().push(i);
    }

    let mut assigned: Vec<Option<&str>> = vec![None; inputs.len()];
    let mut cursor = (shuffle_key(seed, "\u{1}deal") % arms.len() as u64) as usize;
    for members in groups.values_mut() {
        members.sort_by_key(|&i| {
            (shuffle_key(seed, &inputs[i].path.to_string_lossy()), inputs[i].path.clone())
        });
        for &idx in members.iter() {
            assigned[idx] = Some(arms[cursor % arms.len()].as_str());
            cursor += 1;
        }
    }

    let workspaces: Vec<PlanWorkspace> = inputs
        .iter()
        .enumerate()
        .map(|(i, input)| PlanWorkspace {
            path: input.path.to_string_lossy().into_owned(),
            repo: input.repo.clone(),
            arm: assigned[i].unwrap_or_default().to_string(),
            stratum: strata[i].clone(),
        })
        .collect();

    let experiment_id = experiment_id(seed, arms, dims, &workspaces, now);
    Ok(Plan {
        experiment_id,
        created_at: now.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        seed,
        arms: arms.to_vec(),
        stratify: dims.to_vec(),
        workspaces,
    })
}

/// `exp-<YYYYMMDD>-<hash8>`. The date makes two experiments a month apart
/// distinguishable in `~/.loom/experiments/`; the hash makes two *different*
/// assignments on the same day distinguishable from each other.
fn experiment_id(
    seed: u64,
    arms: &[String],
    dims: &[String],
    workspaces: &[PlanWorkspace],
    now: DateTime<Utc>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(seed.to_string().as_bytes());
    hasher.update([0x1e]);
    hasher.update(arms.join(",").as_bytes());
    hasher.update([0x1e]);
    hasher.update(dims.join(",").as_bytes());
    for w in workspaces {
        hasher.update([0x1e]);
        hasher.update(w.path.as_bytes());
        hasher.update([0x1f]);
        hasher.update(w.arm.as_bytes());
        hasher.update([0x1f]);
        hasher.update(w.stratum.as_bytes());
    }
    let digest = hex::encode(hasher.finalize());
    format!("exp-{}-{}", now.format("%Y%m%d"), &digest[..8])
}

/// Render the plan as the operator-facing table `plan` prints.
#[must_use]
pub fn format_plan_table(plan: &Plan) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "experiment {} (seed {}, arms {}, stratify {})\n",
        plan.experiment_id,
        plan.seed,
        plan.arms.join(","),
        if plan.stratify.is_empty() {
            "none".to_string()
        } else {
            plan.stratify.join(",")
        }
    ));
    let repo_w = plan
        .workspaces
        .iter()
        .map(|w| w.repo.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let arm_w = plan
        .workspaces
        .iter()
        .map(|w| w.arm.len())
        .max()
        .unwrap_or(3)
        .max(3);
    out.push_str(&format!("{:repo_w$}  {:arm_w$}  {}\n", "REPO", "ARM", "STRATUM"));
    for w in &plan.workspaces {
        out.push_str(&format!("{:repo_w$}  {:arm_w$}  {}\n", w.repo, w.arm, w.stratum));
    }
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for w in &plan.workspaces {
        *counts.entry(w.arm.as_str()).or_default() += 1;
    }
    out.push_str(&format!(
        "\n{} workspaces: {}\n",
        plan.workspaces.len(),
        counts
            .iter()
            .map(|(a, n)| format!("{a}={n}"))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    out
}
