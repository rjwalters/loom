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
        let home = root.join("isolated-home");
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
