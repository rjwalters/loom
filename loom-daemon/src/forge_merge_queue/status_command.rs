//! External commit-status poster for the merge-queue handoff
//! (`forge.mergeQueue.statusCommand`, #12679).
//!
//! Some consumers do not give the fleet writer App `statuses: write` and send
//! that narrow write through a broker holding its own scoped credential. When
//! the setting is present, every [`REQUIRED_CHECK_CONTEXT`] post goes to the
//! configured argv instead of `gh api .../statuses/...`, and the writer-App
//! permission preflight is replaced by a `--dry-run` probe of the same command.
//!
//! **Fail closed, no fallback.** A non-zero exit, a timeout, a spawn failure, or
//! an unusable setting is an `Err`; nothing here ever retries as the daemon's
//! own token. Unset means [`resolve`] returns `Ok(None)` and every caller keeps
//! today's behaviour.
//!
//! # Command contract
//!
//! The configured argv (no shell) is run with these flags appended:
//!
//! `--repo OWNER/REPO --sha SHA --context CTX --state success|pending|failure
//! --description TEXT --idempotency-key KEY [--pr N]`
//!
//! `--pr` is present only when the post is about one PR's head. `--dry-run`
//! replaces the posting intent with a probe: the same flags plus `--dry-run`,
//! and the command must post nothing. Exit 0 means posted (or, for the probe,
//! would post); any other exit is a refusal whose first stdout line is the reason.

use std::path::Path;
use std::time::Duration;

use sha2::{Digest, Sha256};

use super::github::safe_detail;
use crate::cmd_out::{self, CmdOutcome};

/// Dotted path of the setting in the effective config.
pub const CONFIG_KEY: &str = "forge.mergeQueue.statusCommand";

/// Bound on one invocation (post or probe).
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

/// Placeholder sha for the `--dry-run` probe, which names no real commit.
const PROBE_SHA: &str = "0000000000000000000000000000000000000000";

/// A configured status poster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusCommand {
    argv: Vec<String>,
}

/// What the posting call states.
pub struct StatusPost<'a> {
    pub repo: &'a str,
    pub sha: &'a str,
    pub context: &'a str,
    pub state: &'a str,
    pub description: &'a str,
    pub pr: Option<u32>,
}

/// Read `forge.mergeQueue.statusCommand` from `effective`.
///
/// # Errors
///
/// The setting is present but is not a non-empty array of non-empty strings.
/// The caller must treat that as "configured but unusable" (refuse), not as
/// unset, so a typo cannot silently route posts back to the writer token.
pub fn parse(effective: &serde_json::Value) -> Result<Option<StatusCommand>, String> {
    let bad =
        || format!("{CONFIG_KEY} must be a non-empty array of non-empty strings (argv, no shell)");
    match crate::config_resolver::get_path(effective, CONFIG_KEY) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Array(items)) => {
            let argv: Option<Vec<String>> = items
                .iter()
                .map(|v| v.as_str().filter(|s| !s.is_empty()).map(str::to_string))
                .collect();
            match argv {
                Some(argv) if !argv.is_empty() => Ok(Some(StatusCommand { argv })),
                _ => Err(bad()),
            }
        }
        Some(_) => Err(bad()),
    }
}

/// [`parse`] over the effective config of the checkout at `root`.
///
/// # Errors
///
/// As [`parse`].
pub fn resolve(root: &Path) -> Result<Option<StatusCommand>, String> {
    parse(&crate::config_resolver::resolve_effective_config(root))
}

/// Stable key for a post: the same `(repo, sha, context, state, description)`
/// always yields the same key, so a broker can de-duplicate a repeat.
#[must_use]
pub fn idempotency_key(p: &StatusPost<'_>) -> String {
    let mut h = Sha256::new();
    h.update(format!("{}|{}|{}|{}|{}", p.repo, p.sha, p.context, p.state, p.description));
    hex::encode(h.finalize())
}

impl StatusCommand {
    /// The flags appended to the configured argv for `p`.
    #[must_use]
    pub fn flags(p: &StatusPost<'_>) -> Vec<String> {
        let mut a = vec![
            "--repo".to_string(),
            p.repo.to_string(),
            "--sha".to_string(),
            p.sha.to_string(),
            "--context".to_string(),
            p.context.to_string(),
            "--state".to_string(),
            p.state.to_string(),
            "--description".to_string(),
            p.description.to_string(),
            "--idempotency-key".to_string(),
            idempotency_key(p),
        ];
        if let Some(pr) = p.pr {
            a.push("--pr".to_string());
            a.push(pr.to_string());
        }
        a
    }

    /// Post one status through the command.
    ///
    /// # Errors
    ///
    /// The command did not run to a zero exit; the text carries its reason.
    pub fn post(&self, root: &Path, p: &StatusPost<'_>) -> Result<(), String> {
        self.run(root, Self::flags(p))
    }

    /// Ask the command whether it would post, without posting (`--dry-run`).
    ///
    /// # Errors
    ///
    /// The probe did not run to a zero exit.
    pub fn probe(&self, root: &Path, repo: &str, context: &str) -> Result<(), String> {
        let mut flags = Self::flags(&StatusPost {
            repo,
            sha: PROBE_SHA,
            context,
            state: "pending",
            description: "dry-run probe",
            pr: None,
        });
        flags.push("--dry-run".to_string());
        self.run(root, flags)
    }

    fn run(&self, root: &Path, flags: Vec<String>) -> Result<(), String> {
        let mut args: Vec<&str> = self.argv[1..].iter().map(String::as_str).collect();
        args.extend(flags.iter().map(String::as_str));
        let out = cmd_out::run(&self.argv[0], &args, root, COMMAND_TIMEOUT);
        if out.succeeded() {
            return Ok(());
        }
        let why = match &out {
            CmdOutcome::Ran(_) => {
                let first = out.stdout_trimmed();
                let first = first.lines().next().unwrap_or("").to_string();
                let detail = if first.is_empty() {
                    out.stderr_trimmed()
                } else {
                    first
                };
                out.failure_reason(&self.argv[0])
                    + &if detail.is_empty() {
                        String::new()
                    } else {
                        format!(" — {detail}")
                    }
            }
            _ => out.failure_reason(&self.argv[0]),
        };
        Err(format!("{CONFIG_KEY}: {}", safe_detail(&why)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn post<'a>(pr: Option<u32>) -> StatusPost<'a> {
        StatusPost {
            repo: "o/r",
            sha: "a".repeat(40).leak(),
            context: "loom/merge-authorization",
            state: "success",
            description: "authorized",
            pr,
        }
    }

    #[test]
    fn unset_or_null_is_none() {
        assert_eq!(parse(&json!({})), Ok(None));
        assert_eq!(parse(&json!({"forge": {"mergeQueue": {"statusCommand": null}}})), Ok(None));
    }

    #[test]
    fn argv_array_parses_and_malformed_is_an_error_not_unset() {
        let ok = parse(&json!({"forge": {"mergeQueue": {"statusCommand": ["/bin/x", "post"]}}}));
        assert_eq!(ok.unwrap().unwrap().argv, vec!["/bin/x", "post"]);
        for bad in [json!("sh -c x"), json!([]), json!([""]), json!([1])] {
            let cfg = json!({"forge": {"mergeQueue": {"statusCommand": bad}}});
            assert!(parse(&cfg).is_err(), "{cfg}");
        }
    }

    #[test]
    fn flags_carry_pr_only_when_known_and_key_is_stable() {
        let with = StatusCommand::flags(&post(Some(7)));
        assert_eq!(&with[with.len() - 2..], ["--pr", "7"]);
        assert!(!StatusCommand::flags(&post(None)).contains(&"--pr".to_string()));
        assert_eq!(idempotency_key(&post(None)), idempotency_key(&post(Some(7))));
        let mut other = post(None);
        other.state = "failure";
        assert_ne!(idempotency_key(&post(None)), idempotency_key(&other));
    }

    fn script(body: &str) -> StatusCommand {
        StatusCommand {
            argv: vec!["sh".into(), "-c".into(), body.into(), "stub".into()],
        }
    }

    #[test]
    fn zero_exit_posts_and_nonzero_reports_first_stdout_line() {
        let root = std::env::temp_dir();
        assert!(script("exit 0").post(&root, &post(None)).is_ok());
        let e = script("echo not allowlisted; echo more; exit 3")
            .post(&root, &post(None))
            .unwrap_err();
        assert!(e.contains("not allowlisted") && !e.contains("more"), "{e}");
    }

    #[test]
    fn unspawnable_command_is_an_error() {
        let c = StatusCommand {
            argv: vec!["/nonexistent/loom-status-poster".into()],
        };
        assert!(c.post(&std::env::temp_dir(), &post(None)).is_err());
    }

    #[test]
    fn probe_passes_dry_run_and_post_does_not() {
        let root = std::env::temp_dir();
        let c = script(r#"for a in "$@"; do [ "$a" = --dry-run ] && exit 0; done; exit 9"#);
        assert!(c.probe(&root, "o/r", "ctx").is_ok());
        assert!(c.post(&root, &post(None)).is_err());
    }
}
