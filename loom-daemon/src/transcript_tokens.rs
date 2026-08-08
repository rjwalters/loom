//! On-disk transcript token accounting for the safehouse completion feed (#4699).
//!
//! # Why this module exists
//!
//! The `completion-v1` envelope's `tokens` field (#4497) was originally sourced
//! **only** from the activity DB's per-issue cost rollup
//! ([`crate::activity::db::ActivityDb::get_cost_by_issue`]), whose SQL joins
//! `resource_usage -> agent_inputs -> prompt_github`. Every row in those two
//! analytics tables is written from exactly one place — the IPC
//! `GetTerminalOutput` handler, which scrapes a *managed terminal's* scrollback
//! (`ipc.rs`, the MOM/terminal-manager path).
//!
//! Daemon-dispatched sweeps never traverse that path. `dispatch_sweep` spawns a
//! detached `claude -p` through `spawn-worker.sh`/`spawn-claude.sh` and reaps the
//! OS process; it issues no `SendInput`/`GetTerminalOutput` IPC round trips at
//! all. So on any host whose work arrives via dispatch — i.e. every host that
//! publishes completions to the fleet feed — the rollup is not "empty until the
//! prompt↔usage linkage is established", it is **structurally empty forever**,
//! and `tokens` was unconditionally `null`. Measured on the reference fleet host
//! 2026-07-31: `resource_usage` 0 rows, `prompt_github` 0 rows, DB file untouched
//! for two days across 76 published completions.
//!
//! The token data itself is on disk the whole time, in the sweep's own Claude
//! Code transcripts. This module locates them and sums them, so `tokens` has a
//! source that the dispatch path actually populates.
//!
//! # How a sweep's transcripts are located
//!
//! A sweep runs `claude -p "/loom:sweep <issue> …"` with cwd = the workspace
//! root, so Claude Code writes its session under
//! `${CLAUDE_CONFIG_DIR:-$HOME/.claude}/projects/<cwd-slug>/`:
//!
//! ```text
//! <projects>/-Users-me-GitHub-loom/
//!   <session-uuid>.jsonl            # parent session (the /loom:sweep turn)
//!   <session-uuid>/subagents/
//!     agent-<id>.jsonl              # one per phase (builder, judge, …)
//! ```
//!
//! There is no sweep-id → session-uuid mapping recorded anywhere, so the join is
//! done by content: the parent session's first `user` line carries the slash
//! command verbatim (`<command-name>/loom:sweep</command-name>` plus
//! `<command-args>4705 --claim-owned 4705</command-args>`). Candidates are first
//! narrowed by file mtime against the completion's own time window, so a
//! long-lived project directory is not head-read in full on every completion.
//!
//! # Fidelity
//!
//! The returned figure is the sum of **all four** usage counters
//! (`input_tokens`, `output_tokens`, `cache_read_input_tokens`,
//! `cache_creation_input_tokens`) across the parent session and every subagent
//! transcript — total tokens processed. Cache reads dominate a sweep by volume
//! (~99% on measured runs) and, priced at ~10% of base, still dominate it by
//! cost, so an input+output-only figure would under-report actual spend by well
//! over an order of magnitude and do so unevenly between sweeps. This differs
//! from the activity-DB rollup's narrower `input + output`; the difference is
//! documented in `.loom/docs/safehouse.md` and the DB path is left untouched.
//!
//! Re-dispatched sweeps produce several sessions for one issue; all matching
//! sessions are summed, since the feed reports what the issue cost in total.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};

use crate::script_helpers::sweep_experiment::{
    sum_transcript_usage, sum_transcript_usage_by_model, ModelUsageTotals,
};

/// Bytes of each candidate session file read when testing it for the
/// `/loom:sweep <issue>` slash command. The command is in the first `user`
/// record, a few hundred bytes in; 64 KiB is generous headroom for the
/// `queue-operation` preamble without reading multi-megabyte transcripts.
pub const HEAD_SCAN_BYTES: usize = 64 * 1024;

/// Slack applied to both ends of the completion's time window when filtering
/// candidate sessions by mtime. Covers clock skew, a session flushed after the
/// reaper observed the exit, and `reconcile_recent_merges`' forge-derived
/// (rather than sweep-derived) start time.
pub const WINDOW_SLACK: Duration = Duration::from_secs(2 * 60 * 60);

/// Per-file size ceiling. A transcript above this is skipped rather than read
/// into memory: the summation helper reads whole files, and one pathological
/// transcript must not be able to balloon the daemon's RSS on a code path whose
/// entire output is an optional display field.
pub const MAX_TRANSCRIPT_BYTES: u64 = 256 * 1024 * 1024;

/// Claude Code's project-directory slug: every character outside `[A-Za-z0-9-]`
/// becomes `-`. `/Users/me/GitHub/lean-genius/.loom/worktrees/x` therefore maps
/// to `-Users-me-GitHub-lean-genius--loom-worktrees-x` (the `/` and the `.`
/// each contribute one `-`, hyphens already in a path component survive).
#[must_use]
pub fn project_slug(workspace_root: &Path) -> String {
    workspace_root
        .to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// `${CLAUDE_CONFIG_DIR:-$HOME/.claude}/projects`, or `None` when neither the
/// override nor a home directory resolves.
#[must_use]
pub fn claude_projects_dir() -> Option<PathBuf> {
    let base = std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".claude")))?;
    Some(base.join("projects"))
}

/// Whether `head` — the leading bytes of a Claude Code session transcript —
/// shows that session being launched as `/loom:sweep <issue> …`.
///
/// The slash command is recorded as JSON-escaped markup inside the first `user`
/// record, so this matches on the two literal tags rather than parsing the line.
/// The issue number must be the **first** whitespace-delimited argument, so
/// `/loom:sweep 470 --claim-owned 470` is not mistaken for issue `4705`, and an
/// unrelated later mention of the number cannot match.
#[must_use]
pub fn head_names_sweep_issue(head: &str, issue: u32) -> bool {
    const NAME_TAG: &str = "<command-name>/loom:sweep</command-name>";
    const ARGS_OPEN: &str = "<command-args>";

    let Some(after_name) = head.find(NAME_TAG).map(|i| i + NAME_TAG.len()) else {
        return false;
    };
    let Some(args_at) = head[after_name..].find(ARGS_OPEN).map(|i| after_name + i) else {
        return false;
    };
    let args = &head[args_at + ARGS_OPEN.len()..];
    // Stop at the closing tag if it is within the head slice; a truncated head
    // still yields the first token, which is all that is inspected.
    let args = args.split("</command-args>").next().unwrap_or(args);
    args.split_whitespace()
        .next()
        .and_then(|tok| tok.parse::<u32>().ok())
        == Some(issue)
}

/// Read at most [`HEAD_SCAN_BYTES`] from `path`, lossily decoded.
fn read_head(path: &Path) -> Option<String> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path).ok()?;
    let mut buf = vec![0_u8; HEAD_SCAN_BYTES];
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(_) => return None,
        }
    }
    buf.truncate(filled);
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Whether `mtime` falls inside `[start - slack, end + slack]`. A file whose
/// mtime cannot be read is kept (fail-open: a missed match costs a token total,
/// a spurious one costs only a head read).
fn mtime_in_window(path: &Path, window: Option<(DateTime<Utc>, DateTime<Utc>)>) -> bool {
    let Some((start, end)) = window else {
        return true;
    };
    let Ok(mtime) = std::fs::metadata(path).and_then(|m| m.modified()) else {
        return true;
    };
    // `checked_*` rather than the panicking operators: an absurd timestamp
    // (a zeroed/garbage `createdAt` reaching the reconciliation path) must
    // widen the window, never abort the completion.
    let lo = SystemTime::from(start).checked_sub(WINDOW_SLACK);
    let hi = SystemTime::from(end).checked_add(WINDOW_SLACK);
    lo.is_none_or(|lo| mtime >= lo) && hi.is_none_or(|hi| mtime <= hi)
}

/// Every transcript belonging to `session`: the parent `<uuid>.jsonl` plus each
/// `<uuid>/subagents/*.jsonl`. Subagent records are not duplicated into the
/// parent file (the parent carries no `isSidechain` records), so summing both
/// does not double-count.
fn session_transcripts(session_jsonl: &Path) -> Vec<PathBuf> {
    let mut out = vec![session_jsonl.to_path_buf()];
    let subagents = session_jsonl.with_extension("").join("subagents");
    if let Ok(entries) = std::fs::read_dir(&subagents) {
        let mut nested: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
            .collect();
        nested.sort();
        out.extend(nested);
    }
    out
}

/// Total tokens processed by every `/loom:sweep <issue>` session under
/// `projects_dir` for `workspace_root`, or `None` when nothing attributable was
/// found.
///
/// Blocking file I/O — call from a blocking context. `None` (never `Some(0)`)
/// is returned for "no attributable transcripts", matching the envelope
/// contract that an absent total omits the key rather than publishing a
/// misleading zero.
#[must_use]
pub fn sum_sweep_tokens(
    projects_dir: &Path,
    workspace_root: &Path,
    issue: u32,
    window: Option<(DateTime<Utc>, DateTime<Utc>)>,
) -> Option<u64> {
    let project = projects_dir.join(project_slug(workspace_root));
    let entries = std::fs::read_dir(&project).ok()?;

    let mut total: u64 = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "jsonl") {
            continue;
        }
        if !mtime_in_window(&path, window) {
            continue;
        }
        let Some(head) = read_head(&path) else {
            continue;
        };
        if !head_names_sweep_issue(&head, issue) {
            continue;
        }
        for transcript in session_transcripts(&path) {
            let too_big =
                std::fs::metadata(&transcript).is_ok_and(|m| m.len() > MAX_TRANSCRIPT_BYTES);
            if too_big {
                log::warn!(
                    "safehouse: skipping oversized transcript {} for issue #{issue} token total",
                    transcript.display()
                );
                continue;
            }
            let usage = sum_transcript_usage(&transcript);
            let sum = usage
                .input_tokens
                .saturating_add(usage.output_tokens)
                .saturating_add(usage.cache_read_input_tokens)
                .saturating_add(usage.cache_creation_input_tokens);
            total = total.saturating_add(u64::try_from(sum).unwrap_or(0));
        }
    }
    (total > 0).then_some(total)
}

/// Split input/output token totals for `issue`'s `/loom:sweep` sessions
/// (Issue #5357): same session-matching and per-transcript summation as
/// [`sum_sweep_tokens`], but keeping the input/output axes separate rather
/// than collapsing them into one total — the two price very differently, so
/// a `sweep.outcome` consumer that wants a cost-weighted figure needs both
/// counts alongside the record's own `model`, not a single pre-mixed number.
///
/// "Input" here is the three billing-input counters summed
/// (`input_tokens` + `cache_read_input_tokens` + `cache_creation_input_tokens`
/// — see the module doc's "Fidelity" section for why cache tokens count as
/// input rather than being dropped); "output" is `output_tokens` alone.
/// Deliberately **raw**, not cost-weighted: `sweep.outcome` already carries
/// `model`, so a consumer applies whatever per-model pricing table it wants
/// without this record needing a backfill when that table changes.
///
/// `None` (never `Some((0, 0))`) when nothing attributable was found — same
/// "unknown != zero" contract as [`sum_sweep_tokens`].
#[must_use]
pub fn sum_sweep_tokens_split(
    projects_dir: &Path,
    workspace_root: &Path,
    issue: u32,
    window: Option<(DateTime<Utc>, DateTime<Utc>)>,
) -> Option<(u64, u64)> {
    let project = projects_dir.join(project_slug(workspace_root));
    let entries = std::fs::read_dir(&project).ok()?;

    let mut input_total: u64 = 0;
    let mut output_total: u64 = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "jsonl") {
            continue;
        }
        if !mtime_in_window(&path, window) {
            continue;
        }
        let Some(head) = read_head(&path) else {
            continue;
        };
        if !head_names_sweep_issue(&head, issue) {
            continue;
        }
        for transcript in session_transcripts(&path) {
            let too_big =
                std::fs::metadata(&transcript).is_ok_and(|m| m.len() > MAX_TRANSCRIPT_BYTES);
            if too_big {
                log::warn!(
                    "sweep.outcome: skipping oversized transcript {} for issue #{issue} token split",
                    transcript.display()
                );
                continue;
            }
            let usage = sum_transcript_usage(&transcript);
            let input = usage
                .input_tokens
                .saturating_add(usage.cache_read_input_tokens)
                .saturating_add(usage.cache_creation_input_tokens);
            input_total = input_total.saturating_add(u64::try_from(input).unwrap_or(0));
            output_total =
                output_total.saturating_add(u64::try_from(usage.output_tokens).unwrap_or(0));
        }
    }
    (input_total > 0 || output_total > 0).then_some((input_total, output_total))
}

/// Per-`(model, speed, service_tier)` token totals for `issue`'s
/// `/loom:sweep` sessions (#5740): same session-matching and per-transcript
/// scan as [`sum_sweep_tokens`]/[`sum_sweep_tokens_split`], but grouped by
/// model/speed/tier instead of collapsed into one running total. See
/// [`ModelUsageTotals`] and [`sum_transcript_usage_by_model`] for why a
/// single flat sum cannot be priced.
///
/// Totals from every matching transcript are merged by tuple across the
/// whole sweep (a re-dispatched issue's several sessions, and a sweep whose
/// phases used different models via #5687's downgrade fallback, all
/// contribute to the same output row when the tuple matches).
///
/// `None` (never `Some(vec![])`) when nothing attributable was found — same
/// "unknown != zero" contract as [`sum_sweep_tokens`].
#[must_use]
pub fn sum_sweep_tokens_by_model(
    projects_dir: &Path,
    workspace_root: &Path,
    issue: u32,
    window: Option<(DateTime<Utc>, DateTime<Utc>)>,
) -> Option<Vec<ModelUsageTotals>> {
    let project = projects_dir.join(project_slug(workspace_root));
    let entries = std::fs::read_dir(&project).ok()?;

    let mut totals: std::collections::BTreeMap<(String, String, String), ModelUsageTotals> =
        std::collections::BTreeMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "jsonl") {
            continue;
        }
        if !mtime_in_window(&path, window) {
            continue;
        }
        let Some(head) = read_head(&path) else {
            continue;
        };
        if !head_names_sweep_issue(&head, issue) {
            continue;
        }
        for transcript in session_transcripts(&path) {
            let too_big =
                std::fs::metadata(&transcript).is_ok_and(|m| m.len() > MAX_TRANSCRIPT_BYTES);
            if too_big {
                log::warn!(
                    "safehouse: skipping oversized transcript {} for issue #{issue} per-model token total",
                    transcript.display()
                );
                continue;
            }
            for row in sum_transcript_usage_by_model(&transcript) {
                let key = (row.model.clone(), row.speed.clone(), row.service_tier.clone());
                let entry = totals.entry(key).or_insert_with(|| ModelUsageTotals {
                    model: row.model.clone(),
                    speed: row.speed.clone(),
                    service_tier: row.service_tier.clone(),
                    ..ModelUsageTotals::default()
                });
                entry.input += row.input;
                entry.cache_read += row.cache_read;
                entry.cache_write_5m += row.cache_write_5m;
                entry.cache_write_1h += row.cache_write_1h;
                entry.output += row.output;
            }
        }
    }
    let rows: Vec<ModelUsageTotals> = totals.into_values().collect();
    (!rows.is_empty()).then_some(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;
    use std::fs;

    fn sweep_head(issue_args: &str) -> String {
        format!(
            "{{\"type\":\"queue-operation\"}}\n\
             {{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\
             \"<command-message>loom:sweep</command-message>\\n\
             <command-name>/loom:sweep</command-name>\\n\
             <command-args>{issue_args}</command-args>\"}}}}\n"
        )
    }

    fn usage_line(input: i64, output: i64, cache_read: i64, cache_create: i64) -> String {
        format!(
            "{{\"type\":\"assistant\",\"message\":{{\"model\":\"claude-sonnet-4-5\",\
             \"usage\":{{\"input_tokens\":{input},\"output_tokens\":{output},\
             \"cache_read_input_tokens\":{cache_read},\
             \"cache_creation_input_tokens\":{cache_create}}}}}}}\n"
        )
    }

    /// Same shape as [`usage_line`] but with an explicit `model` and the
    /// 5m/1h cache-write split, for the #5740 per-model tests.
    fn usage_line_for_model(
        model: &str,
        input: i64,
        output: i64,
        cache_read: i64,
        cache_write_5m: i64,
        cache_write_1h: i64,
    ) -> String {
        format!(
            "{{\"type\":\"assistant\",\"message\":{{\"model\":\"{model}\",\
             \"usage\":{{\"input_tokens\":{input},\"output_tokens\":{output},\
             \"cache_read_input_tokens\":{cache_read},\
             \"cache_creation_input_tokens\":{},\
             \"cache_creation\":{{\"ephemeral_5m_input_tokens\":{cache_write_5m},\
             \"ephemeral_1h_input_tokens\":{cache_write_1h}}}}}}}}}\n",
            cache_write_5m + cache_write_1h
        )
    }

    /// Build `<projects>/<slug>/<uuid>.jsonl` (+ optional subagents) for a
    /// session launched as `/loom:sweep <issue_args>`.
    fn seed_session(
        projects: &Path,
        workspace: &Path,
        uuid: &str,
        issue_args: &str,
        parent_usage: &str,
        subagent_usage: &[&str],
    ) -> PathBuf {
        let dir = projects.join(project_slug(workspace));
        fs::create_dir_all(&dir).unwrap();
        let session = dir.join(format!("{uuid}.jsonl"));
        fs::write(&session, format!("{}{parent_usage}", sweep_head(issue_args))).unwrap();
        if !subagent_usage.is_empty() {
            let sub = dir.join(uuid).join("subagents");
            fs::create_dir_all(&sub).unwrap();
            for (i, body) in subagent_usage.iter().enumerate() {
                fs::write(sub.join(format!("agent-{i}.jsonl")), body).unwrap();
            }
        }
        session
    }

    #[test]
    fn project_slug_matches_claude_codes_cwd_mangling() {
        assert_eq!(project_slug(Path::new("/Users/me/GitHub/loom")), "-Users-me-GitHub-loom");
        // A dot-directory contributes its own `-` on top of the separator's,
        // and hyphens inside a component are preserved verbatim.
        assert_eq!(
            project_slug(Path::new("/Users/me/GitHub/lean-genius/.loom/worktrees/x")),
            "-Users-me-GitHub-lean-genius--loom-worktrees-x"
        );
    }

    #[test]
    fn head_matches_only_the_sweeps_own_issue_number() {
        let head = sweep_head("4705 --claim-owned 4705");
        assert!(head_names_sweep_issue(&head, 4705));
        // A prefix of the real number must not match (the guard against
        // substring-style attribution).
        assert!(!head_names_sweep_issue(&head, 470));
        assert!(!head_names_sweep_issue(&head, 47050));
        // A number that appears only as a later argument is not the subject.
        assert!(!head_names_sweep_issue(&sweep_head("4705 --prs 999"), 999));
    }

    #[test]
    fn head_ignores_sessions_that_are_not_sweeps() {
        let other = "{\"type\":\"user\",\"message\":{\"content\":\
             \"<command-name>/loom:builder</command-name>\\n\
             <command-args>4705</command-args>\"}}";
        assert!(!head_names_sweep_issue(other, 4705));
        assert!(!head_names_sweep_issue("no markup here at all", 4705));
    }

    #[test]
    fn sums_parent_and_subagent_usage_including_cache_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Path::new("/Users/me/GitHub/loom");
        seed_session(
            dir.path(),
            workspace,
            "uuid-a",
            "4699 --claim-owned 4699",
            &usage_line(10, 20, 300, 40),
            &[&usage_line(1, 2, 30, 4), &usage_line(100, 200, 3000, 400)],
        );

        // 370 (parent) + 37 (agent-0) + 3700 (agent-1)
        assert_eq!(sum_sweep_tokens(dir.path(), workspace, 4699, None), Some(4107));
    }

    #[test]
    fn sums_every_session_for_a_redispatched_issue_but_not_other_issues() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Path::new("/Users/me/GitHub/loom");
        seed_session(dir.path(), workspace, "uuid-a", "4699", &usage_line(1, 1, 1, 1), &[]);
        seed_session(dir.path(), workspace, "uuid-b", "4699", &usage_line(2, 2, 2, 2), &[]);
        seed_session(dir.path(), workspace, "uuid-c", "4242", &usage_line(9, 9, 9, 9), &[]);

        assert_eq!(sum_sweep_tokens(dir.path(), workspace, 4699, None), Some(12));
        assert_eq!(sum_sweep_tokens(dir.path(), workspace, 4242, None), Some(36));
    }

    #[test]
    fn returns_none_for_an_unknown_issue_or_project() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Path::new("/Users/me/GitHub/loom");
        seed_session(dir.path(), workspace, "uuid-a", "4699", &usage_line(1, 1, 1, 1), &[]);

        // Right project, no such sweep.
        assert_eq!(sum_sweep_tokens(dir.path(), workspace, 1234, None), None);
        // No project directory at all — the common case on a host that has
        // never run a sweep from this workspace.
        assert_eq!(sum_sweep_tokens(dir.path(), Path::new("/nope/nowhere"), 4699, None), None);
    }

    #[test]
    fn a_transcript_with_no_usage_blocks_yields_none_not_zero() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Path::new("/Users/me/GitHub/loom");
        seed_session(dir.path(), workspace, "uuid-a", "4699", "", &[]);

        assert_eq!(sum_sweep_tokens(dir.path(), workspace, 4699, None), None);
    }

    #[test]
    fn the_mtime_window_excludes_sessions_outside_the_completion() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Path::new("/Users/me/GitHub/loom");
        seed_session(dir.path(), workspace, "uuid-a", "4699", &usage_line(1, 1, 1, 1), &[]);

        // Freshly written, so a window around "now" keeps it...
        let now = Utc::now();
        let live = Some((now - chrono::Duration::minutes(30), now));
        assert_eq!(sum_sweep_tokens(dir.path(), workspace, 4699, live), Some(4));

        // ...while a window in the distant past (well beyond WINDOW_SLACK)
        // filters it out before the file is ever read.
        let then = Utc.with_ymd_and_hms(2000, 1, 1, 0, 0, 0).unwrap();
        let stale = Some((then, then + chrono::Duration::hours(1)));
        assert_eq!(sum_sweep_tokens(dir.path(), workspace, 4699, stale), None);
    }

    // --- sum_sweep_tokens_split (#5357) -------------------------------------

    #[test]
    fn split_sums_input_and_output_axes_separately_including_cache_as_input() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Path::new("/Users/me/GitHub/loom");
        seed_session(
            dir.path(),
            workspace,
            "uuid-a",
            "4699 --claim-owned 4699",
            &usage_line(10, 20, 300, 40),
            &[&usage_line(1, 2, 30, 4), &usage_line(100, 200, 3000, 400)],
        );

        // input = (10+300+40) + (1+30+4) + (100+3000+400) = 350 + 35 + 3500
        // output = 20 + 2 + 200
        assert_eq!(sum_sweep_tokens_split(dir.path(), workspace, 4699, None), Some((3885, 222)));
        // The combined total (sum_sweep_tokens) still matches input+output.
        assert_eq!(sum_sweep_tokens(dir.path(), workspace, 4699, None), Some(4107));
    }

    #[test]
    fn split_returns_none_for_an_unknown_issue_or_project() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Path::new("/Users/me/GitHub/loom");
        seed_session(dir.path(), workspace, "uuid-a", "4699", &usage_line(1, 1, 1, 1), &[]);

        assert_eq!(sum_sweep_tokens_split(dir.path(), workspace, 1234, None), None);
        assert_eq!(
            sum_sweep_tokens_split(dir.path(), Path::new("/nope/nowhere"), 4699, None),
            None
        );
    }

    #[test]
    fn split_yields_none_not_zero_when_no_usage_blocks_are_present() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Path::new("/Users/me/GitHub/loom");
        seed_session(dir.path(), workspace, "uuid-a", "4699", "", &[]);

        assert_eq!(sum_sweep_tokens_split(dir.path(), workspace, 4699, None), None);
    }

    // --- sum_sweep_tokens_by_model (#5740) ----------------------------------

    #[test]
    fn by_model_merges_matching_tuples_across_parent_and_subagent_transcripts() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Path::new("/Users/me/GitHub/loom");
        seed_session(
            dir.path(),
            workspace,
            "uuid-a",
            "4699 --claim-owned 4699",
            &usage_line_for_model("claude-sonnet-5", 10, 20, 300, 5, 35),
            &[
                &usage_line_for_model("claude-sonnet-5", 1, 2, 30, 0, 4),
                &usage_line_for_model("claude-opus-5", 100, 200, 3000, 100, 300),
            ],
        );

        let rows = sum_sweep_tokens_by_model(dir.path(), workspace, 4699, None)
            .expect("attributable rows expected");
        assert_eq!(rows.len(), 2, "one row per (model, speed, service_tier): {rows:?}");

        let sonnet = rows.iter().find(|r| r.model == "claude-sonnet-5").unwrap();
        assert_eq!(sonnet.input, 11);
        assert_eq!(sonnet.cache_read, 330);
        assert_eq!(sonnet.cache_write_5m, 5);
        assert_eq!(sonnet.cache_write_1h, 39);
        assert_eq!(sonnet.output, 22);

        let opus = rows.iter().find(|r| r.model == "claude-opus-5").unwrap();
        assert_eq!(opus.input, 100);
        assert_eq!(opus.cache_read, 3000);
        assert_eq!(opus.cache_write_5m, 100);
        assert_eq!(opus.cache_write_1h, 300);
        assert_eq!(opus.output, 200);
    }

    #[test]
    fn by_model_returns_none_for_an_unknown_issue_or_project() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Path::new("/Users/me/GitHub/loom");
        seed_session(
            dir.path(),
            workspace,
            "uuid-a",
            "4699",
            &usage_line_for_model("claude-sonnet-5", 1, 1, 1, 1, 0),
            &[],
        );

        assert_eq!(sum_sweep_tokens_by_model(dir.path(), workspace, 1234, None), None);
        assert_eq!(
            sum_sweep_tokens_by_model(dir.path(), Path::new("/nope/nowhere"), 4699, None),
            None
        );
    }

    #[test]
    fn by_model_yields_none_not_empty_vec_when_no_usage_blocks_are_present() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Path::new("/Users/me/GitHub/loom");
        seed_session(dir.path(), workspace, "uuid-a", "4699", "", &[]);

        assert_eq!(sum_sweep_tokens_by_model(dir.path(), workspace, 4699, None), None);
    }
}
