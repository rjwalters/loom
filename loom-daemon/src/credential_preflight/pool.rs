//! Which GitHub rate-limit pool the daemon's writer credential spends (#9872).
//!
//! GitHub meters every personal access token (classic and fine-grained), OAuth
//! token, `gh` keyring login and App *user* token of one account against that
//! account's **single** per-user pool. A separate pool comes only from a
//! GitHub App **installation** token, a different account, or Actions'
//! `GITHUB_TOKEN`. So a host whose daemon runs on a personal credential shares
//! one budget with every shell, GUI app and agent of that user — and looked,
//! until the quota ran out, exactly like an App host. [`attach`] makes that
//! visible: `credential_preflight.pool` in `loom-daemon status --json`, and a
//! startup WARN for `kind = user`.
//!
//! Never carries or logs a token value: only the kind, the login and the
//! numeric account id.

use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::forge_call_stats::ForgeOp;
use crate::gh_invocation::{AccessIntent, GhCompletion, GhInvocation, GhTarget, Operation};
use crate::proc_exec::Completion;
use crate::types::{CredentialPool, CredentialPreflightReport};

/// `kind` of a GitHub App installation token: its own pool.
pub const KIND_INSTALLATION: &str = "installation";
/// `kind` of a personal credential: the account's one shared pool.
pub const KIND_USER: &str = "user";

/// Bound on the one `gh api user` lookup.
const USER_LOOKUP_TIMEOUT: Duration = Duration::from_secs(10);

impl CredentialPreflightReport {
    /// A report with no pool classification yet ([`attach`] adds it).
    pub(crate) fn new(
        ok: bool,
        mechanism: &str,
        fingerprint: Option<String>,
        message: String,
        checked_at: DateTime<Utc>,
    ) -> Self {
        Self {
            ok,
            mechanism: mechanism.to_string(),
            fingerprint,
            message,
            checked_at,
            pool: None,
        }
    }
}

/// What `gh api user` answered: `Ok((login, id))`, or the (non-secret)
/// stderr of a refusal.
pub type UserLookup = Result<(String, Option<u64>), String>;

/// Classify the pool `report`'s credential spends. `env_token` is the value of
/// the env var the report's mechanism names (`GH_TOKEN` / `GITHUB_TOKEN`),
/// used only for its `ghs_` (installation token) prefix; `lookup_user` is
/// called at most once, and only for a credential not already known to be an
/// installation token.
pub fn classify(
    report: &CredentialPreflightReport,
    env_token: Option<&str>,
    lookup_user: impl FnOnce() -> UserLookup,
) -> Option<CredentialPool> {
    let installation = || CredentialPool {
        kind: KIND_INSTALLATION.to_string(),
        login: None,
        user_id: None,
    };
    // No credential, or the egress gateway holds it (#9986): nothing to say.
    if !report.ok || matches!(report.mechanism.as_str(), "none" | "unknown" | "gateway") {
        return None;
    }
    if report.mechanism == "github-app" || env_token.is_some_and(|t| t.starts_with("ghs_")) {
        return Some(installation());
    }
    match lookup_user() {
        Ok((login, user_id)) => Some(CredentialPool {
            kind: KIND_USER.to_string(),
            login: crate::forge_call_stats::sanitize(&login),
            user_id,
        }),
        // An installation token cannot read `/user`.
        Err(e)
            if e.to_ascii_lowercase()
                .contains("not accessible by integration") =>
        {
            Some(installation())
        }
        Err(_) => Some(CredentialPool {
            kind: KIND_USER.to_string(),
            login: None,
            user_id: None,
        }),
    }
}

/// The startup WARN for a personal pool, or `None` for an installation.
#[must_use]
pub fn warning(pool: &CredentialPool) -> Option<String> {
    if pool.kind != KIND_USER {
        return None;
    }
    let who = match (&pool.login, pool.user_id) {
        (Some(l), Some(id)) => format!("account {l} (id {id})"),
        (Some(l), None) => format!("account {l}"),
        _ => "a personal account (login could not be resolved)".to_string(),
    };
    Some(format!(
        "credential_preflight: the daemon's GitHub credential belongs to {who}, so it spends \
         that user's ONE rate-limit pool, shared with every personal access token, OAuth token \
         and `gh` login of the same user (shells, GUI apps, agents). Another PAT of the same \
         user does not split it; a GitHub App installation does — set forge.githubApp (writes) \
         and forge.identities.readers (reads). See github-authentication.md, \"Rate-limit \
         pools: what actually splits the bucket\" — #9872"
    ))
}

/// One `gh api user` through the facade, pinned to the writer (the answer is
/// "who am I", which a reader App would answer for itself).
fn gh_api_user(program: Option<&str>) -> UserLookup {
    let mut inv = GhInvocation::new(
        Operation::new("credential_preflight.user"),
        AccessIntent::Read,
        GhTarget::None,
        USER_LOOKUP_TIMEOUT,
    )
    .forge_op(ForgeOp::uninventoried(
        "one startup identity lookup for the pool report (#9872)",
    ))
    .writer_identity()
    .args(["api", "user"]);
    if let Some(p) = program {
        inv = inv.program(p);
    }
    match inv.execute() {
        Ok(GhCompletion::Captured(Completion::Exited(out))) if out.status.success() => {
            let v: serde_json::Value =
                serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())?;
            let login = v
                .get("login")
                .and_then(serde_json::Value::as_str)
                .ok_or("no login in `gh api user`")?;
            Ok((login.to_string(), v.get("id").and_then(serde_json::Value::as_u64)))
        }
        Ok(GhCompletion::Captured(Completion::Exited(out))) => {
            Err(String::from_utf8_lossy(&out.stderr).into_owned())
        }
        Ok(_) => Err("`gh api user` timed out".to_string()),
        Err(e) => Err(e.to_string()),
    }
}

/// Classify `report`'s pool, record it on the report and WARN for a personal
/// pool. `program` injects a `gh` stub in tests (`None` = the resolver).
#[must_use]
pub fn attach_with(
    mut report: CredentialPreflightReport,
    program: Option<&str>,
) -> CredentialPreflightReport {
    let env_token = match report.mechanism.as_str() {
        m @ ("GH_TOKEN" | "GITHUB_TOKEN") => std::env::var(m).ok(),
        _ => None,
    };
    report.pool = classify(&report, env_token.as_deref(), || gh_api_user(program));
    if let Some(msg) = report.pool.as_ref().and_then(warning) {
        log::warn!("{msg}");
    }
    report
}

/// [`attach_with`] on the production `gh`.
#[must_use]
pub fn attach(report: CredentialPreflightReport) -> CredentialPreflightReport {
    attach_with(report, None)
}

#[cfg(test)]
#[path = "pool_tests.rs"]
mod tests;
