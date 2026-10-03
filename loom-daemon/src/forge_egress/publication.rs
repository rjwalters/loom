//! Routing-preserving `hosts.yml` publication (#9986, C3 of epic #9983).
//!
//! One rendering function for both publishers in `credential_preflight`
//! (the primary `.loom/gh-config` and each `.loom/gh-config-by-owner/<owner>`),
//! driven by the resolved egress policy:
//!
//! | Stance | `hosts.yml` |
//! |--------|-------------|
//! | no policy | byte-identical to the pre-#9986 token-only shape |
//! | `enforcement.api=observe` | the same, plus `api_host: <apiOrigin host>` |
//! | `enforcement.api=required` | **no** `oauth_token` at all (`git_protocol` only) |
//!
//! `required` hosts route `gh` through the managed launcher, which holds the
//! proxy key itself and never reads this profile; a published GitHub token
//! there is only a bypass for anything that is *not* the launcher. So Loom
//! publishes none, mints none and selects no reader pool ([`github_credential_forbidden`]).
//! An unreadable policy fails closed, exactly like `policy::is_observe_only`.
//!
//! A rolled-back observe publication is recorded on disk
//! ([`record_rollback`], `<workspace>/.loom/gh-config-rollback.json`) so the
//! separate `loom-daemon status` process can surface it.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::policy::{self, dig_str, expected_api_host, PolicySources, Resolution};

/// What the resolved policy says about GitHub credentials on this host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stance {
    /// No policy anywhere: nothing about publication changes.
    Unconfigured,
    /// `enforcement.api=observe`: keep the token, add `api_host`.
    Observe { logical: String, api_host: String },
    /// `enforcement.api=required` (or an unreadable policy): no credential.
    Required { logical: String },
}

/// The workspace root a Loom-owned profile dir belongs to: the parent of the
/// nearest `.loom` ancestor.
#[must_use]
pub fn workspace_of_profile_dir(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .find(|a| a.file_name().is_some_and(|n| n == ".loom"))
        .and_then(Path::parent)
        .map(Path::to_path_buf)
}

fn logical_or_default(policy: &Value) -> String {
    match dig_str(policy, &["github", "logicalHost"]) {
        "" => "github.com".to_string(),
        h => h.to_string(),
    }
}

/// The stance for `workspace` (`None` = host-level policy sources only).
#[must_use]
pub fn stance_for(workspace: Option<&Path>) -> Stance {
    stance_from(&PolicySources::from_process(workspace))
}

/// [`stance_for`] against explicit sources (tests).
#[must_use]
pub fn stance_from(sources: &PolicySources) -> Stance {
    match policy::resolve(sources) {
        Resolution::Unconfigured => Stance::Unconfigured,
        Resolution::Unreadable { .. } => Stance::Required {
            logical: "github.com".to_string(),
        },
        Resolution::Loaded(doc) => {
            let logical = logical_or_default(&doc.data);
            if policy::is_observe_only(&doc.data) {
                Stance::Observe {
                    logical,
                    api_host: expected_api_host(&doc.data),
                }
            } else {
                Stance::Required { logical }
            }
        }
    }
}

/// `true` when Loom must hold no GitHub credential on this host: App minting,
/// reader-pool selection and `hosts.yml` tokens are all off.
#[must_use]
pub fn github_credential_forbidden(workspace: Option<&Path>) -> bool {
    matches!(stance_for(workspace), Stance::Required { .. })
}

/// Render `hosts.yml`. Pure. `token` is ignored under [`Stance::Required`].
#[must_use]
pub fn render_hosts_yaml(token: &str, stance: &Stance) -> String {
    match stance {
        Stance::Unconfigured => format!(
            "github.com:\n    oauth_token: {token}\n    user: x-access-token\n    git_protocol: https\n"
        ),
        Stance::Observe { logical, api_host } if !api_host.is_empty() => format!(
            "{logical}:\n    oauth_token: {token}\n    user: x-access-token\n    git_protocol: https\n    api_host: {api_host}\n"
        ),
        Stance::Observe { logical, .. } => format!(
            "{logical}:\n    oauth_token: {token}\n    user: x-access-token\n    git_protocol: https\n"
        ),
        Stance::Required { logical } => format!("{logical}:\n    git_protocol: https\n"),
    }
}

/// Findings from `findings` that concern `config_dir` (a rollback must not
/// be triggered by an unrelated profile's problem).
#[must_use]
pub fn findings_for_dir<'a>(
    findings: &'a [super::report::Finding],
    config_dir: &Path,
) -> Vec<&'a super::report::Finding> {
    let shown = config_dir.display().to_string();
    findings
        .iter()
        .filter(|f| f.observed.contains(&shown))
        .collect()
}

/// Where a rollback is recorded for `workspace`.
#[must_use]
pub fn rollback_path(workspace: &Path) -> PathBuf {
    workspace.join(".loom").join("gh-config-rollback.json")
}

/// Record that publishing into `dir` was rolled back (codes only, no secrets).
pub fn record_rollback(workspace: &Path, dir: &Path, codes: &[&str]) {
    let doc = json!({
        "dir": dir.display().to_string(),
        "codes": codes,
        "at": chrono::Utc::now().to_rfc3339(),
    });
    let _ = std::fs::write(rollback_path(workspace), doc.to_string());
}

/// A later successful publication clears the record.
pub fn clear_rollback(workspace: &Path) {
    let _ = std::fs::remove_file(rollback_path(workspace));
}

/// The recorded rollback, if any.
#[must_use]
pub fn read_rollback(workspace: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(rollback_path(workspace)).ok()?).ok()
}

/// [`publish_github_app_token`] with the policy stance and the post-write
/// `forge egress assert` injected (#9986). Under `observe` the just-written
/// profile is asserted and, if the assert reports a finding about *this*
/// directory, the previous `hosts.yml` is restored and an error returned (the
/// rollback is recorded for `loom-daemon status`). Under `required` the token
/// is never written. With no policy this is byte-identical to the legacy
/// publication and never runs an assert.
pub fn publish_hosts_with_stance(
    config_dir: &Path,
    token: &str,
    stance: &Stance,
    workspace: Option<&Path>,
    assert: &dyn Fn(&Path) -> Vec<super::report::Finding>,
) -> std::io::Result<()> {
    std::fs::create_dir_all(config_dir)?;
    crate::credential_preflight::set_private_mode(config_dir, 0o700)?;

    let config_path = config_dir.join("config.yml");
    if !config_path.exists() {
        std::fs::write(&config_path, crate::credential_preflight::GH_CONFIG_YAML)?;
        crate::credential_preflight::set_private_mode(&config_path, 0o600)?;
    }

    let hosts_path = config_dir.join("hosts.yml");
    let previous = std::fs::read(&hosts_path).ok();
    write_hosts_atomically(config_dir, render_hosts_yaml(token, stance).as_bytes())?;

    if let (Stance::Observe { .. }, Some(workspace)) = (stance, workspace) {
        let findings = assert(workspace);
        let ours = findings_for_dir(&findings, config_dir);
        if !ours.is_empty() {
            let codes: Vec<&str> = ours.iter().map(|f| f.code).collect();
            match previous {
                Some(bytes) => write_hosts_atomically(config_dir, &bytes)?,
                None => {
                    let _ = std::fs::remove_file(&hosts_path);
                }
            }
            record_rollback(workspace, config_dir, &codes);
            return Err(std::io::Error::other(format!(
                "forge egress assert reported {} after publishing {}; previous profile restored — #9986",
                codes.join(", "),
                hosts_path.display()
            )));
        }
        clear_rollback(workspace);
    }
    Ok(())
}

/// Write `bytes` to `<config_dir>/hosts.yml` via temp + rename, mode 0600.
fn write_hosts_atomically(config_dir: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp_path = config_dir.join("hosts.yml.tmp");
    std::fs::write(&tmp_path, bytes)?;
    crate::credential_preflight::set_private_mode(&tmp_path, 0o600)?;
    std::fs::rename(&tmp_path, config_dir.join("hosts.yml"))
}

/// The preflight result on a `required` egress host (#9986): the gateway owns
/// the GitHub credential, so Loom neither mints nor probes `gh auth status`
/// (which would call api.github.com) and holds no token.
#[must_use]
pub fn gateway_owned_preflight() -> crate::credential_preflight::GithubAppPreflight {
    crate::credential_preflight::GithubAppPreflight {
        report: crate::types::CredentialPreflightReport {
            ok: true,
            mechanism: "gateway".to_string(),
            fingerprint: None,
            message: "forge egress policy enforcement.api=required: the egress gateway owns the \
                      GitHub credential; Loom holds none (#9986)"
                .to_string(),
            checked_at: chrono::Utc::now(),
        },
        minted_gh_token: None,
    }
}

/// `required` host startup: publish a token-less profile into `dir` and make
/// it the process's active `GH_CONFIG_DIR`, so neither the ambient
/// `~/.config/gh` token nor an exported `GH_TOKEN` is a bypass around the
/// gateway for anything the daemon spawns. Same startup-only `set_var` window
/// as the App path in `daemon_service.rs` (no task spawning `gh` exists yet).
/// Returns whether the profile is active.
pub fn activate_tokenless_profile(dir: &Path) -> bool {
    match crate::credential_preflight::publish_github_app_token(dir, "") {
        Ok(()) => {
            std::env::remove_var("GH_TOKEN");
            std::env::remove_var("GITHUB_TOKEN");
            std::env::set_var("GH_CONFIG_DIR", dir);
            true
        }
        Err(e) => {
            log::warn!(
                "credential_preflight: could not publish the token-less profile to {} ({e}) — #9986",
                dir.display()
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observe() -> Stance {
        Stance::Observe {
            logical: "github.com".into(),
            api_host: "github-proxy.2amlogic.com".into(),
        }
    }

    #[test]
    fn golden_no_policy_is_byte_identical_to_the_legacy_shape() {
        assert_eq!(
            render_hosts_yaml("ghs_x", &Stance::Unconfigured),
            "github.com:\n    oauth_token: ghs_x\n    user: x-access-token\n    git_protocol: https\n"
        );
    }

    #[test]
    fn golden_observe_adds_api_host_and_keeps_token() {
        assert_eq!(
            render_hosts_yaml("ghs_x", &observe()),
            "github.com:\n    oauth_token: ghs_x\n    user: x-access-token\n    git_protocol: https\n    api_host: github-proxy.2amlogic.com\n"
        );
    }

    #[test]
    fn golden_required_has_no_token() {
        let y = render_hosts_yaml(
            "ghs_secret",
            &Stance::Required {
                logical: "github.com".into(),
            },
        );
        assert_eq!(y, "github.com:\n    git_protocol: https\n");
        assert!(!y.contains("ghs_secret") && !y.contains("oauth_token"));
    }

    #[test]
    fn observe_rendering_round_trips_through_the_validators_reader() {
        let y = render_hosts_yaml("t", &observe());
        assert_eq!(
            super::super::probe::parse_api_host(&y, "github.com"),
            super::super::checks::ApiHost::Present("github-proxy.2amlogic.com".into())
        );
    }

    #[test]
    fn workspace_is_the_parent_of_the_loom_dir() {
        let w = workspace_of_profile_dir(Path::new("/w/.loom/gh-config-by-owner/o")).unwrap();
        assert_eq!(w, Path::new("/w"));
        assert!(workspace_of_profile_dir(Path::new("/tmp/gh-config")).is_none());
    }
}
