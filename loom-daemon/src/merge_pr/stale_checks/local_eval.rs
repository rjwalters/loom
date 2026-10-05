//! Local merge-tree evaluation of cheap, deterministic stale components (#10388).
//!
//! # Why
//!
//! The #8248 guard's only remedy is a tree-identical re-date push plus a full
//! CI run (~12–15 min). On a busy `main` the base moves faster than that, so
//! approved PRs loop until the re-date budget runs out (#9590). The trigger is
//! almost always a component of `Structural Checks` that is a pure function of
//! the tree — the conflict-marker scan, the prompt/size ratchets — and those
//! can be computed on the exact merge tree in seconds.
//!
//! # The rule
//!
//! When every stale component of every stale required context is on
//! [`CHEAP_CHECKS`] — input-scoped verdicts only ([`super::Verdict::StaleInputs`],
//! built-in specs only) — the merge tree of the base tip the assessment used
//! and the PR head is checked out into a temp git repository, and each such
//! component's **CI steps** — read from that tree's own `ci.yml`, under the
//! component's `# component:` marker, and run the way Actions runs a `run:`
//! step (`bash --noprofile --norc -eo pipefail <file>`) — run there. All pass
//! ⇒ those components are fresh for this merge.
//!
//! # Fail closed
//!
//! Anything else leaves the guard's original refusal standing, so the existing
//! remedy runs: a stale non-allowlisted component (anything expensive), a
//! time-rule or unknown verdict, a repo-declared spec, a missing script or
//! required tool, a PR that edits `ci.yml` or any `*.sh` (only code already
//! on the base is ever executed here, see [`changes_check_code`]), a step this
//! module cannot run faithfully (`uses:`, `env:`,
//! a `${{ }}` expression, a conditional), a merge-tree conflict, a fetched base that is not the base
//! the assessment judged, a head that moved, a check that could not start, and
//! a timeout ([`Outcome::Unknown`]). A check that runs and fails is
//! [`Outcome::Failed`]: the merge stays blocked and the failure is posted.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use chrono::{DateTime, Utc};

use super::inputs::{self, ScopedEvidence};
use super::workflow_scope::CiScopes;
use super::{assess_scoped, CheckRun, Verdict};
use crate::merge_pr::tree_checks::{self, git, merge_tree};

/// One allowlisted component. WHAT runs is not listed here: it is read from
/// the merge tree's own `.github/workflows/ci.yml` (the steps under the
/// component's `# component:` marker, see [`ci_steps`]), so a local run and a
/// CI run of the same tree execute the same commands by construction.
#[derive(Debug, Clone, Copy)]
pub struct CheapCheck {
    /// The component name ([`inputs::CheckSpec::context`]).
    pub component: &'static str,
    /// Must exist in the merge tree, or the evaluation is unknown.
    pub script: &'static str,
    /// Binaries the steps need on `PATH`; one missing is unknown, never a
    /// pass (`check-doc-anchors.sh` exits 78 without `lychee`).
    pub requires: &'static [&'static str],
    /// Step names NOT run locally: tool installs that `requires` replaces.
    pub skip_steps: &'static [&'static str],
}

/// The allowlist: components that are cheap (seconds), toolchain-free and a
/// pure function of the checked-out tree. Keep it small and explicit — every
/// entry is a place where a local run substitutes for CI. Anything that needs
/// cargo or a network service stays off it.
pub const CHEAP_CHECKS: &[CheapCheck] = &[
    CheapCheck {
        component: "Conflict Marker Check",
        script: "defaults/scripts/check-conflict-markers.sh",
        requires: &["bash", "git", "jq"],
        skip_steps: &[],
    },
    CheapCheck {
        component: "File Size Ratchet",
        script: "scripts/check-file-size-budget.sh",
        requires: &["bash", "git"],
        skip_steps: &[],
    },
    CheapCheck {
        component: "Markdown Token Ratchet",
        script: "scripts/check-markdown-token-budget.sh",
        requires: &["bash", "git"],
        skip_steps: &[],
    },
    CheapCheck {
        component: "Role Prompt Prefix Ratchet",
        script: "scripts/check-role-prompt-budget.sh",
        requires: &["bash", "git"],
        skip_steps: &[],
    },
    CheapCheck {
        component: "Docs/Defaults Parity Check",
        script: "scripts/check-docs-defaults-parity.sh",
        requires: &["bash", "git"],
        skip_steps: &[],
    },
    CheapCheck {
        component: "Doc Table-of-Contents Freshness",
        script: "scripts/check-doc-tocs.sh",
        requires: &["bash", "git"],
        skip_steps: &[],
    },
    CheapCheck {
        component: "Vendored Private-Reference Scrub",
        script: "scripts/check-vendored-private-refs.sh",
        requires: &["bash", "git", "grep"],
        skip_steps: &[],
    },
    CheapCheck {
        component: "Dangling Link Check",
        script: "scripts/check-dangling-links.sh",
        requires: &["bash", "git", "lychee"],
        skip_steps: &["Install lychee (pinned, checksum-verified)"],
    },
];

pub mod ci_steps;
pub use ci_steps::ci_steps;

/// Where the steps are read from, relative to the merge tree.
pub const CI_WORKFLOW_PATH: &str = inputs::CI_WORKFLOW;

/// Per-step timeout default; generous against the ~1–8 s these take.
pub const DEFAULT_TIMEOUT_SECS: u64 = 180;

/// Env opt-out: `0`/`false`/`off`/`no` disables local evaluation (the guard
/// then behaves exactly as before #10388).
pub const DISABLE_ENV: &str = "LOOM_STALE_CHECKS_LOCAL_EVAL";

/// Is local evaluation enabled in this process's environment?
#[must_use]
pub fn enabled() -> bool {
    enabled_from(std::env::var(DISABLE_ENV).ok().as_deref())
}

/// [`enabled`] over an explicit env value (unset/anything else = on).
#[must_use]
pub fn enabled_from(value: Option<&str>) -> bool {
    !matches!(value.map(str::trim), Some("0" | "false" | "off" | "no"))
}

/// The allowlist entry for `component`.
#[must_use]
pub fn cheap(component: &str) -> Option<&'static CheapCheck> {
    CHEAP_CHECKS.iter().find(|c| c.component == component)
}

/// The components to evaluate locally, or `None` when the verdict cannot be
/// satisfied that way (see the module header). `None` also when nothing is
/// stale — there is nothing to evaluate.
#[must_use]
pub fn locally_evaluable(
    base_tip: DateTime<Utc>,
    required: &[String],
    runs: &[CheckRun],
    scoped: Option<&ScopedEvidence>,
) -> Option<Vec<&'static str>> {
    locally_evaluable_with(base_tip, required, runs, scoped, |c| cheap(c).is_some())
}

/// [`locally_evaluable`] with the allowlist as a predicate (the test seam).
pub fn locally_evaluable_with(
    base_tip: DateTime<Utc>,
    required: &[String],
    runs: &[CheckRun],
    scoped: Option<&ScopedEvidence>,
    is_cheap: impl Fn(&str) -> bool,
) -> Option<Vec<&'static str>> {
    let ev = scoped?;
    if ev.pr_delta.paths.iter().any(|p| changes_check_code(p)) {
        return None; // trust boundary: run only code the base already has
    }
    let mut contexts: Vec<&String> = required.iter().collect();
    contexts.sort();
    contexts.dedup();
    let mut out: BTreeSet<&'static str> = BTreeSet::new();
    for ctx in contexts {
        // Same per-context verdict the guard reached; only an input-scoped
        // stale verdict has component-level evidence to act on.
        match assess_scoped(base_tip, std::slice::from_ref(ctx), runs, scoped).0 {
            Verdict::Fresh => continue,
            Verdict::StaleInputs { .. } => {}
            Verdict::Stale { .. } | Verdict::Unknown(_) => return None,
        }
        let specs = inputs::specs_for(ctx)?; // repo-declared spec: not ours to vouch for
        let mv = ev.base_moves.get(ctx.as_str())?;
        let ci = CiScopes {
            base: mv.ci_scope.clone(),
            pr: ev.pr_ci_scope.clone(),
        };
        let mut any = false;
        for spec in specs {
            if inputs::stale_reason_scoped(spec, &mv.files, &ev.pr_delta, &ci).is_some() {
                if !is_cheap(spec.context) {
                    return None;
                }
                out.insert(spec.context);
                any = true;
            }
        }
        if !any {
            return None; // stale with no stale component: inconsistent, refuse
        }
    }
    (!out.is_empty()).then(|| out.into_iter().collect())
}

/// Does a PR touching `path` change code a local evaluation would EXECUTE?
/// The steps come from `ci.yml` and run `*.sh` scripts, all read from the
/// merge tree, on the host doing the merge. Local evaluation therefore runs
/// only when the PR changes none of them: the PR contributes data the base's
/// own (already-merged) check code scans, never the code itself. A PR that
/// edits a check script keeps the CI path.
#[must_use]
pub fn changes_check_code(path: &str) -> bool {
    path == inputs::CI_WORKFLOW || path.ends_with(".sh")
}

/// One component's local result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentResult {
    pub component: String,
    pub passed: bool,
}

/// What was evaluated, for the merge log and the PR comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub head_sha: String,
    pub base_sha: String,
    pub tree_sha: String,
    pub results: Vec<ComponentResult>,
}

/// The evaluation's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Every component passed on the merge tree.
    Passed(Record),
    /// A component ran and failed: the merge stays blocked.
    Failed {
        record: Record,
        component: String,
        step: String,
        output: String,
    },
    /// No verdict (fail closed): the guard's refusal stands.
    Unknown(String),
}

/// Evaluate `components` on the merge tree of `<remote>/<base_ref>` (which
/// must still be `expected_base`, the tip the freshness assessment judged)
/// and `head_sha`.
#[allow(clippy::too_many_arguments)]
pub fn evaluate(
    repo_root: &Path,
    remote: &str,
    pr: &str,
    base_ref: &str,
    head_sha: &str,
    expected_base: &str,
    components: &[&str],
    checks: &[CheapCheck],
    timeout: Duration,
) -> Outcome {
    let mut plan: Vec<&CheapCheck> = Vec::new();
    for c in components {
        match checks.iter().find(|k| k.component == *c) {
            Some(k) => plan.push(k),
            None => return Outcome::Unknown(format!("component '{c}' is not on the allowlist")),
        }
    }
    if plan.is_empty() {
        return Outcome::Unknown("no components to evaluate".to_string());
    }
    for k in &plan {
        for tool in k.requires {
            if !on_path(tool) {
                return Outcome::Unknown(format!(
                    "'{}' needs `{tool}`, which is not on PATH here",
                    k.component
                ));
            }
        }
    }
    let mt = match merge_tree(repo_root, remote, pr, base_ref, head_sha) {
        Ok(m) => m,
        Err(e) => return Outcome::Unknown(e),
    };
    if mt.base_sha != expected_base {
        return Outcome::Unknown(format!(
            "the base moved to {} after the freshness assessment judged {expected_base}; \
re-run the merge",
            mt.base_sha
        ));
    }
    let checkout = match build_checkout(repo_root, &mt, head_sha) {
        Ok(d) => d,
        Err(e) => return Outcome::Unknown(e),
    };
    let mut record = Record {
        head_sha: head_sha.to_string(),
        base_sha: mt.base_sha.clone(),
        tree_sha: mt.tree_sha.clone(),
        results: Vec::new(),
    };
    let ci_yml = match std::fs::read_to_string(checkout.path().join(CI_WORKFLOW_PATH)) {
        Ok(t) => t,
        Err(e) => {
            return Outcome::Unknown(format!(
                "the merge tree has no readable {CI_WORKFLOW_PATH}: {e}"
            ))
        }
    };
    let scratch = match tempfile::Builder::new()
        .prefix("loom-local-eval-steps-")
        .tempdir()
    {
        Ok(d) => d,
        Err(e) => return Outcome::Unknown(format!("could not create a temp dir: {e}")),
    };
    for k in plan {
        if !checkout.path().join(k.script).is_file() {
            return Outcome::Unknown(format!(
                "'{}': {} is not in the merge tree",
                k.component, k.script
            ));
        }
        let steps = match ci_steps(&ci_yml, k.component, k.skip_steps) {
            Ok(s) => s,
            Err(e) => return Outcome::Unknown(format!("'{}': {e}", k.component)),
        };
        for (i, step) in steps.iter().enumerate() {
            // A `run:` step is a script FILE run by bash with -eo pipefail,
            // exactly as Actions runs it; the file lives outside the tree.
            let file = scratch.path().join(format!("step-{i}.sh"));
            if let Err(e) = std::fs::write(&file, step) {
                return Outcome::Unknown(format!("could not write a step script: {e}"));
            }
            let cfg = tree_checks::Config {
                checks: vec![format!(
                    "bash --noprofile --norc -eo pipefail '{}'",
                    file.display()
                )],
                timeout,
            };
            match tree_checks::run_checks(checkout.path(), &cfg) {
                tree_checks::Outcome::Clean => {}
                tree_checks::Outcome::Failed { output, .. } if tree_checks::is_timeout(&output) => {
                    return Outcome::Unknown(format!("'{}' timed out: {step}", k.component));
                }
                tree_checks::Outcome::Failed { output, .. } => {
                    record.results.push(ComponentResult {
                        component: k.component.to_string(),
                        passed: false,
                    });
                    return Outcome::Failed {
                        record,
                        component: k.component.to_string(),
                        step: step.clone(),
                        output,
                    };
                }
                tree_checks::Outcome::Unknown(why) => {
                    return Outcome::Unknown(format!("'{}': {why}", k.component));
                }
            }
        }
        record.results.push(ComponentResult {
            component: k.component.to_string(),
            passed: true,
        });
    }
    Outcome::Passed(record)
}

fn on_path(tool: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(tool).is_file()))
}

/// A temp git repository whose HEAD is the merge commit (parents: base, head)
/// and whose index + work tree are exactly the merge tree — what CI's checkout
/// of `refs/pull/<N>/merge` sees, so `git ls-files`-based checks work. Objects
/// are borrowed through `objects/info/alternates`; nothing is written to the
/// primary repository, and the directory is removed on drop.
pub fn build_checkout(
    repo_root: &Path,
    mt: &tree_checks::MergeTree,
    head_sha: &str,
) -> Result<tempfile::TempDir, String> {
    let common = git(repo_root, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
    let dir = tempfile::Builder::new()
        .prefix("loom-local-eval-")
        .tempdir()
        .map_err(|e| format!("could not create a temp dir: {e}"))?;
    let d = dir.path();
    git(d, &["init", "-q"])?;
    git(d, &["config", "gc.auto", "0"])?;
    let alt = d.join(".git/objects/info/alternates");
    std::fs::write(&alt, format!("{}\n", Path::new(&common).join("objects").display()))
        .map_err(|e| format!("could not write alternates: {e}"))?;
    let out = Command::new("git")
        .current_dir(d)
        .env("GIT_AUTHOR_NAME", "loom")
        .env("GIT_AUTHOR_EMAIL", "loom@localhost")
        .env("GIT_COMMITTER_NAME", "loom")
        .env("GIT_COMMITTER_EMAIL", "loom@localhost")
        .args([
            "commit-tree",
            &mt.tree_sha,
            "-p",
            &mt.base_sha,
            "-p",
            head_sha,
            "-m",
            "loom local merge-tree evaluation (#10388)",
        ])
        .output()
        .map_err(|e| format!("could not exec git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git commit-tree failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let commit = String::from_utf8_lossy(&out.stdout).trim().to_string();
    git(d, &["update-ref", "HEAD", &commit])?;
    git(d, &["reset", "-q", "--hard", "HEAD"])?;
    Ok(dir)
}

/// The stderr/merge-log line: one greppable record per evaluation.
#[must_use]
pub fn log_line(pr: &str, verdict: &str, record: &Record) -> String {
    let parts: Vec<String> = record
        .results
        .iter()
        .map(|r| format!("{}={}", r.component, if r.passed { "pass" } else { "FAIL" }))
        .collect();
    format!(
        "LOOM-STALE-CHECKS-LOCAL-EVAL pr={pr} verdict={verdict} head={} base={} tree={} [{}]",
        record.head_sha,
        record.base_sha,
        record.tree_sha,
        parts.join("; ")
    )
}

/// The PR comment recording a local evaluation.
#[must_use]
pub fn comment_body(record: &Record, failure: Option<(&str, &str)>) -> String {
    let rows: String = record
        .results
        .iter()
        .map(|r| format!("| {} | {} |\n", r.component, if r.passed { "pass" } else { "**FAIL**" }))
        .collect();
    let head = format!(
        "<!-- loom:stale-checks-local-eval head={} base={} -->\n",
        record.head_sha, record.base_sha
    );
    let facts = format!(
        "- **Head**: `{}`\n- **Base evaluated**: `{}`\n- **Merge tree**: `{}`\n\n| component | result |\n|---|---|\n{rows}",
        record.head_sha, record.base_sha, record.tree_sha
    );
    match failure {
        None => format!(
            "{head}## Stale cheap checks re-evaluated locally on the merge tree (#10388)\n\n\
The #8248 freshness guard found these required-check components stale. Each is a cheap, \
deterministic function of the tree, so its CI steps were run on the exact merge tree (base + \
this head) instead of re-dating the PR for a full CI run. All passed, so the guard is satisfied \
for them.\n\n{facts}"
        ),
        Some((step, output)) => {
            let output = output.trim_end();
            let longest = output.split(|c| c != '`').map(str::len).max().unwrap_or(0);
            let fence = "`".repeat(longest.max(2) + 1);
            format!(
                "{head}## Merge blocked: a stale cheap check FAILS on the merge tree (#10388)\n\n\
The #8248 freshness guard found these components stale and ran their CI steps on the merge \
tree (base + this head). `{step}` failed, so the merge stays blocked: the PR and the current \
base disagree. Rebase onto the base and fix the failure.\n\n{facts}\n{fence}\n{output}\n{fence}"
            )
        }
    }
}

#[cfg(test)]
mod tests;
