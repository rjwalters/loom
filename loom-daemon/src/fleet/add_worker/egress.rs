//! Forge-egress policy awareness for `fleet add-worker` (#10050, slice of
//! #9986 / #9983 C3).
//!
//! On a host governed by a forge-egress policy with `enforcement.api =
//! required`, provisioning must (1) never wire git through the `gh` credential
//! helper (`gh auth setup-git`), choosing the git protocol from
//! `github.gitOrigin.rollout`, and (2) install the pinned upstream `gh` at
//! `toolchain.upstreamGhPath`, verify its checksum, and finish with
//! `forge egress doctor`. Hosts with no policy (or `observe`) are unchanged.
//!
//! Everything here is pure except policy loading, which fails closed on an
//! unreadable policy (only an absent one means "no policy"); values interpolated
//! into shell are validated against a conservative charset first.

use anyhow::{bail, Result};
use serde_json::Value;

use super::path_bootstrap;
use crate::fleet::{Plan, Step, StepStdin};
use crate::forge_egress::policy::{dig, dig_str, resolve, PolicySources, Resolution};

/// The slice of a `required` policy that provisioning acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressProvisioning {
    /// `true` when `github.gitOrigin.rollout` is `qualified` or `enforced`.
    pub git_ssh: bool,
    pub gh_version: Option<String>,
    pub upstream_path: Option<String>,
    pub upstream_sha256: Option<String>,
}

fn safe_path(s: &str) -> bool {
    s.starts_with('/')
        && !s.contains("..")
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-'))
}

fn safe_version(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

fn safe_sha(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
}

impl EgressProvisioning {
    /// `Some` only for a policy with `enforcement.api == required`.
    #[must_use]
    pub fn from_policy(policy: &Value) -> Option<Self> {
        if dig_str(policy, &["enforcement", "api"]) != "required" {
            return None;
        }
        let rollout = dig_str(policy, &["github", "gitOrigin", "rollout"]);
        let some = |keys: &[&str], ok: fn(&str) -> bool| {
            dig(policy, keys)
                .and_then(Value::as_str)
                .filter(|s| ok(s))
                .map(str::to_string)
        };
        Some(Self {
            git_ssh: matches!(rollout, "qualified" | "enforced"),
            gh_version: some(&["toolchain", "ghPinnedVersion"], safe_version),
            upstream_path: some(&["toolchain", "upstreamGhPath"], safe_path),
            upstream_sha256: some(&["toolchain", "upstreamGhSha256"], safe_sha),
        })
    }
}

/// Resolve the policy from `sources`. The org policy on the operator host is
/// assumed to be the one the worker will be governed by.
///
/// Only an *absent* policy (`Unconfigured`) means "no policy". A present but
/// unreadable/unparseable policy fails closed (policy.rs contract: never a
/// fall-through to a narrower policy), so provisioning refuses rather than
/// silently emitting the legacy `gh auth setup-git` plan.
pub fn load_for_operator(sources: &PolicySources) -> Result<Option<EgressProvisioning>> {
    match resolve(sources) {
        Resolution::Unconfigured => Ok(None),
        Resolution::Loaded(doc) => Ok(EgressProvisioning::from_policy(&doc.data)),
        Resolution::Unreadable {
            candidate, error, ..
        } => bail!(
            "forge-egress policy {} is unreadable ({error}); refusing to provision a worker \
             without knowing whether it is policy-governed. Fix or remove the policy file \
             (or unset LOOM_FORGE_EGRESS_POLICY) and re-run.",
            candidate.path.display()
        ),
    }
}

/// [`load_for_operator`] with the production sources (env > machine).
pub fn load_for_operator_from_process() -> Result<Option<EgressProvisioning>> {
    load_for_operator(&PolicySources::from_process(None))
}

/// Step 4, `forge-auth`. Legacy hosts log `gh` in with the PAT (over stdin)
/// and wire git through `gh auth setup-git`. A policy-governed host stores no
/// GitHub credential at all (#9986: the gateway owns the API credential, and a
/// PAT in `~/.config/gh/hosts.yml` would fail the trailing doctor), so the PAT
/// is neither consumed nor persisted and only `git_protocol` is set.
pub fn push_forge_auth(plan: &mut Plan, pat: Option<&String>, p: Option<&EgressProvisioning>) {
    match (p, pat) {
        (Some(p), _) => {
            let proto = git_protocol(p);
            plan.push_step(Step::new(
                "forge-auth",
                &format!("set gh git_protocol={proto} (policy-governed: no gh credential stored)"),
                Some(format!(
                    "[ \"$(gh config get git_protocol 2>/dev/null)\" = {proto} ]"
                )),
                render_forge_auth(p),
            ));
        }
        (None, Some(pat)) => plan.push_step(
            Step::new(
                "forge-auth",
                "authenticate gh with the fine-grained PAT (via stdin) and set up git credential helper",
                Some("gh auth status >/dev/null 2>&1".to_string()),
                render_forge_auth_legacy(),
            )
            .with_stdin(StepStdin {
                content: pat.clone(),
                secret: true,
            }),
        ),
        (None, None) => plan.push_skip(
            "forge-auth",
            "authenticate gh with the fine-grained PAT",
            "no --pat-file supplied",
        ),
    }
}

fn git_protocol(p: &EgressProvisioning) -> &'static str {
    if p.git_ssh {
        "ssh"
    } else {
        "https"
    }
}

fn render_forge_auth_legacy() -> String {
    // The PAT arrives on stdin; pipe it straight into `gh auth login` so it
    // never lands on a command line. `gh` stores it 0600 under ~/.config/gh.
    let export_line = path_bootstrap::canonical_path_export_line();
    format!(
        r#"set -e
{export_line}gh auth login --with-token
gh auth setup-git
"#
    )
}

/// `forge-auth` script for a governed host: no `gh auth login` (the host
/// holds no GitHub credential) and no `gh auth setup-git`.
#[must_use]
pub fn render_forge_auth(p: &EgressProvisioning) -> String {
    let export_line = path_bootstrap::canonical_path_export_line();
    let proto = git_protocol(p);
    format!(
        r#"set -e
{export_line}# forge-egress policy (enforcement.api=required): this host stores no GitHub
# credential (the gateway owns it), so no gh login; and git must never use the
# gh credential helper, so no gh setup-git either.
gh config set git_protocol {proto}
"#
    )
}

/// Pinned-gh install steps (before `workspace-clone`).
pub fn push_toolchain_steps(plan: &mut Plan, p: &EgressProvisioning) {
    let (Some(version), Some(path)) = (&p.gh_version, &p.upstream_path) else {
        plan.push_skip(
            "pinned-gh",
            "install the pinned upstream gh",
            "policy toolchain.ghPinnedVersion/upstreamGhPath missing or malformed",
        );
        return;
    };
    let sha_check = match &p.upstream_sha256 {
        Some(sha) => format!(r#" && [ "$(sha256sum '{path}' | cut -d' ' -f1)" = '{sha}' ]"#),
        None => String::new(),
    };
    let check =
        format!("'{path}' --version 2>/dev/null | grep -q 'gh version {version} '{sha_check}");
    let verify_sha = match &p.upstream_sha256 {
        Some(sha) => format!(
            r#"echo '{sha}  '"$TMP/gh_{version}_linux_$ARCH/bin/gh" | sha256sum -c - >/dev/null || {{ echo "pinned gh sha256 mismatch" >&2; exit 1; }}"#
        ),
        None => "echo 'toolchain.upstreamGhSha256 is null: skipping checksum verification' >&2"
            .to_string(),
    };
    let apply = format!(
        r#"set -e
case "$(uname -m)" in x86_64) ARCH=amd64 ;; aarch64|arm64) ARCH=arm64 ;; *) echo "unsupported arch" >&2; exit 1 ;; esac
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
curl -fsSL -o "$TMP/gh.tgz" "https://github.com/cli/cli/releases/download/v{version}/gh_{version}_linux_$ARCH.tar.gz"
tar -xzf "$TMP/gh.tgz" -C "$TMP"
{verify_sha}
SUDO=""; [ "$(id -u)" = 0 ] || SUDO="sudo -n"
$SUDO install -D -m 0755 "$TMP/gh_{version}_linux_$ARCH/bin/gh" '{path}'
"#
    );
    plan.push_step(Step::new(
        "pinned-gh",
        &format!("install upstream gh {version} at {path} and verify its sha256"),
        Some(check),
        apply,
    ));
}

/// Trailing step: `forge egress doctor` from the primary workspace.
pub fn push_doctor_step(plan: &mut Plan, primary_rel: &str) {
    let export_line = path_bootstrap::canonical_path_export_line();
    plan.push_step(Step::new(
        "forge-egress-doctor",
        "loom-daemon forge egress doctor (policy-governed host)",
        None,
        format!(
            "set -e\n{export_line}cd \"$HOME/{primary_rel}\"\nloom-daemon forge egress doctor\n"
        ),
    ));
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "egress_tests.rs"]
mod egress_tests;
