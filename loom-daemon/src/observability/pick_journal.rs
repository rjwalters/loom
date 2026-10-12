//! The per-tick pick journal (#10432, follow-up to #10212): what a role agent
//! actually ranked and did, captured at the seams it already goes through, so
//! the tick's `pick.decision` carries the **serving** queue and the **actual**
//! decisions instead of the daemon's admission-gate listing.
//!
//! The daemon creates one journal file per launched role tick ([`attach`]) and
//! hands its path to the agent as [`PICK_JOURNAL_ENV`]. Three writers, all in
//! the agent's own process tree, append JSON lines to it:
//!
//! - `loom-daemon pr-queue` ([`record_pr_queue`]): the ordered queue Judge,
//!   Doctor and Champion consume, with its sort keys — the same rows it prints.
//! - the agent `gh` front's served `issue|pr list` ([`record_listing`]): the
//!   rows a listing-driven role (Curator) read, in listing order, after its own
//!   `--jq` filter.
//! - the agent `gh` front's argv ([`record_gh_actions`]): the forge writes the
//!   agent issued (claim labels, verdict labels, merges), parsed from argv only.
//!
//! **No forge calls are added**: every writer records data its caller already
//! fetched or an argv it is about to run. At the end of the tick
//! [`take`] reads the file and deletes it. Outside a role tick
//! ([`PICK_JOURNAL_ENV`] unset) every writer is a no-op, and every write is
//! best-effort: a journal failure never changes what the caller prints or runs.

use std::cell::RefCell;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::telemetry::kinds::pick_decision::{PickSkipReason, PickSortKey};

/// The journal path the daemon hands to a role agent.
pub const PICK_JOURNAL_ENV: &str = "LOOM_PICK_JOURNAL";

/// Test override for the journal directory.
pub const PICK_JOURNAL_DIR_ENV: &str = "LOOM_PICK_JOURNAL_DIR";

/// A journal stops growing past this many bytes.
const MAX_JOURNAL_BYTES: u64 = 1 << 20;

/// Rows kept per journal line.
const MAX_ROWS_PER_ENTRY: usize = 200;

/// Leftover journals (a crashed tick) older than this are pruned.
const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

/// One candidate row as the role saw it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalRow {
    pub number: u32,
    /// The stage label the row was served under.
    pub stage: String,
    /// The row's labels (for label-derived skip reasons).
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort_key: Option<PickSortKey>,
}

/// One journal line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JournalEntry {
    /// A `pr-queue` result: the role's ordered queue.
    Queue {
        at: DateTime<Utc>,
        role: String,
        /// Whether the agent's `gh` resolves to the agent `gh` front, so its
        /// forge writes are journaled too.
        acts_observable: bool,
        /// Rows in the queue before `rows` was capped; `0` on a line written
        /// before the field existed (read as `rows.len()`).
        #[serde(default)]
        total: usize,
        rows: Vec<JournalRow>,
    },
    /// A listing the agent `gh` front served.
    Listing {
        at: DateTime<Utc>,
        rows: Vec<JournalRow>,
    },
    /// A forge write the agent issued.
    Act {
        at: DateTime<Utc>,
        number: u32,
        action: String,
    },
}

// ---------------------------------------------------------------------------
// Daemon side
// ---------------------------------------------------------------------------

thread_local! {
    /// `(root, journal path)` of the tick running on this blocking thread.
    static ACTIVE: RefCell<Option<(PathBuf, PathBuf)>> = const { RefCell::new(None) };
}

fn journal_dir() -> PathBuf {
    std::env::var(PICK_JOURNAL_DIR_ENV)
        .ok()
        .filter(|d| !d.is_empty())
        .map_or_else(
            || crate::forge_etag_store::host_tmp_base().join("loom-pick-journal"),
            PathBuf::from,
        )
}

/// Would a role agent's `gh` reach the agent `gh` front (the launch prepends
/// it unless opted out, and only when this process is `loom-daemon`)?
fn front_expected() -> bool {
    std::env::var(crate::agent_gh::OPT_OUT_ENV).map_or(true, |v| v != "0")
        && std::env::current_exe()
            .ok()
            .is_some_and(|exe| exe.file_name() == Some(std::ffi::OsStr::new("loom-daemon")))
}

/// Give `command` (a role agent about to launch for `role` in `root`) a
/// journal. Idempotent within a tick: a retried launch reuses the same file.
/// A no-op without an OTLP exporter.
pub fn attach(command: &mut Command, root: &Path, role: &str) {
    if super::ops::global_ops_sink().is_some() {
        attach_in(command, root, role, &journal_dir());
    }
}

fn attach_in(command: &mut Command, root: &Path, role: &str, dir: &Path) {
    let existing = ACTIVE.with(|a| {
        a.borrow()
            .as_ref()
            .filter(|(r, _)| r == root)
            .map(|(_, p)| p.clone())
    });
    let path = if let Some(path) = existing {
        path
    } else {
        if !crate::forge_etag_store::private_dir(dir, true) {
            return;
        }
        prune(dir);
        let role: String = role
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        let path = dir.join(format!("{role}-{}.jsonl", uuid::Uuid::new_v4()));
        ACTIVE.with(|a| *a.borrow_mut() = Some((root.to_path_buf(), path.clone())));
        path
    };
    command.env(PICK_JOURNAL_ENV, &path);
}

fn prune(dir: &Path) {
    let now = std::time::SystemTime::now();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age > STALE_AFTER);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Drop this thread's journal (start of a new tick), deleting its file.
pub fn discard() {
    if let Some((_, path)) = ACTIVE.with(|a| a.borrow_mut().take()) {
        let _ = std::fs::remove_file(path);
    }
}

/// What a finished tick's journal held.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Journal {
    pub entries: Vec<JournalEntry>,
    /// The agent `gh` front was expected on the agent's `PATH`.
    pub front_expected: bool,
    /// The journal file was read. `false` when it was missing or unreadable:
    /// `entries` is then empty because nothing was observed, not because the
    /// agent did nothing. A file that exists and is empty is `true`.
    pub read: bool,
}

/// Drain this thread's journal for `root`: `None` when no journal was
/// attached (the agent never launched, or no exporter). The file is deleted.
pub fn take(root: &Path) -> Option<Journal> {
    let (owner, path) = ACTIVE.with(|a| a.borrow_mut().take())?;
    let raw = std::fs::read_to_string(&path);
    let _ = std::fs::remove_file(&path);
    if owner != root {
        return None;
    }
    Some(Journal {
        read: raw.is_ok(),
        entries: parse(raw.as_deref().unwrap_or_default()),
        front_expected: front_expected(),
    })
}

/// Parse journal lines, skipping any that do not decode.
#[must_use]
pub fn parse(raw: &str) -> Vec<JournalEntry> {
    raw.lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

// ---------------------------------------------------------------------------
// Writer side (the agent's process tree)
// ---------------------------------------------------------------------------

fn journal_path() -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var_os(PICK_JOURNAL_ENV)?);
    path.is_absolute().then_some(path)
}

fn append(entry: &JournalEntry) {
    if let Some(path) = journal_path() {
        append_to(&path, entry);
    }
}

fn append_to(path: &Path, entry: &JournalEntry) {
    if std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_JOURNAL_BYTES) {
        return;
    }
    let Ok(mut line) = serde_json::to_string(entry) else {
        return;
    };
    line.push('\n');
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    if let Ok(mut f) = options.open(path) {
        // One write per line: concurrent appenders never interleave a line.
        let _ = f.write_all(line.as_bytes());
    }
}

fn row_labels(row: &Value) -> Vec<String> {
    row["labels"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|l| l.as_str().or_else(|| l["name"].as_str()))
        .map(str::to_string)
        .collect()
}

fn text(v: &Value) -> String {
    v.as_str().map_or_else(|| v.to_string(), str::to_string)
}

/// The journal rows for a `pr-queue` result, in its order.
#[must_use]
pub fn pr_queue_rows(rows: &[Value]) -> Vec<JournalRow> {
    rows.iter()
        .filter_map(|row| {
            let number = u32::try_from(row["number"].as_u64()?).ok()?;
            let labels = row_labels(row);
            let stage = ["loom:review-requested", "loom:changes-requested", "loom:pr"]
                .into_iter()
                .find(|s| labels.iter().any(|l| l == s))
                .map_or_else(
                    || {
                        if row["mode"] == "fallback" {
                            "fallback".to_string()
                        } else {
                            "unlabeled".to_string()
                        }
                    },
                    str::to_string,
                );
            let sort_key = PickSortKey {
                name: "pr_queue".to_string(),
                value: format!(
                    "level={},reason={},origin={},mode={}",
                    text(&row["operatorPriorityLevel"]),
                    text(&row["priorityReason"]),
                    text(&row["origin"]),
                    text(&row["mode"]),
                ),
            };
            Some(JournalRow {
                number,
                stage,
                labels,
                sort_key: Some(sort_key),
            })
        })
        .take(MAX_ROWS_PER_ENTRY)
        .collect()
}

/// Is the first `gh` on `path` the agent `gh` front (a link to `loom-daemon`)?
#[must_use]
pub fn front_on_path(path: Option<OsString>) -> bool {
    let Some(path) = path else { return false };
    std::env::split_paths(&path)
        .map(|dir| dir.join("gh"))
        .find(|gh| gh.exists())
        .and_then(|gh| gh.canonicalize().ok())
        .is_some_and(|exe| exe.file_name() == Some(std::ffi::OsStr::new("loom-daemon")))
}

/// Journal a `pr-queue` result (the role's serving queue). A no-op outside a
/// role tick.
pub fn record_pr_queue(role: &str, rows: &[Value]) {
    if journal_path().is_none() {
        return;
    }
    append(&JournalEntry::Queue {
        at: Utc::now(),
        role: role.to_string(),
        acts_observable: front_on_path(std::env::var_os("PATH")),
        total: rows.len(),
        rows: pr_queue_rows(rows),
    });
}

/// The journal rows for a served listing: the rows its `--jq` prints, in the
/// order it prints them. `None` when that cannot be told (a projection that
/// does not name its rows): the listing is then unobserved, not guessed.
#[must_use]
pub fn listing_rows(listing: &crate::forge_cached_list::Served) -> Option<Vec<JournalRow>> {
    let keep = listing.surviving()?;
    Some(
        keep.into_iter()
            .filter_map(|i| listing.items.get(i))
            .enumerate()
            .map(|(position, item)| JournalRow {
                number: item.number,
                stage: listing.labels.clone(),
                labels: item.labels.clone(),
                sort_key: Some(PickSortKey {
                    name: "listing_order".to_string(),
                    value: (position + 1).to_string(),
                }),
            })
            .take(MAX_ROWS_PER_ENTRY)
            .collect(),
    )
}

/// Journal a listing the agent `gh` front served. A no-op outside a role tick
/// and for a listing whose printed order is unobserved.
pub fn record_listing(listing: &crate::forge_cached_list::Served) {
    if journal_path().is_none() {
        return;
    }
    let Some(rows) = listing_rows(listing) else {
        return;
    };
    append(&JournalEntry::Listing {
        at: Utc::now(),
        rows,
    });
}

/// Journal the forge writes a `gh` argv issues. A no-op outside a role tick.
pub fn record_gh_actions(raw: &[OsString]) {
    if journal_path().is_none() {
        return;
    }
    let args: Vec<String> = raw
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    for (number, action) in gh_actions(&args) {
        append(&JournalEntry::Act {
            at: Utc::now(),
            number,
            action: action.to_string(),
        });
    }
}

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

/// The role action a label write means (one of `ROLE_ACTIONS`), or `None`
/// for a label that is not a Loom workflow label.
#[must_use]
pub fn action_for_label(label: &str) -> Option<&'static str> {
    Some(match label {
        "loom:reviewing" | "loom:treating" | "loom:curating" | "loom:evaluating"
        | "loom:building" => "claimed",
        "loom:pr" => "approved",
        "loom:changes-requested" => "changes_requested",
        "loom:curated" => "curated",
        "loom:issue" => "promoted",
        "loom:blocked" => "blocked",
        "loom:operator" | "loom:operator-only" | "loom:operator-decision" => "escalated",
        l if l.starts_with("loom:") => "labeled",
        _ => return None,
    })
}

/// Map a role action string back to its `'static` name in `ROLE_ACTIONS`.
#[must_use]
pub fn known_action(action: &str) -> Option<&'static str> {
    crate::telemetry::kinds::pick_decision::ROLE_ACTIONS
        .into_iter()
        .find(|a| *a == action)
}

/// A label-derived reason a candidate was not acted on, if its labels give one.
#[must_use]
pub fn skip_reason_for_labels(labels: &[String]) -> Option<PickSkipReason> {
    let has = |set: &[&str]| labels.iter().any(|l| set.contains(&l.as_str()));
    if has(&[
        "loom:operator",
        "loom:operator-only",
        "loom:operator-decision",
        "loom:operator-mechanical",
    ]) {
        Some(PickSkipReason::OperatorHold)
    } else if has(&["loom:blocked", "loom:ci-failure"]) {
        Some(PickSkipReason::Blocked)
    } else if has(&["loom:sequenced"]) {
        Some(PickSkipReason::OverlapChain)
    } else if has(&[
        "loom:reviewing",
        "loom:treating",
        "loom:curating",
        "loom:evaluating",
        "loom:building",
    ]) {
        Some(PickSkipReason::InFlight)
    } else {
        None
    }
}

/// An issue/PR number from `N`, `#N` or a URL ending in `/N`.
fn number(arg: &str) -> Option<u32> {
    let tail = arg.rsplit('/').next().unwrap_or(arg);
    tail.trim_start_matches('#').parse().ok()
}

/// `gh` flags (of the shapes parsed here) that take no value.
const BOOLEAN_FLAGS: &[&str] = &[
    "--approve",
    "-a",
    "--request-changes",
    "-r",
    "--comment",
    "-c",
    "--remove-milestone",
    "--paginate",
    "--include",
    "-i",
    "--silent",
    "--verbose",
    "--slurp",
    "--squash",
    "--merge",
    "--rebase",
    "--auto",
    "--admin",
    "--delete-branch",
    "-d",
];

/// Split `args` into positionals and `(flag, value)` pairs.
fn split_flags(args: &[String]) -> (Vec<&str>, Vec<(&str, &str)>) {
    let (mut positional, mut flags) = (Vec::new(), Vec::new());
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if let Some((flag, value)) = a.split_once('=').filter(|_| a.starts_with("--")) {
            flags.push((flag, value));
        } else if a.starts_with('-') && a.len() > 1 {
            if BOOLEAN_FLAGS.contains(&a) {
                flags.push((a, ""));
            } else {
                flags.push((a, args.get(i + 1).map_or("", String::as_str)));
                i += 1;
            }
        } else {
            positional.push(a);
        }
        i += 1;
    }
    (positional, flags)
}

/// The `(number, action)` forge writes a `gh` argv issues: `issue|pr edit
/// --add-label`, `pr merge`, `pr review --approve|--request-changes`, and the
/// `gh api` label POST / merge PUT shapes the role prompts use.
#[must_use]
pub fn gh_actions(args: &[String]) -> Vec<(u32, &'static str)> {
    let Some(command) = args.first().map(String::as_str) else {
        return Vec::new();
    };
    let sub = args.get(1).map_or("", String::as_str);
    let (positional, flags) = split_flags(args.get(2..).unwrap_or_default());
    let target = positional.first().and_then(|p| number(p));
    let mut out = Vec::new();
    match (command, sub) {
        ("issue" | "pr", "edit") => {
            let Some(n) = target else { return out };
            for (_, value) in flags.iter().filter(|(f, _)| *f == "--add-label") {
                out.extend(
                    value
                        .split(',')
                        .filter_map(|l| action_for_label(l.trim()))
                        .map(|a| (n, a)),
                );
            }
        }
        ("pr", "merge") => out.extend(target.map(|n| (n, "merged"))),
        ("pr", "review") => {
            let Some(n) = target else { return out };
            if flags.iter().any(|(f, _)| *f == "--approve" || *f == "-a") {
                out.push((n, "approved"));
            } else if flags
                .iter()
                .any(|(f, _)| *f == "--request-changes" || *f == "-r")
            {
                out.push((n, "changes_requested"));
            }
        }
        ("api", _) => out.extend(api_actions(&args[1..])),
        _ => {}
    }
    out
}

fn api_actions(args: &[String]) -> Vec<(u32, &'static str)> {
    let (positional, flags) = split_flags(args);
    let Some(path) = positional.first() else {
        return Vec::new();
    };
    let method = flags
        .iter()
        .find(|(f, _)| *f == "-X" || *f == "--method")
        .map(|(_, m)| m.to_ascii_uppercase());
    let has_fields = flags
        .iter()
        .any(|(f, _)| matches!(*f, "-f" | "-F" | "--field" | "--raw-field"));
    let method = method.unwrap_or_else(|| if has_fields { "POST" } else { "GET" }.to_string());
    let path = path
        .split('?')
        .next()
        .unwrap_or_default()
        .trim_start_matches('/');
    let parts: Vec<&str> = path.split('/').collect();
    match parts.as_slice() {
        ["repos", _, _, "issues", n, "labels"] if method == "POST" => {
            let Some(n) = number(n) else {
                return Vec::new();
            };
            flags
                .iter()
                .filter(|(f, _)| matches!(*f, "-f" | "-F" | "--field" | "--raw-field"))
                .filter_map(|(_, kv)| kv.split_once('='))
                .filter(|(k, _)| *k == "labels[]" || *k == "labels")
                .filter_map(|(_, label)| action_for_label(label.trim()))
                .map(|a| (n, a))
                .collect()
        }
        ["repos", _, _, "pulls", n, "merge"] if method == "PUT" => {
            number(n).map(|n| vec![(n, "merged")]).unwrap_or_default()
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "pick_journal_tests.rs"]
mod tests;
