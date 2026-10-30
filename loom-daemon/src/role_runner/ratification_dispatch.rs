//! The ratification reviewer lane's dispatch layer (#13189, step 2 of 3 of
//! #13186).
//!
//! Launches the two independent key reviews (EE and market) for one candidate
//! PR. Verdict posting is step 3; candidate selection is
//! [`super::ratification_lane`]. The forge is injected ([`InstructionSource`])
//! so every rule is unit-testable offline.
//!
//! # Independence
//!
//! Each session's prompt is built only from (a) that key's pinned instructions
//! and (b) the PR reference. Nothing else crosses: no shared file, env var or
//! prior-output argument, and a session never sees the other's outcome.
//!
//! # Fail closed
//!
//! A pin whose `commit` is not a full 40-hex SHA, any fetch failure (error,
//! empty body, commit that does not resolve to the pinned SHA) yields
//! [`KeyOutcome::FailedClosed`] for that key only; the session is never
//! launched with partial or default instructions. An unreadable repo
//! visibility is treated as public.

use super::ratification_lane::{Key, Pin};
use super::{RoleInvocationRunner, RoleTickOutcome};
use std::path::Path;

/// Appended to the market prompt when the target repo is public.
pub const PUBLIC_SOURCES_ONLY: &str = "This repository is public. Use public sources only: do not \
use, cite or disclose any non-public information.";

/// The forge, injected.
pub trait InstructionSource {
    /// The file at `pin.path` in `pin.repo` at exactly `pin.commit`.
    fn fetch(&self, pin: &Pin) -> Result<String, String>;
    /// Whether `repo` is private; `None` when it could not be read.
    fn repo_is_private(&self, repo: &str) -> Option<bool>;
}

/// The result for one key.
#[derive(Debug, Clone, PartialEq)]
pub enum KeyOutcome {
    /// The session ran; step 3 reads the verdict from it.
    Ran(RoleTickOutcome),
    /// The session was not launched; the reason is recorded.
    FailedClosed(String),
}

/// Per-key results for one PR.
#[derive(Debug, Clone, PartialEq)]
pub struct Dispatched {
    pub ee: KeyOutcome,
    pub market: KeyOutcome,
}

impl Dispatched {
    #[must_use]
    pub fn get(&self, key: Key) -> &KeyOutcome {
        match key {
            Key::Ee => &self.ee,
            Key::Market => &self.market,
        }
    }
}

/// The role name a key's session is invoked as.
#[must_use]
pub fn role_name(key: Key) -> &'static str {
    match key {
        Key::Ee => "ratification-ee",
        Key::Market => "ratification-market",
    }
}

fn is_full_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn fetch_pinned(src: &dyn InstructionSource, pin: &Pin) -> Result<String, String> {
    if !is_full_sha(&pin.commit) {
        return Err(format!("pin commit {:?} is not a full 40-hex SHA", pin.commit));
    }
    let body = src.fetch(pin)?;
    if body.trim().is_empty() {
        return Err("instruction file is empty".to_string());
    }
    Ok(body)
}

fn build_prompt(instructions: &str, pr_ref: &str, public_only: bool) -> String {
    let mut p = format!("{}\n\nPR: {pr_ref}\n", instructions.trim_end());
    if public_only {
        p.push('\n');
        p.push_str(PUBLIC_SOURCES_ONLY);
        p.push('\n');
    }
    p
}

/// Dispatch the EE and market sessions for one candidate PR.
///
/// `target_repo` is the repo the PR lives in (its visibility, not the
/// instruction repo's, decides the market constraint); `pr_ref` is the only
/// text the two prompts share.
pub fn dispatch(
    ee: &Pin,
    market: &Pin,
    target_repo: &str,
    pr_ref: &str,
    src: &dyn InstructionSource,
    runner: &mut dyn RoleInvocationRunner,
) -> Dispatched {
    let is_public = src.repo_is_private(target_repo) != Some(true);
    let mut run = |key: Key, pin: &Pin| -> KeyOutcome {
        match fetch_pinned(src, pin) {
            Err(reason) => KeyOutcome::FailedClosed(reason),
            Ok(text) => {
                let public_only = key == Key::Market && is_public;
                let prompt = build_prompt(&text, pr_ref, public_only);
                KeyOutcome::Ran(runner.invoke(role_name(key), &prompt))
            }
        }
    };
    Dispatched {
        ee: run(Key::Ee, ee),
        market: run(Key::Market, market),
    }
}

/// The real forge: contents API at the pinned SHA.
pub struct GhInstructionSource<'a> {
    pub root: &'a Path,
}

impl InstructionSource for GhInstructionSource<'_> {
    fn fetch(&self, pin: &Pin) -> Result<String, String> {
        let repo = &pin.repo;
        // The pin must resolve to itself: a ref that expands to another commit
        // is a mismatch, not a fetch.
        let commit = crate::script_helpers::run_gh(
            &[
                "api",
                &format!("repos/{repo}/commits/{}", pin.commit),
                "--jq",
                ".sha",
            ],
            self.root,
            false,
        );
        let resolved = commit
            .ok_output()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .trim()
                    .to_ascii_lowercase()
            })
            .ok_or_else(|| format!("could not resolve {repo}@{}", pin.commit))?;
        if resolved != pin.commit.to_ascii_lowercase() {
            return Err(format!("{repo}@{} resolved to {resolved}", pin.commit));
        }
        let path = format!("repos/{repo}/contents/{}?ref={}", pin.path, pin.commit);
        let out = crate::script_helpers::run_gh(
            &["api", "-H", "Accept: application/vnd.github.raw", &path],
            self.root,
            false,
        );
        out.ok_output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .ok_or_else(|| format!("fetch of {repo}:{} failed", pin.path))
    }

    fn repo_is_private(&self, repo: &str) -> Option<bool> {
        let out = crate::script_helpers::run_gh(
            &["api", &format!("repos/{repo}"), "--jq", ".private"],
            self.root,
            false,
        );
        match String::from_utf8_lossy(&out.ok_output()?.stdout).trim() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        }
    }
}
