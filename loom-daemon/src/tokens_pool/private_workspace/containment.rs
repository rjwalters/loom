//! Verified private-clone containment as runtime-admission evidence (#8787).
//!
//! Codex's native `worktreeIsolation` and `hooks` capabilities stay `partial`
//! — their own live-evidence gate is #4495/#4496 and this module does not move
//! it. What an account-private clone provides is a *different*,
//! execution-specific form of repository isolation: the whole clone at
//! `/workspace/repo` is the worker's owned sandbox, it lives on the account's
//! own named volume inside a validated container, and no host checkout,
//! sibling worktree or peer account's repository is mounted anywhere in it.
//! This module turns that verified state into a [`ContainmentProof`] that
//! runtime admission may accept for exactly one requirement
//! ([`crate::runtime_admission::CONTAINMENT_SATISFIES`]).
//!
//! **Containment replaces the filesystem-isolation obligation and nothing
//! else.** A Docker boundary does not stop an authenticated force-push, does
//! not intercept a Loom lifecycle helper, and does not audit an alternate
//! control route — so a proof is refused unless the `loom-private-control-v1`
//! boundary (#8839) is *also* intact for this exact container: the guard code
//! is image-owned and sealed, the effective guard policy is forced from the
//! host onto the Codex process, the profile's `hooks.json` / `config.toml` /
//! `loom-codex-hooks.json` are read-only mount points, the managed
//! registration names the image-owned bridge, **and** the account profile has
//! established Codex hook trust. The obligation-to-mechanism table lives in
//! `defaults/docs/guardrail-parity-codex.md`.
//!
//! A proof has no public constructor and is neither `Clone` nor `Copy`. It is
//! built only
//!
//! - on the host, from a prepared [`dispatch::Selection`] (owned exclusive
//!   lease, durable job identity, container re-inspected **by ID** with its
//!   settings/mounts/ownership re-validated, control identity re-derived
//!   in-container and matched against the one bound at preparation), and
//! - inside the worker, from the host-bound control identity the transport
//!   placed in the container process's own environment via `docker exec`,
//!   re-observed against the running boundary.
//!
//! Neither route can be reached from a configuration flag, a container label,
//! an environment variable alone, or a test-only toggle: every constructor
//! measures the running system.

use super::*;
use crate::runtime_admission::{
    AdmissionContext, ExecutionProvenance, ResolvedRuntime, RuntimeRejection,
};
use std::collections::BTreeMap;

/// What a proof certifies, recorded verbatim in
/// [`ExecutionProvenance::policy`] so a reader never has to infer it.
pub const POLICY: &str = "clone-isolation+sealed-control-bundle+ro-profile-controls+forced-guard-policy+managed-hook-trusted";

/// Precise unmet obligations. Fixed text only: a refusal is copied into host
/// logs and operator diagnostics, so it must never carry worker output,
/// operator configuration or a profile path's contents.
pub(super) const UNPREPARED: &str = "exclusive writer: this launch holds no prepared private-clone selection (owned account lease plus durable job identity), so no containment can be proven for it";
pub(super) const DRIFTED: &str = "exclusive writer and stale-context refusal: the bound private session container was replaced, renamed, stopped or re-attached between preparation and admission; recover the session before dispatch";
const UNMANAGED: &str = "protected remote operations and Loom lifecycle controls: this private clone ships no Loom hook provisioner, so no managed pre_tool_use bridge is registered for the session; container isolation alone does not prevent an authenticated force-push or a Loom lifecycle mutation";
const UNTRUSTED: &str = "protected remote operations and Loom lifecycle controls: the account profile has not established Codex hook trust since Loom's managed registration was installed, and an untrusted hook fails OPEN on the pinned CLI; accept the hook-trust prompt once for this profile with CODEX_HOME at the session's mount point (Codex keys trust by that hooks.json path, so trust accepted on the host does not count), then stop and start the session (Loom never passes --dangerously-bypass-hook-trust)";
const NOT_IN_CONTAINER: &str = "container identity: this process is not running inside the bound private session container, or the clone's identity record does not match the bound job; refusing to claim containment";
const NOT_OWNED_CLONE: &str = "host, sibling and peer repository isolation: /workspace/repo is not an owned, independent Git root (missing, symlinked, or carrying object alternates)";
const MALFORMED: &str = "container identity: the bound container, control identity or base revision is malformed, absent, or predates loom-private-control-v1";

/// Evidence that one launch runs inside a verified private clone whose
/// control boundary is intact. Deliberately neither `Clone` nor constructible
/// outside this module.
#[derive(Debug)]
pub struct ContainmentProof {
    runtime: &'static str,
    account: String,
    container_id: String,
    control: String,
    base_revision: String,
}

impl ContainmentProof {
    fn new(account: &str, container_id: &str, control: &str, base_revision: &str) -> Result<Self> {
        if !hex(container_id, 64)
            || !hex(control, 64)
            || !(hex(base_revision, 40) || hex(base_revision, 64))
        {
            bail!("{MALFORMED}");
        }
        Ok(Self {
            runtime: "codex",
            account: account.to_owned(),
            container_id: container_id.to_owned(),
            control: control.to_owned(),
            base_revision: base_revision.to_owned(),
        })
    }

    /// Synthetic proof for admission unit tests only. Compiled out of every
    /// non-test build, so no shipped code path — flag, environment variable or
    /// configuration key — can reach it.
    #[cfg(test)]
    pub(crate) fn fixture(account: &str) -> Self {
        Self::new(account, &"a".repeat(64), &"c".repeat(64), &"b".repeat(40)).unwrap()
    }

    /// The single runtime this proof is bound to.
    #[must_use]
    pub fn runtime(&self) -> &str {
        self.runtime
    }

    /// The bound `loom-private-control-v1` identity, for the launch path to
    /// re-check against the container it is about to exec into.
    #[must_use]
    pub fn control(&self) -> &str {
        &self.control
    }

    #[must_use]
    pub fn provenance(
        &self,
        satisfied: Vec<String>,
        native: BTreeMap<String, String>,
    ) -> ExecutionProvenance {
        let short = |value: &str| value.chars().take(12).collect::<String>();
        ExecutionProvenance {
            mode: MODE.into(),
            satisfied,
            native,
            policy: POLICY.into(),
            account: self.account.clone(),
            container: short(&self.container_id),
            control: short(&self.control),
            protocol: PROTOCOL.into(),
            base_revision: self.base_revision.clone(),
        }
    }
}

fn hex(value: &str, len: usize) -> bool {
    value.len() == len && value.chars().all(|c| c.is_ascii_hexdigit())
}

/// Roles that mutate the repository and therefore need the managed policy
/// bridge in addition to containment. Mirrors `spawn-codex.sh`'s
/// `LOOM_CODEX_MUTABLE_ROLES`, plus `sweep-lifecycle`: a full sweep is one
/// runtime launch that runs the Builder and Doctor phases in-process.
#[must_use]
pub fn mutable(role: &str) -> bool {
    matches!(
        crate::runtime_admission::canonical_role(role),
        Some("builder" | "doctor" | "sweep-lifecycle")
    )
}

// ---------------------------------------------------------------------------
// The obligations containment does NOT cover, proven per launch.
// ---------------------------------------------------------------------------

/// Every control-boundary obligation a mutable role needs beyond repository
/// isolation, checked against one observed [`bundle::Report`] and the
/// canonical account profile.
///
/// The caller must already have run [`bundle::accept`]/[`bundle::rebind`] on
/// this report, which is what establishes that the bundle is sealed and
/// digest-intact, the forced policy map is the supported one, the profile's
/// three control files are read-only mount points, and the container's view
/// of them is byte-identical to the host's. What remains, and is checked
/// here, is that the boundary is actually *enforcing* for this clone:
///
/// 1. the clone ships Loom's hook provisioner, so a managed registration is
///    mandatory rather than absent-and-acceptable (`Report::managed`), and
/// 2. Codex hook trust has been established for the profile — without it the
///    registration is read and then ignored, silently, by the pinned CLI.
///
/// Both inputs — `hooks.json`, `config.toml` and `loom-codex-hooks.json` — are
/// members of [`bundle::PROFILE_CONTROLS`] and therefore part of the bound
/// control identity, so a verdict taken here is pinned by the same recheck
/// that pins the rest of the boundary at spawn and at exec.
pub(super) fn enforcing(report: &bundle::Report, profile: &Path) -> Result<()> {
    if !report.managed {
        bail!("{UNMANAGED}");
    }
    if !hook_trust_established(profile) {
        bail!("{UNTRUSTED}");
    }
    Ok(())
}

/// The trust rule of `provision-codex-hooks.sh verify`
/// ([`codex_hooks::trust_at`](super::super::codex_hooks::trust_at)), at the
/// one location a private session runs Codex from: `CODEX_HOME` =
/// [`PROFILE`], the container's mount point. Only a `trusted_hash` keyed to
/// Loom's entry under that path counts; trust taken on the host, for another
/// profile, or for another hook is trust for a hook Codex will not run here.
fn hook_trust_established(profile: &Path) -> bool {
    super::super::codex_hooks::trust_at(profile, Path::new(PROFILE)).0
}

// ---------------------------------------------------------------------------
// Host side.
// ---------------------------------------------------------------------------

/// Build the host-side proof for an already re-validated container and its
/// freshly re-observed control boundary. Called only by
/// [`dispatch::Selection::contain`], which owns the lease and has just
/// re-inspected the container by ID.
pub(super) fn host_proof(
    config: &Config,
    report: &bundle::Report,
    job: &lease::Job,
) -> Result<ContainmentProof> {
    enforcing(report, &config.profile)?;
    ContainmentProof::new(&config.account, &job.container_id, &job.control, &job.base_revision)
}

/// Resolve the model a candidate runtime would launch with, so account
/// selection during preparation is model-aware in exactly the way the launch
/// will be. Supplied by the dispatch path that owns model resolution.
pub type ModelFor<'a> = Box<dyn Fn(&str) -> Option<String> + 'a>;

/// Prepare a fresh private selection and try to satisfy an eligible
/// `rejection` with it. The selection — and with it the exclusive account
/// lease — is dropped on every refusal, so a rejected candidate never keeps an
/// account reserved. Docker work happens here, which every caller runs outside
/// its scheduler/registry lock.
#[allow(clippy::too_many_arguments)]
pub fn admit_new(
    root: &Path,
    role: &str,
    explicit: Option<&str>,
    rejection: RuntimeRejection,
    model: Option<&str>,
    kind: JobKind,
    issue: Option<u64>,
    owner: &str,
) -> Result<(ResolvedRuntime, dispatch::Selection, ContainmentProof), RuntimeRejection> {
    if !rejection.containment_eligible() {
        return Err(rejection);
    }
    let selection = match dispatch::Selection::prepare(root, "codex", model, kind, issue, owner) {
        Ok(Some(selection)) => selection,
        Ok(None) => {
            return Err(rejection.with_containment_failure(
                "host, sibling and peer repository isolation: no private-clone workspace is \
                 configured for the selected Codex account (bare-host, shared-mount and ambient \
                 sessions keep the native partial capability and are refused here)",
            ))
        }
        Err(error) => return Err(rejection.with_containment_failure(&error.to_string())),
    };
    let proof = match selection.contain(root) {
        Ok(proof) => proof,
        Err(error) => return Err(rejection.with_containment_failure(&error.to_string())),
    };
    let admitted = admit_with(root, role, explicit, &proof)?;
    if let Err(error) = selection.record_admission(&admitted) {
        return Err(RuntimeRejection {
            role: admitted.role,
            runtime: admitted.runtime,
            source: admitted.source,
            unmet_capabilities: vec![crate::runtime_admission::CONTAINMENT_SATISFIES.into()],
            reason: format!("could not record containment provenance: {error}"),
        });
    }
    log::info!(
        "runtime_admission: {role} admitted on codex via verified private-clone containment — \
         {}{} (#8787)",
        crate::runtime_admission::CONTAINMENT_LOG_MARKER,
        admitted
            .execution
            .as_ref()
            .map(ExecutionProvenance::summary)
            .unwrap_or_default()
    );
    Ok((admitted, selection, proof))
}

/// Admission against an existing proof. A thin, single-purpose wrapper so no
/// call site has to name [`AdmissionContext`] itself.
///
/// # Errors
/// The ordinary [`RuntimeRejection`] when any requirement other than
/// repository isolation is unmet, or when the proof is bound to a different
/// runtime than the binding chose.
pub fn admit_with(
    root: &Path,
    role: &str,
    explicit: Option<&str>,
    proof: &ContainmentProof,
) -> Result<ResolvedRuntime, RuntimeRejection> {
    crate::runtime_admission::resolve_and_admit_in(
        root,
        role,
        explicit,
        AdmissionContext::PrivateClone(proof),
    )
}

/// The containment side of one dispatch decision. At most one private
/// selection is held at a time; a candidate the preference walk passes over
/// releases it (and its lease) before the walk moves on.
pub struct Preparer<'a> {
    root: PathBuf,
    kind: JobKind,
    issue: Option<u64>,
    owner: String,
    model_for: ModelFor<'a>,
    held: Option<(dispatch::Selection, ContainmentProof, Option<String>)>,
}

impl<'a> Preparer<'a> {
    #[must_use]
    pub fn new(
        root: &Path,
        kind: JobKind,
        issue: Option<u64>,
        owner: String,
        model_for: ModelFor<'a>,
    ) -> Self {
        Self {
            root: root.to_owned(),
            kind,
            issue,
            owner,
            model_for,
            held: None,
        }
    }

    /// Hand the prepared selection — and the model it was selected for — to
    /// the launch path. `None` when no contained candidate was admitted.
    pub fn take(&mut self) -> Option<(dispatch::Selection, Option<String>)> {
        self.held
            .take()
            .map(|(selection, _, model)| (selection, model))
    }
}

impl crate::runtime_preference::ContainmentPreparer for Preparer<'_> {
    fn contain(
        &mut self,
        role: &str,
        explicit: Option<&str>,
        rejection: RuntimeRejection,
    ) -> Result<ResolvedRuntime, RuntimeRejection> {
        if !rejection.containment_eligible() {
            return Err(rejection);
        }
        // One dispatch holds one account lease. A re-admission during the same
        // decision (the static pass, then the preference walk) reuses the proof
        // already prepared instead of selecting a second account.
        if let Some((_, proof, _)) = &self.held {
            return admit_with(&self.root, role, explicit, proof);
        }
        let model = (self.model_for)("codex");
        let (admitted, selection, proof) = admit_new(
            &self.root,
            role,
            explicit,
            rejection,
            model.as_deref(),
            self.kind,
            self.issue,
            &self.owner,
        )?;
        self.held = Some((selection, proof, model));
        Ok(admitted)
    }

    fn release(&mut self) {
        self.held = None;
    }
}

// ---------------------------------------------------------------------------
// Worker side: runs inside the private session container.
// ---------------------------------------------------------------------------

/// Build the worker-side proof, immediately before the model is exec'd.
///
/// `bound` is the control identity the host wrote onto **this container
/// process** through `docker exec --env` after re-validating the container and
/// lease; a worker cannot write it. The boundary is then re-observed from the
/// image-owned bundle and the read-only profile controls, and must still hash
/// to that same identity.
pub(super) fn in_container(account: &str, bound: &str, revision: &str) -> Result<ContainmentProof> {
    let profile = Path::new(PROFILE);
    let report = bundle::observe(Path::new(bundle::CONTROL_ROOT), profile);
    bundle::rebind(&report, profile, bound)?;
    enforcing(&report, profile)?;
    let id = in_bound_container(revision)?;
    ContainmentProof::new(account, &id, bound, revision)
}

/// Prove this process really is inside the container the host bound, and that
/// the clone under it is still an owned, independent Git root. Returns the
/// container ID taken from the clone's own identity record.
///
/// A bare host process cannot satisfy this: it needs the private clone's
/// identity record at `/workspace`, a hostname equal to the bound container's
/// short ID, a read-only root filesystem (one of the settings
/// `docker::validate_settings` requires of the session), and an owned
/// `/workspace/repo` with no object alternates.
fn in_bound_container(revision: &str) -> Result<String> {
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(Path::new(ROOT).join("identity.json"))?)?;
    let id = record["container_id"].as_str().unwrap_or_default();
    if record["protocol"] != PROTOCOL
        || !hex(id, 64)
        || record["revision"] != revision
        || std::env::var("LOOM_PRIVATE_REPOSITORY").ok().as_deref() != record["repository"].as_str()
    {
        bail!("{NOT_IN_CONTAINER}");
    }
    let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname")?;
    if hostname.trim() != &id[..12] || !read_only_root()? {
        bail!("{NOT_IN_CONTAINER}");
    }
    let repo = Path::new(REPO);
    if repo.canonicalize()? != repo
        || !std::fs::symlink_metadata(repo.join(".git"))?.is_dir()
        || repo.join(".git/objects/info/alternates").exists()
    {
        bail!("{NOT_OWNED_CLONE}");
    }
    Ok(id.to_owned())
}

fn read_only_root() -> Result<bool> {
    let path = std::ffi::CString::new("/")?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(stat.f_flag & libc::ST_RDONLY != 0)
}
