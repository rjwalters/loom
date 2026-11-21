//! Local diagnostics for attended issue renewal. The receipt has no authority
//! over the lease. An unavailable sink is intentionally a no-op.

use super::{Key, KeyArgs, Store};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

const MAX_LOG_BYTES: u64 = 64 * 1024;
const KEEP_ROTATIONS: usize = 2;

#[derive(Clone, Copy, clap::ValueEnum, Serialize)]
#[serde(rename_all = "kebab-case")]
enum Reason {
    WatchEnded,
    AgeCap,
    Closed,
    Released,
    Superseded,
    Yielded,
    NoLease,
}

#[derive(clap::Args)]
pub(crate) struct TerminalReceiptArgs {
    #[command(flatten)]
    pub(super) key: KeyArgs,
    #[arg(long)]
    token: String,
    #[arg(long)]
    reason: Reason,
    #[arg(long)]
    seen_owner: bool,
    #[arg(long)]
    last_success: Option<DateTime<Utc>>,
    #[arg(long)]
    renewal_failed: bool,
}

#[derive(Serialize)]
struct Receipt {
    issue: u64,
    sweep_fingerprint: String,
    observed_at: DateTime<Utc>,
    reason: Reason,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_success: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_attempt: Option<&'static str>,
}

impl TerminalReceiptArgs {
    pub(super) fn record(&self, store: &Store, key: &Key, cwd: &Path) {
        // A duplicate may stop before it ever owns the key. A prior positive
        // ownership check also covers a loop since superseded by its successor.
        let owned_now = store
            .read(key)
            .ok()
            .flatten()
            .is_some_and(|record| record.token == self.token);
        if !(owned_now || self.seen_owner) || key.sweep.is_empty() {
            return;
        }
        let Some(workspace) = cwd.to_str() else {
            return;
        };
        let Ok(root) = crate::repo_root::resolve_repo_root(workspace) else {
            return;
        };
        let directory = root.join(".loom/logs/lease-ensure");
        let _ = append(
            &directory.join(format!("issue-{}.terminal.jsonl", key.issue)),
            &Receipt {
                issue: key.issue,
                sweep_fingerprint: hex::encode(Sha256::digest(key.sweep.as_bytes())),
                observed_at: Utc::now(),
                reason: self.reason,
                last_success: self.last_success,
                last_attempt: self.renewal_failed.then_some("renewal-failed"),
            },
        );
    }
}

fn open_private(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    // SAFETY: geteuid only reads the process's own effective identity.
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::other("lease receipt path is not an owner-only file"));
    }
    Ok(file)
}

fn append(path: &Path, receipt: &Receipt) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock = open_private(&path.with_extension("lock"))?;
    // SAFETY: the lock file owns the descriptor for the duration of the append.
    // A contended diagnostic can be dropped without delaying lease termination.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    let file = open_private(path)?;
    let mut file = if file.metadata()?.len() >= MAX_LOG_BYTES {
        drop(file);
        crate::rotate_log_file(path, MAX_LOG_BYTES, KEEP_ROTATIONS)?;
        open_private(path)?
    } else {
        file
    };
    let mut row = serde_json::to_vec(receipt)?;
    row.push(b'\n');
    file.write_all(&row)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn fixture() -> Receipt {
        Receipt {
            issue: 7,
            sweep_fingerprint: hex::encode(Sha256::digest(b"fixture-sweep")),
            observed_at: Utc::now(),
            reason: Reason::WatchEnded,
            last_success: None,
            last_attempt: None,
        }
    }

    #[test]
    fn rotates_an_owner_only_receipt_and_omits_unobserved_values() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("issue-7.terminal.jsonl");
        open_private(&path).unwrap().set_len(MAX_LOG_BYTES).unwrap();
        append(&path, &fixture()).unwrap();

        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["reason"], "watch-ended");
        assert_eq!(value["issue"], 7);
        assert!(value.get("last_success").is_none());
        assert!(value.get("last_attempt").is_none());
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(
            std::fs::metadata(temp.path().join("issue-7.terminal.jsonl.1"))
                .unwrap()
                .len(),
            MAX_LOG_BYTES,
        );
    }

    #[test]
    fn refuses_a_shared_sink_without_appending() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("issue-7.terminal.jsonl");
        let file = open_private(&path).unwrap();
        file.set_permissions(std::fs::Permissions::from_mode(0o644))
            .unwrap();
        assert!(append(&path, &fixture()).is_err());
        assert_eq!(file.metadata().unwrap().len(), 0);
    }
}
