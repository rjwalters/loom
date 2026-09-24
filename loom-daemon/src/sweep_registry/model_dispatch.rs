//! Preserve request provenance until the runtime has been admitted once.
use super::*;

/// A resolved value is a literal launch argument (the historical low-level
/// registry API). Requests still need policy resolution; autonomous callers
/// with a cached issue body can supply complexity without another forge read.
#[derive(Clone, Copy)]
pub(crate) enum DispatchModel<'a> {
    Resolved(Option<&'a str>),
    Request(Option<&'a str>),
    Autonomous { complexity: Option<&'a str> },
}

impl DispatchModel<'_> {
    pub(crate) fn resolve(
        self,
        config: &SweepRegistryConfig,
        kind: &SweepKind,
        admitted: Option<&crate::runtime_admission::ResolvedRuntime>,
    ) -> Option<String> {
        if let Self::Resolved(model) = self {
            return model.map(str::to_owned);
        }
        let root = &config.workspace_root;
        // Only hermetic skip-label fixtures lack admission. Reading their
        // configured binding does not select credentials or reserve a slot.
        //
        // Issue #8721: when admission already ran, classify the ACTUAL
        // admitted runtime directly (`DefaultModelPolicy::for_runtime`)
        // instead of re-deriving it from config a second time — this is the
        // one call site that has the real admitted runtime name in hand, so
        // it is also the one place a `runtimes.default`/env override applied
        // between admission and this resolve() can't disagree with what
        // actually launched.
        let policy = admitted.map_or_else(
            || DefaultModelPolicy::resolve(root),
            |admitted| DefaultModelPolicy::for_runtime(&admitted.runtime),
        );
        let resolved = match (self, kind) {
            (Self::Request(None) | Self::Autonomous { .. }, SweepKind::Issue(issue)) => {
                resolve_autonomous_model_for_runtime(
                    root,
                    *issue,
                    || match self {
                        Self::Autonomous { complexity } => complexity.map(str::to_owned),
                        _ => fetch_issue_complexity(
                            config.gh_bin.as_deref().unwrap_or_else(|| Path::new("gh")),
                            root,
                            *issue,
                        ),
                    },
                    policy,
                )
            }
            _ => {
                let explicit = match self {
                    Self::Request(model) => model,
                    _ => None,
                };
                let (model, source) = resolve_dispatch_model_for_runtime(root, explicit, policy);
                ExperimentDispatchModel {
                    model,
                    mode: "off".into(),
                    arm: None,
                    source_label: if source == ModelSource::Default {
                        match policy {
                            DefaultModelPolicy::NativeProfile => "native-profile",
                            DefaultModelPolicy::Codex => "codex-no-default",
                            DefaultModelPolicy::ClaudeOrOther => source.as_str(),
                        }
                    } else {
                        source.as_str()
                    },
                }
            }
        };
        log::info!(
            "sweep dispatch: attempting {kind:?} runtime={} model={} (source={}) arm={:?}",
            admitted.map_or("fixture", |a| a.runtime.as_str()),
            resolved.model,
            resolved.source_label,
            resolved.arm,
        );
        (!resolved.model.is_empty()).then_some(resolved.model)
    }
}

impl SweepRegistry {
    /// Dispatch a sweep. See module docs.
    ///
    /// On idempotency hit returns the existing entry with `was_new = false`.
    ///
    /// `model` (issue #3477): when `Some` and non-empty, the spawned child
    /// receives `--model <value>` appended to the `spawn-claude.sh` argv.
    /// When `None`, no `--model` flag is emitted at all — the session/CLI
    /// default is preserved end-to-end.
    ///
    /// `effort` (issue #3716): mirrors `model` exactly. When `Some` and
    /// non-empty, the spawned child receives `--effort <level>` appended to
    /// the argv (immediately after any `--model`). When `None` or empty, no
    /// `--effort` flag is emitted at all — the session default reasoning
    /// effort is preserved end-to-end.
    ///
    /// `depends_on` (issue #3729, stacked-PR v1): when `Some(N)`, the spawned
    /// child receives `--depends-on <N>` embedded in the `-p` prompt string
    /// (immediately after `--claim-owned`; issue #4121 — NOT a sibling argv
    /// token, since `--depends-on` is not a real `claude` CLI flag),
    /// instructing `/loom:sweep` to branch its worktree/PR off
    /// `feature/issue-<N>`. When `None`, no `--depends-on` text is emitted —
    /// byte-for-byte unchanged behavior. A single optional parent (not a
    /// list) makes diamonds unrepresentable.
    pub fn dispatch(
        &mut self,
        kind: &SweepKind,
        idempotency_key: Option<String>,
        model: Option<&str>,
        effort: Option<&str>,
        depends_on: Option<u32>,
    ) -> Result<DispatchOutcome> {
        self.dispatch_with_model(
            kind,
            idempotency_key,
            DispatchModel::Resolved(model),
            effort,
            depends_on,
        )
    }

    pub(crate) fn dispatch_with_model(
        &mut self,
        kind: &SweepKind,
        idempotency_key: Option<String>,
        model: DispatchModel<'_>,
        effort: Option<&str>,
        depends_on: Option<u32>,
    ) -> Result<DispatchOutcome> {
        self.dispatch_inner(kind, idempotency_key, model, effort, depends_on, None)
    }

    /// Keep the low-level already-resolved API for recovery and registry users.
    pub(crate) fn begin_issue_dispatch(
        &mut self,
        kind: &SweepKind,
        idempotency_key: Option<String>,
        model: Option<&str>,
        effort: Option<&str>,
        depends_on: Option<u32>,
        resume_bypass_pr: Option<u32>,
    ) -> Result<BeginIssueDispatch> {
        self.begin_issue_dispatch_with_model(
            kind,
            idempotency_key,
            DispatchModel::Resolved(model),
            effort,
            depends_on,
            resume_bypass_pr,
        )
    }
}

// Retain the lock-release regression's literal-model seam. Production callers
// carry their unresolved intent through dispatch_model_releasing_poll_lock.
#[cfg(test)]
pub(crate) fn dispatch_issue_releasing_poll_lock(
    registry: &Arc<Mutex<SweepRegistry>>,
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
        DispatchModel::Resolved(model),
        effort,
        depends_on,
    )
}
