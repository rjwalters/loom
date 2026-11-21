//! Local diagnostics for attended issue renewal. The receipt has no authority
//! over the lease. An unavailable sink is intentionally a no-op.

use super::{Key, KeyArgs, Store};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::ffi::{CStr, CString};
use std::fs::{File, Metadata};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
use std::path::{Component, Path};

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
        let Ok(directory) = ReceiptDirectory::open(&root) else {
            return;
        };
        let _ = append(
            &directory,
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

fn is_owner(metadata: &Metadata) -> bool {
    // SAFETY: geteuid only reads the process's own effective identity.
    metadata.uid() == unsafe { libc::geteuid() }
}

fn open_at(directory: &File, name: &CStr, flags: libc::c_int) -> io::Result<File> {
    // SAFETY: the directory descriptor and NUL-terminated name live through
    // the call. A successful open returns a new descriptor owned by this File.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600 as libc::c_int,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful open returned this descriptor to the caller.
    Ok(unsafe { File::from_raw_fd(fd) })
}

struct ReceiptDirectory(File);

impl ReceiptDirectory {
    fn open(root: &Path) -> io::Result<Self> {
        if !root.is_absolute() {
            return Err(io::Error::other("lease receipt root is not absolute"));
        }
        let mut directory = File::open("/")?;
        for component in root.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => {
                    directory =
                        open_at(&directory, &CString::new(name.as_bytes())?, libc::O_DIRECTORY)?;
                }
                _ => return Err(io::Error::other("lease receipt root is not canonical")),
            }
        }
        for name in [c".loom", c"logs", c"lease-ensure"] {
            directory = match open_at(&directory, name, libc::O_DIRECTORY) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    // SAFETY: this is a fixed child name of the bound directory.
                    let result =
                        unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) };
                    if result != 0 {
                        let error = io::Error::last_os_error();
                        if error.kind() != io::ErrorKind::AlreadyExists {
                            return Err(error);
                        }
                    }
                    open_at(&directory, name, libc::O_DIRECTORY)?
                }
                result => result?,
            };
            let metadata = directory.metadata()?;
            if !is_owner(&metadata) || metadata.mode() & 0o022 != 0 {
                return Err(io::Error::other(
                    "lease receipt directory is writable by another user",
                ));
            }
        }
        Ok(Self(directory))
    }

    fn private_file(&self, name: &CStr, flags: libc::c_int) -> io::Result<File> {
        let file = open_at(&self.0, name, flags | libc::O_NONBLOCK)?;
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || !is_owner(&metadata)
            || metadata.mode() & 0o077 != 0
            || metadata.nlink() != 1
        {
            return Err(io::Error::other("lease receipt path is not a private single-link file"));
        }
        Ok(file)
    }

    fn rotate(&self, name: &CStr, basename: &str) -> io::Result<()> {
        let files = (1..=KEEP_ROTATIONS)
            .map(|index| CString::new(format!("{basename}.{index}")))
            .collect::<Result<Vec<_>, _>>()?;
        for file in &files {
            allow_missing(self.private_file(file, libc::O_RDONLY).map(drop))?;
        }
        let fd = self.0.as_raw_fd();
        // SAFETY: the oldest filename and bound directory live through unlinkat.
        allow_missing(syscall_result(unsafe {
            libc::unlinkat(fd, files[KEEP_ROTATIONS - 1].as_ptr(), 0)
        }))?;
        for pair in files.windows(2).rev() {
            // SAFETY: both filenames resolve inside the same bound directory.
            allow_missing(syscall_result(unsafe {
                libc::renameat(fd, pair[0].as_ptr(), fd, pair[1].as_ptr())
            }))?;
        }
        // SAFETY: both filenames resolve inside the same bound directory.
        syscall_result(unsafe { libc::renameat(fd, name.as_ptr(), fd, files[0].as_ptr()) })
    }
}

fn syscall_result(result: libc::c_int) -> io::Result<()> {
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn allow_missing(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

fn append(directory: &ReceiptDirectory, receipt: &Receipt) -> anyhow::Result<()> {
    let basename = format!("issue-{}.terminal.jsonl", receipt.issue);
    let name = CString::new(basename.as_bytes())?;
    let lock_name = CString::new(format!("issue-{}.terminal.lock", receipt.issue))?;
    let append = libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND;
    let lock = directory.private_file(&lock_name, append)?;
    // SAFETY: the lock file owns the descriptor for the duration of the append.
    // A contended diagnostic can be dropped without delaying lease termination.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    let file = directory.private_file(&name, append)?;
    let mut file = if file.metadata()?.len() >= MAX_LOG_BYTES {
        drop(file);
        directory.rotate(&name, &basename)?;
        directory.private_file(&name, append)?
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
    use std::os::unix::fs::{symlink, PermissionsExt};

    const LOG_DIRECTORY: &str = ".loom/logs/lease-ensure";
    const LOG_NAME: &str = "issue-7.terminal.jsonl";
    const APPEND: libc::c_int = libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND;

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
        let root = temp.path().canonicalize().unwrap();
        let directory = ReceiptDirectory::open(&root).unwrap();
        let path = root.join(LOG_DIRECTORY).join(LOG_NAME);
        directory
            .private_file(c"issue-7.terminal.jsonl", APPEND)
            .unwrap()
            .set_len(MAX_LOG_BYTES)
            .unwrap();
        append(&directory, &fixture()).unwrap();

        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["reason"], "watch-ended");
        assert_eq!(value["issue"], 7);
        assert!(value.get("last_success").is_none());
        assert!(value.get("last_attempt").is_none());
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(
            std::fs::metadata(root.join(LOG_DIRECTORY).join("issue-7.terminal.jsonl.1"))
                .unwrap()
                .len(),
            MAX_LOG_BYTES,
        );
    }

    #[test]
    fn refuses_a_shared_sink_without_appending() {
        let temp = tempfile::tempdir().unwrap();
        let directory = ReceiptDirectory::open(&temp.path().canonicalize().unwrap()).unwrap();
        let file = directory
            .private_file(c"issue-7.terminal.jsonl", APPEND)
            .unwrap();
        file.set_permissions(std::fs::Permissions::from_mode(0o644))
            .unwrap();
        assert!(append(&directory, &fixture()).is_err());
        assert_eq!(file.metadata().unwrap().len(), 0);
    }

    #[test]
    fn refuses_each_linked_directory_component() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let outside = root.join("sibling");
        std::fs::create_dir(&outside).unwrap();
        for name in [".loom", ".loom/logs", LOG_DIRECTORY] {
            let link = root.join(name);
            symlink(&outside, &link).unwrap();
            assert!(ReceiptDirectory::open(&root).is_err(), "{name}");
            assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
            std::fs::remove_file(&link).unwrap();
            std::fs::create_dir(&link).unwrap();
        }
    }

    #[test]
    fn refuses_a_multiply_linked_leaf_or_rotation() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let directory = ReceiptDirectory::open(&root).unwrap();
        let sink = root.join(LOG_DIRECTORY);
        let outside = root.join("untouched");
        std::fs::write(&outside, b"inert\n").unwrap();
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o600)).unwrap();
        let current = sink.join(LOG_NAME);
        std::fs::hard_link(&outside, &current).unwrap();
        assert!(append(&directory, &fixture()).is_err());
        std::fs::remove_file(&current).unwrap();
        let active = directory
            .private_file(c"issue-7.terminal.jsonl", APPEND)
            .unwrap();
        active.set_len(MAX_LOG_BYTES).unwrap();
        std::fs::hard_link(&outside, sink.join("issue-7.terminal.jsonl.1")).unwrap();
        assert!(append(&directory, &fixture()).is_err());
        assert_eq!(active.metadata().unwrap().len(), MAX_LOG_BYTES);
        assert_eq!(std::fs::read(&outside).unwrap(), b"inert\n");
    }

    #[test]
    fn binds_lock_append_and_rotation_to_the_open_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let directory = ReceiptDirectory::open(&root).unwrap();
        directory
            .private_file(c"issue-7.terminal.jsonl", APPEND)
            .unwrap()
            .set_len(MAX_LOG_BYTES)
            .unwrap();
        let moved = root.join("moved");
        let outside = root.join("sibling");
        std::fs::create_dir(&outside).unwrap();
        std::fs::rename(root.join(LOG_DIRECTORY), &moved).unwrap();
        symlink(&outside, root.join(LOG_DIRECTORY)).unwrap();
        append(&directory, &fixture()).unwrap();
        assert!(moved.join(LOG_NAME).is_file());
        assert!(moved.join("issue-7.terminal.lock").is_file());
        assert!(moved.join("issue-7.terminal.jsonl.1").is_file());
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    }
}
