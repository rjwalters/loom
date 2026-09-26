//! `loom-daemon sweep-outcomes` CLI surface: the flag set, the `summary`
//! sub-verb (Issue #8057), and the dispatch between them.
//!
//! **Why this is not in `main.rs`.** `main.rs` is over
//! `scripts/check-file-size-budget.sh`'s threshold, so it is frozen at its
//! current size: new CLI surface goes in a sibling module with a one-line
//! dispatch arm left behind. Moving the whole `sweep-outcomes` arg set here
//! (as a `#[command(flatten)]`-style `Args` struct) shrinks `main.rs` rather
//! than growing it, and parses byte-identically — a flattened `Args` struct
//! and an inline variant body produce the same `Command` tree.
//!
//! The handler bodies stay where they were: the pre-existing paths in
//! [`crate::cli::misc_cmds::handle_sweep_outcomes_command`] (untouched by
//! #8057) and the new grouped report in
//! [`loom_daemon::sweep_outcome_summary`].

use anyhow::{bail, Result};
use chrono::Utc;
use clap::{Args, Subcommand};

use super::misc_cmds::handle_sweep_outcomes_command;

/// Every `loom-daemon sweep-outcomes` argument, lifted verbatim out of
/// `main.rs`'s `Commands::SweepOutcomes` variant (Issue #8057). Field names,
/// flag spellings, defaults and doc comments are unchanged — this is a move,
/// not a redesign, and `tests::existing_sweep_outcomes_invocations_parse_exactly_as_before`
/// pins that.
#[derive(Args)]
pub(crate) struct SweepOutcomesArgs {
    /// Repo root whose `.loom/logs/sweep-outcome-telemetry.jsonl` to
    /// read (plain path, default `.` — no upward `.git` walk).
    #[arg(long, value_name = "PATH", default_value = ".")]
    pub workspace: String,

    /// Only include records for this dispatched model (matches the
    /// `"default"` group for records with no explicit model).
    #[arg(long)]
    pub model: Option<String>,

    /// Only include records with this terminal result: success, failure,
    /// cancelled, blocked.
    #[arg(long)]
    pub result: Option<String>,

    /// List individual records (newest first) instead of the
    /// summary-by-model table.
    #[arg(long)]
    pub records: bool,

    /// Cap the number of records considered (after filtering), newest
    /// first. Applies to both the summary and `--records` listing.
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,

    /// Print how many locally-journaled records the observability
    /// exporter has not yet attempted to send to the backend (Issue
    /// #5084) instead of the summary/records output — every other flag
    /// except `--json`/`--workspace` is ignored in this mode. This is the
    /// AC's "N local outcomes not yet in the backend" measurability
    /// gauge: `0` means the backfill drain (see
    /// `.loom/docs/observability.md`) is caught up.
    #[arg(long = "pending-export")]
    pub pending_export: bool,

    /// Emit machine-readable JSON instead of the human-readable table.
    #[arg(long)]
    pub json: bool,

    /// Sub-verb. Only `summary` exists (Issue #8057); with none, this
    /// command behaves exactly as it always has.
    #[command(subcommand)]
    pub command: Option<SweepOutcomesAction>,
}

/// Sub-actions for `loom-daemon sweep-outcomes` (Issue #8057).
#[derive(Subcommand)]
pub(crate) enum SweepOutcomesAction {
    /// Fleet-wide grouped summary of the `sweep.outcome` journals.
    ///
    /// This is a documented SUPERSET of the by-model summary the bare
    /// `loom-daemon sweep-outcomes` prints (#4137 AC4), which is NOT
    /// superseded and is byte-for-byte unchanged. The two use deliberately
    /// different names for different things: the old one reports
    /// `success_rate` over every record; this one reports `real_failure_rate`,
    /// which discounts spawn deaths (a run that died before doing any work).
    /// Do not compare them directly.
    ///
    /// Read-only and purely file-based — no running daemon, like the parent
    /// command. From a workspace registered in `~/.loom/workspaces.json` this
    /// sub-verb (and ONLY this sub-verb) defaults to reading every registered
    /// workspace; `--this-workspace` restores the single-workspace read.
    ///
    /// `--group-by tap` (#8556) cuts by `(runtime, credential source)` — the
    /// one dimension whose economics actually differ, and the cut "how much
    /// went to the metered backstop vs. the subscriptions" reduces to.
    ///
    /// Three caveats it prints rather than hides: `--group-by host` is
    /// degenerate on a local read (every envelope this host wrote carries
    /// this host's id, so there is exactly one bucket — the grouping is
    /// meaningful only over a pooled/exported corpus); `--group-by arm` marks
    /// an arm INFERRED from the dispatched model until #8055 stamps one; and
    /// the weighted-token figures price through a rate card the output names,
    /// whose rates are the known-stale Jan-2025 set (#8060).
    Summary {
        /// Repo root to read (plain path, default `.` — no upward `.git`
        /// walk). Also decides whether the all-workspaces default applies.
        #[arg(long, value_name = "PATH", default_value = ".")]
        workspace: String,

        /// Only consider records emitted since this point: `7d`, `36h`,
        /// `90m`, `2w`, or an absolute `YYYY-MM-DD` (00:00 UTC).
        #[arg(long, value_name = "SPEC")]
        since: Option<String>,

        /// Read every workspace in `~/.loom/workspaces.json`, even when run
        /// from an unregistered directory (the default already applies when
        /// the invoking workspace is itself registered).
        #[arg(long)]
        all_workspaces: bool,

        /// Read only `--workspace`, even when it is registered — the escape
        /// hatch from the all-workspaces default.
        #[arg(long, conflicts_with = "all_workspaces")]
        this_workspace: bool,

        /// Grouping dimension: arm, model, repo, host, day, tap, complexity,
        /// model-complexity.
        ///
        /// `tap` groups by `<runtime>@<credential source>` (#8556) — the cut
        /// that answers "how much went to the metered backstop vs. the
        /// subscriptions". A record with no explicit tap stamp is placed from
        /// the credential keys it does carry; see
        /// `sweep_outcome_summary::resolve_tap`. Such a record keeps a bare
        /// runtime key (no profile is invented for it), so one tap can show up
        /// as two rows across the #8625 stamping boundary — the report emits a
        /// note naming the affected keys when it does (#8634).
        #[arg(long, value_name = "DIM", default_value = "model")]
        group_by: String,

        /// Drop spawn deaths (a classified `preflight-token-selection-failed`
        /// / `account-exhausted:*`, or — with no class available — a sub-60s
        /// non-success) instead of only discounting them from the real
        /// failure rate. The number dropped is always reported.
        #[arg(long)]
        exclude_spawn_deaths: bool,

        /// Skip the `pr_number` -> `gh pr view --json mergedAt` join. Merged
        /// counts then report as unavailable (`null` / `n/a`), never as `0`.
        #[arg(long)]
        no_merge_join: bool,

        /// Emit machine-readable JSON instead of the human-readable table.
        #[arg(long)]
        json: bool,
    },
}

/// Route `loom-daemon sweep-outcomes` to the pre-existing handler or to the
/// `summary` sub-verb. The `None` arm is byte-for-byte the call `main.rs`
/// used to make.
pub(crate) fn dispatch(args: SweepOutcomesArgs) -> Result<()> {
    match args.command {
        Some(SweepOutcomesAction::Summary {
            workspace,
            since,
            all_workspaces,
            this_workspace,
            group_by,
            exclude_spawn_deaths,
            no_merge_join,
            json,
        }) => handle_summary(&SweepOutcomesSummaryArgs {
            workspace: &workspace,
            since: since.as_deref(),
            all_workspaces,
            this_workspace,
            group_by: &group_by,
            exclude_spawn_deaths,
            no_merge_join,
            json,
        }),
        None => handle_sweep_outcomes_command(
            &args.workspace,
            args.model.as_deref(),
            args.result.as_deref(),
            args.records,
            args.limit,
            args.pending_export,
            args.json,
        ),
    }
}

/// Arguments for `loom-daemon sweep-outcomes summary` (Issue #8057), bundled
/// into a struct rather than a nine-parameter function signature.
struct SweepOutcomesSummaryArgs<'a> {
    /// Repo root to resolve (and, absent `--all-workspaces`, the only journal
    /// read).
    pub workspace: &'a str,
    /// `--since` spec (`7d`, `36h`, `2026-09-22`, ...). `None` reads the whole
    /// journal.
    pub since: Option<&'a str>,
    /// Force the fleet-wide read even from an unregistered workspace.
    pub all_workspaces: bool,
    /// Force the single-workspace read even from a registered one.
    pub this_workspace: bool,
    /// `--group-by` dimension.
    pub group_by: &'a str,
    /// Drop spawn deaths instead of only discounting them.
    pub exclude_spawn_deaths: bool,
    /// Skip the forge merge join entirely (merged counts report as
    /// unavailable, never `0`).
    pub no_merge_join: bool,
    /// Emit JSON instead of the table.
    pub json: bool,
}

/// `loom-daemon sweep-outcomes summary` — the fleet-wide, grouped superset of
/// the by-model summary the bare command prints (Issue #8057).
///
/// Read-only and file-based, exactly like its parent: no running daemon, no
/// writes to any journal. The only thing it writes at all is the merged-PR
/// join's cache, and only when the join actually learned something.
///
/// **The default-widening rule is scoped to this sub-verb alone.** Running it
/// from a workspace registered in `~/.loom/workspaces.json` defaults to
/// reading every registered workspace; the pre-existing `sweep-outcomes`
/// paths (`--records`, the default summary, `--pending-export`) keep reading
/// exactly one workspace, so no existing script's output silently widens.
fn handle_summary(args: &SweepOutcomesSummaryArgs) -> Result<()> {
    use loom_daemon::sweep_outcome_summary as summary;
    use loom_daemon::workspace_registry::{self, WorkspaceRegistry};

    let group_by = summary::GroupBy::parse(args.group_by)?;
    let since = args
        .since
        .map(|spec| summary::parse_since(spec, Utc::now()))
        .transpose()?;

    // Resolving the repo root is only fatal when we actually need it: with
    // `--all-workspaces` the registry alone is a complete answer, so running
    // from outside any checkout is legitimate there.
    let repo_root = match loom_daemon::worktree_ops::repo::resolve_repo_root(args.workspace) {
        Ok(root) => Some(root),
        Err(e) if args.all_workspaces => {
            log::debug!("--all-workspaces: ignoring unresolvable workspace root: {e}");
            None
        }
        Err(e) => return Err(e),
    };

    // A missing/unreadable registry is not fatal: it just means "no fleet",
    // and the single-workspace read still answers.
    let registry = workspace_registry::default_registry_path()
        .and_then(|path| WorkspaceRegistry::load(&path))
        .unwrap_or_default();
    let registered = repo_root.as_ref().is_some_and(|root| {
        let normalized = workspace_registry::normalize_path(root);
        registry
            .workspaces
            .iter()
            .any(|w| workspace_registry::normalize_path(&w.root) == normalized)
    });

    let use_all = !args.this_workspace
        && (args.all_workspaces || registered)
        && !registry.workspaces.is_empty();
    let roots: Vec<std::path::PathBuf> = if use_all {
        registry.workspaces.iter().map(|w| w.root.clone()).collect()
    } else {
        match repo_root {
            Some(root) => vec![root],
            None => bail!(
                "no workspaces to read: ~/.loom/workspaces.json is empty and the current \
                 directory is not a Loom checkout"
            ),
        }
    };

    let opts = summary::SummaryOptions {
        group_by,
        since,
        exclude_spawn_deaths: args.exclude_spawn_deaths,
        merge_join_attempted: !args.no_merge_join,
    };

    let mut cached;
    let mut skip = summary::SkipMergeJoin;
    let mut report = if args.no_merge_join {
        summary::summarize_workspaces(&roots, opts, &mut skip)
    } else {
        match summary::CachedMergeLookup::with_defaults() {
            Ok(lookup) => {
                cached = lookup;
                let mut report = summary::summarize_workspaces(&roots, opts, &mut cached);
                report.merge_join.cache_path = Some(cached.cache_path().display().to_string());
                cached.flush();
                report
            }
            // No home directory for a cache: degrade to "unavailable", never
            // to an un-cached forge burst across 48 workspaces.
            Err(e) => {
                log::debug!("merge-join cache unavailable ({e}); reporting merged PRs as unknown");
                let mut report = summary::summarize_workspaces(&roots, opts, &mut skip);
                report.merge_join.degraded = true;
                report
            }
        }
    };
    if use_all {
        report.notes.push(format!(
            "read {} registered workspace(s) from ~/.loom/workspaces.json",
            roots.len()
        ));
    }

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", summary::render_text(&report));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// Parse a full argv through the real top-level `Cli`, so these tests see
    /// exactly the `Command` tree the binary builds — the `Args`-struct move
    /// this module performs is only safe if that tree is unchanged.
    fn parse(args: &[&str]) -> crate::Commands {
        crate::Cli::try_parse_from(args)
            .expect("parse")
            .command
            .expect("a subcommand")
    }

    /// One pre-change invocation and the parse it must still produce.
    struct LegacyCase {
        argv: &'static [&'static str],
        workspace: &'static str,
        model: Option<&'static str>,
        result: Option<&'static str>,
        records: bool,
        limit: Option<usize>,
        pending_export: bool,
        json: bool,
    }

    /// Issue #8057 regression: adding the `summary` sub-verb must not change
    /// how ANY pre-existing `sweep-outcomes` invocation parses. This is the
    /// in-repo half of the byte-identical-output criterion — the handler body
    /// these fields feed is untouched, so an identical parse means identical
    /// output.
    #[test]
    fn existing_sweep_outcomes_invocations_parse_exactly_as_before() {
        let cases = [
            LegacyCase {
                argv: &["loom-daemon", "sweep-outcomes"],
                workspace: ".",
                model: None,
                result: None,
                records: false,
                limit: None,
                pending_export: false,
                json: false,
            },
            LegacyCase {
                argv: &["loom-daemon", "sweep-outcomes", "--json"],
                workspace: ".",
                model: None,
                result: None,
                records: false,
                limit: None,
                pending_export: false,
                json: true,
            },
            LegacyCase {
                argv: &[
                    "loom-daemon",
                    "sweep-outcomes",
                    "--records",
                    "--limit",
                    "20",
                ],
                workspace: ".",
                model: None,
                result: None,
                records: true,
                limit: Some(20),
                pending_export: false,
                json: false,
            },
            LegacyCase {
                argv: &[
                    "loom-daemon",
                    "sweep-outcomes",
                    "--model",
                    "opus",
                    "--result",
                    "failure",
                    "--records",
                ],
                workspace: ".",
                model: Some("opus"),
                result: Some("failure"),
                records: true,
                limit: None,
                pending_export: false,
                json: false,
            },
            LegacyCase {
                argv: &[
                    "loom-daemon",
                    "sweep-outcomes",
                    "--pending-export",
                    "--json",
                ],
                workspace: ".",
                model: None,
                result: None,
                records: false,
                limit: None,
                pending_export: true,
                json: true,
            },
            LegacyCase {
                argv: &["loom-daemon", "sweep-outcomes", "--workspace", "/srv/repo"],
                workspace: "/srv/repo",
                model: None,
                result: None,
                records: false,
                limit: None,
                pending_export: false,
                json: false,
            },
        ];

        for case in &cases {
            let argv = case.argv;
            match parse(argv) {
                crate::Commands::SweepOutcomes(a) => {
                    let SweepOutcomesArgs {
                        workspace,
                        model,
                        result,
                        records,
                        limit,
                        pending_export,
                        json,
                        command,
                    } = a;
                    assert_eq!(workspace, case.workspace, "{argv:?}");
                    assert_eq!(model.as_deref(), case.model, "{argv:?}");
                    assert_eq!(result.as_deref(), case.result, "{argv:?}");
                    assert_eq!(records, case.records, "{argv:?}");
                    assert_eq!(limit, case.limit, "{argv:?}");
                    assert_eq!(pending_export, case.pending_export, "{argv:?}");
                    assert_eq!(json, case.json, "{argv:?}");
                    assert!(
                        command.is_none(),
                        "{argv:?} must route to the pre-existing handler, not the new sub-verb"
                    );
                }
                _ => panic!("{argv:?} did not parse as sweep-outcomes"),
            }
        }
    }

    #[test]
    fn summary_sub_verb_parses_its_own_flags() {
        match parse(&[
            "loom-daemon",
            "sweep-outcomes",
            "summary",
            "--since",
            "7d",
            "--group-by",
            "arm",
            "--all-workspaces",
            "--exclude-spawn-deaths",
            "--json",
        ]) {
            crate::Commands::SweepOutcomes(args) => match args.command {
                Some(SweepOutcomesAction::Summary {
                    workspace,
                    since,
                    all_workspaces,
                    this_workspace,
                    group_by,
                    exclude_spawn_deaths,
                    no_merge_join,
                    json,
                }) => {
                    assert_eq!(workspace, ".");
                    assert_eq!(since.as_deref(), Some("7d"));
                    assert!(all_workspaces);
                    assert!(!this_workspace);
                    assert_eq!(group_by, "arm");
                    assert!(exclude_spawn_deaths);
                    assert!(!no_merge_join);
                    assert!(json);
                }
                None => panic!("summary sub-verb was not routed"),
            },
            _ => panic!("wrong command"),
        }
    }

    /// `--group-by` defaults to `model` so a bare `summary` answers the same
    /// question the bare parent command does, only fleet-wide.
    #[test]
    fn summary_defaults_to_group_by_model_and_a_live_merge_join() {
        match parse(&["loom-daemon", "sweep-outcomes", "summary"]) {
            crate::Commands::SweepOutcomes(SweepOutcomesArgs {
                command:
                    Some(SweepOutcomesAction::Summary {
                        group_by,
                        no_merge_join,
                        all_workspaces,
                        this_workspace,
                        since,
                        ..
                    }),
                ..
            }) => {
                assert_eq!(group_by, "model");
                assert!(!no_merge_join);
                assert!(!all_workspaces);
                assert!(!this_workspace);
                assert!(since.is_none());
            }
            _ => panic!("summary did not parse"),
        }
    }

    /// `--all-workspaces` and `--this-workspace` are mutually exclusive: the
    /// default-widening rule needs exactly one answer, not a precedence quiz.
    #[test]
    fn all_workspaces_and_this_workspace_conflict() {
        assert!(crate::Cli::try_parse_from([
            "loom-daemon",
            "sweep-outcomes",
            "summary",
            "--all-workspaces",
            "--this-workspace",
        ])
        .is_err());
    }
}
