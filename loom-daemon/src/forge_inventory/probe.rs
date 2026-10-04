//! The hosted-probe manifest (Issue #9777 AC4).
//!
//! #9769 wants hosted Gitea probes started *immediately*, before any caller
//! migration or fleet move. So the probe manifest is derived purely from the
//! embedded inventory: no forge call, no credential, no daemon, no fleet
//! config. `loom-daemon forge-inventory probe-manifest --profile
//! required-coordination --json` is runnable on a laptop with no network, and
//! its output is the complete work list for a probe harness.
//!
//! # Evidence class is part of the contract, not a footnote
//!
//! Each entry carries the [`EvidenceClass`] a *passing* probe would establish.
//! A probe that calls Gitea directly through a thin driver establishes
//! `platform` — the provider can do it. Only a probe that drives the real Loom
//! caller establishes `caller-integration`. Encoding that per entry stops a
//! green probe sheet from being read as "Loom works on Gitea", which is the
//! single most expensive misreading available at the decision gate.

use serde::Serialize;

use crate::forge_inventory::model::{Coverage, Inventory, Operation, Profile};

/// What a passing probe of this entry would actually establish.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EvidenceClass {
    /// The provider exposes the operation. A thin driver is enough.
    Platform,
    /// A Loom adapter normalizes the operation correctly.
    Adapter,
    /// The production Loom caller routes through the adapter end to end.
    CallerIntegration,
}

impl EvidenceClass {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            EvidenceClass::Platform => "platform",
            EvidenceClass::Adapter => "adapter",
            EvidenceClass::CallerIntegration => "caller-integration",
        }
    }
}

/// One probe work item.
#[derive(Debug, Clone, Serialize)]
pub struct ProbeEntry {
    pub id: String,
    pub group: String,
    pub profile: &'static str,
    pub disposition: &'static str,
    pub class: &'static str,
    pub risk: &'static str,
    pub inputs: Vec<String>,
    pub required_fields: Vec<String>,
    pub permissions: Vec<String>,
    pub pagination: &'static str,
    pub consistency: &'static str,
    pub idempotency: &'static str,
    pub retry: &'static str,
    pub github_routes: Vec<String>,
    pub github_commands: Vec<String>,
    pub version_support: std::collections::BTreeMap<String, String>,
    pub test_id: Option<String>,
    pub owner: String,
    /// The strongest evidence a probe of this entry can currently establish.
    pub establishes: EvidenceClass,
    /// What a pass would NOT establish, spelled out.
    pub does_not_establish: Vec<&'static str>,
    pub unknowns: Vec<String>,
}

/// The emitted manifest.
#[derive(Debug, Clone, Serialize)]
pub struct ProbeManifest {
    pub schema_version: u32,
    pub inventory_source: String,
    pub base_sha: String,
    /// Which profiles this manifest covers.
    pub profiles: Vec<&'static str>,
    pub entries: Vec<ProbeEntry>,
}

fn pagination_str(op: &Operation) -> &'static str {
    use crate::forge_inventory::model::Pagination as P;
    match op.pagination {
        P::NotPaginated => "not-paginated",
        P::CompleteRequired => "complete-required",
        P::FirstPageSufficient => "first-page-sufficient",
    }
}

fn consistency_str(op: &Operation) -> &'static str {
    use crate::forge_inventory::model::Consistency as C;
    match op.consistency {
        C::Eventual => "eventual",
        C::ReadAfterWrite => "read-after-write",
        C::FreshRead => "fresh-read",
    }
}

fn idempotency_str(op: &Operation) -> &'static str {
    use crate::forge_inventory::model::Idempotency as I;
    match op.idempotency {
        I::Idempotent => "idempotent",
        I::NotIdempotent => "not-idempotent",
        I::ExpectedHeadGuarded => "expected-head-guarded",
        I::MarkerDeduped => "marker-deduped",
    }
}

fn retry_str(op: &Operation) -> &'static str {
    use crate::forge_inventory::model::Retry as R;
    match op.retry {
        R::BackoffSafe => "backoff-safe",
        R::RereadThenRetry => "reread-then-retry",
        R::NoAutoRetry => "no-auto-retry",
    }
}

/// The strongest class a probe can establish today, given what Loom has built.
fn establishes(op: &Operation) -> EvidenceClass {
    match (op.adapter_coverage, op.caller_integration) {
        (_, Coverage::Complete) => EvidenceClass::CallerIntegration,
        (Coverage::Complete | Coverage::Partial, _) => EvidenceClass::Adapter,
        _ => EvidenceClass::Platform,
    }
}

fn does_not_establish(class: EvidenceClass) -> Vec<&'static str> {
    match class {
        EvidenceClass::Platform => vec![
            "that a Loom adapter normalizes the response",
            "that any production Loom caller uses it",
        ],
        EvidenceClass::Adapter => {
            vec!["that any production Loom caller routes through the adapter"]
        }
        EvidenceClass::CallerIntegration => vec![],
    }
}

fn entry_for(op: &Operation) -> ProbeEntry {
    let class = establishes(op);
    ProbeEntry {
        id: op.id.clone(),
        group: op.group.clone(),
        profile: op.profile.as_str(),
        disposition: op.disposition.as_str(),
        class: match op.class {
            crate::forge_inventory::model::AccessClass::Read => "read",
            crate::forge_inventory::model::AccessClass::Write => "write",
        },
        risk: match op.risk {
            crate::forge_inventory::model::Risk::High => "high",
            crate::forge_inventory::model::Risk::Normal => "normal",
        },
        inputs: op.inputs.clone(),
        required_fields: op.required_fields.clone(),
        permissions: op.permissions.clone(),
        pagination: pagination_str(op),
        consistency: consistency_str(op),
        idempotency: idempotency_str(op),
        retry: retry_str(op),
        github_routes: op.github.routes.clone(),
        github_commands: op.github.commands.clone(),
        version_support: op.version_support.clone(),
        test_id: op.test_id.clone(),
        owner: op.owner.clone(),
        establishes: class,
        does_not_establish: does_not_establish(class),
        unknowns: op.unknowns.clone(),
    }
}

/// Build a probe manifest for `profiles` (empty = every required profile).
/// High-risk rows come first: #9777 seeds those cases before the easy ones, and
/// a probe harness that runs the list in order inherits that priority for free.
#[must_use]
pub fn build(inv: &Inventory, profiles: &[Profile]) -> ProbeManifest {
    let selected: Vec<Profile> = if profiles.is_empty() {
        vec![
            Profile::RequiredCoordination,
            Profile::RequiredCiLanding,
            Profile::FleetBootstrap,
            Profile::Delivery,
        ]
    } else {
        profiles.to_vec()
    };
    let mut entries: Vec<ProbeEntry> = inv
        .operations
        .iter()
        .filter(|o| o.is_active() && selected.contains(&o.profile))
        .map(entry_for)
        .collect();
    entries.sort_by(|a, b| {
        let rank = |r: &str| if r == "high" { 0 } else { 1 };
        rank(a.risk)
            .cmp(&rank(b.risk))
            .then_with(|| a.id.cmp(&b.id))
    });
    ProbeManifest {
        schema_version: inv.header.schema_version,
        inventory_source: inv.header.inventory_source.clone(),
        base_sha: inv.header.base_sha.clone(),
        profiles: selected.iter().map(|p| p.as_str()).collect(),
        entries,
    }
}
