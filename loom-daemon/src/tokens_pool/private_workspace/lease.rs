use super::*;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Job {
    pub owner: String,
    pub kind: JobKind,
    pub issue: Option<u64>,
    pub branch: Option<String>,
    pub container_id: String,
    pub base_revision: String,
    pub host_pid: u32,
}

/// Secret-free provenance of a containment admission (#8787), beside the
/// durable job it belongs to and retired with it.
pub(super) const ADMISSION: &str = "admission.json";

/// The open file owns process exclusion; the durable JSON owns uncertainty
/// after a crash. Never unlink a lock inode: two callers must lock the same one.
pub(super) struct Lease {
    _file: File,
    pub dir: PathBuf,
}

impl Lease {
    pub(super) fn fd(&self) -> i32 {
        self._file.as_raw_fd()
    }
    pub(super) fn from_file(file: File, dir: PathBuf) -> Self {
        Self { _file: file, dir }
    }

    pub fn acquire(dir: &Path) -> Result<Self> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        if std::fs::symlink_metadata(dir)?.file_type().is_symlink() {
            bail!("session lease directory must not be a symlink");
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(dir.join("account.lock"))?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            bail!("account session is busy; role, sweep, interactive and lifecycle operations share one exclusive lease");
        }
        Ok(Self {
            _file: file,
            dir: dir.to_owned(),
        })
    }

    pub fn recover(&self, config: &Config, state: Option<&serde_json::Value>) -> Result<()> {
        if let Some(job) = read(&self.dir)? {
            // A different live container is not evidence the original worker
            // died. Same engine + absent old container, or exact idle proof,
            // is required; elapsed time and a dead host PID prove nothing.
            if state.is_some_and(|s| s["Id"] != job.container_id) {
                bail!("lease belongs to another container identity; preserve it for operator recovery");
            }
            if state.is_none() {
                // Names are mutable Docker aliases. A renamed container is
                // still the original worker; only ID absence or confirmed
                // stoppage proves it safe when the configured name vanished.
                if let Some(original) = docker::inspect_id(&job.container_id)? {
                    if original["State"]["Running"] != false {
                        bail!("original container ID still exists/runs under another name; retain its lease and restore the name or stop that container explicitly");
                    }
                }
            }
            if !docker::idle(state, &config.container)? {
                bail!("stale lease still has a surviving or unverified container worker; retain lease and workspace");
            }
            self.finish()?;
        }
        Ok(())
    }

    pub fn begin(&self, job: &Job) -> Result<()> {
        save(&self.dir.join("job.json"), job)
    }

    pub fn finish(&self) -> Result<()> {
        match std::fs::rename(self.dir.join(ADMISSION), self.dir.join("last-admission.json")) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let path = self.dir.join("job.json");
        match std::fs::rename(&path, self.dir.join("last-job.json")) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

/// The containment admission recorded for the CURRENT job, if any.
pub(super) fn admission(dir: &Path) -> Result<Option<serde_json::Value>> {
    match std::fs::read(dir.join(ADMISSION)) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn read(dir: &Path) -> Result<Option<Job>> {
    match std::fs::read(dir.join("job.json")) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes)
                .context("invalid job lease; operator recovery required")?,
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}
