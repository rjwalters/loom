//! Populate a pooled provider profile from the operator's default one
//! (issue #8672).
//!
//! # The gap this closes
//!
//! `loom-daemon accounts add codex <name>` creates `<profile root>/<name>`,
//! runs `codex login` into it, and stops. But the CLI reads *everything* from
//! that directory — `AGENTS.md`, `config.toml`, `prompts/`, MCP servers, the
//! managed `pre_tool_use` hook bridge — so the rotated account is a **blank
//! install**: different trust config, no repo instructions, no guard bridge.
//! That is why a pooled non-default account behaves unlike the operator's own.
//!
//! This module populates it, idempotently, per
//! [`super::profile_sharing::ProviderProfileRules`] — symlink the capability
//! and session trees, copy the plain files, key-merge the settings documents
//! under a credential/identity denylist, and never touch credentials or
//! per-project trust state. [`super::profile_ledger`] records what was
//! written so a later run overwrites only while the profile still holds
//! exactly that; an operator's own edit inside a pooled profile wins forever.
//!
//! # Deliberate non-goals
//!
//! - **The credential is never read.** `auth.json` is on the provider's
//!   `never_share` list, is absent from its sharing table (asserted by
//!   [`super::profile_sharing::ProviderProfileRules::validate`]), and
//!   [`ensure_shareable`] re-checks every path immediately before it is
//!   opened. Provisioning a profile never opens, copies, parses, or logs it.
//! - **Hook trust is not established here.** The managed bridge is installed
//!   per profile by the provider's own bridge script; Codex hook *trust*
//!   stays a per-profile, operator-attested one-time step, and Loom never
//!   passes `--dangerously-bypass-hook-trust` (see
//!   `defaults/docs/guardrail-parity-codex.md`).
//! - **Session-managed profiles are skipped.** Once `accounts session start`
//!   adopts a profile, its container is the sole process allowed to touch
//!   that `CODEX_HOME` (issue #6925, ADR-0017 Decision 1). Host-side
//!   provisioning would race it.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Serialize;

use super::account_registry::{account_inventory_quiet, AccountProvider};
use super::profile_ledger::{sha256_hex, ProfileLedger, LEDGER_FILE};
use super::profile_merge::{merge_json, merge_toml, MergeOutcome};
use super::profile_sharing::{ProviderProfileRules, ShareMode, SurfaceRule};
use super::session_lifecycle::is_session_managed;

/// Kill switch for the daemon-start provisioning pass. Any value other than
/// `0`/`false` leaves it enabled.
pub const PROVISION_ON_START_ENV: &str = "LOOM_PROFILE_PROVISION_ON_START";

#[derive(Debug, Clone)]
pub struct ProvisionOptions {
    /// The default (operator) profile to provision *from*.
    pub source: PathBuf,
    /// Workspace passed to the managed hook bridge, so the guard it invokes
    /// resolves the right project root.
    pub workspace: PathBuf,
    /// Plan only: report what would change and write nothing.
    pub dry_run: bool,
    /// Skip the managed hook bridge (the surfaces are provisioned either way).
    pub skip_hook_bridge: bool,
}

/// What one surface did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SurfaceOutcome {
    pub path: String,
    pub mode: ShareMode,
    /// `linked` | `copied` | `merged` | `unchanged` | `preserved` | `skipped`
    pub action: &'static str,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HookBridgeOutcome {
    pub ran: bool,
    pub ok: bool,
    pub exit_code: Option<i32>,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProvisionReport {
    pub schema_version: u32,
    pub provider: String,
    /// The profile **directory name** only — never a path, so a report is
    /// safe to print and to ship in `--json`.
    pub profile: String,
    pub source: String,
    /// `true` when this run wrote anything at all. A second run over an
    /// unchanged pair reports `false`.
    pub changed: bool,
    pub surfaces: Vec<SurfaceOutcome>,
    /// Keys refused by the provider's denylist, `<file>:<dotted key>`.
    pub denied_keys: Vec<String>,
    pub hook_bridge: Option<HookBridgeOutcome>,
}

/// Resolve a provider's **default** profile directory.
///
/// Deliberately does *not* consult the provider's own home variable
/// (`CODEX_HOME`): a daemon child already has that pointed at whichever
/// pooled profile it was dispatched with, so trusting it would let one pooled
/// account become the provisioning source for another. The explicit
/// `LOOM_*_DEFAULT_HOME` override exists for an operator whose profile is not
/// at the conventional location.
///
/// Under `cfg(test)` the `$HOME` fallback is refused outright, for the reason
/// [`super::paths::shared_tokens_dir`] refuses its own (#4657): a test that
/// fell through to the real `~/.codex` would read the operator's live profile.
#[must_use]
pub fn default_profile_home(rules: &ProviderProfileRules) -> Option<PathBuf> {
    match std::env::var(rules.default_home_env) {
        Ok(value) if value.trim().is_empty() => None,
        Ok(value) => Some(super::paths::expand_tilde(value.trim())),
        #[cfg(test)]
        Err(_) => None,
        #[cfg(not(test))]
        Err(_) => dirs::home_dir().map(|home| home.join(rules.default_home_relative)),
    }
}

/// `true` unless the daemon-start pass has been switched off.
#[must_use]
pub fn provision_on_start_enabled() -> bool {
    !matches!(
        std::env::var(PROVISION_ON_START_ENV)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "0" | "false" | "no" | "off"
    )
}

/// Provision every pooled profile registered for `rules.provider` in
/// `workspace`'s account registry.
///
/// Session-managed profiles are skipped (see the module docs). A failure on
/// one profile does not abort the rest: each profile's error is returned in
/// its own `Err`, alongside the successful reports.
pub fn provision_all(
    rules: &ProviderProfileRules,
    workspace: &Path,
    options: &ProvisionOptions,
) -> Vec<(String, Result<ProvisionReport>)> {
    let provider = match registry_provider(rules) {
        Ok(provider) => provider,
        Err(error) => return vec![(rules.provider.to_string(), Err(error))],
    };
    let accounts = match account_inventory_quiet(workspace, provider) {
        Ok(accounts) => accounts,
        Err(error) => return vec![(String::from("*"), Err(error))],
    };
    accounts
        .into_iter()
        .map(|account| {
            let name = account.id.name.clone();
            let report = provision_profile(rules, &account.credential_reference, options);
            (name, report)
        })
        .collect()
}

/// The account-registry provider a sharing table's profiles are registered
/// under. Only providers Loom actually tracks accounts for can be enumerated
/// or resolved by name; any other table is still provisionable by path.
fn registry_provider(rules: &ProviderProfileRules) -> Result<AccountProvider> {
    match rules.provider {
        "codex" => Ok(AccountProvider::Codex),
        other => bail!(
            "provider {other:?} has no account registry on this host; provision its profiles by \
             path instead"
        ),
    }
}

/// Resolve a registered account's profile directory.
pub fn profile_dir_for(
    rules: &ProviderProfileRules,
    workspace: &Path,
    name: &str,
) -> Result<PathBuf> {
    let provider = registry_provider(rules)?;
    account_inventory_quiet(workspace, provider)?
        .into_iter()
        .find(|account| account.id.name == name)
        .map(|account| account.credential_reference)
        .ok_or_else(|| anyhow::anyhow!("{} account {name:?} does not exist", rules.provider))
}

/// Provision one profile directory.
pub fn provision_profile(
    rules: &ProviderProfileRules,
    profile: &Path,
    options: &ProvisionOptions,
) -> Result<ProvisionReport> {
    rules.validate()?;
    let source = &options.source;
    if !source.is_dir() {
        bail!(
            "the default {} profile {} does not exist; nothing to provision from",
            rules.provider,
            source.display()
        );
    }
    if !profile.is_dir() {
        bail!("profile directory {} does not exist", profile.display());
    }
    reject_self_provisioning(source, profile)?;
    if is_session_managed(profile) {
        return Ok(ProvisionReport {
            schema_version: 1,
            provider: rules.provider.to_string(),
            profile: profile_label(profile),
            source: source.display().to_string(),
            changed: false,
            surfaces: vec![SurfaceOutcome {
                path: String::from("*"),
                mode: ShareMode::Copy,
                action: "skipped",
                detail: String::from(
                    "profile is session-managed; its container owns this directory (#6925)",
                ),
            }],
            denied_keys: Vec::new(),
            hook_bridge: None,
        });
    }

    let mut ledger = ProfileLedger::load(profile, rules.provider, source);
    // A ledger carried over from a *different* source profile describes
    // content this run cannot vouch for. Start fresh rather than treat the
    // other source's fingerprints as Loom's ownership record here.
    if ledger.source != source.display().to_string() {
        ledger = ProfileLedger::new(rules.provider, source);
    }

    let mut report = ProvisionReport {
        schema_version: 1,
        provider: rules.provider.to_string(),
        profile: profile_label(profile),
        source: source.display().to_string(),
        changed: false,
        surfaces: Vec::new(),
        denied_keys: Vec::new(),
        hook_bridge: None,
    };

    for surface in rules.surfaces {
        let outcome = apply_surface(rules, surface, profile, options, &mut ledger, &mut report)
            .unwrap_or_else(|error| SurfaceOutcome {
                path: surface.path.to_string(),
                mode: surface.mode,
                action: "skipped",
                detail: format!("{error:#}"),
            });
        if matches!(outcome.action, "linked" | "copied" | "merged") {
            report.changed = true;
        }
        report.surfaces.push(outcome);
    }

    if !options.dry_run && ledger.save_if_changed(profile)? {
        report.changed = true;
    }

    if !options.skip_hook_bridge && !options.dry_run {
        report.hook_bridge = install_hook_bridge(rules, profile, &options.workspace);
    }

    Ok(report)
}

/// Refuse a source/target pair where one contains the other, or where they are
/// the same directory. Both would make provisioning read its own output — and
/// the nesting case is exactly what a stale ambient `CODEX_HOME` would produce
/// if it were ever trusted as a source.
fn reject_self_provisioning(source: &Path, profile: &Path) -> Result<()> {
    let source_key = source
        .canonicalize()
        .unwrap_or_else(|_| source.to_path_buf());
    let profile_key = profile
        .canonicalize()
        .unwrap_or_else(|_| profile.to_path_buf());
    if source_key == profile_key {
        bail!("refusing to provision {} from itself", profile_key.display());
    }
    if source_key.starts_with(&profile_key) || profile_key.starts_with(&source_key) {
        bail!(
            "refusing to provision {} from {}: one contains the other",
            profile_key.display(),
            source_key.display()
        );
    }
    Ok(())
}

/// Last-resort assertion before any open: the relative path must not name a
/// never-shared file, and must not escape the profile root.
fn ensure_shareable(rules: &ProviderProfileRules, relative: &Path) -> Result<()> {
    if rules.is_never_shared(relative) {
        bail!("{} is never shared between {} profiles", relative.display(), rules.provider);
    }
    if relative
        .components()
        .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        bail!("{} is not a plain relative path", relative.display());
    }
    Ok(())
}

fn apply_surface(
    rules: &ProviderProfileRules,
    surface: &SurfaceRule,
    profile: &Path,
    options: &ProvisionOptions,
    ledger: &mut ProfileLedger,
    report: &mut ProvisionReport,
) -> Result<SurfaceOutcome> {
    let relative = Path::new(surface.path);
    ensure_shareable(rules, relative)?;
    let source = options.source.join(relative);
    let target = profile.join(relative);
    let skipped = |detail: String| SurfaceOutcome {
        path: surface.path.to_string(),
        mode: surface.mode,
        action: "skipped",
        detail,
    };

    if !source.exists() {
        return Ok(skipped(String::from("absent from the default profile — nothing to share")));
    }

    match surface.mode {
        ShareMode::Symlink => link_surface(surface, &source, &target, options, ledger),
        ShareMode::Copy => copy_surface(surface, &source, &target, options, ledger),
        ShareMode::MergeJson | ShareMode::MergeToml => {
            merge_surface(rules, surface, &source, &target, options, ledger, report)
        }
    }
}

fn link_surface(
    surface: &SurfaceRule,
    source: &Path,
    target: &Path,
    options: &ProvisionOptions,
    ledger: &mut ProfileLedger,
) -> Result<SurfaceOutcome> {
    let want = source.to_path_buf();
    let outcome = |action: &'static str, detail: String| SurfaceOutcome {
        path: surface.path.to_string(),
        mode: surface.mode,
        action,
        detail,
    };

    match std::fs::symlink_metadata(target) {
        Ok(meta) if meta.file_type().is_symlink() => {
            let current = std::fs::read_link(target).unwrap_or_default();
            if current == want {
                ledger
                    .links
                    .insert(surface.path.to_string(), want.display().to_string());
                return Ok(outcome("unchanged", String::from("already linked")));
            }
            let loom_owned = ledger.links.get(surface.path).map(String::as_str)
                == Some(current.display().to_string().as_str());
            if !loom_owned {
                return Ok(outcome(
                    "preserved",
                    format!("points at {} — not Loom's link", current.display()),
                ));
            }
            if options.dry_run {
                return Ok(outcome("linked", String::from("would repoint (dry run)")));
            }
            std::fs::remove_file(target)?;
        }
        Ok(_) => {
            return Ok(outcome("preserved", String::from("exists as real content in this profile")))
        }
        Err(_) => {}
    }

    if options.dry_run {
        return Ok(outcome("linked", String::from("would link (dry run)")));
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    symlink_dir(&want, target).with_context(|| format!("failed to link {}", target.display()))?;
    ledger
        .links
        .insert(surface.path.to_string(), want.display().to_string());
    Ok(outcome("linked", format!("-> {}", want.display())))
}

#[cfg(unix)]
fn symlink_dir(source: &Path, target: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(source, target)
}

#[cfg(not(unix))]
fn symlink_dir(_source: &Path, _target: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "symlinked profile surfaces are only supported on unix hosts",
    ))
}

fn copy_surface(
    surface: &SurfaceRule,
    source: &Path,
    target: &Path,
    options: &ProvisionOptions,
    ledger: &mut ProfileLedger,
) -> Result<SurfaceOutcome> {
    let outcome = |action: &'static str, detail: String| SurfaceOutcome {
        path: surface.path.to_string(),
        mode: surface.mode,
        action,
        detail,
    };
    if !source.is_file() {
        return Ok(outcome(
            "skipped",
            String::from("the default profile's entry is not a regular file"),
        ));
    }
    let bytes =
        std::fs::read(source).with_context(|| format!("failed to read {}", source.display()))?;
    let want = sha256_hex(&bytes);

    match std::fs::symlink_metadata(target) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Ok(outcome("preserved", String::from("this profile has its own symlink here")))
        }
        Ok(_) => {
            let current = sha256_hex(&std::fs::read(target)?);
            if ledger.copied.get(surface.path).map(String::as_str) != Some(current.as_str()) {
                return Ok(outcome(
                    "preserved",
                    String::from("edited in this profile since Loom last wrote it"),
                ));
            }
            if current == want {
                return Ok(outcome("unchanged", String::from("already identical")));
            }
        }
        Err(_) => {}
    }

    if options.dry_run {
        return Ok(outcome("copied", String::from("would copy (dry run)")));
    }
    super::profile_ledger::write_private(target, &bytes)
        .with_context(|| format!("failed to write {}", target.display()))?;
    ledger.copied.insert(surface.path.to_string(), want);
    Ok(outcome("copied", format!("{} bytes", bytes.len())))
}

#[allow(clippy::too_many_arguments)]
fn merge_surface(
    rules: &ProviderProfileRules,
    surface: &SurfaceRule,
    source: &Path,
    target: &Path,
    options: &ProvisionOptions,
    ledger: &mut ProfileLedger,
    report: &mut ProvisionReport,
) -> Result<SurfaceOutcome> {
    let outcome = |action: &'static str, detail: String| SurfaceOutcome {
        path: surface.path.to_string(),
        mode: surface.mode,
        action,
        detail,
    };
    if !source.is_file() {
        return Ok(outcome(
            "skipped",
            String::from("the default profile's entry is not a regular file"),
        ));
    }
    if std::fs::symlink_metadata(target).is_ok_and(|m| m.file_type().is_symlink()) {
        return Ok(outcome("preserved", String::from("this profile has its own symlink here")));
    }
    let source_text = std::fs::read_to_string(source)
        .with_context(|| format!("failed to read {}", source.display()))?;
    let target_text = std::fs::read_to_string(target).unwrap_or_default();

    let merged: MergeOutcome = match surface.mode {
        ShareMode::MergeToml => {
            merge_toml(rules, surface.path, &source_text, &target_text, ledger)?
        }
        _ => merge_json(rules, surface.path, &source_text, &target_text, ledger)?,
    };

    for key in &merged.denied {
        report.denied_keys.push(format!("{}:{key}", surface.path));
    }

    let detail = format!(
        "{} written, {} unchanged, {} preserved, {} denied",
        merged.written.len(),
        merged.unchanged.len(),
        merged.preserved.len(),
        merged.denied.len()
    );

    if options.dry_run {
        let action = if merged.changed() {
            "merged"
        } else {
            "unchanged"
        };
        return Ok(outcome(action, format!("{detail} (dry run)")));
    }

    if let Some(rendered) = &merged.rendered {
        super::profile_ledger::write_private(target, rendered.as_bytes())
            .with_context(|| format!("failed to write {}", target.display()))?;
    }
    for (dotted, canonical) in merged.written.iter().chain(merged.unchanged.iter()) {
        ledger.record_merged(surface.path, dotted, canonical.clone());
    }
    Ok(outcome(
        if merged.changed() {
            "merged"
        } else {
            "unchanged"
        },
        detail,
    ))
}

/// Run the provider's managed hook-bridge installer against `profile`.
///
/// Best effort: a host without the script (or without `jq`) still gets a
/// provisioned profile, and the outcome says so. **The argv is built here and
/// nowhere else, and it never carries a hook-trust bypass** — Codex hook trust
/// stays a per-profile operator-attested step (#4495/#5005).
fn install_hook_bridge(
    rules: &ProviderProfileRules,
    profile: &Path,
    workspace: &Path,
) -> Option<HookBridgeOutcome> {
    let spec = rules.hook_bridge?;
    let Some(script) = resolve_hook_bridge(&spec, workspace) else {
        return Some(HookBridgeOutcome {
            ran: false,
            ok: false,
            exit_code: None,
            detail: format!("no managed hook-bridge provisioner found (set {})", spec.script_env),
        });
    };
    let output = std::process::Command::new("bash")
        .arg(&script)
        .arg(spec.install_arg)
        .arg(spec.home_flag)
        .arg(profile)
        .arg(spec.workspace_flag)
        .arg(workspace)
        .output();
    match output {
        Ok(result) => {
            let code = result.status.code();
            Some(HookBridgeOutcome {
                ran: true,
                ok: result.status.success(),
                exit_code: code,
                detail: if result.status.success() {
                    String::from("managed pre_tool_use bridge installed")
                } else {
                    // stderr can name paths but never credentials (the bridge
                    // script prints only the profile directory name).
                    format!(
                        "bridge installer exited {}",
                        code.map_or_else(|| String::from("by signal"), |c| c.to_string())
                    )
                },
            })
        }
        Err(error) => Some(HookBridgeOutcome {
            ran: false,
            ok: false,
            exit_code: None,
            detail: format!("could not run {}: {error}", script.display()),
        }),
    }
}

fn resolve_hook_bridge(
    spec: &super::profile_sharing::HookBridgeSpec,
    workspace: &Path,
) -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var(spec.script_env) {
        let trimmed = explicit.trim();
        if trimmed.is_empty() {
            return None;
        }
        let path = super::paths::expand_tilde(trimmed);
        return path.is_file().then_some(path);
    }
    spec.script_candidates
        .iter()
        .map(|candidate| workspace.join(candidate))
        .find(|path| path.is_file())
}

/// The directory name of a profile — the only identity this module ever
/// prints, matching `provision-codex-hooks.sh`'s own secret-free convention.
fn profile_label(profile: &Path) -> String {
    profile
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| profile.display().to_string())
}

/// Best-effort provisioning invoked from the account lifecycle right after a
/// profile is created (`accounts add` / `accounts import`).
///
/// Never fails the account: a freshly created, correctly-registered account
/// with an unpopulated profile is strictly better than no account at all, and
/// `accounts provision <name>` can fill it in later. Returns a one-line
/// advisory when something was worth saying.
#[must_use]
pub fn provision_new_profile_quietly(
    rules: &ProviderProfileRules,
    profile: &Path,
    workspace: &Path,
) -> Option<String> {
    let source = default_profile_home(rules)?;
    if !source.is_dir() {
        return None;
    }
    let options = ProvisionOptions {
        source,
        workspace: workspace.to_path_buf(),
        dry_run: false,
        skip_hook_bridge: false,
    };
    match provision_profile(rules, profile, &options) {
        Ok(report) if report.changed => Some(format!(
            "Provisioned {} profile {:?} from {} ({} surface(s), ledger {LEDGER_FILE}).",
            rules.provider,
            report.profile,
            report.source,
            report
                .surfaces
                .iter()
                .filter(|s| matches!(s.action, "linked" | "copied" | "merged"))
                .count()
        )),
        Ok(_) => None,
        Err(error) => Some(format!(
            "Profile provisioning did not complete for {:?}: {error:#}. The account is registered; \
             run `loom-daemon accounts provision {}` once the cause is fixed.",
            profile_label(profile),
            profile_label(profile)
        )),
    }
}

#[cfg(test)]
#[path = "profile_provisioning_tests.rs"]
mod tests;
