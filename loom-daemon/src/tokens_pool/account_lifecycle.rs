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
use super::session_lifecycle::{container_name, is_session_managed};

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
    /// The profile has been adopted by `loom-daemon accounts session start`
    /// (issue #6925, ADR-0017 Decision 1's ownership rule) — no ambient host
    /// `codex` process may probe or refresh this `CODEX_HOME` directly — and
    /// the in-container probe that replaces the host-direct one (issue #6927)
    /// could not run either, because the account's session container is not
    /// running (or `docker` itself is unavailable). Nothing is known about
    /// the account's auth state; start the container
    /// (`loom-daemon accounts session start <name>`) and re-run to find out.
    SessionUnavailable,
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
    /// Whether this profile has been adopted by `accounts session start`
    /// (issue #6927). Before the in-container probe existed, `login_state`
    /// itself carried this fact (as the former `SessionManaged` placeholder);
    /// now that a session-managed account reports a real probed state
    /// indistinguishable from a host-direct one, the mechanism is reported
    /// separately instead of being inferred from a missing probe.
    pub session_managed: bool,
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

/// Classify `codex login status` output into the three stable summaries the
/// rest of this module keys on. Shared by the host-direct probe and the
/// in-container probe (issue #6927) so a session-managed account's auth state
/// is read exactly the way a host-direct one is — only the transport differs.
#[must_use]
pub fn classify_login_status(success: bool, text: &str) -> &'static str {
    let text = text.to_ascii_lowercase();
    if text.contains("not logged in") {
        "not logged in"
    } else if success && text.contains("logged in") {
        "logged in"
    } else {
        "Codex login status failed"
    }
}

/// Map a `codex login status` probe result onto a reportable [`LoginState`].
/// Shared by both probe transports for the same reason
/// [`classify_login_status`] is.
#[must_use]
pub fn login_state_from(output: &RunnerOutput) -> LoginState {
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
}

pub trait CodexCommandRunner {
    fn login(&self, profile: &Path, device_auth: bool) -> Result<RunnerOutput>;
    fn login_status(&self, profile: &Path) -> Result<RunnerOutput>;

    /// Probe a **session-managed** profile's auth state from inside the
    /// container that owns it (issue #6927): `docker exec <container> codex
    /// login status`, never a host-direct `CODEX_HOME` read (ADR-0017
    /// Decision 1 forbids the latter outright once a profile is adopted).
    ///
    /// `Ok(None)` means no probe was possible at all — the container is not
    /// running, or the container runtime itself is unavailable — which is
    /// reported as [`LoginState::SessionUnavailable`], never as evidence
    /// that the account is logged out.
    ///
    /// The default implementation returns `Ok(None)`: a runner with no
    /// container seam (every test double predating #6927) truthfully reports
    /// "could not probe" rather than fabricating an auth state.
    fn session_login_status(&self, container: &str) -> Result<Option<RunnerOutput>> {
        let _ = container;
        Ok(None)
    }
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
                let summary =
                    classify_login_status(status.success(), &String::from_utf8_lossy(&bytes));
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

    fn session_login_status(&self, container: &str) -> Result<Option<RunnerOutput>> {
        super::session_lifecycle::probe_container_login_status(
            &super::session_lifecycle::ProcessContainerRunner,
            container,
        )
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
        self.add_with_email(name, device_auth, None)
    }

    /// As [`Self::add`], but also registers `email` (issue #7389) so
    /// `loom-daemon accounts session <action>` can later resolve this account
    /// by that email as well as by `name` — see
    /// [`super::account_registry::account_matches_reference`].
    pub fn add_with_email(
        &self,
        name: &str,
        device_auth: bool,
        email: Option<&str>,
    ) -> Result<AccountStatus> {
        validate_name(name)?;
        self.ensure_absent(name)?;
        let profile = self.claim_profile_dir(name)?;
        let result = self.runner.login(&profile, device_auth)?;
        if !result.success {
            // A real `codex login --device-auth` populates `log/`/`tmp/`
            // inside `CODEX_HOME` before the device code is redeemed, so a
            // timed-out or cancelled login leaves a non-empty, credential-less
            // directory (issue #7400) — the emptiness check this replaced
            // never caught that case. `claim_profile_dir` guarantees this
            // call exclusively created `profile`, so removing it here can
            // never touch a concurrent winner's files; the only thing worth
            // preserving is an actual credential.
            if !profile.join(AUTH_FILE).is_file() {
                let _ = fs::remove_dir_all(&profile);
            }
            return Err(login_failure(result));
        }
        // The post-login commit is all-or-nothing over the (profile, registry)
        // pair, mirroring `import`: any failure removes the profile this call
        // created, so the name stays reusable instead of being wedged as an
        // unregistered credential directory that `remove` cannot reach.
        let commit = (|| -> Result<()> {
            tighten_profile_permissions(&profile)?;
            if !inspect_profile(&profile).valid() {
                bail!("Codex login completed but the profile credential is missing or unsafe");
            }
            register_codex_account(&self.workspace, name, name, true, email)
        })();
        if let Err(error) = commit {
            fs::remove_dir_all(&profile)
                .context("account setup failed and new profile rollback also failed")?;
            return Err(error.context("account setup failed; new profile was removed"));
        }
        self.status(name)
    }

    pub fn import(&self, name: &str, source: &Path) -> Result<AccountStatus> {
        self.import_with_email(name, source, None)
    }

    /// As [`Self::import`], but also registers `email` (issue #7389) — see
    /// [`Self::add_with_email`].
    pub fn import_with_email(
        &self,
        name: &str,
        source: &Path,
        email: Option<&str>,
    ) -> Result<AccountStatus> {
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
        self.claim_profile_dir(name)?;
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
        if let Err(error) = register_codex_account(&self.workspace, name, name, true, email) {
            // Safe to delete unconditionally: `claim_profile_dir` guarantees
            // this call exclusively created the directory, so a concurrent
            // winner's profile can never be destroyed by this rollback.
            fs::remove_dir_all(&profile)
                .context("account registry update failed and imported profile rollback failed")?;
            return Err(error.context("account registry update failed; imported profile removed"));
        }
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
        if is_session_managed(&account.credential_reference) {
            // Ownership rule (issue #6925, ADR-0017 Decision 1): once a
            // profile is adopted by `session start`, the session container
            // is the sole process allowed to touch its `CODEX_HOME` — an
            // ambient host `codex login` here would race the container's
            // own refresh chain (the exact `auth.json` clobber class this
            // rule exists to prevent).
            bail!(
                "Codex account {name:?} is session-managed; re-authenticate via `loom-daemon \
                 accounts session attach {name}` (interactive `codex login` inside the \
                 container), not a host-direct `reauth`"
            );
        }
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
        let recovery_file = account.credential_reference.join("recovery.json");
        if !purge {
            let metadata = serde_json::json!({
                "version": 1,
                "provider": "codex",
                "name": name,
                "retired_at_unix": stamp,
                "original_reference": name,
            });
            let mut file =
                private_file(&recovery_file).context("failed to create recovery metadata")?;
            let metadata_result = (|| -> Result<()> {
                serde_json::to_writer_pretty(&mut file, &metadata)?;
                file.write_all(b"\n")?;
                file.sync_all()?;
                Ok(())
            })();
            if metadata_result.is_err() {
                let _ = fs::remove_file(&recovery_file);
            }
            metadata_result.context("failed to prepare recovery metadata")?;
        }
        // The registry commit happens *before* the profile leaves its live
        // location. If the process dies between the two steps, the residue is
        // an orphan profile directory with no registry entry — annoying but
        // inert. The previous order (rename, then unregister) could leave a
        // registry entry pointing at a vanished directory, which poisoned
        // `codex_inventory` for every account until the registry was
        // hand-edited.
        let removed = match unregister_codex_account(&self.workspace, name) {
            Ok(removed) => removed,
            Err(error) => {
                // Registry and profile are both untouched; remove only the
                // metadata this call staged so the profile is byte-identical
                // to its pre-command state. Cleanup is best effort and only
                // annotates the error when residue actually survives.
                let metadata_left_behind =
                    !purge && fs::remove_file(&recovery_file).is_err() && recovery_file.exists();
                let result: Result<RemovalOutcome> =
                    Err(error.context("account registry update failed; profile is unchanged"));
                return if metadata_left_behind {
                    result.context("recovery metadata rollback also failed")
                } else {
                    result
                };
            }
        };
        if let Err(error) = fs::rename(&account.credential_reference, &destination) {
            // The profile is still live: restore its registry entry verbatim
            // and remove the metadata this call staged. The rename error is
            // the actionable one; rollback failures only annotate it.
            let reregistered = register_codex_account(
                &self.workspace,
                name,
                &removed.credential_reference,
                removed.enabled,
                removed.email.as_deref(),
            );
            let metadata_left_behind =
                !purge && fs::remove_file(&recovery_file).is_err() && recovery_file.exists();
            let mut result: Result<RemovalOutcome> =
                Err(error).context("failed to quarantine profile atomically");
            if let Err(reregister_error) = reregistered {
                result =
                    result.context(format!("registry rollback also failed: {reregister_error:#}"));
            }
            if metadata_left_behind {
                result = result.context("recovery metadata rollback also failed");
            }
            return result;
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
        Ok(RemovalOutcome {
            provider: AccountProvider::Codex,
            name: name.into(),
            purged: false,
            recovery_reference: Some(recovery_reference),
        })
    }

    /// Move a profile directory and its registry entry to `new_name`,
    /// atomically from the caller's perspective: any failure after the
    /// registry's old entry is freed rolls the directory move and the
    /// registry back to the pre-rename state (issue #7401).
    ///
    /// Refuses when `old_name` is session-managed (`is_session_managed`,
    /// ADR-0017 Decision 1's ownership rule, the same check `reauth` already
    /// applies) — the session container's bind mount is keyed to the
    /// original directory path, and adoption is permanent regardless of
    /// whether the container happens to be running right now, so renaming a
    /// session-managed profile is refused unconditionally rather than only
    /// while its container is observed running.
    pub fn rename(&self, old_name: &str, new_name: &str) -> Result<AccountStatus> {
        validate_name(old_name)?;
        validate_name(new_name)?;
        if old_name == new_name {
            bail!("Codex account {old_name:?} rename target must differ from its current name");
        }
        let account = self.find(old_name)?;
        if is_session_managed(&account.credential_reference) {
            bail!(
                "Codex account {old_name:?} is session-managed; stop its session container \
                 (`loom-daemon accounts session stop {old_name}`) before renaming it, per the \
                 #7246 ownership rule"
            );
        }
        self.ensure_absent(new_name)?;

        // Registry first, mirroring `remove`'s registry-before-bytes ordering:
        // a crash between the two steps leaves an orphan directory (inert,
        // recoverable via `adopt`) rather than a registry entry pointing at a
        // vanished one.
        let removed = unregister_codex_account(&self.workspace, old_name)?;
        let old_profile = account.credential_reference.clone();
        let new_profile = self.profile(new_name)?;

        let commit = (|| -> Result<()> {
            fs::rename(&old_profile, &new_profile)
                .context("failed to rename Codex profile directory")?;
            register_codex_account(&self.workspace, new_name, new_name, removed.enabled)
        })();

        if let Err(error) = commit {
            // Best-effort rollback: move the directory back if it moved, then
            // restore the freed registry entry under its original name.
            if new_profile.exists() && !old_profile.exists() {
                let _ = fs::rename(&new_profile, &old_profile);
            }
            let reregistered = register_codex_account(
                &self.workspace,
                old_name,
                &removed.credential_reference,
                removed.enabled,
            );
            return match reregistered {
                Ok(()) => {
                    Err(error.context("rename failed; account restored to its original name"))
                }
                Err(reregister_error) => Err(error.context(format!(
                    "rename failed and registry rollback also failed: {reregister_error:#}"
                ))),
            };
        }
        self.status_without_probe(new_name)
    }

    /// Register an on-disk, credentialed Codex profile directory into
    /// `.loom/accounts.json` when it is not already a registry entry (issue
    /// #7401). This is the supported recovery path once the registry file
    /// exists: `account_inventory`'s pre-registry directory discovery
    /// (`codex_inventory`) only runs when `.loom/accounts.json` is entirely
    /// absent, so a workspace that already has a registry has no other way
    /// to make a fresh or hand-renamed directory visible again — `enable`
    /// on an unregistered name fails with "does not exist" rather than
    /// adopting it.
    pub fn adopt(&self, name: &str) -> Result<AccountStatus> {
        validate_name(name)?;
        let profile = self.profile(name)?;
        let diagnostics = inspect_profile(&profile);
        if !diagnostics.profile_exists {
            bail!(
                "Codex profile {name:?} does not exist under the configured profile root; \
                 nothing to adopt"
            );
        }
        if !diagnostics.valid() {
            bail!(
                "Codex profile {name:?} must pass credential shape and permission validation \
                 before it can be adopted"
            );
        }
        // `register_codex_account` itself is the single source of truth for
        // "already registered" (it reads `.loom/accounts.json` directly, not
        // the discovery-widened inventory), so a pre-registry, merely
        // *discovered* profile is still adoptable here.
        register_codex_account(&self.workspace, name, name, true)?;
        self.status_without_probe(name)
    }

    fn status_for(&self, account: AccountDescriptor, probe: bool) -> Result<AccountStatus> {
        let diagnostics = inspect_profile(&account.credential_reference);
        let session_managed = is_session_managed(&account.credential_reference);
        let login_state = if session_managed {
            // Ownership rule (issue #6925, ADR-0017 Decision 1): a
            // session-managed profile refuses host-direct `CODEX_HOME` use,
            // including this read-only `codex login status` probe — the
            // session container is the sole process allowed to touch it. So
            // ask that container instead of giving up (issue #6927): the
            // probe runs *inside* the ownership boundary, which is exactly
            // how it stays compatible with the rule rather than an exception
            // to it. Profile diagnostics deliberately do not gate this probe:
            // an adopted profile is owned by the image's uid, so a host-side
            // owner/permission mismatch is expected and says nothing about
            // whether the container's own auth chain is alive.
            if probe {
                match self
                    .runner
                    .session_login_status(&container_name(&account.id.name))?
                {
                    Some(output) => login_state_from(&output),
                    None => LoginState::SessionUnavailable,
                }
            } else {
                LoginState::NotChecked
            }
        } else if !probe || !diagnostics.valid() {
            LoginState::NotChecked
        } else {
            login_state_from(&self.runner.login_status(&account.credential_reference)?)
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
            session_managed,
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
        let profile = self.profile(name)?;
        // With no explicit per-repo registry, `account_inventory` discovers
        // *every* directory under the profile root as an account by its
        // directory name (see `discovered_codex_profiles`) — so a bare
        // inventory-name match here would also fire for a credential-less
        // leftover from an interrupted login (issue #7400) and never let the
        // more specific branch below run. The only case an inventory match
        // adds real signal beyond "does `profile` hold a credential" is an
        // explicit repo registry entry that pins `name` to some *other*
        // backing directory (a genuine naming conflict this call cannot see
        // just by looking at `profile`).
        let pinned_elsewhere = account_inventory(&self.workspace, AccountProvider::Codex)?
            .into_iter()
            .any(|account| {
                account.id.name == name
                    && account.credential_reference.file_name() != profile.file_name()
            });
        if pinned_elsewhere || profile.join(AUTH_FILE).is_file() {
            // A genuine account: either pinned to a different directory
            // already, or this directory holds a real credential. Point at
            // the recovery path rather than a dead end.
            bail!("Codex account {name:?} already exists; run `accounts reauth {name}` to refresh its credential");
        }
        if profile.exists() {
            // No registration and no credential at this path — a leftover
            // directory that survived whatever created it (e.g. a login this
            // process could not clean up itself, such as a hard-killed
            // process; issue #7400 fixed the common in-process case in
            // `add()`'s own failure-cleanup path). Rather than guessing at
            // its provenance and deleting it automatically, say exactly what
            // to remove so the operator's `rm -rf` fallback becomes a
            // one-command copy/paste.
            bail!(
                "Codex profile directory {} exists but holds no credential and is not \
                 registered as an account; delete it (`rm -rf {}`) and retry `accounts add`",
                profile.display(),
                profile.display()
            );
        }
        Ok(())
    }

    fn profile(&self, name: &str) -> Result<PathBuf> {
        validate_name(name)?;
        Ok(self.root.join(name))
    }

    /// Atomically claim the profile directory for `name`. The non-recursive
    /// `create_dir` is the mutual-exclusion point for concurrent `add`/`import`
    /// of the same name: exactly one caller creates the directory and thereby
    /// owns every later rollback of it; the loser fails here without ever
    /// touching the winner's files or registration.
    fn claim_profile_dir(&self, name: &str) -> Result<PathBuf> {
        let profile = self.profile(name)?;
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(&profile) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                bail!("Codex account {name:?} already exists");
            }
            Err(error) => {
                return Err(anyhow!(error).context("failed to create Codex profile directory"))
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&profile, fs::Permissions::from_mode(0o700))?;
        }
        Ok(profile)
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
        skip_auth_write: bool,
        /// Mirrors real `codex login --device-auth`, which creates `log/` and
        /// `tmp/` inside `CODEX_HOME` before the device code is redeemed —
        /// so a timed-out/cancelled login leaves a non-empty, credential-less
        /// directory behind (issue #7400) rather than a truly empty one.
        leave_login_artifacts: bool,
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
            if self.fail_login && self.leave_login_artifacts {
                fs::create_dir_all(profile.join("log"))?;
                fs::create_dir_all(profile.join("tmp"))?;
            }
            if !self.fail_login && !self.skip_auth_write {
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

    /// A manually provisioned (pre-registry) profile with safe permissions,
    /// as an operator would create with `codex login` by hand.
    fn provision_manual_profile(root: &Path, name: &str) {
        let dir = root.join(name);
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join(AUTH_FILE), "recognizable-fake-secret").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
            fs::set_permissions(dir.join(AUTH_FILE), fs::Permissions::from_mode(0o600)).unwrap();
        }
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

    /// #7389: `add --email`/`import --email` register a non-secret email
    /// alongside the profile, so `loom-daemon accounts session <action>` can
    /// later resolve the account by either the profile name or this email.
    #[test]
    #[serial]
    fn add_with_email_registers_the_email_in_the_inventory() {
        let (workspace, root) = setup();
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", root.path());
        let service = AccountLifecycle::new(workspace.path(), FakeRunner::default()).unwrap();
        service
            .add_with_email("agent-1", false, Some("agent-1@2amlogic.com"))
            .unwrap();
        let accounts = account_inventory(workspace.path(), AccountProvider::Codex).unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].email.as_deref(), Some("agent-1@2amlogic.com"));
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
    fn interrupted_login_leftover_is_removed_and_the_name_is_reusable() {
        // Regression test for issue #7400: a real `codex login --device-auth`
        // that times out or is cancelled leaves `log/`/`tmp/` behind in
        // `CODEX_HOME` — a non-empty, credential-less directory the old
        // `fs::read_dir(...).next().is_none()` emptiness check never cleaned
        // up, permanently wedging the name behind a false "already exists".
        let (workspace, root) = setup();
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", root.path());
        let flaky = AccountLifecycle::new(
            workspace.path(),
            FakeRunner {
                fail_login: true,
                leave_login_artifacts: true,
                ..FakeRunner::default()
            },
        )
        .unwrap();
        let error = flaky.add("alice", true).unwrap_err();
        assert_eq!(login_exit_code(&error), Some(23));
        // The whole directory — including the `log`/`tmp` leftovers — must be
        // gone, not just left behind because it wasn't literally empty.
        assert!(!root.path().join("alice").exists());
        assert!(flaky.list(false).unwrap().is_empty());

        // A second `add` for the same name must succeed immediately instead
        // of failing with "already exists".
        let healthy = AccountLifecycle::new(workspace.path(), FakeRunner::default()).unwrap();
        healthy.add("alice", true).unwrap();
        assert_eq!(healthy.list(false).unwrap().len(), 1);
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn add_refuses_a_credentialed_leftover_and_points_at_reauth() {
        // A directory that actually holds a credential is a genuine existing
        // account, not a leftover to reclaim — `add` must still refuse it,
        // and the message must point at the recovery path (issue #7400 AC #3).
        let (workspace, root) = setup();
        provision_manual_profile(root.path(), "alice");
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", root.path());
        let service = AccountLifecycle::new(workspace.path(), FakeRunner::default()).unwrap();
        let error = service.add("alice", false).unwrap_err().to_string();
        assert!(error.contains("already exists"));
        assert!(error.contains("accounts reauth alice"));
        assert_eq!(
            fs::read_to_string(root.path().join("alice").join(AUTH_FILE)).unwrap(),
            "recognizable-fake-secret"
        );
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn add_refuses_a_credential_less_leftover_with_an_actionable_message() {
        // A leftover that this process could not clean up itself (e.g. one
        // that predates this fix, or survived a hard-killed process) must not
        // be silently deleted by a later `add` — but the error must say
        // exactly what to remove (issue #7400 AC #3).
        let (workspace, root) = setup();
        let leftover = root.path().join("alice");
        fs::create_dir_all(leftover.join("log")).unwrap();
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", root.path());
        let service = AccountLifecycle::new(workspace.path(), FakeRunner::default()).unwrap();
        let error = service.add("alice", false).unwrap_err().to_string();
        assert!(!error.contains("already exists"));
        assert!(error.contains(&leftover.display().to_string()));
        assert!(error.to_lowercase().contains("rm -rf"));
        assert!(leftover.exists());
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
    fn import_registry_failure_removes_committed_profile_without_secret_leak() {
        let (workspace, root) = setup();
        let source = root.path().join("source");
        fs::write(&source, "recognizable-fake-secret").unwrap();
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", root.path());
        let service = AccountLifecycle::new(workspace.path(), FakeRunner::default()).unwrap();
        let lock = per_repo_accounts_file(workspace.path()).with_extension("json.lock");
        fs::create_dir_all(lock.parent().unwrap()).unwrap();
        fs::create_dir(&lock).unwrap();

        let error = service.import("alice", &source).unwrap_err().to_string();
        fs::remove_dir(&lock).unwrap();
        assert!(error.contains("registry"));
        assert!(!root.path().join("alice").exists());
        assert!(service.list(false).unwrap().is_empty());
        assert!(!error.contains("recognizable-fake-secret"));
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
    fn recovery_metadata_collision_leaves_live_profile_and_registry_unchanged() {
        let (workspace, root) = setup();
        let source = root.path().join("source");
        fs::write(&source, "recognizable-fake-secret").unwrap();
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", root.path());
        let service = AccountLifecycle::new(workspace.path(), FakeRunner::default()).unwrap();
        service.import("alice", &source).unwrap();
        let recovery = root.path().join("alice/recovery.json");
        fs::write(&recovery, "recognizable-existing-metadata").unwrap();

        let error = service.remove("alice", false).unwrap_err().to_string();
        assert!(root.path().join("alice").join(AUTH_FILE).exists());
        assert!(service.find("alice").is_ok());
        assert_eq!(fs::read_to_string(recovery).unwrap(), "recognizable-existing-metadata");
        assert!(!error.contains("recognizable-fake-secret"));
        assert!(!error.contains("recognizable-existing-metadata"));
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    #[cfg(unix)]
    fn quarantine_rename_failure_rolls_back_staged_recovery_metadata() {
        use std::os::unix::fs::PermissionsExt;

        // Permission-based rename injection is meaningless as root.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }

        let (workspace, root) = setup();
        let source_dir = tempfile::tempdir().unwrap();
        let source = source_dir.path().join("auth.json");
        fs::write(&source, "recognizable-fake-secret").unwrap();
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", root.path());
        let service = AccountLifecycle::new(workspace.path(), FakeRunner::default()).unwrap();
        service.import("alice", &source).unwrap();

        // Pre-create `.quarantine` so lifecycle setup succeeds, then make the
        // profile root non-writable. Staging `recovery.json` inside the live
        // profile still succeeds, but unlinking `alice` from the root during
        // `fs::rename` fails with EACCES.
        fs::create_dir(root.path().join(".quarantine")).unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o500)).unwrap();
        let error = service.remove("alice", false).unwrap_err().to_string();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();

        assert!(error.contains("quarantine"));
        // The staged metadata this invocation created must not survive.
        assert!(!root.path().join("alice/recovery.json").exists());
        assert!(!error.contains("recovery metadata rollback also failed"));
        // Profile, credential bytes, and registry are all untouched.
        assert!(root.path().join("alice").join(AUTH_FILE).is_file());
        assert_eq!(
            fs::read_to_string(root.path().join("alice").join(AUTH_FILE)).unwrap(),
            "recognizable-fake-secret"
        );
        assert!(service.find("alice").is_ok());
        assert!(!error.contains("recognizable-fake-secret"));
        assert!(fs::read_dir(root.path().join(".quarantine"))
            .unwrap()
            .next()
            .is_none());

        // A later recoverable removal is not blocked by stale metadata.
        let removed = service.remove("alice", false).unwrap();
        assert!(!removed.purged);
        assert!(removed.recovery_reference.is_some());
        assert!(!root.path().join("alice").exists());
        assert!(service.find("alice").is_err());
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn concurrent_import_of_same_name_never_destroys_the_winner() {
        let (workspace, root) = setup();
        let source_dir = tempfile::tempdir().unwrap();
        let source = source_dir.path().join("auth.json");
        fs::write(&source, "recognizable-fake-secret").unwrap();
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", root.path());
        let service = AccountLifecycle::new(workspace.path(), FakeRunner::default()).unwrap();

        let outcomes: Vec<Result<AccountStatus>> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..4)
                .map(|_| scope.spawn(|| service.import("alice", &source)))
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect()
        });

        assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
        for failure in outcomes.iter().filter(|outcome| outcome.is_err()) {
            let message = format!("{:#}", failure.as_ref().unwrap_err());
            assert!(message.contains("exists"));
            assert!(!message.contains("recognizable-fake-secret"));
        }
        // No loser's rollback may have deleted the winner's profile or its
        // registration.
        assert_eq!(
            fs::read_to_string(root.path().join("alice").join(AUTH_FILE)).unwrap(),
            "recognizable-fake-secret"
        );
        let listed = service.list(false).unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].diagnostics.valid());
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn add_registry_failure_rolls_back_profile_and_frees_the_name() {
        let (workspace, root) = setup();
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", root.path());
        let service = AccountLifecycle::new(workspace.path(), FakeRunner::default()).unwrap();
        let lock = per_repo_accounts_file(workspace.path()).with_extension("json.lock");
        fs::create_dir_all(lock.parent().unwrap()).unwrap();
        fs::create_dir(&lock).unwrap();

        let error = format!("{:#}", service.add("alice", false).unwrap_err());
        fs::remove_dir(&lock).unwrap();
        assert!(error.contains("new profile was removed"));
        assert!(!error.contains("recognizable-fake-secret"));
        assert!(!root.path().join("alice").exists());
        assert!(service.list(false).unwrap().is_empty());
        // The name is immediately reusable, not permanently wedged.
        service.add("alice", false).unwrap();
        assert_eq!(service.list(false).unwrap().len(), 1);
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn add_without_credential_rolls_back_profile_and_frees_the_name() {
        let (workspace, root) = setup();
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", root.path());
        let broken = AccountLifecycle::new(
            workspace.path(),
            FakeRunner {
                skip_auth_write: true,
                ..FakeRunner::default()
            },
        )
        .unwrap();

        let error = format!("{:#}", broken.add("alice", false).unwrap_err());
        assert!(error.contains("new profile was removed"));
        assert!(!root.path().join("alice").exists());
        assert!(broken.list(false).unwrap().is_empty());
        // A later, healthy login can claim the same name.
        let healthy = AccountLifecycle::new(workspace.path(), FakeRunner::default()).unwrap();
        healthy.add("alice", false).unwrap();
        assert_eq!(healthy.list(false).unwrap().len(), 1);
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn discovered_profiles_support_disable_enable_and_remove() {
        let (workspace, root) = setup();
        provision_manual_profile(root.path(), "alice");
        provision_manual_profile(root.path(), "bob");
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", root.path());
        let service = AccountLifecycle::new(workspace.path(), FakeRunner::default()).unwrap();

        let listed = service.list(false).unwrap();
        assert_eq!(listed.len(), 2);
        assert!(listed
            .iter()
            .all(|account| account.provenance == InventoryProvenance::Shared));

        // Mutating one discovered account adopts the whole discovered set, so
        // `list` shows the same accounts before and after.
        let disabled = service.disable("alice").unwrap();
        assert!(!disabled.enabled);
        let listed = service.list(false).unwrap();
        assert_eq!(listed.len(), 2);
        let bob = listed.iter().find(|account| account.name == "bob").unwrap();
        assert!(bob.enabled);

        assert!(service.enable("alice").unwrap().enabled);

        let removed = service.remove("bob", false).unwrap();
        assert!(removed.recovery_reference.is_some());
        assert!(!root.path().join("bob").exists());
        assert_eq!(service.list(false).unwrap().len(), 1);
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn disable_enable_and_remove_work_pre_registry_without_registry_file() {
        let (workspace, root) = setup();
        provision_manual_profile(root.path(), "alice");
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", root.path());
        let service = AccountLifecycle::new(workspace.path(), FakeRunner::default()).unwrap();

        // `remove` on a discovered account adopts and then unregisters it in
        // one lifecycle command; the quarantine copy keeps the credential.
        let removed = service.remove("alice", false).unwrap();
        let recovery = root
            .path()
            .join(".quarantine")
            .join(removed.recovery_reference.unwrap());
        assert!(recovery.join(AUTH_FILE).exists());
        assert!(service.list(false).unwrap().is_empty());
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn orphan_profile_directory_does_not_poison_registered_inventory() {
        let (workspace, root) = setup();
        let source = root.path().join("source");
        fs::write(&source, "recognizable-fake-secret").unwrap();
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", root.path());
        let service = AccountLifecycle::new(workspace.path(), FakeRunner::default()).unwrap();
        service.import("alice", &source).unwrap();
        service.import("bob", &source).unwrap();

        // Crash residue from the reordered `remove` (killed between the
        // registry commit and the quarantine rename) is a live directory with
        // no registry entry. It must not affect any other account's lifecycle.
        provision_manual_profile(root.path(), "carol");
        assert_eq!(service.list(false).unwrap().len(), 2);
        service.disable("alice").unwrap();
        service.remove("alice", false).unwrap();
        assert_eq!(service.list(false).unwrap().len(), 1);
        // Only the orphaned name itself stays blocked until an operator
        // clears the directory.
        assert!(service
            .import("carol", &source)
            .unwrap_err()
            .to_string()
            .contains("exists"));
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    // ---- rename / adopt (issue #7401) -------------------------------------

    #[test]
    #[serial]
    fn rename_moves_directory_and_updates_registry_atomically() {
        let (workspace, root) = setup();
        let source_dir = tempfile::tempdir().unwrap();
        let source = source_dir.path().join("auth.json");
        fs::write(&source, "recognizable-fake-secret").unwrap();
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", root.path());
        let service = AccountLifecycle::new(workspace.path(), FakeRunner::default()).unwrap();
        service.import("agent-3", &source).unwrap();
        service.disable("agent-3").unwrap();

        let renamed = service.rename("agent-3", "agent-three").unwrap();
        assert_eq!(renamed.name, "agent-three");
        assert!(!renamed.enabled, "rename must preserve the enabled flag");
        assert!(!root.path().join("agent-3").exists());
        assert_eq!(
            fs::read_to_string(root.path().join("agent-three/auth.json")).unwrap(),
            "recognizable-fake-secret"
        );
        assert!(service.find("agent-3").is_err());
        assert!(service.find("agent-three").is_ok());
        assert_eq!(service.list(false).unwrap().len(), 1);
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn rename_refuses_a_session_managed_profile() {
        let (workspace, root) = setup();
        let source_dir = tempfile::tempdir().unwrap();
        let source = source_dir.path().join("auth.json");
        fs::write(&source, "recognizable-fake-secret").unwrap();
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", root.path());
        let service = AccountLifecycle::new(workspace.path(), FakeRunner::default()).unwrap();
        service.import("alice", &source).unwrap();
        super::super::session_lifecycle::mark_session_managed(
            &root.path().join("alice"),
            "loom-codex-session-alice",
        )
        .unwrap();

        let error = service.rename("alice", "alice2").unwrap_err().to_string();
        assert!(error.contains("session-managed"));
        assert!(error.contains("session stop"));
        // Nothing moved and the registry is untouched.
        assert!(root.path().join("alice").join(AUTH_FILE).is_file());
        assert!(service.find("alice").is_ok());
        assert!(service.find("alice2").is_err());
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn rename_rejects_same_name_missing_source_and_existing_target() {
        let (workspace, root) = setup();
        let source_dir = tempfile::tempdir().unwrap();
        let source = source_dir.path().join("auth.json");
        fs::write(&source, "recognizable-fake-secret").unwrap();
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", root.path());
        let service = AccountLifecycle::new(workspace.path(), FakeRunner::default()).unwrap();
        service.import("alice", &source).unwrap();
        service.import("bob", &source).unwrap();

        assert!(service
            .rename("alice", "alice")
            .unwrap_err()
            .to_string()
            .contains("must differ"));
        assert!(service.rename("ghost", "ghost2").is_err());
        // Renaming onto an already-registered name must not clobber it.
        assert!(service
            .rename("alice", "bob")
            .unwrap_err()
            .to_string()
            .contains("exists"));
        assert!(service.find("alice").is_ok());
        assert!(service.find("bob").is_ok());
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn adopt_registers_an_unregistered_on_disk_profile() {
        let (workspace, root) = setup();
        provision_manual_profile(root.path(), "alice");
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", root.path());
        let service = AccountLifecycle::new(workspace.path(), FakeRunner::default()).unwrap();
        // A registry already exists (from an unrelated `import`), which is
        // exactly the "renamed profile vanished from `list`" scenario: once
        // any registry entry exists, pre-registry directory discovery stops.
        let source_dir = tempfile::tempdir().unwrap();
        let source = source_dir.path().join("auth.json");
        fs::write(&source, "recognizable-fake-secret").unwrap();
        service.import("bob", &source).unwrap();
        assert_eq!(service.list(false).unwrap().len(), 1);

        // `enable` on the unregistered directory still fails ("does not
        // exist") -- `adopt` is the supported recovery path instead.
        assert!(service
            .enable("alice")
            .unwrap_err()
            .to_string()
            .contains("does not exist"));

        let adopted = service.adopt("alice").unwrap();
        assert_eq!(adopted.name, "alice");
        assert!(adopted.enabled);
        assert_eq!(service.list(false).unwrap().len(), 2);
        assert!(service.find("alice").is_ok());
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn adopt_rejects_missing_directory_and_already_registered_name() {
        let (workspace, root) = setup();
        let source_dir = tempfile::tempdir().unwrap();
        let source = source_dir.path().join("auth.json");
        fs::write(&source, "recognizable-fake-secret").unwrap();
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", root.path());
        let service = AccountLifecycle::new(workspace.path(), FakeRunner::default()).unwrap();
        assert!(service
            .adopt("ghost")
            .unwrap_err()
            .to_string()
            .contains("nothing to adopt"));

        service.import("alice", &source).unwrap();
        // Adopting an already-registered, byte-identical entry is a harmless
        // idempotent no-op (the same convention `register_codex_account`
        // already applies to a concurrent adoption race).
        assert!(service.adopt("alice").is_ok());

        // But adopting over a registered entry that actually differs (here:
        // disabled vs. `adopt`'s always-enabled default) is a real conflict,
        // not a no-op, and must fail rather than silently re-enable it.
        service.disable("alice").unwrap();
        assert!(service
            .adopt("alice")
            .unwrap_err()
            .to_string()
            .contains("exists"));
        assert!(!service.status_without_probe("alice").unwrap().enabled);
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    // ---- session-managed ownership rule (issue #6925, ADR-0017 Decision 1) ----

    #[test]
    #[serial]
    fn reauth_refuses_a_session_managed_profile() {
        let (workspace, root) = setup();
        let source_dir = tempfile::tempdir().unwrap();
        let source = source_dir.path().join("auth.json");
        fs::write(&source, "recognizable-fake-secret").unwrap();
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", root.path());
        let service = AccountLifecycle::new(workspace.path(), FakeRunner::default()).unwrap();
        service.import("alice", &source).unwrap();
        super::super::session_lifecycle::mark_session_managed(
            &root.path().join("alice"),
            "loom-codex-session-alice",
        )
        .unwrap();

        let error = service.reauth("alice", false).unwrap_err().to_string();
        assert!(error.contains("session-managed"));
        assert!(error.contains("session attach"));
        // Refusal happens before any host-direct `codex login` runs.
        assert!(service.runner.calls.lock().unwrap().is_empty());
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    fn import_session_managed(workspace: &Path, root: &Path, name: &str) {
        let source_dir = tempfile::tempdir().unwrap();
        let source = source_dir.path().join("auth.json");
        fs::write(&source, "recognizable-fake-secret").unwrap();
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", root);
        AccountLifecycle::new(workspace, FakeRunner::default())
            .unwrap()
            .import(name, &source)
            .unwrap();
        super::super::session_lifecycle::mark_session_managed(
            &root.join(name),
            &super::super::session_lifecycle::container_name(name),
        )
        .unwrap();
    }

    #[test]
    #[serial]
    fn status_of_a_session_managed_account_never_probes_the_host_directly() {
        let (workspace, root) = setup();
        import_session_managed(workspace.path(), root.path(), "alice");
        // A runner with no container seam reports "could not probe" (the
        // trait's default `session_login_status`), never a fabricated state.
        let service = AccountLifecycle::new(workspace.path(), FakeRunner::default()).unwrap();

        let status = service.status("alice").unwrap();
        assert_eq!(status.login_state, LoginState::SessionUnavailable);
        assert!(status.session_managed);
        // The host-direct read-only probe must never have run (ADR-0017
        // Decision 1's ownership rule).
        assert!(service.runner.calls.lock().unwrap().is_empty());
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    /// Issue #6927: once the in-container probe answers, a session-managed
    /// account reports exactly what a host-direct one would — same
    /// `LoginState`, different transport — and it does so without the
    /// host-direct probe ever touching the adopted `CODEX_HOME`.
    #[test]
    #[serial]
    fn status_of_a_session_managed_account_reports_the_in_container_probe_result() {
        struct ContainerRunner(RunnerOutput, Mutex<Vec<String>>);

        impl CodexCommandRunner for ContainerRunner {
            fn login(&self, _profile: &Path, _device_auth: bool) -> Result<RunnerOutput> {
                unreachable!("probe-only test runner")
            }

            fn login_status(&self, _profile: &Path) -> Result<RunnerOutput> {
                unreachable!("a session-managed profile must never be probed host-directly")
            }

            fn session_login_status(&self, container: &str) -> Result<Option<RunnerOutput>> {
                self.1.lock().unwrap().push(container.to_string());
                Ok(Some(self.0.clone()))
            }
        }

        for (summary, success, expected) in [
            ("logged in", true, LoginState::LoggedIn),
            ("not logged in", false, LoginState::NotLoggedIn),
        ] {
            let (workspace, root) = setup();
            import_session_managed(workspace.path(), root.path(), "alice");
            let service = AccountLifecycle::new(
                workspace.path(),
                ContainerRunner(
                    RunnerOutput {
                        success,
                        unavailable: false,
                        timed_out: false,
                        exit_code: Some(i32::from(!success)),
                        summary: summary.into(),
                    },
                    Mutex::new(Vec::new()),
                ),
            )
            .unwrap();

            let status = service.status("alice").unwrap();
            assert_eq!(status.login_state, expected);
            assert!(status.session_managed);
            assert_eq!(
                *service.runner.1.lock().unwrap(),
                vec!["loom-codex-session-alice".to_string()],
                "the probe must be addressed to the account's own container"
            );
            std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
        }
    }

    #[test]
    #[serial]
    fn a_non_probing_status_of_a_session_managed_account_is_not_checked() {
        let (workspace, root) = setup();
        import_session_managed(workspace.path(), root.path(), "alice");
        let service = AccountLifecycle::new(workspace.path(), FakeRunner::default()).unwrap();
        let status = service.status_without_probe("alice").unwrap();
        assert_eq!(status.login_state, LoginState::NotChecked);
        assert!(status.session_managed);
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    #[cfg(unix)]
    fn bounded_status_real_process_does_not_misclassify_not_logged_in() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = tempfile::tempdir().unwrap();
        let bin = fixture.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let codex = bin.join("codex");
        fs::write(&codex, "#!/bin/sh\nprintf 'Not logged in\\n'\nexit 0\n").unwrap();
        fs::set_permissions(&codex, fs::Permissions::from_mode(0o700)).unwrap();
        let old_path = std::env::var_os("PATH");
        let joined = std::env::join_paths(
            std::iter::once(bin).chain(
                old_path
                    .as_deref()
                    .map(std::env::split_paths)
                    .into_iter()
                    .flatten(),
            ),
        )
        .unwrap();
        std::env::set_var("PATH", joined);
        let output = ProcessCodexRunner::bounded_status(fixture.path()).unwrap();
        if let Some(path) = old_path {
            std::env::set_var("PATH", path);
        } else {
            std::env::remove_var("PATH");
        }

        assert!(output.success);
        assert_eq!(output.summary, "not logged in");
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
