//! Child-log parsing and crash/exhaustion classification: everything that
//! turns a dispatched sweep's log tail / exit code into a verdict.

use super::*;

/// Issue #3802: fallback `token_name` recorded when `spawn-claude.sh`'s
/// account selection cannot be captured (e.g. the `LOOM_SPAWN_NO_EXPORT`
/// bypass path, where the caller pre-exported `CLAUDE_CODE_OAUTH_TOKEN` and no
/// account is selected at all — nothing to report, not a bug).
pub const UNKNOWN_TOKEN_NAME: &str = "unknown";

/// Issue #3802: the marker substring `spawn-claude.sh` logs immediately after
/// selecting an OAuth account — `spawn-claude: using OAuth account '<name>'
/// (mode=<mode>)` (see `defaults/scripts/spawn-claude.sh`). The daemon already
/// captures the child's stderr into the per-sweep log, so `spawn_child` reads
/// the log back and extracts `<name>` from the text following this marker. The
/// account name itself carries no ANSI colour codes (only the timestamp prefix
/// does), so a plain substring scan is sufficient — no ANSI stripping needed.
pub(crate) const TOKEN_NAME_LOG_MARKER: &str = "using OAuth account '";
pub(crate) const ACCOUNT_LOG_MARKER: &str = "# LOOM_ACCOUNT name=";
pub(crate) const RUNTIME_LOG_MARKER: &str = "# LOOM_RUNTIME_RESOLVED runtime=";
pub(crate) const CLI_START_MARKER: &str = "# LOOM_CLI_START";
pub(crate) const TERMINAL_RESULT_MARKER: &str = "# LOOM_TERMINAL_RESULT ";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TerminalResult {
    pub(crate) provider: AccountProvider,
    pub(crate) account: String,
    pub(crate) category: TerminalClassification,
    pub(crate) exit_code: i32,
}

/// Parse the adapter's strict v1 record from only the current dispatch region.
/// Unknown fields, duplicate records, malformed identities, and unknown
/// categories all fail closed: no account health is mutated.
pub(crate) fn parse_terminal_result_after(
    contents: &str,
    header_anchor: &str,
) -> Option<TerminalResult> {
    let region = &contents[contents.rfind(header_anchor)?..];
    let records: Vec<_> = region
        .lines()
        .filter(|line| line.starts_with(TERMINAL_RESULT_MARKER))
        .collect();
    if records.len() != 1 {
        return None;
    }
    let fields: HashMap<_, _> = records[0][TERMINAL_RESULT_MARKER.len()..]
        .split_ascii_whitespace()
        .filter_map(|field| field.split_once('='))
        .collect();
    if fields.len() != 5 || fields.get("v") != Some(&"1") {
        return None;
    }
    let provider = match *fields.get("provider")? {
        "claude" => AccountProvider::Claude,
        "codex" => AccountProvider::Codex,
        _ => return None,
    };
    let account = fields.get("account")?.to_string();
    if account == UNKNOWN_TOKEN_NAME
        || account.is_empty()
        || !account
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return None;
    }
    Some(TerminalResult {
        provider,
        account,
        category: fields.get("category")?.parse().ok()?,
        exit_code: fields.get("exit_code")?.parse().ok()?,
    })
}

/// Issue #3802: how long `spawn_child` polls the per-sweep log for the
/// `TOKEN_NAME_LOG_MARKER` line before giving up and recording
/// `UNKNOWN_TOKEN_NAME`. `spawn-claude.sh` selects and logs the account early
/// (well before it `exec`s `claude`), so the line normally appears within a
/// second. The poll also short-circuits the moment the child exits without
/// having logged a selection (keeps no-selection fixtures fast), so this full
/// window is only ever waited out in the pathological "child alive but never
/// logged" case — capture then degrades gracefully to `UNKNOWN_TOKEN_NAME`
/// and never blocks or fails dispatch.
pub(crate) const TOKEN_NAME_CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);

/// Issue #3802: poll cadence for the `TOKEN_NAME_LOG_MARKER` scan.
pub(crate) const TOKEN_NAME_CAPTURE_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// The dispatch-header marker `spawn_child` writes before each spawn. Reused by
/// the watchdog's progress probe to anchor its scan to the current dispatch.
pub(crate) const DISPATCH_HEADER_MARKER: &str = "==== loom-daemon dispatch:";

/// One piece of evidence that a *live* process is still using an issue
/// worktree, gathered by [`SweepRegistry::worktree_in_use`] (Issue #4449).
///
/// Any non-empty set of evidence vetoes the mid-build watchdog's destructive
/// `git reset --hard` / `git clean -fd` path. Each variant carries enough detail
/// to name the holder in the daemon log so a refusal is actionable rather than
/// mysterious.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeUseEvidence {
    /// A `.loom-in-use` marker file names an owning session. Written by flows
    /// that enter a worktree (including manual, non-daemon sessions), so this
    /// is the one signal an operator can set by hand to fence off a worktree.
    InUseMarker { task_id: String, pid: String },
    /// A claim-lock (`.loom/locks/issue-<N>/owner.json`) whose recorded owner
    /// PID is still alive. A dead owner's lock is *not* evidence (the reaper /
    /// `reconstruct` prune those), so this only fires for a genuinely live
    /// claim the in-memory registry does not represent as active.
    LiveClaimLock { pid: u32, sweep_id: String },
    /// One or more live processes have their current working directory inside
    /// the worktree — an operator's shell, a manual role session, or an
    /// orphaned grandchild still writing files.
    LiveProcesses(Vec<u32>),
    /// A git operation is mid-flight (`index.lock` present) — precisely the
    /// `git commit` window in which #4449 lost an uncommitted fix.
    GitOperationInFlight(PathBuf),
}

impl std::fmt::Display for WorktreeUseEvidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InUseMarker { task_id, pid } => {
                write!(f, ".loom-in-use marker (task: {task_id}, pid: {pid})")
            }
            Self::LiveClaimLock { pid, sweep_id } => {
                write!(f, "live claim-lock owner (pid {pid}, sweep {sweep_id})")
            }
            Self::LiveProcesses(pids) => {
                write!(f, "live process(es) with cwd inside the worktree: {pids:?}")
            }
            Self::GitOperationInFlight(path) => {
                write!(f, "git operation in flight ({} exists)", path.display())
            }
        }
    }
}

/// Render a set of [`WorktreeUseEvidence`] as a single `; `-joined log fragment.
#[must_use]
pub fn describe_worktree_use(evidence: &[WorktreeUseEvidence]) -> String {
    evidence
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

/// Truncate `text` to at most `max` lines, appending a `… (+N more)` marker when
/// lines were dropped. Returns an empty string for empty/blank input.
#[must_use]
pub(crate) fn truncate_lines(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let lines: Vec<&str> = trimmed.lines().collect();
    if lines.len() <= max {
        return lines.join("\n");
    }
    let extra = lines.len() - max;
    let mut out = lines[..max].join("\n");
    out.push_str(&format!("\n… (+{extra} more line(s))"));
    out
}

/// Resolve a worktree's `index.lock` path — the marker git holds while an index
/// write (`git add`, `git commit`, …) is in flight (Issue #4449).
///
/// Asks git itself (`git rev-parse --git-path index.lock`) rather than assuming
/// `<wt>/.git/index.lock`, because a linked worktree's `.git` is a *file*
/// pointing at `.git/worktrees/<name>/`. Returns `None` when the path cannot be
/// resolved (not a repo, git unavailable) — the caller treats that as "no
/// evidence" and relies on the other signals.
#[must_use]
pub(crate) fn git_index_lock_path(worktree: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["rev-parse", "--git-path", "index.lock"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let rel = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if rel.is_empty() {
        return None;
    }
    let path = PathBuf::from(&rel);
    // `--git-path` yields a path relative to the *worktree* when git was invoked
    // with `-C <worktree>`; absolutise it so `exists()` is cwd-independent.
    Some(if path.is_absolute() {
        path
    } else {
        worktree.join(path)
    })
}

/// Decide, from a `.loom/tokens/.ranking` file's contents, whether the token
/// pool still has capacity to (re-)dispatch (Issue #3895). The ranking is
/// pipe-delimited `name|status` lines (refreshed by `loom-tokens check
/// --ranking`); an account is *healthy* when its status is neither `exhausted`
/// nor `blocked`.
///
/// Returns `true` (pool has capacity — proceed) when EITHER at least one account
/// is healthy OR there are no parseable `name|status` entries at all (an empty
/// or malformed ranking degrades gracefully to current behavior — the spawn-time
/// selector makes its own choice). Returns `false` only when there is at least
/// one entry and EVERY entry is `exhausted`/`blocked`, so re-dispatching would
/// just exhaust again mid-run.
#[must_use]
pub fn ranking_has_capacity(contents: &str) -> bool {
    let mut saw_entry = false;
    let mut saw_healthy = false;
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((name, status)) = line.split_once('|') else {
            continue;
        };
        if name.trim().is_empty() {
            continue;
        }
        saw_entry = true;
        let status = status.trim().to_ascii_lowercase();
        if status != "exhausted" && status != "blocked" {
            saw_healthy = true;
        }
    }
    // No parseable entries ⇒ degrade to "proceed". Otherwise require a healthy
    // account.
    !saw_entry || saw_healthy
}

/// Decide, from a sweep log's contents, whether the child has produced any
/// output *past* the daemon spawn header + runtime-adapter preamble lines.
///
/// A hung child's log region (after the most recent dispatch header) contains
/// only the header itself and `spawn-claude:`-prefixed wrapper lines (the
/// account/model/effort selection). Any other non-blank line means the child
/// got past local startup / MCP-init and is making progress. Anchoring to the
/// LAST dispatch header ensures a previous run's output in a reused per-issue
/// log is never counted as this dispatch's progress.
#[must_use]
pub fn log_has_progress(contents: &str) -> bool {
    let region = match contents.rfind(DISPATCH_HEADER_MARKER) {
        Some(i) => &contents[i..],
        None => contents,
    };
    region.lines().any(|line| {
        let t = line.trim();
        !t.is_empty()
            && !t.contains("loom-daemon dispatch:")
            && !t.contains("spawn-claude:")
            && !t.contains("spawn-codex:")
            && !t.contains("spawn-worker:")
            && !t.starts_with("# LOOM_RUNTIME_RESOLVED")
            && !t.starts_with("# LOOM_ACCOUNT")
    })
}

// ============================================================================
// Token-name capture (Issue #3802)
// ============================================================================

/// Extract the OAuth account name `spawn-claude.sh` logged, from the per-sweep
/// log `contents`, scanning only the region at/after this dispatch's header
/// (`header_anchor`, e.g. `sweep_id=<id>`). Returns `None` when the marker /
/// closing quote isn't present yet or the captured name is empty. Anchoring to
/// the current header avoids picking up a stale selection line left by a
/// previous dispatch in the same reused per-issue log.
pub(crate) fn parse_token_name_after(contents: &str, header_anchor: &str) -> Option<String> {
    // Use the LAST header occurrence: reruns append a fresh header, and the
    // current child logs its selection after the most recent one.
    let region_start = contents.rfind(header_anchor)?;
    let region = &contents[region_start..];
    let (marker_at, terminator) = if let Some(i) = region.find(ACCOUNT_LOG_MARKER) {
        (i + ACCOUNT_LOG_MARKER.len(), '\n')
    } else {
        (region.find(TOKEN_NAME_LOG_MARKER)? + TOKEN_NAME_LOG_MARKER.len(), '\'')
    };
    let after = &region[marker_at..];
    let close = after.find(terminator).unwrap_or(after.len());
    let name = after[..close].trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

pub(crate) fn parse_runtime_after(contents: &str, header_anchor: &str) -> Option<String> {
    let region = &contents[contents.rfind(header_anchor)?..];
    let after = &region[region.find(RUNTIME_LOG_MARKER)? + RUNTIME_LOG_MARKER.len()..];
    let runtime = after.lines().next()?.trim();
    (!runtime.is_empty()).then(|| runtime.to_string())
}

pub(crate) fn marker_after(contents: &str, header_anchor: &str, marker: &str) -> bool {
    contents
        .rfind(header_anchor)
        .is_some_and(|start| contents[start..].contains(marker))
}

/// Poll `log_path` for the account-selection marker until it appears, the
/// child exits, or `TOKEN_NAME_CAPTURE_TIMEOUT` elapses.
///
/// Returns the captured account name, or `UNKNOWN_TOKEN_NAME` when no
/// selection was logged (the `LOOM_SPAWN_NO_EXPORT` bypass, a timeout, or a
/// child that exited before logging). Never blocks longer than the timeout and
/// never fails dispatch.
///
/// The `try_wait` early-exit keeps no-selection cases fast (a fixture that
/// exits without logging is detected immediately rather than waiting out the
/// full window). `try_wait` caches the exit status, so the reaper's later
/// `try_wait` on the same handle still observes the exit.
pub(crate) fn poll_observability(
    child: &mut Child,
    log_path: &Path,
    header_anchor: &str,
) -> (String, String) {
    let deadline = std::time::Instant::now() + TOKEN_NAME_CAPTURE_TIMEOUT;
    let mut runtime = None;
    loop {
        if let Ok(contents) = std::fs::read_to_string(log_path) {
            runtime = runtime.or_else(|| parse_runtime_after(&contents, header_anchor));
            if let Some(name) = parse_token_name_after(&contents, header_anchor) {
                return (name, runtime.unwrap_or_else(|| UNKNOWN_TOKEN_NAME.to_string()));
            }
            if runtime.is_some() && marker_after(&contents, header_anchor, CLI_START_MARKER) {
                return (
                    UNKNOWN_TOKEN_NAME.to_string(),
                    runtime.unwrap_or_else(|| UNKNOWN_TOKEN_NAME.to_string()),
                );
            }
        }
        // If the child has already exited without logging a selection, the
        // line will never appear — do a final read (covers a log flushed right
        // before exit) and stop, rather than waiting out the timeout.
        if matches!(child.try_wait(), Ok(Some(_))) {
            if let Ok(contents) = std::fs::read_to_string(log_path) {
                if let Some(name) = parse_token_name_after(&contents, header_anchor) {
                    return (
                        name,
                        parse_runtime_after(&contents, header_anchor)
                            .or(runtime)
                            .unwrap_or_else(|| UNKNOWN_TOKEN_NAME.to_string()),
                    );
                }
            }
            return (
                UNKNOWN_TOKEN_NAME.to_string(),
                runtime.unwrap_or_else(|| UNKNOWN_TOKEN_NAME.to_string()),
            );
        }
        if std::time::Instant::now() >= deadline {
            return (
                UNKNOWN_TOKEN_NAME.to_string(),
                runtime.unwrap_or_else(|| UNKNOWN_TOKEN_NAME.to_string()),
            );
        }
        std::thread::sleep(TOKEN_NAME_CAPTURE_POLL_INTERVAL);
    }
}

/// Recover the OAuth account name a previously-dispatched, now-adopted sweep
/// selected, from its surviving per-sweep log (Issue #4173).
///
/// Restart adoption (`reconstruct`) restores in-flight sweeps from the
/// per-issue lock's `owner.json`, which records the `sweep_id` but **not** the
/// token — so the adoption site previously defaulted `token_name` to
/// `UNKNOWN_TOKEN_NAME`, losing status attribution and (worse) leaving an
/// account-exhaustion death un-markable via the #4122 path (`insta_crash_is_
/// account_exhaustion` cannot mark an account bad when `token=unknown`).
///
/// The token was captured at dispatch by parsing the per-sweep log for the
/// `using OAuth account '<name>'` marker anchored to this dispatch's
/// `sweep_id=<id>` header (`parse_token_name_after`). Both the log file and the
/// `sweep_id` (from `owner.json`) survive a daemon restart, so we re-run the
/// same parser once at startup — a single bounded read, no polling: the line
/// was either written long ago or (for a rotated/truncated/no-selection log)
/// never will be.
///
/// Degrades gracefully to `UNKNOWN_TOKEN_NAME` when the log is missing or lacks
/// the selection line, so adoption never fails on capture.
///
/// Note (capacity accounting): recovery restores *attribution* only. An
/// `unknown` adopted sweep was never a capacity-exemption bug — the concurrency
/// cap is `min(disk_headroom, ram_headroom, configured_max)` vs. total occupancy
/// (`resolve_dynamic_max_concurrent`, #5270), and adopted `Running` entries
/// already count toward that occupancy regardless of `token_name`. What
/// `unknown` costs is `status` visibility and the #4122 bad-marking path.
pub(crate) fn recover_adopted_token_name(log_path: &Path, sweep_id: &str) -> String {
    let anchor = format!("sweep_id={sweep_id}");
    std::fs::read_to_string(log_path)
        .ok()
        .and_then(|contents| parse_token_name_after(&contents, &anchor))
        .unwrap_or_else(|| UNKNOWN_TOKEN_NAME.to_string())
}

pub(crate) fn recover_adopted_runtime(log_path: &Path, sweep_id: &str) -> String {
    let anchor = format!("sweep_id={sweep_id}");
    std::fs::read_to_string(log_path)
        .ok()
        .and_then(|contents| parse_runtime_after(&contents, &anchor))
        .unwrap_or_else(|| UNKNOWN_TOKEN_NAME.to_string())
}

// ============================================================================
// Account-exhaustion classification at insta-crash time (Issue #4122)
// ============================================================================

/// Number of trailing log lines scanned for an account-exhaustion signature
/// when classifying an insta-crash (#4122). The wrapper's exhaustion banner /
/// `RATE_LIMIT_ABORT` sentinel is emitted right before the child dies, so the
/// tail is where it lands; a bounded tail keeps the read cheap.
pub(crate) const EXHAUSTION_LOG_TAIL_LINES: usize = 200;

/// The account-exhaustion signature table (#4122).
///
/// Each row is a `(label, regex)` pair matched against the tail of a dead
/// sweep's log at insta-crash classification time. A match means the child died
/// because its OAuth account hit a usage/weekly/session limit — that is the
/// ACCOUNT's fault, not the ISSUE's, so the reaper marks the account bad rather
/// than charging the issue's insta-crash quarantine tally.
///
/// Kept as a table so a future signature (e.g. #4027's `Unknown command:`
/// crash) can be added as another row without touching the classifier logic.
/// The regexes mirror `defaults/scripts/claude-wrapper.sh::is_account_exhaustion`
/// so the daemon path classifies exhaustion the same way the wrapper does.
pub(crate) fn exhaustion_signatures() -> &'static [(&'static str, Regex)] {
    static SIGS: OnceLock<Vec<(&'static str, Regex)>> = OnceLock::new();
    SIGS.get_or_init(|| {
        vec![
            (
                // Neutral label (#4212): this banner matches 5h-window ("hit
                // your limit"), session, weekly, AND monthly signals — we
                // cannot tell which window from the text, so labeling it
                // "weekly-limit" materially misleads recovery decisions (a real
                // weekly limit justifies waiting days; a 5h event, hours). The
                // reason is transient exhaustion regardless, so use a neutral
                // "rate-limited" that does not overclaim the window.
                "rate-limited",
                Regex::new(
                    r"(?i)hit your (limit|session limit|weekly limit)|hit\.your\.limit|monthly usage limit|out of extra usage",
                )
                .expect("valid static exhaustion regex"),
            ),
            (
                "rate-limit-abort",
                // The output/startup monitors kill the CLI and emit this
                // sentinel on the interactive usage/plan-limit modal and the
                // 100%-weekly banner (claude-wrapper.sh). Case-sensitive: it is
                // a literal sentinel token, not free-form prose.
                Regex::new(r"RATE_LIMIT_ABORT").expect("valid static exhaustion regex"),
            ),
            (
                // Issue #5697 (daemon-path half of #5687): the per-model-TIER
                // credit exhaustion signature `classify-error.sh` names
                // `MODEL_CREDITS_EXHAUSTED` — "You're out of usage credits".
                // This table already fed the reaper's crash-classification
                // label (`account-exhausted:<sig>`), so this row gives that
                // label a name distinct from `rate-limited` /
                // `rate-limit-abort` / `model-limit` — mirroring the shell
                // classifier's regex byte-for-byte (kept as two independent
                // literals rather than a shared source so Bash `grep -E` and
                // Rust `regex` syntax can each stay idiomatic). The account
                // pool's health TREATMENT is unaffected either way: this only
                // feeds the reaper's best-effort crash-signature label, never
                // `tokens_pool::health` (Claude-runtime sweeps never call
                // that Codex/generic-only module — see its header comment).
                // Ordered LAST, after the `TOKEN_EXHAUSTED`-family rows above,
                // mirroring `classify-error.sh`'s explicit ordering (its own
                // comment: the #4501 per-model-ceiling message also mentions
                // "credits" via its `/usage-credits` command hint, and must
                // keep matching `rate-limited`/`model-limit` first).
                "model-credits-exhausted",
                Regex::new(
                    r"(?i)(ran |run )?out of (usage |extra |plan )?credits|no (usage |extra |plan )?credits (remaining|left)|insufficient (usage |plan )?credits",
                )
                .expect("valid static credit-exhaustion regex"),
            ),
        ]
    })
}

/// The PER-MODEL usage-ceiling signature (#4501): `You've reached your Fable 5
/// limit. Run /usage-credits to continue or switch models with /model.` The model
/// name sits between "your" and "limit", so none of the "hit your … limit"
/// phrasings in [`exhaustion_signatures`] matched it — a child that insta-crashed
/// on this banner was charged to the ISSUE's quarantine tally instead of the
/// ACCOUNT, which is exactly backwards. The filler is bounded to at most three
/// words so an unrelated sentence merely ending in "limit" cannot match.
///
/// Kept out of [`exhaustion_signatures`] because it needs the line-wise
/// concurrency exclusion below, which a plain `(label, regex)` row cannot express
/// (the `regex` crate has no lookaround).
pub(crate) fn model_limit_signature() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)reached your (?:\S+\s+){0,3}limit")
            .expect("valid static model-limit regex")
    })
}

/// The concurrent-session-limit wording (#3947), used ONLY to exclude a line from
/// [`model_limit_signature`] (#4501). `Error: you have reached your concurrent
/// session limit` also matches the "reached your … limit" shape, but it is a
/// CAPACITY fault (the account is healthy, it just cannot start another
/// simultaneous session) — marking the account bad for it would shrink the healthy
/// pool. This mirrors `classify-error.sh`'s SESSION_LIMIT-before-TOKEN_EXHAUSTED
/// ordering on the daemon side.
pub(crate) fn session_capacity_exclusion() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)concurrent (session|sessions|request)|maximum number of concurrent|too many concurrent|simultaneous session|another session is (already )?(active|running)",
        )
        .expect("valid static session-capacity regex")
    })
}

/// Scan `log_tail` for any account-exhaustion signature (#4122). Returns the
/// matched signature label on the first hit, else `None`.
///
/// The per-model ceiling (#4501) is matched LINE-WISE after the table rows, so a
/// line whose wording is a concurrent-session capacity fault (#3947) is skipped
/// rather than mistaken for quota exhaustion.
pub(crate) fn classify_account_exhaustion(log_tail: &str) -> Option<&'static str> {
    if let Some(label) = exhaustion_signatures()
        .iter()
        .find(|(_, re)| re.is_match(log_tail))
        .map(|(label, _)| *label)
    {
        return Some(label);
    }
    log_tail
        .lines()
        .any(|line| {
            model_limit_signature().is_match(line) && !session_capacity_exclusion().is_match(line)
        })
        .then_some("model-limit")
}

// ============================================================================
// Claude-wrapper pre-flight-death classification (Issue #4386)
// ============================================================================

/// The claude-wrapper pre-flight-death signature table (#4386): explicit
/// positive markers `defaults/scripts/claude-wrapper.sh` emits when it aborts
/// **before ever exec'ing the CLI** (e.g. its MCP-init pre-flight probe
/// fails). Matched against the tail of a dead sweep's log exactly like
/// [`exhaustion_signatures`] — kept as a table so a future explicit
/// wrapper-abort marker is one new row, not a new code path.
///
/// Unlike `exhaustion_signatures`, this table only covers markers with an
/// explicit positive signature in the log. The complementary "the process
/// died without EVER reaching `# CLAUDE_CLI_START`" case has no marker of its
/// own — it's an absence, not a positive signature — so it is checked
/// separately by [`classify_preflight_death`], after this table comes up
/// empty.
pub(crate) fn preflight_death_signatures() -> &'static [(&'static str, Regex)] {
    static SIGS: OnceLock<Vec<(&'static str, Regex)>> = OnceLock::new();
    SIGS.get_or_init(|| {
        vec![
            (
                "preflight-mcp-failed",
                // Emitted by claude-wrapper.sh's MCP-init pre-flight check right
                // before it bails without ever exec'ing the CLI. Case-sensitive:
                // a literal sentinel token, not free-form prose (mirrors
                // `RATE_LIMIT_ABORT` in `exhaustion_signatures`).
                Regex::new(r"#\s*MCP_PREFLIGHT_FAILED").expect("valid static preflight regex"),
            ),
            (
                // Issue #4644: `spawn-claude.sh`'s "Token selection" step exits
                // 78 (`EX_CONFIG`) on any of three distinct failures — no
                // `loom-daemon` binary supporting `tokens select`, the select
                // subcommand itself failing (typically an empty/exhausted
                // pool), or a selection that resolved to an empty key — before
                // ever touching the CLI. Matching this exact prose gives the
                // reaper a machine-readable `token_selection_failed`-shaped
                // label distinct from the generic absence-based
                // `preflight-no-cli-start` fallback below, so a durable
                // journal reader (or an operator) can tell "the token pool
                // itself is the problem" apart from other pre-flight causes
                // (a stale `.mcp.json`, etc.) without reading log prose.
                "preflight-token-selection-failed",
                Regex::new(
                    r"(?i)token selection failed|token selection returned an empty key|no loom-daemon binary supporting 'tokens select'",
                )
                .expect("valid static token-selection preflight regex"),
            ),
        ]
    })
}

/// The literal marker `claude-wrapper.sh` logs immediately before exec'ing the
/// CLI (Issue #4386). Its absence from a dead sweep's ENTIRE log tail means
/// the child never got that far — the complementary, absence-based half of
/// [`classify_preflight_death`].
pub(crate) const CLAUDE_CLI_START_MARKER: &str = "# CLAUDE_CLI_START";

/// Classify `log_tail` as a claude-wrapper pre-flight death (Issue #4386):
/// either an explicit [`preflight_death_signatures`] marker matched, or —
/// when no explicit marker matched — the tail never reached
/// [`CLAUDE_CLI_START_MARKER`] at all, meaning the child died before the
/// wrapper ever exec'd the CLI. Returns the matched label, else `None`.
///
/// Callers are responsible for the precedence rule with
/// [`classify_account_exhaustion`] (exhaustion wins — see
/// `SweepRegistry::record_preflight_streak`) and for treating an
/// unreadable/missing log as `None`/"unknown" rather than calling this with
/// an empty tail (mirrors how [`classify_crash`] is fed by its callers).
pub(crate) fn classify_preflight_death(log_tail: &str) -> Option<&'static str> {
    // Per-issue logs are append-only. Restrict both positive and absence-based
    // classification to the newest dispatch so markers from a prior run
    // cannot classify (or clear) the current run's pre-flight death.
    let current_dispatch = log_tail
        .rfind(DISPATCH_HEADER_MARKER)
        .map_or(log_tail, |start| &log_tail[start..]);
    if let Some((label, _)) = preflight_death_signatures()
        .iter()
        .find(|(_, re)| re.is_match(current_dispatch))
    {
        return Some(label);
    }
    if !current_dispatch.contains(CLAUDE_CLI_START_MARKER)
        && !current_dispatch.contains(CLI_START_MARKER)
    {
        return Some("preflight-no-cli-start");
    }
    None
}

/// The three-way outcome of consulting a dead sweep's log tail for the #4386
/// pre-flight workspace tripwire, feeding
/// `SweepRegistry::record_preflight_streak`'s streak update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PreflightOutcome {
    /// [`classify_preflight_death`] matched — this death counts toward (and
    /// is reported as) the pre-flight streak.
    Preflight(&'static str),
    /// The tail reached [`CLAUDE_CLI_START_MARKER`] (and no exhaustion
    /// signature matched) — definitively NOT a pre-flight death, resetting
    /// the streak.
    NonPreflight,
    /// The log was unreadable/missing, OR an account-exhaustion signature
    /// matched (exhaustion wins — that death is already attributed to the
    /// account, #4122). Neutral: neither increments nor resets the streak.
    Unknown,
}

/// Classify a dead sweep's (possibly absent) log tail for the #4386 pre-flight
/// workspace tripwire. `tail` is `None` when the log was missing/unreadable at
/// reap time.
pub(crate) fn classify_preflight_outcome(tail: Option<&str>) -> PreflightOutcome {
    let Some(t) = tail else {
        return PreflightOutcome::Unknown;
    };
    if classify_account_exhaustion(t).is_some() {
        // Exhaustion wins (#4122): this death is already attributed to the
        // spawn account, so it must not also be counted toward — or reset —
        // the pre-flight streak.
        return PreflightOutcome::Unknown;
    }
    match classify_preflight_death(t) {
        Some(label) => PreflightOutcome::Preflight(label),
        None => PreflightOutcome::NonPreflight,
    }
}

/// The `self-kill:background-wait` signature regex (Issue #4389, a recurrence
/// of #4257): matches final assistant/log text that promises to be notified
/// about — or to check back on — outstanding background work, e.g. "I'll
/// wait for the background rebuild to complete and check back" or "will be
/// notified when the background task completes". That is the exact self-kill
/// pattern this issue targets: in headless `claude -p` mode there is no
/// "later" — ending the turn kills the process and every still-running
/// background child with it, so a death whose log tail reads like this is a
/// process-design self-kill, not a crash. Matched case-insensitively.
pub(crate) fn background_wait_signature() -> &'static Regex {
    static SIG: OnceLock<Regex> = OnceLock::new();
    SIG.get_or_init(|| {
        Regex::new(r"(?i)background.*(notify|complete|check|pick up)")
            .expect("valid static background-wait regex")
    })
}

/// Best-effort crash classification for the `sweep.issue.{N}.crashed` payload
/// (Issue #4255, extended by #4389). Derives a short, stable label from the
/// dead sweep's log tail plus its exit code so a subscriber (#4137
/// telemetry) can attribute WHY the sweep died rather than re-parsing
/// free-form logs. The caller only invokes this when a checkpoint exists for
/// the dead sweep (i.e. it made mid-lifecycle progress before dying) — see
/// the `checkpoint.exists()` guard around this function's call site.
///
/// Precedence, most-specific first:
/// 1. `account-exhausted:<signature>` — the account hit a usage/weekly/session
///    limit (reuses the #4122 signature table so the daemon classifies
///    exhaustion the same way the wrapper does).
/// 2. `execution-error` — the CLI printed the bare `Execution error` string
///    (the chronic transient-death mode this issue targets; the wrapper now
///    retries it, so reaching the reaper means retries were exhausted).
/// 3. `self-kill:background-wait` (#4389) — the log tail reads like the
///    orchestrator ended its turn believing outstanding background work
///    (a Task subagent or a `run_in_background` Bash task) would notify it
///    later. Checked regardless of exit code: this self-kill mode often
///    exits *clean* (0) or with no captured code — the orchestrator believed
///    it was finishing normally — which is exactly why it fell through to
///    `None` (unclassified) before this signature existed.
/// 4. `exit-<code>` — any other non-zero exit whose log yields no known
///    signature. `None` when the exit was clean/unknown with no signature, so
///    the field is omitted from the wire form rather than carrying noise.
pub(crate) fn classify_crash(log_tail: &str, exit_code: Option<i32>) -> Option<String> {
    if let Some(sig) = classify_account_exhaustion(log_tail) {
        return Some(format!("account-exhausted:{sig}"));
    }
    // Literal `Execution error` (case-insensitive) — the unlogged transient
    // death mode. Mirrors `claude-wrapper.sh::is_transient_error`'s new pattern
    // so a death that survived the wrapper's retry loop is still labeled.
    if log_tail.to_ascii_lowercase().contains("execution error") {
        return Some("execution-error".to_string());
    }
    // #4389: a background-wait self-kill often exits clean/uncaptured, so this
    // must be checked BEFORE the exit_code match below (which would otherwise
    // silently fall through to `None` for exactly that case).
    if background_wait_signature().is_match(log_tail) {
        return Some("self-kill:background-wait".to_string());
    }
    match exit_code {
        Some(0) | None => None,
        Some(code) => Some(format!("exit-{code}")),
    }
}

// ============================================================================
// Internal helpers
// ============================================================================

/// POSIX `EPERM`. Value 1 on every Loom target (Linux and macOS both fix it at
/// 1 in `<sys/errno.h>`), so no `libc` dependency is needed to name it.
#[cfg(unix)]
pub(crate) const EPERM: i32 = 1;

/// Liveness probe via `kill(pid, 0)`. Returns true when the process exists.
/// PID 0 is always treated as dead.
///
/// **Fail-safe on `EPERM` (#4691).** `kill(2)` has two distinct failure modes
/// and treating them alike is a correctness bug, not a nicety:
///
/// - `ESRCH` — no such process. Genuinely dead; callers may reap its state.
/// - `EPERM` — the process **exists** but this caller may not signal it
///   (different UID, sandbox, namespace). Reporting that as "dead" lets the
///   reaper, [`crate::claim_reconciliation`], and `loom-daemon clean` destroy a
///   *running* sweep's state — e.g. deleting a live run's
///   `main-clean-baseline-<RUN_ID>.txt` out from under it.
///
/// Every consumer of this probe uses "dead" to authorize destruction, so the
/// ambiguous case must resolve to *alive*.
///
/// `pub(crate)` so [`crate::sweep_journal`] and [`crate::claim_reconciliation`]
/// can reuse the exact same probe the reaper uses (Issue #3953) rather than
/// duplicating the `kill(pid, 0)` dance.
#[cfg(unix)]
pub(crate) fn is_pid_alive(pid: u32) -> bool {
    is_pid_alive_with(pid, |pid_t| {
        // SAFETY: kill(pid, 0) is a documented liveness probe; signal 0 is not
        // sent, just checked.
        if libc_kill(pid_t, 0) == 0 {
            Ok(())
        } else {
            // `errno` is only meaningful immediately after a failed call.
            Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
        }
    })
}

/// Decision core of [`is_pid_alive`], parameterized over the raw `kill(pid, 0)`
/// syscall so the `ESRCH`-vs-`EPERM` distinction is unit-testable without
/// needing a real unsignallable process (#4691).
///
/// `probe` returns `Ok(())` when the signal would be deliverable, or
/// `Err(errno)` with the raw `errno` from the failed `kill(2)`.
#[cfg(unix)]
pub(crate) fn is_pid_alive_with(pid: u32, probe: impl Fn(i32) -> Result<(), i32>) -> bool {
    if pid == 0 {
        return false;
    }
    let pid_t: i32 = match pid.try_into() {
        Ok(p) => p,
        Err(_) => return false,
    };
    match probe(pid_t) {
        Ok(()) => true,
        // EPERM proves existence just as well as success does.
        Err(errno) => errno == EPERM,
    }
}

#[cfg(not(unix))]
pub(crate) fn is_pid_alive(_pid: u32) -> bool {
    // Non-unix platforms are not supported targets for Loom; assume alive
    // so the test suite can run without a hard panic.
    true
}

#[cfg(unix)]
extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
    fn getpgid(pid: i32) -> i32;
}

#[cfg(unix)]
#[allow(non_snake_case)]
pub(crate) fn libc_kill(pid: i32, sig: i32) -> i32 {
    // Indirection so we can stub this in unit tests if needed.
    unsafe { kill(pid, sig) }
}

/// The process-group ID `pid` currently belongs to, or `None` when it cannot be
/// determined (the process is gone — `ESRCH` — or the pid is out of range).
///
/// Issue #4980: the registry persists a sweep's pgid so a *reconstructed* entry
/// (post-restart) or a fresh `loom-daemon cancel` process can still group-kill
/// the whole tree. This probe is how the recorded value is **verified** rather
/// than assumed: a sweep child spawned with `process_group(0)` is its own group
/// leader, so `getpgid(child) == child`. Anything else means the spawn did not
/// take (or the pid was recycled), and the caller must degrade to single-PID
/// signalling instead of blast-radiusing an unrelated group.
///
/// `pid == 0` is rejected: `getpgid(0)` means "the caller's own group", which is
/// never a meaningful answer for a *tracked child* and would invite exactly the
/// self-group kill this module's `pgid == 0` guards exist to prevent.
#[cfg(unix)]
pub(crate) fn process_group_of(pid: u32) -> Option<u32> {
    if pid == 0 {
        return None;
    }
    let pid_t: i32 = pid.try_into().ok()?;
    // SAFETY: `getpgid` is a read-only query with no memory arguments.
    let pgid = unsafe { getpgid(pid_t) };
    u32::try_from(pgid).ok().filter(|&p| p != 0)
}

#[cfg(not(unix))]
pub(crate) fn process_group_of(_pid: u32) -> Option<u32> {
    None
}

/// The process group **this daemon/CLI process itself** belongs to (Issue
/// #4980).
///
/// Load-bearing safety input, not a diagnostic: `kill(-pgid, sig)` against our
/// own group would signal the daemon (and every one of its children) — the
/// catastrophic mis-target that `send_group_signal`'s `pgid == 0` rejection
/// already guards for the literal-zero case. A persisted-but-stale pgid read
/// back from an old `owner.json` could name our own group by coincidence of PID
/// recycling, so every group-signal caller cross-checks against this first.
#[cfg(unix)]
pub(crate) fn current_process_group() -> Option<u32> {
    process_group_of(std::process::id())
}

#[cfg(not(unix))]
pub(crate) fn current_process_group() -> Option<u32> {
    None
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::expect_used,
    unused_imports
)]
mod tests {
    use super::*;
    use crate::sweep_registry::test_support::*;
    use serial_test::serial;
    use std::os::unix::fs::PermissionsExt;
    use std::time::SystemTime;
    use tempfile::tempdir;

    #[test]
    fn is_pid_alive_reports_deliverable_signal_as_alive() {
        assert!(is_pid_alive_with(4242, |_| Ok(())));
    }

    #[test]
    fn is_pid_alive_reports_esrch_as_dead() {
        // The only failure mode that actually proves the process is gone.
        assert!(!is_pid_alive_with(4242, |_| Err(ESRCH)));
    }

    #[test]
    fn is_pid_alive_treats_eperm_as_alive_not_dead() {
        // #4691: `kill(pid, 0)` failing with EPERM means the process EXISTS but
        // is not signallable by this caller. Conflating it with ESRCH lets the
        // reaper / `loom-daemon clean` destroy a live sweep's state.
        assert!(
            is_pid_alive_with(4242, |_| Err(EPERM)),
            "EPERM proves existence — must never be reported as dead"
        );
    }

    #[test]
    fn is_pid_alive_rejects_pid_zero_without_probing() {
        // PID 0 addresses the caller's whole process group in kill(2); it is
        // never a valid liveness handle, so it must short-circuit to dead.
        assert!(!is_pid_alive_with(0, |_| panic!("must not probe pid 0")));
    }

    #[test]
    fn is_pid_alive_of_self_is_true_through_the_real_syscall() {
        // End-to-end sanity that the real probe still works after the refactor.
        assert!(is_pid_alive(std::process::id()));
    }

    /// Issue #4255: `classify_crash` derives a stable label from a dead sweep's
    /// log tail + exit code for the `sweep.issue.{N}.crashed` payload.
    #[test]
    fn classify_crash_labels_death_causes() {
        // The chronic transient-death signature (case-insensitive).
        assert_eq!(
            classify_crash("spawn preamble\nExecution error\n", None).as_deref(),
            Some("execution-error")
        );
        // Account exhaustion wins over everything else (reuses the #4122 table).
        assert_eq!(
            classify_crash("hit your weekly limit — try again later", Some(1)).as_deref(),
            Some("account-exhausted:rate-limited")
        );
        // Issue #5697: per-model-TIER credit exhaustion carries its own
        // distinct crash label, never folded into `rate-limited`.
        assert_eq!(
            classify_crash("You're out of usage credits for this model.", Some(1)).as_deref(),
            Some("account-exhausted:model-credits-exhausted")
        );
        // Any other non-zero exit with no known signature → bare exit label.
        assert_eq!(classify_crash("some opaque build failure", Some(2)).as_deref(), Some("exit-2"));
        // A clean/unknown exit with no signature carries no classification.
        assert_eq!(classify_crash("", Some(0)), None);
        assert_eq!(classify_crash("nothing notable", None), None);
    }

    /// Issue #4389 (a recurrence of #4257): `classify_crash` gains a
    /// `self-kill:background-wait` signature for a dead sweep whose log tail
    /// reads like the orchestrator ended its turn believing outstanding
    /// background work would notify it later.
    #[test]
    fn classify_crash_labels_background_wait_self_kill() {
        // Match: promises to be notified when a background task completes,
        // with a clean exit code — exactly the case that fell through to
        // `None` before this signature existed (#4389's root cause).
        assert_eq!(
            classify_crash(
                "I'll wait for the background rebuild to notify me when it completes.",
                Some(0),
            )
            .as_deref(),
            Some("self-kill:background-wait")
        );
        // Match: also fires with no captured exit code at all.
        assert_eq!(
            classify_crash(
                "I'll wait for the background task and check back once it's done.",
                None
            )
            .as_deref(),
            Some("self-kill:background-wait")
        );
        // Non-match: "background" appears but not paired with a
        // notify/complete/check/pick-up promise anywhere after it.
        assert_eq!(
            classify_crash("running the rebuild in the background for speed", Some(0)),
            None
        );
        // Non-match: a notify/complete/check word with no "background" in the
        // same tail — not this signature at all.
        assert_eq!(classify_crash("the build will complete shortly", Some(0)), None);
        // Precedence: account exhaustion still wins over the background-wait
        // signature even when both are present in the tail.
        assert_eq!(
            classify_crash(
                "hit your weekly limit — try again later\nI'll check the background task after that.",
                Some(1),
            )
            .as_deref(),
            Some("account-exhausted:rate-limited")
        );
        // Precedence: the literal `Execution error` signature still wins over
        // the background-wait signature.
        assert_eq!(
            classify_crash("Execution error\nI'll check on the background task later.", Some(1),)
                .as_deref(),
            Some("execution-error")
        );
    }

    /// The signature classifier matches the wrapper's exhaustion banners /
    /// sentinel and ignores unrelated crash text (#4122).
    #[test]
    fn classify_account_exhaustion_matches_signatures() {
        assert_eq!(
            classify_account_exhaustion("Claude: hit your weekly limit — try again later"),
            Some("rate-limited")
        );
        assert_eq!(
            classify_account_exhaustion("you are out of extra usage this month"),
            Some("rate-limited")
        );
        assert_eq!(
            classify_account_exhaustion("monitor emitted RATE_LIMIT_ABORT and exited"),
            Some("rate-limit-abort")
        );
        assert_eq!(classify_account_exhaustion("thread 'main' panicked at foo.rs:1"), None);
        assert_eq!(classify_account_exhaustion(""), None);
    }

    /// Issue #5697: the per-model-TIER credit exhaustion signature
    /// (`classify-error.sh`'s `MODEL_CREDITS_EXHAUSTED`, "You're out of usage
    /// credits") gets its OWN account-exhaustion label — distinct from
    /// `rate-limited`, `rate-limit-abort`, AND `model-limit` — so the reaper's
    /// crash classification can name it, without changing how the account
    /// pool treats it.
    #[test]
    fn classify_account_exhaustion_matches_model_credit_exhaustion() {
        assert_eq!(
            classify_account_exhaustion("You're out of usage credits for this model."),
            Some("model-credits-exhausted")
        );
        assert_eq!(
            classify_account_exhaustion("Error: you ran out of plan credits"),
            Some("model-credits-exhausted")
        );
        assert_eq!(
            classify_account_exhaustion("No extra credits remaining on this account."),
            Some("model-credits-exhausted")
        );
        assert_eq!(
            classify_account_exhaustion("insufficient usage credits to continue"),
            Some("model-credits-exhausted")
        );
        // The #4501 per-model-ceiling message mentions "credits" only via its
        // `/usage-credits` command hint — it must keep classifying as
        // `model-limit`, never fall into the new credit-exhaustion row.
        assert_eq!(
            classify_account_exhaustion(
                "You've reached your Fable 5 limit. Run /usage-credits to continue or switch \
                 models with /model."
            ),
            Some("model-limit")
        );
    }

    /// Issue #4501: the CLI's PER-MODEL ceiling is an account fault too. Before
    /// this, a child that insta-crashed on "You've reached your Fable 5 limit"
    /// matched no signature, so the reaper charged the ISSUE's quarantine tally
    /// instead of marking the account bad — the daemon-side half of the same
    /// missed phrasing `classify-error.sh` missed.
    #[test]
    fn classify_account_exhaustion_matches_per_model_ceiling() {
        assert_eq!(
            classify_account_exhaustion(
                "You've reached your Fable 5 limit. Run /usage-credits to continue or switch \
                 models with /model."
            ),
            Some("model-limit")
        );
        assert_eq!(
            classify_account_exhaustion("Claude: you have reached your limit"),
            Some("model-limit")
        );
        // A concurrent-session capacity fault (#3947) must NOT be treated as
        // exhaustion — marking the account bad would shrink the healthy pool.
        assert_eq!(
            classify_account_exhaustion("Error: you have reached your concurrent session limit"),
            None
        );
        // An unrelated sentence that merely ends in "limit" is not exhaustion.
        assert_eq!(
            classify_account_exhaustion(
                "reached your goal well before the team agreed on any spending limit"
            ),
            None
        );
        // Mixed tail: the model-limit line still classifies even when a
        // concurrency line is also present.
        assert_eq!(
            classify_account_exhaustion(
                "Error: you have reached your concurrent session limit\n\
                 You've reached your Fable 5 limit. Run /usage-credits to continue."
            ),
            Some("model-limit")
        );
        // The legacy table still wins its label when both shapes appear.
        assert_eq!(
            classify_account_exhaustion(
                "You've hit your weekly limit\nYou've reached your Fable 5 limit."
            ),
            Some("rate-limited")
        );
    }

    /// #4386: `classify_preflight_death` matches the explicit
    /// `# MCP_PREFLIGHT_FAILED` marker, matches a tail that never reaches
    /// `# CLAUDE_CLI_START` at all, and does NOT match a normal tail that
    /// does reach `# CLAUDE_CLI_START`.
    #[test]
    fn classify_preflight_death_matches_signatures() {
        assert_eq!(
            classify_preflight_death("spawn-claude: dispatching\n# MCP_PREFLIGHT_FAILED\n"),
            Some("preflight-mcp-failed")
        );
        assert_eq!(
            classify_preflight_death("spawn-claude: dispatching\nsome other early failure\n"),
            Some("preflight-no-cli-start")
        );
        assert_eq!(
            classify_preflight_death(
                "spawn-claude: dispatching\n# CLAUDE_CLI_START\nClaude: working on it\n"
            ),
            None
        );
        assert_eq!(
            classify_preflight_death(
                "==== loom-daemon dispatch: old ====\n\
# LOOM_CLI_START runtime=codex\n\
==== loom-daemon dispatch: current ====\nspawn-codex: preflight failed\n"
            ),
            Some("preflight-no-cli-start")
        );
    }

    /// Issue #4173 + #4122: an adopted sweep that insta-crashes on an
    /// account-exhaustion signature marks the RECOVERED account bad — the fix
    /// closes the gap where `token=unknown` left the burned account in rotation.
    #[test]
    fn adopted_exhaustion_marks_recovered_account_bad() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        seed_token_pool(dir.path(), "agent4-2amlogic");

        let locks = registry.config.locks_dir();
        std::fs::create_dir_all(&locks).unwrap();
        let lock = locks.join("issue-404");
        std::fs::create_dir(&lock).unwrap();
        let sweep_id = "sweep-issue-404-adopt";
        let owner = LockOwner {
            pgid: None,
            issue: 404,
            owner_pid: std::process::id(),
            acquired_at: Utc::now().to_rfc3339(),
            sweep_id: sweep_id.to_string(),
        };
        std::fs::write(lock.join("owner.json"), serde_json::to_string_pretty(&owner).unwrap())
            .unwrap();

        // Surviving log: dispatch header + selection line + exhaustion tail.
        let log_path = registry.compute_log_path(404);
        std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
        std::fs::write(
            &log_path,
            format!(
                "sweep_id={sweep_id} issue=404 ====\n\
                 spawn-claude: using OAuth account 'agent4-2amlogic' (mode=ranking)\n\
                 Claude: hit your weekly limit — try again later\n"
            ),
        )
        .unwrap();

        assert!(registry.reconstruct().unwrap() >= 1);
        assert_eq!(
            registry.get(sweep_id).unwrap().token_name,
            "agent4-2amlogic",
            "precondition: token recovered on adoption"
        );

        // The adopted sweep dies with an exhaustion signature: the recovered
        // account (not `unknown`) is marked bad via the #4122 path.
        let charged_account =
            registry.insta_crash_is_account_exhaustion(&sweep_id.to_string(), 404);
        assert!(charged_account, "exhaustion classified → account charged, issue spared");
        assert!(
            bad_tokens::is_bad(dir.path(), "agent4-2amlogic"),
            "recovered account marked bad on adopted-sweep exhaustion death (#4173)"
        );
    }

    #[test]
    fn terminal_result_parser_is_anchored_strict_and_provider_aware() {
        let log = "\
sweep_id=old
# LOOM_TERMINAL_RESULT v=1 provider=codex account=stale category=TOKEN_EXPIRED exit_code=1
sweep_id=current
# LOOM_TERMINAL_RESULT v=1 provider=codex account=profile-a category=TOKEN_EXHAUSTED exit_code=42
";
        assert_eq!(
            parse_terminal_result_after(log, "sweep_id=current"),
            Some(TerminalResult {
                provider: AccountProvider::Codex,
                account: "profile-a".into(),
                category: TerminalClassification::TokenExhausted,
                exit_code: 42,
            })
        );
        assert!(parse_terminal_result_after(
            "sweep_id=current\n# LOOM_TERMINAL_RESULT v=9 provider=codex account=a category=SUCCESS exit_code=0",
            "sweep_id=current"
        )
        .is_none());
        assert!(parse_terminal_result_after(
            "sweep_id=current\n# LOOM_TERMINAL_RESULT v=1 provider=codex account=unknown category=SUCCESS exit_code=0",
            "sweep_id=current"
        )
        .is_none());
        assert!(parse_terminal_result_after(
            "sweep_id=current\n# LOOM_TERMINAL_RESULT v=1 provider=codex account=a category=BOGUS exit_code=1",
            "sweep_id=current"
        )
        .is_none());
    }

    // ------------------------------------------------------------------------
    // Issue #3802: capture spawn-claude's selected account into token_name.
    // ------------------------------------------------------------------------

    #[test]
    fn parse_token_name_after_extracts_selected_account() {
        let log = "\
==== loom-daemon dispatch: 2026-07-23T00:00:00Z sweep_id=sweep-issue-3780-abc issue=3780 ====
\u{1b}[0;34m[2026-07-23T00:00:01Z]\u{1b}[0m spawn-claude: using OAuth account 'agent3-2amlogic' (mode=random)
";
        assert_eq!(
            parse_token_name_after(log, "sweep_id=sweep-issue-3780-abc").as_deref(),
            Some("agent3-2amlogic"),
        );
    }

    #[test]
    fn neutral_observability_parser_is_current_dispatch_scoped() {
        let log = "==== loom-daemon dispatch: old sweep_id=old ====\n\
# LOOM_RUNTIME_RESOLVED runtime=claude\n# LOOM_ACCOUNT name=stale\n\
==== loom-daemon dispatch: new sweep_id=new ====\n\
# LOOM_RUNTIME_RESOLVED runtime=codex\n# LOOM_ACCOUNT name=codex-profile\n";
        assert_eq!(parse_runtime_after(log, "sweep_id=new").as_deref(), Some("codex"));
        assert_eq!(parse_token_name_after(log, "sweep_id=new").as_deref(), Some("codex-profile"));
    }

    #[test]
    fn poll_observability_waits_for_current_account_after_stale_cli_start() {
        let dir = tempdir().unwrap();
        let log_path = dir.path().join("sweep.log");
        let header_anchor = "sweep_id=current";
        std::fs::write(
            &log_path,
            "==== loom-daemon dispatch: old sweep_id=old ====\n\
# LOOM_CLI_START runtime=codex\n\
==== loom-daemon dispatch: new sweep_id=current ====\n\
# LOOM_RUNTIME_RESOLVED runtime=codex\n",
        )
        .unwrap();

        let append_path = log_path.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            use std::io::Write;
            let mut log = std::fs::OpenOptions::new()
                .append(true)
                .open(append_path)
                .unwrap();
            writeln!(log, "# LOOM_ACCOUNT name=current-profile").unwrap();
        });
        let mut child = Command::new("sleep").arg("1").spawn().unwrap();

        let observed = poll_observability(&mut child, &log_path, header_anchor);

        child.kill().ok();
        child.wait().ok();
        writer.join().unwrap();
        assert_eq!(observed, ("current-profile".to_string(), "codex".to_string()));
    }

    #[test]
    fn parse_token_name_after_ignores_stale_line_before_current_header() {
        // A previous dispatch's selection line, then this dispatch's header
        // with NO selection line yet: must NOT return the stale name.
        let log = "\
==== loom-daemon dispatch: old sweep_id=sweep-issue-3780-OLD issue=3780 ====
spawn-claude: using OAuth account 'stale-account' (mode=random)
==== loom-daemon dispatch: new sweep_id=sweep-issue-3780-NEW issue=3780 ====
";
        assert_eq!(parse_token_name_after(log, "sweep_id=sweep-issue-3780-NEW"), None,);
        // Once the current child logs, the current selection wins.
        let log2 =
            format!("{log}spawn-claude: using OAuth account 'fresh-account' (mode=ranking)\n");
        assert_eq!(
            parse_token_name_after(&log2, "sweep_id=sweep-issue-3780-NEW").as_deref(),
            Some("fresh-account"),
        );
    }

    #[test]
    fn parse_token_name_after_none_when_marker_absent_or_empty() {
        // No marker at all.
        assert_eq!(parse_token_name_after("no selection here", "sweep_id=x"), None);
        // Header present but no marker.
        assert_eq!(
            parse_token_name_after("sweep_id=x issue=1 ====\nsome other line\n", "sweep_id=x"),
            None,
        );
        // Empty account name is treated as "nothing to report".
        assert_eq!(
            parse_token_name_after("sweep_id=x using OAuth account '' (mode=random)", "sweep_id=x"),
            None,
        );
    }

    // --- log_has_progress probe ---

    #[test]
    fn log_has_progress_false_for_header_and_wrapper_only() {
        // The hung case: only the daemon header + spawn-claude wrapper lines.
        let log = "\n==== loom-daemon dispatch: 2026-07-23T00:00:00Z sweep_id=sweep-issue-1-1 issue=1 ====\n\
                   [2026-07-23T00:00:01Z] spawn-claude: model=default\n\
                   [2026-07-23T00:00:01Z] spawn-claude: using OAuth account 'agent-2' (mode=ranked)\n";
        assert!(!log_has_progress(log));
    }

    #[test]
    fn log_has_progress_ignores_codex_adapter_preamble() {
        let log = "==== loom-daemon dispatch: t sweep_id=s issue=7 ====\n\
# LOOM_RUNTIME_RESOLVED runtime=codex\n\
[ts] spawn-worker: runtime=codex\n\
[ts] spawn-codex: model=default\n\
# LOOM_ACCOUNT name=profile-a\n";
        assert!(!log_has_progress(log));
        assert!(log_has_progress(&format!("{log}# LOOM_CLI_START runtime=codex\n")));
    }

    #[test]
    fn log_has_progress_true_when_claude_emits_output() {
        let log = "\n==== loom-daemon dispatch: 2026-07-23T00:00:00Z sweep_id=sweep-issue-1-1 issue=1 ====\n\
                   [2026-07-23T00:00:01Z] spawn-claude: using OAuth account 'agent-2' (mode=ranked)\n\
                   Stage 0: resolving backend...\n";
        assert!(log_has_progress(log));
    }

    #[test]
    fn log_has_progress_anchors_to_last_dispatch() {
        // A prior run produced output; the CURRENT dispatch (after the last
        // header) has none ⇒ no progress for this dispatch.
        let log = "==== loom-daemon dispatch: t1 sweep_id=a issue=1 ====\n\
                   Curator done\n\
                   ==== loom-daemon dispatch: t2 sweep_id=b issue=1 ====\n\
                   [ts] spawn-claude: using OAuth account 'x' (mode=random)\n";
        assert!(!log_has_progress(log));
    }

    #[test]
    fn log_has_progress_empty_log_is_false() {
        assert!(!log_has_progress(""));
    }

    // --- #4449 helpers: truncate_lines / describe_worktree_use ---

    #[test]
    fn truncate_lines_keeps_short_input_and_caps_long_input() {
        assert_eq!(truncate_lines("", 5), "");
        assert_eq!(truncate_lines("   \n  ", 5), "");
        assert_eq!(truncate_lines("a\nb", 5), "a\nb");
        let long = (1..=10)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let capped = truncate_lines(&long, 3);
        assert!(capped.starts_with("1\n2\n3"), "keeps the first `max` lines: {capped}");
        assert!(capped.contains("+7 more"), "reports how many lines were dropped: {capped}");
    }

    #[test]
    fn describe_worktree_use_joins_every_signal() {
        let described = describe_worktree_use(&[
            WorktreeUseEvidence::InUseMarker {
                task_id: "t1".into(),
                pid: "42".into(),
            },
            WorktreeUseEvidence::LiveProcesses(vec![7, 9]),
        ]);
        assert!(described.contains(".loom-in-use"), "{described}");
        assert!(described.contains("pid: 42"), "{described}");
        assert!(described.contains("[7, 9]"), "{described}");
        assert!(described.contains("; "), "signals are joined: {described}");
        assert_eq!(describe_worktree_use(&[]), "");
    }

    // --- ranking_has_capacity token-health gate ---

    #[test]
    fn ranking_has_capacity_missing_or_empty_degrades_to_proceed() {
        assert!(ranking_has_capacity(""), "empty ranking degrades to proceed");
        assert!(ranking_has_capacity("   \n  \n"), "all-blank ranking degrades to proceed");
        assert!(
            ranking_has_capacity("garbage-with-no-pipe\nmore garbage"),
            "unparseable ranking degrades to proceed"
        );
    }

    #[test]
    fn ranking_has_capacity_at_least_one_healthy_is_true() {
        assert!(ranking_has_capacity("a|available\nb|exhausted\nc|blocked"));
        // rate_limited is transient (not exhausted/blocked) ⇒ still eligible,
        // matching the spawn-time selector's "first non-exhausted/non-blocked".
        assert!(ranking_has_capacity("a|rate_limited"));
    }

    #[test]
    fn ranking_has_capacity_all_exhausted_or_blocked_is_false() {
        assert!(!ranking_has_capacity("a|exhausted\nb|blocked"));
        // Case-insensitive on the status token.
        assert!(!ranking_has_capacity("a|EXHAUSTED\nb|Blocked"));
        // A blank-name line is skipped; the remaining entry is exhausted.
        assert!(!ranking_has_capacity("|available\na|exhausted"));
    }
}
