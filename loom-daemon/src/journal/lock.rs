//! The per-stream writer lock and the repairs only its holder may make.
//!
//! The lock is the stable file `<stream>/.lock` (mode 0600), never rotated or
//! deleted — see the module docs for why it is not the active segment.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use super::envelope::decode;
use super::segment::{self, Line, LineReader, Segment};
use super::{sync_dir, JournalError};

pub(crate) const LOCK_FILE: &str = ".lock";

/// Proof of holding a stream's writer lock; released on drop.
#[derive(Debug)]
pub struct StreamLock {
    _file: File,
}

impl StreamLock {
    /// Create the stream directory if needed (durably), then take the lock,
    /// retrying until `retry` elapses.
    pub(crate) fn acquire(dir: &Path, retry: Duration) -> Result<Self, JournalError> {
        ensure_dir(dir)?;
        let mut options = OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(dir.join(LOCK_FILE))?;
        let deadline = Instant::now() + retry;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(StreamLock { _file: file }),
                Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    log::warn!("journal: lock retry budget exhausted for {}", dir.display());
                    return Err(JournalError::Busy);
                }
                Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
            }
        }
    }

    /// Truncate `path` to its last newline (a crashed append's torn tail),
    /// then sync. Returns whether anything was cut. Only the lock holder
    /// may call this; readers never do.
    pub(crate) fn repair_tail(&self, path: &Path) -> io::Result<bool> {
        let mut file = match OpenOptions::new().read(true).write(true).open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        };
        let len = file.metadata()?.len();
        let complete = segment::complete_len(&mut file)?;
        if complete == len {
            return Ok(false);
        }
        file.set_len(complete)?;
        file.sync_data()?;
        log::warn!("journal: repaired torn tail of {}", path.display());
        Ok(true)
    }

    /// The highest `seq` written so far, recovered from the newest segment's
    /// tail. The full segment (bounded by the segment cap) is streamed only
    /// when its last line is unusable.
    pub(crate) fn last_seq(&self, segments: &[Segment]) -> io::Result<u64> {
        let Some(newest) = segments.last() else {
            return Ok(0);
        };
        let floor = newest.first_seq.saturating_sub(1);
        let mut file = File::open(&newest.path)?;
        let end = segment::complete_len(&mut file)?;
        if let Some(seq) = segment::last_line(&mut file, end)?.and_then(|l| decode(&l).seq()) {
            return Ok(seq.max(floor));
        }
        let mut reader = LineReader::open(&newest.path, 0)?;
        let mut best = floor;
        while let Some((_, line)) = reader.next_line()? {
            if let Line::Complete(bytes) = line {
                if let Some(seq) = decode(&bytes).seq() {
                    best = best.max(seq);
                }
            }
        }
        Ok(best)
    }
}

/// Create `dir` and fsync each newly created level's parent.
fn ensure_dir(dir: &Path) -> io::Result<()> {
    if dir.as_os_str().is_empty() || dir.is_dir() {
        return Ok(());
    }
    let parent = dir
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    ensure_dir(parent)?;
    match std::fs::create_dir(dir) {
        Ok(()) => sync_dir(parent),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}
