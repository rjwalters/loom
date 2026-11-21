//! `loom-daemon operator-decision resolve` (#14399): the attended path that
//! records an operator's choice of one ranked option, bound to the exact
//! decision and proposal the operator reviewed. See [`super::resolution`] for
//! the record and the verifier.
//!
//! The order is the safety property:
//!
//! 1. read the issue, parse the published decision with the existing ranked
//!    parser, and **show** the exact option, effect and scope digests;
//! 2. require an explicit confirmation (the operator types the option ID);
//! 3. **re-read** the issue and refuse if the decision or proposal changed
//!    since the review;
//! 4. record the comment (a retry of the same resolution records nothing);
//! 5. read the comments back and run the **same verifier the gate uses**; a
//!    resolution that does not verify under the forge's author metadata never
//!    releases anything;
//! 6. only then, and only with `--release`, drop the `loom:operator` hold.
//!
//! Forge access goes through [`ResolveForge`] so every ordering rule is tested
//! against a recording fake.

use super::cli::exit;
use super::resolution::{
    candidates, current, digest_option, render_comment, verify, Current, Effect, Evidence,
    Resolution, Status,
};
use super::DECISION_LABEL;
use crate::comment_trust::TrustPolicy;
use std::io::Write;

/// Not run from an attended session.
pub const UNATTENDED: i32 = 5;
/// The comment was recorded (or already stood) but does not verify as an
/// operator's resolution under the forge's author metadata; nothing was
/// released.
pub const UNVERIFIED: i32 = 6;

/// An issue as `resolve` reads it.
#[derive(Debug, Clone, Default)]
pub struct IssueView {
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
}

/// The forge operations `resolve` needs.
pub trait ResolveForge {
    /// Canonical lower-case `owner/repo`.
    fn repo(&mut self) -> Result<String, String>;
    fn view(&mut self, issue: u64) -> Result<IssueView, String>;
    /// Every comment of the issue, REST listing shape, oldest first.
    fn comments(&mut self, issue: u64) -> Result<Vec<serde_json::Value>, String>;
    fn post_comment(&mut self, issue: u64, body: &str) -> Result<(), String>;
    fn remove_label(&mut self, issue: u64, label: &str) -> Result<(), String>;
}

#[derive(Debug, Clone)]
pub struct ResolveRequest {
    pub issue: u64,
    pub option: String,
    pub effect: Effect,
    /// Drop `loom:operator` once the resolution verifies (proceed only).
    pub release: bool,
    pub dry_run: bool,
}

/// Environment markers of a dispatched (unattended) run.
const UNATTENDED_ENV: &[&str] = &[
    "LOOM_SWEEP_ID",
    "LOOM_AGENT_SESSION_PID",
    "LOOM_SWEEP_LEASE_RENEW_DISPATCHED",
];

/// `Ok` only for an attended terminal session: stdin and stderr are TTYs and
/// no dispatched-run marker is set. A recommending agent, a sweep and a pipe
/// all fail it.
///
/// # Errors
///
/// Why the session is not attended.
pub fn require_attended() -> Result<(), String> {
    use std::io::IsTerminal;
    if let Some(v) = UNATTENDED_ENV
        .iter()
        .find(|v| std::env::var_os(v).is_some_and(|x| !x.is_empty()))
    {
        return Err(format!("{v} is set: this is a dispatched run, not an attended session"));
    }
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return Err(
            "stdin/stderr is not a terminal: an operator must run this interactively".to_string()
        );
    }
    Ok(())
}

fn read_current(
    forge: &mut dyn ResolveForge,
    repo: &str,
    issue: u64,
    err: &mut dyn Write,
) -> Result<(IssueView, Current), i32> {
    let view = forge.view(issue).map_err(|e| {
        let _ = writeln!(err, "operator-decision: could not read issue #{issue}: {e}");
        exit::FORGE
    })?;
    let cur = current(repo, issue, &view.title, &view.body).map_err(|why| {
        let _ = writeln!(err, "operator-decision: REFUSED - cannot resolve #{issue}: {why}");
        exit::REFUSED
    })?;
    Ok((view, cur))
}

fn status_now(
    forge: &mut dyn ResolveForge,
    policy: &TrustPolicy,
    cur: &Current,
    err: &mut dyn Write,
) -> Result<(Status, usize), i32> {
    let items = forge.comments(cur.issue).map_err(|e| {
        let _ =
            writeln!(err, "operator-decision: could not read the comments of #{}: {e}", cur.issue);
        exit::FORGE
    })?;
    let a = verify(&Evidence {
        current: Ok(cur.clone()),
        candidates: candidates(policy, &items),
    });
    Ok((a.status, a.ignored_unauthorized))
}

/// Record the resolution. Returns the exit code.
pub fn resolve(
    forge: &mut dyn ResolveForge,
    policy: &TrustPolicy,
    req: &ResolveRequest,
    confirm: &mut dyn FnMut(&str) -> bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    if req.release && req.effect != Effect::Proceed {
        let _ = writeln!(err, "operator-decision: --release applies only to --effect proceed");
        return exit::USAGE;
    }
    let repo = match forge.repo() {
        Ok(r) => r.to_ascii_lowercase(),
        Err(e) => {
            let _ = writeln!(err, "operator-decision: could not determine the repository: {e}");
            return exit::FORGE;
        }
    };
    let n = req.issue;
    let (view, cur) = match read_current(forge, &repo, n, err) {
        Ok(x) => x,
        Err(code) => return code,
    };
    let option_id = req.option.trim();
    let Some(opt) = cur
        .decision
        .options
        .iter()
        .find(|o| o.id.trim() == option_id)
    else {
        let ids: Vec<&str> = cur.decision.options.iter().map(|o| o.id.trim()).collect();
        let _ = writeln!(
            err,
            "operator-decision: REFUSED - option `{option_id}` is not in the published decision \
             (options: {})",
            ids.join(", ")
        );
        return exit::REFUSED;
    };
    if !super::resolution::id_is_markable(option_id) {
        let _ = writeln!(
            err,
            "operator-decision: REFUSED - option id `{option_id}` has characters a resolution \
             record cannot carry (use letters, digits, . _ : -); re-publish the decision with a \
             plain id"
        );
        return exit::REFUSED;
    }
    let resolution = Resolution {
        repo: repo.clone(),
        issue: n,
        decision: cur.decision_digest.clone(),
        scope: cur.scope_digest.clone(),
        option: option_id.to_string(),
        option_digest: digest_option(opt),
        effect: req.effect,
    };

    // The review: exactly what is being approved, before anything is asked.
    let one_line = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let _ = writeln!(out, "REPO={repo}");
    let _ = writeln!(out, "ISSUE={n}");
    let _ = writeln!(out, "TITLE={}", one_line(&view.title));
    let _ = writeln!(out, "QUESTION={}", one_line(&cur.decision.question));
    let _ = writeln!(out, "OPTION={option_id}");
    let _ = writeln!(out, "OPTION_LABEL={}", one_line(&opt.label));
    let _ = writeln!(out, "OPTION_WHY={}", one_line(opt.why.as_deref().unwrap_or("")));
    let _ = writeln!(out, "EFFECT={}", req.effect.as_str());
    let _ = writeln!(out, "DECISION_DIGEST={}", cur.decision_digest);
    let _ = writeln!(out, "SCOPE_DIGEST={}", cur.scope_digest);
    let _ = writeln!(out, "RELEASE={}", req.release);
    if req.dry_run {
        let _ = writeln!(out, "DRY_RUN=true");
        return exit::OK;
    }

    if !confirm(option_id) {
        let _ = writeln!(err, "operator-decision: REFUSED - not confirmed; nothing was recorded");
        return exit::REFUSED;
    }

    // Scope may change between review and recording: re-read, compare.
    let (view, fresh) = match read_current(forge, &repo, n, err) {
        Ok(x) => x,
        Err(code) => return code,
    };
    if fresh.decision_digest != cur.decision_digest || fresh.scope_digest != cur.scope_digest {
        let _ = writeln!(
            err,
            "operator-decision: REFUSED - the decision or the proposal changed after you \
             reviewed it; nothing was recorded. Re-run to review the current text"
        );
        return exit::REFUSED;
    }

    let (prior, _) = match status_now(forge, policy, &cur, err) {
        Ok(x) => x,
        Err(code) => return code,
    };
    let already = match &prior {
        Status::Resolved(r) if r.option == resolution.option && r.effect == resolution.effect => {
            true
        }
        Status::Pending | Status::Stale(_) | Status::Unavailable(_) | Status::Unreadable(_) => {
            false
        }
        other => {
            let _ = writeln!(
                err,
                "operator-decision: REFUSED - this decision already has a standing resolution \
                 ({}): {}",
                other.token(),
                other
                    .reason()
                    .unwrap_or_else(|| "a different choice is recorded".to_string())
            );
            return exit::REFUSED;
        }
    };
    if !already {
        let body = render_comment(&resolution, &opt.label);
        if let Err(e) = forge.post_comment(n, &body) {
            let _ =
                writeln!(err, "operator-decision: recording the resolution on #{n} failed: {e}");
            return exit::FORGE;
        }
    }

    // Persist, then verify what the forge now says, before any release.
    let (after, ignored) = match status_now(forge, policy, &cur, err) {
        Ok(x) => x,
        Err(code) => return code,
    };
    match &after {
        Status::Resolved(r) if r.option == resolution.option && r.effect == resolution.effect => {}
        other => {
            let _ = writeln!(
                err,
                "operator-decision: the resolution is on #{n} but does not verify ({}): {}{}. \
                 Nothing was released",
                other.token(),
                other.reason().unwrap_or_default(),
                if ignored > 0 {
                    "; the forge does not report the posting account as an operator of this \
                     repository (OWNER/MEMBER/COLLABORATOR or fleet admin)"
                } else {
                    ""
                }
            );
            return UNVERIFIED;
        }
    }
    let _ = writeln!(out, "RECORDED={}", if already { "already" } else { "true" });
    let _ = writeln!(out, "VERIFIED=true");

    if req.release {
        let held = view.labels.iter().any(|l| l == DECISION_LABEL);
        if held {
            if let Err(e) = forge.remove_label(n, DECISION_LABEL) {
                let _ = writeln!(
                    err,
                    "operator-decision: removing {DECISION_LABEL} from #{n} failed: {e}"
                );
                return exit::FORGE;
            }
        }
        let _ = writeln!(out, "RELEASED={held}");
    }
    exit::OK
}

#[cfg(test)]
#[path = "resolve_tests.rs"]
mod tests;
