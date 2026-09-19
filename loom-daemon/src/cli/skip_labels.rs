//! `loom-daemon skip-labels` — the combined "not a work item" label list for
//! a role prompt's unfiltered fallback query (Issue #8255).
//!
//! # The gap this closes
//!
//! `defaults/scripts/hard-exclusion-labels.sh` (#7528) is deliberately a
//! FIXED, fleet-wide list — `external` today — and is explicitly documented
//! as **not** a per-repo knob (see that script's header and
//! [`crate::hard_exclusion`]'s "What this is not"). A consumer repo cannot
//! add a repo-local non-work label to it.
//!
//! `autonomous.workFinder.extraSkipLabels` (#6685) is exactly that per-repo
//! extension point, and the daemon's own work finder already reads it
//! ([`crate::work_finder::resolve_extra_skip_labels_with_config`]) when
//! deciding what to dispatch. But `curator.md`'s Priority-2 fallback query
//! (and the analogous hand-maintained exclusion lists in `builder.md` and
//! `guide.md`) only ever called `hard-exclusion-labels.sh`, so the two halves
//! of the pipeline disagreed about what is dispatchable: 2AMLogic/2am's
//! long-lived `journal` status label (upstream 2am#582/#625) was never
//! excluded from the shell-side query and kept surfacing as a curation
//! candidate every fallback pass, even though the work finder itself already
//! knew to skip it.
//!
//! This subcommand is the missing shell-facing half: the fleet-wide hard
//! exclusions plus this workspace's configured `extraSkipLabels`, resolved
//! through the exact same config/env precedence
//! ([`crate::work_finder::read_work_finder_config`] +
//! [`crate::work_finder::resolve_extra_skip_labels_with_config`]) the work
//! finder itself uses, rendered in the same four modes
//! `hard-exclusion-labels.sh` already offers so it is a drop-in superset. A
//! repo with no `extraSkipLabels` configured gets byte-identical output to
//! `hard-exclusion-labels.sh` (Acceptance Criterion 2).
//!
//! `defaults/scripts/skip-labels.sh` is the thin stub role prompts invoke by
//! path; this module is where the logic actually lives, per
//! `.loom/docs/shell-language-policy.md`.

use std::path::PathBuf;

use anyhow::Result;

use loom_daemon::hard_exclusion::HARD_EXCLUSION_LABELS;
use loom_daemon::work_finder::{read_work_finder_config, resolve_extra_skip_labels_with_config};

#[derive(clap::Args)]
pub(crate) struct SkipLabelsArgs {
    /// One label name per line (the default when no rendering flag is given).
    #[arg(long)]
    pub lines: bool,

    /// Render as a JSON array of label names.
    #[arg(long)]
    pub json: bool,

    /// Render as a jq boolean expression, TRUE when an issue carries none of
    /// the labels — for `gh issue list --jq 'select(...)'`.
    #[arg(long = "jq-not")]
    pub jq_not: bool,

    /// Render as gh/forge search qualifiers excluding the labels, e.g.
    /// `-label:"journal"`.
    #[arg(long)]
    pub search: bool,

    /// Repo root to resolve `autonomous.workFinder.extraSkipLabels` from
    /// (the same tier-chain config `read_work_finder_config` resolves).
    /// Defaults to the current directory.
    #[arg(long, value_name = "PATH")]
    pub repo_root: Option<PathBuf>,
}

/// The fleet-wide hard exclusions ([`HARD_EXCLUSION_LABELS`]) followed by
/// this workspace's configured `autonomous.workFinder.extraSkipLabels`
/// (env > config > default, mirroring the work finder's own precedence),
/// deduplicated with the hard-exclusion ordering preserved.
///
/// `loom:building` can never appear here even if named in config/env —
/// `resolve_extra_skip_labels_with_config` already filters it defensively,
/// and this function does not need to repeat that guard.
#[must_use]
pub(crate) fn resolve(repo_root: &std::path::Path) -> Vec<String> {
    let mut out: Vec<String> = HARD_EXCLUSION_LABELS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    let config = read_work_finder_config(repo_root);
    for label in resolve_extra_skip_labels_with_config(&config) {
        if !out.contains(&label) {
            out.push(label);
        }
    }
    out
}

/// Render `labels` as the `--jq-not` fragment: a jq boolean, TRUE when an
/// issue's `.labels[].name` array contains none of them. Multiple labels are
/// ANDed, matching `hard-exclusion-labels.sh`'s rendering exactly.
#[must_use]
pub(crate) fn render_jq_not(labels: &[String]) -> String {
    labels
        .iter()
        .map(|l| format!("([.labels[].name] | contains([\"{l}\"]) | not)"))
        .collect::<Vec<_>>()
        .join(" and ")
}

/// Render `labels` as `-label:"..."` search qualifiers, space-separated.
#[must_use]
pub(crate) fn render_search(labels: &[String]) -> String {
    labels
        .iter()
        .map(|l| format!("-label:\"{l}\""))
        .collect::<Vec<_>>()
        .join(" ")
}

impl SkipLabelsArgs {
    pub(crate) fn run(self) -> Result<()> {
        let root = match self.repo_root {
            Some(r) => r,
            None => std::env::current_dir()?,
        };
        let labels = resolve(&root);

        if self.json {
            println!("{}", serde_json::to_string(&labels)?);
        } else if self.jq_not {
            println!("{}", render_jq_not(&labels));
        } else if self.search {
            println!("{}", render_search(&labels));
        } else {
            for l in &labels {
                println!("{l}");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_config(dir: &std::path::Path, extra_skip_labels: &[&str]) {
        std::fs::create_dir_all(dir.join(".loom")).unwrap();
        let json = serde_json::json!({
            "autonomous": {
                "workFinder": {
                    "extraSkipLabels": extra_skip_labels,
                }
            }
        });
        let mut f = std::fs::File::create(dir.join(".loom/config.json")).unwrap();
        f.write_all(json.to_string().as_bytes()).unwrap();
    }

    /// AC2: default behavior with no `extraSkipLabels` is unchanged — the
    /// resolved list is exactly the fleet-wide hard-exclusion list.
    #[test]
    fn no_config_matches_hard_exclusion_only() {
        let dir = tempfile::tempdir().unwrap();
        let resolved = resolve(dir.path());
        assert_eq!(
            resolved,
            HARD_EXCLUSION_LABELS
                .iter()
                .map(|s| (*s).to_string())
                .collect::<Vec<_>>()
        );
    }

    /// AC1: a repo with `"extraSkipLabels": ["journal"]` folds `journal`
    /// into the resolved list, alongside the fleet-wide `external` (2am's
    /// motivating case, upstream 2AMLogic/2am#625).
    #[test]
    fn extra_skip_labels_from_config_are_included() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), &["journal"]);
        let resolved = resolve(dir.path());
        assert!(resolved.contains(&"external".to_string()));
        assert!(resolved.contains(&"journal".to_string()));
    }

    /// `loom:building` must never be resolvable, even via an explicit
    /// (mis)configuration — mirrors the work finder's own defensive filter.
    #[test]
    fn loom_building_is_never_included() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), &["journal", "loom:building"]);
        let resolved = resolve(dir.path());
        assert!(resolved.contains(&"journal".to_string()));
        assert!(!resolved.contains(&"loom:building".to_string()));
    }

    /// Duplicate entries between the hard-exclusion floor and a repo's
    /// config (an operator naming `external` again) must not appear twice.
    #[test]
    fn duplicate_of_hard_exclusion_label_is_deduped() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), &["external", "journal"]);
        let resolved = resolve(dir.path());
        assert_eq!(resolved.iter().filter(|l| *l == "external").count(), 1);
        assert!(resolved.contains(&"journal".to_string()));
    }

    #[test]
    fn jq_not_rendering_ands_every_label() {
        let labels = vec!["external".to_string(), "journal".to_string()];
        let rendered = render_jq_not(&labels);
        assert_eq!(
            rendered,
            "([.labels[].name] | contains([\"external\"]) | not) and \
             ([.labels[].name] | contains([\"journal\"]) | not)"
        );
    }

    #[test]
    fn search_rendering_quotes_every_label() {
        let labels = vec!["external".to_string(), "journal".to_string()];
        assert_eq!(render_search(&labels), "-label:\"external\" -label:\"journal\"");
    }
}
