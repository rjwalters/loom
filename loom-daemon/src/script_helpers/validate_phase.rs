//! Sweep phase-contract validation — the native port of
//! `loom_tools.validate_phase` (#4275), behind `validate-phase.sh`.
//!
//! A phase-contract validator checks that the expected artifacts exist after a
//! `/loom:sweep` phase completes (e.g. the Builder created a PR carrying
//! `loom:review-requested`). When a contract is not satisfied the validator
//! marks the issue `loom:blocked` and posts diagnostics for manual
//! intervention.
//!
//! | Phase | Contract | Recovery |
//! |---|---|---|
//! | `curator` | issue has `loom:curated` | apply the label |
//! | `builder` | an open PR with `loom:review-requested` exists | add the label, or commit+push+open a PR from worktree changes |
//! | `judge` | PR has `loom:pr` or `loom:changes-requested` | none (diagnose only) |
//! | `doctor` | PR has `loom:review-requested` | none (diagnose only) |
//!
//! Exit codes (unchanged from the Python CLI): `0` contract satisfied (initially
//! or after recovery), `1` contract failed, `2` invalid arguments.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

use super::{run_gh, run_git, GhResult};

/// The phases this validator knows how to check.
pub const VALID_PHASES: [&str; 4] = ["curator", "builder", "judge", "doctor"];

/// Recovery events are truncated to this many entries so
/// `.loom/metrics/recovery-events.json` cannot grow unbounded.
const MAX_RECOVERY_EVENTS: usize = 1000;

/// Outcome of a phase-contract check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationStatus {
    Satisfied,
    Recovered,
    Failed,
}

impl ValidationStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Satisfied => "satisfied",
            Self::Recovered => "recovered",
            Self::Failed => "failed",
        }
    }

    /// The single-glyph prefix the human-readable CLI output uses.
    #[must_use]
    pub const fn glyph(self) -> &'static str {
        match self {
            Self::Satisfied => "\u{2713}",
            Self::Recovered => "\u{27f3}",
            Self::Failed => "\u{2717}",
        }
    }
}

/// Result of a phase-contract validation.
#[derive(Debug, Clone)]
pub struct ValidationResult {
    pub phase: String,
    pub issue: i64,
    pub status: ValidationStatus,
    pub message: String,
    pub recovery_action: String,
}

impl ValidationResult {
    fn new(phase: &str, issue: i64, status: ValidationStatus, message: impl Into<String>) -> Self {
        Self {
            phase: phase.to_string(),
            issue,
            status,
            message: message.into(),
            recovery_action: "none".to_string(),
        }
    }

    fn with_action(mut self, action: &str) -> Self {
        self.recovery_action = action.to_string();
        self
    }

    /// True when the contract is met — either initially or after recovery.
    #[must_use]
    pub const fn satisfied(&self) -> bool {
        matches!(self.status, ValidationStatus::Satisfied | ValidationStatus::Recovered)
    }

    /// The JSON shape the bash script and its consumers already parse.
    #[must_use]
    pub fn to_value(&self) -> Value {
        json!({
            "phase": self.phase,
            "issue": self.issue,
            "status": self.status.as_str(),
            "message": self.message,
            "recovery_action": self.recovery_action,
        })
    }
}

/// Everything a validation run needs, so the whole surface stays testable
/// without touching the process environment.
#[derive(Debug, Clone, Default)]
pub struct ValidateOpts {
    pub phase: String,
    pub issue: i64,
    pub worktree: Option<String>,
    pub pr_number: Option<i64>,
    pub task_id: Option<String>,
    pub json_output: bool,
    /// Only check contract status; skip every side effect (labels, comments,
    /// commits, pushes, PR creation).
    pub check_only: bool,
    /// Attempt recovery but suppress diagnostic comments and label changes on
    /// failure. Used by retry loops so an intermediate failure does not leave a
    /// noisy comment behind after the sweep later recovers (issue #2609).
    pub quiet: bool,
}

// --------------------------------------------------------------------------
// Side-effect helpers
// --------------------------------------------------------------------------

/// Invoke `report-milestone.sh` when a task id is set (best-effort, never
/// fatal — a missing script is simply skipped).
fn report_milestone(task_id: Option<&str>, repo_root: &Path, action: &str) {
    let Some(task_id) = task_id.filter(|t| !t.is_empty()) else {
        return;
    };
    let script = repo_root
        .join(".loom")
        .join("scripts")
        .join("report-milestone.sh");
    if !script.is_file() {
        return;
    }
    let _ = std::process::Command::new(&script)
        .args(["heartbeat", "--task-id", task_id, "--action", action])
        .output();
}

/// Append a recovery event to `.loom/metrics/recovery-events.json`, keeping only
/// the most recent [`MAX_RECOVERY_EVENTS`].
fn log_recovery_event(
    repo_root: &Path,
    issue: i64,
    recovery_type: &str,
    reason: &str,
    worktree_had_changes: bool,
    pr_number: Option<i64>,
    builder_exit_reason: Option<&str>,
) {
    let metrics_dir = repo_root.join(".loom").join("metrics");
    let recovery_file = metrics_dir.join("recovery-events.json");
    if std::fs::create_dir_all(&metrics_dir).is_err() {
        super::log_warning(&format!(
            "Failed to create metrics directory {}",
            metrics_dir.display()
        ));
        return;
    }

    let mut event = json!({
        "timestamp": super::now_iso(),
        "issue": issue,
        "recovery_type": recovery_type,
        "reason": reason,
        "elapsed_seconds": Value::Null,
        "worktree_had_changes": worktree_had_changes,
        "commits_recovered": 0,
        "pr_number": pr_number.map_or(Value::Null, Value::from),
    });
    if let Some(exit_reason) = builder_exit_reason {
        if let Some(obj) = event.as_object_mut() {
            obj.insert("builder_exit_reason".into(), json!(exit_reason));
        }
    }

    let mut events: Vec<Value> = super::read_json_file(&recovery_file)
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    events.push(event);
    if events.len() > MAX_RECOVERY_EVENTS {
        events.drain(..events.len() - MAX_RECOVERY_EVENTS);
    }
    if super::write_json_file(&recovery_file, &Value::Array(events)).is_err() {
        super::log_warning(&format!(
            "Failed to write recovery event to {}",
            recovery_file.display()
        ));
    }
}

/// Mark the issue with a phase-failure label and post a diagnostic comment.
///
/// A `quiet` call is a complete no-op: that is the #2609 contract — an
/// intermediate retry must not leave a comment that outlives the failure.
fn mark_phase_failed(
    repo_root: &Path,
    issue: i64,
    phase: &str,
    reason: &str,
    diagnostics: &str,
    quiet: bool,
) {
    if quiet {
        return;
    }
    let issue_s = issue.to_string();
    let _ = run_gh(
        &[
            "issue",
            "edit",
            &issue_s,
            "--remove-label",
            "loom:building",
            "--add-label",
            "loom:blocked",
        ],
        repo_root,
        false,
    );

    let mut body = format!(
        "**Phase contract failed**: `{phase}` phase did not produce expected outcome. \
         {reason}\n\n\
         For label state documentation and manual recovery steps, see \
         [`.claude/commands/loom/shepherd-lifecycle.md`]\
         (../blob/main/.claude/commands/loom/shepherd-lifecycle.md#label-state-machine)."
    );
    if !diagnostics.is_empty() {
        body.push_str("\n\n");
        body.push_str(diagnostics);
    }
    let _ = run_gh(&["issue", "comment", &issue_s, "--body", &body], repo_root, false);
}

// --------------------------------------------------------------------------
// PR search helpers
// --------------------------------------------------------------------------

/// Parse a PR number from `gh` output; `None` for empty/`null`/non-numeric.
#[must_use]
pub fn parse_pr_number(output: &str) -> Option<i64> {
    let text = output.trim();
    if text.is_empty() || text == "null" {
        return None;
    }
    text.parse::<i64>().ok()
}

/// Find an open PR for `issue`, returning `(number, found_by)`.
///
/// Search order: the caller's cached number (validated still OPEN), the
/// `feature/issue-<N>` branch name, then a body search for each closing keyword.
fn find_pr_for_issue(
    repo_root: &Path,
    issue: i64,
    cached_pr: Option<i64>,
) -> Option<(i64, &'static str)> {
    if let Some(cached) = cached_pr {
        let cached_s = cached.to_string();
        let r = run_gh(
            &["pr", "view", &cached_s, "--json", "state", "--jq", ".state"],
            repo_root,
            true,
        );
        if r.success && r.trimmed_stdout() == "OPEN" {
            return Some((cached, "caller_cached"));
        }
    }

    let head = format!("feature/issue-{issue}");
    let r = run_gh(
        &[
            "pr",
            "list",
            "--head",
            &head,
            "--state",
            "open",
            "--json",
            "number",
            "--jq",
            ".[0].number",
        ],
        repo_root,
        true,
    );
    if let Some(pr) = parse_pr_number(&r.stdout) {
        return Some((pr, "branch_name"));
    }

    for (keyword, found_by) in [
        ("Closes", "closes_keyword"),
        ("Fixes", "fixes_keyword"),
        ("Resolves", "resolves_keyword"),
    ] {
        let search = format!("{keyword} #{issue}");
        let r = run_gh(
            &[
                "pr",
                "list",
                "--search",
                &search,
                "--state",
                "open",
                "--json",
                "number",
                "--jq",
                ".[0].number",
            ],
            repo_root,
            true,
        );
        if let Some(pr) = parse_pr_number(&r.stdout) {
            return Some((pr, found_by));
        }
    }
    None
}

/// The label names on a PR (one per line from `gh`), or an empty vector on
/// failure.
fn pr_labels(repo_root: &Path, pr: i64) -> Vec<String> {
    let pr_s = pr.to_string();
    let r = run_gh(
        &[
            "pr",
            "view",
            &pr_s,
            "--json",
            "labels",
            "--jq",
            ".labels[].name",
        ],
        repo_root,
        true,
    );
    if !r.success {
        return Vec::new();
    }
    r.stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Closing-keyword references (`Closes|Fixes|Resolves #N`) found in `body`.
///
/// Case-insensitive on the keyword, matching the Python `re.IGNORECASE` scan.
/// Exposed for testing because the wrong-issue rewrite below is the subtlest
/// part of the builder validator.
#[must_use]
pub fn closing_references(body: &str) -> Vec<(String, i64)> {
    let mut out = Vec::new();
    for keyword in ["closes", "fixes", "resolves"] {
        let lower = body.to_lowercase();
        let mut from = 0usize;
        while let Some(rel) = lower[from..].find(keyword) {
            let start = from + rel;
            from = start + keyword.len();
            let rest = &body[from..];
            let trimmed = rest.trim_start();
            let ws = rest.len() - trimmed.len();
            // The Python `\s+#(\d+)` requires at least one space then `#`.
            if ws == 0 || !trimmed.starts_with('#') {
                continue;
            }
            let digits: String = trimmed[1..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            if digits.is_empty() {
                continue;
            }
            if let Ok(n) = digits.parse::<i64>() {
                // Preserve the keyword's original casing, as the Python did.
                out.push((body[start..start + keyword.len()].to_string(), n));
            }
        }
    }
    out
}

/// Ensure the PR body contains a `Closes #<issue>` reference, and strike through
/// closing keywords that reference the *wrong* issue.
///
/// Runs for every discovered PR (not just branch-name matches) because a Builder
/// may have solved a different issue than the one being validated.
fn ensure_pr_body_references_issue(repo_root: &Path, pr: i64, issue: i64, task_id: Option<&str>) {
    let pr_s = pr.to_string();
    let r = run_gh(&["pr", "view", &pr_s, "--json", "body", "--jq", ".body"], repo_root, true);
    let mut body = if r.success {
        r.trimmed_stdout().to_string()
    } else {
        String::new()
    };

    let refs = closing_references(&body);
    let wrong: Vec<(String, i64)> = refs.iter().filter(|(_, n)| *n != issue).cloned().collect();
    let has_correct_ref = refs.iter().any(|(_, n)| *n == issue);
    let mut needs_edit = false;

    for (keyword, num) in &wrong {
        let needle = format!("{keyword} #{num}");
        if let Some(pos) = body.find(&needle) {
            let replacement = format!("~~{keyword} #{num}~~ (removed: wrong issue)");
            body.replace_range(pos..pos + needle.len(), &replacement);
            needs_edit = true;
        }
    }
    if !wrong.is_empty() {
        let wrong_list = wrong
            .iter()
            .map(|(_, n)| format!("#{n}"))
            .collect::<Vec<_>>()
            .join(", ");
        report_milestone(
            task_id,
            repo_root,
            &format!(
                "warning: PR #{pr} referenced wrong issue(s) {wrong_list} \
                 instead of #{issue} -- removed closing keywords"
            ),
        );
    }

    if !has_correct_ref {
        body = if body.is_empty() || body == "null" {
            format!("Closes #{issue}")
        } else {
            format!("{body}\n\nCloses #{issue}")
        };
        needs_edit = true;
    }

    if needs_edit {
        let r = run_gh(&["pr", "edit", &pr_s, "--body", &body], repo_root, false);
        if r.success {
            let mut action = format!("recovery: ensured PR #{pr} body references #{issue}");
            if !wrong.is_empty() {
                let wrong_list = wrong
                    .iter()
                    .map(|(_, n)| format!("#{n}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                action.push_str(&format!(" (removed wrong refs: {wrong_list})"));
            }
            report_milestone(task_id, repo_root, &action);
        }
    }
}

/// Generic PR-title anti-patterns that mean the Builder did not derive a title
/// from its diff. Matched case-insensitively.
#[must_use]
pub fn generic_title_reason(title: &str) -> Option<&'static str> {
    let t = title.trim().to_lowercase();
    if t.is_empty() {
        return None;
    }
    // `implement changes for issue` / `implement change for issue`
    if t.contains("implement changes for issue") || t.contains("implement change for issue") {
        return Some("implement changes for issue");
    }
    if t.contains("implement feature from issue") {
        return Some("implement feature from issue");
    }
    // `address issue #?<digits>`
    if let Some(rest) = t.split("address issue").nth(1) {
        let rest = rest.trim_start().trim_start_matches('#');
        if rest.starts_with(|c: char| c.is_ascii_digit()) {
            return Some("address issue #N");
        }
    }
    // A bare `issue #?<digits>` title and nothing else.
    if let Some(rest) = t.strip_prefix("issue") {
        let rest = rest.trim().trim_start_matches('#');
        if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
            return Some("bare issue #N");
        }
    }
    None
}

/// Log a warning when the PR title matches a known generic anti-pattern.
///
/// A *warning*, not a hard failure: the PR already exists, and blocking
/// validation here would disrupt the pipeline. The warning surfaces in logs and
/// milestones so the pattern can be tracked.
fn warn_generic_pr_title(repo_root: &Path, pr: i64, task_id: Option<&str>) {
    let pr_s = pr.to_string();
    let r = run_gh(&["pr", "view", &pr_s, "--json", "title", "--jq", ".title"], repo_root, true);
    if !r.success {
        return;
    }
    if let Some(pattern) = generic_title_reason(r.trimmed_stdout()) {
        report_milestone(
            task_id,
            repo_root,
            &format!(
                "warning: PR #{pr} has generic title matching anti-pattern /{pattern}/: {:?}",
                r.trimmed_stdout()
            ),
        );
    }
}

/// Whether a PR body is "minimal": no `## Summary` section, and under 80
/// characters of content once `Closes/Fixes/Resolves #N` lines are stripped.
#[must_use]
pub fn is_minimal_pr_body(body: &str) -> bool {
    if body.lines().any(|l| l.starts_with("## Summary")) {
        return false;
    }
    let stripped: String = body
        .lines()
        .filter(|line| closing_reference_only_line(line).is_none())
        .collect::<Vec<_>>()
        .join("\n");
    stripped.trim().chars().count() < 80
}

/// `Some(issue)` when `line` is *nothing but* a closing reference.
fn closing_reference_only_line(line: &str) -> Option<i64> {
    let t = line.trim();
    let lower = t.to_lowercase();
    for keyword in ["closes", "fixes", "resolves"] {
        if let Some(rest) = lower.strip_prefix(keyword) {
            let rest = rest.trim_start();
            if let Some(digits) = rest.strip_prefix('#') {
                if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
                    return digits.parse().ok();
                }
            }
        }
    }
    None
}

/// Enrich a PR body that carries no meaningful summary.
fn recover_minimal_pr_body(repo_root: &Path, pr: i64, issue: i64, task_id: Option<&str>) {
    let pr_s = pr.to_string();
    let r = run_gh(&["pr", "view", &pr_s, "--json", "body", "--jq", ".body"], repo_root, false);
    if !r.success {
        return;
    }
    let body = {
        let t = r.trimmed_stdout();
        if t.is_empty() || t == "null" {
            String::new()
        } else {
            t.to_string()
        }
    };
    if !is_minimal_pr_body(&body) {
        return;
    }

    let r = run_gh(
        &[
            "pr",
            "view",
            &pr_s,
            "--json",
            "files",
            "--jq",
            r#".files[] | "\(.path) (+\(.additions)/-\(.deletions))""#,
        ],
        repo_root,
        false,
    );
    let file_lines: Vec<String> = if r.success {
        r.stdout
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .take(25)
            .map(|l| format!("- `{l}`"))
            .collect()
    } else {
        Vec::new()
    };

    let mut parts: Vec<String> = vec![
        "## Summary".to_string(),
        String::new(),
        "> **Note:** This summary was auto-generated because the builder \
         created a PR with a minimal body."
            .to_string(),
        String::new(),
    ];
    if !file_lines.is_empty() {
        parts.push("## Changes".to_string());
        parts.push(String::new());
        parts.extend(file_lines);
        parts.push(String::new());
    }
    if !body.is_empty() {
        parts.push(body);
    }
    let new_body = parts.join("\n");

    let r = run_gh(&["pr", "edit", &pr_s, "--body", &new_body], repo_root, false);
    if r.success {
        report_milestone(
            task_id,
            repo_root,
            &format!("recovery: enriched minimal PR #{pr} body for issue #{issue}"),
        );
        log_recovery_event(
            repo_root,
            issue,
            "enrich_pr_body",
            "minimal_pr_body",
            false,
            Some(pr),
            None,
        );
    }
}

// --------------------------------------------------------------------------
// Builder diagnostics
// --------------------------------------------------------------------------

/// Diagnostic information gathered when builder validation fails.
#[derive(Debug, Default, Clone)]
pub struct BuilderDiagnostics {
    pub worktree_path: String,
    pub worktree_exists: bool,
    pub branch: String,
    pub commits_ahead: String,
    pub commits_behind: String,
    pub has_remote_tracking: bool,
    pub log_tail: String,
    pub log_path: String,
    pub issue_labels: String,
    pub main_uncommitted: String,
    pub issue: i64,
    pub worktree_mtime: String,
}

impl BuilderDiagnostics {
    /// The collapsible markdown block appended to the failure comment.
    #[must_use]
    pub fn to_markdown(&self) -> String {
        let issue = self.issue;
        let mut parts: Vec<String> =
            vec!["<details>\n<summary>Diagnostic Information</summary>\n".to_string()];

        if !self.worktree_mtime.is_empty() {
            parts.push("### Previous Attempt".to_string());
            parts.push(format!("**Worktree last modified**: {}", self.worktree_mtime));
            parts.push(String::new());
        }

        parts.push("### Worktree State".to_string());
        if self.worktree_exists {
            parts.push(format!("**Worktree**: `{}` exists", self.worktree_path));
            parts.push(format!("**Branch**: `{}`", self.branch));
            parts.push(format!("**Commits ahead of main**: {}", self.commits_ahead));
            parts.push(format!("**Commits behind main**: {}", self.commits_behind));
            let tracking = if self.has_remote_tracking {
                "configured"
            } else {
                "not configured (branch never pushed)"
            };
            parts.push(format!("**Remote tracking**: {tracking}"));
        } else {
            parts.push(format!("**Worktree**: `{}` does not exist", self.worktree_path));
        }

        if !self.log_tail.is_empty() {
            parts.push(format!("\n**Last 15 lines from session log** (`{}`):", self.log_path));
            parts.push(format!("```\n{}\n```", self.log_tail));
        }
        if !self.issue_labels.is_empty() {
            parts.push(format!("\n**Current issue labels**: {}", self.issue_labels));
        }
        if !self.main_uncommitted.is_empty() {
            parts.push(
                "\n**\u{26a0}\u{fe0f} WARNING: Uncommitted changes detected on main branch**:"
                    .to_string(),
            );
            parts.push(format!("```\n{}\n```", self.main_uncommitted));
            parts.push(
                "This suggests the builder may have worked directly on main instead of in a \
                 worktree.\nThis is a workflow violation - builders MUST work in worktrees."
                    .to_string(),
            );
        }

        parts.push("\n### Possible Causes".to_string());
        if self.worktree_exists {
            if self.commits_ahead == "0" || self.commits_ahead == "?" {
                parts.push("- Builder exited without making any commits".to_string());
                parts.push(
                    "- Builder may have determined issue was invalid or already resolved"
                        .to_string(),
                );
                parts.push(
                    "- Builder may have encountered an error during implementation".to_string(),
                );
                parts.push("- Builder may have timed out before completing work".to_string());
                parts.push(
                    "- **Agent may have worked on main instead of worktree** (check for \
                     uncommitted changes on main)"
                        .to_string(),
                );
            }
        } else {
            parts.push("- Worktree was never created (agent may have failed early)".to_string());
            parts.push("- Worktree creation script failed".to_string());
            parts.push(
                "- **Agent worked on main instead of worktree** (check for uncommitted \
                 changes on main)"
                    .to_string(),
            );
        }

        parts.push(format!(
            r#"
### Recovery Options

**Option A: Clean worktree and retry** (recommended if worktree has no valuable changes)
```bash
# Navigate to repo root first (worktree removal breaks shell CWD)
cd "$(git rev-parse --show-toplevel)"
# Remove stale worktree
git worktree remove .loom/worktrees/issue-{issue} --force 2>/dev/null || true
git branch -D feature/issue-{issue} 2>/dev/null || true
# Reset labels and retry
gh issue edit {issue} --remove-label loom:blocked --add-label loom:issue
claude -p "/loom:sweep {issue}" --dangerously-skip-permissions
```

**Option B: Retry preserving worktree** (if worktree may have partial work)
```bash
gh issue edit {issue} --remove-label loom:blocked --add-label loom:issue
claude -p "/loom:sweep {issue}" --dangerously-skip-permissions
```

**Option C: Complete manually**
1. Create worktree: `./.loom/scripts/worktree.sh {issue}`
2. Navigate: `cd .loom/worktrees/issue-{issue}`
3. Implement the fix, commit changes
4. Push and create PR:
   ```bash
   git push -u origin feature/issue-{issue}
   gh pr create --label loom:review-requested --body "Closes #{issue}"
   ```
5. Remove blocked label: `gh issue edit {issue} --remove-label loom:blocked`

### Investigation Tips
- Check the issue description for clarity - is it actionable?
- Review any curator comments for implementation guidance
- If log file is large, use: `cat {log_path} | ./.loom/scripts/strip-ansi.sh | tail -100`

</details>"#,
            issue = issue,
            log_path = self.log_path
        ));

        parts.join("\n")
    }
}

/// Gather diagnostics about a failed builder phase.
fn gather_builder_diagnostics(repo_root: &Path, issue: i64, worktree: &str) -> BuilderDiagnostics {
    let mut diag = BuilderDiagnostics {
        worktree_path: worktree.to_string(),
        issue,
        commits_ahead: "?".to_string(),
        commits_behind: "?".to_string(),
        branch: "unknown".to_string(),
        ..BuilderDiagnostics::default()
    };
    let wt = PathBuf::from(worktree);

    if wt.is_dir() {
        diag.worktree_exists = true;
        if let Ok(meta) = std::fs::metadata(&wt) {
            if let Ok(mtime) = meta.modified() {
                let dt: chrono::DateTime<chrono::Utc> = mtime.into();
                diag.worktree_mtime = dt.format("%Y-%m-%dT%H:%M:%SZ").to_string();
            }
        }

        let r = run_git(&wt, &["rev-parse", "--abbrev-ref", "HEAD"]);
        if r.success {
            diag.branch = r.trimmed_stdout().to_string();
        }

        // Detect the default branch name rather than assuming `main`.
        let r = run_git(&wt, &["symbolic-ref", "refs/remotes/origin/HEAD"]);
        let main_branch = if r.success {
            r.trimmed_stdout()
                .replace("refs/remotes/origin/", "")
                .trim()
                .to_string()
        } else {
            "main".to_string()
        };

        let ahead_range = format!("origin/{main_branch}..HEAD");
        let r = run_git(&wt, &["rev-list", "--count", &ahead_range]);
        if r.success {
            diag.commits_ahead = r.trimmed_stdout().to_string();
        }
        let behind_range = format!("HEAD..origin/{main_branch}");
        let r = run_git(&wt, &["rev-list", "--count", &behind_range]);
        if r.success {
            diag.commits_behind = r.trimmed_stdout().to_string();
        }
        diag.has_remote_tracking =
            run_git(&wt, &["rev-parse", "--abbrev-ref", "@{upstream}"]).success;
    }

    // Session log (first existing candidate wins).
    let session_name = format!("loom-builder-issue-{issue}");
    let candidates = [
        PathBuf::from(format!("/tmp/loom-{session_name}.out")),
        repo_root
            .join(".loom")
            .join("logs")
            .join(format!("{session_name}.log")),
    ];
    for path in &candidates {
        if path.is_file() {
            diag.log_path = path.display().to_string();
            if let Ok(bytes) = std::fs::read(path) {
                let text = String::from_utf8_lossy(&bytes);
                let lines: Vec<&str> = text.lines().collect();
                let tail = lines[lines.len().saturating_sub(15)..].join("\n");
                diag.log_tail = super::log_filter::strip_ansi(&tail);
            }
            break;
        }
    }

    // Issue labels.
    let issue_s = issue.to_string();
    let r = run_gh(
        &[
            "issue",
            "view",
            &issue_s,
            "--json",
            "labels",
            "--jq",
            ".labels[].name",
        ],
        repo_root,
        true,
    );
    if r.success && !r.trimmed_stdout().is_empty() {
        diag.issue_labels = r.trimmed_stdout().replace('\n', ", ");
    }

    // Uncommitted changes on the main checkout (the workflow-violation signal).
    let r = run_git(repo_root, &["status", "--porcelain"]);
    if r.success && !r.trimmed_stdout().is_empty() {
        diag.main_uncommitted = r
            .trimmed_stdout()
            .lines()
            .take(10)
            .collect::<Vec<_>>()
            .join("\n");
    }

    diag
}

/// Whether the builder exited because the Claude CLI hit an Anthropic rate
/// limit (its `/rate-limit-options` prompt shows up in the session log).
///
/// Distinct from GitHub API rate limits (handled elsewhere) — it only changes
/// the recovery PR's messaging, since the work itself completed.
fn is_rate_limited_builder_exit(repo_root: &Path, issue: i64) -> bool {
    let logs_dir = repo_root.join(".loom").join("logs");
    let Ok(entries) = std::fs::read_dir(&logs_dir) else {
        return false;
    };
    let prefix = format!("loom-builder-issue-{issue}");
    let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(std::ffi::OsStr::to_str)
                .is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".log"))
        })
        .filter_map(|p| {
            std::fs::metadata(&p)
                .and_then(|m| m.modified())
                .ok()
                .map(|t| (t, p))
        })
        .collect();
    candidates.sort_by_key(|(t, _)| *t);
    let Some((_, newest)) = candidates.last() else {
        return false;
    };
    std::fs::read(newest)
        .is_ok_and(|bytes| String::from_utf8_lossy(&bytes).contains("/rate-limit-options"))
}

/// Build a descriptive body for a recovery-created PR.
///
/// A pre-written `<worktree>/.loom/pr-body.md` (authored by the builder while
/// its context was fresh) wins — it produces far richer descriptions than the
/// diff-stat fallback below.
#[must_use]
pub fn build_recovery_pr_body(issue: i64, worktree: &str, rate_limited: bool) -> String {
    let pr_body_path = PathBuf::from(worktree).join(".loom").join("pr-body.md");
    if pr_body_path.is_file() {
        if let Ok(text) = std::fs::read_to_string(&pr_body_path) {
            let mut pr_body = text.trim().to_string();
            let has_close = [
                format!("Closes #{issue}"),
                format!("Fixes #{issue}"),
                format!("Resolves #{issue}"),
            ]
            .iter()
            .any(|kw| pr_body.contains(kw.as_str()));
            if !has_close {
                pr_body.push_str(&format!("\n\nCloses #{issue}"));
            }
            return pr_body;
        }
    }

    let wt = PathBuf::from(worktree);
    let mut lines: Vec<String> = vec![format!("Closes #{issue}"), String::new()];
    if rate_limited {
        lines.push(
            "> **Note:** Builder was rate-limited after completing work. \
             PR created via recovery path."
                .to_string(),
        );
    } else {
        lines.push(
            "> **Note:** This PR was created automatically via the builder \
             recovery path. The builder produced changes but exited before \
             creating a PR. Reviewers should examine the diff carefully."
                .to_string(),
        );
    }
    lines.push(String::new());

    let default_branch = resolve_default_branch(&wt);

    let stat_range = format!("{default_branch}...HEAD");
    let r = run_git(&wt, &["diff", "--stat", &stat_range]);
    if r.success && !r.trimmed_stdout().is_empty() {
        lines.push("## Changes".to_string());
        lines.push(String::new());
        lines.push("```".to_string());
        lines.push(r.trimmed_stdout().to_string());
        lines.push("```".to_string());
        lines.push(String::new());
    }

    let log_range = format!("{default_branch}..HEAD");
    let r = run_git(&wt, &["log", "--oneline", &log_range]);
    if r.success && !r.trimmed_stdout().is_empty() {
        lines.push("## Commits".to_string());
        lines.push(String::new());
        for commit in r.trimmed_stdout().lines() {
            lines.push(format!("- `{commit}`"));
        }
        lines.push(String::new());
    }

    lines.push("## Test plan".to_string());
    lines.push(String::new());
    if rate_limited {
        lines.push("- [ ] Verify changes match issue requirements".to_string());
        lines.push(
            "- [ ] Confirm tests pass (builder completed tests before rate limit)".to_string(),
        );
    } else {
        lines.push("- [ ] Review diff carefully (recovery-created PR)".to_string());
        lines.push("- [ ] Verify changes match issue requirements".to_string());
        lines.push("- [ ] Run tests locally if needed".to_string());
    }

    lines.join("\n")
}

/// The remote-tracking ref a Loom worktree branched from — `origin/HEAD`'s
/// target when it resolves, else `origin/main`.
fn resolve_default_branch(wt: &Path) -> String {
    let r = run_git(wt, &["rev-parse", "--abbrev-ref", "origin/HEAD"]);
    if r.success && !r.trimmed_stdout().is_empty() {
        r.trimmed_stdout().to_string()
    } else {
        "origin/main".to_string()
    }
}

/// Is `path` real implementation work, as opposed to a Loom runtime marker?
///
/// The path-level counterpart of [`substantive_status_lines`], plus the
/// `.no-changes-needed` signal (which means "the Builder deliberately decided
/// nothing was needed", never lost work).
#[must_use]
pub fn is_substantive_path(path: &str) -> bool {
    let p = path.trim();
    !p.is_empty()
        && p != ".no-changes-needed"
        && !p.ends_with(".loom-in-use")
        && !p.contains(".loom/")
}

/// Does this worktree's branch already carry pushed, substantive commits?
/// (#6074)
///
/// The Builder-recovery path used to treat "clean worktree, nothing unpushed"
/// as "there is nothing to recover" and fail. That is wrong for the exact
/// incident this check exists for: the Builder finished the work, committed
/// it, and **pushed successfully** (the App installation token still had
/// `Contents:write`) — only `gh pr create` failed, with `403 Resource not
/// accessible by integration`, because `Pull-requests:write` had not yet
/// propagated into that cached token. The sweep then failed with no PR, the
/// issue stayed ready, and the next dispatch rebuilt the identical work while
/// the pushed branch was left orphaned (2AMLogic/klayout-tools#851 rebuilt 3+
/// times).
///
/// When this returns true the recovery path proceeds instead of failing: the
/// push is a no-op and the PR is opened from what is already on the remote —
/// no rebuild, no orphaned branch.
fn pushed_branch_is_adoptable(wt: &Path) -> bool {
    let range = format!("{}..HEAD", resolve_default_branch(wt));
    let log = run_git(wt, &["log", "--oneline", &range]);
    if !log.success || log.trimmed_stdout().is_empty() {
        return false;
    }
    let files = run_git(wt, &["diff", "--name-only", &range]);
    files.success && files.stdout.lines().any(is_substantive_path)
}

/// Open the recovery PR through `.loom/scripts/create-pr.sh` when the repo has
/// it, falling back to a direct `gh pr create` when it does not (#6074).
///
/// The script adopts an already-open PR for the branch and escalates the
/// credential on an App permission-scope 403 (fresh installation-token mint,
/// then a personal token) — so this recovery path survives the same window the
/// Builder's own PR creation now survives, instead of re-failing on the very
/// 403 that sent it here.
fn create_recovery_pr(repo_root: &Path, branch: &str, title: &str, body: &str) -> GhResult {
    let script = repo_root.join(".loom").join("scripts").join("create-pr.sh");
    if script.is_file() {
        let spawned = Command::new("bash")
            .arg(&script)
            .args([
                "--head",
                branch,
                "--title",
                title,
                "--label",
                "loom:review-requested",
                "--body",
                body,
            ])
            .current_dir(repo_root)
            .output();
        // A spawn failure (no bash, unreadable script) is the only case that
        // falls through — a script that RAN and failed has already exhausted
        // the escalation ladder, and a bare `gh pr create` behind it would
        // only repeat the same rejection.
        if let Ok(out) = spawned {
            return GhResult::from_output(&out);
        }
    }
    run_gh(
        &[
            "pr",
            "create",
            "--head",
            branch,
            "--title",
            title,
            "--label",
            "loom:review-requested",
            "--body",
            body,
        ],
        repo_root,
        false,
    )
}

/// Extract the file path from a `git status --porcelain` line.
///
/// Porcelain v1 is `XY PATH`; the historical `line[3:]` slice was fragile when
/// whitespace varied. This skips the 2-char status, strips leading whitespace
/// and surrounding quotes, and for rename entries (`R  old -> new`) returns the
/// **destination** path (what `git add` needs).
#[must_use]
pub fn parse_porcelain_path(line: &str) -> String {
    if line.chars().count() < 3 {
        return line.trim().to_string();
    }
    // Byte-slicing is safe here: porcelain status codes are always 2 ASCII bytes.
    let raw = line[2..].trim_start().trim_matches('"').to_string();
    if line.starts_with('R') {
        if let Some((_, new_path)) = raw.rsplit_once(" -> ") {
            return new_path.trim_matches('"').to_string();
        }
    }
    raw
}

/// A conventional-commit-style PR title derived from an issue title.
///
/// An existing conventional prefix is normalised to lowercase; otherwise
/// `feat:` is prepended. An empty title falls back to
/// `feat: implement changes for issue #<N>`.
#[must_use]
pub fn conventional_pr_title(issue_title: &str, issue: i64) -> String {
    const PREFIXES: [&str; 7] = ["fix", "feat", "refactor", "docs", "test", "chore", "perf"];
    let title = issue_title.trim();
    if title.is_empty() {
        return format!("feat: implement changes for issue #{issue}");
    }
    let lower = title.to_lowercase();
    for prefix in PREFIXES {
        // `^(fix|feat|...)\s*:`
        if let Some(rest) = lower.strip_prefix(prefix) {
            let rest_trimmed = rest.trim_start();
            if let Some(after) = rest_trimmed.strip_prefix(':') {
                let consumed = title.len() - after.len();
                let tail = title[consumed..].trim();
                return format!("{prefix}: {tail}");
            }
        }
    }
    let mut chars = title.chars();
    let first = chars
        .next()
        .map(|c| c.to_lowercase().to_string())
        .unwrap_or_default();
    format!("feat: {first}{}", chars.as_str())
}

/// A recovery commit message: the issue title as a conventional commit when it
/// can be fetched, else a file-based summary, else a generic message.
fn derive_commit_message(repo_root: &Path, issue: i64, staged_files: &[String]) -> String {
    let issue_s = issue.to_string();
    let r = run_gh(
        &[
            "issue", "view", &issue_s, "--json", "title", "--jq", ".title",
        ],
        repo_root,
        false,
    );
    if r.success && !r.trimmed_stdout().is_empty() {
        return conventional_pr_title(r.trimmed_stdout(), issue);
    }
    if !staged_files.is_empty() {
        let names: Vec<&str> = staged_files
            .iter()
            .take(5)
            .map(|f| f.rsplit('/').next().unwrap_or(f))
            .collect();
        let files_desc = if staged_files.len() <= 3 {
            names.join(", ")
        } else {
            format!("{} and {} more", names[..3].join(", "), staged_files.len() - 3)
        };
        return format!("feat: update {files_desc} for issue #{issue}");
    }
    format!("feat: implement changes for issue #{issue}")
}

// --------------------------------------------------------------------------
// Phase validators
// --------------------------------------------------------------------------

/// Curator contract: the issue must carry `loom:curated`.
#[must_use]
pub fn validate_curator(repo_root: &Path, opts: &ValidateOpts) -> ValidationResult {
    let issue = opts.issue;
    let issue_s = issue.to_string();
    let r = run_gh(
        &[
            "issue",
            "view",
            &issue_s,
            "--json",
            "labels",
            "--jq",
            ".labels[].name",
        ],
        repo_root,
        true,
    );
    if !r.success {
        return ValidationResult::new(
            "curator",
            issue,
            ValidationStatus::Failed,
            "Could not fetch issue labels",
        );
    }
    if r.stdout.lines().any(|l| l.trim() == "loom:curated") {
        return ValidationResult::new(
            "curator",
            issue,
            ValidationStatus::Satisfied,
            "Issue has loom:curated label",
        );
    }
    if opts.check_only {
        return ValidationResult::new(
            "curator",
            issue,
            ValidationStatus::Failed,
            "Issue missing loom:curated label (check-only mode, no recovery attempted)",
        );
    }

    let r = run_gh(
        &[
            "issue",
            "edit",
            &issue_s,
            "--remove-label",
            "loom:curating",
            "--add-label",
            "loom:curated",
        ],
        repo_root,
        false,
    );
    if r.success {
        report_milestone(
            opts.task_id.as_deref(),
            repo_root,
            "recovery: applied loom:curated label",
        );
        return ValidationResult::new(
            "curator",
            issue,
            ValidationStatus::Recovered,
            "Applied loom:curated label",
        )
        .with_action("apply_label");
    }
    ValidationResult::new(
        "curator",
        issue,
        ValidationStatus::Failed,
        "Could not apply loom:curated label",
    )
}

/// Judge contract: the PR must carry `loom:pr` or `loom:changes-requested`.
#[must_use]
pub fn validate_judge(repo_root: &Path, opts: &ValidateOpts) -> ValidationResult {
    let issue = opts.issue;
    let Some(pr) = opts.pr_number else {
        return ValidationResult::new(
            "judge",
            issue,
            ValidationStatus::Failed,
            "PR number required for judge phase validation",
        );
    };
    let pr_s = pr.to_string();
    let probe = run_gh(
        &[
            "pr",
            "view",
            &pr_s,
            "--json",
            "labels",
            "--jq",
            ".labels[].name",
        ],
        repo_root,
        true,
    );
    if !probe.success {
        return ValidationResult::new(
            "judge",
            issue,
            ValidationStatus::Failed,
            "Could not fetch PR labels",
        );
    }
    let labels: Vec<&str> = probe.stdout.lines().map(str::trim).collect();

    if labels.contains(&"loom:pr") {
        return ValidationResult::new(
            "judge",
            issue,
            ValidationStatus::Satisfied,
            format!("PR #{pr} approved (loom:pr)"),
        );
    }
    if labels.contains(&"loom:changes-requested") {
        return ValidationResult::new(
            "judge",
            issue,
            ValidationStatus::Satisfied,
            format!("PR #{pr} has changes requested (loom:changes-requested)"),
        );
    }

    // Issue #1998: after Doctor applies fixes it removes
    // `loom:changes-requested` and adds `loom:review-requested`. Seeing that
    // here is an expected intermediate state, worth naming distinctly.
    let msg = if labels.contains(&"loom:review-requested") {
        format!(
            "PR #{pr} has loom:review-requested (Doctor applied fixes) but judge did not \
             produce outcome label yet"
        )
    } else {
        format!("Judge did not produce loom:pr or loom:changes-requested on PR #{pr}")
    };

    if !opts.check_only {
        mark_phase_failed(
            repo_root,
            issue,
            "judge",
            &format!("Judge phase did not produce a review decision on PR #{pr}."),
            "",
            opts.quiet,
        );
    }
    ValidationResult::new("judge", issue, ValidationStatus::Failed, msg)
}

/// Doctor contract: the PR must carry `loom:review-requested`.
#[must_use]
pub fn validate_doctor(repo_root: &Path, opts: &ValidateOpts) -> ValidationResult {
    let issue = opts.issue;
    let Some(pr) = opts.pr_number else {
        return ValidationResult::new(
            "doctor",
            issue,
            ValidationStatus::Failed,
            "PR number required for doctor phase validation",
        );
    };
    let pr_s = pr.to_string();
    let probe = run_gh(
        &[
            "pr",
            "view",
            &pr_s,
            "--json",
            "labels",
            "--jq",
            ".labels[].name",
        ],
        repo_root,
        true,
    );
    if !probe.success {
        return ValidationResult::new(
            "doctor",
            issue,
            ValidationStatus::Failed,
            "Could not fetch PR labels",
        );
    }
    if probe
        .stdout
        .lines()
        .any(|l| l.trim() == "loom:review-requested")
    {
        return ValidationResult::new(
            "doctor",
            issue,
            ValidationStatus::Satisfied,
            format!("PR #{pr} has loom:review-requested"),
        );
    }
    if !opts.check_only {
        mark_phase_failed(
            repo_root,
            issue,
            "doctor",
            &format!("Doctor phase did not apply loom:review-requested to PR #{pr}."),
            "",
            opts.quiet,
        );
    }
    ValidationResult::new(
        "doctor",
        issue,
        ValidationStatus::Failed,
        format!("Doctor did not re-request review on PR #{pr}"),
    )
}

/// Which worktree changes count as substantive (marker/infrastructure paths do
/// not).
#[must_use]
pub fn substantive_status_lines(status_output: &str) -> Vec<&str> {
    status_output
        .lines()
        .filter(|line| !line.trim_end().ends_with(".loom-in-use") && !line.contains(".loom/"))
        .collect()
}

/// Builder contract: an open PR carrying `loom:review-requested` must exist for
/// the issue; failing that, finish the mechanical git/PR steps from the
/// worktree's changes.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn validate_builder(repo_root: &Path, opts: &ValidateOpts) -> ValidationResult {
    let issue = opts.issue;
    let issue_s = issue.to_string();
    let quiet = opts.quiet;
    let fail = |msg: String| ValidationResult::new("builder", issue, ValidationStatus::Failed, msg);

    // Pre-check: workflow-violation detection.
    if let Some(worktree) = opts.worktree.as_deref() {
        if !PathBuf::from(worktree).is_dir() {
            let r = run_git(repo_root, &["status", "--porcelain"]);
            if r.success && !r.trimmed_stdout().is_empty() {
                let head: String = r.trimmed_stdout().chars().take(200).collect();
                super::log_warning(&format!(
                    "WORKFLOW VIOLATION: Builder appears to have worked on main instead of \
                     in worktree '{worktree}'. Uncommitted changes on main: {head}"
                ));
            }
        }
    }

    // Already-closed issue: a close with no PR means the Builder abandoned it.
    let state = run_gh(
        &[
            "issue", "view", &issue_s, "--json", "state", "--jq", ".state",
        ],
        repo_root,
        true,
    );
    if state.success && state.trimmed_stdout() == "CLOSED" {
        if let Some((pr, _)) = find_pr_for_issue(repo_root, issue, opts.pr_number) {
            return ValidationResult::new(
                "builder",
                issue,
                ValidationStatus::Satisfied,
                format!("Issue #{issue} is closed with associated PR #{pr}"),
            );
        }
        // Merged PRs do not appear in an open search.
        let head = format!("feature/issue-{issue}");
        let merged = run_gh(
            &[
                "pr",
                "list",
                "--head",
                &head,
                "--state",
                "merged",
                "--json",
                "number",
                "--jq",
                ".[0].number",
            ],
            repo_root,
            true,
        );
        if let Some(pr) = parse_pr_number(&merged.stdout) {
            return ValidationResult::new(
                "builder",
                issue,
                ValidationStatus::Satisfied,
                format!("Issue #{issue} is closed with merged PR #{pr}"),
            );
        }
        // Reopen so a legitimate feature request is not destroyed.
        if !opts.check_only {
            let _ = run_gh(&["issue", "reopen", &issue_s], repo_root, true);
            mark_phase_failed(
                repo_root,
                issue,
                "builder",
                "Issue was closed without an associated PR. Builder may have abandoned the \
                 issue instead of implementing it. Issue has been automatically reopened.",
                "",
                quiet,
            );
        }
        return fail(format!(
            "Issue #{issue} was closed without a PR — builder abandoned issue (reopened)"
        ));
    }

    let mut pr = find_pr_for_issue(repo_root, issue, opts.pr_number);

    // Checkpoint-aware retry: if no PR was found but the builder checkpoint says
    // one was just created, GitHub's eventual consistency may not have caught
    // up. Wait briefly and retry once (#2710).
    if pr.is_none() {
        if let Some(worktree) = opts.worktree.as_deref() {
            let wt = PathBuf::from(worktree);
            if wt.is_dir() {
                if let Some(cp) = super::checkpoints::read_checkpoint(&wt) {
                    if cp.stage == "pr_created" {
                        super::log_warning(&format!(
                            "No PR found for issue #{issue} but checkpoint indicates \
                             pr_created — retrying after 2s for API propagation"
                        ));
                        std::thread::sleep(std::time::Duration::from_secs(2));
                        pr = find_pr_for_issue(repo_root, issue, opts.pr_number);
                    }
                }
            }
        }
    }

    if let Some((pr_num, _found_by)) = pr {
        let pr_s = pr_num.to_string();
        if !opts.check_only {
            // The Builder may have solved the wrong issue, so this runs for
            // every PR — not just branch-name discoveries.
            ensure_pr_body_references_issue(repo_root, pr_num, issue, opts.task_id.as_deref());
            warn_generic_pr_title(repo_root, pr_num, opts.task_id.as_deref());
            recover_minimal_pr_body(repo_root, pr_num, issue, opts.task_id.as_deref());
        }

        if pr_labels(repo_root, pr_num)
            .iter()
            .any(|l| l == "loom:review-requested")
        {
            return ValidationResult::new(
                "builder",
                issue,
                ValidationStatus::Satisfied,
                format!("PR #{pr_num} exists with loom:review-requested"),
            );
        }
        if opts.check_only {
            return fail(format!(
                "PR #{pr_num} exists but missing loom:review-requested (check-only mode, no \
                 recovery attempted)"
            ));
        }

        let r = run_gh(
            &["pr", "edit", &pr_s, "--add-label", "loom:review-requested"],
            repo_root,
            false,
        );
        if r.success {
            report_milestone(
                opts.task_id.as_deref(),
                repo_root,
                &format!("recovery: added loom:review-requested to PR #{pr_num}"),
            );
            log_recovery_event(
                repo_root,
                issue,
                "add_label",
                "validation_failed",
                false,
                Some(pr_num),
                None,
            );
            return ValidationResult::new(
                "builder",
                issue,
                ValidationStatus::Recovered,
                format!("Added loom:review-requested to existing PR #{pr_num}"),
            )
            .with_action("add_label");
        }
    }

    // No PR found (or the label add failed).
    if opts.check_only {
        return fail(format!(
            "No PR found for issue #{issue} (check-only mode, no recovery attempted)"
        ));
    }

    let Some(worktree) = opts.worktree.as_deref() else {
        mark_phase_failed(
            repo_root,
            issue,
            "builder",
            &format!(
                "Builder did not create a PR. Searched for: branch 'feature/issue-{issue}' \
                 and 'Closes/Fixes/Resolves #{issue}' in PR body. No worktree available."
            ),
            "",
            quiet,
        );
        return fail(format!(
            "No PR found (searched by branch 'feature/issue-{issue}' and keywords) and no \
             worktree path provided"
        ));
    };

    let wt = PathBuf::from(worktree);
    if !wt.is_dir() {
        let diag = gather_builder_diagnostics(repo_root, issue, worktree);
        mark_phase_failed(
            repo_root,
            issue,
            "builder",
            "Builder did not create a PR and worktree path does not exist.",
            &diag.to_markdown(),
            quiet,
        );
        return fail(format!("Worktree path does not exist: {worktree}"));
    }

    let status = run_git(&wt, &["status", "--porcelain"]);
    if !status.success {
        mark_phase_failed(
            repo_root,
            issue,
            "builder",
            "Builder did not create a PR and worktree is not a valid git directory.",
            "",
            quiet,
        );
        return fail("Could not check worktree status".to_string());
    }
    let status_output = status.trimmed_stdout().to_string();
    let mut adopted_pushed_branch = false;

    if status_output.is_empty() {
        // No uncommitted changes — are there unpushed commits?
        let unpushed = run_git(&wt, &["log", "--oneline", "@{upstream}..HEAD"]);
        let has_unpushed = unpushed.success && !unpushed.trimmed_stdout().is_empty();
        if !has_unpushed && pushed_branch_is_adoptable(&wt) {
            // Everything is already committed AND pushed; only the PR is
            // missing (the #6074 App-permission window). Adopt the branch —
            // the push below is a no-op and the PR is opened from it — rather
            // than failing, which would strand the branch and make the next
            // dispatch rebuild the identical work.
            adopted_pushed_branch = true;
        } else if !has_unpushed {
            let diag = gather_builder_diagnostics(repo_root, issue, worktree);
            mark_phase_failed(
                repo_root,
                issue,
                "builder",
                "Builder did not create a PR. Worktree had no uncommitted or unpushed changes.",
                &diag.to_markdown(),
                quiet,
            );
            return fail("No PR found and no changes in worktree.".to_string());
        }

        // There ARE unpushed commits. If they only add the no-changes-needed
        // marker, this is a deliberate "no changes needed", not lost work.
        let committed = run_git(&wt, &["diff", "--name-only", "@{upstream}..HEAD"]);
        let files: Vec<&str> = if committed.success {
            committed
                .stdout
                .lines()
                .map(str::trim)
                .filter(|f| !f.is_empty())
                .collect()
        } else {
            Vec::new()
        };
        if files == [".no-changes-needed"] {
            let diag = gather_builder_diagnostics(repo_root, issue, worktree);
            mark_phase_failed(
                repo_root,
                issue,
                "builder",
                "Builder committed only the .no-changes-needed marker — treating as \
                 'no changes needed', skipping recovery PR.",
                &diag.to_markdown(),
                quiet,
            );
            return fail(
                "No substantive changes to recover (only .no-changes-needed committed)."
                    .to_string(),
            );
        }
    }

    let substantive: Vec<String> = substantive_status_lines(&status_output)
        .into_iter()
        .map(str::to_string)
        .collect();
    if !status_output.is_empty() && substantive.is_empty() {
        let diag = gather_builder_diagnostics(repo_root, issue, worktree);
        mark_phase_failed(
            repo_root,
            issue,
            "builder",
            "Builder did not produce substantive changes. Only marker/infrastructure files \
             were found in the worktree.",
            &diag.to_markdown(),
            quiet,
        );
        return fail("No substantive changes to recover (only marker files found).".to_string());
    }

    // Mechanical recovery: stage, commit, push, create PR.
    let branch = format!("feature/issue-{issue}");

    if !status_output.is_empty() {
        let files_to_stage: Vec<String> = substantive
            .iter()
            .map(|l| parse_porcelain_path(l))
            .filter(|p| !p.is_empty())
            .collect();
        if !files_to_stage.is_empty() {
            let mut args: Vec<&str> = vec!["add", "--"];
            args.extend(files_to_stage.iter().map(String::as_str));
            let r = run_git(&wt, &args);
            if !r.success {
                let head: String = r.stderr.trim().chars().take(200).collect();
                let diag = gather_builder_diagnostics(repo_root, issue, worktree);
                mark_phase_failed(
                    repo_root,
                    issue,
                    "builder",
                    &format!("Recovery failed: git add failed: {head}"),
                    &diag.to_markdown(),
                    quiet,
                );
                return fail("Recovery failed: could not stage changes.".to_string());
            }

            let commit_msg = derive_commit_message(repo_root, issue, &files_to_stage);
            let r = run_git(&wt, &["commit", "-m", &commit_msg]);
            if !r.success {
                let head: String = r.stderr.trim().chars().take(200).collect();
                let diag = gather_builder_diagnostics(repo_root, issue, worktree);
                mark_phase_failed(
                    repo_root,
                    issue,
                    "builder",
                    &format!("Recovery failed: git commit failed: {head}"),
                    &diag.to_markdown(),
                    quiet,
                );
                return fail("Recovery failed: could not commit changes.".to_string());
            }
        }
    }

    let r = run_git(&wt, &["push", "-u", "origin", &branch]);
    if !r.success {
        let head: String = r.stderr.trim().chars().take(200).collect();
        let diag = gather_builder_diagnostics(repo_root, issue, worktree);
        mark_phase_failed(
            repo_root,
            issue,
            "builder",
            &format!("Recovery failed: git push failed: {head}"),
            &diag.to_markdown(),
            quiet,
        );
        return fail("Recovery failed: could not push branch.".to_string());
    }

    let rate_limited = is_rate_limited_builder_exit(repo_root, issue);
    let title_probe = run_gh(
        &[
            "issue", "view", &issue_s, "--json", "title", "--jq", ".title",
        ],
        repo_root,
        true,
    );
    let raw_title = if title_probe.success {
        title_probe.trimmed_stdout()
    } else {
        ""
    };
    let pr_title = conventional_pr_title(raw_title, issue);
    let pr_body = build_recovery_pr_body(issue, worktree, rate_limited);

    let created = create_recovery_pr(repo_root, &branch, &pr_title, &pr_body);
    if !created.success {
        let head: String = created.stderr.trim().chars().take(200).collect();
        let diag = gather_builder_diagnostics(repo_root, issue, worktree);
        mark_phase_failed(
            repo_root,
            issue,
            "builder",
            &format!("Recovery failed: gh pr create failed: {head}"),
            &diag.to_markdown(),
            quiet,
        );
        return fail("Recovery failed: could not create PR.".to_string());
    }

    // `gh pr create` prints the PR URL; the number is its last path segment.
    let url = created.trimmed_stdout();
    let recovered_pr = parse_pr_number(url.rsplit('/').next().unwrap_or(url));

    let recovery_reason = if rate_limited {
        "rate_limited"
    } else {
        "validation_failed"
    };
    report_milestone(
        opts.task_id.as_deref(),
        repo_root,
        &format!(
            "recovery: created PR from {} worktree changes for issue #{issue}",
            if rate_limited {
                "rate-limited"
            } else {
                "uncommitted"
            }
        ),
    );
    let action = if adopted_pushed_branch {
        "adopt_pushed_branch"
    } else {
        "commit_and_pr"
    };
    log_recovery_event(
        repo_root,
        issue,
        action,
        recovery_reason,
        !status_output.is_empty(),
        recovered_pr,
        if rate_limited {
            Some("rate_limited")
        } else {
            None
        },
    );
    let message = if adopted_pushed_branch {
        "Recovered: opened a PR from the branch the builder had already pushed (no rebuild)"
            .to_string()
    } else {
        format!(
            "Recovered: staged, committed, pushed, and created PR from worktree changes{}",
            if rate_limited {
                " (builder was rate-limited)"
            } else {
                ""
            }
        )
    };
    ValidationResult::new("builder", issue, ValidationStatus::Recovered, message)
        .with_action(action)
}

/// Validate a sweep phase contract against an explicit repo root.
#[must_use]
pub fn validate_phase(repo_root: &Path, opts: &ValidateOpts) -> ValidationResult {
    match opts.phase.as_str() {
        "curator" => validate_curator(repo_root, opts),
        "builder" => validate_builder(repo_root, opts),
        "judge" => validate_judge(repo_root, opts),
        "doctor" => validate_doctor(repo_root, opts),
        other => ValidationResult::new(
            other,
            opts.issue,
            ValidationStatus::Failed,
            format!("Invalid phase '{other}'. Must be one of: {}", VALID_PHASES.join(", ")),
        ),
    }
}

// --------------------------------------------------------------------------
// CLI
// --------------------------------------------------------------------------

/// Run the `validate-phase` CLI, returning the process exit code (`0`
/// contract satisfied, `1` failed, `2` invalid arguments).
#[must_use]
pub fn run(cwd: &Path, opts: &ValidateOpts) -> i32 {
    if !VALID_PHASES.contains(&opts.phase.as_str()) {
        eprintln!(
            "loom-daemon validate-phase: error: argument phase: invalid choice: '{}' \
             (choose from {})",
            opts.phase,
            VALID_PHASES
                .iter()
                .map(|p| format!("'{p}'"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        return 2;
    }
    // The Python resolved the repo root by walking up from cwd, falling back to
    // cwd itself; keep that (a `gh`/`git` call from the wrong root simply fails
    // and surfaces as a contract failure, never a panic).
    let repo_root = super::find_repo_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
    let result = validate_phase(&repo_root, opts);

    if opts.json_output {
        println!(
            "{}",
            serde_json::to_string(&result.to_value())
                .unwrap_or_else(|_| result.to_value().to_string())
        );
    } else {
        println!(
            "{} {} phase contract {}: {}",
            result.status.glyph(),
            result.phase,
            result.status.as_str(),
            result.message
        );
    }
    i32::from(!result.satisfied())
}

/// Re-export for callers that want the raw `gh` runner shape.
pub type GhOutcome = GhResult;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn opts(phase: &str, issue: i64) -> ValidateOpts {
        ValidateOpts {
            phase: phase.to_string(),
            issue,
            check_only: true,
            ..ValidateOpts::default()
        }
    }

    // ===== ValidationResult shape =====

    #[test]
    fn result_json_matches_the_bash_output_shape() {
        let r = ValidationResult::new("builder", 42, ValidationStatus::Recovered, "did the thing")
            .with_action("add_label");
        assert_eq!(
            r.to_value(),
            json!({
                "phase": "builder",
                "issue": 42,
                "status": "recovered",
                "message": "did the thing",
                "recovery_action": "add_label",
            })
        );
        assert!(r.satisfied());
    }

    #[test]
    fn satisfied_covers_both_satisfied_and_recovered() {
        for status in [ValidationStatus::Satisfied, ValidationStatus::Recovered] {
            assert!(ValidationResult::new("p", 1, status, "m").satisfied());
        }
        assert!(!ValidationResult::new("p", 1, ValidationStatus::Failed, "m").satisfied());
    }

    #[test]
    fn status_glyphs_match_the_python_cli() {
        assert_eq!(ValidationStatus::Satisfied.glyph(), "\u{2713}");
        assert_eq!(ValidationStatus::Recovered.glyph(), "\u{27f3}");
        assert_eq!(ValidationStatus::Failed.glyph(), "\u{2717}");
    }

    // ===== phase dispatch / argument validation =====

    #[test]
    fn an_invalid_phase_fails_with_the_valid_list() {
        let dir = tempdir().unwrap();
        let r = validate_phase(dir.path(), &opts("frobnicate", 1));
        assert_eq!(r.status, ValidationStatus::Failed);
        assert!(r.message.contains("curator, builder, judge, doctor"));
        // …and the CLI turns that into exit 2 before touching the forge.
        assert_eq!(run(dir.path(), &opts("frobnicate", 1)), 2);
    }

    #[test]
    fn judge_and_doctor_require_a_pr_number() {
        let dir = tempdir().unwrap();
        for phase in ["judge", "doctor"] {
            let r = validate_phase(dir.path(), &opts(phase, 1));
            assert_eq!(r.status, ValidationStatus::Failed);
            assert!(r.message.contains("PR number required"), "{phase}: {}", r.message);
            assert_eq!(r.recovery_action, "none");
        }
    }

    // ===== closing-reference parsing =====

    #[test]
    fn closing_references_are_found_case_insensitively() {
        let refs = closing_references("Closes #12\nfixes #34\nRESOLVES #56");
        let nums: Vec<i64> = refs.iter().map(|(_, n)| *n).collect();
        assert!(nums.contains(&12));
        assert!(nums.contains(&34));
        assert!(nums.contains(&56));
    }

    #[test]
    fn closing_references_require_a_space_and_a_hash() {
        assert!(closing_references("Closes#12").is_empty());
        assert!(closing_references("Closes 12").is_empty());
        assert!(closing_references("Closes #").is_empty());
        // Prose mentioning the word alone must not register.
        assert!(closing_references("this closes the loop").is_empty());
    }

    #[test]
    fn closing_references_preserve_the_original_keyword_casing() {
        let refs = closing_references("FIXES #7");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].0, "FIXES");
        assert_eq!(refs[0].1, 7);
    }

    // ===== generic-title detection =====

    #[test]
    fn generic_pr_titles_are_detected() {
        assert!(generic_title_reason("feat: implement changes for issue #42").is_some());
        assert!(generic_title_reason("Address issue #42").is_some());
        assert!(generic_title_reason("implement feature from issue 42").is_some());
        assert!(generic_title_reason("Issue #42").is_some());
        assert!(generic_title_reason("issue 42").is_some());
    }

    #[test]
    fn meaningful_pr_titles_are_not_flagged() {
        assert!(generic_title_reason("fix: strip ANSI escapes before dedup").is_none());
        assert!(generic_title_reason("feat(daemon): port the script-helper family").is_none());
        assert!(generic_title_reason("").is_none());
        // "issue" as a real word must not trip the bare-issue pattern.
        assert!(generic_title_reason("issue tracker sync is flaky").is_none());
    }

    // ===== minimal-body detection =====

    #[test]
    fn a_bare_closes_line_is_a_minimal_body() {
        assert!(is_minimal_pr_body("Closes #42"));
        assert!(is_minimal_pr_body(""));
        assert!(is_minimal_pr_body("Closes #42\n\nFixes #43\n"));
    }

    #[test]
    fn a_body_with_a_summary_section_is_never_minimal() {
        assert!(!is_minimal_pr_body("## Summary\n\nshort"));
    }

    #[test]
    fn a_long_body_is_not_minimal() {
        let body = format!("Closes #42\n\n{}", "x".repeat(100));
        assert!(!is_minimal_pr_body(&body));
    }

    // ===== porcelain parsing =====

    #[test]
    fn porcelain_paths_survive_whitespace_quotes_and_renames() {
        assert_eq!(parse_porcelain_path(" M src/main.rs"), "src/main.rs");
        assert_eq!(parse_porcelain_path("?? src/new.rs"), "src/new.rs");
        assert_eq!(parse_porcelain_path("A  src/a.rs"), "src/a.rs");
        assert_eq!(parse_porcelain_path(r#" M "src/with space.rs""#), "src/with space.rs");
        assert_eq!(parse_porcelain_path("R  old.rs -> new.rs"), "new.rs");
        assert_eq!(parse_porcelain_path("x"), "x");
    }

    // ===== marker-file filtering =====

    #[test]
    fn marker_and_infrastructure_paths_are_not_substantive() {
        let status = " M .loom-in-use\n?? .loom/pr-body.md\n M src/main.rs";
        assert_eq!(substantive_status_lines(status), vec![" M src/main.rs"]);

        let only_markers = " M .loom-in-use\n?? .loom/stats/x.jsonl";
        assert!(substantive_status_lines(only_markers).is_empty());
    }

    // ===== conventional titles =====

    #[test]
    fn conventional_titles_normalize_an_existing_prefix() {
        assert_eq!(conventional_pr_title("Fix: broken thing", 1), "fix: broken thing");
        assert_eq!(conventional_pr_title("FEAT : spaced colon", 1), "feat: spaced colon");
        assert_eq!(conventional_pr_title("refactor: tidy up", 1), "refactor: tidy up");
    }

    #[test]
    fn conventional_titles_add_a_prefix_when_absent() {
        assert_eq!(
            conventional_pr_title("Strip ANSI escapes", 1),
            "feat: Strip ANSI escapes".to_lowercase_first()
        );
    }

    #[test]
    fn conventional_titles_fall_back_for_an_empty_title() {
        assert_eq!(conventional_pr_title("", 42), "feat: implement changes for issue #42");
        assert_eq!(conventional_pr_title("   ", 42), "feat: implement changes for issue #42");
    }

    // ===== recovery PR body =====

    #[test]
    fn a_prewritten_pr_body_wins_and_gains_a_closes_line() {
        let dir = tempdir().unwrap();
        let loom = dir.path().join(".loom");
        std::fs::create_dir_all(&loom).unwrap();
        std::fs::write(loom.join("pr-body.md"), "## Summary\n\nReal work.\n").unwrap();

        let body = build_recovery_pr_body(42, &dir.path().display().to_string(), false);
        assert!(body.starts_with("## Summary"));
        assert!(body.ends_with("Closes #42"));
    }

    #[test]
    fn a_prewritten_pr_body_with_a_close_keyword_is_left_alone() {
        let dir = tempdir().unwrap();
        let loom = dir.path().join(".loom");
        std::fs::create_dir_all(&loom).unwrap();
        std::fs::write(loom.join("pr-body.md"), "Fixes #42\n\nReal work.\n").unwrap();
        let body = build_recovery_pr_body(42, &dir.path().display().to_string(), false);
        assert_eq!(body.matches("#42").count(), 1);
    }

    #[test]
    fn the_fallback_pr_body_names_the_recovery_path_and_test_plan() {
        let dir = tempdir().unwrap();
        let body = build_recovery_pr_body(42, &dir.path().display().to_string(), false);
        assert!(body.starts_with("Closes #42"));
        assert!(body.contains("builder recovery path"));
        assert!(body.contains("## Test plan"));

        let limited = build_recovery_pr_body(42, &dir.path().display().to_string(), true);
        assert!(limited.contains("rate-limited"));
        assert!(limited.contains("builder completed tests before rate limit"));
    }

    // ===== diagnostics markdown =====

    #[test]
    fn diagnostics_markdown_covers_the_missing_worktree_case() {
        let diag = BuilderDiagnostics {
            worktree_path: "/tmp/wt".into(),
            issue: 42,
            ..BuilderDiagnostics::default()
        };
        let md = diag.to_markdown();
        assert!(md.contains("<details>"));
        assert!(md.contains("`/tmp/wt` does not exist"));
        assert!(md.contains("Worktree was never created"));
        assert!(md.contains("worktree.sh 42"));
        assert!(md.trim_end().ends_with("</details>"));
    }

    #[test]
    fn diagnostics_markdown_flags_zero_commits_and_dirty_main() {
        let diag = BuilderDiagnostics {
            worktree_path: "/tmp/wt".into(),
            worktree_exists: true,
            branch: "feature/issue-42".into(),
            commits_ahead: "0".into(),
            commits_behind: "3".into(),
            main_uncommitted: " M src/main.rs".into(),
            worktree_mtime: "2026-07-30T00:00:00Z".into(),
            issue: 42,
            ..BuilderDiagnostics::default()
        };
        let md = diag.to_markdown();
        assert!(md.contains("### Previous Attempt"));
        assert!(md.contains("**Commits ahead of main**: 0"));
        assert!(md.contains("Builder exited without making any commits"));
        assert!(md.contains("WARNING: Uncommitted changes detected on main"));
        assert!(md.contains("not configured (branch never pushed)"));
    }

    // ===== recovery-event log =====

    #[test]
    fn recovery_events_append_and_stay_bounded() {
        let dir = tempdir().unwrap();
        let file = dir
            .path()
            .join(".loom")
            .join("metrics")
            .join("recovery-events.json");

        log_recovery_event(dir.path(), 1, "add_label", "validation_failed", false, Some(9), None);
        let events = super::super::read_json_file(&file).unwrap();
        let arr = events.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["issue"], json!(1));
        assert_eq!(arr[0]["pr_number"], json!(9));
        assert!(arr[0].get("builder_exit_reason").is_none());

        log_recovery_event(
            dir.path(),
            2,
            "commit_and_pr",
            "rate_limited",
            true,
            None,
            Some("rate_limited"),
        );
        let arr = super::super::read_json_file(&file)
            .unwrap()
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[1]["worktree_had_changes"], json!(true));
        assert_eq!(arr[1]["pr_number"], Value::Null);
        assert_eq!(arr[1]["builder_exit_reason"], json!("rate_limited"));
    }

    #[test]
    fn recovery_events_truncate_to_the_cap() {
        let dir = tempdir().unwrap();
        let metrics = dir.path().join(".loom").join("metrics");
        std::fs::create_dir_all(&metrics).unwrap();
        let seed: Vec<Value> = (0..MAX_RECOVERY_EVENTS + 5)
            .map(|i| json!({"issue": i}))
            .collect();
        std::fs::write(metrics.join("recovery-events.json"), Value::Array(seed).to_string())
            .unwrap();

        log_recovery_event(dir.path(), 99, "add_label", "x", false, None, None);
        let arr = super::super::read_json_file(&metrics.join("recovery-events.json"))
            .unwrap()
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(arr.len(), MAX_RECOVERY_EVENTS);
        assert_eq!(arr[arr.len() - 1]["issue"], json!(99));
    }

    #[test]
    fn a_corrupt_recovery_log_is_replaced_not_lost_to_a_panic() {
        let dir = tempdir().unwrap();
        let metrics = dir.path().join(".loom").join("metrics");
        std::fs::create_dir_all(&metrics).unwrap();
        std::fs::write(metrics.join("recovery-events.json"), "not json").unwrap();
        log_recovery_event(dir.path(), 1, "add_label", "x", false, None, None);
        let arr = super::super::read_json_file(&metrics.join("recovery-events.json"))
            .unwrap()
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(arr.len(), 1);
    }

    // ===== quiet mode (#2609) =====

    /// `quiet` must be a total no-op — no label edit, no comment. Verified by
    /// pointing it at a directory with no `gh` reachable state and asserting it
    /// returns without side effects (a non-quiet call would shell out).
    #[test]
    fn quiet_mark_phase_failed_is_a_total_no_op() {
        let dir = tempdir().unwrap();
        mark_phase_failed(dir.path(), 1, "builder", "reason", "diag", true);
        // No metrics/log artifacts may be created by a quiet call.
        assert!(!dir.path().join(".loom").exists());
    }

    // ===== PR number parsing =====

    #[test]
    fn pr_numbers_parse_and_soft_fail() {
        assert_eq!(parse_pr_number("123\n"), Some(123));
        assert_eq!(parse_pr_number("  456  "), Some(456));
        assert_eq!(parse_pr_number("null"), None);
        assert_eq!(parse_pr_number(""), None);
        assert_eq!(parse_pr_number("not-a-number"), None);
    }

    #[test]
    fn marker_paths_are_never_substantive_work() {
        assert!(is_substantive_path("src/lib.rs"));
        assert!(is_substantive_path("  defaults/scripts/create-pr.sh  "));
        assert!(!is_substantive_path(".no-changes-needed"));
        assert!(!is_substantive_path(".loom-in-use"));
        assert!(!is_substantive_path(".loom/sweep-checkpoint/issue-1.json"));
        assert!(!is_substantive_path(""));
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git must be available");
        assert!(out.status.success(), "git {args:?} failed");
    }

    /// A repo whose branch carries `commit_files` on top of a faked
    /// `origin/main`, i.e. exactly the post-push state of a Builder whose
    /// `gh pr create` 403'd (#6074).
    fn repo_with_pushed_branch(files: &[&str]) -> tempfile::TempDir {
        let dir = tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q"]);
        git(p, &["config", "user.name", "t"]);
        git(p, &["config", "user.email", "t@t"]);
        std::fs::write(p.join("README.md"), "base").unwrap();
        git(p, &["add", "README.md"]);
        git(p, &["commit", "-qm", "base"]);
        // Stand in for the remote-tracking ref `resolve_default_branch` finds.
        git(p, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        git(p, &["checkout", "-q", "-b", "feature/issue-1"]);
        for f in files {
            std::fs::write(p.join(f), "work").unwrap();
            git(p, &["add", f]);
        }
        if !files.is_empty() {
            git(p, &["commit", "-qm", "work"]);
        }
        dir
    }

    #[test]
    fn a_pushed_branch_with_real_commits_is_adoptable() {
        let dir = repo_with_pushed_branch(&["src.rs"]);
        assert!(pushed_branch_is_adoptable(dir.path()));
    }

    #[test]
    fn a_branch_with_no_commits_ahead_of_the_base_is_not_adoptable() {
        let dir = repo_with_pushed_branch(&[]);
        assert!(!pushed_branch_is_adoptable(dir.path()));
    }

    #[test]
    fn a_pushed_branch_carrying_only_the_no_changes_marker_is_not_adoptable() {
        let dir = repo_with_pushed_branch(&[".no-changes-needed"]);
        assert!(!pushed_branch_is_adoptable(dir.path()));
    }

    #[test]
    fn the_recovery_pr_is_opened_through_create_pr_sh_when_the_repo_has_it() {
        let dir = tempdir().unwrap();
        let scripts = dir.path().join(".loom").join("scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        let script = scripts.join("create-pr.sh");
        std::fs::write(
            &script,
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" > \"$(dirname \"$0\")/argv\"\n\
             echo https://github.test/o/r/pull/9\n",
        )
        .unwrap();

        let r = create_recovery_pr(dir.path(), "feature/issue-1", "fix: t", "Closes #1");
        assert!(r.success);
        assert_eq!(r.trimmed_stdout(), "https://github.test/o/r/pull/9");
        let argv = std::fs::read_to_string(scripts.join("argv")).unwrap();
        assert!(argv.contains("--head feature/issue-1"), "argv: {argv}");
        assert!(argv.contains("--label loom:review-requested"), "argv: {argv}");
    }

    /// Test-only helper mirroring the Python's "lowercase the first character"
    /// behavior, so the expectation in `conventional_titles_add_a_prefix_when_absent`
    /// reads as the rule rather than a hardcoded string.
    #[cfg(test)]
    trait LowercaseFirst {
        fn to_lowercase_first(&self) -> String;
    }

    #[cfg(test)]
    impl LowercaseFirst for str {
        fn to_lowercase_first(&self) -> String {
            // "feat: Strip ANSI escapes" -> "feat: strip ANSI escapes"
            match self.split_once(": ") {
                Some((prefix, rest)) => {
                    let mut chars = rest.chars();
                    let first = chars
                        .next()
                        .map(|c| c.to_lowercase().to_string())
                        .unwrap_or_default();
                    format!("{prefix}: {first}{}", chars.as_str())
                }
                None => self.to_string(),
            }
        }
    }
}
