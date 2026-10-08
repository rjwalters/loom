//! The per-host append-only sink files of [`super`] (moved out of the
//! parent in #10607 so it stays small): one `calls-<epoch-hour>.jsonl` per
//! hour, owner-only, pruned past [`super::RETAIN_HOURS`].

use std::io::Write;
use std::path::{Path, PathBuf};

use super::{SinkLine, RETAIN_HOURS};

pub(super) fn sink_file(dir: &Path, hour: i64) -> PathBuf {
    dir.join(format!("calls-{hour}.jsonl"))
}

pub(super) fn append(dir: &Path, line: &SinkLine) -> std::io::Result<()> {
    // Same owner-only rules as the ETag store: a 0700 dir we own, 0600 files.
    if !crate::forge_etag_store::private_dir(dir, true) {
        return Err(std::io::Error::other("untrusted sink dir"));
    }
    let mut buf = serde_json::to_vec(line)?;
    buf.push(b'\n');
    let hour = line.t.div_euclid(3600);
    let path = sink_file(dir, hour);
    let mut create = std::fs::OpenOptions::new();
    create.append(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut create, 0o600);
    let (mut file, fresh) = match create.open(&path) {
        Ok(f) => (f, true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            (std::fs::OpenOptions::new().append(true).open(&path)?, false)
        }
        Err(e) => return Err(e),
    };
    // One write of one short line: atomic under O_APPEND.
    file.write_all(&buf)?;
    if fresh {
        prune(dir, hour);
    }
    Ok(())
}

/// Remove sink files more than [`RETAIN_HOURS`] older than `current_hour`.
fn prune(dir: &Path, current_hour: i64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let hour = name
            .to_str()
            .and_then(|n| n.strip_prefix("calls-"))
            .and_then(|n| n.strip_suffix(".jsonl"))
            .and_then(|n| n.parse::<i64>().ok());
        if hour.is_some_and(|h| h < current_hour - RETAIN_HOURS) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The raw sink text of every hour file covering `since..=now`.
pub(super) fn read_since(dir: &Path, since: i64, now: i64) -> String {
    let mut raw = String::new();
    for hour in since.div_euclid(3600)..=now.div_euclid(3600) {
        if let Ok(text) = std::fs::read_to_string(sink_file(dir, hour)) {
            raw.push_str(&text);
        }
    }
    raw
}
