//! Slow private workspace admission runs without holding the registry mutex.
use super::*;
use crate::tokens_pool::private_workspace::{dispatch::Selection, JobKind};

pub(crate) struct PreparedLaunch {
    pub admission: crate::runtime_preference::DispatchAdmission,
    pub model: Option<String>,
    pub selection: Option<Selection>,
}

pub(crate) fn prepare(
    registry: &Arc<Mutex<SweepRegistry>>,
    kind: &SweepKind,
    idempotency_key: Option<&str>,
    model: DispatchModel<'_>,
) -> Result<Option<PreparedLaunch>> {
    let config = {
        let registry = registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if idempotency_key.is_some_and(|key| registry.find_running_by_key(key).is_some()) {
            return Ok(None);
        }
        registry.config.clone()
    };
    if config.skip_label_flip {
        return Ok(None);
    }
    let admission = match crate::runtime_preference::resolve_for_dispatch(
        &config.workspace_root,
        "sweep-lifecycle",
        None,
    ) {
        Ok(admission) => admission,
        Err(rejection) => {
            registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .emit_event(Event::SweepGlobalRuntimeRejected {
                    kind: kind.clone(),
                    role: rejection.role.clone(),
                    runtime: rejection.runtime.clone(),
                    runtime_source: rejection.source.clone(),
                    unmet_capabilities: rejection.unmet_capabilities.clone(),
                    reason: rejection.reason.clone(),
                    repo: None,
                });
            return Err(rejection.into());
        }
    };
    let resolved = model.resolve(&config, kind, admission.admitted.as_ref());
    let issue = match kind {
        SweepKind::Issue(n) => Some(u64::from(*n)),
        SweepKind::PrSet(_) => None,
    };
    let selection = Selection::prepare(
        &config.workspace_root,
        admission
            .admitted
            .as_ref()
            .map_or("claude", |a| a.runtime.as_str()),
        resolved.as_deref(),
        JobKind::Sweep,
        issue,
        &format!("dispatch-{}", uuid::Uuid::new_v4()),
    )?;
    Ok(Some(PreparedLaunch {
        admission,
        model: resolved,
        selection,
    }))
}

impl SweepRegistry {
    /// Registry API for dispatchers that can release their mutex around slow
    /// account/workspace preparation. IPC and scheduled dispatch use this same
    /// preparation/begin/poll/finish protocol.
    pub fn dispatch_unlocked(
        registry: &Arc<Mutex<Self>>,
        kind: &SweepKind,
        idempotency_key: Option<String>,
        model: Option<&str>,
        effort: Option<&str>,
        depends_on: Option<u32>,
    ) -> Result<DispatchOutcome> {
        dispatch_model_releasing_poll_lock(
            registry,
            kind,
            idempotency_key,
            DispatchModel::Request(model),
            effort,
            depends_on,
        )
    }

    /// Compatibility entry for lock-held watchdog/reaper paths. A private
    /// recovery must be requested through unlocked dispatch; never block the
    /// daemon's status mutex with Docker or silently route it to another pool.
    pub(crate) fn begin_issue_dispatch_with_model(
        &mut self,
        kind: &SweepKind,
        idempotency_key: Option<String>,
        model: DispatchModel<'_>,
        effort: Option<&str>,
        depends_on: Option<u32>,
        resume_bypass_pr: Option<u32>,
    ) -> Result<BeginIssueDispatch> {
        let existing = idempotency_key
            .as_deref()
            .is_some_and(|key| self.find_running_by_key(key).is_some());
        if !existing && !self.config.skip_label_flip {
            let private_issue = matches!(kind, SweepKind::Issue(n) if crate::tokens_pool::private_workspace::export::has_issue(&self.config.workspace_root, *n));
            let codex = crate::runtime_admission::resolve_binding(
                &self.config.workspace_root,
                "sweep-lifecycle",
                None,
            )
            .is_ok_and(|(runtime, _)| runtime == "codex");
            if private_issue
                || (codex
                    && crate::tokens_pool::private_workspace::dispatch::uses_private(
                        &self.config.workspace_root,
                    )?)
            {
                anyhow::bail!("private workspace requires recovery/preparation through unlocked dispatch; no issue claim or account failover was attempted");
            }
        }
        self.begin_prepared_issue_dispatch(
            kind,
            idempotency_key,
            model,
            effort,
            depends_on,
            resume_bypass_pr,
            None,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn existing_private_dispatch_returns_before_admission_or_account_preparation() {
        let root = tempfile::tempdir().unwrap();
        let mut registry = SweepRegistry::new(SweepRegistryConfig::new(root.path().into()));
        std::fs::create_dir_all(root.path().join(".loom/private-jobs")).unwrap();
        std::fs::write(root.path().join(".loom/private-jobs/issue-7.json"), "{}").unwrap();
        let entry = SweepInfo {
            pgid: None,
            sweep_id: "existing".into(),
            kind: SweepKind::Issue(7),
            pid: 123,
            token_name: "private-account".into(),
            runtime: "codex".into(),
            runtime_source: None,
            log_path: root.path().join("existing.log"),
            idempotency_key: Some("same-key".into()),
            started_at: Utc::now(),
            state: SweepState::Running,
            latest_phase: None,
            pr_number: None,
            model: None,
            effort: None,
            depends_on: None,
            repo: None,
        };
        registry.entries.insert(entry.sweep_id.clone(), entry);
        let registry = Arc::new(Mutex::new(registry));
        // No runtime manifests/account config exist: any attempt to re-admit
        // or prepare would fail instead of returning the existing sweep.
        assert!(prepare(
            &registry,
            &SweepKind::Issue(7),
            Some("same-key"),
            DispatchModel::Request(None)
        )
        .unwrap()
        .is_none());
        let mut registry = registry.lock().unwrap();
        let result = registry
            .begin_issue_dispatch_with_model(
                &SweepKind::Issue(7),
                Some("same-key".into()),
                DispatchModel::Request(None),
                None,
                None,
                None,
            )
            .unwrap();
        match result {
            BeginIssueDispatch::Done(Ok(outcome)) => {
                assert!(!outcome.was_new);
                assert_eq!(outcome.sweep_id, "existing");
            }
            _ => panic!("idempotency retry prepared or spawned a new job"),
        }
    }
}
