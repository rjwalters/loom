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
    /// The EE session's output, when it ran and the runner exposes it (#13502).
    pub ee_output: Option<String>,
    pub market_output: Option<String>,
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

impl Dispatched {
    /// The session output captured for `key`, if any.
    #[must_use]
    pub fn output(&self, key: Key) -> Option<&str> {
        match key {
            Key::Ee => self.ee_output.as_deref(),
            Key::Market => self.market_output.as_deref(),
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
    let mut run = |key: Key, pin: &Pin| -> (KeyOutcome, Option<String>) {
        match fetch_pinned(src, pin) {
            Err(reason) => (KeyOutcome::FailedClosed(reason), None),
            Ok(text) => {
                let public_only = key == Key::Market && is_public;
                let prompt = build_prompt(&text, pr_ref, public_only);
                let outcome = runner.invoke(role_name(key), &prompt);
                (KeyOutcome::Ran(outcome), runner.last_output())
            }
        }
    };
    let (ee, ee_output) = run(Key::Ee, ee);
    let (market, market_output) = run(Key::Market, market);
    Dispatched {
        ee,
        market,
        ee_output,
        market_output,
    }
}

/// The real forge: contents API at the pinned SHA.
pub struct GhInstructionSource<'a> {
    pub root: &'a Path,
}

/// Longest stderr excerpt carried into a failure reason, in characters.
pub const MAX_GH_DETAIL_CHARS: usize = 200;

/// Why a `gh` call produced no usable answer: the exit status and a bounded
/// stderr excerpt when it ran, or the [`crate::cmd_out::Unavailable`] reason
/// when it did not (spawn failure, timeout, ...). Whitespace is collapsed and
/// token-shaped words are redacted so a credential echoed on stderr is never
/// logged.
#[must_use]
pub fn gh_failure_detail(out: &crate::cmd_out::CmdOutcome) -> String {
    use crate::cmd_out::CmdOutcome;
    match out {
        CmdOutcome::Unavailable(u) => u.to_string(),
        CmdOutcome::Ran(o) => {
            let status = o
                .status
                .code()
                .map_or_else(|| "killed by signal".to_string(), |c| format!("exit {c}"));
            let stderr = String::from_utf8_lossy(&o.stderr);
            let words: Vec<&str> = stderr
                .split_whitespace()
                .map(|w| {
                    let t = w.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_');
                    if ["ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_"]
                        .iter()
                        .any(|p| t.starts_with(p))
                    {
                        "[redacted]"
                    } else {
                        w
                    }
                })
                .collect();
            let text = words.join(" ");
            if text.is_empty() {
                return format!("{status}, no stderr");
            }
            let bounded: String = text.chars().take(MAX_GH_DETAIL_CHARS).collect();
            let ellipsis = if text.chars().count() > MAX_GH_DETAIL_CHARS {
                "..."
            } else {
                ""
            };
            format!("{status}: {bounded}{ellipsis}")
        }
    }
}

/// A forge read through the reader credential path (#12555), as the PR list
/// and file reads in the lane already are: `Ok(stdout)` or the failure detail.
fn read_gh(op: &'static str, root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let gh = std::path::PathBuf::from(crate::gh_invocation::gh_bin());
    let out = crate::claim_reconciliation::gh_call::read(op, &gh, root)
        .args(args.iter().copied())
        .run();
    match &out {
        crate::cmd_out::CmdOutcome::Ran(o) if o.status.success() => Ok(o.stdout.clone()),
        _ => Err(gh_failure_detail(&out)),
    }
}

/// [`read_gh`] for the lane's head-content read (`ratification.raw`).
pub(super) fn read_raw(root: &Path, endpoint: &str) -> Result<String, String> {
    read_gh(
        "ratification.raw",
        root,
        &["api", "-H", "Accept: application/vnd.github.raw", endpoint],
    )
    .map(|b| String::from_utf8_lossy(&b).into_owned())
}

impl InstructionSource for GhInstructionSource<'_> {
    fn fetch(&self, pin: &Pin) -> Result<String, String> {
        let repo = &pin.repo;
        // The pin must resolve to itself: a ref that expands to another commit
        // is a mismatch, not a fetch.
        let commit = read_gh(
            "ratification.pin_commit",
            self.root,
            &[
                "api",
                &format!("repos/{repo}/commits/{}", pin.commit),
                "--jq",
                ".sha",
            ],
        )
        .map_err(|e| format!("could not resolve {repo}@{}: {e}", pin.commit))?;
        let resolved = String::from_utf8_lossy(&commit).trim().to_ascii_lowercase();
        if resolved != pin.commit.to_ascii_lowercase() {
            return Err(format!("{repo}@{} resolved to {resolved}", pin.commit));
        }
        let path = format!("repos/{repo}/contents/{}?ref={}", pin.path, pin.commit);
        read_raw(self.root, &path).map_err(|e| format!("fetch of {repo}:{} failed: {e}", pin.path))
    }

    fn repo_is_private(&self, repo: &str) -> Option<bool> {
        let out = read_gh(
            "ratification.repo_private",
            self.root,
            &["api", &format!("repos/{repo}"), "--jq", ".private"],
        )
        .ok()?;
        match String::from_utf8_lossy(&out).trim() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        }
    }
}
