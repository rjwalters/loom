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

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};

use crate::script_helpers::sweep_experiment::{
    sum_transcript_usage, sum_transcript_usage_by_model, ModelUsageTotals,
};
use crate::script_helpers::transcript_usage::{merge_records, UsageFold};

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
///
/// `pub(crate)` since #8056: the role-tick journal attributes transcripts by
/// the SAME first-user-message slash-command marker, and a second private
/// copy of this bounded head read would be free to drift from this one.
pub(crate) fn read_head(path: &Path) -> Option<String> {
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
///
/// Public since #8059: `activity::transcript_ingest` enumerates exactly the
/// same set of files when ingesting token usage into `activity.db`, and a
/// second private copy of this walk would be free to drift from this one.
#[must_use]
pub fn session_transcripts(session_jsonl: &Path) -> Vec<PathBuf> {
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

/// Every transcript file attributable to `issue`'s own `/loom:sweep` sessions:
/// each parent session under `projects_dir`/`slug(workspace_root)` whose mtime
/// falls in `window` and whose head names this issue, plus that session's
/// subagent transcripts, with oversized files dropped (and logged as `what`).
///
/// `None` — never an empty vec — when the project directory cannot be read, so
/// every caller keeps its "unknown is not zero" early return.
///
/// Extracted by #9443 so the flat total, the input/output split, the per-model
/// breakdown, and the per-phase windowed fold provably scan the **same** file
/// set: the per-phase numbers are reconciled against the sweep totals, and a
/// drifted file set would break that invariant silently.
fn sweep_transcript_files(
    projects_dir: &Path,
    workspace_root: &Path,
    issue: u32,
    window: Option<(DateTime<Utc>, DateTime<Utc>)>,
    what: &str,
) -> Option<Vec<PathBuf>> {
    let project = projects_dir.join(project_slug(workspace_root));
    let entries = std::fs::read_dir(&project).ok()?;
    let mut out = Vec::new();
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
            if std::fs::metadata(&transcript).is_ok_and(|m| m.len() > MAX_TRANSCRIPT_BYTES) {
                log::warn!(
                    "{what}: skipping oversized transcript {} for issue #{issue}",
                    transcript.display()
                );
                continue;
            }
            out.push(transcript);
        }
    }
    Some(out)
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
    let files = sweep_transcript_files(
        projects_dir,
        workspace_root,
        issue,
        window,
        "safehouse token total",
    )?;
    let mut total: u64 = 0;
    for transcript in files {
        let usage = sum_transcript_usage(&transcript);
        let sum = usage
            .input_tokens
            .saturating_add(usage.output_tokens)
            .saturating_add(usage.cache_read_input_tokens)
            .saturating_add(usage.cache_creation_input_tokens);
        total = total.saturating_add(u64::try_from(sum).unwrap_or(0));
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
    let files = sweep_transcript_files(
        projects_dir,
        workspace_root,
        issue,
        window,
        "sweep.outcome token split",
    )?;
    let mut input_total: u64 = 0;
    let mut output_total: u64 = 0;
    for transcript in files {
        let usage = sum_transcript_usage(&transcript);
        let input = usage
            .input_tokens
            .saturating_add(usage.cache_read_input_tokens)
            .saturating_add(usage.cache_creation_input_tokens);
        input_total = input_total.saturating_add(u64::try_from(input).unwrap_or(0));
        output_total = output_total.saturating_add(u64::try_from(usage.output_tokens).unwrap_or(0));
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
    let files = sweep_transcript_files(
        projects_dir,
        workspace_root,
        issue,
        window,
        "safehouse per-model token total",
    )?;
    let mut totals: BTreeMap<(String, String, String), ModelUsageTotals> = BTreeMap::new();
    for transcript in files {
        crate::script_helpers::transcript_usage::merge_rows(
            &mut totals,
            sum_transcript_usage_by_model(&transcript),
        );
    }
    let rows: Vec<ModelUsageTotals> = totals.into_values().collect();
    (!rows.is_empty()).then_some(rows)
}

/// Which of `slices` an instant belongs to (Issue #9443), or `None` when it
/// falls outside every one of them.
///
/// Slices are half-open `[start, end)` so contiguous phase windows — which is
/// what the sampler produces, each phase ending exactly where the next begins —
/// partition their records with no double-count. The **final** slice is closed
/// at its end instead, so a record written at the exact last phase boundary is
/// attributed rather than silently dropped into the remainder.
#[must_use]
fn slice_of(at: DateTime<Utc>, slices: &[(DateTime<Utc>, DateTime<Utc>)]) -> Option<usize> {
    if let Some(index) = slices
        .iter()
        .position(|(start, end)| at >= *start && at < *end)
    {
        return Some(index);
    }
    let last = slices.len().checked_sub(1)?;
    (at == slices[last].1).then_some(last)
}

/// Per-model token totals for `issue`'s `/loom:sweep` sessions, split across
/// `slices` (Issue #9443) — the per-phase counterpart of
/// [`sum_sweep_tokens_by_model`], and what makes clean-landing cost separable
/// from rework cost.
///
/// Returns one entry per slice, in the same order, `None` where no usage record
/// fell inside that slice (absent, never a fabricated zero row set). The vec is
/// always `slices.len()` long — including when the project directory is
/// unreadable, which yields all-`None`.
///
/// # Why this cannot be `sum_sweep_tokens_by_model(…, Some(phase_window))`
///
/// `window` in every other function here is a **file-mtime** prefilter, not a
/// per-record filter: a session file's mtime is when it was last appended to,
/// so a narrow per-phase window would admit or reject a transcript *whole*.
/// Handed phase windows, it would attribute every token of a sweep to whichever
/// phase happened to contain the file's final write — i.e. the last one — and
/// nothing to the rest. So attribution is done per **record**, keyed on the
/// record's own `timestamp` ([`UsageRecord::at`]), with `window` kept for its
/// original cheap-prefilter job.
///
/// # What it is honest about
///
/// - Dedupe happens per transcript **before** partitioning ([`UsageFold`]), and
///   a message is attributed wholly to the phase its first chunk started in, so
///   Σ over the slices plus the remainder equals the sweep total exactly — a
///   streamed message straddling a boundary is never counted twice.
/// - A record with no parseable `timestamp`, or one outside every slice (the
///   trailing in-flight segment the sampler cannot name a phase for, or an
///   earlier dispatch's session admitted by the mtime slack), is attributed to
///   **no** slice. The caller reports that remainder as `tokens_unattributed`.
/// - Slice boundaries are the sampled phase-transition instants, so attribution
///   is accurate to within one reaper tick — the same caveat `phase_durations`
///   already carries.
///
/// [`UsageRecord::at`]: crate::script_helpers::transcript_usage::UsageRecord::at
#[must_use]
pub fn sum_sweep_tokens_by_window(
    projects_dir: &Path,
    workspace_root: &Path,
    issue: u32,
    window: Option<(DateTime<Utc>, DateTime<Utc>)>,
    slices: &[(DateTime<Utc>, DateTime<Utc>)],
) -> Vec<Option<Vec<ModelUsageTotals>>> {
    if slices.is_empty() {
        return Vec::new();
    }
    let Some(files) = sweep_transcript_files(
        projects_dir,
        workspace_root,
        issue,
        window,
        "sweep.outcome per-phase token attribution",
    ) else {
        return vec![None; slices.len()];
    };
    let mut totals: Vec<BTreeMap<(String, String, String), ModelUsageTotals>> =
        vec![BTreeMap::new(); slices.len()];
    for transcript in files {
        let Ok(text) = std::fs::read_to_string(&transcript) else {
            continue;
        };
        let mut fold = UsageFold::default();
        fold.add_text(&text);
        for message in fold.messages() {
            let Some(index) = message.at().and_then(|at| slice_of(at, slices)) else {
                continue;
            };
            merge_records(&mut totals[index], std::iter::once(message));
        }
    }
    totals
        .into_iter()
        .map(|rows| {
            let rows: Vec<ModelUsageTotals> = rows.into_values().collect();
            (!rows.is_empty()).then_some(rows)
        })
        .collect()
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

    // ----------------------------------------------------------------------
    // Per-phase windowed attribution (Issue #9443)
    // ----------------------------------------------------------------------

    /// A stamped usage record: a distinct `message.id` (so the fold's dedupe
    /// treats each as its own message) and a top-level `timestamp`, which is
    /// what the windowed fold partitions on.
    fn stamped(id: &str, at: DateTime<Utc>, input: i64, output: i64) -> String {
        format!(
            "{{\"type\":\"assistant\",\"timestamp\":\"{}\",\"message\":{{\"id\":\"{id}\",\
             \"model\":\"claude-sonnet-5\",\"usage\":{{\"input_tokens\":{input},\
             \"output_tokens\":{output},\"cache_read_input_tokens\":0,\
             \"cache_creation_input_tokens\":0}}}}}}\n",
            at.to_rfc3339()
        )
    }

    /// The same record with no `timestamp` at all — counted in the sweep
    /// totals, attributable to no phase.
    fn unstamped(id: &str, input: i64, output: i64) -> String {
        format!(
            "{{\"type\":\"assistant\",\"message\":{{\"id\":\"{id}\",\
             \"model\":\"claude-sonnet-5\",\"usage\":{{\"input_tokens\":{input},\
             \"output_tokens\":{output},\"cache_read_input_tokens\":0,\
             \"cache_creation_input_tokens\":0}}}}}}\n"
        )
    }

    fn split_of(rows: &[ModelUsageTotals]) -> (i64, i64) {
        (
            rows.iter()
                .map(|r| r.input + r.cache_read + r.cache_write_5m + r.cache_write_1h)
                .sum(),
            rows.iter().map(|r| r.output).sum(),
        )
    }

    /// AC: a `curator → builder → judge(fail) → doctor → judge(pass)` lifecycle
    /// yields five attributed windows, and Σ windows + the remainder equals the
    /// flat sweep split exactly — the invariant that makes clean-landing cost
    /// (curator + builder + FIRST judge) subtractable from the total.
    #[test]
    fn five_phase_windows_each_get_their_own_records_and_reconcile_with_the_sweep_total() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Path::new("/Users/me/GitHub/loom");
        let t0 = Utc.with_ymd_and_hms(2026, 9, 29, 0, 0, 0).unwrap();
        let mark = |secs: i64| t0 + chrono::Duration::seconds(secs);
        // Contiguous windows, as the phase sampler produces them.
        let slices = [
            (t0, mark(100)),
            (mark(100), mark(200)),
            (mark(200), mark(300)),
            (mark(300), mark(400)),
            (mark(400), mark(500)),
        ];
        let parent = [
            stamped("curator", mark(50), 10, 1),
            stamped("builder", mark(150), 100, 10),
            stamped("judge-1", mark(250), 20, 2),
            stamped("doctor", mark(350), 200, 20),
            // On the exact final boundary: the last window is closed at its end,
            // so this belongs to judge attempt 2 rather than being dropped.
            stamped("judge-2", mark(500), 30, 3),
            // After every window (the trailing in-flight segment) and with no
            // instant at all: both unattributable.
            stamped("trailing", mark(560), 7, 5),
            unstamped("no-clock", 3, 1),
        ]
        .concat();
        seed_session(dir.path(), workspace, "uuid-a", "9443", &parent, &[]);

        let per_phase = sum_sweep_tokens_by_window(dir.path(), workspace, 9443, None, &slices);
        assert_eq!(per_phase.len(), 5, "one entry per requested window, in order");
        let splits: Vec<(i64, i64)> = per_phase
            .iter()
            .map(|rows| split_of(rows.as_deref().expect("every window here has records")))
            .collect();
        assert_eq!(splits, vec![(10, 1), (100, 10), (20, 2), (200, 20), (30, 3)]);

        // Σ windows + remainder == the flat split over the same file set.
        let (total_in, total_out) =
            sum_sweep_tokens_split(dir.path(), workspace, 9443, None).unwrap();
        let attributed_in: i64 = splits.iter().map(|s| s.0).sum();
        let attributed_out: i64 = splits.iter().map(|s| s.1).sum();
        assert_eq!(
            (attributed_in, attributed_out),
            (360, 36),
            "the trailing and clock-less records are attributed to no window"
        );
        assert_eq!(
            (
                u64::try_from(attributed_in).unwrap() + 10,
                u64::try_from(attributed_out).unwrap() + 6
            ),
            (total_in, total_out),
            "remainder is the trailing (7,5) plus the clock-less (3,1) record"
        );
    }

    /// AC: an unmeasured window is **absent**, never a zero row set — and the
    /// invariant still holds, with the shortfall showing up as a larger
    /// remainder rather than as a phase claiming it was free.
    #[test]
    fn a_window_with_no_records_is_absent_rather_than_a_zero_row_set() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Path::new("/Users/me/GitHub/loom");
        let t0 = Utc.with_ymd_and_hms(2026, 9, 29, 0, 0, 0).unwrap();
        let mark = |secs: i64| t0 + chrono::Duration::seconds(secs);
        let slices = [
            (t0, mark(100)),
            (mark(100), mark(200)),
            (mark(200), mark(300)),
        ];
        let parent = [
            stamped("curator", mark(50), 10, 1),
            stamped("judge", mark(250), 30, 3),
        ]
        .concat();
        seed_session(dir.path(), workspace, "uuid-a", "9443", &parent, &[]);

        let per_phase = sum_sweep_tokens_by_window(dir.path(), workspace, 9443, None, &slices);
        assert!(per_phase[0].is_some());
        assert_eq!(per_phase[1], None, "the empty middle window must be absent, not Some(vec![])");
        assert!(per_phase[2].is_some());
    }

    /// A streamed message's chunks repeat one `message.id` with cumulative
    /// counters. Dedupe happens before partitioning, so a message whose chunks
    /// straddle a window boundary counts ONCE, in the window it started in —
    /// the property that keeps Σ windows ≤ the sweep total.
    #[test]
    fn a_streamed_message_straddling_a_boundary_is_counted_once_in_the_window_it_started_in() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Path::new("/Users/me/GitHub/loom");
        let t0 = Utc.with_ymd_and_hms(2026, 9, 29, 0, 0, 0).unwrap();
        let mark = |secs: i64| t0 + chrono::Duration::seconds(secs);
        let slices = [(t0, mark(100)), (mark(100), mark(200))];
        let parent = [
            stamped("streamed", mark(95), 40, 4),
            // Later chunk of the SAME message, past the boundary, carrying the
            // cumulative counters.
            stamped("streamed", mark(105), 90, 9),
        ]
        .concat();
        seed_session(dir.path(), workspace, "uuid-a", "9443", &parent, &[]);

        let per_phase = sum_sweep_tokens_by_window(dir.path(), workspace, 9443, None, &slices);
        assert_eq!(split_of(per_phase[0].as_deref().unwrap()), (90, 9), "max over chunks, once");
        assert_eq!(per_phase[1], None, "the second chunk is not a second message");
        assert_eq!(
            sum_sweep_tokens_split(dir.path(), workspace, 9443, None),
            Some((90, 9)),
            "and the sweep total counts it exactly once too"
        );
    }

    /// No slices asked for ⇒ no work and no entries; an unreadable project
    /// directory still returns one entry per slice, all absent.
    #[test]
    fn degenerate_inputs_never_fabricate_entries_or_zeros() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Path::new("/Users/me/GitHub/loom");
        let t0 = Utc.with_ymd_and_hms(2026, 9, 29, 0, 0, 0).unwrap();
        let slices = [(t0, t0 + chrono::Duration::seconds(10))];

        assert!(sum_sweep_tokens_by_window(dir.path(), workspace, 9443, None, &[]).is_empty());
        assert_eq!(
            sum_sweep_tokens_by_window(dir.path(), workspace, 9443, None, &slices),
            vec![None],
            "an unreadable project directory is unknown for every window"
        );
    }
}
