//! Test support shared by the H5 tests and the H4 → H5 hand-off tests in
//! `pause_roll` (#10832): a [`ResumeHost`] over one real sweep registry that
//! runs the production sweep operations, and the fakes it needs.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::host::{self, sweeps, Launched, Liveness, ResumeHost};
use super::PauseResumeStatus;
use crate::auto_update::pause_manifest::{ManifestItem, PauseManifest, SafePointRecord};
use crate::auto_update::pause_roll::teardown::TeardownReport;
use crate::ipc::{DrainState, ResumeHold};
use crate::roll_pause::{self, PauseRequest};
use crate::sweep_registry::resume_handle::RollResumeLaunch;
use crate::sweep_registry::roll_requeue::RollRequeueNotice;
use crate::sweep_registry::roll_resume::RollResumeRefusal;
use crate::sweep_registry::test_support;
use crate::sweep_registry::{SweepRegistry, SweepRegistryConfig};

/// A [`ResumeHost`] over one real sweep registry (the production sweep ops).
pub(crate) struct RegistryHost {
    pub(crate) registry: Arc<Mutex<SweepRegistry>>,
    pub(crate) events: Mutex<Vec<(String, serde_json::Value)>>,
    pub(crate) drain: DrainState,
    pub(crate) launches: Mutex<Vec<String>>,
}

impl RegistryHost {
    pub(crate) fn new(registry: SweepRegistry) -> Arc<Self> {
        Arc::new(Self {
            registry: Arc::new(Mutex::new(registry)),
            events: Mutex::default(),
            drain: DrainState::new(),
            launches: Mutex::default(),
        })
    }
}

impl ResumeHost for RegistryHost {
    fn hold_dispatch(&self) -> ResumeHold {
        self.drain.roll_resume_hold("resuming".to_string())
    }
    fn health_sample(&self) -> Result<(), String> {
        Ok(())
    }
    fn safe_point(&self, item: &ManifestItem, request: &PauseRequest) -> Option<SafePointRecord> {
        host::disk_safe_point(item, request)
    }
    fn reap_residue(&self, item: &ManifestItem) -> TeardownReport {
        host::reap_residue(item)
    }
    fn refresh_lease(&self, item: &ManifestItem, timeout: Duration) -> Result<(), String> {
        sweeps::refresh_lease(&self.registry, item, timeout)
    }
    fn already_resumed(&self, item: &ManifestItem) -> Option<String> {
        sweeps::already_resumed(&self.registry, item)
    }
    fn check(
        &self,
        item: &ManifestItem,
        launch: &RollResumeLaunch,
    ) -> Result<(), RollResumeRefusal> {
        sweeps::check(&self.registry, item, launch)
    }
    fn launch(
        &self,
        item: &ManifestItem,
        launch: &RollResumeLaunch,
        _wait: Duration,
    ) -> Result<Launched, RollResumeRefusal> {
        self.launches.lock().unwrap().push(item.id.clone());
        sweeps::launch(&self.registry, item, launch)
    }
    fn liveness(&self, item: &ManifestItem, launched: &Launched) -> Liveness {
        sweeps::liveness(&self.registry, item, launched)
    }
    fn abandon(&self, _item: &ManifestItem, launched: &Launched) {
        sweeps::abandon(&self.registry, launched);
    }
    fn settle(&self, manifest_id: &str, item: &ManifestItem, new_item_id: &str) {
        roll_pause::suppress::release_item(manifest_id, &item.id);
        roll_pause::hold::release(new_item_id, host::RESUME_HOLD_OWNER);
    }
    fn requeue(
        &self,
        item: &ManifestItem,
        notice: &RollRequeueNotice,
        forge: bool,
    ) -> Result<(), String> {
        sweeps::requeue(&self.registry, item, notice, forge)
    }
    fn recover(&self, manifest_id: &str, item: &ManifestItem) {
        roll_pause::suppress::release_item(manifest_id, &item.id);
        sweeps::recover(&self.registry, item);
    }
    fn finish(&self, manifest_id: &str, note: &str) {
        roll_pause::suppress::disarm(manifest_id);
        self.drain.release_roll_resume_hold(note.to_string());
    }
    fn emit(&self, topic: &str, payload: serde_json::Value) {
        self.events
            .lock()
            .unwrap()
            .push((topic.to_string(), payload));
    }
    fn publish(&self, _status: &PauseResumeStatus) {}
}

pub(crate) fn executable(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    if let Ok(f) = std::fs::File::open(path) {
        let _ = f.sync_all();
    }
}

/// A fake `gh` that logs every call. Issues are open; `labels` are every
/// issue's labels; there are no lease records.
pub(crate) fn fake_gh(root: &Path, labels: &str) -> (PathBuf, PathBuf) {
    let (gh, log) = (root.join("fake-gh.sh"), root.join("gh.log"));
    executable(
        &gh,
        &format!(
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >> \"{log}\"\n\
             if [[ \"$1\" == repo && \"$2\" == view ]]; then echo rjwalters/loom; exit 0; fi\n\
             if [[ \"$1\" == issue && \"$2\" == view ]]; then echo false; exit 0; fi\n\
             if [[ \"$1\" == api && \"$*\" == */comments* ]]; then exit 0; fi\n\
             if [[ \"$1\" == api && \"$*\" == *is_pr* ]]; then echo '{state}'; exit 0; fi\n\
             if [[ \"$1\" == api && \"$2\" == repos/* ]]; then printf '%s\\n' {labels}; exit 0; fi\n\
             exit 0\n",
            log = log.display(),
            state = test_support::state_probe_json("open", false),
        ),
    );
    (gh, log)
}

/// A spawn script that stays up like a session, or exits with `exit`.
pub(crate) fn spawn_bin(root: &Path, body: &str) -> PathBuf {
    let scripts = root.join(".loom").join("scripts");
    std::fs::create_dir_all(&scripts).unwrap();
    let bin = scripts.join("spawn-claude.sh");
    executable(&bin, &format!("#!/usr/bin/env bash\n{body}\n"));
    bin
}

pub(crate) fn real_registry(root: &Path, gh: Option<PathBuf>, spawn_body: &str) -> SweepRegistry {
    let mut config = SweepRegistryConfig::new(root.to_path_buf());
    config.spawn_bin = Some(spawn_bin(root, spawn_body));
    config.skip_label_flip = gh.is_none();
    config.gh_bin = gh;
    config.journal_path = Some(root.join("sweeps.json"));
    if !config.skip_label_flip {
        // A resume re-admits the recorded runtime, as a dispatch does.
        test_support::install_runtime_admission_fixture(root);
    }
    SweepRegistry::new(config)
}

pub(crate) fn gh_calls(log: &Path) -> String {
    std::fs::read_to_string(log).unwrap_or_default()
}

/// Arm the recovery suppression for `m`'s items, as startup does.
pub(crate) fn arm_for(root: &Path, m: &PauseManifest) {
    roll_pause::suppress::arm(
        &m.manifest_id,
        m.items
            .iter()
            .map(|i| roll_pause::suppress::HeldItem {
                id: i.id.clone(),
                repo: root.to_path_buf(),
                issue: i.issue,
            })
            .collect(),
    );
}
