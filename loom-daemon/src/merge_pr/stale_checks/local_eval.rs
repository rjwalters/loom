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
//! # Default-on (#10465)
//!
//! On unless disabled: config `merge.reverifyStaleChecks` (default `true`),
//! env `LOOM_MERGE_REVERIFY_STALE_CHECKS` beating it ([`enabled_for_root`]).
//! Only the toolchain-free [`CHEAP_CHECKS`] allowlist is ever evaluated. Set
//! it to `false`/`0`/`off` and the guard behaves exactly as before #10388.
//!
//! # Fail closed
//!
//! Anything else leaves the guard's original refusal standing, so the existing
//! remedy runs: a stale non-allowlisted component (anything expensive), a
//! time-rule or unknown verdict, a repo-declared spec, a missing script or
//! required tool, a pinned tool at another version than CI's, a merge tree
//! that changes `ci.yml` or any `*.sh` relative to the judged base (only code
//! already on the base is ever executed here, see [`changes_check_code`] and
//! [`tree_check_code_changes`]), a step this module cannot run faithfully
//! (`uses:`, `env:`, a `${{ }}` expression, a conditional, a
//! [`ci_steps::DENIED_COMMANDS`] entry), a merge-tree conflict, a fetched base
//! that is not the base the assessment judged, a head that moved, a check that
//! could not start, a timeout, and a host-environment failure (exit 78/127, a
//! signal, a failing `--self-test`) — all [`Outcome::Unknown`]. A check that
//! runs and fails is [`Outcome::Failed`]: the merge stays blocked and the
//! failure is posted.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use super::inputs::{self, ScopedEvidence};
use super::workflow_scope::CiScopes;
use super::{assess_scoped, CheckRun, Verdict};
use crate::merge_pr::tree_checks::{self, git, merge_tree_with};

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
    /// Tools whose host copy must be the exact version CI installs.
    pub pins: &'static [ToolPin],
}

/// A host tool that must match the version a (skipped) CI install step pins:
/// slug and parsing rules differ between releases, so another version is a
/// false-pass path, not a substitute.
#[derive(Debug, Clone, Copy)]
pub struct ToolPin {
    /// The binary on `PATH`; its `--version` output must name the pin.
    pub tool: &'static str,
    /// The CI step (one of `skip_steps`) that installs it.
    pub step: &'static str,
    /// The variable that step assigns the version to (`VER=v0.24.2`).
    pub var: &'static str,
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
        pins: &[],
    },
    CheapCheck {
        component: "File Size Ratchet",
        script: "scripts/check-file-size-budget.sh",
        requires: &["bash", "git"],
        skip_steps: &[],
        pins: &[],
    },
    CheapCheck {
        component: "Markdown Token Ratchet",
        script: "scripts/check-markdown-token-budget.sh",
        requires: &["bash", "git"],
        skip_steps: &[],
        pins: &[],
    },
    CheapCheck {
        component: "Role Prompt Prefix Ratchet",
        script: "scripts/check-role-prompt-budget.sh",
        requires: &["bash", "git"],
        skip_steps: &[],
        pins: &[],
    },
    CheapCheck {
        component: "Docs/Defaults Parity Check",
        script: "scripts/check-docs-defaults-parity.sh",
        requires: &["bash", "git"],
        skip_steps: &[],
        pins: &[],
    },
    CheapCheck {
        component: "Doc Table-of-Contents Freshness",
        script: "scripts/check-doc-tocs.sh",
        requires: &["bash", "git"],
        skip_steps: &[],
        pins: &[],
    },
    CheapCheck {
        component: "Vendored Private-Reference Scrub",
        script: "scripts/check-vendored-private-refs.sh",
        requires: &["bash", "git", "grep"],
        skip_steps: &[],
        pins: &[],
    },
    CheapCheck {
        component: "Dangling Link Check",
        script: "scripts/check-dangling-links.sh",
        requires: &["bash", "git", "lychee"],
        skip_steps: &[LYCHEE_INSTALL_STEP],
        pins: &[ToolPin {
            tool: "lychee",
            step: LYCHEE_INSTALL_STEP,
            var: "VER",
        }],
    },
];

/// `ci.yml`'s lychee install step (skipped locally; read for its pin).
const LYCHEE_INSTALL_STEP: &str = "Install lychee (pinned, checksum-verified)";

pub mod ci_steps;
pub use ci_steps::ci_steps;

/// Where the steps are read from, relative to the merge tree.
pub const CI_WORKFLOW_PATH: &str = inputs::CI_WORKFLOW;

/// Per-step timeout default; generous against the ~1–8 s these take.
pub const DEFAULT_TIMEOUT_SECS: u64 = 180;

/// Config key enabling local re-verification (default `true`, #10465).
pub const CONFIG_KEY: &str = "merge.reverifyStaleChecks";

/// Env override for [`CONFIG_KEY`] (`1`/`true`/`yes`/`on` or
/// `0`/`false`/`no`/`off`; anything else falls through to the config).
pub const ENABLE_ENV: &str = "LOOM_MERGE_REVERIFY_STALE_CHECKS";

/// The PR comment marker, one per evaluated (base, head, tree).
pub const MARKER: &str = "loom:merge-tree-reverify";

/// Is local re-verification enabled for the repository at `root`?
/// env > config > default (`true`).
#[must_use]
pub fn enabled_for_root(root: &Path) -> bool {
    resolve_enabled(
        std::env::var(ENABLE_ENV).ok().as_deref(),
        &crate::config_resolver::resolve_effective_config(root),
    )
}

/// [`enabled_for_root`] over explicit inputs. An unparseable value at either
/// tier falls through to the next one. A numeric config value is off only
/// when it is `0` (any other number is on), matching `merge-pr.sh`'s
/// `_mp_warn_reverify_floor`, whose `jq tostring` makes `0` read as `"0"`.
#[must_use]
pub fn resolve_enabled(env: Option<&str>, config: &serde_json::Value) -> bool {
    env.and_then(parse_flag)
        .or_else(|| {
            crate::config_resolver::get_path(config, CONFIG_KEY).and_then(|v| {
                v.as_bool()
                    .or_else(|| v.as_str().and_then(parse_flag))
                    .or_else(|| v.as_f64().map(|n| n != 0.0))
            })
        })
        .unwrap_or(true)
}

fn parse_flag(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
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
    // Fast path only: the forge's file list is capped (3000 files, silently),
    // so the authoritative test is [`tree_check_code_changes`] in `evaluate`.
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

/// The authoritative trust-boundary test: the [`changes_check_code`] paths
/// that differ between the judged base tip and the merge tree, read from git
/// objects with `git diff <base> <tree>`. Unlike the forge's file list (capped
/// at 3000 entries with no truncation signal) it cannot miss a path, and it
/// is immune to a head that moved after the delta was read. `Err` when the
/// diff cannot be read; the caller fails closed.
pub fn tree_check_code_changes(
    repo_root: &Path,
    base_sha: &str,
    tree_sha: &str,
) -> Result<Vec<String>, String> {
    let out = Command::new("git")
        .current_dir(repo_root)
        .args([
            "diff",
            "--name-only",
            "--no-renames",
            "--no-ext-diff",
            "-z",
            base_sha,
            tree_sha,
            "--",
        ])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("could not exec git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git diff {base_sha} {tree_sha} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter(|p| !p.is_empty() && changes_check_code(p))
        .map(String::from)
        .collect())
}

/// One executed step, for the evidence comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepRun {
    /// The step's first command line (the full body is in `ci.yml`).
    pub command: String,
    /// `0` on a pass; the exit code on a failure, `None` for a signal.
    pub exit: Option<i32>,
    pub millis: u128,
}

/// One component's local result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentResult {
    pub component: String,
    pub passed: bool,
    pub steps: Vec<StepRun>,
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
    let tmp = std::env::temp_dir();
    let at = Target {
        repo_root,
        remote,
        pr,
        base_ref,
        head_sha,
        expected_base,
    };
    evaluate_in(&tmp, &at, components, checks, timeout)
}

/// Which merge [`evaluate_in`] checks out.
#[derive(Debug, Clone, Copy)]
pub struct Target<'a> {
    pub repo_root: &'a Path,
    pub remote: &'a str,
    pub pr: &'a str,
    pub base_ref: &'a str,
    pub head_sha: &'a str,
    /// The base tip the freshness assessment judged; a different fetched tip
    /// is no verdict.
    pub expected_base: &'a str,
}

/// [`evaluate`] with every temp dir (the checkout, the step scripts, the
/// output logs) created under `tmp_root` — the seam that lets a test prove
/// nothing is left behind.
pub fn evaluate_in(
    tmp_root: &Path,
    at: &Target<'_>,
    components: &[&str],
    checks: &[CheapCheck],
    timeout: Duration,
) -> Outcome {
    let Target {
        repo_root,
        remote,
        pr,
        base_ref,
        head_sha,
        expected_base,
    } = *at;
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
    // No FETCH_HEAD write: the primary repository's refs and state files stay
    // exactly as they were (its private fetch refs are deleted again).
    let mt = match merge_tree_with(repo_root, remote, pr, base_ref, head_sha, false) {
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
    match tree_check_code_changes(repo_root, &mt.base_sha, &mt.tree_sha) {
        Ok(code) if code.is_empty() => {}
        Ok(code) => {
            return Outcome::Unknown(format!(
                "the merge tree changes check code this host would execute ({}); only code \
already on the base runs locally, so CI must evaluate this PR",
                code.join(", ")
            ))
        }
        Err(e) => {
            return Outcome::Unknown(format!(
                "could not diff the merge tree against the base, so whether it changes check \
code is unknown: {e}"
            ))
        }
    }
    let checkout = match build_checkout_in(tmp_root, repo_root, &mt, head_sha) {
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
        .tempdir_in(tmp_root)
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
        if let Err(e) = check_pins(&ci_yml, k) {
            return Outcome::Unknown(format!("'{}': {e}", k.component));
        }
        let mut runs: Vec<StepRun> = Vec::new();
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
            let started = Instant::now();
            let ran = tree_checks::run_checks(checkout.path(), &cfg);
            let millis = started.elapsed().as_millis();
            match ran {
                tree_checks::Outcome::Clean => runs.push(StepRun {
                    command: first_line(step),
                    exit: Some(0),
                    millis,
                }),
                tree_checks::Outcome::Failed { output, .. } if tree_checks::is_timeout(&output) => {
                    return Outcome::Unknown(format!("'{}' timed out: {step}", k.component));
                }
                tree_checks::Outcome::Failed { output, .. } => {
                    let exit = exit_status(&output);
                    if let Some(why) = environment_failure(step, exit) {
                        // The host, not the PR: no verdict, and no "Merge
                        // blocked … fix the failure" comment sending agents
                        // after a defect that does not exist.
                        return Outcome::Unknown(format!(
                            "'{}': `{}` {why}; treated as no verdict, not a failing check",
                            k.component,
                            first_line(step)
                        ));
                    }
                    runs.push(StepRun {
                        command: first_line(step),
                        exit: exit.and_then(|e| e.ok()),
                        millis,
                    });
                    record.results.push(ComponentResult {
                        component: k.component.to_string(),
                        passed: false,
                        steps: runs,
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
            steps: runs,
        });
    }
    Outcome::Passed(record)
}

/// How a failed step ended, from [`tree_checks::run_checks`]' trailer:
/// `Ok(code)` for an exit, `Err(())` for a signal, `None` when unreadable.
fn exit_status(output: &str) -> Option<Result<i32, ()>> {
    let (_, tail) = output.trim_end().rsplit_once("\n(terminated by ")?;
    let what = tail.strip_suffix(')')?;
    if what == "a signal" {
        return Some(Err(()));
    }
    what.strip_prefix("exit ")?.parse().ok().map(Ok)
}

/// Is this failure the host's environment rather than the PR? Exit 78
/// (`EX_CONFIG`: a prerequisite such as `lychee` is missing), 127 (command
/// not found), a signal, or a failing `--self-test` (the checker itself does
/// not work here, e.g. a GNU-vs-BSD userland difference).
fn environment_failure(step: &str, exit: Option<Result<i32, ()>>) -> Option<&'static str> {
    match exit {
        Some(Ok(78)) => Some("exited 78 (a prerequisite is missing on this host)"),
        Some(Ok(127)) => Some("exited 127 (a command is not installed on this host)"),
        Some(Err(())) => Some("was killed by a signal"),
        _ if step.contains("--self-test") => {
            Some("failed its self-test, so the checker does not work on this host")
        }
        _ => None,
    }
}

fn first_line(step: &str) -> String {
    let line = step
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with("set -"))
        .or_else(|| step.lines().map(str::trim).find(|l| !l.is_empty()))
        .unwrap_or("");
    let mut out: String = line.chars().take(100).collect();
    if line.chars().count() > 100 {
        out.push('…');
    }
    out
}

/// Every [`ToolPin`] of `k` holds on this host: the tool's `--version` names
/// the version `ci.yml`'s install step pins.
fn check_pins(ci_yml: &str, k: &CheapCheck) -> Result<(), String> {
    for pin in k.pins {
        let body = ci_steps::step_run(ci_yml, k.component, pin.step)?;
        let want = ci_steps::assigned_value(&body, pin.var).ok_or_else(|| {
            format!("`{}` assigns no {}=, so the pin is unknown", pin.step, pin.var)
        })?;
        let have = tool_version(pin.tool)?;
        if !version_matches(&have, &want) {
            return Err(format!(
                "CI pins {} {want}, but this host has `{have}`; a different version can \
disagree with CI",
                pin.tool
            ));
        }
    }
    Ok(())
}

fn tool_version(tool: &str) -> Result<String, String> {
    let out = Command::new(tool)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("could not run `{tool} --version`: {e}"))?;
    if !out.status.success() {
        return Err(format!("`{tool} --version` failed"));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Does a `--version` line name exactly `want` (a leading `v` ignored on
/// both sides)? `lychee 0.24.2` matches `v0.24.2`; `0.24.20` does not.
#[must_use]
pub fn version_matches(have: &str, want: &str) -> bool {
    let want = want.trim().trim_start_matches('v');
    !want.is_empty()
        && have
            .split_whitespace()
            .any(|w| w.trim_start_matches('v') == want)
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
    build_checkout_in(&std::env::temp_dir(), repo_root, mt, head_sha)
}

/// [`build_checkout`] under `tmp_root`.
pub fn build_checkout_in(
    tmp_root: &Path,
    repo_root: &Path,
    mt: &tree_checks::MergeTree,
    head_sha: &str,
) -> Result<tempfile::TempDir, String> {
    let common = git(repo_root, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
    let dir = tempfile::Builder::new()
        .prefix("loom-local-eval-")
        .tempdir_in(tmp_root)
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
        "LOOM-MERGE-TREE-REVERIFY pr={pr} verdict={verdict} base={} head={} tree={} components=[{}]",
        record.base_sha,
        record.head_sha,
        record.tree_sha,
        parts.join("; ")
    )
}

/// The `<!-- loom:merge-tree-reverify base= head= tree= -->` marker.
#[must_use]
pub fn marker(record: &Record) -> String {
    format!(
        "<!-- {MARKER} base={} head={} tree={} -->",
        record.base_sha, record.head_sha, record.tree_sha
    )
}

/// The PR comment recording a local evaluation: per component, each step's
/// command, exit code and duration.
#[must_use]
pub fn comment_body(record: &Record, failure: Option<(&str, &str)>) -> String {
    let mut rows = String::new();
    for r in &record.results {
        let result = if r.passed { "pass" } else { "**FAIL**" };
        for (i, st) in r.steps.iter().enumerate() {
            let exit = st.exit.map_or("signal".to_string(), |c| c.to_string());
            let name = if i == 0 { r.component.as_str() } else { "" };
            let res = if i + 1 == r.steps.len() { result } else { "" };
            let cmd = st.command.replace('|', "\\|").replace('`', "'");
            rows.push_str(&format!(
                "| {name} | `{cmd}` | {exit} | {:.1}s | {res} |\n",
                st.millis as f64 / 1000.0
            ));
        }
    }
    let head = format!("{}\n", marker(record));
    let facts = format!(
        "- **Head**: `{}`\n- **Base evaluated**: `{}`\n- **Merge tree**: `{}`\n\n\
| component | command | exit | time | result |\n|---|---|---|---|---|\n{rows}",
        record.head_sha, record.base_sha, record.tree_sha
    );
    match failure {
        None => format!(
            "{head}## Stale cheap checks re-verified on the merge tree (#10388)\n\n\
The #8248 freshness guard found these required-check components stale. Each is a cheap, \
deterministic function of the tree, so its CI steps were run on the exact merge tree (base + \
this head) instead of re-dating the PR for a full CI run. All passed, so the guard is satisfied \
for them (`{CONFIG_KEY}` is on for this repository).\n\n{facts}"
        ),
        Some((step, output)) => {
            let output = output.trim_end();
            let longest = output.split(|c| c != '`').map(str::len).max().unwrap_or(0);
            let fence = "`".repeat(longest.max(2) + 1);
            format!(
                "{head}## Merge blocked: a stale cheap check FAILS on the merge tree (#10388)\n\n\
The #8248 freshness guard found these components stale and ran their CI steps on the merge \
tree (base + this head). `{}` failed, so the merge stays blocked: the PR and the current \
base disagree. Rebase onto the base and fix the failure.\n\n{facts}\n{fence}\n{output}\n{fence}",
                first_line(step)
            )
        }
    }
}

#[cfg(test)]
mod tests;
