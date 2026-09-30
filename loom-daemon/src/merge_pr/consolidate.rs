//! Combined-PR candidate preparation and recovery (#9688; contract ADR-0023).
//!
//! # What this module owns
//!
//! One stage of the consolidation contract: turning an eligible group of
//! component PRs into ONE clearly-attributed candidate PR, with every source
//! reserved against landing until the candidate's fate is decided — plus the
//! abort path that undoes an attempt. Landing is NOT here: the candidate
//! merges through the canonical `merge-pr.sh` path like any PR, and the
//! post-merge reconciliation is #9689's `consolidate-reconcile`.
//!
//! # The one design decision everything hangs on (ADR-0023 §1)
//!
//! **Reservations are sequencing holds.** A source PR gets the existing
//! `loom:sequenced` label with a marker whose predecessor is the candidate:
//!
//! ```text
//! <!-- loom:sequence after=<candidate> pred_head=<cand head> follower_head=<source head> plan=cons-<attempt> source=pass -->
//! ```
//!
//! so "a source cannot land concurrently with its candidate", release on
//! landing, and self-healing expiry of abandoned attempts are all inherited
//! from the #9378 gate and the #9686 pass — this module writes markers and
//! labels, and the existing machinery does the rest. No new merge-path
//! surface; the ratchet-frozen script is untouched.
//!
//! # Identity and convergence (ADR-0023 §4)
//!
//! The attempt id is `cons-` + 8 hex of SHA-256 over the sorted `number:head`
//! component pins — deterministic, so two workers preparing the same group
//! derive the SAME id and converge on one candidate branch/PR instead of
//! racing two (the adopt-first checks make that convergence explicit). The
//! candidate PR body carries trusted `loom:consolidation` markers — the
//! mapping is reconstructible from the candidate PR alone, which is what
//! makes #9689's reconciliation resumable by anyone.
//!
//! # Eligibility (ADR-0023 §2)
//!
//! Checked fresh immediately before any mutation, and again (head pins only)
//! after the candidate exists: operator holds are absolute exclusions,
//! workflow-editing components are excluded per the original ruling, a group
//! must show overlap evidence AND carry a recorded rationale, the review size
//! is bounded, and a construction conflict is a hard abort — v1 does no
//! evict/retry search; sources stay exactly as they were.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::merge_pr::sequence::{html_comment_spans, SequenceMarker};

/// The marker prefix identifying a candidate PR's consolidation mapping.
pub const CONSOLIDATION_PREFIX: &str = "loom:consolidation";

/// The branch namespace for candidate branches (Loom-managed, so cleanup can
/// distinguish them from contributor branches; #9372 guards respected).
pub const BRANCH_NAMESPACE: &str = "loom/consolidated";

/// `LOOM_CONSOLIDATE_MAX_COMPONENTS` — review-size bound (ADR-0023 E9).
pub const MAX_COMPONENTS_ENV: &str = "LOOM_CONSOLIDATE_MAX_COMPONENTS";
const DEFAULT_MAX_COMPONENTS: usize = 4;

/// `LOOM_CONSOLIDATE_MAX_DIFF_LINES` — review-size bound (ADR-0023 E9).
pub const MAX_DIFF_LINES_ENV: &str = "LOOM_CONSOLIDATE_MAX_DIFF_LINES";
const DEFAULT_MAX_DIFF_LINES: u64 = 800;

/// Absolute exclusions (ADR-0023 E3): any of these labels on a component
/// rejects the group before any mutation. `loom:operator-only` /
/// `loom:operator-decision` are the original operator ruling, preserved
/// verbatim; the rest are ordinary holds this contract must not widen.
pub const INELIGIBLE_LABELS: [&str; 6] = [
    "loom:blocked",
    "loom:operator",
    "loom:operator-only",
    "loom:operator-decision",
    "loom:changes-requested",
    "loom:ci-failure",
];

/// In-flight claims (ADR-0023 E5): an agent owns the PR right now.
pub const IN_FLIGHT_LABELS: [&str; 3] = ["loom:reviewing", "loom:treating", "loom:building"];

/// The review-size bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    pub max_components: usize,
    pub max_diff_lines: u64,
}

impl Bounds {
    /// The configured bounds, defaults per ADR-0023 §2 E9.
    #[must_use]
    pub fn from_env() -> Self {
        let max_components = std::env::var(MAX_COMPONENTS_ENV)
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .filter(|n: &usize| *n >= 2)
            .unwrap_or(DEFAULT_MAX_COMPONENTS);
        let max_diff_lines = std::env::var(MAX_DIFF_LINES_ENV)
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_MAX_DIFF_LINES);
        Self {
            max_components,
            max_diff_lines,
        }
    }
}

// --- Identity -----------------------------------------------------------

/// The attempt id for a group of pinned components: `cons-` + 8 hex of
/// SHA-256 over the sorted `number:head` lines (the `seq-` derivation, one
/// id format, two namespaces). Deterministic across daemons and versions:
/// duplicate workers converge on one candidate.
#[must_use]
pub fn attempt_id(members: &[(u32, &str)]) -> String {
    let mut lines: Vec<String> = members.iter().map(|(n, h)| format!("{n}:{h}")).collect();
    lines.sort();
    let mut hasher = Sha256::new();
    for line in &lines {
        hasher.update(line.as_bytes());
        hasher.update(b"\n");
    }
    let digest: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("cons-{}", &digest[..8])
}

/// The candidate branch for an attempt.
#[must_use]
pub fn candidate_branch(attempt: &str) -> String {
    format!("{BRANCH_NAMESPACE}/{attempt}")
}

// --- Eligibility (pure core) --------------------------------------------

/// One component PR, as eligibility and pinning need it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentState {
    pub number: u32,
    /// "OPEN" or "CLOSED" (the forge's rendering, uppercased on parse).
    pub state: String,
    pub draft: bool,
    pub head_sha: Option<String>,
    pub base_ref: String,
    pub labels: Vec<String>,
    pub files: BTreeSet<String>,
    pub additions: u64,
    pub deletions: u64,
}

impl ComponentState {
    #[must_use]
    pub fn has(&self, label: &str) -> bool {
        self.labels.iter().any(|l| l == label)
    }

    /// The pin: `(number, head)` when pinnable.
    #[must_use]
    pub fn pin(&self) -> Option<(u32, &str)> {
        self.head_sha.as_deref().map(|h| (self.number, h))
    }
}

/// Why a group (or one component) is not consolidation-eligible. The reason
/// strings are the verb's rejection output — named precisely, because a
/// rejection that does not say why is how silent scope-widening starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EligibilityFailure {
    /// E1 — every component must target the repo default branch.
    NotDefaultBase { number: u32, base: String },
    /// E2 — open, not a draft, pinnable head.
    NotOpenOrUnpinnable { number: u32, detail: String },
    /// E3 — an absolute hold (operator holds included).
    Held { number: u32, label: String },
    /// E4 — workflow-editing components are excluded by the operator ruling.
    WorkflowEdit { number: u32, file: String },
    /// E5 — an agent is actively working the PR.
    InFlight { number: u32, label: String },
    /// E6 — an unresolved consolidation attempt already reserves this PR.
    AlreadyReserved { number: u32, attempt: String },
    /// E7 — sequenced behind an unlanded predecessor outside the group.
    SequencedOutsideGroup { number: u32, after: u32 },
    /// E8 — no shared-file overlap evidence across the group.
    NoOverlap,
    /// E9 — review size bound exceeded.
    TooLarge { components: usize, diff_lines: u64 },
    /// E8 — no rationale recorded.
    NoReason,
}

/// Check one assembled group. `reserved_attempts` maps component number →
/// the `cons-` attempt id of an unresolved reservation marker, if any;
/// `markers` maps component number → its newest trusted `loom:sequence`
/// marker (ordering or reservation). Pure: the caller fetched everything.
#[must_use]
pub fn check_eligibility(
    components: &[ComponentState],
    markers: &std::collections::BTreeMap<u32, SequenceMarker>,
    default_branch: &str,
    reason: &str,
    bounds: &Bounds,
) -> Vec<EligibilityFailure> {
    let mut failures = Vec::new();
    if reason.trim().is_empty() {
        failures.push(EligibilityFailure::NoReason);
    }
    if components.len() > bounds.max_components {
        let diff_lines: u64 = components.iter().map(|c| c.additions + c.deletions).sum();
        failures.push(EligibilityFailure::TooLarge {
            components: components.len(),
            diff_lines,
        });
    }
    let diff_lines: u64 = components.iter().map(|c| c.additions + c.deletions).sum();
    if diff_lines > bounds.max_diff_lines {
        failures.push(EligibilityFailure::TooLarge {
            components: components.len(),
            diff_lines,
        });
    }
    let numbers: BTreeSet<u32> = components.iter().map(|c| c.number).collect();
    for c in components {
        if c.base_ref != default_branch {
            failures.push(EligibilityFailure::NotDefaultBase {
                number: c.number,
                base: c.base_ref.clone(),
            });
        }
        if c.state != "OPEN" || c.draft {
            failures.push(EligibilityFailure::NotOpenOrUnpinnable {
                number: c.number,
                detail: format!("state={}, draft={}", c.state, c.draft),
            });
        }
        if c.head_sha.is_none() {
            failures.push(EligibilityFailure::NotOpenOrUnpinnable {
                number: c.number,
                detail: "no head SHA".to_string(),
            });
        }
        for label in INELIGIBLE_LABELS {
            if c.has(label) {
                failures.push(EligibilityFailure::Held {
                    number: c.number,
                    label: label.to_string(),
                });
            }
        }
        for label in IN_FLIGHT_LABELS {
            if c.has(label) {
                failures.push(EligibilityFailure::InFlight {
                    number: c.number,
                    label: label.to_string(),
                });
            }
        }
        if let Some(file) = c.files.iter().find(|f| f.starts_with(".github/workflows/")) {
            failures.push(EligibilityFailure::WorkflowEdit {
                number: c.number,
                file: file.clone(),
            });
        }
        if let Some(attempt) = markers
            .get(&c.number)
            .and_then(|m| m.plan.starts_with("cons-").then(|| m.plan.clone()))
        {
            failures.push(EligibilityFailure::AlreadyReserved {
                number: c.number,
                attempt,
            });
        }
        if let Some(m) = markers.get(&c.number) {
            if m.source.as_deref() == Some("pass") && !numbers.contains(&m.after) {
                failures.push(EligibilityFailure::SequencedOutsideGroup {
                    number: c.number,
                    after: m.after,
                });
            }
        }
    }
    // E8: overlap evidence across the group.
    let overlap = components.iter().enumerate().any(|(i, a)| {
        components[i + 1..]
            .iter()
            .any(|b| a.files.iter().any(|f| b.files.contains(f)))
    });
    if components.len() >= 2 && !overlap {
        failures.push(EligibilityFailure::NoOverlap);
    }
    failures
}

// --- Mapping markers ----------------------------------------------------

/// The mapping reconstructed from a candidate PR's trusted markers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateMapping {
    pub attempt: String,
    pub base: String,
    /// The candidate branch head at prepare time — the tree the candidate's
    /// CI tested and Judge reviewed.
    pub candidate_head: String,
    /// `(component PR, pinned head)` in landing order.
    pub components: Vec<(u32, String)>,
}

/// The candidate PR body: trusted mapping markers first (machine state),
/// then the human-readable table with the recorded rationale (ADR-0023 §4).
#[must_use]
pub fn mapping_body(
    attempt: &str,
    base: &str,
    candidate_head: &str,
    components: &[(u32, &str)],
    reason: &str,
) -> String {
    let mut body = format!(
        "<!-- {CONSOLIDATION_PREFIX} attempt={attempt} base={base} candidate_head={candidate_head} -->\n"
    );
    for (n, h) in components {
        body.push_str(&format!("<!-- {CONSOLIDATION_PREFIX}-component pr={n} head={h} -->\n"));
    }
    body.push_str(&format!(
        "\n**Combined candidate PR** — consolidation attempt `{attempt}` (#9688, ADR-0023).\n\n\
         This PR bundles {} component PRs for ONE independent review and one CI run. \
         A green component never approved this combination: this diff gets its own Judge \
         verdict and its own CI, and the component PRs stay open until landing is verified.\n\n\
         | Component | Pinned head |\n|---|---|\n",
        components.len()
    ));
    for (n, h) in components {
        body.push_str(&format!("| #{n} | `{h}` |\n"));
    }
    body.push_str(&format!(
        "\n**Recorded compatibility rationale**: {reason}\n\n\
         The component PRs are reserved (sequenced behind this PR) until this candidate's \
         landing is verified. Abort: `loom-daemon merge-pr consolidate-abort --pr <this PR>` — \
         sources are preserved untouched.\n\n\
         ---\n\
         *Automated by loom-daemon merge-pr consolidate-prepare (#9688, ADR-0023)*"
    ));
    body
}

/// Parse the mapping out of a candidate PR body. Single-line HTML comments
/// only, same immunity rules as the sequencing markers.
#[must_use]
pub fn parse_mapping(body: &str) -> Option<CandidateMapping> {
    let mut attempt = None;
    let mut base = None;
    let mut candidate_head = None;
    let mut components = Vec::new();
    for line in body.lines() {
        for span in html_comment_spans(line) {
            let text = span.trim();
            let rest = match text.strip_prefix(CONSOLIDATION_PREFIX) {
                Some(r) => r,
                None => continue,
            };
            // `-component` is the second marker shape; both must start with
            // whitespace after their prefix, or they are a different
            // namespace (`loom:consolidation-x`).
            let is_component = rest.starts_with("-component");
            let rest = if is_component {
                rest.strip_prefix("-component").unwrap_or(rest)
            } else {
                rest
            };
            if !rest.starts_with(char::is_whitespace) {
                continue;
            }
            let mut component: Option<(u32, String)> = None;
            for field in rest.split_whitespace() {
                let (key, value) = field.split_once('=')?;
                match (is_component, key) {
                    (false, "attempt") => attempt = Some(value.to_string()),
                    (false, "base") => base = Some(value.to_string()),
                    (false, "candidate_head") => candidate_head = Some(value.to_string()),
                    (true, "pr") => {
                        let n: u32 = value.parse().ok()?;
                        component = Some((n, String::new()));
                    }
                    (true, "head") => {
                        let c = component.as_mut()?;
                        c.1 = value.to_string();
                    }
                    _ => return None,
                }
            }
            if is_component {
                match component {
                    // A component pin without its head is a corrupted write.
                    Some(c) if !c.1.is_empty() => components.push(c),
                    Some(_) => return None,
                    None => return None,
                }
            }
        }
    }
    Some(CandidateMapping {
        attempt: attempt?,
        base: base?,
        candidate_head: candidate_head?,
        components,
    })
}

// --- Construction (git) --------------------------------------------------

/// A construction failure that means HARD ABORT: the pinned heads do not
/// merge cleanly. Sources are untouched; the caller cleans up and reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstructionConflict {
    /// The component whose merge failed.
    pub number: u32,
    pub detail: String,
}

/// Merge the pinned heads into a scratch worktree off `base`, in order.
///
/// Pure git mechanics, parameterized on the binary for tests: creates the
/// worktree at `worktree_path` detached at `base`, merges each pin with
/// `--no-ff` (so each component's inclusion is an ancestry fact #9689 can
/// verify), and leaves the worktree in place for the caller to push and
/// remove. Conflict ⇒ [`ConstructionConflict`] naming the component; the
/// worktree is removed before returning the error.
pub fn construct(
    git_bin: &str,
    repo_root: &Path,
    worktree_path: &Path,
    base: &str,
    pinned: &[(u32, &str)],
) -> Result<String, ConstructionConflict> {
    let run = |args: &[&str], cwd: &Path| -> Result<String, String> {
        let out = Command::new(git_bin)
            .args(args)
            .current_dir(cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| format!("git {:?}: {e}", args.first().unwrap_or(&"")))?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    if let Err(e) = run(
        &[
            "worktree",
            "add",
            "--detach",
            worktree_path.to_str().unwrap_or_default(),
            base,
        ],
        repo_root,
    ) {
        return Err(ConstructionConflict {
            number: 0,
            detail: format!("worktree add failed: {e}"),
        });
    }
    for (number, head) in pinned {
        if let Err(e) = run(&["merge", "--no-ff", "--no-edit", head], worktree_path) {
            let _ = run(
                &[
                    "worktree",
                    "remove",
                    "--force",
                    worktree_path.to_str().unwrap_or_default(),
                ],
                repo_root,
            );
            return Err(ConstructionConflict {
                number: *number,
                detail: e,
            });
        }
    }
    run(&["rev-parse", "HEAD"], worktree_path).map_err(|e| {
        let _ = run(
            &[
                "worktree",
                "remove",
                "--force",
                worktree_path.to_str().unwrap_or_default(),
            ],
            repo_root,
        );
        ConstructionConflict {
            number: 0,
            detail: format!("rev-parse after construction: {e}"),
        }
    })
}

/// Remove the scratch worktree (best-effort; a leftover is cleaned up by the
/// normal worktree reapers).
pub fn remove_worktree(git_bin: &str, repo_root: &Path, worktree_path: &Path) {
    let _ = Command::new(git_bin)
        .args([
            "worktree",
            "remove",
            "--force",
            worktree_path.to_str().unwrap_or_default(),
        ])
        .current_dir(repo_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .status();
}

/// Push the constructed candidate branch (separate from [`construct`] so
/// tests can exercise construction without a remote).
pub fn push_branch(
    git_bin: &str,
    _repo_root: &Path,
    worktree_path: &Path,
    branch: &str,
) -> Result<()> {
    let out = Command::new(git_bin)
        .args(["push", "origin", &format!("HEAD:refs/heads/{branch}")])
        .current_dir(worktree_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("git push")?;
    if !out.status.success() {
        return Err(anyhow!(
            "pushing {branch} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

// --- Forge reads ---------------------------------------------------------

/// The `gh` binary, honoring `LOOM_GH_BIN` — the shared seam.
#[must_use]
pub fn gh_bin_env() -> String {
    std::env::var("LOOM_GH_BIN").unwrap_or_else(|_| "gh".to_string())
}

/// Run `gh <args…>` in `root` with the per-root credential applied.
fn gh(gh_bin: &Path, root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let mut cmd = Command::new(gh_bin);
    cmd.args(args);
    cmd.current_dir(root);
    crate::credential_preflight::apply_gh_config_for_root(&mut cmd, root);
    if let Ok(repo) = std::env::var("LOOM_REPO") {
        cmd.arg("--repo").arg(repo);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let out = cmd
        .output()
        .with_context(|| format!("failed to invoke {}", gh_bin.display()))?;
    if !out.status.success() {
        return Err(anyhow!(
            "gh {} failed in {}: {}",
            args.first().copied().unwrap_or_default(),
            root.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(out.stdout)
}

#[derive(Debug, Deserialize)]
struct GhComponent {
    state: String,
    #[serde(default)]
    is_draft: bool,
    #[serde(rename = "headRefOid", default)]
    head_ref_oid: Option<String>,
    #[serde(rename = "baseRefName", default)]
    base_ref_name: Option<String>,
    #[serde(default)]
    labels: Vec<GhLabel>,
    #[serde(default)]
    files: Vec<GhFile>,
    #[serde(default)]
    additions: u64,
    #[serde(default)]
    deletions: u64,
}

#[derive(Debug, Deserialize)]
struct GhLabel {
    name: String,
}

#[derive(Debug, Deserialize)]
struct GhFile {
    #[serde(rename = "path")]
    _path: String,
}

/// Fetch one component PR's eligibility state.
///
/// # Errors
/// Forge read failure, or a malformed payload (which is never read as
/// "eligible").
pub fn fetch_component(gh_bin: &Path, root: &Path, number: u32) -> Result<ComponentState> {
    let stdout = gh(
        gh_bin,
        root,
        &[
            "pr",
            "view",
            &number.to_string(),
            "--json",
            "state,isDraft,headRefOid,baseRefName,labels,files,additions,deletions",
        ],
    )?;
    let r: GhComponent = serde_json::from_slice(&stdout).context("parse gh pr view JSON")?;
    Ok(ComponentState {
        number,
        state: r.state.to_uppercase(),
        draft: r.is_draft,
        head_sha: r.head_ref_oid,
        base_ref: r.base_ref_name.unwrap_or_default(),
        labels: r.labels.into_iter().map(|l| l.name).collect(),
        files: r.files.into_iter().map(|f| f._path).collect(),
        additions: r.additions,
        deletions: r.deletions,
    })
}

pub fn default_branch(gh_bin: &Path, root: &Path) -> Result<String> {
    #[derive(Debug, Deserialize)]
    struct Ref {
        #[serde(rename = "defaultBranchRef")]
        default_branch_ref: DefaultRef,
    }
    #[derive(Debug, Deserialize)]
    struct DefaultRef {
        name: String,
    }
    let stdout = gh(gh_bin, root, &["repo", "view", "--json", "defaultBranchRef"])?;
    let r: Ref = serde_json::from_slice(&stdout).context("parse gh repo view JSON")?;
    Ok(r.default_branch_ref.name)
}

/// The live default-branch tip: fetched (not a local ref guess), so
/// construction starts from what the candidate will actually target.
pub fn live_base_sha(gh_bin: &Path, root: &Path, default_branch: &str) -> Result<String> {
    #[derive(Debug, Deserialize)]
    struct Ref {
        #[serde(rename = "object")]
        object: Obj,
    }
    #[derive(Debug, Deserialize)]
    struct Obj {
        sha: String,
    }
    let stdout = gh(
        gh_bin,
        root,
        &[
            "api",
            &format!("repos/{{owner}}/{{repo}}/commits/{default_branch}"),
        ],
    )?;
    let r: Ref = serde_json::from_slice(&stdout).context("parse commits API JSON")?;
    Ok(r.object.sha)
}

/// An open candidate PR for this head branch, if one exists — the adopt-first
/// check that makes duplicate workers converge (ADR-0023 §5).
pub fn find_open_candidate(gh_bin: &Path, root: &Path, branch: &str) -> Result<Option<u32>> {
    #[derive(Debug, Deserialize)]
    struct Row {
        number: u32,
    }
    let stdout = gh(
        gh_bin,
        root,
        &[
            "pr", "list", "--state", "open", "--head", branch, "--json", "number",
        ],
    )?;
    let rows: Vec<Row> = serde_json::from_slice(&stdout).context("parse gh pr list JSON")?;
    Ok(rows.first().map(|r| r.number))
}

// --- Reservation writing --------------------------------------------------

/// The reservation marker for one source PR against the candidate.
#[must_use]
pub fn reservation_marker(
    candidate_pr: u32,
    candidate_head: &str,
    source: &ComponentState,
    attempt: &str,
) -> SequenceMarker {
    SequenceMarker {
        after: candidate_pr,
        pred_head: candidate_head.to_string(),
        follower_head: source.head_sha.clone().unwrap_or_default(),
        plan: attempt.to_string(),
        source: Some("pass".to_string()),
    }
}

/// True when `bodies` already carries this exact marker — the repeated-comment
/// guard that makes competing workers converge (ADR-0023 §5).
#[must_use]
pub fn reservation_present(bodies: &[String], marker: &SequenceMarker) -> bool {
    bodies.iter().any(|b| b.contains(&marker_text_of(marker)))
}

fn marker_text_of(marker: &SequenceMarker) -> String {
    // sequence::marker_text is the canonical renderer; a local alias keeps
    // the call sites readable.
    crate::merge_pr::sequence::marker_text(marker)
}

/// The reservation apply comment (machine marker + concise explanation).
#[must_use]
pub fn reservation_comment_body(marker: &SequenceMarker, attempt: &str) -> String {
    format!(
        "**Reserved by consolidation attempt `{attempt}`** (#9688, ADR-0023)\n\n\
         This PR is a component of combined candidate #{}: it cannot merge while the candidate \
         is open (the `loom:sequenced` gate), so the candidate gets one independent review and \
         one CI run instead of one per component. If the candidate lands, this reservation \
         releases and the landing reconciliation follows with per-component status. If the \
         attempt is aborted or abandoned, this reservation self-releases (72 h bound) or is \
         released by `consolidate-abort` — this PR is never modified beyond the label and this \
         notice.\n\n\
         {}\n\n\
         ---\n\
         *Automated by loom-daemon merge-pr consolidate-prepare (#9688)*",
        marker.after,
        marker_text_of(marker)
    )
}

/// The abort release comment — distinct from the landing release so the
/// transcript shows which fate the attempt met.
#[must_use]
pub fn reservation_release_body(marker: &SequenceMarker, attempt: &str) -> String {
    format!(
        "<!-- loom:sequence released plan={} -->\n\
         **Consolidation attempt `{}` released this reservation** — the attempt was aborted; \
         this PR is back to its normal pipeline, untouched and actionable.\n\n\
         ---\n\
         *Automated by loom-daemon merge-pr consolidate-abort (#9688)*",
        marker.plan, attempt
    )
}

// --- Prepare decision (pure core the CLI verb drives) ---------------------

/// The outcome of one prepare run. The verb prints it; the AC "repeated or
/// concurrent invocation produces no duplicate candidate or competing
/// reservation" is visible here as `AlreadyPrepared` vs `Prepared`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrepareOutcome {
    /// Fresh candidate created; N reservations applied.
    Prepared {
        attempt: String,
        candidate_pr: u32,
        branch: String,
        reservations: usize,
    },
    /// An open candidate for this exact group already existed — adopted, no
    /// duplicate created; M reservations were missing and are now applied.
    AlreadyPrepared {
        attempt: String,
        candidate_pr: u32,
        reservations_applied: usize,
        reservations_present: usize,
    },
}

#[cfg(test)]
mod tests;
