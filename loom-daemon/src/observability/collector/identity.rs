//! Launch metadata appears asynchronously after dispatch. Poll only live registry
//! entries, including adopted sweeps, without delaying dispatch or holding registry
//! locks across disk/network work. Bounded incremental reads recover long logs too.
use super::*;
use crate::launch_record::{parse_launch_runtime, RuntimeAttribution};
use crate::telemetry::SweepIdentityRecord;
use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};

const READ_BYTES: u64 = 256 * 1024;
const MAX_LINE_BYTES: usize = 16 * 1024;

#[derive(Default)]
struct LaunchReader {
    offset: u64,
    partial: String,
    in_dispatch: bool,
    resolved: Option<RuntimeAttribution>,
}

impl LaunchReader {
    /// Incremental, bounded I/O. Select the first launch in the exact dispatch
    /// header's region, so later child-role records cannot relabel the sweep.
    fn read(&mut self, path: &Path, sweep_id: &str) -> Option<RuntimeAttribution> {
        if self.resolved.is_some() {
            return self.resolved.clone();
        }
        let mut file = std::fs::File::open(path).ok()?;
        if file.metadata().ok()?.len() < self.offset {
            *self = Self::default();
        }
        file.seek(SeekFrom::Start(self.offset)).ok()?;
        let mut bytes = Vec::new();
        file.take(READ_BYTES).read_to_end(&mut bytes).ok()?;
        self.offset += bytes.len() as u64;
        self.partial.push_str(&String::from_utf8_lossy(&bytes));
        let anchor = format!("sweep_id={sweep_id}");
        while let Some(end) = self.partial.find('\n') {
            let line: String = self.partial.drain(..=end).collect();
            if line.starts_with("==== loom-daemon dispatch: ") {
                self.in_dispatch = line.split_whitespace().any(|word| word == anchor);
            } else if self.in_dispatch {
                // Reuse the shared line-anchored record scanner and field parser.
                if let Some(body) = crate::api_keys_pool::ingest::last_launch_record_body(&line) {
                    self.resolved = parse_launch_runtime(body);
                    if self.resolved.is_some() {
                        break;
                    }
                }
            }
        }
        // A transcript line can be arbitrarily large; its suffix cannot be a
        // header/launch marker. Keep a sentinel until its terminating newline.
        if self.partial.len() > MAX_LINE_BYTES {
            self.partial = "[truncated transcript]".to_owned();
        }
        self.resolved.clone()
    }
}

#[derive(Default)]
pub(super) struct Sampler {
    readers: HashMap<(PathBuf, String), LaunchReader>,
    sent: HashMap<(PathBuf, String), SweepIdentityRecord>,
}

impl Sampler {
    /// An early identity can reach the Worker before its lifecycle row exists.
    /// Replay after a row-creating event, retaining the resolved launch/offset.
    /// Queue acknowledgment alone does not prove the Worker applied enrichment.
    pub(super) fn observe(&mut self, event: &Event, default_root: &Path) {
        let (issue, sweep_id) = match event {
            Event::SweepGlobalDispatch {
                kind: SweepKind::Issue(issue),
                sweep_id,
                ..
            } => (*issue, Some(sweep_id)),
            Event::SweepPhase { issue, .. } => (*issue, None),
            _ => return,
        };
        let root = event_repo_path(event)
            .map(PathBuf::from)
            .unwrap_or_else(|| default_root.to_path_buf());
        self.sent.retain(|(path, id), record| {
            path != &root
                || record.issue != issue
                || sweep_id.is_some_and(|expected| expected != id)
        });
    }

    fn enqueue(
        &mut self,
        queue: &dyn crate::observability::queue::QueueSink,
        host: &str,
        root: PathBuf,
        record: SweepIdentityRecord,
    ) {
        let key = (root, record.sweep_id.clone());
        if self.sent.get(&key) != Some(&record) {
            queue.offer(TelemetryEnvelope::new(
                host,
                TelemetryRecord::SweepIdentity(record.clone()),
            ));
            self.sent.insert(key, record);
        }
    }

    pub(super) async fn sample(
        &mut self,
        queue: &dyn crate::observability::queue::QueueSink,
        host: &str,
        pool: &Arc<WorkspacePool>,
        slugs: &mut HashMap<String, String>,
    ) {
        let pool = pool.clone();
        let mut readers = std::mem::take(&mut self.readers);
        let Ok((readers, samples, active, complete_snapshot)) =
            tokio::task::spawn_blocking(move || {
                let mut samples = Vec::new();
                let mut active = HashSet::new();
                let mut complete_snapshot = true;
                for registry in pool.provisioned_registries() {
                    let (root, snapshot) = {
                        let Ok(guard) = registry.try_lock() else {
                            complete_snapshot = false;
                            continue;
                        };
                        (guard.config().workspace_root.clone(), guard.snapshot())
                    };
                    for info in snapshot.list(None) {
                        let SweepKind::Issue(issue) = info.kind else {
                            continue;
                        };
                        if info.state.is_terminal() {
                            continue;
                        }
                        let key = (root.clone(), info.sweep_id.clone());
                        active.insert(key.clone());
                        let log_path = if info.log_path.is_absolute() {
                            info.log_path.clone()
                        } else {
                            root.join(&info.log_path)
                        };
                        let launch = readers
                            .entry(key)
                            .or_default()
                            .read(&log_path, &info.sweep_id);
                        samples.push((root.clone(), issue, info, launch));
                    }
                }
                // A busy registry is not evidence that its sweeps ended. Preserve
                // cached attribution/read offsets until a complete snapshot exists.
                if complete_snapshot {
                    readers.retain(|key, _| active.contains(key));
                }
                (readers, samples, active, complete_snapshot)
            })
            .await
        else {
            return;
        };
        self.readers = readers;
        if complete_snapshot {
            self.sent.retain(|key, _| active.contains(key));
        }
        for (root, issue, info, launch) in samples {
            let Some(repo) = resolve_repo_slug_cached(slugs, &root.to_string_lossy()).await else {
                continue;
            };
            let visibility = resolve_visibility(&repo).await;
            let record = identity_record(
                repo,
                visibility,
                issue,
                info.sweep_id.clone(),
                &info.runtime,
                launch.as_ref(),
            );
            self.enqueue(queue, host, root, record);
        }
    }
}

fn identity_record(
    repo: String,
    visibility: RepoVisibility,
    issue: u32,
    sweep_id: String,
    admitted_runtime: &str,
    launch: Option<&RuntimeAttribution>,
) -> SweepIdentityRecord {
    let runtime = launch.map(|r| r.runtime.clone()).or_else(|| {
        let runtime = admitted_runtime.trim();
        (!runtime.is_empty() && runtime != "unknown").then(|| runtime.to_owned())
    });
    SweepIdentityRecord {
        repo,
        visibility,
        issue,
        sweep_id,
        runtime,
        provider: launch.and_then(|r| r.provider.clone()),
        model: launch.and_then(|r| r.model.clone()),
    }
}

#[cfg(test)]
mod tests;
