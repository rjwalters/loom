//! Secret-safe lifecycle management for machine-level Codex profiles.
//!
//! Codex owns `auth.json`; Loom treats it as opaque mutable state and never
//! reads, parses, serializes, or logs its contents.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;

use super::account_registry::{
    account_inventory, register_codex_account, set_codex_account_enabled, unregister_codex_account,
    validate_name, AccountDescriptor, AccountProvider, CredentialKind, InventoryProvenance,
};
use super::paths::codex_profile_root;

const AUTH_FILE: &str = "auth.json";
const STATUS_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_STATUS_BYTES: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LoginState {
    LoggedIn,
    NotLoggedIn,
    CliMissing,
    TimedOut,
    Failed,
    NotChecked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProfileDiagnostics {
    pub profile_exists: bool,
    pub auth_shape: &'static str,
    pub directory_mode_valid: bool,
    pub auth_mode_valid: bool,
    pub owner_valid: bool,
}

impl ProfileDiagnostics {
    #[must_use]
    pub fn valid(&self) -> bool {
        self.profile_exists
            && self.auth_shape == "valid"
            && self.directory_mode_valid
            && self.auth_mode_valid
            && self.owner_valid
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AccountStatus {
    pub schema_version: u32,
    pub provider: AccountProvider,
    pub name: String,
    pub enabled: bool,
    pub credential_kind: CredentialKind,
    pub provenance: InventoryProvenance,
    pub diagnostics: ProfileDiagnostics,
    pub login_state: LoginState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemovalOutcome {
    pub provider: AccountProvider,
    pub name: String,
    pub purged: bool,
    pub recovery_reference: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerOutput {
    pub success: bool,
    pub unavailable: bool,
    pub timed_out: bool,
    pub exit_code: Option<i32>,
    pub summary: String,
}

#[derive(Debug)]
pub struct LoginCommandFailed {
    pub exit_code: Option<i32>,
    summary: String,
}

impl std::fmt::Display for LoginCommandFailed {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.summary)
    }
}

impl std::error::Error for LoginCommandFailed {}

#[must_use]
pub fn login_exit_code(error: &anyhow::Error) -> Option<i32> {
    error
        .downcast_ref::<LoginCommandFailed>()
        .and_then(|failure| failure.exit_code)
}

fn login_failure(output: RunnerOutput) -> anyhow::Error {
    LoginCommandFailed {
        exit_code: output.exit_code,
        summary: output.summary,
    }
    .into()
}

pub trait CodexCommandRunner {
    fn login(&self, profile: &Path, device_auth: bool) -> Result<RunnerOutput>;
    fn login_status(&self, profile: &Path) -> Result<RunnerOutput>;
}

pub struct ProcessCodexRunner;

impl ProcessCodexRunner {
    fn bounded_status(profile: &Path) -> Result<RunnerOutput> {
        let mut child = match Command::new("codex")
            .args(["login", "status"])
            .env("CODEX_HOME", profile)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(RunnerOutput {
                    success: false,
                    unavailable: true,
                    timed_out: false,
                    exit_code: None,
                    summary: "Codex CLI is not installed or not on PATH".into(),
                });
            }
            Err(error) => return Err(error.into()),
        };
        let started = Instant::now();
        loop {
            if let Some(status) = child.try_wait()? {
                let mut bytes = Vec::new();
                if let Some(stdout) = child.stdout.take() {
                    stdout
                        .take(MAX_STATUS_BYTES as u64)
                        .read_to_end(&mut bytes)?;
                }
                if bytes.is_empty() {
                    if let Some(stderr) = child.stderr.take() {
                        stderr
                            .take(MAX_STATUS_BYTES as u64)
                            .read_to_end(&mut bytes)?;
                    }
                }
                let text = String::from_utf8_lossy(&bytes).to_ascii_lowercase();
                let summary = if status.success() && text.contains("logged in") {
                    "logged in"
                } else if text.contains("not logged in") {
                    "not logged in"
                } else {
                    "Codex login status failed"
                };
                return Ok(RunnerOutput {
                    success: status.success(),
                    unavailable: false,
                    timed_out: false,
                    exit_code: status.code(),
                    summary: summary.into(),
                });
            }
            if started.elapsed() >= STATUS_TIMEOUT {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(RunnerOutput {
                    success: false,
                    unavailable: false,
                    timed_out: true,
                    exit_code: None,
                    summary: "Codex login status timed out".into(),
                });
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl CodexCommandRunner for ProcessCodexRunner {
    fn login(&self, profile: &Path, device_auth: bool) -> Result<RunnerOutput> {
        let mut command = Command::new("codex");
        command.arg("login").env("CODEX_HOME", profile);
        if device_auth {
            command.arg("--device-auth");
        }
        let status = match command.status() {
            Ok(status) => status,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(RunnerOutput {
                    success: false,
                    unavailable: true,
                    timed_out: false,
                    exit_code: None,
                    summary: "Codex CLI is not installed or not on PATH".into(),
                });
            }
            Err(error) => return Err(error.into()),
        };
        Ok(RunnerOutput {
            success: status.success(),
            unavailable: false,
            timed_out: false,
            exit_code: status.code(),
            summary: if status.success() {
                "login completed"
            } else {
                "Codex login failed or was cancelled"
            }
            .into(),
        })
    }

    fn login_status(&self, profile: &Path) -> Result<RunnerOutput> {
        Self::bounded_status(profile)
    }
}

pub struct AccountLifecycle<R> {
    workspace: PathBuf,
    root: PathBuf,
    runner: R,
}

impl<R: CodexCommandRunner> AccountLifecycle<R> {
    pub fn new(workspace: impl Into<PathBuf>, runner: R) -> Result<Self> {
        let workspace = workspace.into();
        let root = codex_profile_root()
            .ok_or_else(|| anyhow!("Codex profiles are disabled by LOOM_CODEX_PROFILE_ROOT"))?;
        reject_repository_local_root(&workspace, &root)?;
        ensure_private_dir(&root)?;
        Ok(Self {
            workspace,
            root,
            runner,
        })
    }

    pub fn add(&self, name: &str, device_auth: bool) -> Result<AccountStatus> {
        validate_name(name)?;
        self.ensure_absent(name)?;
        let profile = self.profile(name)?;
        ensure_private_dir(&profile)?;
        let result = self.runner.login(&profile, device_auth)?;
        if !result.success {
            if fs::read_dir(&profile)?.next().is_none() {
                let _ = fs::remove_dir(&profile);
            }
            return Err(login_failure(result));
        }
        tighten_profile_permissions(&profile)?;
        let diagnostics = inspect_profile(&profile);
        if !diagnostics.valid() {
            bail!("Codex login completed but the profile credential is missing or unsafe");
        }
        register_codex_account(&self.workspace, name, name, true)?;
        self.status(name)
    }

    pub fn import(&self, name: &str, source: &Path) -> Result<AccountStatus> {
        validate_name(name)?;
        self.ensure_absent(name)?;
        let source_meta = fs::symlink_metadata(source)
            .context("authentication source is missing or unreadable")?;
        if !source_meta.file_type().is_file() || source_meta.len() == 0 {
            bail!("authentication source must be a non-empty regular file");
        }
        let source = source
            .canonicalize()
            .context("authentication source cannot be resolved")?;
        let profile = self.profile(name)?;
        let destination = profile.join(AUTH_FILE);
        if source == destination {
            bail!("authentication source and destination must differ");
        }
        ensure_private_dir(&profile)?;
        let staged = profile.join(format!(".auth.json.tmp-{}", std::process::id()));
        let result = (|| -> Result<()> {
            let mut input = OpenOptions::new()
                .read(true)
                .open(&source)
                .context("authentication source is unreadable")?;
            let mut output = private_file(&staged)?;
            std::io::copy(&mut input, &mut output).context("authentication import failed")?;
            output
                .sync_all()
                .context("authentication import sync failed")?;
            fs::rename(&staged, &destination).context("authentication import commit failed")?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&staged);
            let _ = fs::remove_dir(&profile);
        }
        result?;
        register_codex_account(&self.workspace, name, name, true)?;
        self.status_without_probe(name)
    }

    pub fn list(&self, probe: bool) -> Result<Vec<AccountStatus>> {
        account_inventory(&self.workspace, AccountProvider::Codex)?
            .into_iter()
            .map(|account| self.status_for(account, probe))
            .collect()
    }

    pub fn status(&self, name: &str) -> Result<AccountStatus> {
        let account = self.find(name)?;
        self.status_for(account, true)
    }

    pub fn status_without_probe(&self, name: &str) -> Result<AccountStatus> {
        let account = self.find(name)?;
        self.status_for(account, false)
    }

    pub fn disable(&self, name: &str) -> Result<AccountStatus> {
        self.find(name)?;
        set_codex_account_enabled(&self.workspace, name, false)?;
        self.status_without_probe(name)
    }

    pub fn enable(&self, name: &str) -> Result<AccountStatus> {
        let account = self.find(name)?;
        let diagnostics = inspect_profile(&account.credential_reference);
        if !diagnostics.valid() {
            bail!("profile must pass credential shape and permission validation before enabling");
        }
        set_codex_account_enabled(&self.workspace, name, true)?;
        self.status_without_probe(name)
    }

    pub fn reauth(&self, name: &str, device_auth: bool) -> Result<AccountStatus> {
        let account = self.find(name)?;
        let enabled = account.enabled;
        let result = self
            .runner
            .login(&account.credential_reference, device_auth)?;
        if !result.success {
            return Err(login_failure(result));
        }
        tighten_profile_permissions(&account.credential_reference)?;
        let status = self.status(name)?;
        if status.enabled != enabled {
            bail!("account enabled state changed unexpectedly during reauthentication");
        }
        Ok(status)
    }

    pub fn remove(&self, name: &str, purge: bool) -> Result<RemovalOutcome> {
        let account = self.find(name)?;
        let quarantine = self.root.join(".quarantine");
        ensure_private_dir(&quarantine)?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?;
        let stamp = now.as_secs();
        let recovery_reference = format!("{name}-{stamp}-{}", now.subsec_nanos());
        validate_name(&recovery_reference)?;
        let destination = quarantine.join(&recovery_reference);
        fs::rename(&account.credential_reference, &destination)
            .context("failed to quarantine profile atomically")?;
        if let Err(error) = unregister_codex_account(&self.workspace, name) {
            fs::rename(&destination, &account.credential_reference)
                .context("account registry update failed and profile rollback also failed")?;
            return Err(error.context("account registry update failed; profile was restored"));
        }
        if purge {
            // Only destroy bytes after the registry commit. A deletion error
            // may leave private quarantine residue, but never a stale live
            // registry entry pointing at a partially deleted profile.
            fs::remove_dir_all(&destination)
                .context("failed to purge quarantined Codex profile")?;
            return Ok(RemovalOutcome {
                provider: AccountProvider::Codex,
                name: name.into(),
                purged: true,
                recovery_reference: None,
            });
        }
        let metadata = serde_json::json!({
            "version": 1,
            "provider": "codex",
            "name": name,
            "retired_at_unix": stamp,
            "original_reference": name,
        });
        let mut file = private_file(&destination.join("recovery.json"))?;
        serde_json::to_writer_pretty(&mut file, &metadata)?;
        file.write_all(b"\n")?;
        Ok(RemovalOutcome {
            provider: AccountProvider::Codex,
            name: name.into(),
            purged: false,
            recovery_reference: Some(recovery_reference),
        })
    }

    fn status_for(&self, account: AccountDescriptor, probe: bool) -> Result<AccountStatus> {
        let diagnostics = inspect_profile(&account.credential_reference);
        let login_state = if !probe || !diagnostics.valid() {
            LoginState::NotChecked
        } else {
            let output = self.runner.login_status(&account.credential_reference)?;
            if output.unavailable {
                LoginState::CliMissing
            } else if output.timed_out {
                LoginState::TimedOut
            } else if output.success && output.summary == "logged in" {
                LoginState::LoggedIn
            } else if output.summary == "not logged in" {
                LoginState::NotLoggedIn
            } else {
                LoginState::Failed
            }
        };
        Ok(AccountStatus {
            schema_version: 1,
            provider: account.id.provider,
            name: account.id.name,
            enabled: account.enabled,
            credential_kind: account.credential_kind,
            provenance: account.provenance,
            diagnostics,
            login_state,
        })
    }

    fn find(&self, name: &str) -> Result<AccountDescriptor> {
        validate_name(name)?;
        account_inventory(&self.workspace, AccountProvider::Codex)?
            .into_iter()
            .find(|account| account.id.name == name)
            .ok_or_else(|| anyhow!("Codex account {name:?} does not exist"))
    }

    fn ensure_absent(&self, name: &str) -> Result<()> {
        if account_inventory(&self.workspace, AccountProvider::Codex)?
            .iter()
            .any(|account| account.id.name == name)
            || self.profile(name)?.exists()
        {
            bail!("Codex account {name:?} already exists");
        }
        Ok(())
    }

    fn profile(&self, name: &str) -> Result<PathBuf> {
        validate_name(name)?;
        Ok(self.root.join(name))
    }
}

fn reject_repository_local_root(workspace: &Path, root: &Path) -> Result<()> {
    let workspace = workspace
        .canonicalize()
        .context("workspace cannot be resolved")?;
    let absolute_root = if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir()?.join(root)
    };
    let canonical_intent = absolute_root
        .ancestors()
        .find(|candidate| candidate.exists())
        .and_then(|existing| existing.canonicalize().ok().map(|base| (existing, base)))
        .map(|(existing, base)| {
            absolute_root
                .strip_prefix(existing)
                .map_or(base.clone(), |suffix| base.join(suffix))
        })
        .unwrap_or(absolute_root);
    if canonical_intent.starts_with(&workspace) {
        bail!("Codex profile root must not be repository-local");
    }
    Ok(())
}

fn ensure_private_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    fs::create_dir_all(path)?;
    Ok(())
}

fn private_file(path: &Path) -> Result<fs::File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}

fn tighten_profile_permissions(profile: &Path) -> Result<()> {
    ensure_private_dir(profile)?;
    let auth = profile.join(AUTH_FILE);
    if !auth.is_file() {
        bail!("Codex login did not create a regular auth.json");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&auth, fs::Permissions::from_mode(0o600))
            .context("failed to secure Codex auth file permissions")?;
    }
    Ok(())
}

fn inspect_profile(profile: &Path) -> ProfileDiagnostics {
    #[cfg(unix)]
    let invoking_uid = unsafe { libc::geteuid() };
    #[cfg(not(unix))]
    let invoking_uid = 0;
    inspect_profile_for_uid(profile, invoking_uid)
}

fn inspect_profile_for_uid(profile: &Path, invoking_uid: u32) -> ProfileDiagnostics {
    let profile_meta = fs::symlink_metadata(profile).ok();
    let auth_meta = fs::symlink_metadata(profile.join(AUTH_FILE)).ok();
    let auth_shape = match &auth_meta {
        None => "missing",
        Some(meta) if !meta.file_type().is_file() => "non_regular",
        Some(meta) if meta.len() == 0 => "empty",
        Some(_)
            if OpenOptions::new()
                .read(true)
                .open(profile.join(AUTH_FILE))
                .is_err() =>
        {
            "unreadable"
        }
        Some(_) => "valid",
    };
    #[cfg(unix)]
    let (directory_mode_valid, auth_mode_valid, owner_valid) = {
        use std::os::unix::fs::MetadataExt;
        let directory_mode_valid = profile_meta
            .as_ref()
            .is_some_and(|meta| meta.is_dir() && meta.mode() & 0o077 == 0);
        let auth_mode_valid = auth_meta
            .as_ref()
            .is_some_and(|meta| meta.mode() & 0o077 == 0);
        let owner_valid = match (&profile_meta, &auth_meta) {
            (Some(directory), Some(auth)) => {
                directory.uid() == invoking_uid && auth.uid() == invoking_uid
            }
            _ => false,
        };
        (directory_mode_valid, auth_mode_valid, owner_valid)
    };
    #[cfg(not(unix))]
    let (directory_mode_valid, auth_mode_valid, owner_valid) = (true, true, true);
    ProfileDiagnostics {
        profile_exists: profile_meta.is_some_and(|meta| meta.is_dir()),
        auth_shape,
        directory_mode_valid,
        auth_mode_valid,
        owner_valid,
    }
}

#[cfg(test)]
mod tests {
    use super::super::paths::per_repo_accounts_file;
    use super::*;
    use serial_test::serial;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeRunner {
        calls: Mutex<Vec<(PathBuf, Vec<String>)>>,
        fail_login: bool,
    }

    struct StatusRunner(RunnerOutput);

    impl CodexCommandRunner for StatusRunner {
        fn login(&self, _profile: &Path, _device_auth: bool) -> Result<RunnerOutput> {
            unreachable!("status-only test runner")
        }

        fn login_status(&self, _profile: &Path) -> Result<RunnerOutput> {
            Ok(self.0.clone())
        }
    }

    impl CodexCommandRunner for FakeRunner {
        fn login(&self, profile: &Path, device_auth: bool) -> Result<RunnerOutput> {
            let mut args = vec!["login".to_string()];
            if device_auth {
                args.push("--device-auth".into());
            }
            self.calls.lock().unwrap().push((profile.into(), args));
            if !self.fail_login {
                let mut options = OpenOptions::new();
                options.write(true).create(true).truncate(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                let mut file = options.open(profile.join(AUTH_FILE))?;
                file.write_all(b"recognizable-fake-secret")?;
            }
            Ok(RunnerOutput {
                success: !self.fail_login,
                unavailable: false,
                timed_out: false,
                exit_code: self.fail_login.then_some(23),
                summary: if self.fail_login {
                    "cancelled"
                } else {
                    "login completed"
                }
                .into(),
            })
        }

        fn login_status(&self, profile: &Path) -> Result<RunnerOutput> {
            self.calls
                .lock()
                .unwrap()
                .push((profile.into(), vec!["login".into(), "status".into()]));
            Ok(RunnerOutput {
                success: true,
                unavailable: false,
                timed_out: false,
                exit_code: Some(0),
                summary: "logged in".into(),
            })
        }
    }

    fn setup() -> (tempfile::TempDir, tempfile::TempDir) {
        (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap())
    }

    #[test]
    #[serial]
    fn add_two_profiles_device_auth_and_reauth_keep_canonical_identity() {
        let (workspace, root) = setup();
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", root.path());
        let service = AccountLifecycle::new(workspace.path(), FakeRunner::default()).unwrap();
        service.add("alice", true).unwrap();
        service.add("bob", false).unwrap();
        service.disable("alice").unwrap();
        let reauthed = service.reauth("alice", true).unwrap();
        assert!(!reauthed.enabled);
        assert_eq!(service.list(false).unwrap().len(), 2);
        let calls = service.runner.calls.lock().unwrap();
        assert_eq!(calls[0].0.file_name().unwrap(), "alice");
        assert_eq!(calls[0].1, ["login", "--device-auth"]);
        assert_eq!(calls[2].0.file_name().unwrap(), "bob");
        assert_eq!(calls[4].0.file_name().unwrap(), "alice");
        assert!(!format!("{:?}", &*calls).contains("recognizable-fake-secret"));
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn import_is_private_atomic_and_secret_free() {
        let (workspace, root) = setup();
        let source_dir = tempfile::tempdir().unwrap();
        let source = source_dir.path().join("auth.json");
        fs::write(&source, "recognizable-fake-secret").unwrap();
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", root.path());
        let service = AccountLifecycle::new(workspace.path(), FakeRunner::default()).unwrap();
        let status = service.import("alice", &source).unwrap();
        assert!(status.diagnostics.valid());
        let encoded = serde_json::to_string(&status).unwrap();
        assert!(!encoded.contains("recognizable-fake-secret"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(root.path().join("alice"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(root.path().join("alice/auth.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert!(service
            .import("alice", &source)
            .unwrap_err()
            .to_string()
            .contains("exists"));
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn failed_add_leaves_no_account_or_empty_profile() {
        let (workspace, root) = setup();
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", root.path());
        let service = AccountLifecycle::new(
            workspace.path(),
            FakeRunner {
                fail_login: true,
                ..FakeRunner::default()
            },
        )
        .unwrap();
        let error = service.add("alice", false).unwrap_err();
        assert_eq!(login_exit_code(&error), Some(23));
        assert!(!root.path().join("alice").exists());
        assert!(service.list(false).unwrap().is_empty());
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn enable_requires_safe_shape_and_disable_is_non_destructive() {
        let (workspace, root) = setup();
        let source_dir = tempfile::tempdir().unwrap();
        let source = source_dir.path().join("auth.json");
        fs::write(&source, "recognizable-fake-secret").unwrap();
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", root.path());
        let service = AccountLifecycle::new(workspace.path(), FakeRunner::default()).unwrap();
        service.import("alice", &source).unwrap();
        service.disable("alice").unwrap();
        assert_eq!(
            fs::read_to_string(root.path().join("alice/auth.json")).unwrap(),
            "recognizable-fake-secret"
        );
        fs::remove_file(root.path().join("alice/auth.json")).unwrap();
        assert!(service.enable("alice").is_err());
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn remove_quarantines_without_secret_metadata_and_purge_is_explicit() {
        let (workspace, root) = setup();
        let source_dir = tempfile::tempdir().unwrap();
        let source = source_dir.path().join("auth.json");
        fs::write(&source, "recognizable-fake-secret").unwrap();
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", root.path());
        let service = AccountLifecycle::new(workspace.path(), FakeRunner::default()).unwrap();
        service.import("alice", &source).unwrap();
        let removed = service.remove("alice", false).unwrap();
        let recovery = root
            .path()
            .join(".quarantine")
            .join(removed.recovery_reference.unwrap());
        let metadata = fs::read_to_string(recovery.join("recovery.json")).unwrap();
        assert!(!metadata.contains("recognizable-fake-secret"));
        assert!(recovery.join("auth.json").exists());
        service.import("alice", &source).unwrap();
        let second = service.remove("alice", false).unwrap();
        assert_ne!(recovery.file_name().unwrap(), second.recovery_reference.as_deref().unwrap());
        service.import("bob", &source).unwrap();
        assert!(service.remove("bob", true).unwrap().purged);
        assert!(!root.path().join("bob").exists());
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn rejects_names_and_repository_local_root() {
        let (workspace, _) = setup();
        let local_root = workspace.path().join(".loom/codex-profiles");
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", &local_root);
        assert!(AccountLifecycle::new(workspace.path(), FakeRunner::default()).is_err());
        let external = tempfile::tempdir().unwrap();
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", external.path());
        let service = AccountLifecycle::new(workspace.path(), FakeRunner::default()).unwrap();
        for name in ["../escape", "/absolute", "a/b", r"a\b"] {
            assert!(service.add(name, false).is_err());
        }
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn diagnostics_cover_unsafe_shapes_modes_and_invoking_owner() {
        let (_workspace, root) = setup();
        let profile = root.path().join("alice");
        fs::create_dir(&profile).unwrap();

        let missing = inspect_profile(&profile);
        assert_eq!(missing.auth_shape, "missing");

        let auth = profile.join(AUTH_FILE);
        fs::write(&auth, "").unwrap();
        assert_eq!(inspect_profile(&profile).auth_shape, "empty");
        fs::remove_file(&auth).unwrap();
        fs::create_dir(&auth).unwrap();
        assert_eq!(inspect_profile(&profile).auth_shape, "non_regular");
        fs::remove_dir(&auth).unwrap();
        fs::write(&auth, "recognizable-fake-secret").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            fs::set_permissions(&profile, fs::Permissions::from_mode(0o755)).unwrap();
            fs::set_permissions(&auth, fs::Permissions::from_mode(0o644)).unwrap();
            let unsafe_modes = inspect_profile(&profile);
            assert!(!unsafe_modes.directory_mode_valid);
            assert!(!unsafe_modes.auth_mode_valid);

            fs::set_permissions(&profile, fs::Permissions::from_mode(0o700)).unwrap();
            fs::set_permissions(&auth, fs::Permissions::from_mode(0o600)).unwrap();
            let actual_uid = fs::metadata(&profile).unwrap().uid();
            assert!(!inspect_profile_for_uid(&profile, actual_uid.saturating_add(1)).owner_valid);
        }
    }

    #[test]
    #[serial]
    fn import_rejects_symlink_and_never_echoes_secret_in_errors() {
        let (workspace, root) = setup();
        let source_dir = tempfile::tempdir().unwrap();
        let source = source_dir.path().join("auth.json");
        fs::write(&source, "recognizable-fake-secret").unwrap();
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", root.path());
        let service = AccountLifecycle::new(workspace.path(), FakeRunner::default()).unwrap();
        #[cfg(unix)]
        {
            let link = source_dir.path().join("auth-link.json");
            std::os::unix::fs::symlink(&source, &link).unwrap();
            let error = service.import("alice", &link).unwrap_err().to_string();
            assert!(error.contains("regular file"));
            assert!(!error.contains("recognizable-fake-secret"));
        }
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn registry_failure_rolls_quarantine_and_purge_back_to_live_profile() {
        let (workspace, root) = setup();
        let source_dir = tempfile::tempdir().unwrap();
        let source = source_dir.path().join("auth.json");
        fs::write(&source, "recognizable-fake-secret").unwrap();
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", root.path());
        let service = AccountLifecycle::new(workspace.path(), FakeRunner::default()).unwrap();

        for (name, purge) in [("retire", false), ("purge", true)] {
            service.import(name, &source).unwrap();
            let lock = per_repo_accounts_file(workspace.path()).with_extension("json.lock");
            fs::create_dir(&lock).unwrap();
            let error = service.remove(name, purge).unwrap_err().to_string();
            fs::remove_dir(&lock).unwrap();
            assert!(error.contains("registry"));
            assert!(root.path().join(name).join(AUTH_FILE).exists());
            assert!(service.find(name).is_ok());
            assert!(!error.contains("recognizable-fake-secret"));
        }
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn quarantine_setup_failure_leaves_live_profile_and_registry_unchanged() {
        let (workspace, root) = setup();
        let source = root.path().join("source");
        fs::write(&source, "recognizable-fake-secret").unwrap();
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", root.path());
        let service = AccountLifecycle::new(workspace.path(), FakeRunner::default()).unwrap();
        service.import("alice", &source).unwrap();
        fs::write(root.path().join(".quarantine"), "not a directory").unwrap();

        let error = service.remove("alice", false).unwrap_err().to_string();
        assert!(root.path().join("alice").join(AUTH_FILE).exists());
        assert!(service.find("alice").is_ok());
        assert!(!error.contains("recognizable-fake-secret"));
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn status_maps_missing_timeout_nonzero_and_not_logged_in_without_output_leaks() {
        let cases = [
            (
                RunnerOutput {
                    success: false,
                    unavailable: true,
                    timed_out: false,
                    exit_code: None,
                    summary: "Codex CLI is not installed or not on PATH".into(),
                },
                LoginState::CliMissing,
            ),
            (
                RunnerOutput {
                    success: false,
                    unavailable: false,
                    timed_out: true,
                    exit_code: None,
                    summary: "Codex login status timed out".into(),
                },
                LoginState::TimedOut,
            ),
            (
                RunnerOutput {
                    success: false,
                    unavailable: false,
                    timed_out: false,
                    exit_code: Some(9),
                    summary: "Codex login status failed".into(),
                },
                LoginState::Failed,
            ),
            (
                RunnerOutput {
                    success: false,
                    unavailable: false,
                    timed_out: false,
                    exit_code: Some(1),
                    summary: "not logged in".into(),
                },
                LoginState::NotLoggedIn,
            ),
        ];
        for (index, (output, expected)) in cases.into_iter().enumerate() {
            let (workspace, root) = setup();
            let source = root.path().join(format!("source-{index}"));
            fs::write(&source, "recognizable-fake-secret").unwrap();
            std::env::set_var("LOOM_CODEX_PROFILE_ROOT", root.path());
            let importer = AccountLifecycle::new(workspace.path(), FakeRunner::default()).unwrap();
            importer.import("alice", &source).unwrap();
            let service = AccountLifecycle::new(workspace.path(), StatusRunner(output)).unwrap();
            let status = service.status("alice").unwrap();
            assert_eq!(status.login_state, expected);
            assert!(!serde_json::to_string(&status)
                .unwrap()
                .contains("recognizable-fake-secret"));
        }
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }
}
