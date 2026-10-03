//! Readiness of Loom's managed Codex `pre_tool_use` hook in one profile:
//! the implementation behind `provision-codex-hooks.sh verify` (issue #9390).
//!
//! `install` and `remove` stay in the shell script, which merges Loom's one
//! entry into an operator-owned `hooks.json`. The readiness DECISION lives
//! here, so the shell stub, `spawn-codex.sh`, the daemon's own callers and the
//! private-session admission gate all read one implementation rather than a
//! shell copy and a Rust copy kept in agreement by a test.
//!
//! # What "ready" means
//!
//! All of:
//!
//! 1. **Installed.** `hooks.json` carries Loom's entry, identified by the
//!    `guard-codex-bridge.sh` ownership marker.
//! 2. **The right registration** for the mode this check runs in:
//!    - *workspace-independent* (no `--bridge`; managed version 2): the entry
//!      IS [`SHARED_COMMAND`], byte for byte. That one fixed command resolves
//!      the session's checkout at hook time, so the same entry serves every
//!      workspace and one trust decision per profile covers all of them.
//!    - *pinned* (`--bridge <path>`; managed version 1): the entry names that
//!      bridge and carries the version marker. Private-clone sessions use
//!      this for their image-owned bridge (#8839).
//! 3. **Pinned by Loom's receipt** (`loom-codex-hooks.json`): the receipt's
//!    `commandSha256` is the SHA-256 of the installed command, so a hand edit
//!    reads as stale.
//! 4. **A readable bridge.** Workspace-independent: the named workspace's own
//!    `.loom/hooks/guard-codex-bridge.sh`, because that is what the entry will
//!    run there. Pinned: the named bridge.
//! 5. **Codex trust for Loom's entry, where it runs**, by the #5005 baseline
//!    diff. Codex records trust as
//!    `hooks.state."<hooks.json path>:pre_tool_use:<group>:<handler>".trusted_hash`
//!    under the CANONICALIZED `CODEX_HOME` it runs with (`codex_hooks::hook_key`,
//!    `find_codex_home`). Only a hash under a key Codex would look Loom's entry
//!    up under, at the runtime `CODEX_HOME` ([`runtime_codex_home`]), counts:
//!    trust accepted on the host does not apply inside the account's session
//!    container (where the same file is `/home/loom/.codex-profile/hooks.json`)
//!    and vice versa, and trust for an operator's hook or another profile's
//!    path is trust for a different hook. Codex skips Loom's entry silently in
//!    every such case. Of those keyed hashes, one must be new since Loom's
//!    current entry was installed (the receipt's `trustBaselineHashes`); a
//!    receipt from before that field existed falls back to "any keyed hash
//!    present" (`legacy-coarse`).
//!
//! Only the profile's directory NAME is ever reported; no path contents and no
//! credential bytes. `auth.json` is never opened.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::Serialize;

/// The ownership marker that identifies Loom's entry among an operator's
/// hooks.
pub const MARKER: &str = "guard-codex-bridge.sh";
/// Loom's non-secret readiness receipt inside the profile.
pub const RECEIPT: &str = "loom-codex-hooks.json";
/// Managed version of a pinned (`--bridge`) registration. Also compared by
/// private-session admission (`private_workspace::bundle::HOOK_VERSION`).
pub const PINNED_VERSION: u32 = 1;
/// Managed version of the workspace-independent registration (#9390).
pub const SHARED_VERSION: u32 = 2;
/// Where a private-clone session's image-owned bridge lives (#8839).
pub const PRIVATE_CONTROL_PREFIX: &str = "/opt/loom/private-control/";
/// `CODEX_HOME` inside every account session container: the profile's mount
/// point (`session_lifecycle`'s `CONTAINER_CODEX_HOME`, the session image's
/// `ENV CODEX_HOME`).
pub const SESSION_CODEX_HOME: &str = "/home/loom/.codex-profile";
/// The marker `accounts session start` writes into a profile it adopts.
const SESSION_MARKER: &str = ".session-managed.json";

/// The workspace-independent managed command (#9390). Must equal
/// `LOOM_SHARED_HOOK_COMMAND` in `provision-codex-hooks.sh`, which writes it;
/// `the_shared_command_is_the_one_the_provisioner_installs` holds them
/// together. Codex runs it as `$SHELL -lc <command>` in the session's cwd.
/// Exit 2 is Codex's PreToolUse *block*; every failure path maps to it,
/// because any other non-zero exit is a hook failure, not a denial.
pub const SHARED_COMMAND: &str = r#"root="$(cd "$(git rev-parse --git-common-dir 2>/dev/null || echo /nonexistent)/.." 2>/dev/null && pwd -P)" && bash "$root/.loom/hooks/guard-codex-bridge.sh" --project-root "$root" --loom-hook-version 2 || { echo "Loom guard: this workspace has no readable .loom/hooks/guard-codex-bridge.sh, or it failed; denying (fail closed, loom#9390)" >&2; exit 2; }"#;

/// Which registration a check expects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Registration {
    /// One fixed command for every workspace (#9390).
    WorkspaceIndependent,
    /// A named bridge baked into the entry.
    Pinned { bridge: PathBuf },
}

impl Registration {
    #[must_use]
    pub fn version(&self) -> u32 {
        match self {
            Self::WorkspaceIndependent => SHARED_VERSION,
            Self::Pinned { .. } => PINNED_VERSION,
        }
    }

    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::WorkspaceIndependent => "workspace-independent",
            Self::Pinned { .. } => "pinned",
        }
    }
}

/// One readiness question.
#[derive(Debug, Clone)]
pub struct Check {
    /// The profile (`CODEX_HOME`) to inspect.
    pub codex_home: PathBuf,
    /// The workspace the session will run in, if named.
    pub workspace: Option<PathBuf>,
    pub registration: Registration,
    /// The bridge to treat as "this checkout's" when no workspace is named:
    /// the provisioner's own sibling `../hooks/guard-codex-bridge.sh`.
    pub fallback_bridge: Option<PathBuf>,
    /// `CODEX_HOME` as Codex will see it when it runs; `None` derives it
    /// ([`runtime_codex_home`]).
    pub runtime_home: Option<PathBuf>,
}

/// The verdict, in the JSON shape `provision-codex-hooks.sh verify --json`
/// has always printed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Verdict {
    pub profile: String,
    pub ready: bool,
    pub installed: bool,
    pub trusted: bool,
    pub trust_signal: &'static str,
    pub stale: bool,
    pub bridge_readable: bool,
    pub version: u32,
    pub registration: &'static str,
    /// Where trust must be recorded for this profile, without a path:
    /// `the session container`, or `profile '<name>' on this host`.
    pub trust_location: String,
    pub reason: String,
}

impl Check {
    /// The bridge whose readability decides criterion 4.
    #[must_use]
    pub fn bridge(&self) -> Option<PathBuf> {
        match &self.registration {
            Registration::Pinned { bridge } => Some(bridge.clone()),
            Registration::WorkspaceIndependent => match &self.workspace {
                Some(workspace) => Some(workspace.join(".loom/hooks").join(MARKER)),
                None => self.fallback_bridge.clone(),
            },
        }
    }

    /// The exact command a correct registration carries.
    #[must_use]
    pub fn expected_command_prefix(&self) -> Option<String> {
        match &self.registration {
            Registration::WorkspaceIndependent => None,
            // As the shell resolved it: the bridge's DIRECTORY canonicalized,
            // its own name kept (`cd "$(dirname ..)" && pwd -P`).
            Registration::Pinned { bridge } => {
                let dir = bridge
                    .parent()
                    .and_then(|dir| dir.canonicalize().ok())
                    .unwrap_or_else(|| bridge.parent().unwrap_or(Path::new("")).to_path_buf());
                Some(dir.join(bridge.file_name()?).display().to_string())
            }
        }
    }

    /// Evaluate readiness. Never fails: an unreadable input is a verdict.
    #[must_use]
    pub fn verify(&self) -> Verdict {
        let profile = profile_label(&self.codex_home);
        let bridge_readable = self
            .bridge()
            .is_some_and(|bridge| std::fs::File::open(bridge).is_ok());
        let mut reason = String::new();
        let installed_cmd = match read_hooks(&self.codex_home) {
            Ok(hooks) => loom_command(&hooks),
            Err(()) => {
                reason = "hooks.json is unreadable or malformed".into();
                None
            }
        };
        let installed = installed_cmd.is_some();

        let runtime_home = self
            .runtime_home
            .clone()
            .unwrap_or_else(|| runtime_codex_home(&self.codex_home));
        let (trusted, trust_signal) = trust_at(&self.codex_home, &runtime_home);
        let trust_location = if runtime_home == Path::new(SESSION_CODEX_HOME) {
            "the session container".to_owned()
        } else {
            format!("profile '{profile}' on this host")
        };

        let mut stale = false;
        let note = |text: String, reason: &mut String| {
            if reason.is_empty() {
                *reason = text;
            }
        };
        if let Some(cmd) = &installed_cmd {
            match receipt_sha(&self.codex_home) {
                None => {
                    stale = true;
                    note(
                        "no managed-hook receipt: the installed entry is unpinned".into(),
                        &mut reason,
                    );
                }
                Some(pinned) if pinned != sha256_hex(cmd.as_bytes()) => {
                    stale = true;
                    note(
                        "the installed managed-hook entry does not match the pinned receipt (stale)"
                            .into(),
                        &mut reason,
                    );
                }
                Some(_) => {}
            }
            match &self.registration {
                Registration::WorkspaceIndependent => {
                    if cmd != SHARED_COMMAND {
                        stale = true;
                        note(
                            if cmd.starts_with(PRIVATE_CONTROL_PREFIX) {
                                "this profile carries a private-clone session's pinned \
                                 registration, not the workspace-independent entry"
                                    .into()
                            } else {
                                "the installed managed-hook entry is not the workspace-independent \
                                 registration (a pre-#9390 per-workspace entry?); re-run install, \
                                 then re-establish Codex hook trust once for this profile"
                                    .into()
                            },
                            &mut reason,
                        );
                    }
                }
                Registration::Pinned { .. } => {
                    if !cmd.contains(&format!("--loom-hook-version {PINNED_VERSION}")) {
                        stale = true;
                        note(
                            format!(
                                "the installed managed-hook entry is not version {PINNED_VERSION}"
                            ),
                            &mut reason,
                        );
                    }
                    if bridge_readable
                        && self
                            .expected_command_prefix()
                            .is_some_and(|prefix| !cmd.starts_with(&prefix))
                    {
                        stale = true;
                        note(
                            "the installed managed-hook entry points at a different bridge than \
                             this workspace's"
                                .into(),
                            &mut reason,
                        );
                    }
                }
            }
        } else {
            note(
                "Loom's managed PreToolUse hook is not installed in this profile".into(),
                &mut reason,
            );
        }

        if !bridge_readable {
            reason = match self.registration {
                Registration::WorkspaceIndependent => "this workspace has no readable \
                     .loom/hooks/guard-codex-bridge.sh for the managed hook to run"
                    .into(),
                Registration::Pinned { .. } => {
                    "the managed hook bridge is missing or unreadable".into()
                }
            };
        } else if !trusted && reason.is_empty() {
            reason = if trust_signal == "baseline-diff-no-new-trust" {
                "Codex hook trust has not been (re-)established for this profile since Loom's \
                 managed hook was last (re)installed — hooks.state carries no trusted_hash beyond \
                 the pre-install baseline"
                    .into()
            } else if trust_signal == "wrong-location" {
                format!(
                    "Codex hook trust is recorded in this profile, but not for Loom's entry at the \
                     hooks.json location Codex reads at runtime ({trust_location}) — trust \
                     accepted on the host does not carry into a session container, or the \
                     reverse; accept the hook-trust prompt where the role runs"
                )
            } else {
                "Codex hook trust is not established for this profile (no hooks.state \
                 trusted_hash in config.toml)"
                    .into()
            };
        }

        let ready = installed && trusted && !stale && bridge_readable;
        if ready {
            let version = self.registration.version();
            reason = if trust_signal == "baseline-diff" {
                format!(
                    "managed hook v{version} installed, pinned, and a NEW Codex hook trust \
                     decision was recorded for this profile since the managed hook was \
                     (re)installed"
                )
            } else {
                format!(
                    "managed hook v{version} installed, pinned, and the profile has established \
                     Codex hook trust (legacy signal: no install-time baseline recorded for this \
                     profile, falling back to any trusted_hash present)"
                )
            };
        }
        if reason.is_empty() {
            reason = "not ready".into();
        }
        Verdict {
            profile,
            ready,
            installed,
            trusted,
            trust_signal,
            stale,
            bridge_readable,
            version: self.registration.version(),
            registration: self.registration.label(),
            trust_location,
            reason,
        }
    }
}

/// `CODEX_HOME` as Codex will see it when it runs with `codex_home`: the
/// session container's mount point for a profile an account session has
/// adopted, otherwise the canonical profile path (Codex canonicalizes
/// `CODEX_HOME` before keying trust).
#[must_use]
pub fn runtime_codex_home(codex_home: &Path) -> PathBuf {
    if codex_home.join(SESSION_MARKER).is_file() {
        PathBuf::from(SESSION_CODEX_HOME)
    } else {
        codex_home
            .canonicalize()
            .unwrap_or_else(|_| codex_home.to_path_buf())
    }
}

/// `(trusted, trustSignal)` for Loom's entry in `codex_home`, as Codex will
/// read it with `CODEX_HOME=runtime_home`: the keyed hashes, then the #5005
/// baseline diff over them. `wrong-location` = trust exists in the profile,
/// but none of it is for Loom's entry where it runs.
#[must_use]
pub fn trust_at(codex_home: &Path, runtime_home: &Path) -> (bool, &'static str) {
    let keys = read_hooks(codex_home)
        .map(|hooks| loom_trust_keys(&hooks, runtime_home))
        .unwrap_or_default();
    let config = codex_home.join("config.toml");
    let current = keyed_trusted_hashes(&config, &keys);
    if current.is_empty() {
        let any = !keyed_trusted_hashes(&config, &BTreeSet::new()).is_empty();
        return (false, if any { "wrong-location" } else { "none" });
    }
    match trust_baseline(&codex_home.join(RECEIPT)) {
        None => (true, "legacy-coarse"),
        Some(baseline) if current.difference(&baseline).next().is_some() => (true, "baseline-diff"),
        Some(_) => (false, "baseline-diff-no-new-trust"),
    }
}

/// The `hooks.state` keys Codex looks Loom's entry up under with
/// `CODEX_HOME=runtime_home`: one per `PreToolUse` position the entry holds.
#[must_use]
pub fn loom_trust_keys(hooks: &serde_json::Value, runtime_home: &Path) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    for (group, entry) in hooks["hooks"]["PreToolUse"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        for (handler, hook) in entry["hooks"].as_array().into_iter().flatten().enumerate() {
            if hook["command"].as_str().is_some_and(|c| c.contains(MARKER)) {
                keys.insert(format!(
                    "{}/hooks.json:pre_tool_use:{group}:{handler}",
                    runtime_home.display()
                ));
            }
        }
    }
    keys
}

/// The non-empty `trusted_hash` values `config.toml` records under `keys`
/// (every key when `keys` is empty). Parsed as TOML, so every spelling of a
/// key (Codex's own `[hooks.state."<key>"]` table, dotted keys, inline
/// tables) is the same key. A file that does not parse carries no trust: Codex
/// would not load it either.
#[must_use]
pub fn keyed_trusted_hashes(config: &Path, keys: &BTreeSet<String>) -> BTreeSet<String> {
    let Ok(text) = std::fs::read_to_string(config) else {
        return BTreeSet::new();
    };
    let Ok(document) = text.parse::<toml::Table>() else {
        return BTreeSet::new();
    };
    let Some(state) = document
        .get("hooks")
        .and_then(|hooks| hooks.get("state"))
        .and_then(toml::Value::as_table)
    else {
        return BTreeSet::new();
    };
    state
        .iter()
        .filter(|(key, _)| keys.is_empty() || keys.contains(*key))
        .filter_map(|(_, entry)| entry.get("trusted_hash")?.as_str())
        .filter(|hash| !hash.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The receipt's install-time trust baseline, or `None` when the receipt is
/// absent/unreadable or predates the field.
#[must_use]
pub fn trust_baseline(receipt: &Path) -> Option<BTreeSet<String>> {
    let bytes = std::fs::read(receipt).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let entries = value
        .get("loomManagedHook")?
        .get("trustBaselineHashes")?
        .as_array()?;
    Some(
        entries
            .iter()
            .filter_map(|entry| entry.as_str().map(str::to_owned))
            .collect(),
    )
}

fn receipt_sha(codex_home: &Path) -> Option<String> {
    let bytes = std::fs::read(codex_home.join(RECEIPT)).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value["loomManagedHook"]["commandSha256"]
        .as_str()
        .filter(|sha| !sha.is_empty())
        .map(str::to_owned)
}

/// `{}` when absent; `Err` when present but unreadable or not JSON.
fn read_hooks(codex_home: &Path) -> Result<serde_json::Value, ()> {
    let path = codex_home.join("hooks.json");
    if !path.exists() {
        return Ok(serde_json::json!({}));
    }
    let bytes = std::fs::read(&path).map_err(|_| ())?;
    serde_json::from_slice(&bytes).map_err(|_| ())
}

/// The command of Loom's entry (the first handler carrying [`MARKER`]).
#[must_use]
pub fn loom_command(hooks: &serde_json::Value) -> Option<String> {
    hooks["hooks"]["PreToolUse"]
        .as_array()?
        .iter()
        .flat_map(|group| group["hooks"].as_array().into_iter().flatten())
        .filter_map(|hook| hook["command"].as_str())
        .find(|command| command.contains(MARKER))
        .map(str::to_owned)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

fn profile_label(profile: &Path) -> String {
    profile
        .file_name()
        .map_or_else(|| profile.display().to_string(), |n| n.to_string_lossy().into_owned())
}

/// Whether `profile` backs a private-clone session: loom-daemon keeps that
/// session's identity at `<profile root>/.private-sessions/<name>/workspace.json`.
#[must_use]
pub fn is_private_clone(profile: &Path) -> bool {
    match (profile.parent(), profile.file_name()) {
        (Some(parent), Some(name)) => parent
            .join(".private-sessions")
            .join(name)
            .join("workspace.json")
            .is_file(),
        _ => false,
    }
}

/// The pooled profiles `--all-profiles` covers under `root`, sorted: every
/// immediate subdirectory except dot-directories (loom-daemon bookkeeping such
/// as `.private-sessions/`, never an account) and, for a workspace-independent
/// check, private-clone profiles (their pinned entry is proven in-container).
#[must_use]
pub fn pooled_profiles(root: &Path, registration: &Registration) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .map(|entry| entry.path())
        .filter(|path| {
            *registration != Registration::WorkspaceIndependent || !is_private_clone(path)
        })
        .collect();
    out.sort();
    out
}

#[cfg(test)]
#[path = "codex_hooks_tests.rs"]
mod tests;
