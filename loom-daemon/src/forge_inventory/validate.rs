//! The coverage validator (Issue #9777).
//!
//! Rejects a manifest that would let a qualification claim rest on nothing:
//! duplicate IDs, a required operation with no test, an inventory group from
//! #9769 with no mapped rows, a `version_support` pin below the provider floor
//! (a **stale version**), an optional exclusion with no stated reason or
//! preflight, and a non-waivable group's requirement waived with nothing named
//! to carry it.
//!
//! Every rule returns a [`Finding`] naming the operation and what to do, not a
//! bare boolean — a gate whose failure message is "invalid manifest" costs a
//! reviewer the same time it saves.
//!
//! # Two rule sets, because "unknown" is phase 1's correct answer
//!
//! There are two different questions to ask of this manifest and they must not
//! share a gate:
//!
//! | Function | Question | Runs |
//! |---|---|---|
//! | [`validate`] | Is the manifest **well-formed and honest**? | Every CI run |
//! | [`validate_qualification`] | Is the evidence **complete enough to pass a provider**? | At #9769's GO/NO-GO (#9792) |
//!
//! Phase 1's whole job is to record what is *not* yet established — a hosted
//! probe has not run. A single gate that failed on `platform_support =
//! "unknown"` would therefore force every row to assert support it has no
//! evidence for, turning the accounting into exactly the unverified prose
//! #9769 set out to replace. So [`validate`] requires an unknown to be
//! **acknowledged** (the row must say in `unknowns` what is unestablished),
//! and [`validate_qualification`] is the gate that refuses to count an
//! acknowledged unknown, a reserved-but-unimplemented test, or probe-only
//! evidence as a pass. #9777's "unknown/skipped required rows are not a pass"
//! lands there, where it decides something, rather than here, where it would
//! only buy a false claim.

use std::collections::{BTreeMap, BTreeSet};

use crate::forge_inventory::model::{
    is_stale_version, Coverage, Disposition, Inventory, Operation, Profile, Support, TestStatus,
    INVENTORY_GROUPS, NON_WAIVABLE_GROUPS, SEEDED_HIGH_RISK_CASES,
};

/// Stable rule identifier, so a failure can be cited and suppressed-by-fix
/// rather than by guesswork.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rule {
    DuplicateId,
    MalformedId,
    UnknownGroup,
    UncoveredGroup,
    MissingTest,
    DuplicateTestId,
    MalformedTestId,
    TestEvidenceMissing,
    UnacknowledgedUnknownSupport,
    MissingOwner,
    UnknownOwnerArea,
    StaleVersion,
    UnknownProvider,
    MissingExclusionReason,
    MissingPreflight,
    QuietlyWaivedRequirement,
    UnknownRequirementCarrier,
    GroupFullyWaived,
    RequiredInOptionalProfile,
    IntegrationWithoutCaller,
    UnseededHighRiskCase,
    HighRiskWithoutCase,
    // ---- qualification-only rules (`validate_qualification`) ----
    UnresolvedRequiredSupport,
    UnprovenRequiredTest,
    UnintegratedRequiredCaller,
}

impl Rule {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Rule::DuplicateId => "duplicate-id",
            Rule::MalformedId => "malformed-id",
            Rule::UnknownGroup => "unknown-group",
            Rule::UncoveredGroup => "uncovered-group",
            Rule::MissingTest => "missing-test",
            Rule::DuplicateTestId => "duplicate-test-id",
            Rule::MalformedTestId => "malformed-test-id",
            Rule::TestEvidenceMissing => "test-evidence-missing",
            Rule::UnacknowledgedUnknownSupport => "unacknowledged-unknown-support",
            Rule::MissingOwner => "missing-owner",
            Rule::UnknownOwnerArea => "unknown-owner-area",
            Rule::StaleVersion => "stale-version",
            Rule::UnknownProvider => "unknown-provider",
            Rule::MissingExclusionReason => "missing-exclusion-reason",
            Rule::MissingPreflight => "missing-preflight",
            Rule::QuietlyWaivedRequirement => "quietly-waived-requirement",
            Rule::UnknownRequirementCarrier => "unknown-requirement-carrier",
            Rule::GroupFullyWaived => "group-fully-waived",
            Rule::RequiredInOptionalProfile => "required-in-optional-profile",
            Rule::IntegrationWithoutCaller => "integration-without-caller",
            Rule::UnseededHighRiskCase => "unseeded-high-risk-case",
            Rule::HighRiskWithoutCase => "high-risk-without-case",
            Rule::UnresolvedRequiredSupport => "unresolved-required-support",
            Rule::UnprovenRequiredTest => "unproven-required-test",
            Rule::UnintegratedRequiredCaller => "unintegrated-required-caller",
        }
    }
}

/// One validation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub rule: Rule,
    /// Operation ID, or the group/provider name for manifest-level rules.
    pub subject: String,
    pub detail: String,
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {} — {}", self.rule.as_str(), self.subject, self.detail)
    }
}

fn finding(rule: Rule, subject: &str, detail: impl Into<String>) -> Finding {
    Finding {
        rule,
        subject: subject.to_string(),
        detail: detail.into(),
    }
}

/// A stable operation ID is lowercase dotted segments of `[a-z0-9-]`, at least
/// two segments deep (`issue.create`, never a bare `issue`).
fn id_is_well_formed(id: &str) -> bool {
    let segments: Vec<&str> = id.split('.').collect();
    segments.len() >= 2
        && segments.iter().all(|s| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                && !s.starts_with('-')
                && !s.ends_with('-')
        })
}

/// A test identifier is `::`-separated lowercase segments, at least two deep.
fn test_id_is_well_formed(id: &str) -> bool {
    let segments: Vec<&str> = id.split("::").collect();
    segments.len() >= 2
        && segments.iter().all(|s| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        })
}

/// Filesystem-backed half of the test-evidence rule: a row claiming
/// `test_status = "implemented"` must name a `test_path` that exists under
/// `root` **and** contains its `test_id`'s last segment. Split from
/// [`validate`] because that function is pure — it runs against a manifest
/// with no repo in sight (tests, `--manifest-dir`), and a pure rule set is
/// what lets those callers exist.
#[must_use]
pub fn validate_test_evidence(inv: &Inventory, root: &std::path::Path) -> Vec<Finding> {
    let mut out = Vec::new();
    for op in &inv.operations {
        if op.test_status != TestStatus::Implemented {
            continue;
        }
        let Some(rel) = op.test_path.as_deref().filter(|p| !p.is_empty()) else {
            continue; // already reported by `validate`'s pure half
        };
        let path = root.join(rel);
        let Ok(text) = std::fs::read_to_string(&path) else {
            out.push(finding(
                Rule::TestEvidenceMissing,
                &op.id,
                format!("test_path `{rel}` does not exist (or is unreadable) — an `implemented` claim must point at a real file"),
            ));
            continue;
        };
        let needle = op
            .test_id
            .as_deref()
            .unwrap_or_default()
            .rsplit("::")
            .next()
            .unwrap_or_default()
            .replace('-', "_");
        if !needle.is_empty() && !text.contains(&needle) {
            out.push(finding(
                Rule::TestEvidenceMissing,
                &op.id,
                format!("test_path `{rel}` does not mention `{needle}` — the named test is not in that file"),
            ));
        }
    }
    out.sort_by(|a, b| (a.rule, &a.subject).cmp(&(b.rule, &b.subject)));
    out
}

/// Run every rule. An empty result means the manifest is publishable.
#[must_use]
pub fn validate(inv: &Inventory) -> Vec<Finding> {
    let mut out = Vec::new();
    check_ids(inv, &mut out);
    check_groups(inv, &mut out);
    check_required_rows(inv, &mut out);
    check_dispositions(inv, &mut out);
    check_versions(inv, &mut out);
    check_owners(inv, &mut out);
    check_high_risk(inv, &mut out);
    out.sort_by(|a, b| (a.rule, &a.subject).cmp(&(b.rule, &b.subject)));
    out
}

fn check_ids(inv: &Inventory, out: &mut Vec<Finding>) {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut seen_tests: BTreeMap<&str, &str> = BTreeMap::new();
    for op in &inv.operations {
        if !id_is_well_formed(&op.id) {
            out.push(finding(
                Rule::MalformedId,
                &op.id,
                "operation IDs are lowercase dotted segments of [a-z0-9-], at least two deep (e.g. `issue.create`)",
            ));
        }
        if !seen.insert(&op.id) {
            out.push(finding(
                Rule::DuplicateId,
                &op.id,
                "declared more than once; operation IDs are the stable key every report, probe and test uses",
            ));
        }
        if let Some(test_id) = op.test_id.as_deref() {
            if let Some(prev) = seen_tests.insert(test_id, &op.id) {
                out.push(finding(
                    Rule::DuplicateTestId,
                    &op.id,
                    format!("test_id `{test_id}` is already claimed by `{prev}`; one test cannot be the proof for two operations"),
                ));
            }
            if !test_id_is_well_formed(test_id) {
                out.push(finding(
                    Rule::MalformedTestId,
                    &op.id,
                    format!("test_id `{test_id}` must be `::`-separated lowercase segments of [a-z0-9_-], at least two deep (e.g. `forge-probe::coordination::issue-create`)"),
                ));
            }
        }
        if op.test_status == TestStatus::Implemented
            && op.test_path.as_deref().is_none_or(str::is_empty)
        {
            out.push(finding(
                Rule::TestEvidenceMissing,
                &op.id,
                "`test_status = implemented` requires `test_path` naming the file that holds the test",
            ));
        }
    }
}

fn check_groups(inv: &Inventory, out: &mut Vec<Finding>) {
    let known: BTreeSet<&str> = INVENTORY_GROUPS.iter().copied().collect();
    let mut covered: BTreeSet<&str> = BTreeSet::new();
    for op in &inv.operations {
        if known.contains(op.group.as_str()) {
            if op.is_active() {
                covered.insert(op.group.as_str());
            }
        } else {
            out.push(finding(
                Rule::UnknownGroup,
                &op.id,
                format!(
                    "group `{}` is not one of epic #9769's inventory groups (see INVENTORY_GROUPS)",
                    op.group
                ),
            ));
        }
    }
    for group in INVENTORY_GROUPS {
        if !covered.contains(group) {
            out.push(finding(
                Rule::UncoveredGroup,
                group,
                "inventory group from #9769 has no active mapped operation",
            ));
        }
    }
}

fn check_required_rows(inv: &Inventory, out: &mut Vec<Finding>) {
    for op in inv.operations.iter().filter(|o| o.is_required()) {
        if op.test_id.as_deref().is_none_or(str::is_empty) {
            out.push(finding(
                Rule::MissingTest,
                &op.id,
                "a required operation needs a `test_id`; an unproven required row is an unknown, not a capability",
            ));
        }
        // An unknown is allowed — it is phase 1's honest answer — but only
        // when the row SAYS what is unestablished. An unfilled field and a
        // recorded open question are indistinguishable without this, and the
        // report prints `unknowns` verbatim, so the acknowledgement is also
        // what carries the question forward into the hosted-probe phases.
        if op.platform_support == Support::Unknown && op.unknowns.is_empty() {
            out.push(finding(
                Rule::UnacknowledgedUnknownSupport,
                &op.id,
                "required operation has `platform_support = unknown` and no `unknowns` entry; record what is unestablished (an unfilled field is not an open question), or establish support",
            ));
        }
        if !op.profile.is_required() {
            out.push(finding(
                Rule::RequiredInOptionalProfile,
                &op.id,
                "disposition `required` cannot sit in the `optional` profile; profile reduction may not waive a required claim",
            ));
        }
        if op.caller_integration != Coverage::None && op.callers.is_empty() {
            out.push(finding(
                Rule::IntegrationWithoutCaller,
                &op.id,
                format!(
                    "claims `caller_integration = {}` but declares no caller; probe evidence through a thin driver is not caller integration",
                    op.caller_integration.as_str()
                ),
            ));
        }
    }
}

fn check_dispositions(inv: &Inventory, out: &mut Vec<Finding>) {
    let non_waivable: BTreeSet<&str> = NON_WAIVABLE_GROUPS.iter().copied().collect();
    for op in &inv.operations {
        match op.disposition {
            Disposition::Optional => {
                if op.exclusion_reason.as_deref().is_none_or(str::is_empty) {
                    out.push(finding(
                        Rule::MissingExclusionReason,
                        &op.id,
                        "an optional exclusion needs a stated `exclusion_reason`",
                    ));
                }
                if op.preflight.is_none() {
                    out.push(finding(
                        Rule::MissingPreflight,
                        &op.id,
                        "an optional exclusion needs `preflight` — what happens if a deployment enables it anyway",
                    ));
                }
                check_waiver(inv, op, &non_waivable, out);
            }
            Disposition::Unsupported => {
                if op.preflight.is_none() {
                    out.push(finding(
                        Rule::MissingPreflight,
                        &op.id,
                        "a known-unsupported operation needs `preflight` so enabling it fails visibly",
                    ));
                }
                check_waiver(inv, op, &non_waivable, out);
            }
            Disposition::Required | Disposition::Prohibition | Disposition::TestFixture => {}
        }
    }
    check_non_waivable_groups(inv, out);
}

/// A declined (`optional`) or absent (`unsupported`) row in a **non-waivable**
/// group must name the required row(s) that still carry its requirement.
///
/// #9777 forbids profile reduction from *quietly* waiving a claims,
/// identity/trust, review, CI, protection or guarded-merge requirement. It does
/// not forbid declining one convenience operation inside such a group —
/// `branch.update-from-base` is genuinely optional because Loom rebases locally
/// and `merge.expected-head-guarded` still carries the landing-safety
/// requirement. The checkable difference between the two cases is whether a
/// required row is named as the carrier, so that is what is enforced: a named
/// carrier must exist and must itself be `required`. A row that names nothing
/// is the quiet waiver the issue rules out.
fn check_waiver(
    inv: &Inventory,
    op: &Operation,
    non_waivable: &BTreeSet<&str>,
    out: &mut Vec<Finding>,
) {
    if !non_waivable.contains(op.group.as_str()) {
        return;
    }
    if op.requirement_carried_by.is_empty() {
        out.push(finding(
            Rule::QuietlyWaivedRequirement,
            &op.id,
            format!(
                "group `{}` may not be waived quietly (claims, identity/trust, reviews, CI, protection, guarded merge, mixed fleet): name the required operation(s) that still carry this requirement in `requirement_carried_by`, or make this row `required`",
                op.group
            ),
        ));
        return;
    }
    for carrier in &op.requirement_carried_by {
        match inv.get(carrier) {
            None => out.push(finding(
                Rule::UnknownRequirementCarrier,
                &op.id,
                format!("`requirement_carried_by` names `{carrier}`, which is not an operation in this inventory"),
            )),
            Some(c) if !c.is_required() => out.push(finding(
                Rule::UnknownRequirementCarrier,
                &op.id,
                format!(
                    "`requirement_carried_by` names `{carrier}`, whose disposition is `{}`; only a `required` row can carry a non-waivable requirement",
                    c.disposition.as_str()
                ),
            )),
            Some(_) => {}
        }
    }
}

/// A non-waivable group with no `required` active row has had its requirement
/// waived outright, however each individual row justified itself. This is the
/// whole-group backstop to [`check_waiver`]'s per-row rule: carriers could
/// otherwise point at each other until nothing was required.
fn check_non_waivable_groups(inv: &Inventory, out: &mut Vec<Finding>) {
    for group in NON_WAIVABLE_GROUPS {
        let mut present = false;
        let mut required = false;
        for op in inv.operations.iter().filter(|o| o.group == *group) {
            present = true;
            required |= op.is_required();
        }
        if present && !required {
            out.push(finding(
                Rule::GroupFullyWaived,
                group,
                "non-waivable group has no `required` operation left; its requirement has been waived outright",
            ));
        }
    }
}

fn check_versions(inv: &Inventory, out: &mut Vec<Finding>) {
    for op in &inv.operations {
        for (provider, pinned) in &op.version_support {
            let Some(floor) = inv.header.providers.get(provider) else {
                out.push(finding(
                    Rule::UnknownProvider,
                    &op.id,
                    format!("`version_support.{provider}` names a provider absent from manifest.toml's [providers] table"),
                ));
                continue;
            };
            if is_stale_version(pinned, &floor.floor) {
                out.push(finding(
                    Rule::StaleVersion,
                    &op.id,
                    format!(
                        "`version_support.{provider} = {pinned}` is below the qualification floor {}; re-verify against the floor or raise the pin",
                        floor.floor
                    ),
                ));
            }
        }
    }
}

fn check_owners(inv: &Inventory, out: &mut Vec<Finding>) {
    for op in &inv.operations {
        if op.owner.trim().is_empty() {
            out.push(finding(Rule::MissingOwner, &op.id, "no owner recorded"));
        } else if !inv.header.owners.contains_key(&op.owner) {
            out.push(finding(
                Rule::UnknownOwnerArea,
                &op.id,
                format!("owner `{}` is not declared in manifest.toml's [owners] table", op.owner),
            ));
        }
    }
}

fn check_high_risk(inv: &Inventory, out: &mut Vec<Finding>) {
    let known: BTreeSet<&str> = SEEDED_HIGH_RISK_CASES.iter().copied().collect();
    let mut covered: BTreeSet<String> = BTreeSet::new();
    for op in &inv.operations {
        if op.risk == crate::forge_inventory::model::Risk::High && op.high_risk_cases.is_empty() {
            out.push(finding(
                Rule::HighRiskWithoutCase,
                &op.id,
                "marked `risk = high` but names no `high_risk_cases` entry",
            ));
        }
        for case in &op.high_risk_cases {
            if known.contains(case.as_str()) {
                covered.insert(case.clone());
            } else {
                out.push(finding(
                    Rule::HighRiskWithoutCase,
                    &op.id,
                    format!("`{case}` is not one of SEEDED_HIGH_RISK_CASES"),
                ));
            }
        }
    }
    for case in SEEDED_HIGH_RISK_CASES {
        if !covered.contains(*case) {
            out.push(finding(
                Rule::UnseededHighRiskCase,
                case,
                "no operation covers this high-risk case; #9777 requires the first manifest to seed all of them",
            ));
        }
    }
}

/// The qualification gate's rules: what may **not** count as a pass when a
/// provider is judged against this inventory (#9777, for #9769's GO/NO-GO at
/// #9792).
///
/// Deliberately separate from [`validate`] and deliberately **not** run in CI.
/// Every finding here is a true statement about phase 1's tree — the hosted
/// probes have not run, so of course nothing is probed yet — and wiring it into
/// CI would only teach the fleet to assert evidence it does not have. Its value
/// is at the decision point: `validate --qualification` answers "could this
/// inventory, as it stands, support a GO?" with a list of exactly what is still
/// missing, and an empty list is the only affirmative answer.
#[must_use]
pub fn validate_qualification(inv: &Inventory) -> Vec<Finding> {
    let mut out = Vec::new();
    for op in inv.operations.iter().filter(|o| o.is_required()) {
        if op.platform_support == Support::Unknown {
            out.push(finding(
                Rule::UnresolvedRequiredSupport,
                &op.id,
                "required operation's platform support is still `unknown`; unknown or skipped required evidence is not a pass",
            ));
        }
        if op.test_status != TestStatus::Implemented {
            out.push(finding(
                Rule::UnprovenRequiredTest,
                &op.id,
                format!(
                    "required operation's test `{}` is reserved but not implemented; a reserved identifier is an open unknown, not coverage",
                    op.test_id.as_deref().unwrap_or("<none>")
                ),
            ));
        }
        if op.caller_integration == Coverage::None {
            out.push(finding(
                Rule::UnintegratedRequiredCaller,
                &op.id,
                "required operation has no caller integration; a probe calling the provider through a thin driver proves the platform, not that Loom's production caller works",
            ));
        }
    }
    out.sort_by(|a, b| (a.rule, &a.subject).cmp(&(b.rule, &b.subject)));
    out
}

/// Count of active, required and still-unknown rows — the summary line the
/// validator prints on success so a green run still reports what it measured.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    pub rows: usize,
    pub active: usize,
    pub required: usize,
    pub unknown_support: usize,
    pub no_adapter: usize,
    pub no_caller_integration: usize,
    /// Required rows whose `test_id` is reserved but not yet implemented.
    pub declared_tests: usize,
}

#[must_use]
pub fn totals(inv: &Inventory) -> Totals {
    let mut t = Totals {
        rows: inv.operations.len(),
        ..Totals::default()
    };
    for op in inv.operations.iter().filter(|o| o.is_active()) {
        t.active += 1;
        if op.is_required() {
            t.required += 1;
            if op.test_status == TestStatus::Declared {
                t.declared_tests += 1;
            }
        }
        if op.platform_support == Support::Unknown {
            t.unknown_support += 1;
        }
        if op.adapter_coverage == Coverage::None {
            t.no_adapter += 1;
        }
        if op.caller_integration == Coverage::None {
            t.no_caller_integration += 1;
        }
    }
    t
}

/// Active rows per profile, for the report header.
#[must_use]
pub fn by_profile(inv: &Inventory) -> BTreeMap<Profile, Vec<&Operation>> {
    let mut out: BTreeMap<Profile, Vec<&Operation>> = BTreeMap::new();
    for op in inv.operations.iter().filter(|o| o.is_active()) {
        out.entry(op.profile).or_default().push(op);
    }
    out
}
