//! The scripted [`ResumeHost`] the H5 decision tests drive (#10832).

use super::*;

// ============================================================================
// Scripted host
// ============================================================================

#[derive(Default)]
pub(super) struct FakeHost {
    /// Every host call, in order.
    pub(super) calls: Mutex<Vec<String>>,
    pub(super) events: Mutex<Vec<(String, serde_json::Value)>>,
    pub(super) statuses: Mutex<Vec<PauseResumeStatus>>,
    /// `check` refusals by item id.
    pub(super) refuse_check: Mutex<BTreeMap<String, RollResumeRefusal>>,
    /// `launch` refusals by item id.
    pub(super) refuse_launch: Mutex<BTreeMap<String, RollResumeRefusal>>,
    /// Items whose relaunch dies, with the reason.
    pub(super) die: Mutex<BTreeMap<String, String>>,
    /// Items found already running, with their new id.
    pub(super) running: Mutex<BTreeMap<String, String>>,
    /// Items with a safe-point record on disk.
    pub(super) parked: Mutex<BTreeSet<String>>,
    /// Items with residue.
    pub(super) residue: Mutex<BTreeSet<String>>,
    /// Items whose requeue forge write fails.
    pub(super) requeue_fails: Mutex<BTreeSet<String>>,
    /// Health samples that fail before health holds.
    pub(super) unhealthy: Mutex<u32>,
    /// `hold_dispatch` calls refused before the hold is placed.
    pub(super) hold_blocked: Mutex<u32>,
    /// Lose the hold once this many items have been launched.
    pub(super) lose_hold_after: Mutex<Option<usize>>,
    pub(super) launch_delay: Duration,
    pub(super) drain: DrainState,
}

impl FakeHost {
    pub(super) fn log(&self, call: String) {
        self.calls.lock().unwrap().push(call);
    }
    pub(super) fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
    pub(super) fn called(&self, prefix: &str) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter(|c| c.starts_with(prefix))
            .collect()
    }
    pub(super) fn topic(&self, topic: &str) -> Vec<serde_json::Value> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|(t, _)| t == topic)
            .map(|(_, p)| p.clone())
            .collect()
    }
}

impl ResumeHost for FakeHost {
    fn hold_dispatch(&self) -> bool {
        let mut blocked = self.hold_blocked.lock().unwrap();
        if *blocked > 0 {
            *blocked -= 1;
            return false;
        }
        self.drain.hold_for_roll_resume("resuming".to_string())
    }
    fn dispatch_held(&self) -> bool {
        self.drain.is_roll_resume_held()
    }
    fn health_sample(&self) -> Result<(), String> {
        self.log("health".to_string());
        let mut left = self.unhealthy.lock().unwrap();
        if *left > 0 {
            *left -= 1;
            return Err("IPC ping got no answer".to_string());
        }
        Ok(())
    }
    fn safe_point(&self, item: &ManifestItem) -> Option<SafePointRecord> {
        self.parked
            .lock()
            .unwrap()
            .contains(&item.id)
            .then(|| SafePointRecord {
                reached_at: rfc3339(Utc::now()),
                parked_tool: Some("Edit".to_string()),
                parked_summary: None,
            })
    }
    fn reap_residue(&self, item: &ManifestItem, scope_only: bool) -> TeardownReport {
        self.log(format!("residue {}{}", item.id, if scope_only { " scope-only" } else { "" }));
        if self.residue.lock().unwrap().contains(&item.id) {
            return TeardownReport {
                pids: vec![4242, 4243],
                ..TeardownReport::default()
            };
        }
        TeardownReport::default()
    }
    fn refresh_lease(&self, item: &ManifestItem, _timeout: Duration) -> Result<(), String> {
        self.log(format!("lease {}", item.id));
        Ok(())
    }
    fn already_resumed(&self, item: &ManifestItem) -> Option<String> {
        self.running.lock().unwrap().get(&item.id).cloned()
    }
    fn check(
        &self,
        item: &ManifestItem,
        _launch: &RollResumeLaunch,
    ) -> Result<(), RollResumeRefusal> {
        self.log(format!("check {}", item.id));
        match self.refuse_check.lock().unwrap().get(&item.id) {
            Some(refusal) => Err(refusal.clone()),
            None => Ok(()),
        }
    }
    fn launch(
        &self,
        item: &ManifestItem,
        launch: &RollResumeLaunch,
        _wait: Duration,
    ) -> Result<Launched, RollResumeRefusal> {
        if let Some(refusal) = self.refuse_launch.lock().unwrap().get(&item.id) {
            return Err(refusal.clone());
        }
        std::thread::sleep(self.launch_delay);
        self.log(format!(
            "launch {} session={} count={}",
            item.id, launch.session_id, launch.resume_count
        ));
        let launched = self.called("launch ").len();
        if self
            .lose_hold_after
            .lock()
            .unwrap()
            .is_some_and(|n| launched >= n)
        {
            // An operator drain replaces the hold.
            let _ = self.drain.begin(Duration::from_secs(60), false, false);
        }
        Ok(Launched {
            item_id: format!("{}-r{}", item.id, launch.resume_count),
            pid: Some(1),
            at: Instant::now(),
            log_path: None,
            header_anchor: None,
        })
    }
    fn liveness(&self, item: &ManifestItem, _launched: &Launched) -> Liveness {
        match self.die.lock().unwrap().get(&item.id) {
            Some(reason) => Liveness::Died {
                reason: reason.clone(),
                detail: "exited code 1".to_string(),
            },
            None => Liveness::Running,
        }
    }
    fn abandon(&self, item: &ManifestItem, _launched: &Launched) {
        self.log(format!("abandon {}", item.id));
    }
    fn settle(&self, _manifest_id: &str, item: &ManifestItem, new_item_id: &str) {
        self.log(format!("settle {} as {new_item_id}", item.id));
    }
    fn requeue(
        &self,
        item: &ManifestItem,
        notice: &RollRequeueNotice,
        forge: bool,
    ) -> Result<(), String> {
        self.log(format!("requeue {} {} forge={forge}", item.id, notice.reason));
        if forge && self.requeue_fails.lock().unwrap().contains(&item.id) {
            return Err("forge down".to_string());
        }
        Ok(())
    }
    fn recover(&self, _manifest_id: &str, item: &ManifestItem) {
        self.log(format!("recover {}", item.id));
    }
    fn finish(&self, manifest_id: &str, note: &str) {
        self.log(format!("finish {manifest_id}"));
        self.drain.release_roll_resume_hold(note.to_string());
    }
    fn emit(&self, topic: &str, payload: serde_json::Value) {
        self.events
            .lock()
            .unwrap()
            .push((topic.to_string(), payload));
    }
    fn publish(&self, status: &PauseResumeStatus) {
        self.statuses.lock().unwrap().push(status.clone());
    }
}

pub(super) fn refusal(reason: &str) -> RollResumeRefusal {
    RollResumeRefusal::new(reason, "observed")
}
