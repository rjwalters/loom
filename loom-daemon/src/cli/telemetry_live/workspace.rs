//! Isolated read-only curator smoke repository; no forge or global credentials.
use anyhow::{ensure, Context, Result};
use std::{
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

pub(super) struct Workspace {
    pub root: PathBuf,
    pub home: PathBuf,
    pub expected: String,
    pub input: Vec<u8>,
}

impl Workspace {
    pub fn create(parent: &Path, runtime: &str, endpoint: &str) -> Result<Self> {
        let root = parent.join(runtime);
        std::fs::create_dir(&root)?;
        // Harnesses may persist auth/session state. Keep it outside even this
        // disposable Git repository, under the operator's private home tree.
        let home = parent.join(format!("{runtime}-home"));
        std::fs::create_dir(&home)?;
        std::fs::create_dir_all(root.join(".loom/roles"))?;
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let input = format!("{nonce}\n").into_bytes();
        std::fs::write(root.join("canary-input.txt"), &input)?;
        std::fs::write(
            root.join(".loom/roles/curator.json"),
            include_str!("../../../../defaults/roles/curator.json"),
        )?;
        std::fs::write(root.join(".loom/roles/curator.md"),
            "You are performing an isolated read-only curator smoke test, not curating a real issue. Use the guarded loom_read tool to read canary-input.txt. Respond with exactly CANARY_RESULT: followed by the file's nonce, on one line. Do not write files, invoke other tools, or contact a forge.\n")?;
        let profiles: serde_json::Value =
            serde_json::from_str(include_str!("../../../../defaults/model-profiles.json"))?;
        let profile = &profiles["zai-flash"];
        ensure!(
            profile["model"] == "glm-5.3-flash"
                && profile["providers"]["pi"] == "zai"
                && profile["providers"]["opencode"] == "zai-coding-plan"
                && profile["credentialEnv"] == "ZAI_API_KEY",
            "bundled zai-flash profile changed; review canary pins before spending"
        );
        std::fs::write(
            root.join(".loom/config.json"),
            serde_json::to_vec(&serde_json::json!({
                "observability":{"enabled":true,"exporter":"otlp","endpoint":endpoint},
                "runtimes":{"default":runtime,"modelProfiles":{"zai-flash":profile}}
            }))?,
        )?;
        let mut git = Command::new("git");
        git.args(["init", "--quiet"]).current_dir(&root);
        let init = loom_daemon::proc_exec::run_bounded(git, Duration::from_secs(10))?;
        ensure!(init.succeeded(), "cannot initialize isolated canary repository");
        Ok(Self {
            root,
            home,
            expected: format!("CANARY_RESULT:{nonce}"),
            input,
        })
    }

    pub fn command(
        &self,
        binary: &Path,
        guards: &Path,
        runtime: &str,
        key: &str,
    ) -> Result<Command> {
        let path = std::env::var_os("PATH").context("PATH required for installed harnesses")?;
        let mut command = Command::new(binary);
        command
            .env_clear()
            .current_dir(&self.root)
            .env("PATH", path)
            .env("HOME", &self.home)
            .env("PWD", &self.root)
            .env("XDG_CONFIG_HOME", self.home.join("config"))
            .env("XDG_DATA_HOME", self.home.join("data"))
            .env("XDG_CACHE_HOME", self.home.join("cache"))
            .env("XDG_STATE_HOME", self.home.join("state"))
            .env("LOOM_WORKSPACE", &self.root)
            .env("LOOM_RUNTIME", runtime)
            .env("LOOM_NATIVE_GUARD_DIR", guards)
            .env("LOOM_NATIVE_TOOLS_DIR", self.home.join("native-tools"))
            .env("LOOM_DAEMON_BIN", binary)
            .env("LOOM_SHARED_API_KEYS_DIR", "")
            .env("ZAI_API_KEY", key)
            .env("OPENCODE_DISABLE_AUTOUPDATE", "1");
        Ok(command)
    }

    pub fn input_unchanged(&self) -> bool {
        std::fs::read(self.root.join("canary-input.txt")).is_ok_and(|bytes| bytes == self.input)
    }
}

pub(super) fn validate_guards(guards: &Path) -> Result<()> {
    for file in [
        "guard-codex-bridge.sh",
        "guard-destructive.sh",
        "guard-destructive-generic.sh",
        "guard-worktree-paths.sh",
        "guard-loom-workflow.sh",
    ] {
        ensure!(guards.join(file).is_file(), "guard directory lacks a required installed hook");
    }
    Ok(())
}

pub(super) fn private_output_parent(output: &Path, zshrc: &Path, key_file: &Path) -> Result<()> {
    let home = dirs::home_dir().context("operator home is unavailable")?;
    validate_private_paths(&home, output, zshrc, key_file)
}

fn validate_private_paths(home: &Path, output: &Path, zshrc: &Path, key_file: &Path) -> Result<()> {
    let home = home.canonicalize()?;
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    for path in [parent, zshrc, key_file] {
        // Inspect the caller's spelling before resolving symlinks. A repo-local
        // alias to a private external file is still a prohibited checkout path.
        outside_checkout(&std::path::absolute(path)?)?;
        let resolved = path.canonicalize()?;
        ensure!(
            resolved.starts_with(&home),
            "canary credentials and private state must remain under the operator home directory"
        );
        outside_checkout(&resolved)?;
    }
    Ok(())
}

fn outside_checkout(path: &Path) -> Result<()> {
    // Both regular .git directories and linked-worktree .git files count. Do
    // not follow or read their contents, and fail closed on unreadable ancestry.
    for ancestor in path.ancestors() {
        match std::fs::symlink_metadata(ancestor.join(".git")) {
            Ok(_) => anyhow::bail!(
                "canary credentials and private state must be outside every repository checkout"
            ),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod private_path_tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let home = tempfile::tempdir().unwrap();
        let zshrc = home.path().join("zshrc");
        let key = home.path().join("ingest.key");
        std::fs::write(&zshrc, "fixture only").unwrap();
        std::fs::write(&key, "fixture only").unwrap();
        let output = home.path().join("new-output");
        (home, output, zshrc, key)
    }

    #[test]
    fn accepts_private_paths_but_rejects_home_checkouts_and_gitfiles() {
        let (home, output, zshrc, key) = fixture();
        assert!(validate_private_paths(home.path(), &output, &zshrc, &key).is_ok());
        for gitfile in [false, true] {
            let repo = home.path().join(if gitfile { "linked" } else { "repo" });
            std::fs::create_dir(&repo).unwrap();
            if gitfile {
                std::fs::write(repo.join(".git"), "gitdir: fixture").unwrap();
            } else {
                std::fs::create_dir(repo.join(".git")).unwrap();
            }
            let credential = repo.join("credential");
            std::fs::write(&credential, "fixture only").unwrap();
            assert!(validate_private_paths(home.path(), &output, &credential, &key).is_err());
            assert!(validate_private_paths(home.path(), &output, &zshrc, &credential).is_err());
            assert!(
                validate_private_paths(home.path(), &repo.join("output"), &zshrc, &key).is_err()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_repository_aliases_in_both_directions_before_reading_credentials() {
        use std::os::unix::fs::symlink;
        let (home, output, zshrc, key) = fixture();
        let repo = home.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        std::fs::write(repo.join(".git"), "gitdir: fixture").unwrap();
        let alias = repo.join("external-key");
        symlink(&key, &alias).unwrap();
        assert!(validate_private_paths(home.path(), &output, &alias, &key).is_err());
        assert!(validate_private_paths(home.path(), &output, &zshrc, &alias).is_err());
        let output_alias = repo.join("external-home");
        symlink(home.path(), &output_alias).unwrap();
        assert!(
            validate_private_paths(home.path(), &output_alias.join("new"), &zshrc, &key).is_err()
        );
        let inside = repo.join("credential");
        std::fs::write(&inside, "fixture only").unwrap();
        let reverse_alias = home.path().join("key-alias");
        symlink(&inside, &reverse_alias).unwrap();
        assert!(validate_private_paths(home.path(), &output, &zshrc, &reverse_alias).is_err());
        let reverse_output = home.path().join("repo-alias");
        symlink(&repo, &reverse_output).unwrap();
        assert!(
            validate_private_paths(home.path(), &reverse_output.join("new"), &zshrc, &key).is_err()
        );
    }
}
