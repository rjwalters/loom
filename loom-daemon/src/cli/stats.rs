//! `loom-daemon stats` handler (Issue #4712 — split out of `main.rs`):
//! `handle_stats_command` (the original interactive dashboard) plus
//! `handle_agent_metrics_command` (the `agent-metrics.sh` parity CLI, epic
//! #4081 Phase 3 family 4).

use anyhow::{anyhow, Result};

use loom_daemon::activity::{self, ActivityDb, StatsQueries};

#[allow(clippy::too_many_lines)]
pub(crate) fn handle_stats_command(
    role: Option<&str>,
    issue: Option<i32>,
    weekly: bool,
    format: &str,
) -> Result<()> {
    let loom_dir = dirs::home_dir()
        .ok_or_else(|| anyhow!("No home directory"))?
        .join(".loom");

    let db_path = loom_dir.join("activity.db");

    if !db_path.exists() {
        eprintln!("No activity database found at {}", db_path.display());
        eprintln!("Run the Loom daemon first to start collecting metrics.");
        return Ok(());
    }

    let db = ActivityDb::new(db_path)?;

    let is_json = format == "json";

    if let Some(issue_num) = issue {
        let costs = db.get_cost_per_issue(Some(issue_num))?;

        if is_json {
            println!("{}", serde_json::to_string_pretty(&costs)?);
        } else {
            println!("\n=== Cost Breakdown for Issue #{issue_num} ===\n");
            if costs.is_empty() {
                println!("No data found for issue #{issue_num}");
            } else {
                for cost in &costs {
                    println!("Issue #{}:", cost.issue_number);
                    println!("  Prompts:      {}", cost.prompt_count);
                    println!("  Total Cost:   ${:.4}", cost.total_cost);
                    println!("  Total Tokens: {}", cost.total_tokens);
                    if let Some(started) = &cost.started {
                        println!("  Started:      {}", started.format("%Y-%m-%d %H:%M"));
                    }
                    if let Some(completed) = &cost.completed {
                        println!("  Completed:    {}", completed.format("%Y-%m-%d %H:%M"));
                    }
                    println!();
                }
            }
        }
        return Ok(());
    }

    if let Some(role_filter) = role {
        let effectiveness = db.get_agent_effectiveness(Some(role_filter))?;

        if is_json {
            println!("{}", serde_json::to_string_pretty(&effectiveness)?);
        } else {
            println!("\n=== Agent Effectiveness: {role_filter} ===\n");
            if effectiveness.is_empty() {
                println!("No data found for role '{role_filter}'");
            } else {
                for agent in &effectiveness {
                    print_agent_effectiveness(agent);
                }
            }
        }
        return Ok(());
    }

    if weekly {
        let velocity = db.get_weekly_velocity()?;

        if is_json {
            println!("{}", serde_json::to_string_pretty(&velocity)?);
        } else {
            println!("\n=== Weekly Velocity ===\n");
            if velocity.is_empty() {
                println!("No weekly data available.");
            } else {
                println!("{:<12} {:>10} {:>12}", "Week", "Prompts", "Cost (USD)");
                println!("{:-<36}", "");
                for week in &velocity {
                    println!("{:<12} {:>10} {:>12.4}", week.week, week.prompts, week.cost);
                }
            }
        }
        return Ok(());
    }

    let summary = db.get_stats_summary()?;
    let effectiveness = db.get_agent_effectiveness(None)?;

    if is_json {
        #[derive(serde::Serialize)]
        struct FullStats {
            summary: activity::StatsSummary,
            effectiveness: Vec<activity::AgentEffectiveness>,
        }
        let full = FullStats {
            summary,
            effectiveness,
        };
        println!("{}", serde_json::to_string_pretty(&full)?);
    } else {
        println!("\n=== Loom Activity Summary ===\n");
        println!("Total Prompts:   {}", summary.total_prompts);
        println!("Total Cost:      ${:.4}", summary.total_cost);
        println!("Total Tokens:    {}", summary.total_tokens);
        println!("Issues Worked:   {}", summary.issues_count);
        println!("PRs Created:     {}", summary.prs_count);
        println!("Avg Success:     {:.1}%", summary.avg_success_rate);

        if !effectiveness.is_empty() {
            println!("\n=== Agent Effectiveness by Role ===\n");
            println!(
                "{:<12} {:>10} {:>10} {:>12} {:>12} {:>12}",
                "Role", "Prompts", "Success", "Rate", "Avg Cost", "Avg Time"
            );
            println!("{:-<70}", "");
            for agent in &effectiveness {
                println!(
                    "{:<12} {:>10} {:>10} {:>11.1}% {:>11.4} {:>10.1}s",
                    agent.agent_role,
                    agent.total_prompts,
                    agent.successful_prompts,
                    agent.success_rate,
                    agent.avg_cost,
                    agent.avg_duration_sec
                );
            }
        }

        let top_issues = db.get_cost_per_issue(None)?;
        if !top_issues.is_empty() {
            println!("\n=== Top 5 Most Expensive Issues ===\n");
            println!("{:<8} {:>10} {:>12} {:>12}", "Issue", "Prompts", "Cost (USD)", "Tokens");
            println!("{:-<44}", "");
            for cost in top_issues.iter().take(5) {
                println!(
                    "#{:<7} {:>10} {:>12.4} {:>12}",
                    cost.issue_number, cost.prompt_count, cost.total_cost, cost.total_tokens
                );
            }
        }

        println!();
    }

    Ok(())
}

fn print_agent_effectiveness(agent: &activity::AgentEffectiveness) {
    println!("Role: {}", agent.agent_role);
    println!("  Total Prompts:      {}", agent.total_prompts);
    println!("  Successful Prompts: {}", agent.successful_prompts);
    println!("  Success Rate:       {:.1}%", agent.success_rate);
    println!("  Average Cost:       ${:.4}", agent.avg_cost);
    println!("  Average Duration:   {:.1}s", agent.avg_duration_sec);
    println!();
}

/// Valid `agent-metrics` subcommands (mirrors `loom_tools.agent_metrics`'s
/// argparse `choices`).
const AGENT_METRICS_COMMANDS: &[&str] = &["summary", "effectiveness", "costs", "velocity"];
/// Valid `--period` values.
const AGENT_METRICS_PERIODS: &[&str] = &["today", "week", "month", "all"];
/// Valid `--role` values (mirrors `loom_tools.agent_metrics._VALID_ROLES`).
const AGENT_METRICS_ROLES: &[&str] = &[
    "builder",
    "judge",
    "curator",
    "architect",
    "hermit",
    "doctor",
    "guide",
    "champion",
    "shepherd",
];

/// Handle `loom-daemon stats <command>` — the native port of
/// `loom_tools.agent_metrics` (epic #4081 Phase 3 family 4, issue #4274).
/// `command` is one of `summary`/`effectiveness`/`costs`/`velocity`, the CLI
/// contract `agent-metrics.sh` forwards to (and, transitively,
/// `mcp__loom__get_agent_metrics`). Exits nonzero on validation failure or a
/// missing activity database, matching the retired Python CLI's exit codes.
#[allow(clippy::too_many_lines)]
pub(crate) fn handle_agent_metrics_command(
    command: &str,
    role: Option<&str>,
    issue: Option<i32>,
    period: &str,
    by_model: bool,
    format: &str,
) -> Result<()> {
    if !AGENT_METRICS_COMMANDS.contains(&command) {
        eprintln!(
            "Invalid command: {command} (expected one of: {})",
            AGENT_METRICS_COMMANDS.join(", ")
        );
        std::process::exit(1);
    }

    if let Some(r) = role {
        if !AGENT_METRICS_ROLES.contains(&r) {
            eprintln!("Invalid role: {r}");
            std::process::exit(1);
        }
    }

    if !AGENT_METRICS_PERIODS.contains(&period) {
        eprintln!(
            "Invalid period: {period} (expected one of: {})",
            AGENT_METRICS_PERIODS.join(", ")
        );
        std::process::exit(1);
    }

    let is_json = format == "json";

    let db_path = std::env::var_os("LOOM_ACTIVITY_DB")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("."))
                .join(".loom")
                .join("activity.db")
        });

    if !db_path.is_file() {
        if is_json {
            println!(
                "{}",
                serde_json::json!({
                    "error": "Activity database not available",
                    "db_path": db_path.display().to_string(),
                })
            );
        } else {
            println!(
                "Error: activity database not found at {}. Set LOOM_ACTIVITY_DB or enable agent activity tracking.",
                db_path.display()
            );
        }
        std::process::exit(1);
    }

    let db = ActivityDb::new(db_path)?;

    match command {
        "summary" => {
            let metrics = db.get_summary_metrics(role, period)?;
            if is_json {
                println!("{}", serde_json::to_string_pretty(&metrics)?);
            } else {
                let tokens_k = metrics.total_tokens / 1000;
                println!("\nAgent Performance Summary ({period})");
                println!("{:-<40}", "");
                println!("  Total Prompts:   {}", metrics.total_prompts);
                println!("  Total Tokens:    {tokens_k}K");
                println!("  Total Cost:      ${:.4}", metrics.total_cost);
                println!("  Issues Worked:   {}", metrics.issues_count);
                println!("  PRs Created:     {}", metrics.prs_count);
                println!("  Success Rate:    {:.1}%", metrics.success_rate);
                println!();
            }
        }
        "effectiveness" => {
            let rows = db.get_effectiveness_rows(role, period, by_model)?;
            if is_json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                println!("\nAgent Effectiveness by Role ({period})");
                if by_model {
                    println!(
                        "{:<12} {:<22} {:>10} {:>10} {:>10} {:>10} {:>10}",
                        "Role", "Model", "Prompts", "Success", "Rate", "Avg Cost", "Avg Time"
                    );
                } else {
                    println!(
                        "{:<12} {:>10} {:>10} {:>10} {:>10} {:>10}",
                        "Role", "Prompts", "Success", "Rate", "Avg Cost", "Avg Time"
                    );
                }
                for r in &rows {
                    let rate_str = format!("{:.1}%", r.success_rate);
                    let cost_str = format!("${:.4}", r.avg_cost);
                    let time_str = format!("{:.1}s", r.avg_duration_sec);
                    if by_model {
                        println!(
                            "{:<12} {:<22} {:>10} {:>10} {:>10} {:>10} {:>10}",
                            r.role,
                            r.model.as_deref().unwrap_or("default"),
                            r.total_prompts,
                            r.successful_prompts,
                            rate_str,
                            cost_str,
                            time_str
                        );
                    } else {
                        println!(
                            "{:<12} {:>10} {:>10} {:>10} {:>10} {:>10}",
                            r.role,
                            r.total_prompts,
                            r.successful_prompts,
                            rate_str,
                            cost_str,
                            time_str
                        );
                    }
                }
                if rows.is_empty() {
                    println!("No data found");
                }
                println!();
            }
        }
        "costs" => {
            let rows = db.get_cost_rows(issue, by_model)?;
            if is_json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                println!("\nCost Breakdown by Issue");
                if by_model {
                    println!(
                        "{:<8} {:<22} {:>10} {:>12} {:>12}",
                        "Issue", "Model", "Prompts", "Cost", "Tokens"
                    );
                } else {
                    println!("{:<8} {:>10} {:>12} {:>12}", "Issue", "Prompts", "Cost", "Tokens");
                }
                for r in &rows {
                    let cost_str = format!("${:.4}", r.total_cost);
                    if by_model {
                        println!(
                            "#{:<7} {:<22} {:>10} {:>12} {:>12}",
                            r.issue_number,
                            r.model.as_deref().unwrap_or("default"),
                            r.prompt_count,
                            cost_str,
                            r.total_tokens
                        );
                    } else {
                        println!(
                            "#{:<7} {:>10} {:>12} {:>12}",
                            r.issue_number, r.prompt_count, cost_str, r.total_tokens
                        );
                    }
                }
                if rows.is_empty() {
                    println!("No data found");
                }
                println!();
            }
        }
        "velocity" => {
            let rows = db.get_velocity_rows()?;
            if is_json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                println!("\nDevelopment Velocity (Last 8 Weeks)");
                println!(
                    "{:<10} {:>10} {:>10} {:>10} {:>10}",
                    "Week", "Prompts", "Issues", "PRs", "Cost"
                );
                for r in &rows {
                    println!(
                        "{:<10} {:>10} {:>10} {:>10} {:>10}",
                        r.week,
                        r.prompts,
                        r.issues,
                        r.prs_merged,
                        format!("${:.2}", r.cost)
                    );
                }
                println!();
            }
        }
        _ => unreachable!("validated above"),
    }

    Ok(())
}
