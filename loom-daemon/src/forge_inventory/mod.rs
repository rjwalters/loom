//! Versioned forge **operation inventory** and its coverage accounting
//! (Issue #9777, phase 1 of epic #9769).
//!
//! #9769 enumerated the GitHub surface Loom consumes as prose tables. Prose
//! cannot fail a build, so it cannot answer the qualification question the
//! epic's decision gate asks: *which* operations does a replacement forge have
//! to satisfy, who owns each one, what proves it, and what is still unknown.
//! This module turns that enumeration into data plus three mechanisms over it:
//!
//! | Mechanism | Entry point | Answers |
//! |---|---|---|
//! | Coverage validator | `forge-inventory validate` | Is the manifest itself well-formed and complete? |
//! | Change gate | `forge-inventory gate` | Did a change introduce an **unclassified** direct forge call? |
//! | Probe manifest | `forge-inventory probe-manifest` | What must a hosted probe exercise, and with what semantics? |
//! | Coverage report | `forge-inventory report` | Platform support vs adapter coverage vs caller integration vs unknowns |
//!
//! # The manifest is embedded, not discovered
//!
//! `defaults/forge/manifest.toml` plus `defaults/forge/operations/*.toml` are
//! `include_str!`'d into the binary, exactly like
//! [`crate::runtime_admission`]'s runtime manifests (#5002). A validator that
//! has to find its own input on disk is a validator that silently passes on a
//! checkout where the input moved; embedding makes "the manifest is missing" a
//! compile error instead. [`load_from_dir`] exists only so tests can validate
//! a *synthetic* manifest — the shipped one is always the embedded copy.
//!
//! # Where the gates actually run
//!
//! Both gates run as **Rust tests** over this checkout
//! ([`tests`]: `the_embedded_manifest_is_publishable` and
//! `the_repository_has_no_unclassified_direct_forge_call`), not as a
//! `# component:` step in `.github/workflows/ci.yml`. That is a deliberate
//! choice, not an omission:
//!
//! - The CLI verbs and the tests call the *same* functions, so there is one
//!   mechanism per behaviour — the command is for operators and for
//!   `gate --update`, the test is the pre-merge gate.
//! - Registering a new required `Daemon Checks` component means giving it a
//!   `CheckSpec` in [`crate::merge_pr::stale_checks::inputs`]. This gate's
//!   `scanned` set is *every tracked `.sh`/`.rs`/workflow file*, so its spec
//!   would make `Daemon Checks` stale on nearly every base move — directly
//!   worsening the stale-check remedy exhaustion that already holds PRs
//!   (#8248/#8508). Registering it needs that consequence designed, not
//!   inherited; see the follow-up issue the PR names.
//!
//! **The test form does not escape that hazard, it only costs less.** The test
//! runs over the *merged* tree too, so a base move that adds an undeclared,
//! unbaselined caller fails this PR's build for a file this PR never touched —
//! which is exactly how this slice's own first review found six unclassified
//! entries in CI against two on the branch (PR #9832). The difference is the
//! remedy, not the exposure: a failing test is fixed by rebasing and re-running
//! `gate --update`, which is ordinary Doctor work, whereas a stale required
//! *check* consumes the re-date remedy budget and can terminate in an operator
//! hold. Regenerating the baseline is therefore a named step of the rebase
//! recipe, not a surprise; softening a baseline miss on a file outside the PR's
//! own diff into guidance rather than a hard failure belongs to the
//! registration follow-up, where the `CheckSpec` decision is made.
//!
//! # What this phase deliberately does NOT do
//!
//! It does not migrate callers. The change gate lands with a **baseline** of
//! every file that already makes a direct forge call (owner + removal issue
//! per entry) so the ratchet can start catching *new* bypasses immediately;
//! eliminating the baselined ones is the caller-migration issue's job. Nothing
//! here makes a forge call, so none of it can consume quota or need a token.

pub mod gate;
pub mod model;
pub mod probe;
pub mod report;
pub mod validate;

use std::path::Path;

use anyhow::{Context, Result};

pub use model::{
    AccessClass, Actor, Caller, CallerKind, Consistency, Coverage, Disposition, Idempotency,
    Inventory, ManifestHeader, Operation, OperationFile, Pagination, Preflight, Profile,
    ProviderFloor, ProviderSurface, Retry, Risk, Support, INVENTORY_GROUPS, NON_WAIVABLE_GROUPS,
    SEEDED_HIGH_RISK_CASES,
};

/// The embedded manifest header.
const EMBEDDED_HEADER: &str = include_str!("../../../defaults/forge/manifest.toml");

/// The embedded operation files, in the order they are concatenated. Adding a
/// group file here is the only wiring a new batch of rows needs.
const EMBEDDED_OPERATIONS: &[(&str, &str)] = &[
    (
        "coordination.toml",
        include_str!("../../../defaults/forge/operations/coordination.toml"),
    ),
    (
        "pull-requests.toml",
        include_str!("../../../defaults/forge/operations/pull-requests.toml"),
    ),
    (
        "ci-landing.toml",
        include_str!("../../../defaults/forge/operations/ci-landing.toml"),
    ),
    (
        "fleet-delivery.toml",
        include_str!("../../../defaults/forge/operations/fleet-delivery.toml"),
    ),
];

/// The embedded change-gate bypass baseline.
const EMBEDDED_BASELINE: &str = include_str!("../../../defaults/forge/call-bypass-baseline.toml");

/// Repo-relative path of the baseline, for `--update` and error messages.
pub const BASELINE_PATH: &str = "defaults/forge/call-bypass-baseline.toml";

/// The inventory compiled into this binary. Infallible in practice — a
/// malformed embedded manifest is caught by this module's own tests, which
/// parse exactly this constant.
pub fn load_embedded() -> Result<Inventory> {
    let header: ManifestHeader =
        toml::from_str(EMBEDDED_HEADER).context("parsing embedded defaults/forge/manifest.toml")?;
    let mut operations = Vec::new();
    for (name, text) in EMBEDDED_OPERATIONS {
        let file: OperationFile = toml::from_str(text)
            .with_context(|| format!("parsing embedded defaults/forge/operations/{name}"))?;
        operations.extend(file.operation);
    }
    Ok(Inventory { header, operations })
}

/// The embedded bypass baseline.
pub fn load_embedded_baseline() -> Result<gate::Baseline> {
    toml::from_str(EMBEDDED_BASELINE).with_context(|| format!("parsing embedded {BASELINE_PATH}"))
}

/// Load an inventory from a directory shaped like `defaults/forge/`
/// (`manifest.toml` + `operations/*.toml`). Test-facing: lets a test feed the
/// validator a deliberately broken manifest without touching the shipped one.
pub fn load_from_dir(dir: &Path) -> Result<Inventory> {
    let header_path = dir.join("manifest.toml");
    let header: ManifestHeader = toml::from_str(
        &std::fs::read_to_string(&header_path)
            .with_context(|| format!("reading {}", header_path.display()))?,
    )
    .with_context(|| format!("parsing {}", header_path.display()))?;

    let ops_dir = dir.join("operations");
    let mut files: Vec<_> = std::fs::read_dir(&ops_dir)
        .with_context(|| format!("reading {}", ops_dir.display()))?
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .collect();
    files.sort();

    let mut operations = Vec::new();
    for path in files {
        let file: OperationFile = toml::from_str(
            &std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?,
        )
        .with_context(|| format!("parsing {}", path.display()))?;
        operations.extend(file.operation);
    }
    Ok(Inventory { header, operations })
}

#[cfg(test)]
#[path = "tests/mod.rs"]
mod tests;
