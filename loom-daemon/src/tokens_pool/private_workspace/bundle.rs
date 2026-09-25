//! Versioned private-session control boundary (issue #8839).
//!
//! A private session owns a writable clone and a read-write account profile, so
//! every guard input that lives in either of them is worker-mutable: deleting
//! `defaults/hooks/guard-codex-bridge.sh` removed enforcement outright, and a
//! `.loom/config.json` carrying `guards.forceScope: "off"` turned a denied
//! force-push over a protected ref into an allowed one. Both were demonstrated
//! against the production bridge with disposable fixtures.
//!
//! This module is the boundary that closes them, and the versioned protocol
//! #8787 consumes. Three properties, each enforced rather than configured:
//!
//! 1. **Guard code is image-owned.** Every guard script and every library they
//!    source lives under [`CONTROL_ROOT`] on the session image's read-only
//!    rootfs. The registered Codex hook names that absolute path, so the
//!    worker-writable copies in its clone are not on the enforcement path at
//!    all. The bundle ships the generic guard under BOTH names the bridge may
//!    invoke, which also removes `guard-destructive.sh`'s canonical-guard
//!    indirection through `$LOOM_PROJECT_ROOT/.claude/skills/...` — a
//!    worker-writable path it would otherwise prefer.
//! 2. **Effective policy is forced through a channel the worker cannot
//!    reach.** Every guard category toggle has a documented `LOOM_*` override
//!    that wins over every config tier; [`POLICY`] pins the protective value
//!    for all of them in the Codex process's own environment. A worker can
//!    still write `.loom/config.json` (or invent a higher tier, or a deeper
//!    config root) and change nothing about the decision.
//! 3. **Identity is bound and rechecked.** The manifest, the recomputed asset
//!    digests, the hook registration, the readiness receipt and the profile's
//!    control files hash to one identity string that is bound into the account
//!    lease alongside the container ID, verified again immediately before
//!    execution on the host, and verified a third time in-container by the
//!    process that execs the model. A replaced image, a stale control version,
//!    an altered policy, a removed hook or a lease mismatch is refused before
//!    any mutable work.
//!
//! Authentication is deliberately untouched: `auth.json` stays writable in the
//! canonical profile mount, no credential byte enters the bundle, the clone, the
//! manifest or the image, and nothing here fabricates or bypasses hook trust.
//! Mutable admission remains refused — #4496 is still the live production gate.
use super::*;
use std::collections::BTreeMap;

/// Wire identity of this control boundary. A worker-visible bundle that does
/// not answer with exactly this string is refused, so an older or newer image
/// cannot be silently accepted by a host that expects different semantics.
pub const CONTROL_PROTOCOL: &str = "loom-private-control-v1";
/// Monotonic revision inside the protocol, bumped when the asset set or the
/// pinned policy changes in a way that invalidates an already-bound identity.
pub const CONTROL_VERSION: u32 = 1;
/// Image-owned bundle root. Lives on the read-only rootfs and is deliberately
/// NOT a mount destination, so the enumerated mount inventory
/// (`docker::validate_settings`) cannot be used to shadow it.
pub const CONTROL_ROOT: &str = "/opt/loom/private-control";
/// Managed-hook version marker; must equal `provision-codex-hooks.sh`'s
/// `LOOM_HOOK_VERSION`, which is verified against the receipt at every check.
pub const HOOK_VERSION: u32 = 1;
/// Codex CLI floor this boundary is proven against. The bundle records the
/// version actually present in the image at seal time rather than trusting a
/// build argument; see `defaults/docs/private-control-bundle.md` for the exact
/// tested combinations.
pub const CODEX_FLOOR: &str = "0.146.0";

/// Guard code the bundle must carry. `guard-destructive.sh` is a copy of the
/// generic guard on purpose (see the module docs).
const GUARDS: [&str; 5] = [
    "hooks/guard-codex-bridge.sh",
    "hooks/guard-destructive.sh",
    "hooks/guard-destructive-generic.sh",
    "hooks/guard-loom-workflow.sh",
    "hooks/guard-worktree-paths.sh",
];
/// Libraries the guards source, resolved bundle-relative (`$SCRIPT_DIR/../`),
/// so no sourced byte comes from the clone either.
const LIBS: [&str; 5] = [
    "scripts/lib/canonical-path.sh",
    "scripts/lib/config-resolver.sh",
    "scripts/lib/default-branch.sh",
    "scripts/lib/installed-file-guard.sh",
    "scripts/lib/worktree-root.sh",
];
/// The hook provisioner. It produces and checks the readiness evidence, so
/// running the clone's copy would let a worker certify its own readiness by
/// replacing one tracked file; the bundle's copy is the only one a private
/// session ever executes. Its own default bridge resolution
/// (`$SCRIPT_DIR/../hooks/guard-codex-bridge.sh`) lands inside this bundle.
pub const PROVISIONER: &str = "scripts/provision-codex-hooks.sh";

/// Every sealed asset, in a stable order.
fn sealed_assets() -> impl Iterator<Item = &'static str> {
    GUARDS
        .iter()
        .chain(LIBS.iter())
        .copied()
        .chain(std::iter::once(PROVISIONER))
}

/// The effective guard policy a private session enforces, as environment
/// overrides. Each entry is the documented `LOOM_*` override for one guard
/// category toggle, at its protective value; every one of them wins over
/// `.loom/config.json` and every higher config tier, so no worker-writable
/// configuration can weaken a category.
///
/// `guards.reversibleGh` is deliberately absent: it is an opt-in ask that is
/// off by default, and forcing it on would convert trivially reversible forge
/// operations into headless denials without protecting anything.
pub const POLICY: [(&str, &str); 10] = [
    ("LOOM_FORCE_SCOPE", "protected"),
    ("LOOM_RM_SCOPE", "repo"),
    ("LOOM_GUARD_SQL", "1"),
    ("LOOM_GUARD_CLOUD", "1"),
    ("LOOM_GUARD_CARGO_CLEAN", "1"),
    ("LOOM_GUARD_STASH_SCOPE", "1"),
    ("LOOM_GUARD_WORKTREE_ISOLATION", "1"),
    ("LOOM_GUARD_INSTALLED_FILE_WRITES", "1"),
    ("LOOM_GUARD_WORKSPACE_REGISTRY", "1"),
    // The read-only fast path is an allow-path whose `readOnlyFastPathExtra`
    // list is a per-command-word bypass a worker could extend from its own
    // clone. Off costs forks, never decisions: a genuinely read-only command
    // matches no deny/ask pattern on the full path either.
    ("LOOM_GUARD_READONLY_FASTPATH", "0"),
];

/// Sealed, image-owned description of one control bundle.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub protocol: String,
    pub control_version: u32,
    /// Codex CLI version observed in the image when the bundle was sealed.
    pub codex_cli: String,
    /// Codex hook schema the bridge in this bundle is pinned to.
    pub codex_schema_pin: String,
    /// Bundle-relative path -> SHA-256 of the sealed content.
    pub assets: BTreeMap<String, String>,
    /// Forced guard policy (see [`POLICY`]).
    pub policy: BTreeMap<String, String>,
    /// The exact `hooks.json` command the managed registration must carry.
    pub registration: String,
}

/// Why a control boundary was refused. Only fixed variants cross the Docker
/// boundary: a diagnostic could carry operator configuration.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    Ready,
    /// No bundle at all — an image without the control boundary.
    Missing,
    /// Manifest present but its protocol/version/policy is not this one.
    Unsupported,
    /// A sealed asset's content no longer matches the manifest.
    Mutated,
    /// The bundle is writable by the worker, so nothing above it is provable.
    Writable,
    /// The managed hook registration or its readiness receipt is missing,
    /// deregistered, or no longer names the image-owned bridge.
    Registration,
}

/// Worker-side observation of the control boundary, evaluated in-container.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Report {
    pub protocol: String,
    pub control_version: u32,
    pub status: Status,
    pub codex_cli: String,
    /// One digest over the whole observed boundary (see [`identity`]).
    pub identity: String,
    /// Digests of the account profile's control files as the container sees
    /// them, cross-checked against the host's own read of the same files.
    pub profile: BTreeMap<String, String>,
    /// Whether the clone ships Loom's Codex hook provisioner, which makes a
    /// managed registration naming the image-owned bridge mandatory. A
    /// repository with no installed Loom surface has no managed hook to
    /// register, and its pre-#8839 behavior (no bridge, capability still
    /// `partial`, mutable roles still refused by runtime admission) is
    /// unchanged. A worker cannot downgrade itself into that mode: the
    /// provisioner is tracked, so deleting it leaves the clone dirty and the
    /// next admission's `prepare` refuses reuse before this report is taken.
    pub managed: bool,
}

/// Files in the account profile that carry hook registration, Codex-owned
/// trust state, and Loom's readiness receipt. The worker must not be able to
/// swap any of them without the boundary noticing.
pub const PROFILE_CONTROLS: [&str; 3] = ["hooks.json", "config.toml", "loom-codex-hooks.json"];

fn digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

fn file_digest(path: &Path) -> Result<String> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("control asset is absent: {}", path.display()))?;
    if !metadata.is_file() {
        bail!("control asset is not a regular file: {}", path.display());
    }
    Ok(digest(&std::fs::read(path)?))
}

/// Digests of the profile's control files, missing entries recorded as empty so
/// a removed registration is a value mismatch rather than an absent key.
pub fn profile_digests(profile: &Path) -> BTreeMap<String, String> {
    PROFILE_CONTROLS
        .iter()
        .map(|name| {
            let value = std::fs::read(profile.join(name)).map(|b| digest(&b));
            ((*name).to_owned(), value.unwrap_or_default())
        })
        .collect()
}

/// The registration command the managed Codex hook must carry: the image-owned
/// bridge, the fixed private workspace, and the managed-hook version marker.
#[must_use]
pub fn registration() -> String {
    format!("{CONTROL_ROOT}/hooks/guard-codex-bridge.sh --project-root {REPO} --loom-hook-version {HOOK_VERSION}")
}

fn expected_policy() -> BTreeMap<String, String> {
    POLICY
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

/// One digest over everything that must not change between admission and
/// execution. Canonical JSON (serde_json over `BTreeMap`s) so the same boundary
/// always hashes identically.
fn identity(manifest: &Manifest, report: &Report) -> Result<String> {
    Ok(digest(&serde_json::to_vec(&serde_json::json!({
        "protocol": CONTROL_PROTOCOL,
        "control_version": CONTROL_VERSION,
        "manifest": manifest,
        "profile": report.profile,
        "managed": report.managed,
    }))?))
}

fn parse_version(value: &str) -> Option<(u64, u64, u64)> {
    let digits: String = value
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let mut parts = digits.split('.').map(str::parse::<u64>);
    match (parts.next(), parts.next(), parts.next()) {
        (Some(Ok(a)), Some(Ok(b)), Some(Ok(c))) => Some((a, b, c)),
        _ => None,
    }
}

/// Seal a staged bundle: prove every declared asset is present, record its
/// digest and the Codex CLI actually installed alongside it, and write the
/// manifest. Run at image build time, never at runtime — a runtime reseal would
/// make mutation self-certifying.
pub fn seal(root: &Path) -> Result<Manifest> {
    let mut assets = BTreeMap::new();
    for relative in sealed_assets() {
        assets.insert(relative.to_owned(), file_digest(&root.join(relative))?);
    }
    // The bridge dispatches `guard-destructive.sh`; in this bundle that name
    // must BE the vendored generic guard, so no decision can be delegated to a
    // canonical guard inside the worker's clone.
    if assets["hooks/guard-destructive.sh"] != assets["hooks/guard-destructive-generic.sh"] {
        bail!("control bundle must ship the generic guard as guard-destructive.sh; a dispatcher would prefer a worker-writable canonical guard");
    }
    let output = std::process::Command::new("codex")
        .arg("--version")
        .output()
        .context("sealing a control bundle requires the Codex CLI it is sealed against")?;
    if !output.status.success() {
        bail!("Codex CLI did not report a version; refusing to seal an unproven control bundle");
    }
    let codex_cli = String::from_utf8(output.stdout)?.trim().to_owned();
    let (observed, floor) = (
        parse_version(&codex_cli).context("Codex CLI version is unparseable")?,
        parse_version(CODEX_FLOOR).unwrap(),
    );
    if observed < floor {
        bail!("Codex CLI is below the {CODEX_FLOOR} control floor");
    }
    let manifest = Manifest {
        protocol: CONTROL_PROTOCOL.into(),
        control_version: CONTROL_VERSION,
        codex_cli,
        codex_schema_pin: CODEX_FLOOR.into(),
        assets,
        policy: expected_policy(),
        registration: registration(),
    };
    save(&root.join("manifest.json"), &manifest)?;
    Ok(manifest)
}

/// Read a sealed manifest without evaluating it.
pub fn manifest(root: &Path) -> Result<Option<Manifest>> {
    match std::fs::read(root.join("manifest.json")) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).context(
            "control manifest is unreadable; the session image's control bundle is unusable",
        )?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Evaluate the boundary from inside the container: recompute every sealed
/// digest, prove the bundle is not writable, and prove the managed registration
/// still names the image-owned bridge. Never fails — an unprovable boundary is
/// a refusing [`Status`], so the host always gets an answer it can act on.
pub fn observe(root: &Path, profile: &Path) -> Report {
    observe_at(root, profile, Path::new(REPO))
}

/// [`observe`] against an explicit clone root. The container always evaluates
/// the one fixed private clone; the parameter exists so the boundary's own
/// tests can stage both the managed and unmanaged repository shapes.
pub fn observe_at(root: &Path, profile: &Path, clone: &Path) -> Report {
    let mut report = Report {
        protocol: CONTROL_PROTOCOL.into(),
        control_version: CONTROL_VERSION,
        status: Status::Missing,
        codex_cli: String::new(),
        identity: String::new(),
        profile: profile_digests(profile),
        managed: clone
            .join(".loom/scripts/provision-codex-hooks.sh")
            .is_file(),
    };
    let Ok(Some(manifest)) = manifest(root) else {
        return report;
    };
    report.codex_cli.clone_from(&manifest.codex_cli);
    report.status = evaluate(root, profile, &manifest, report.managed);
    if report.status == Status::Ready {
        match identity(&manifest, &report) {
            Ok(value) => report.identity = value,
            Err(_) => report.status = Status::Unsupported,
        }
    }
    report
}

fn evaluate(root: &Path, profile: &Path, manifest: &Manifest, managed: bool) -> Status {
    if manifest.protocol != CONTROL_PROTOCOL
        || manifest.control_version != CONTROL_VERSION
        || manifest.policy != expected_policy()
        || manifest.registration != registration()
        || manifest.codex_schema_pin != CODEX_FLOOR
        || parse_version(&manifest.codex_cli) < parse_version(CODEX_FLOOR)
        || manifest.assets.len() != sealed_assets().count()
        || !sealed_assets().all(|relative| manifest.assets.contains_key(relative))
    {
        return Status::Unsupported;
    }
    for (relative, sealed) in &manifest.assets {
        if file_digest(&root.join(relative)).ok().as_ref() != Some(sealed) {
            return Status::Mutated;
        }
    }
    // A read-only bundle is the whole premise. Prove it positively — in every
    // directory that holds a sealed asset, not just the root — instead of
    // trusting the image's own file modes or the container's declared rootfs.
    // A directory the worker can write is a directory where it can replace a
    // guard by rename even when it cannot open the file itself for writing.
    let mut directories: Vec<PathBuf> = sealed_assets()
        .filter_map(|relative| root.join(relative).parent().map(Path::to_owned))
        .collect();
    directories.push(root.to_owned());
    directories.sort();
    directories.dedup();
    if directories
        .iter()
        .any(|dir| tempfile::NamedTempFile::new_in(dir).is_ok())
    {
        return Status::Writable;
    }
    if managed
        && (registered(profile).as_deref() != Some(&manifest.registration)
            || !receipt_pins(profile))
    {
        return Status::Registration;
    }
    Status::Ready
}

/// The managed hook command currently registered in a profile's `hooks.json`.
fn registered(profile: &Path) -> Option<String> {
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(profile.join("hooks.json")).ok()?).ok()?;
    value["hooks"]["PreToolUse"]
        .as_array()?
        .iter()
        .flat_map(|group| group["hooks"].as_array().into_iter().flatten())
        .filter_map(|hook| hook["command"].as_str())
        .find(|command| command.contains("guard-codex-bridge.sh"))
        .map(str::to_owned)
}

/// Loom's readiness receipt must pin the same registration at the same managed
/// version, so a worker cannot replace the evidence that provisioning happened.
fn receipt_pins(profile: &Path) -> bool {
    let Ok(bytes) = std::fs::read(profile.join("loom-codex-hooks.json")) else {
        return false;
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    let hook = &value["loomManagedHook"];
    hook["command"] == registration().as_str()
        && hook["commandSha256"] == digest(registration().as_bytes()).as_str()
        && hook["version"] == HOOK_VERSION
}

/// Host-side acceptance: the container's observation must be Ready on exactly
/// this protocol, and its view of the account profile's control files must match
/// the host's own read of the canonical profile. Returns the bound identity.
pub fn accept(report: &Report, profile: &Path) -> Result<String> {
    if report.protocol != CONTROL_PROTOCOL || report.control_version != CONTROL_VERSION {
        bail!("private session control protocol is {} v{}, not the supported {CONTROL_PROTOCOL} v{CONTROL_VERSION}; use a matching session image", report.protocol, report.control_version);
    }
    match report.status {
        Status::Ready => {}
        Status::Missing => bail!("private session image ships no {CONTROL_PROTOCOL} control bundle; guard code and effective policy would live in worker-writable paths"),
        Status::Unsupported => bail!("private session control bundle declares a different protocol, version or effective policy; refusing admission on an unproven boundary"),
        Status::Mutated => bail!("private session control bundle no longer matches its sealed digests; refusing admission and preserving the session for inspection"),
        Status::Writable => bail!("private session control bundle is writable by the worker; refusing admission because no guard code or policy input above it is provable"),
        Status::Registration => bail!("private session managed hook registration or readiness receipt does not name the image-owned guard bridge; refusing admission"),
    }
    if report.identity.len() != 64 || !report.identity.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("private session control identity is malformed");
    }
    let host = profile_digests(profile);
    if host != report.profile {
        bail!("private session view of the account profile's control files differs from the host's; refusing admission on a mutated or unfaithful profile mount");
    }
    Ok(report.identity.clone())
}

/// Recheck an already-bound identity. Called immediately before execution, and
/// again in-container by the process that execs the model.
pub fn rebind(report: &Report, profile: &Path, bound: &str) -> Result<()> {
    let observed = accept(report, profile)?;
    if bound.is_empty() {
        bail!("private session lease carries no bound control identity; it predates {CONTROL_PROTOCOL} and must be recovered before mutable work");
    }
    if observed != bound {
        bail!("private session control identity changed after admission (guard code, effective policy, hook registration or readiness receipt); refusing execution");
    }
    Ok(())
}

/// Apply the forced policy to a command. Same map on the host's `docker exec`
/// and on the in-container exec, so neither side can be the only one holding it.
pub fn apply_policy(command: &mut std::process::Command) {
    for (key, value) in POLICY {
        command.env(key, value);
    }
}

/// The forced policy as `NAME=VALUE` entries for the session-exec env list.
#[must_use]
pub fn policy_env() -> Vec<String> {
    POLICY
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn staged() -> (tempfile::TempDir, Manifest) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("bundle");
        for relative in sealed_assets() {
            let path = root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let body = if relative.starts_with("hooks/guard-destructive") {
                "#!/usr/bin/env bash\n# shared generic guard\n"
            } else {
                "#!/usr/bin/env bash\n"
            };
            std::fs::write(&path, body).unwrap();
        }
        let mut assets = BTreeMap::new();
        for relative in sealed_assets() {
            assets.insert(relative.to_owned(), file_digest(&root.join(relative)).unwrap());
        }
        let manifest = Manifest {
            protocol: CONTROL_PROTOCOL.into(),
            control_version: CONTROL_VERSION,
            codex_cli: "codex-cli 0.149.1".into(),
            codex_schema_pin: CODEX_FLOOR.into(),
            assets,
            policy: expected_policy(),
            registration: registration(),
        };
        save(&root.join("manifest.json"), &manifest).unwrap();
        (dir, manifest)
    }

    fn profile(dir: &Path) -> PathBuf {
        let profile = dir.join("profile");
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::write(
            profile.join("hooks.json"),
            serde_json::to_vec(&serde_json::json!({
                "hooks": {"PreToolUse": [{"matcher": "*", "hooks": [
                    {"type": "command", "command": registration(), "timeout": 30}]}]}
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            profile.join("loom-codex-hooks.json"),
            serde_json::to_vec(&serde_json::json!({"loomManagedHook": {
                "version": HOOK_VERSION,
                "command": registration(),
                "commandSha256": digest(registration().as_bytes()),
            }}))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(profile.join("config.toml"), "model='fixture'\n").unwrap();
        profile
    }

    /// A clone shaped like a repository that ships Loom's installed surface, so
    /// a managed hook registration naming the image-owned bridge is required.
    fn clone_root(dir: &Path, managed: bool) -> PathBuf {
        let clone = dir.join("repo");
        if managed {
            std::fs::create_dir_all(clone.join(".loom/scripts")).unwrap();
            std::fs::write(clone.join(".loom/scripts/provision-codex-hooks.sh"), "#!/bin/sh\n")
                .unwrap();
        } else {
            std::fs::create_dir_all(&clone).unwrap();
        }
        clone
    }

    /// Directories a sealed bundle occupies, deepest first so a chmod sequence
    /// over them never locks itself out of a child.
    fn bundle_dirs(root: &Path) -> Vec<PathBuf> {
        let mut dirs = vec![
            root.join("scripts/lib"),
            root.join("scripts"),
            root.join("hooks"),
            root.to_owned(),
        ];
        dirs.retain(|dir| dir.is_dir());
        dirs
    }

    /// The bundle root is read-only in a private session (read-only rootfs plus
    /// root-owned modes). Tests run as the owner, so mode 0o500 is the only
    /// portable way to reproduce "the worker cannot write here".
    fn seal_modes(root: &Path) {
        for path in bundle_dirs(root) {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o500)).unwrap();
        }
    }

    fn sealed() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let (dir, _) = staged();
        let (root, profile) = (dir.path().join("bundle"), profile(dir.path()));
        let clone = clone_root(dir.path(), true);
        seal_modes(&root);
        (dir, root, profile, clone)
    }

    /// Restore owner-writable modes so the temporary directory can be removed.
    fn writable(root: &Path) {
        for path in bundle_dirs(root).into_iter().rev() {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
    }

    #[test]
    fn ready_boundary_binds_one_identity_that_both_sides_recheck() {
        let (dir, root, profile, clone) = sealed();
        let report = observe_at(&root, &profile, &clone);
        assert_eq!(report.status, Status::Ready, "{report:?}");
        let bound = accept(&report, &profile).unwrap();
        assert_eq!(bound.len(), 64);
        rebind(&observe_at(&root, &profile, &clone), &profile, &bound).unwrap();
        // A lease from before this protocol carries no identity to recheck.
        assert!(rebind(&observe_at(&root, &profile, &clone), &profile, "")
            .unwrap_err()
            .to_string()
            .contains("predates"));
        writable(&root);
        drop(dir);
    }

    #[test]
    fn mutated_guard_code_policy_and_registration_each_refuse_before_mutable_work() {
        let (dir, root, profile, clone) = sealed();
        let bound = accept(&observe_at(&root, &profile, &clone), &profile).unwrap();

        // 1. Guard code swapped for a permissive stub (the demonstrated
        //    `guard-codex-bridge.sh` deletion/replacement, reproduced against
        //    the sealed bundle rather than a worker-writable copy).
        writable(&root);
        let bridge = root.join("hooks/guard-codex-bridge.sh");
        std::fs::write(&bridge, "#!/usr/bin/env bash\nexit 0\n").unwrap();
        assert_eq!(observe_at(&root, &profile, &clone).status, Status::Mutated);
        assert!(accept(&observe_at(&root, &profile, &clone), &profile).is_err());
        std::fs::remove_file(&bridge).unwrap();
        assert_eq!(observe_at(&root, &profile, &clone).status, Status::Mutated);

        // 2. A writable bundle is refused even when every digest still matches:
        //    nothing above it is provable at the moment of the decision.
        let (restored, _) = staged();
        let root = restored.path().join("bundle");
        assert_eq!(observe_at(&root, &profile, &clone).status, Status::Writable);

        // 3. A manifest that declares a weaker policy, a different protocol, or
        //    a registration pointing anywhere else is Unsupported.
        for mutate in [
            |m: &mut Manifest| {
                m.policy
                    .insert("LOOM_FORCE_SCOPE".into(), "off".into())
                    .unwrap();
            },
            |m: &mut Manifest| m.protocol = "loom-private-control-v0".into(),
            |m: &mut Manifest| m.control_version = CONTROL_VERSION + 1,
            |m: &mut Manifest| m.registration = format!("{REPO}/.loom/hooks/guard-codex-bridge.sh"),
            |m: &mut Manifest| m.codex_cli = "codex-cli 0.140.0".into(),
        ] {
            let (fresh, mut manifest) = staged();
            let root = fresh.path().join("bundle");
            mutate(&mut manifest);
            save(&root.join("manifest.json"), &manifest).unwrap();
            seal_modes(&root);
            let report = observe_at(&root, &profile, &clone);
            assert_eq!(report.status, Status::Unsupported, "{manifest:?}");
            assert!(accept(&report, &profile).is_err());
            writable(&root);
        }

        // 4. Deregistering the managed hook, or replacing the readiness
        //    receipt, refuses rather than reading as an unhooked session.
        let (fresh, _) = staged();
        let root = fresh.path().join("bundle");
        seal_modes(&root);
        for break_profile in ["hooks.json" as &str, "loom-codex-hooks.json"] {
            let broken = profile.parent().unwrap().join(break_profile);
            std::fs::rename(profile.join(break_profile), &broken).unwrap();
            let report = observe_at(&root, &profile, &clone);
            assert_eq!(report.status, Status::Registration, "{break_profile}");
            assert!(accept(&report, &profile).is_err());
            std::fs::rename(&broken, profile.join(break_profile)).unwrap();
        }

        // 5. A faithful-looking report whose profile digests disagree with the
        //    host's own read of the canonical profile is refused, and an
        //    identity that changed after admission fails the recheck.
        let mut report = observe_at(&root, &profile, &clone);
        assert_eq!(report.status, Status::Ready);
        report
            .profile
            .insert("config.toml".into(), digest(b"fabricated trust"))
            .unwrap();
        assert!(accept(&report, &profile)
            .unwrap_err()
            .to_string()
            .contains("differs from the host"));
        std::fs::write(profile.join("config.toml"), "model='changed'\n").unwrap();
        assert!(rebind(&observe_at(&root, &profile, &clone), &profile, &bound)
            .unwrap_err()
            .to_string()
            .contains("identity changed"));
        writable(&root);
        drop(dir);
    }

    /// A private clone over a repository with no installed Loom surface has no
    /// managed hook to register. Its pre-#8839 behavior is unchanged, the bundle
    /// is still sealed and bound, and it cannot be reached by deleting the
    /// provisioner (that leaves the clone dirty, which `prepare` already
    /// refuses before this report is ever taken).
    #[test]
    fn a_repository_without_loom_helpers_still_binds_a_sealed_boundary() {
        let (dir, root, profile, _) = sealed();
        let bare = clone_root(&dir.path().join("bare"), false);
        let report = observe_at(&root, &profile, &bare);
        assert!(!report.managed);
        assert_eq!(report.status, Status::Ready, "{report:?}");
        let bare_identity = accept(&report, &profile).unwrap();
        // The managed shape is a DIFFERENT identity over the same bundle, so a
        // session cannot silently move between the two modes after admission.
        let managed = observe_at(&root, &profile, &clone_root(dir.path(), true));
        assert!(managed.managed);
        assert_ne!(accept(&managed, &profile).unwrap(), bare_identity);
        writable(&root);
    }

    #[test]
    fn sealing_refuses_a_dispatcher_that_would_prefer_a_worker_writable_guard() {
        let (dir, _) = staged();
        let root = dir.path().join("bundle");
        std::fs::write(
            root.join("hooks/guard-destructive.sh"),
            "#!/usr/bin/env bash\nexec bash \"$LOOM_PROJECT_ROOT/.claude/skills/repo/hooks/guard-destructive.sh\"\n",
        )
        .unwrap();
        assert!(seal(&root)
            .unwrap_err()
            .to_string()
            .contains("worker-writable canonical guard"));
    }

    #[test]
    fn forced_policy_covers_every_category_toggle_and_never_weakens_one() {
        let env = policy_env();
        assert_eq!(env.len(), POLICY.len());
        assert!(env.contains(&"LOOM_FORCE_SCOPE=protected".to_owned()));
        // The demonstrated escalation is `guards.forceScope: "off"`; the forced
        // channel must never carry the disabling value for any category.
        assert!(!env.iter().any(|entry| entry.ends_with("=off")));
        let mut command = std::process::Command::new("true");
        apply_policy(&mut command);
        assert_eq!(
            command
                .get_envs()
                .filter(|(_, value)| value.is_some())
                .count(),
            POLICY.len()
        );
    }
}
