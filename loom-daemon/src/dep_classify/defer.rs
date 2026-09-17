//! The `--check-defer` decision — the Rust port of
//! `classify-dependency-block.sh`'s `check_defer` (epic #7810, PR 3).
//!
//! Answers: *should this proposal be parked on an open dependency?* Champion
//! calls it before escalating anything to an operator.
//!
//! # Shape
//!
//! The shell interleaved forge reads, decisions and `echo`/`exit` calls in one
//! function, so no part of it could be exercised without a `gh` stub on `PATH`.
//! Here the **decision** is a pure function over already-gathered inputs
//! ([`Inputs`]) returning a value ([`Decision`]); rendering and exit codes live
//! at the CLI boundary, and the forge reads live in [`super::state`].
//!
//! That split is what makes the ordering below testable at all — and the
//! ordering is the substance, because each early return means something
//! different to the caller.

use super::fingerprint::fingerprint;
use super::state::ClassifiedRefs;

/// Everything the decision needs, already gathered.
#[derive(Debug, Clone, Default)]
pub struct Inputs {
    /// The issue body — source of the startable subset, and the fallback source
    /// of blocker references.
    pub body: String,
    /// The verdict text findings are extracted from: an explicit
    /// `--findings-file`, else the **last** comment containing the rejection
    /// marker.
    pub source_body: String,
    /// Blocker references, already classified by the forge.
    pub refs: ClassifiedRefs,
    /// Whether the dependency graph contains a cycle.
    pub has_cycle: bool,
}

/// What `--check-defer` decided.
///
/// Five outcomes, not two. Each carries a distinct exit code at the CLI
/// boundary, and collapsing any pair changes what Champion does next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Do not defer. `reason` is one of the shell's fixed tokens — callers
    /// match on it, so the strings are part of the contract.
    NoDefer { reason: &'static str },
    /// Every recorded blocker has closed: re-evaluate rather than keep waiting.
    Reevaluate { cleared: Vec<String> },
    /// Blockers remain, but the issue declares work that does not depend on
    /// them — promote that subset instead of parking the whole issue.
    PromoteSubset { open: Vec<String>, subset: String },
    /// Park it: real, open, non-cyclic blockers and nothing startable.
    Defer {
        open: Vec<String>,
        blocker_fingerprint: String,
    },
}

/// Blocker references for a defer decision.
///
/// Findings first, falling back to the issue body — a Champion verdict names
/// the blocker it actually objected to, which is more precise than every
/// dependency the body happens to declare. The fallback exists because older
/// verdicts did not always cite one.
///
/// **The two sides use different parsers, on purpose.** The findings have
/// already passed [`super::finding::findings_are_dependency_only`], so every
/// reference on them is a blocker and no phrase gate applies:
/// [`super::refs::extract_refs`]. The body has not, so most `#N` mentions in it
/// are not dependencies at all, and it keeps the gate:
/// [`super::refs::parse_dependency_refs`]. Using the gated parser on both — the
/// obvious simplification — silently drops a verdict that says "blocked by #9"
/// in lower case, or cites the unblocking PR by URL.
///
/// `self_node` is dropped: an issue listed as its own blocker is a typo, and
/// treating it as real would park the issue on itself forever.
#[must_use]
pub fn resolve_blockers(findings: &str, body: &str, repo: &str, self_node: &str) -> Vec<String> {
    let from_findings = drop_self(super::refs::extract_refs(findings, repo), self_node);
    if !from_findings.is_empty() {
        return from_findings;
    }
    drop_self(super::refs::parse_dependency_refs(body, repo), self_node)
}

fn drop_self(refs: Vec<String>, self_node: &str) -> Vec<String> {
    refs.into_iter().filter(|r| r != self_node).collect()
}

/// Drop `[premise-false]`-tagged bullets from a finding set (#7904).
///
/// That finding kind self-clears through its **own** gate — re-verifying the
/// cited mechanical check against current `main` — never by waiting on a
/// blocker. It is not dependency-shaped, so leaving it in makes
/// [`super::finding::findings_are_dependency_only`] fail, and a set that mixed
/// one premise-false bullet with a genuine open-dependency bullet fell through
/// to ordinary escalation instead of deferring.
///
/// Scoped to the defer decision only. An already-escalated issue's
/// `--check-unescalate` path is a different life-cycle question and is
/// deliberately untouched.
#[must_use]
pub fn strip_premise_false(findings: &str) -> String {
    findings
        .lines()
        .filter(|line| {
            let t = line.trim_start();
            let Some(rest) = t.strip_prefix(['-', '*']) else {
                return true;
            };
            !rest.trim_start().starts_with("[premise-false]")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The blockers a caller must classify with the forge before [`decide`] can
/// answer — empty when a gate *before* classification already settles it.
///
/// This exists so the I/O boundary does not have to re-implement the gate
/// order to know whether a read is warranted. The shell's bounded-cost promise
/// is explicit ("a proposal with no dependency findings costs exactly one
/// read"), and an eager classify would quietly break it: every proposal with a
/// merits objection would start paying one forge call per cited reference.
///
/// [`decide`] re-runs the same gates, so a caller that classifies nothing still
/// gets the right refusal.
#[must_use]
pub fn blockers_to_classify(inputs: &Inputs, repo: &str, self_node: &str) -> Vec<String> {
    if inputs.source_body.chars().all(char::is_whitespace) {
        return Vec::new();
    }
    let findings = super::findings::extract_findings(&inputs.source_body);
    if findings.chars().all(char::is_whitespace) {
        return Vec::new();
    }
    // The stripped set, matching `decide` — see `strip_premise_false`.
    let findings = strip_premise_false(&findings);
    if findings.chars().all(char::is_whitespace)
        || !super::finding::findings_are_dependency_only(&findings)
    {
        return Vec::new();
    }
    resolve_blockers(&findings, &inputs.body, repo, self_node)
}

/// Decide whether to defer.
///
/// The order of these checks is the contract. Each early return is a different
/// answer, and moving one changes which reason Champion records:
///
/// 1. no findings at all → nothing was objected to
/// 2. every finding is `[premise-false]` → not a wait and not a merits
///    objection; its own reason, so the caller can route to the close gate
/// 3. findings exist but are not dependency-only → a **merits** objection,
///    which waiting will never resolve
/// 4. no blocker reference → nothing concrete to wait on
/// 5. every blocker closed → re-evaluate
/// 6. the graph is cyclic → waiting is futile, so do not defer
/// 7. a startable subset exists → promote it rather than parking everything
/// 8. otherwise → defer
#[must_use]
pub fn decide(inputs: &Inputs, repo: &str, self_node: &str) -> Decision {
    if inputs.source_body.chars().all(char::is_whitespace) {
        return Decision::NoDefer {
            reason: "no-findings",
        };
    }

    let findings = super::findings::extract_findings(&inputs.source_body);
    if findings.chars().all(char::is_whitespace) {
        return Decision::NoDefer {
            reason: "no-findings",
        };
    }

    // #7904: classify on the finding set with `[premise-false]` bullets removed.
    let findings = strip_premise_false(&findings);
    if findings.chars().all(char::is_whitespace) {
        // EVERY finding was premise-false. Nothing left to classify as a
        // dependency wait — but this is not a merits finding either, and
        // saying so would route it to escalation. Its own distinct reason,
        // so the caller runs the premise-false close gate against the full,
        // unfiltered set instead.
        return Decision::NoDefer {
            reason: "premise-false-only",
        };
    }

    // A merits objection is not a timing one. Deferring here would park work a
    // human rejected on substance, and it would never come back. Checked AFTER
    // the strip, so a co-occurring premise-false bullet cannot manufacture one
    // — and BEFORE nothing, so a real merits finding alongside a premise-false
    // one still reports `merits-finding`, never `premise-false-only`.
    if !super::finding::findings_are_dependency_only(&findings) {
        return Decision::NoDefer {
            reason: "merits-finding",
        };
    }

    let blockers = resolve_blockers(&findings, &inputs.body, repo, self_node);
    if blockers.is_empty() {
        return Decision::NoDefer {
            reason: "no-recorded-blocker",
        };
    }

    if inputs.refs.open.is_empty() {
        return Decision::Reevaluate {
            cleared: inputs.refs.resolved.clone(),
        };
    }

    // Checked AFTER "all clear" deliberately: a cycle whose members have since
    // closed is not a live cycle, and reporting one would be misleading.
    if inputs.has_cycle {
        return Decision::NoDefer {
            reason: "dependency-cycle",
        };
    }

    let subset = super::subset::extract_startable_subset(&inputs.body);
    if !subset.chars().all(char::is_whitespace) {
        return Decision::PromoteSubset {
            open: inputs.refs.open.clone(),
            subset,
        };
    }

    Decision::Defer {
        blocker_fingerprint: fingerprint(&inputs.refs.open_joined()),
        open: inputs.refs.open.clone(),
    }
}

/// Render a decision as the shell's stdout, and its exit code.
///
/// Both are contract: role prompts parse these lines and branch on the code.
#[must_use]
pub fn render(decision: &Decision, unreadable: &[String]) -> (String, i32) {
    let mut out = String::new();

    // Unreadable references are reported BEFORE the verdict, on every path that
    // reaches classification — an operator needs to know the answer was formed
    // with incomplete information.
    if !unreadable.is_empty() {
        out.push_str(&format!("UNREADABLE: {}\n", unreadable.join(" ")));
    }

    match decision {
        Decision::NoDefer { reason } => {
            out.push_str("NO_DEFER\n");
            out.push_str(&format!("REASON: {reason}\n"));
            (out, 1)
        }
        Decision::Reevaluate { cleared } => {
            out.push_str("REEVALUATE\n");
            out.push_str("REASON: blockers-cleared\n");
            if !cleared.is_empty() {
                out.push_str(&format!("CLEARED_BLOCKERS: {}\n", cleared.join(" ")));
            }
            (out, 3)
        }
        Decision::PromoteSubset { open, subset } => {
            out.push_str("PROMOTE_SUBSET\n");
            out.push_str(&format!("OPEN_BLOCKERS: {}\n", open.join(" ")));
            out.push_str("STARTABLE_SUBSET:\n");
            out.push_str(subset);
            if !subset.ends_with('\n') {
                out.push('\n');
            }
            (out, 4)
        }
        Decision::Defer {
            open,
            blocker_fingerprint,
        } => {
            out.push_str("DEFER\n");
            out.push_str(&format!("OPEN_BLOCKERS: {}\n", open.join(" ")));
            out.push_str(&format!("BLOCKER_FINGERPRINT: {blocker_fingerprint}\n"));
            (out, 0)
        }
    }
}

#[cfg(test)]
mod tests;
