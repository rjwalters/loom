//! The coverage report (Issue #9777 AC3).
//!
//! Four axes, never collapsed into one "supported?" column:
//!
//! 1. **platform support** — does the provider document/expose the operation?
//! 2. **adapter coverage** — has Loom implemented a normalized operation for it?
//! 3. **caller integration** — does the production caller actually route
//!    through that adapter, or is the only evidence a thin probe driver?
//! 4. **unknowns** — what is still unestablished, verbatim.
//!
//! A GO/NO-GO that reads one merged column cannot tell a Gitea platform
//! limitation from unfinished Loom integration, which is exactly the
//! distinction #9769's decision gate (#9792) is required to make. Keeping the
//! four apart in the data structure is the only way the rendering cannot lose
//! it.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::forge_inventory::model::{Coverage, Inventory, Operation, Profile, Support};

/// One row of the report.
#[derive(Debug, Clone, Serialize)]
pub struct Row {
    pub id: String,
    pub group: String,
    pub profile: &'static str,
    pub disposition: &'static str,
    pub risk: &'static str,
    pub platform_support: &'static str,
    pub adapter_coverage: &'static str,
    pub caller_integration: &'static str,
    pub test_id: Option<String>,
    pub test_status: &'static str,
    pub owner: String,
    pub unknowns: Vec<String>,
    /// `true` when the only recorded evidence is probe-side: the provider may
    /// support it and an adapter may exist, but no production caller uses it.
    pub probe_only_evidence: bool,
}

/// Per-group rollup.
#[derive(Debug, Clone, Default, Serialize)]
pub struct GroupRollup {
    pub operations: usize,
    pub required: usize,
    pub platform_unknown: usize,
    pub adapter_none: usize,
    pub caller_integration_none: usize,
    pub open_unknowns: usize,
}

/// The whole report.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub inventory_source: String,
    pub base_sha: String,
    pub providers: BTreeMap<String, String>,
    pub rows: Vec<Row>,
    pub groups: BTreeMap<String, GroupRollup>,
    pub profiles: BTreeMap<String, usize>,
    /// Groups named by #9769 with no active mapped operation — the omissions
    /// #9777 AC1 requires the report to state rather than silently omit.
    pub uncovered_groups: Vec<String>,
    /// High-risk cases with no seeded operation.
    pub unseeded_high_risk_cases: Vec<String>,
}

fn row_for(op: &Operation) -> Row {
    Row {
        id: op.id.clone(),
        group: op.group.clone(),
        profile: op.profile.as_str(),
        disposition: op.disposition.as_str(),
        risk: match op.risk {
            crate::forge_inventory::model::Risk::High => "high",
            crate::forge_inventory::model::Risk::Normal => "normal",
        },
        platform_support: op.platform_support.as_str(),
        adapter_coverage: op.adapter_coverage.as_str(),
        caller_integration: op.caller_integration.as_str(),
        test_id: op.test_id.clone(),
        test_status: op.test_status.as_str(),
        owner: op.owner.clone(),
        unknowns: op.unknowns.clone(),
        probe_only_evidence: op.platform_support != Support::Unknown
            && op.caller_integration == Coverage::None,
    }
}

/// Build the report from an inventory.
#[must_use]
pub fn build(inv: &Inventory) -> Report {
    let mut rows = Vec::new();
    let mut groups: BTreeMap<String, GroupRollup> = BTreeMap::new();
    let mut profiles: BTreeMap<String, usize> = BTreeMap::new();

    for op in inv.operations.iter().filter(|o| o.is_active()) {
        rows.push(row_for(op));
        let g = groups.entry(op.group.clone()).or_default();
        g.operations += 1;
        if op.is_required() {
            g.required += 1;
        }
        if op.platform_support == Support::Unknown {
            g.platform_unknown += 1;
        }
        if op.adapter_coverage == Coverage::None {
            g.adapter_none += 1;
        }
        if op.caller_integration == Coverage::None {
            g.caller_integration_none += 1;
        }
        g.open_unknowns += op.unknowns.len();
        *profiles.entry(op.profile.as_str().to_string()).or_default() += 1;
    }

    let covered: std::collections::BTreeSet<&str> = rows.iter().map(|r| r.group.as_str()).collect();
    let uncovered_groups = super::model::INVENTORY_GROUPS
        .iter()
        .filter(|g| !covered.contains(**g))
        .map(|g| (*g).to_string())
        .collect();

    let seeded: std::collections::BTreeSet<&str> = inv
        .operations
        .iter()
        .flat_map(|o| o.high_risk_cases.iter())
        .map(String::as_str)
        .collect();
    let unseeded_high_risk_cases = super::model::SEEDED_HIGH_RISK_CASES
        .iter()
        .filter(|c| !seeded.contains(**c))
        .map(|c| (*c).to_string())
        .collect();

    Report {
        schema_version: inv.header.schema_version,
        inventory_source: inv.header.inventory_source.clone(),
        base_sha: inv.header.base_sha.clone(),
        providers: inv
            .header
            .providers
            .iter()
            .map(|(k, v)| (k.clone(), v.floor.clone()))
            .collect(),
        rows,
        groups,
        profiles,
        uncovered_groups,
        unseeded_high_risk_cases,
    }
}

/// Human-readable rendering. One line per operation with the four axes in
/// fixed columns, then the group rollup and the omissions.
#[must_use]
pub fn render_text(report: &Report) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "forge operation inventory v{} (source {}, base {})\n",
        report.schema_version,
        report.inventory_source,
        &report.base_sha[..report.base_sha.len().min(12)]
    ));
    let providers: Vec<String> = report
        .providers
        .iter()
        .map(|(p, floor)| format!("{p}>={floor}"))
        .collect();
    out.push_str(&format!("  provider floors: {}\n", providers.join(", ")));
    out.push_str(&format!(
        "  {} active operation(s) across {} group(s)\n\n",
        report.rows.len(),
        report.groups.len()
    ));

    out.push_str(&format!(
        "{:<34} {:<22} {:<10} {:<10} {:<11} {:<12}\n",
        "OPERATION", "GROUP", "PLATFORM", "ADAPTER", "CALLER-INT", "TEST"
    ));
    for row in &report.rows {
        out.push_str(&format!(
            "{:<34} {:<22} {:<10} {:<10} {:<11} {:<12}\n",
            row.id,
            truncate(&row.group, 22),
            row.platform_support,
            row.adapter_coverage,
            row.caller_integration,
            if row.test_id.is_some() {
                row.test_status
            } else {
                "MISSING"
            }
        ));
    }

    out.push_str("\nGROUP ROLLUP (required / platform-unknown / no-adapter / no-caller-integration / open-unknowns)\n");
    for (group, r) in &report.groups {
        out.push_str(&format!(
            "  {:<30} {:>3} ops  req {:>2}  plat? {:>2}  adapter- {:>2}  caller- {:>2}  unk {:>2}\n",
            group,
            r.operations,
            r.required,
            r.platform_unknown,
            r.adapter_none,
            r.caller_integration_none,
            r.open_unknowns
        ));
    }

    out.push_str("\nPROFILES\n");
    for (profile, n) in &report.profiles {
        out.push_str(&format!("  {profile:<24} {n:>3} ops\n"));
    }

    out.push_str("\nEVIDENCE CAUTION\n");
    let probe_only = report.rows.iter().filter(|r| r.probe_only_evidence).count();
    out.push_str(&format!(
        "  {probe_only} operation(s) have provider-side evidence but NO production caller integration.\n  \
         A hosted probe calling the provider through a thin driver proves the platform, not Loom.\n"
    ));
    let declared = report
        .rows
        .iter()
        .filter(|r| r.disposition == "required" && r.test_status == "declared")
        .count();
    out.push_str(&format!(
        "  {declared} required operation(s) have a RESERVED test id with nothing running under it yet;\n  \
         a reserved identifier is an open unknown, not coverage.\n"
    ));

    out.push_str("\nOMISSIONS\n");
    if report.uncovered_groups.is_empty() {
        out.push_str("  none — every #9769 inventory group has at least one mapped operation\n");
    } else {
        for group in &report.uncovered_groups {
            out.push_str(&format!("  UNCOVERED GROUP  {group}\n"));
        }
    }
    for case in &report.unseeded_high_risk_cases {
        out.push_str(&format!("  UNSEEDED HIGH-RISK CASE  {case}\n"));
    }

    let with_unknowns: Vec<&Row> = report
        .rows
        .iter()
        .filter(|r| !r.unknowns.is_empty())
        .collect();
    out.push_str(&format!("\nREMAINING UNKNOWNS ({})\n", with_unknowns.len()));
    for row in with_unknowns {
        for unknown in &row.unknowns {
            out.push_str(&format!("  {}: {unknown}\n", row.id));
        }
    }
    out
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n.saturating_sub(1)])
    }
}

/// Rows in one profile, for `report --profile`.
#[must_use]
pub fn filter_profile(inv: &Inventory, profile: Profile) -> Vec<&Operation> {
    inv.in_profile(profile).collect()
}
