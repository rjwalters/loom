//! Verified private-clone containment as runtime-admission evidence (#8787).
//!
//! Codex's native `worktreeIsolation` stays `partial` (the managed hook bridge
//! is not yet live-proven, #4495). An account-private clone inside a validated
//! container is a *different*, execution-specific form of repository
//! isolation: the whole clone is the worker's owned sandbox and no host
//! checkout, sibling or peer repository is mounted. This module turns that
//! verified state into a [`ContainmentProof`] that admission may accept for
//! exactly one requirement ([`crate::runtime_admission::CONTAINMENT_SATISFIES`]).
//!
//! A proof has no public constructor. It is built only
//!
//! - on the host, from a prepared [`dispatch::Selection`] (owned lease, durable
//!   job identity, Docker settings/mounts/ownership re-validated by container
//!   ID) **plus** a passing in-container policy verification, and
//! - inside the worker, from the transport-bound container identity (checked
//!   against the clone's identity record, the container hostname and the
//!   read-only root filesystem) **plus** the same policy verification.
//!
//! Containment replaces only the filesystem-isolation obligation. Protected
//! remote operations, Loom lifecycle controls and destructive commands are
//! still enforced by Loom's managed Codex hook bridge, so a mutable role is
//! refused unless that bridge is installed for the clone, trusted by the
//! profile, and byte-identical to the base revision. See the obligation table
//! in `defaults/docs/guardrail-parity-codex.md`.
use super::*;
use crate::runtime_admission::{
    AdmissionContext, ExecutionProvenance, ResolvedRuntime, RuntimeRejection,
};
use std::collections::BTreeMap;
use std::process::{Command, Stdio};

/// What a proof certifies was verified for the launch, recorded verbatim in
/// [`ExecutionProvenance::policy`].
pub const POLICY: &str = "managed-hooks-trusted+guard-bundle-at-base";

/// Evidence that one launch runs inside a verified private clone. Deliberately
/// neither `Clone` nor constructible outside this module.
#[derive(Debug)]
pub struct ContainmentProof {
    runtime: &'static str,
    account: String,
    container_id: String,
    base_revision: String,
}

impl ContainmentProof {
    fn verified(account: &str, container_id: &str, base_revision: &str) -> Self {
        Self {
            runtime: "codex",
            account: account.to_owned(),
            container_id: container_id.to_owned(),
            base_revision: base_revision.to_owned(),
        }
    }

    /// Synthetic proof for admission unit tests only; compiled out of every
    /// non-test build, so no runtime path (flag, env var, config) reaches it.
    #[cfg(test)]
    pub(crate) fn fixture(account: &str) -> Self {
        Self::verified(account, &"a".repeat(64), &"b".repeat(40))
    }

    /// The single runtime the proof is bound to.
    #[must_use]
    pub fn runtime(&self) -> &str {
        self.runtime
    }

    #[must_use]
    pub fn provenance(
        &self,
        satisfied: Vec<String>,
        native: BTreeMap<String, String>,
    ) -> ExecutionProvenance {
        ExecutionProvenance {
            mode: MODE.into(),
            satisfied,
            native,
            policy: POLICY.into(),
            account: self.account.clone(),
            container: self.container_id.chars().take(12).collect(),
            protocol: PROTOCOL.into(),
            base_revision: self.base_revision.clone(),
        }
    }
}

/// Roles that mutate the repository and therefore need the managed policy
/// bridge in addition to containment. Mirrors `spawn-codex.sh`'s
/// `LOOM_CODEX_MUTABLE_ROLES` (a full sweep runs the Builder/Doctor phases).
#[must_use]
pub fn mutable(role: &str) -> bool {
    matches!(
        crate::runtime_admission::canonical_role(role),
        Some("builder" | "doctor" | "sweep-lifecycle")
    )
}

// ---------------------------------------------------------------------------
// Worker side: runs inside the private session container.
// ---------------------------------------------------------------------------

/// Fixed status codes only cross the Docker boundary (see `SetupReport`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum PolicyStatus {
    Ready,
    ContextInvalid,
    GuardModified,
    HooksNotReady,
    Failed,
}

impl PolicyStatus {
    /// The precise unmet obligation, for refusals. Fixed text: never copies
    /// worker output, configuration or credentials into host diagnostics.
    pub(super) fn obligation(self) -> &'static str {
        match self {
            Self::Ready => "none",
            Self::ContextInvalid => "container identity: the private execution context does not match the bound container, clone identity or base revision (stale or replaced session); recover the session before dispatch",
            Self::GuardModified => "control/guard integrity: the managed guard bundle (hooks, guard libraries, hook provisioner or Loom config layers) in the private clone differs from the base revision or has untracked additions; restore it before admitting a mutable role",
            Self::HooksNotReady => "protected remote operations and Loom lifecycle controls: Loom's managed Codex pre_tool_use hook is not installed for /workspace/repo or not trusted by this account profile; accept the hook-trust prompt once inside the session container (Loom never bypasses hook trust)",
            Self::Failed => "policy verification could not complete inside the private session; update the session image and inspect the session locally",
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PolicyReport {
    pub protocol: String,
    pub status: PolicyStatus,
}

/// `loom-daemon private-workspace verify-policy` entry point.
pub(super) fn report(container_id: &str, revision: &str) -> PolicyReport {
    PolicyReport {
        protocol: PROTOCOL.into(),
        status: verify_local(container_id, revision),
    }
}

pub(super) fn verify_local(container_id: &str, revision: &str) -> PolicyStatus {
    if evidence(container_id, revision).is_err() {
        return PolicyStatus::ContextInvalid;
    }
    let repo = Path::new(REPO);
    match guard_integrity(repo, revision) {
        Ok(()) => {}
        Err(_) => return PolicyStatus::GuardModified,
    }
    match hooks_ready(repo) {
        Ok(true) => PolicyStatus::Ready,
        Ok(false) => PolicyStatus::HooksNotReady,
        Err(_) => PolicyStatus::Failed,
    }
}

/// Host-side proof, for [`dispatch::Selection::contain`] only, after it has
/// re-validated the bound container and received a `Ready` policy report.
pub(super) fn proof_for(
    account: &str,
    container_id: &str,
    revision: &str,
) -> Result<ContainmentProof> {
    if !hex(container_id, 64) || !(hex(revision, 40) || hex(revision, 64)) {
        bail!("invalid private execution identity");
    }
    Ok(ContainmentProof::verified(account, container_id, revision))
}

/// Worker-side proof for [`super::worker_setup::execute`]. The container ID
/// and base revision come from the host transport (which re-validated the
/// inherited lease and container by ID), never from worker-writable state.
pub(super) fn in_container_proof(
    account: &str,
    container_id: &str,
    revision: &str,
) -> Result<ContainmentProof> {
    match verify_local(container_id, revision) {
        PolicyStatus::Ready => Ok(ContainmentProof::verified(account, container_id, revision)),
        status => bail!("{}", status.obligation()),
    }
}

fn hex(value: &str, len: usize) -> bool {
    value.len() == len && value.chars().all(|c| c.is_ascii_hexdigit())
}

/// Identity of the running container, checked against host-supplied values.
/// A bare host process cannot satisfy it: it needs the clone identity record
/// at `/workspace`, a hostname equal to the bound container's short ID and a
/// read-only root filesystem — the settings `docker::validate` requires.
pub(super) fn evidence(container_id: &str, revision: &str) -> Result<()> {
    if !hex(container_id, 64) || !(hex(revision, 40) || hex(revision, 64)) {
        bail!("invalid private execution identity");
    }
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(Path::new(ROOT).join("identity.json"))?)?;
    if record["protocol"] != PROTOCOL
        || record["container_id"] != container_id
        || record["revision"] != revision
        || std::env::var("LOOM_PRIVATE_REPOSITORY").ok().as_deref() != record["repository"].as_str()
    {
        bail!("private clone identity differs from the bound job");
    }
    let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname")?;
    if hostname.trim() != &container_id[..12] {
        bail!("process is not running in the bound container");
    }
    if !read_only_root()? {
        bail!("container root filesystem is writable");
    }
    let repo = Path::new(REPO);
    if repo.canonicalize()? != repo
        || !std::fs::symlink_metadata(repo.join(".git"))?.is_dir()
        || repo.join(".git/objects/info/alternates").exists()
    {
        bail!("private Git root is not an owned clone");
    }
    Ok(())
}

fn read_only_root() -> Result<bool> {
    let path = std::ffi::CString::new("/")?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(stat.f_flag & libc::ST_RDONLY != 0)
}

/// Git with repository-controlled command hooks disabled. `.git/config` is
/// worker-writable, so no fsmonitor/pager/replace-object setting may run code
/// or redirect what this verification reads.
fn git(repo: &Path, args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>> {
    use std::io::Write;
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
            "--no-replace-objects",
        ])
        .args(args)
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("start guard verification Git")?;
    if let Some(input) = input {
        child.stdin.take().unwrap().write_all(input)?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!("guard verification Git {} failed", args.first().unwrap_or(&""));
    }
    Ok(output.stdout)
}

/// Read one object and prove its content hashes to its name, so a rewritten
/// loose object cannot substitute a different guard tree.
fn verified_object(repo: &Path, kind: &str, oid: &str) -> Result<Vec<u8>> {
    let body = git(repo, &["cat-file", kind, oid], None)?;
    let hashed = git(repo, &["hash-object", "-t", kind, "--stdin"], Some(&body))?;
    if String::from_utf8(hashed)?.trim() != oid {
        bail!("object {oid} does not match its content");
    }
    Ok(body)
}

/// `(mode, kind, oid, name)` entries of a verified tree object.
fn tree_entries(repo: &Path, oid: &str) -> Result<Vec<(String, String, String, String)>> {
    verified_object(repo, "tree", oid)?;
    let listing = git(repo, &["ls-tree", "-z", oid], None)?;
    listing
        .split(|b| *b == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            let entry = std::str::from_utf8(entry)?;
            let (meta, name) = entry.split_once('\t').context("malformed tree entry")?;
            let mut fields = meta.split(' ');
            match (fields.next(), fields.next(), fields.next()) {
                (Some(mode), Some(kind), Some(oid)) => {
                    Ok((mode.into(), kind.into(), oid.into(), name.into()))
                }
                _ => bail!("malformed tree entry"),
            }
        })
        .collect()
}

/// The verified tree ID at `rel` inside `revision`, or `None` when absent.
fn subtree(repo: &Path, revision: &str, rel: &Path) -> Result<Option<(String, String)>> {
    let commit = String::from_utf8(verified_object(repo, "commit", revision)?)?;
    let mut current = (
        "tree".to_owned(),
        commit
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("tree "))
            .context("commit has no tree")?
            .to_owned(),
    );
    for component in rel.components() {
        let name = component.as_os_str().to_str().context("non-UTF-8 path")?;
        if current.0 != "tree" {
            return Ok(None);
        }
        let Some((_, kind, oid, _)) = tree_entries(repo, &current.1)?
            .into_iter()
            .find(|(_, _, _, entry)| entry == name)
        else {
            return Ok(None);
        };
        current = (kind, oid);
    }
    Ok(Some(current))
}

/// Compare one on-disk path with the base revision: identical file content
/// for blobs; for trees, the exact same set of entries,
/// recursively, with no untracked additions. Symlinks are refused outright.
fn matches_revision(repo: &Path, disk: &Path, expected: Option<(String, String)>) -> Result<()> {
    let metadata = std::fs::symlink_metadata(disk).ok();
    match (expected, metadata) {
        (None, None) => Ok(()),
        (None, Some(_)) => bail!("untracked guard input {}", disk.display()),
        (Some(_), None) => bail!("missing guard input {}", disk.display()),
        (Some((kind, oid)), Some(metadata)) => {
            if metadata.file_type().is_symlink() {
                bail!("guard input {} is a symlink", disk.display());
            }
            if kind == "blob" {
                if !metadata.is_file() {
                    bail!("guard input {} is not a file", disk.display());
                }
                let hashed = git(
                    repo,
                    &[
                        "hash-object",
                        "--no-filters",
                        "--",
                        disk.to_str().context("path")?,
                    ],
                    None,
                )?;
                if String::from_utf8(hashed)?.trim() != oid {
                    bail!("guard input {} was modified", disk.display());
                }
                return Ok(());
            }
            if kind != "tree" || !metadata.is_dir() {
                bail!("guard input {} changed type", disk.display());
            }
            let entries = tree_entries(repo, &oid)?;
            let mut names = std::collections::BTreeSet::new();
            for (mode, kind, oid, name) in entries {
                if mode == "120000" || kind == "commit" {
                    bail!("guard tree contains a symlink or submodule");
                }
                names.insert(name.clone());
                matches_revision(repo, &disk.join(&name), Some((kind, oid)))?;
            }
            for entry in std::fs::read_dir(disk)? {
                if !names.contains(entry?.file_name().to_str().unwrap_or("")) {
                    bail!("untracked guard input in {}", disk.display());
                }
            }
            Ok(())
        }
    }
}

/// Paths the managed Codex policy executes or reads, relative to the clone:
/// the canonical hook directory (bridge + shared guards), the guard libraries
/// the bridge/guards source, the hook provisioner that proves readiness, and
/// every repository config layer that carries `guards.*` toggles.
fn guard_inputs(repo: &Path) -> Result<Vec<PathBuf>> {
    let inside = |path: PathBuf| -> Result<PathBuf> {
        let canonical = path
            .canonicalize()
            .with_context(|| format!("guard input {} is missing", path.display()))?;
        Ok(canonical
            .strip_prefix(repo)
            .context("guard input resolves outside the private clone")?
            .to_owned())
    };
    let bridge = inside(repo.join(".loom/hooks/guard-codex-bridge.sh"))?;
    let hooks = bridge
        .parent()
        .context("bridge has no directory")?
        .to_owned();
    let mut inputs = vec![
        hooks.clone(),
        inside(repo.join(".loom/scripts/provision-codex-hooks.sh"))?,
    ];
    let lib = repo.join(&hooks).join("../scripts/lib");
    if lib.exists() {
        inputs.push(inside(lib)?);
    }
    // Config layers are checked by name: absent at the base revision means
    // they must be absent on disk too.
    inputs.extend(
        [
            ".loom/config.json",
            ".loom-project/project.json",
            ".loom-local/local.json",
        ]
        .into_iter()
        .map(PathBuf::from),
    );
    Ok(inputs)
}

pub(super) fn guard_integrity(repo: &Path, revision: &str) -> Result<()> {
    for rel in guard_inputs(repo)? {
        matches_revision(repo, &repo.join(&rel), subtree(repo, revision, &rel)?)?;
    }
    Ok(())
}

/// The same readiness proof `spawn-codex.sh` requires for mutable roles on a
/// host — installed, pinned, pointing at THIS clone's bridge, and trusted by
/// the profile — run against the clone's (integrity-checked) provisioner.
fn hooks_ready(repo: &Path) -> Result<bool> {
    let status = Command::new("bash")
        .arg(repo.join(".loom/scripts/provision-codex-hooks.sh"))
        .args([
            "verify",
            "--codex-home",
            PROFILE,
            "--workspace",
            REPO,
            "--json",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("run hook readiness verification")?;
    Ok(status.success())
}

// ---------------------------------------------------------------------------
// Host side.
// ---------------------------------------------------------------------------

/// Resolve the model a candidate runtime would launch with, for model-aware
/// account selection during preparation.
pub type ModelFor<'a> = Box<dyn Fn(&str) -> Option<String> + 'a>;

/// Prepare a fresh private selection and try to satisfy an eligible
/// `rejection` with it. The selection (and its lease) is dropped on every
/// refusal, so a rejected candidate never keeps an account reserved.
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
                "no verified private-clone workspace is configured for the selected Codex account (host, shared-mount and ambient sessions keep native partial isolation)",
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
        "runtime_admission: {role} admitted on codex via verified private-clone containment — {}{} (#8787)",
        crate::runtime_admission::CONTAINMENT_LOG_MARKER,
        admitted
            .execution
            .as_ref()
            .map(ExecutionProvenance::summary)
            .unwrap_or_default()
    );
    Ok((admitted, selection, proof))
}

/// Admission against an existing proof, recording provenance on success.
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
/// selection is held; a candidate the preference walk passes over releases
/// it. Docker work happens only here, which callers run outside scheduler and
/// registry locks.
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

    /// Hand the prepared selection and the model it was selected for to the
    /// launch path. `None` when no contained candidate was admitted.
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
        // One dispatch holds one account lease: re-admission (a preference
        // walk after the static check) reuses the proof already prepared.
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
