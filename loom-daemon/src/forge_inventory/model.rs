//! The forge operation inventory's schema (Issue #9777, epic #9769 phase 1).
//!
//! One [`Operation`] row is the unit of accounting: a stable ID, the inventory
//! group it belongs to, what it reads/writes, who may call it, how it paginates
//! and retries, which provider versions support it, the test that proves it,
//! and the owner who answers for it. The manifest is **data** —
//! `defaults/forge/` TOML, embedded with `include_str!` — so the validator,
//! the change gate and the hosted probe emitter all read one source of truth
//! instead of three greps.
//!
//! # Why the three coverage axes are separate fields
//!
//! #9769's qualification question is not one question. "Gitea documents a
//! merge endpoint" (platform support), "Loom has an adapter that calls it"
//! (adapter coverage) and "the production caller actually routes through that
//! adapter" (caller integration) are three independent facts, and a GO/NO-GO
//! that conflates them cannot tell a platform limitation from unfinished Loom
//! integration. So each row carries all three, each defaulting to the
//! pessimistic value, and every report prints them in separate columns.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The inventory groups enumerated in epic #9769's "Enumerated GitHub surface"
/// tables. Every group must have at least one mapped operation — that is what
/// makes "the inventory covers #9769" a checkable claim rather than a promise.
pub const INVENTORY_GROUPS: &[&str] = &[
    // "Workflow and landing"
    "issue-creation-discovery",
    "issue-mutation-lifecycle",
    "label-catalogue-transitions",
    "issue-pr-conversation",
    "timeline-event-history",
    "epics-relationships",
    "pr-creation-discovery-state",
    "diff-commits-checkout",
    "formal-reviews-inline-comments",
    "issue-closing-relationships",
    "ci-readiness",
    "branch-update-guarded-merge",
    "server-side-auto-merge",
    "protection-repository-policy",
    // "Fleet, credentials and delivery"
    "identity-authentication",
    "quota-caching-errors",
    "repository-discovery-creation",
    "git-objects-remote-writes",
    "ci-diagnostics-remediation",
    "releases-distribution",
    "optional-actions-roles-settings",
];

/// The high-risk cases #9777 requires the first manifest to seed. Each must be
/// covered by at least one [`Risk::High`] operation, so "seed the high-risk
/// cases first" cannot be satisfied by an inventory of easy rows.
pub const SEEDED_HIGH_RISK_CASES: &[&str] = &[
    "review-resolution",
    "queued-hidden-ci",
    "expected-head-merge",
    "competing-claims",
    "revoked-permissions",
    "cross-origin-identity",
];

/// Groups whose requirements a profile reduction may never waive (#9777:
/// "Profile reduction cannot quietly waive claims, identity/trust, reviews,
/// CI, protection, guarded merge or mixed-fleet requirements"). An operation
/// in one of these groups may not be dispositioned [`Disposition::Optional`].
pub const NON_WAIVABLE_GROUPS: &[&str] = &[
    "label-catalogue-transitions",
    "formal-reviews-inline-comments",
    "ci-readiness",
    "branch-update-guarded-merge",
    "protection-repository-policy",
    "identity-authentication",
];

/// Which qualification profile an operation belongs to. The first four are
/// required profiles; [`Profile::Optional`] is the only one a deployment may
/// decline, and only with a stated reason and preflight behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Profile {
    /// Claims, labels, issues, comments — the coordination authority itself.
    RequiredCoordination,
    /// CI readiness and the guarded landing path.
    RequiredCiLanding,
    /// Installer, host bootstrap and fleet-config plumbing.
    FleetBootstrap,
    /// Releases, artifacts, container publication, self-update.
    Delivery,
    /// Declined features. Needs `exclusion_reason` + `preflight`.
    Optional,
}

impl Profile {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Profile::RequiredCoordination => "required-coordination",
            Profile::RequiredCiLanding => "required-ci-landing",
            Profile::FleetBootstrap => "fleet-bootstrap",
            Profile::Delivery => "delivery",
            Profile::Optional => "optional",
        }
    }

    /// Is this one of the profiles a qualification run must prove?
    #[must_use]
    pub fn is_required(self) -> bool {
        !matches!(self, Profile::Optional)
    }
}

/// Every inventoried operation needs a disposition — the issue forbids a row
/// with no decision attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Disposition {
    /// Loom needs it; a provider that cannot do it fails qualification.
    Required,
    /// Declined for this qualification. Needs a reason and preflight behavior.
    Optional,
    /// Known-unsupported, recorded explicitly rather than left blank.
    Unsupported,
    /// Mentioned in the tree only as a prohibition (e.g. `gh pr merge`), never
    /// an active operation.
    Prohibition,
    /// Appears only in test fixtures; classified, not counted as active.
    TestFixture,
}

impl Disposition {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Disposition::Required => "required",
            Disposition::Optional => "optional",
            Disposition::Unsupported => "unsupported",
            Disposition::Prohibition => "prohibition",
            Disposition::TestFixture => "test-fixture",
        }
    }
}

/// Read vs write class — what a revoked write permission would break.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AccessClass {
    Read,
    Write,
}

/// Who holds the credential this operation runs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Actor {
    /// The daemon's own forge credential (PAT or App installation token).
    Daemon,
    /// A dispatched agent session's credential.
    Agent,
    /// Install-time operator credential.
    Installer,
    /// A CI job's workflow token.
    CiToken,
    /// Only a human operator may perform it.
    Operator,
}

/// Pagination semantics — a first-page read where completeness is required is
/// the #9769 "queued workflows with no check runs" failure mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Pagination {
    /// Single-object response; pagination does not apply.
    NotPaginated,
    /// Every page must be read; a truncated read is a wrong answer.
    CompleteRequired,
    /// The first page is a sufficient answer for this caller.
    FirstPageSufficient,
}

/// What freshness the caller's decision depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Consistency {
    /// Any cached/eventually-consistent answer is fine.
    Eventual,
    /// Must observe this process's own prior write.
    ReadAfterWrite,
    /// Must be a fresh read at decision time; a cached answer is unsafe.
    FreshRead,
}

/// Retry/idempotency rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Idempotency {
    /// Safe to repeat; the second call is a no-op or returns the same state.
    Idempotent,
    /// Repeating it duplicates an effect (a second comment, a second issue).
    NotIdempotent,
    /// Guarded by an expected head/SHA precondition, so a stale repeat fails
    /// closed instead of acting on a tree that moved.
    ExpectedHeadGuarded,
    /// Made idempotent by a marker the caller searches for first.
    MarkerDeduped,
}

/// Whether a failed call may be retried automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Retry {
    /// Retry with backoff.
    BackoffSafe,
    /// Retry only after re-reading state (the precondition may have moved).
    RereadThenRetry,
    /// Never retry automatically; surface the failure.
    NoAutoRetry,
}

/// Risk banding — [`Risk::High`] rows are the ones #9777 wants seeded first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Risk {
    High,
    Normal,
}

/// Does the *provider* document/expose this operation at all?
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Support {
    /// Not yet established. The pessimistic default.
    #[default]
    Unknown,
    /// Documented by the provider but not yet exercised.
    Documented,
    /// Exercised against a live instance by a probe.
    Probed,
    /// Established as absent.
    Unsupported,
}

impl Support {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Support::Unknown => "unknown",
            Support::Documented => "documented",
            Support::Probed => "probed",
            Support::Unsupported => "unsupported",
        }
    }
}

/// How much of this operation Loom itself implements / routes through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Coverage {
    /// Nothing yet. The pessimistic default.
    #[default]
    None,
    Partial,
    Complete,
}

impl Coverage {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Coverage::None => "none",
            Coverage::Partial => "partial",
            Coverage::Complete => "complete",
        }
    }
}

/// What a preflight must do when an optional/unsupported operation is enabled
/// anyway. #9769: "enabling an unsupported required feature must fail
/// preflight".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Preflight {
    /// Refuse to start.
    FailClosed,
    /// Warn and continue with the feature disabled.
    WarnAndDisable,
    /// Nothing to check.
    NotApplicable,
}

/// How real the operation's test evidence is **today**.
///
/// Most rows start [`TestStatus::Declared`]: the ID is reserved for the hosted
/// probe harness (#9769 phase 2+) but nothing runs under it yet. That is an
/// honest state, and the report counts it as an open unknown rather than
/// letting a reserved identifier read as coverage. A row may only claim
/// [`TestStatus::Implemented`] when `test_path` names a file in this tree that
/// actually contains the test — which `validate_test_evidence` checks against
/// the filesystem, so the stronger claim cannot be made for free.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TestStatus {
    /// Identifier reserved; no test runs under it yet.
    #[default]
    Declared,
    /// A test exists in this tree at `test_path`.
    Implemented,
}

impl TestStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            TestStatus::Declared => "declared",
            TestStatus::Implemented => "implemented",
        }
    }
}

/// What kind of surface a caller is. Role prompts are *instructions*, not
/// calls, so they are inventoried but never gated as source call sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CallerKind {
    /// Rust inside `loom-daemon`.
    Rust,
    /// A maintained shell script under `defaults/scripts/` or `scripts/`.
    Script,
    /// A canonical role prompt (generated copies derive from their source).
    RolePrompt,
    /// An installer script.
    Installer,
    /// A GitHub Actions workflow.
    Workflow,
    /// A dashboard/UI deep link.
    DashboardLink,
    /// A `git` helper rather than a forge API call.
    GitHelper,
    /// A release/self-update path.
    Release,
}

/// One declared caller of an operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Caller {
    pub kind: CallerKind,
    /// Repo-relative path.
    pub path: String,
    /// Optional symbol / function / step name within the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// `true` when the command line is assembled at runtime rather than being
    /// a literal — #9777 explicitly wants dynamic construction counted.
    #[serde(default)]
    pub dynamic: bool,
}

/// The provider-side surface an operation maps onto today (GitHub) — the
/// commands observed plus the REST/GraphQL routes behind them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderSurface {
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default)]
    pub routes: Vec<String>,
}

/// One inventoried forge operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Operation {
    /// Stable operation ID: lowercase dotted segments, e.g. `issue.create`.
    /// Never renamed once published — reports, probes and tests key on it.
    pub id: String,
    /// One of [`INVENTORY_GROUPS`].
    pub group: String,
    /// One-line description of what Loom uses it for.
    pub summary: String,
    pub profile: Profile,
    pub disposition: Disposition,
    pub class: AccessClass,
    #[serde(default)]
    pub inputs: Vec<String>,
    #[serde(default)]
    pub outputs: Vec<String>,
    /// Normalized fields a caller cannot work without.
    #[serde(default)]
    pub required_fields: Vec<String>,
    pub actor: Actor,
    #[serde(default)]
    pub permissions: Vec<String>,
    pub pagination: Pagination,
    pub consistency: Consistency,
    pub idempotency: Idempotency,
    pub retry: Retry,
    /// provider -> minimum version/edition known to support it.
    #[serde(default)]
    pub version_support: BTreeMap<String, String>,
    /// The identifier the test/probe proving this operation reports under.
    /// Required for every required row, and unique across the inventory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_id: Option<String>,
    /// Whether anything runs under `test_id` yet.
    #[serde(default)]
    pub test_status: TestStatus,
    /// Repo-relative file holding that test. Required — and checked against
    /// the filesystem — when `test_status = "implemented"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_path: Option<String>,
    /// Implementation owner — an area name from the manifest's owner table.
    pub owner: String,
    #[serde(default = "default_risk")]
    pub risk: Risk,
    /// Which of [`SEEDED_HIGH_RISK_CASES`] this row covers.
    #[serde(default)]
    pub high_risk_cases: Vec<String>,
    #[serde(default)]
    pub callers: Vec<Caller>,
    #[serde(default)]
    pub github: ProviderSurface,
    #[serde(default)]
    pub platform_support: Support,
    #[serde(default)]
    pub adapter_coverage: Coverage,
    #[serde(default)]
    pub caller_integration: Coverage,
    /// Required when `disposition = optional`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclusion_reason: Option<String>,
    /// Operation IDs that still carry this row's *requirement* when this row
    /// itself is declined (`optional`) or absent (`unsupported`).
    ///
    /// This is what makes a waiver **loud** instead of quiet. #9777 forbids a
    /// profile reduction from waiving a claims / identity-trust / review / CI /
    /// protection / guarded-merge requirement, but it does not forbid declining
    /// one *convenience operation* inside such a group — Loom rebases locally,
    /// so a server-side `update-branch` endpoint is optional while the guarded
    /// merge it sits next to is not. The difference between those two cases is
    /// exactly "is some required row still carrying the requirement?", so the
    /// manifest records the answer as data and
    /// [`crate::forge_inventory::validate`] checks it: each named ID must exist
    /// and must itself be a `required` row. Prose in `exclusion_reason` cannot
    /// be checked; this can.
    #[serde(default)]
    pub requirement_carried_by: Vec<String>,
    /// Required when `disposition = optional` or `unsupported`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preflight: Option<Preflight>,
    /// Free-text remaining unknowns, surfaced verbatim in reports.
    #[serde(default)]
    pub unknowns: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

fn default_risk() -> Risk {
    Risk::Normal
}

impl Operation {
    /// Does this row count as an active operation a qualification must prove?
    /// Prohibitions and test fixtures are classified, never counted (#9777).
    #[must_use]
    pub fn is_active(&self) -> bool {
        matches!(
            self.disposition,
            Disposition::Required | Disposition::Optional | Disposition::Unsupported
        )
    }

    /// Required rows are the ones the coverage validator holds to the full bar.
    #[must_use]
    pub fn is_required(&self) -> bool {
        self.disposition == Disposition::Required
    }
}

/// A provider Loom may qualify against, plus the version floor below which a
/// `version_support` pin counts as **stale**.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderFloor {
    /// Lowest version this qualification accepts evidence for.
    pub floor: String,
    /// What the qualification is actually targeting, for the report header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

/// The manifest header (`defaults/forge/manifest.toml`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestHeader {
    pub schema_version: u32,
    /// Where the inventory came from, e.g. `rjwalters/loom#9769`.
    pub inventory_source: String,
    /// The commit the source enumeration was verified against.
    pub base_sha: String,
    /// provider -> version floor.
    pub providers: BTreeMap<String, ProviderFloor>,
    /// Owner-area name -> who answers for it.
    pub owners: BTreeMap<String, String>,
}

/// One `defaults/forge/operations/*.toml` file.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OperationFile {
    #[serde(default)]
    pub operation: Vec<Operation>,
}

/// The whole inventory: header plus every operation row, in file order.
#[derive(Debug, Clone)]
pub struct Inventory {
    pub header: ManifestHeader,
    pub operations: Vec<Operation>,
}

impl Inventory {
    /// Rows in `profile`, active only.
    pub fn in_profile(&self, profile: Profile) -> impl Iterator<Item = &Operation> {
        self.operations
            .iter()
            .filter(move |o| o.profile == profile && o.is_active())
    }

    /// Look one row up by its stable ID.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Operation> {
        self.operations.iter().find(|o| o.id == id)
    }

    /// Every distinct repo-relative caller path the manifest declares, across
    /// every operation. This is the set the change gate treats as classified.
    #[must_use]
    pub fn declared_caller_paths(&self) -> std::collections::BTreeSet<String> {
        self.operations
            .iter()
            .flat_map(|o| o.callers.iter())
            .map(|c| c.path.clone())
            .collect()
    }
}

/// Parse a dotted version string into comparable numeric components. Non-numeric
/// trailers (`1.24.0-rc1`) compare by their numeric prefix only.
#[must_use]
pub fn version_key(v: &str) -> Vec<u64> {
    v.split(['.', '-', '+'])
        .map(|part| {
            part.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse()
                .unwrap_or(0)
        })
        .collect()
}

/// Is `pinned` below `floor`? Used by the validator's stale-version rule.
#[must_use]
pub fn is_stale_version(pinned: &str, floor: &str) -> bool {
    let (a, b) = (version_key(pinned), version_key(floor));
    let n = a.len().max(b.len());
    for i in 0..n {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x < y;
        }
    }
    false
}
