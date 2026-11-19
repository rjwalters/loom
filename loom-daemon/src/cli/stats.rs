//! `loom-daemon stats` handler (Issue #4712 — split out of `main.rs`):
//! `handle_stats_command` (the original interactive dashboard) plus
//! `handle_agent_metrics_command` (the `agent-metrics.sh` parity CLI, epic
//! #4081 Phase 3 family 4).

use std::fmt::Write as _;

use anyhow::{anyhow, Result};

use crate::activity::{self, ActivityDb, StatsQueries};

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
    print!("{}", render_stats(&db, role, issue, weekly, format == "json")?);
    Ok(())
}

/// Render the `loom-daemon stats` dashboard from the `*_rows` query stack
/// (the same one `agent-metrics` uses; #13637, slice 1 of #12752).
#[allow(clippy::too_many_lines)]
fn render_stats(
    db: &ActivityDb,
    role: Option<&str>,
    issue: Option<i32>,
    weekly: bool,
    is_json: bool,
) -> Result<String> {
    let mut out = String::new();

    if let Some(issue_num) = issue {
        let costs = db.get_cost_rows(Some(issue_num), false)?;

        if is_json {
            let v: Vec<_> = costs
                .iter()
                .map(|c| {
                    serde_json::json!({
                        "issue_number": c.issue_number,
                        "prompt_count": c.prompt_count,
                        "total_cost": c.total_cost,
                        "total_tokens": c.total_tokens,
                        "started": parse_ts(c.started.as_deref()),
                        "completed": parse_ts(c.completed.as_deref()),
                    })
                })
                .collect();
            writeln!(out, "{}", serde_json::to_string_pretty(&v)?)?;
        } else {
            writeln!(out, "\n=== Cost Breakdown for Issue #{issue_num} ===\n")?;
            if costs.is_empty() {
                writeln!(out, "No data found for issue #{issue_num}")?;
            } else {
                for cost in &costs {
                    writeln!(out, "Issue #{}:", cost.issue_number)?;
                    writeln!(out, "  Prompts:      {}", cost.prompt_count)?;
                    writeln!(out, "  Total Cost:   ${:.4}", cost.total_cost)?;
                    writeln!(out, "  Total Tokens: {}", cost.total_tokens)?;
                    if let Some(started) = parse_ts(cost.started.as_deref()) {
                        writeln!(out, "  Started:      {}", started.format("%Y-%m-%d %H:%M"))?;
                    }
                    if let Some(completed) = parse_ts(cost.completed.as_deref()) {
                        writeln!(out, "  Completed:    {}", completed.format("%Y-%m-%d %H:%M"))?;
                    }
                    writeln!(out)?;
                }
            }
        }
        return Ok(out);
    }

    if let Some(role_filter) = role {
        let effectiveness = db.get_effectiveness_rows(Some(role_filter), "all", false)?;

        if is_json {
            let v: Vec<_> = effectiveness.iter().map(effectiveness_json).collect();
            writeln!(out, "{}", serde_json::to_string_pretty(&v)?)?;
        } else {
            writeln!(out, "\n=== Agent Effectiveness: {role_filter} ===\n")?;
            if effectiveness.is_empty() {
                writeln!(out, "No data found for role '{role_filter}'")?;
            } else {
                for agent in &effectiveness {
                    write_agent_effectiveness(&mut out, agent)?;
                }
            }
        }
        return Ok(out);
    }

    if weekly {
        // `get_velocity_rows` returns the last 8 weeks newest-first; the
        // dashboard has always listed weeks oldest-first.
        let mut velocity = db.get_velocity_rows()?;
        velocity.reverse();

        if is_json {
            let v: Vec<_> = velocity
                .iter()
                .map(|w| serde_json::json!({"week": w.week, "prompts": w.prompts, "cost": w.cost}))
                .collect();
            writeln!(out, "{}", serde_json::to_string_pretty(&v)?)?;
        } else {
            writeln!(out, "\n=== Weekly Velocity ===\n")?;
            if velocity.is_empty() {
                writeln!(out, "No weekly data available.")?;
            } else {
                writeln!(out, "{:<12} {:>10} {:>12}", "Week", "Prompts", "Cost (USD)")?;
                writeln!(out, "{:-<36}", "")?;
                for week in &velocity {
                    writeln!(out, "{:<12} {:>10} {:>12.4}", week.week, week.prompts, week.cost)?;
                }
            }
        }
        return Ok(out);
    }

    let summary = db.get_summary_metrics(None, "all")?;
    let effectiveness = db.get_effectiveness_rows(None, "all", false)?;

    if is_json {
        let full = serde_json::json!({
            "summary": {
                "total_prompts": summary.total_prompts,
                "total_cost": summary.total_cost,
                "total_tokens": summary.total_tokens,
                "issues_count": summary.issues_count,
                "prs_count": summary.prs_count,
                "avg_success_rate": summary.success_rate,
                "cost_data_available": summary.cost_data_available,
                "github_data_available": summary.github_data_available,
                "quality_data_available": summary.quality_data_available,
            },
            "effectiveness": effectiveness.iter().map(effectiveness_json).collect::<Vec<_>>(),
        });
        writeln!(out, "{}", serde_json::to_string_pretty(&full)?)?;
    } else {
        writeln!(out, "\n=== Loom Activity Summary ===\n")?;
        writeln!(out, "Total Prompts:   {}", summary.total_prompts)?;
        // Metrics genuinely unavailable (the backing table has never
        // recorded a row) render as `n/a`, never `0`/`0.0%` — that's
        // indistinguishable from a real failure (#6128).
        if summary.cost_data_available {
            writeln!(out, "Total Cost:      ${:.4}", summary.total_cost)?;
            writeln!(out, "Total Tokens:    {}", summary.total_tokens)?;
        } else {
            writeln!(out, "Total Cost:      n/a (no resource-usage telemetry recorded)")?;
            writeln!(out, "Total Tokens:    n/a (no resource-usage telemetry recorded)")?;
        }
        if summary.github_data_available {
            writeln!(out, "Issues Worked:   {}", summary.issues_count)?;
            writeln!(out, "PRs Created:     {}", summary.prs_count)?;
        } else {
            writeln!(out, "Issues Worked:   n/a (no GitHub-linked prompts recorded)")?;
            writeln!(out, "PRs Created:     n/a (no GitHub-linked prompts recorded)")?;
        }
        if summary.quality_data_available {
            writeln!(out, "Avg Success:     {:.1}%", summary.success_rate)?;
        } else {
            writeln!(out, "Avg Success:     n/a (no quality-metrics recorded)")?;
        }

        if !effectiveness.is_empty() {
            writeln!(out, "\n=== Agent Effectiveness by Role ===\n")?;
            // Column width is derived from the longest role name actually
            // present (plus the "Role" header) instead of a fixed 12 chars (#6128).
            let role_width = effectiveness
                .iter()
                .map(|a| a.role.len())
                .max()
                .unwrap_or(4)
                .max("Role".len());
            writeln!(
                out,
                "{:<role_width$} {:>10} {:>10} {:>12} {:>12} {:>12}",
                "Role",
                "Prompts",
                "Success",
                "Rate",
                "Avg Cost",
                "Avg Time",
                role_width = role_width
            )?;
            writeln!(out, "{:-<width$}", "", width = role_width + 61)?;
            for agent in &effectiveness {
                let rate_str = if agent.quality_data_available {
                    format!("{:.1}%", agent.success_rate)
                } else {
                    "n/a".to_string()
                };
                let cost_str = if agent.cost_data_available {
                    format!("{:.4}", agent.avg_cost)
                } else {
                    "n/a".to_string()
                };
                let time_str = if agent.cost_data_available {
                    format!("{:.1}s", agent.avg_duration_sec)
                } else {
                    "n/a".to_string()
                };
                writeln!(
                    out,
                    "{:<role_width$} {:>10} {:>10} {:>12} {:>12} {:>12}",
                    agent.role,
                    agent.total_prompts,
                    agent.successful_prompts,
                    rate_str,
                    cost_str,
                    time_str,
                    role_width = role_width
                )?;
            }
        }

        let top_issues = db.get_cost_rows(None, false)?;
        if !top_issues.is_empty() {
            writeln!(out, "\n=== Top 5 Most Expensive Issues ===\n")?;
            writeln!(
                out,
                "{:<8} {:>10} {:>12} {:>12}",
                "Issue", "Prompts", "Cost (USD)", "Tokens"
            )?;
            writeln!(out, "{:-<44}", "")?;
            for cost in top_issues.iter().take(5) {
                writeln!(
                    out,
                    "#{:<7} {:>10} {:>12.4} {:>12}",
                    cost.issue_number, cost.prompt_count, cost.total_cost, cost.total_tokens
                )?;
            }
        }

        writeln!(out)?;
    }

    Ok(out)
}

/// Parse a raw DB timestamp the way the legacy `cost_per_issue` mapper did
/// (RFC 3339 only; anything else renders as absent).
fn parse_ts(raw: Option<&str>) -> Option<chrono::DateTime<chrono::Utc>> {
    raw.and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc))
}

/// Dashboard JSON for one effectiveness row — field-compatible with the
/// legacy `AgentEffectiveness` shape (`agent_role` key included).
fn effectiveness_json(a: &activity::EffectivenessRow) -> serde_json::Value {
    serde_json::json!({
        "agent_role": a.role,
        "total_prompts": a.total_prompts,
        "successful_prompts": a.successful_prompts,
        "success_rate": a.success_rate,
        "avg_cost": a.avg_cost,
        "avg_duration_sec": a.avg_duration_sec,
        "cost_data_available": a.cost_data_available,
        "quality_data_available": a.quality_data_available,
    })
}

fn write_agent_effectiveness(out: &mut String, agent: &activity::EffectivenessRow) -> Result<()> {
    writeln!(out, "Role: {}", agent.role)?;
    writeln!(out, "  Total Prompts:      {}", agent.total_prompts)?;
    writeln!(out, "  Successful Prompts: {}", agent.successful_prompts)?;
    if agent.quality_data_available {
        writeln!(out, "  Success Rate:       {:.1}%", agent.success_rate)?;
    } else {
        writeln!(out, "  Success Rate:       n/a (no quality-metrics recorded)")?;
    }
    if agent.cost_data_available {
        writeln!(out, "  Average Cost:       ${:.4}", agent.avg_cost)?;
        writeln!(out, "  Average Duration:   {:.1}s", agent.avg_duration_sec)?;
    } else {
        writeln!(out, "  Average Cost:       n/a (no resource-usage telemetry recorded)")?;
        writeln!(out, "  Average Duration:   n/a (no resource-usage telemetry recorded)")?;
    }
    writeln!(out)?;
    Ok(())
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, ActivityDb, rusqlite::Connection) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("activity.db");
        let db = ActivityDb::new(path.clone()).unwrap();
        let conn = rusqlite::Connection::open(path).unwrap();
        (dir, db, conn)
    }

    fn add_prompt(conn: &rusqlite::Connection, role: &str, issue: Option<i64>) -> i64 {
        conn.execute(
            "INSERT INTO agent_inputs (terminal_id, timestamp, input_type, content, agent_role, context)
             VALUES ('t1', datetime('now'), 'manual', 'x', ?1, '{}')",
            [role],
        )
        .unwrap();
        let id = conn.last_insert_rowid();
        if let Some(n) = issue {
            conn.execute(
                "INSERT INTO prompt_github (input_id, issue_number, event_type) VALUES (?1, ?2, 'label_added')",
                rusqlite::params![id, n],
            )
            .unwrap();
        }
        id
    }

    fn add_telemetry(conn: &rusqlite::Connection, id: i64, cost: f64, passed: i64) {
        conn.execute(
            "INSERT INTO resource_usage (input_id, timestamp, model, tokens_input, tokens_output, cost_usd, duration_ms, provider)
             VALUES (?1, datetime('now'), 'm', 1000, 500, ?2, 2000, 'anthropic')",
            rusqlite::params![id, cost],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO quality_metrics (input_id, timestamp, tests_passed, tests_failed)
             VALUES (?1, datetime('now'), ?2, 0)",
            rusqlite::params![id, passed],
        )
        .unwrap();
    }

    #[test]
    fn summary_without_telemetry_prints_na() {
        let (_d, db, conn) = fixture();
        add_prompt(&conn, "builder", None);
        let out = render_stats(&db, None, None, false, false).unwrap();
        assert!(out.contains("Total Prompts:   1\n"));
        assert!(out.contains("Total Cost:      n/a (no resource-usage telemetry recorded)\n"));
        assert!(out.contains("Total Tokens:    n/a (no resource-usage telemetry recorded)\n"));
        assert!(out.contains("Issues Worked:   n/a (no GitHub-linked prompts recorded)\n"));
        assert!(out.contains("PRs Created:     n/a (no GitHub-linked prompts recorded)\n"));
        assert!(out.contains("Avg Success:     n/a (no quality-metrics recorded)\n"));
        let row = out.lines().find(|l| l.starts_with("builder")).unwrap();
        assert_eq!(row.matches("n/a").count(), 3, "{row}");
    }

    #[test]
    fn summary_with_data_matches_layout() {
        let (_d, db, conn) = fixture();
        let id = add_prompt(&conn, "builder", Some(42));
        add_telemetry(&conn, id, 0.25, 3);
        let out = render_stats(&db, None, None, false, false).unwrap();
        let expected = format!(
            "\n=== Loom Activity Summary ===\n\n\
Total Prompts:   1\n\
Total Cost:      $0.2500\n\
Total Tokens:    1500\n\
Issues Worked:   1\n\
PRs Created:     0\n\
Avg Success:     100.0%\n\
\n=== Agent Effectiveness by Role ===\n\n\
{:<7} {:>10} {:>10} {:>12} {:>12} {:>12}\n{}\n\
{:<7} {:>10} {:>10} {:>12} {:>12} {:>12}\n\
\n=== Top 5 Most Expensive Issues ===\n\n\
{:<8} {:>10} {:>12} {:>12}\n{}\n\
#{:<7} {:>10} {:>12} {:>12}\n\n",
            "Role",
            "Prompts",
            "Success",
            "Rate",
            "Avg Cost",
            "Avg Time",
            "-".repeat(68),
            "builder",
            1,
            1,
            "100.0%",
            "0.2500",
            "2.0s",
            "Issue",
            "Prompts",
            "Cost (USD)",
            "Tokens",
            "-".repeat(44),
            42,
            1,
            "0.2500",
            1500,
        );
        assert_eq!(out, expected);
    }

    #[test]
    fn issue_role_weekly_paths() {
        let (_d, db, conn) = fixture();
        let id = add_prompt(&conn, "builder", Some(42));
        add_telemetry(&conn, id, 0.25, 3);

        let issue = render_stats(&db, None, Some(42), false, false).unwrap();
        assert!(issue.starts_with("\n=== Cost Breakdown for Issue #42 ===\n\nIssue #42:\n"));
        assert!(
            issue.contains("  Prompts:      1\n  Total Cost:   $0.2500\n  Total Tokens: 1500\n")
        );
        let none = render_stats(&db, None, Some(7), false, false).unwrap();
        assert!(none.contains("No data found for issue #7\n"));

        let role = render_stats(&db, Some("builder"), None, false, false).unwrap();
        assert!(role.contains("Role: builder\n  Total Prompts:      1\n  Successful Prompts: 1\n"));
        assert!(role.contains("  Success Rate:       100.0%\n  Average Cost:       $0.2500\n  Average Duration:   2.0s\n"));
        let norole = render_stats(&db, Some("judge"), None, false, false).unwrap();
        assert!(norole.contains("No data found for role 'judge'\n"));

        let weekly = render_stats(&db, None, None, true, false).unwrap();
        assert!(weekly.starts_with("\n=== Weekly Velocity ===\n\nWeek"));
        assert!(weekly.contains("0.2500"));
    }

    #[test]
    fn role_path_without_telemetry_prints_na() {
        let (_d, db, conn) = fixture();
        add_prompt(&conn, "builder", None);
        let out = render_stats(&db, Some("builder"), None, false, false).unwrap();
        assert!(out.contains("  Success Rate:       n/a (no quality-metrics recorded)\n"));
        assert!(out.contains("  Average Cost:       n/a (no resource-usage telemetry recorded)\n"));
    }

    #[test]
    fn empty_db_and_json_shapes() {
        let (_d, db, conn) = fixture();
        let out = render_stats(&db, None, None, false, false).unwrap();
        assert!(out.contains("Total Prompts:   0\n"));
        assert!(!out.contains("Agent Effectiveness by Role"));
        assert!(render_stats(&db, None, None, true, false)
            .unwrap()
            .contains("No weekly data available."));

        let id = add_prompt(&conn, "builder", Some(1));
        add_telemetry(&conn, id, 0.1, 1);
        let j: serde_json::Value =
            serde_json::from_str(&render_stats(&db, None, None, false, true).unwrap()).unwrap();
        assert_eq!(j["summary"]["total_prompts"], 1);
        assert_eq!(j["summary"]["cost_data_available"], true);
        assert_eq!(j["effectiveness"][0]["agent_role"], "builder");
        let c: serde_json::Value =
            serde_json::from_str(&render_stats(&db, None, Some(1), false, true).unwrap()).unwrap();
        assert_eq!(c[0]["issue_number"], 1);
        assert!(c[0].get("started").is_some());
    }
}
