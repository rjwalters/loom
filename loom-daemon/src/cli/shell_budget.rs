//! `loom-daemon shell-budget` — how far epic #7810 actually is.
//!
//! A gate that only prevents growth gives no answer to "are we getting
//! anywhere". This prints the number the epic is driving down, what it started
//! at, and which way it has moved — so progress is something anyone can look
//! at rather than something inferred from merged PR titles.

use anyhow::{Context, Result};
use loom_daemon::shell_budget;

#[derive(clap::Args)]
pub(crate) struct ShellBudgetArgs {
    /// Emit the measurement as JSON for scripting.
    #[arg(long)]
    pub json: bool,

    /// Repo root to measure. Defaults to the current directory.
    #[arg(long, value_name = "PATH")]
    pub root: Option<std::path::PathBuf>,

    /// Enforce the ratchet: exit 1 when THIS change grows the portable pool.
    ///
    /// Compares against the merge-base with `--base` (default `origin/main`),
    /// so it measures what your change did and needs no committed number to
    /// stay in sync. A baseline file goes stale every time `main` moves; this
    /// cannot.
    ///
    /// The report prints either way — a gate that only speaks up on failure
    /// teaches nobody which way the number is moving, which is how the portable
    /// pool grew +317 across four merged ports without anyone noticing.
    #[arg(long)]
    pub check: bool,

    /// The ref `--check` compares against. Its merge-base with `HEAD` is used,
    /// so an un-rebased branch is measured on its own contribution rather than
    /// being blamed for everything that landed while it was open.
    #[arg(long, value_name = "REF", default_value = "origin/main")]
    pub base: String,
}

impl ShellBudgetArgs {
    pub(crate) fn run(self) -> Result<()> {
        let root = match self.root {
            Some(r) => r,
            None => std::env::current_dir().context("could not resolve the current directory")?,
        };
        let budget = shell_budget::measure(&root).map_err(anyhow::Error::msg)?;

        // #8233: the benefit side. Fail-soft — a shallow clone has no
        // six-month window, and omitting a section beats refusing to report.
        let churn = std::fs::read_to_string(root.join("scripts/shell-allowlist.txt"))
            .ok()
            .map(|t| shell_budget::parse_allowlist(&t))
            .and_then(|cats| shell_budget::churn::measure(&root, &cats).ok())
            .unwrap_or_default();
        let origin = shell_budget::read_origin_portable(&root).unwrap_or(0);

        // Floor growth that has been declared and accepted since the epic began
        // (#8154 ask 2). Fail-soft: a shallow clone cannot reach the epic-start
        // rev, and a report that refuses to print because of that is worse than
        // one that omits a line.
        let accepted: Vec<shell_budget::GrowthDeclaration> = shell_budget::read_origin_rev(&root)
            .and_then(|rev| shell_budget::collect_growth_declarations(&root, &rev).ok())
            .map(|(ok, _)| ok)
            .unwrap_or_default();
        let accepted_total: u64 = accepted.iter().map(|d| d.lines).sum();

        if self.json {
            let by_cat: serde_json::Map<String, serde_json::Value> = budget
                .by_category
                .iter()
                .map(|(k, v)| (k.clone(), serde_json::json!(v)))
                .collect();
            println!(
                "{}",
                serde_json::json!({
                    "portable": budget.portable(),
                    "floor": budget.floor(),
                    "stubbed": budget.stubbed(),
                    // #8237. `settled` is descoped, not retired: shell this
                    // repo deliberately keeps. `comparable` is what
                    // `net_vs_epic_start` is measured on — portable + settled,
                    // so reclassifying a script cannot improve the figure.
                    "settled": budget.settled(),
                    "settled_files": budget.files_by_category.get(shell_budget::SETTLED).copied().unwrap_or(0),
                    "comparable": budget.comparable(),
                    "total": budget.total(),
                    "files": budget.file_count(),
                    "origin_portable": origin,
                    "net_vs_epic_start": i128::from(budget.comparable()) - i128::from(origin),
                    "churn_retired": churn.retired,
                    "churn_remaining": churn.remaining,
                    "churn_retired_pct": churn.retired_pct(),
                    "churn_delegating": churn.delegating,
                    "churn_window": churn.window,
                    "churn_worst": churn.worst.iter().take(shell_budget::churn::JSON_WORST).map(|(p, l, f)| serde_json::json!({
                        "path": p, "code_lines": l, "fixes": f,
                    })).collect::<Vec<_>>(),
                    "declared_floor_growth": accepted_total,
                    "declared_floor_growth_declarations": accepted.iter().map(|d| serde_json::json!({
                        "lines": d.lines,
                        "reason": d.reason,
                        "issue": d.issue,
                    })).collect::<Vec<_>>(),
                    "by_category": by_cat,
                    "unlisted": budget.unlisted.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
                })
            );
        } else {
            print!("{}", shell_budget::render_report(&budget, origin));
            print!("{}", shell_budget::churn::render(&churn));
            if !accepted.is_empty() {
                println!(
                    "\n  declared floor growth  {accepted_total:>6}   declared across {} commit(s) since the epic began",
                    accepted.len()
                );
                for d in &accepted {
                    println!("    {:>4} — {}", d.lines, d.reason);
                }
            }
        }

        if self.check {
            // The tree invariants need no comparison: an unlisted production
            // script makes every figure an undercount whichever revision you
            // measure, and a scope filter that stops seeing the tree makes the
            // gate pass for the wrong reason.
            //
            // Call the shared function rather than inlining part of it. The
            // first cut inlined only the `unlisted` half, which left the
            // scope-filter floor running solely in the path-filtered Rust test
            // job — exactly the gating mistake this job's own comment exists to
            // warn about.
            if let Err(why) = shell_budget::check_invariants(&budget) {
                eprintln!("\nshell-budget: {why}");
                std::process::exit(1);
            }

            let cmp = shell_budget::comparison(&root, &self.base).map_err(anyhow::Error::msg)?;
            let before =
                shell_budget::measure_at_rev(&root, &cmp.rev).map_err(anyhow::Error::msg)?;
            let desc = cmp.desc;

            // Declared floor growth (#8154). The gate's own message told
            // authors to "say why in the commit" while nothing read the
            // commit; these two lines are what make that true.
            let (declared, malformed) = shell_budget::collect_growth_declarations(&root, &cmp.rev)
                .map_err(anyhow::Error::msg)?;

            // A typo'd override degrades to "no override", and then the build
            // fails with a message about growth rather than about the typo —
            // so say it plainly before anything else is printed.
            for m in &malformed {
                eprintln!(
                    "\nshell-budget: ignoring a malformed {} trailer — {}\n  {}",
                    shell_budget::GROWTH_TRAILER,
                    m.why,
                    m.line
                );
            }

            let ctx = shell_budget::GrowthContext {
                declared: &declared,
            };

            if let Err(why) = shell_budget::check_against_rev(&budget, &before, &desc, &ctx) {
                // Not "PORTABLE SHELL GREW": three of the four refusal paths
                // (floor growth, a short declaration, a change that both grows
                // the floor and retires portable shell)
                // fire when portable FELL or held. A header that names the
                // wrong cause sends the author to fix the wrong thing — the
                // same message-vs-reality defect this whole change is about.
                eprintln!("\nshell-budget: REFUSED\n\n{why}");
                std::process::exit(1);
            }

            // Declared growth stays visible. #8154 ask 3: the point is that
            // growth remains a conscious act, so it must not vanish into a
            // silently-passing build.
            //
            // "declared", not "accepted": over-declaring is allowed, so this is
            // an upper bound on what the change actually spent, not a
            // measurement of it. Calling it "accepted" invited reading the
            // cumulative figure as real growth.
            if !declared.is_empty() {
                let total: u64 = declared
                    .iter()
                    .fold(0u64, |acc, d| acc.saturating_add(d.lines));
                let actual = budget.total().saturating_sub(before.total());
                if self.json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "declared_this_change": total,
                            "measured_growth_this_change": actual,
                            "declarations": declared.iter().map(|d| serde_json::json!({
                                "lines": d.lines, "reason": d.reason, "issue": d.issue,
                            })).collect::<Vec<_>>(),
                        })
                    );
                } else {
                    println!(
                        "\nshell-budget: {total} line(s) of floor growth DECLARED \
                         ({actual} measured) — an upper bound, not a measurement:"
                    );
                    // The reason carries its own `#<issue>` by construction —
                    // the parser rejects one that does not — so do not print
                    // it twice.
                    for d in &declared {
                        println!("  {} lines — {}", d.lines, d.reason);
                    }
                }
            }
            if !self.json {
                let delta = i128::from(budget.portable()) - i128::from(before.portable());
                println!(
                    "\nshell-budget: this change moves portable shell by {delta:+} vs {desc}."
                );
                // #8237 constraint 1, at the one place the gate speaks to the
                // author directly. A change that reclassifies scripts as
                // `settled` shrinks the PORTABLE pool without retiring a
                // single line, and a bare `-3533` reads as a win that did not
                // happen. Say which part was descoped and what the epic
                // figure — measured on portable + settled — actually did.
                let comparable = i128::from(budget.comparable()) - i128::from(before.comparable());
                if comparable != delta {
                    println!(
                        "  {} line(s) of that moved into `settled` rather than out of the repo. \
                         Descoping is not retiring, so the epic figure moved by {comparable:+}, \
                         not {delta:+}.",
                        comparable - delta
                    );
                }
            }
        }
        Ok(())
    }
}
