//! Hard label exclusions — labels that take an issue out of the automated
//! pipeline entirely (Issue #7528).
//!
//! # The gap this closes
//!
//! `external` (an issue filed by a non-collaborator, or auto-labeled by an
//! intake workflow, that a maintainer must clear before any agent touches it)
//! was enforced **only** inside the markdown role prompts' `gh issue list
//! --jq` filters (`curator.md`, `builder.md`). The daemon's autonomous work
//! finder knew nothing about it, so an issue carrying `loom:issue` +
//! `external` was:
//!
//! 1. listed as a ready candidate and dispatched,
//! 2. given a `loom:building` claim and a full agent session (~90s of a token
//!    account's session budget),
//! 3. declined by the role prompt on the `external` rule,
//! 4. and then had its claim released straight back to `loom:issue` by the
//!    reaper's checkpoint-less clean-exit path — so the very next work-finder
//!    tick re-picked it.
//!
//! 23 dispatches in roughly one hour on rjwalters/kicad-tools#5197, with
//! nothing in the daemon log naming the loop.
//!
//! # Where the list lives
//!
//! [`HARD_EXCLUSION_LABELS`] below is the value the daemon uses. It is a
//! compile-time const on purpose: this list is consulted once per candidate
//! per work-finder tick, so it must not shell out, read a file, or depend on a
//! resolved repo root — all three are costs the pre-#7528 filter did not pay
//! and all three can fail in ways that would silently *weaken* an exclusion.
//!
//! The shell/markdown side reads `defaults/scripts/hard-exclusion-labels.sh`,
//! which carries the same list in one array plus the `--jq-not` / `--search`
//! renderings the role prompts paste into their queries. The two are kept in
//! lockstep by [`tests::rust_const_matches_shipped_shell_script`], which parses
//! the shipped script and fails the build on any divergence — the same
//! "constants must stay in lockstep, enforced by a test" shape
//! [`crate::work_finder::SKIP_LABELS`] and `PARK_LABELS` already use.
//!
//! # What this is *not*
//!
//! - Not a per-repo knob. `autonomous.workFinder.extraSkipLabels` (Issue
//!   #6685) is the per-repo/per-workspace extension point; this list is the
//!   fleet-wide floor Loom ships with, deliberately not overridable from repo
//!   config or an env var (weakening it is what produced the loop above).
//! - Not a park. A [`crate::work_finder::PARK_LABELS`] entry says "a human
//!   took this out of the queue"; a hard exclusion says "no agent has standing
//!   to act on this issue at all until the label is removed". They are counted
//!   separately on the tick line (`declined-skip` vs `labeled-skip`) so an
//!   operator can tell an intake backlog from an operator park.

/// Labels that exclude an issue from **every** automated role and from
/// work-finder dispatch, until a maintainer removes the label (Issue #7528).
///
/// Kept in lockstep with `defaults/scripts/hard-exclusion-labels.sh` by
/// [`tests::rust_const_matches_shipped_shell_script`]. Edit both (the test
/// will tell you if you forgot one).
pub const HARD_EXCLUSION_LABELS: &[&str] = &["external"];

/// The first [`HARD_EXCLUSION_LABELS`] entry `labels` carries, or `None` when
/// the issue carries none of them (the overwhelmingly common case).
///
/// Returns the label *name* rather than a bool so every caller can name the
/// rule it declined on — the log lines, the `declined-skip` accounting, and
/// the reaper's decline record all quote it, which is exactly what the pre-fix
/// daemon log was missing.
#[must_use]
pub fn declining_label(labels: &[String]) -> Option<&'static str> {
    HARD_EXCLUSION_LABELS
        .iter()
        .copied()
        .find(|excluded| labels.iter().any(|l| l == excluded))
}

/// True when `labels` carries any [`HARD_EXCLUSION_LABELS`] entry.
#[must_use]
pub fn is_hard_excluded(labels: &[String]) -> bool {
    declining_label(labels).is_some()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn declining_label_names_the_rule() {
        assert_eq!(declining_label(&["loom:issue".into(), "external".into()]), Some("external"));
        assert!(is_hard_excluded(&["external".into()]));
    }

    #[test]
    fn an_ordinary_ready_issue_is_not_excluded() {
        assert_eq!(declining_label(&["loom:issue".into(), "loom:curated".into()]), None);
        assert!(!is_hard_excluded(&[]));
    }

    /// A near-miss must not match: the check is exact-name, not substring, so
    /// a repo-local label like `external-dependency` (or a `loom:external`
    /// that never existed) does not silently inherit the fleet-wide floor.
    #[test]
    fn matching_is_exact_not_substring() {
        assert_eq!(declining_label(&["external-dependency".into()]), None);
        assert_eq!(declining_label(&["loom:external".into()]), None);
    }

    /// Parse the `HARD_EXCLUSION_LABELS=( ... )` array out of the shipped
    /// shell accessor. Deliberately a dumb line parser rather than an
    /// `eval`/`bash -c`: the test must fail loudly if the script's shape
    /// changes, not quietly start reading nothing.
    fn labels_from_shell_script(text: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut inside = false;
        for raw in text.lines() {
            let line = raw.trim();
            if !inside {
                if line.starts_with("HARD_EXCLUSION_LABELS=(") {
                    inside = true;
                }
                continue;
            }
            if line.starts_with(')') {
                return out;
            }
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            out.push(line.trim_matches(['"', '\'']).to_string());
        }
        panic!(
            "defaults/scripts/hard-exclusion-labels.sh: could not find a closing `)` for the \
             HARD_EXCLUSION_LABELS=( ... ) array — the shared-source contract in \
             loom-daemon/src/hard_exclusion.rs depends on that shape"
        );
    }

    /// The #7528 lockstep guard. `HARD_EXCLUSION_LABELS` and the shipped
    /// `defaults/scripts/hard-exclusion-labels.sh` array are two renderings of
    /// ONE list (the Rust side because a per-tick filter cannot shell out, the
    /// shell side because the markdown role prompts can only paste shell).
    /// This test is what makes "one shared source" true rather than aspirational.
    #[test]
    fn rust_const_matches_shipped_shell_script() {
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("loom-daemon/ has a parent")
            .join("defaults/scripts/hard-exclusion-labels.sh");
        let text = std::fs::read_to_string(&script)
            .unwrap_or_else(|e| panic!("read {}: {e}", script.display()));
        let shell = labels_from_shell_script(&text);
        let rust: Vec<String> = HARD_EXCLUSION_LABELS
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        assert_eq!(
            shell,
            rust,
            "HARD_EXCLUSION_LABELS (Rust) and {} disagree — #7528 requires ONE shared list. \
             Update both.",
            script.display()
        );
    }

    /// The shell accessor's `--jq-not` rendering is what the curator/builder
    /// prompts paste; assert it actually names every label in the const, so a
    /// label added to both lists but mis-rendered by the script cannot slip
    /// past the role prompts.
    #[test]
    fn shell_jq_not_rendering_covers_every_label() {
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("loom-daemon/ has a parent")
            .join("defaults/scripts/hard-exclusion-labels.sh");
        let out = std::process::Command::new("bash")
            .arg(&script)
            .arg("--jq-not")
            .output()
            .unwrap_or_else(|e| panic!("run {} --jq-not: {e}", script.display()));
        assert!(out.status.success(), "--jq-not exited non-zero");
        let rendered = String::from_utf8_lossy(&out.stdout);
        for label in HARD_EXCLUSION_LABELS {
            assert!(
                rendered.contains(&format!("contains([\"{label}\"]) | not")),
                "--jq-not output {rendered:?} does not exclude {label}"
            );
        }
    }
}
