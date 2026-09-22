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
    pub directory: PathBuf,
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

pub(super) fn private_directory(path: &Path) -> Result<()> {
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

fn create(
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
    let base = canonical_destination(&base)?;
    outside_repositories(&root, &base)?;
    // Validate and read an explicitly requested external snapshot before any
    // provisioning. Existing auth sources are never moved, changed or removed.
    let auth = auth.map(|path| auth_snapshot(&root, path)).transpose()?;
    let workspace = base.join(hex::encode(Sha256::digest(root.as_os_str().as_encoded_bytes())));
    outside_repositories(&root, &canonical_destination(&workspace)?)?;
    private_directory(&workspace)?;
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
    Ok(State { directory, auth })
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
    pub(super) fn configure(&self, command: &mut Command, runtime: &str) -> Result<()> {
        if let Some(auth) = &self.auth {
            validate_auth_format(auth, runtime)?;
        }
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
