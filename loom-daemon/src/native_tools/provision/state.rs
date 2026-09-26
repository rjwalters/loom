//! Private per-workspace, per-launch state for guarded native harnesses.
use anyhow::{bail, ensure, Context, Result};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
    process::Command,
};

pub(super) struct State {
    /// This launch's own private directory. Nothing else may read or write it.
    pub directory: PathBuf,
    /// The per-workspace parent of [`Self::directory`], which also holds the
    /// content-keyed binding trees every launch of this workspace shares
    /// (#8663, [`super::shared`]).
    pub workspace: PathBuf,
    pub auth: Option<Vec<u8>>,
}

pub(super) fn prepare(root: &Path) -> Result<State> {
    let base = std::env::var_os("LOOM_NATIVE_TOOLS_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let auth = std::env::var_os("LOOM_NATIVE_AUTH_FILE")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let root = root
        .canonicalize()
        .context("cannot resolve native workspace")?;
    for key in [
        "HOME",
        "PI_CODING_AGENT_DIR",
        "PI_CODING_AGENT_SESSION_DIR",
        "OPENCODE_CONFIG_DIR",
        "KIMI_CODE_HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
    ] {
        if let Some(value) = std::env::var_os(key).filter(|value| !value.is_empty()) {
            validate_override(&root, Path::new(&value))
                .with_context(|| format!("unsafe native harness directory override: {key}"))?;
        }
    }
    create(&root, base.as_deref(), dirs::home_dir().as_deref(), auth.as_deref())
}

fn validate_override(root: &Path, path: &Path) -> Result<()> {
    outside_repositories(root, path)?;
    outside_repositories(root, &canonical_destination(path)?)
}

fn canonical_destination(path: &Path) -> Result<PathBuf> {
    ensure!(path.is_absolute(), "native state paths must be absolute");
    ensure!(
        !path.components().any(|c| matches!(c, Component::ParentDir)),
        "native state paths must not contain parent traversal"
    );
    let mut ancestor = path;
    let mut missing = Vec::new();
    loop {
        match fs::symlink_metadata(ancestor) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(
                    ancestor
                        .file_name()
                        .context("native state path has no existing ancestor")?
                        .to_owned(),
                );
                ancestor = ancestor
                    .parent()
                    .context("native state path has no existing ancestor")?;
            }
            Err(_) => bail!("cannot inspect native state path"),
        }
    }
    let mut resolved = ancestor
        .canonicalize()
        .context("cannot resolve native state path")?;
    for component in missing.into_iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn outside_repositories(root: &Path, path: &Path) -> Result<()> {
    ensure!(
        !path.starts_with(root),
        "native state and auth files must stay outside the workspace"
    );
    outside_every_repository(path)
}

/// Whether no ancestor of `path` is a repository checkout (ordinary clone,
/// linked worktree, or bare repository).
///
/// `pub` since #8581 so the readiness measurement's shared package cache is
/// held to the same "never inside a checkout" rule as per-launch native state,
/// by the same code rather than a second copy of the ancestry scan.
///
/// # Errors
///
/// Propagates an ancestry inspection failure — fails closed on an unreadable
/// ancestor rather than assuming it is not a repository.
pub fn outside_every_repository(path: &Path) -> Result<()> {
    for ancestor in path.ancestors().filter(|ancestor| !ancestor.is_file()) {
        // Git linked worktrees have a .git file; ordinary clones have a directory.
        let checkout = ancestor
            .join(".git")
            .try_exists()
            .context("cannot inspect native state ancestry")?;
        let bare = ancestor.join("HEAD").is_file()
            && ancestor.join("objects").is_dir()
            && ancestor.join("config").is_file();
        ensure!(
            !checkout && !bare,
            "native state and auth files must stay outside every repository"
        );
    }
    Ok(())
}

/// Create `path` (and any missing parent) as a 0700 directory owned by the
/// current user, then validate that it really is one.
///
/// `pub` since #8581 so the readiness measurement can create its isolated
/// per-attempt state with the same check the production launch path applies,
/// rather than a second, weaker copy of it.
///
/// # Errors
///
/// Propagates a creation failure, or a validation failure when the path is a
/// symlink, is not owned by the current user, or is not mode 0700.
pub fn private_directory(path: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .context("cannot create private native state directory")?;
    validate_private_directory(path)
}

fn validate_private_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "native state directory cannot be a symlink"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // SAFETY: geteuid only reads the caller's effective user id.
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() },
            "native state directory must belong to the current user"
        );
        ensure!(
            metadata.permissions().mode() & 0o777 == 0o700,
            "native state directory must have permissions 0700"
        );
    }
    Ok(())
}

fn auth_snapshot(root: &Path, path: &Path) -> Result<Vec<u8>> {
    outside_repositories(root, path)?;
    let path = canonical_destination(path)?;
    outside_repositories(root, &path)?;
    validate_private_directory(
        path.parent()
            .context("native auth file requires a private parent directory")?,
    )?;
    let file = fs::File::open(path).context("cannot open external native auth file")?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "native auth source must be a regular file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // SAFETY: geteuid has no side effects.
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() }
                && metadata.permissions().mode() & 0o777 == 0o600,
            "native auth file must belong to the current user and have permissions 0600"
        );
    }
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 1024 * 1024, "native auth file exceeds the size limit");
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("native auth file must contain a JSON object"))?;
    ensure!(value.is_object(), "native auth file must contain a JSON object");
    Ok(bytes)
}

/// `pub(super)` so the shared-binding tests can build two launches' state for
/// one workspace without going through the environment [`prepare`] reads.
pub(super) fn create(
    root: &Path,
    override_base: Option<&Path>,
    home: Option<&Path>,
    auth: Option<&Path>,
) -> Result<State> {
    let root = root
        .canonicalize()
        .context("cannot resolve native workspace")?;
    let base = match override_base {
        Some(base) => base.to_owned(),
        None => home
            .context("native state requires a home directory or LOOM_NATIVE_TOOLS_DIR")?
            .join(".local/state/loom/native-tools"),
    };
    outside_repositories(&root, &base)?;
    let base = canonical_destination(&base)?;
    outside_repositories(&root, &base)?;
    // Validate and read an explicitly requested external snapshot before any
    // provisioning. Existing auth sources are never moved, changed or removed.
    let auth = auth.map(|path| auth_snapshot(&root, path)).transpose()?;
    let workspace = base.join(hex::encode(Sha256::digest(root.as_os_str().as_encoded_bytes())));
    outside_repositories(&root, &canonical_destination(&workspace)?)?;
    private_directory(&workspace)?;
    let workspace = workspace.canonicalize()?;
    // The only Loom process that reliably exists around a native session is the
    // one starting the *next* one: `worker_spawn::exec` hands this process
    // image to the harness CLI, so there is no parent left at session exit to
    // clean up after it (#8663). Never fatal — a launch that cannot reap still
    // launches; `loom-daemon clean` is the other pass.
    let _ = super::reap::reap_workspace(&workspace, &super::reap::Policy::default(), false);
    let directory = workspace.join(uuid::Uuid::new_v4().to_string());
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(&directory)
        .context("cannot allocate isolated native launch state")?;
    let directory = directory.canonicalize()?;
    outside_repositories(&root, &directory)?;
    // Records the pid `execve` hands to the harness, so the next launch can
    // tell a live 12-hour sweep from an exited one instead of guessing on age.
    super::reap::record_session(&directory)?;
    Ok(State {
        directory,
        workspace,
        auth,
    })
}

fn validate_auth_format(bytes: &[u8], runtime: &str) -> Result<()> {
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|_| anyhow::anyhow!("native auth snapshot has invalid JSON"))?;
    let providers = value
        .as_object()
        .context("native auth snapshot must be an object")?;
    for credential in providers.values() {
        let text = |key: &str| credential.get(key).is_some_and(|value| value.is_string());
        let valid = match credential.get("type").and_then(|kind| kind.as_str()) {
            Some("api_key") if runtime == "pi" => {
                text("key")
                    || credential
                        .get("env")
                        .and_then(|env| env.as_object())
                        .is_some_and(|env| env.values().all(|value| value.is_string()))
            }
            Some("api") if runtime == "opencode" => text("key"),
            Some("wellknown") if runtime == "opencode" => text("key") && text("token"),
            Some("oauth") => {
                text("refresh")
                    && text("access")
                    && credential
                        .get("expires")
                        .and_then(|value| value.as_u64())
                        .is_some()
            }
            _ => false,
        };
        ensure!(
            valid,
            "native auth snapshot does not match the selected harness credential format"
        );
    }
    Ok(())
}

impl State {
    /// Pin `TMPDIR` to a private directory inside this launch's own state
    /// (#8650).
    ///
    /// Without this, a `bun --compile` harness (OpenCode) falls back to the OS
    /// default (`$TMPDIR`, else `/tmp`) and extracts its ~5.5 MB embedded
    /// native addon there on **every** launch, under a fresh
    /// `.<hash>-0000000N.{so,node}` name that nothing ever removes — measured
    /// at 7.6 GB across 1,382 files in 40 hours of scheduled role ticks on one
    /// worker, which contributed to a live ENOSPC outage. Relocating the
    /// writes into `<launch state>/tmp` makes them reclaimable by
    /// [`crate::native_state_reclaim`], which removes the whole per-launch
    /// directory once it is old enough; a bare `/tmp` extract is
    /// unattributable and would have to be matched by filename pattern
    /// instead (see that module's docs for why this direction was chosen).
    fn pin_tmpdir(&self, command: &mut Command) -> Result<()> {
        let path = self.directory.join("tmp");
        private_directory(&path)?;
        command.env("TMPDIR", path);
        Ok(())
    }

    pub(super) fn configure(&self, command: &mut Command, runtime: &str) -> Result<()> {
        if runtime == "kimi" {
            // `KIMI_CODE_HOME` is relocated by `provision::write_kimi_bindings`
            // and holds config, MCP registry, sessions and credentials in one
            // directory, so there is nothing further to pin here. An external
            // auth snapshot is refused rather than guessed at: Kimi's
            // credential file format has no verified shape in
            // `validate_auth_format`, and writing an unvalidated blob into a
            // relocated home could silently authenticate the wrong account.
            ensure!(
                self.auth.is_none(),
                "LOOM_NATIVE_AUTH_FILE is not supported for the kimi harness; use the model \
                 profile's credentialEnv mapping (KIMI_MODEL_API_KEY) instead"
            );
            // `TMPDIR` is still pinned: every guarded harness is a self-extracting
            // single-file binary of some shape, and none of them may scatter
            // per-launch extracts into the shared OS `/tmp`.
            return self.pin_tmpdir(command);
        }
        if let Some(auth) = &self.auth {
            validate_auth_format(auth, runtime)?;
        }
        self.pin_tmpdir(command)?;
        // Guarded launches own these paths; ambient harness config cannot move
        // auth/session writes back into a checkout or another worker's state.
        if runtime == "pi" {
            for (key, leaf) in [
                ("PI_CODING_AGENT_DIR", "pi-agent"),
                ("PI_CODING_AGENT_SESSION_DIR", "pi-sessions"),
            ] {
                let path = self.directory.join(leaf);
                private_directory(&path)?;
                command.env(key, path);
            }
        } else {
            for (key, leaf) in [
                ("XDG_CONFIG_HOME", "config"),
                ("XDG_DATA_HOME", "data"),
                ("XDG_STATE_HOME", "state"),
                ("XDG_CACHE_HOME", "cache"),
            ] {
                let path = self.directory.join(leaf);
                private_directory(&path)?;
                command.env(key, path);
            }
            private_directory(&self.directory.join("data/opencode"))?;
        }
        if let Some(auth) = &self.auth {
            let path = if runtime == "pi" {
                self.directory.join("pi-agent/auth.json")
            } else {
                self.directory.join("data/opencode/auth.json")
            };
            let mut file = tempfile::NamedTempFile::new_in(
                path.parent().context("auth destination has no parent")?,
            )?;
            std::io::Write::write_all(&mut file, auth)?;
            file.persist(path).map_err(|error| error.error)?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;
