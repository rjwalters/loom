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
///
/// Only a trusted origin (`Env` / `Machine`, the same rule as
/// [`policy::Origin::may_run_canary`]) whose document passes
/// [`policy::assert_policy_shape`] may *actuate* `observe`, i.e. decide where
/// the minted token is sent via `api_host`. A repo-origin (committed) or
/// schema-invalid observe policy keeps the legacy token-only shape, so a
/// commit can never redirect the credential (#9986 review). `required` is
/// honoured from any origin: it only removes credentials.
#[must_use]
pub fn stance_from(sources: &PolicySources) -> Stance {
    match policy::resolve(sources) {
        Resolution::Unconfigured => Stance::Unconfigured,
        Resolution::Unreadable { .. } => Stance::Required {
            logical: "github.com".to_string(),
        },
        Resolution::Loaded(doc) => {
            let actuatable =
                doc.origin.may_run_canary() && policy::assert_policy_shape(&doc.data).is_empty();
            if policy::is_observe_only(&doc.data) {
                if !actuatable {
                    return Stance::Unconfigured;
                }
                Stance::Observe {
                    logical: logical_or_default(&doc.data),
                    api_host: expected_api_host(&doc.data),
                }
            } else {
                // The logical host is only a hosts.yml key here (no token is
                // written), but an untrusted/invalid document still gets the
                // default rather than shaping the file.
                let logical = if actuatable {
                    logical_or_default(&doc.data)
                } else {
                    "github.com".to_string()
                };
                Stance::Required { logical }
            }
        }
    }
}

/// How long a cached [`github_credential_forbidden`] verdict is trusted before
/// the repo tier (`resolve_effective_config`) is re-resolved. Edits to a policy
/// file itself are seen immediately via its mtime.
const STANCE_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(30);

type StanceKey = (Option<PathBuf>, Option<std::ffi::OsString>);
type Stamps = Vec<Option<std::time::SystemTime>>;

fn policy_stamps(sources: &PolicySources) -> Stamps {
    [&sources.env_path, &sources.machine_path, &sources.repo_path]
        .into_iter()
        .map(|p| {
            p.as_ref()
                .and_then(|p| std::fs::metadata(p).ok())
                .and_then(|m| m.modified().ok())
        })
        .collect()
}

/// `true` when Loom must hold no GitHub credential on this host: App minting,
/// reader-pool selection and `hosts.yml` tokens are all off.
///
/// Called on every reader selection and mint, so the resolution is cached per
/// workspace for [`STANCE_CACHE_TTL`], invalidated early when any policy
/// candidate's mtime changes.
#[must_use]
pub fn github_credential_forbidden(workspace: Option<&Path>) -> bool {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    use std::time::Instant;
    type Entry = (Instant, PolicySources, Stamps, bool);
    static CACHE: OnceLock<Mutex<HashMap<StanceKey, Entry>>> = OnceLock::new();

    let key: StanceKey = (workspace.map(Path::to_path_buf), std::env::var_os(policy::POLICY_ENV));
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(map) = cache.lock() {
        if let Some((at, sources, stamps, verdict)) = map.get(&key) {
            if at.elapsed() < STANCE_CACHE_TTL && policy_stamps(sources) == *stamps {
                return *verdict;
            }
        }
    }
    let sources = PolicySources::from_process(workspace);
    let stamps = policy_stamps(&sources);
    let verdict = matches!(stance_from(&sources), Stance::Required { .. });
    if let Ok(mut map) = cache.lock() {
        map.insert(key, (Instant::now(), sources, stamps, verdict));
    }
    verdict
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
///
/// Profile findings render `observed` as exactly `<dir>` or `<dir>: <detail>`
/// (`checks::assert_api_routing`, `checks::assert_no_github_token`), so this
/// matches the whole path, never a substring: the primary `.loom/gh-config`
/// is a string prefix of every `.loom/gh-config-by-owner/<owner>` (and `acme`
/// of `acme-corp`), and a sibling's finding must not roll it back.
#[must_use]
pub fn findings_for_dir<'a>(
    findings: &'a [super::report::Finding],
    config_dir: &Path,
) -> Vec<&'a super::report::Finding> {
    let shown = config_dir.display().to_string();
    let with_detail = format!("{shown}: ");
    findings
        .iter()
        .filter(|f| f.observed == shown || f.observed.starts_with(&with_detail))
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
            pool: None,
        },
        minted_gh_token: None,
    }
}

/// Publish a token-less profile into `dir` and into every other Loom-owned
/// profile of the same workspace ([`super::probe::loom_owned_profile_dirs`]:
/// the primary plus each `.loom/gh-config-by-owner/<owner>`). Otherwise a host
/// moving to `required` keeps every pre-existing per-owner `oauth_token`, which
/// both bypasses the gateway and makes `apiconfig.github-token-present` refuse
/// all dispatch until an operator hand-edits files Loom wrote itself.
/// The stance is explicit, so a policy change between the caller's read and
/// this write can never render an empty `oauth_token:` line.
pub fn publish_tokenless_everywhere(dir: &Path, logical: &str) -> std::io::Result<()> {
    let stance = Stance::Required {
        logical: logical.to_string(),
    };
    let no_assert = |_: &Path| Vec::new();
    let workspace = workspace_of_profile_dir(dir);
    publish_hosts_with_stance(dir, "", &stance, workspace.as_deref(), &no_assert)?;
    let Some(ws) = workspace else {
        return Ok(());
    };
    let mut first_err = None;
    for other in super::probe::loom_owned_profile_dirs(&ws) {
        if other == dir {
            continue;
        }
        if let Err(e) = publish_hosts_with_stance(&other, "", &stance, Some(&ws), &no_assert) {
            log::warn!(
                "credential_preflight: could not scrub the token from {} ({e}) — #9986",
                other.display()
            );
            first_err.get_or_insert(e);
        }
    }
    first_err.map_or(Ok(()), Err)
}

/// `required` host startup: publish token-less profiles
/// ([`publish_tokenless_everywhere`]) and make `dir` the process's active
/// `GH_CONFIG_DIR`, so neither the ambient `~/.config/gh` token nor an
/// exported `GH_TOKEN` is a bypass around the gateway for anything the daemon
/// spawns. Same startup-only `set_var` window as the App path in
/// `daemon_service.rs` (no task spawning `gh` exists yet). Returns whether the
/// profile is active.
pub fn activate_tokenless_profile(dir: &Path) -> bool {
    let logical = match stance_for(workspace_of_profile_dir(dir).as_deref()) {
        Stance::Required { logical } => logical,
        _ => "github.com".to_string(),
    };
    if let Err(e) = publish_tokenless_everywhere(dir, &logical) {
        log::warn!(
            "credential_preflight: could not publish every token-less profile under {} ({e}) — #9986",
            dir.display()
        );
    }
    // Activate the primary whenever it is token-less, even if a sibling
    // scrub failed: falling back to an ambient token would be worse.
    if !dir.join("hosts.yml").exists() || super::probe::profile_holds_token(dir) {
        return false;
    }
    std::env::remove_var("GH_TOKEN");
    std::env::remove_var("GITHUB_TOKEN");
    std::env::set_var("GH_CONFIG_DIR", dir);
    true
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
