//! The one hygiene answer remembered across passes (W6 PR2): **a pull
//! request that merged**.
//!
//! A merged PR stays merged, so a kept `pr-<N>` worktree whose PR merged
//! (dirty, or still inside its grace period) need not be re-read on every
//! reaper tick. Nothing else is terminal:
//!
//! - a CLOSED issue or a closed-without-merge PR can be reopened;
//! - OPEN is the state everything leaves;
//! - "unknown", "gone" and every error are not answers at all.
//!
//! So [`record`] stores a `Merged` answer and refuses every other one, and
//! [`lookup`] can only ever say "merged".
//!
//! # Never a licence to remove
//!
//! An entry is a file in the per-user store, which any process of the same
//! uid can write ([`super::forge_state`], "Trust boundary"). It therefore
//! only saves the *discovery* read. Every removal re-reads the PR from the
//! forge, unconditionally, and keeps the worktree unless that answer agrees
//! ([`super::hygiene_pass::Pass::confirm_pull`]).
//!
//! # Scope
//!
//! One entry per `(forge host, repository identity, credential, PR number)`.
//! The repository identity is the forge's numeric repo id when the repo-facts
//! record carries one, so a renamed or transferred repository keeps its own
//! entries and a new repository that takes over an old name never reads
//! them; without an id it is the canonical `owner/name`, in a separate
//! namespace. A root with no repo fact (the placeholder path) has no
//! identity and is never cached. The credential component follows ADR-0021:
//! a persisted answer is never served to another identity.
//!
//! Entries live in the shared store directory under their own
//! `hygiene-merged-` prefix, which no earlier daemon version reads, prunes
//! or invalidates. In test builds the store is off unless the test thread
//! opts in ([`store::set_test_daemon_store_dir`]).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::forge_etag_store as store;
use crate::forge_repo_facts::Fact;

use super::clean::PrStatus;
use super::forge_state::{self, PullFacts, Where};

/// Filename prefix of a terminal entry in the shared store directory.
pub(crate) const PREFIX: &str = "hygiene-merged-";

/// Entries not rewritten for this long are dropped by the next [`record`],
/// which bounds the store: worktrees of long-merged PRs are removed, and
/// their entries would otherwise stay forever. A dropped entry costs one
/// read.
const MAX_AGE: Duration = Duration::from_secs(30 * 24 * 3600);

/// The entry format. A file of another version is ignored.
const VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Entry {
    v: u32,
    /// [`repo_scope`] of the repository the answer belongs to.
    repo: String,
    pr: u32,
    merged_at: String,
    #[serde(default)]
    head_sha: Option<String>,
}

/// `<host>#id:<repo id>` when the record knows the id, else
/// `<host>#name:<owner/name>` (lowercased).
pub(crate) fn repo_scope(fact: &Fact) -> String {
    let host = fact.host.to_ascii_lowercase();
    match fact.repo_id {
        Some(id) => format!("{host}#id:{id}"),
        None => format!("{host}#name:{}", fact.full_name().to_ascii_lowercase()),
    }
}

/// The entry path and the scope it must carry, or `None` when the root has
/// no repository identity or no store.
fn locate(root: &Path, pr: u32) -> Option<(PathBuf, String)> {
    let Where::Repo(fact) = forge_state::resolve(root) else {
        return None;
    };
    let dir = store::daemon_store_dir()?;
    let scope = repo_scope(&fact);
    let credential = store::credential_scope(Some(root), &forge_state::target_of(&fact));
    let key = format!("{scope}#{credential}#pr:{pr}");
    Some((store::entry_path_with_prefix(&dir, PREFIX, &key), scope))
}

/// The remembered merge of PR `pr` in `root`'s repository, if there is one.
/// Only ever `Merged`; a missing, unreadable, foreign or malformed entry is
/// `None` (read the forge).
pub(crate) fn lookup(root: &Path, pr: u32) -> Option<PullFacts> {
    let (path, scope) = locate(root, pr)?;
    let entry: Entry = store::read_private_json(&path)?;
    let ours = entry.v == VERSION && entry.repo == scope && entry.pr == pr;
    if !ours || entry.merged_at.trim().is_empty() {
        return None;
    }
    Some(PullFacts {
        status: PrStatus::Merged {
            merged_at: entry.merged_at,
        },
        head_sha: entry.head_sha,
    })
}

/// Remember `facts` for PR `pr` — only when it is `Merged`. Every other
/// status is refused: nothing but a merge is terminal.
pub(crate) fn record(root: &Path, pr: u32, facts: &PullFacts) {
    let PrStatus::Merged { merged_at } = &facts.status else {
        return;
    };
    if merged_at.trim().is_empty() {
        return;
    }
    let Some((path, scope)) = locate(root, pr) else {
        return;
    };
    let entry = Entry {
        v: VERSION,
        repo: scope,
        pr,
        merged_at: merged_at.clone(),
        head_sha: facts.head_sha.clone(),
    };
    store::write_private_json(&path, &entry);
    if let Some(dir) = path.parent() {
        prune(dir);
    }
}

/// Drop the entry for PR `pr`: a fresh read disagreed with it.
pub(crate) fn forget(root: &Path, pr: u32) {
    if let Some((path, _)) = locate(root, pr) {
        let _ = std::fs::remove_file(path);
    }
}

/// Remove terminal entries last written more than [`MAX_AGE`] ago.
fn prune(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with(PREFIX) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > MAX_AGE);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}
