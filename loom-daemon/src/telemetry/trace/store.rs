//! Bounded active-execution context, atomically persisted before child launch.
use super::TraceContext;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub const TRACEPARENT_ENV: &str = "LOOM_TRACEPARENT";
pub const CONTEXT_FILE_ENV: &str = "LOOM_TRACE_CONTEXT_FILE";
const MAX_CONTEXT_BYTES: u64 = 4096;
const MAX_ACTIVE_CONTEXTS: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionContext {
    pub identity: String,
    pub context: TraceContext,
    pub started_at: DateTime<Utc>,
    /// The issue story this execution belongs to (#9037): `context` shares
    /// its trace id and the root span is parented to it. Absent for
    /// executions outside an issue, and in files persisted before #9038.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub story: Option<TraceContext>,
}

#[derive(Debug)]
pub struct TraceStore {
    directory: PathBuf,
}

impl TraceStore {
    #[must_use]
    pub fn new(workspace: &Path) -> Self {
        Self {
            directory: workspace.join(".loom/logs/trace-context"),
        }
    }

    fn identity(workspace: &Path, execution: &str) -> String {
        let root = workspace
            .canonicalize()
            .unwrap_or_else(|_| workspace.to_path_buf());
        let mut hash = Sha256::new();
        hash.update(root.as_os_str().as_encoded_bytes());
        hash.update([0]);
        hash.update(execution.as_bytes());
        hex::encode(hash.finalize())
    }

    /// An execution's root context. Inside an issue story it is the story's
    /// child keyed by the execution (sweep) id; outside one it is its own
    /// trace keyed by repo + execution id. Either way `sweep-issue-42-…` in
    /// `owner/repo` always maps to the same IDs, on any host.
    #[must_use]
    pub fn root_context(
        workspace: &Path,
        execution: &str,
        story: Option<&TraceContext>,
    ) -> TraceContext {
        match story {
            Some(story) => story.derived_child(&["loom.sweep", execution]),
            None => {
                TraceContext::derived("execution", &[&Self::fallback_repo(workspace), execution])
            }
        }
    }

    /// The repo key for an execution outside a story: `$LOOM_REPO`, else the
    /// workspace basename ([`crate::peer_claims::repo_slug`]), lowercased.
    #[must_use]
    pub fn fallback_repo(workspace: &Path) -> String {
        crate::peer_claims::repo_slug(workspace).to_ascii_lowercase()
    }

    #[must_use]
    pub fn path(&self, workspace: &Path, execution: &str) -> PathBuf {
        self.directory
            .join(format!("{}.json", Self::identity(workspace, execution)))
    }

    /// Nonblocking lock: unavailable local telemetry must never stall dispatch.
    fn lock(&self) -> Result<File> {
        std::fs::create_dir_all(&self.directory)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.directory, std::fs::Permissions::from_mode(0o700))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.directory.join(".lock"))?;
        file.try_lock().context("trace context store busy")?;
        Ok(file)
    }

    pub fn load_or_create(&self, workspace: &Path, execution: &str) -> Result<ExecutionContext> {
        self.load_or_create_story(workspace, execution, None)
    }

    /// As [`Self::load_or_create`], but a new execution joins `story`'s trace
    /// as its child. An already-persisted execution keeps its saved context.
    pub fn load_or_create_story(
        &self,
        workspace: &Path,
        execution: &str,
        story: Option<&TraceContext>,
    ) -> Result<ExecutionContext> {
        let _lock = self.lock()?;
        let path = self.path(workspace, execution);
        if path.exists() {
            let context = Self::load(&path)?;
            anyhow::ensure!(
                context.identity == Self::identity(workspace, execution),
                "trace identity mismatch"
            );
            return Ok(context);
        }
        let count = std::fs::read_dir(&self.directory)?
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|v| v == "json"))
            .count();
        anyhow::ensure!(count < MAX_ACTIVE_CONTEXTS, "active trace context limit reached");
        let context = ExecutionContext {
            identity: Self::identity(workspace, execution),
            context: Self::root_context(workspace, execution, story),
            started_at: Utc::now(),
            story: story.cloned(),
        };
        let mut tmp = tempfile::NamedTempFile::new_in(&self.directory)?;
        serde_json::to_writer(&mut tmp, &context)?;
        tmp.write_all(b"\n")?;
        tmp.as_file().sync_all()?;
        tmp.persist_noclobber(&path).map_err(|e| e.error)?;
        File::open(&self.directory)?.sync_all()?;
        Ok(context)
    }

    pub fn load(path: &Path) -> Result<ExecutionContext> {
        let file = File::open(path)?;
        anyhow::ensure!(file.metadata()?.len() <= MAX_CONTEXT_BYTES, "trace context too large");
        let mut bytes = Vec::new();
        file.take(MAX_CONTEXT_BYTES + 1).read_to_end(&mut bytes)?;
        anyhow::ensure!(bytes.len() as u64 <= MAX_CONTEXT_BYTES, "trace context too large");
        serde_json::from_slice(&bytes).context("invalid persisted trace context")
    }

    /// Call only after the terminal span has been durably offered to the queue.
    pub fn complete(&self, workspace: &Path, execution: &str) -> Result<()> {
        let _lock = self.lock()?;
        match std::fs::remove_file(self.path(workspace, execution)) {
            Ok(()) => {
                File::open(&self.directory)?.sync_all()?;
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}
