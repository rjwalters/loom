//! `loom-daemon forge comment <N> (--body TEXT | --body-file PATH)` — the
//! shell/agent-facing reach into [`loom_daemon::forge_comment`], the daemon's
//! single comment chokepoint (#9772).
//!
//! ADR-0018: the behavior (footer format, idempotence, the one POST endpoint)
//! lives in the core module; this file only parses arguments and maps the
//! core's `Result` onto an exit code. A shell twin (`post-comment.sh`) execs
//! this rather than reimplementing the footer.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use loom_daemon::forge_comment;

/// Handle `forge comment`. Returns `Err` only when the body cannot be read or
/// the forge call fails; the footer is never optional.
pub(crate) fn run(
    number: u32,
    body: Option<String>,
    body_file: Option<PathBuf>,
    repo: Option<String>,
    is_pr: bool,
) -> Result<()> {
    let text = match (body, body_file) {
        (Some(b), _) => b,
        (None, Some(p)) => std::fs::read_to_string(&p)
            .with_context(|| format!("could not read --body-file {}", p.display()))?,
        (None, None) => anyhow::bail!("one of --body or --body-file is required"),
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let nwo = resolve_repo(repo.as_deref(), &cwd);
    let gh = std::env::var("LOOM_GH_BIN").unwrap_or_else(|_| "gh".to_string());
    let out = forge_comment::post_in(&gh, Some(&cwd), &nwo, number, is_pr, &text)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if !out.is_empty() {
        println!("{out}");
    }
    Ok(())
}

/// `--repo` when given, else the cwd's `origin` remote (the convention the
/// other `forge` verbs use). An empty result is not fatal: `gh api` still
/// resolves `{owner}/{repo}` itself, the footer is simply the one thing that
/// cannot be built without a slug.
fn resolve_repo(repo: Option<&str>, cwd: &Path) -> String {
    repo.map(str::trim)
        .filter(|r| !r.is_empty())
        .map(str::to_string)
        .or_else(|| forge_comment::resolve_nwo(cwd))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_repo_flag_wins_over_the_remote() {
        assert_eq!(resolve_repo(Some("o/r"), Path::new("/nonexistent")), "o/r".to_string());
    }

    #[test]
    fn a_blank_repo_flag_is_treated_as_absent() {
        // No git remote under a nonexistent path either, so this lands on the
        // empty fallback rather than posting to `repos/ /issues/...`.
        let prior = std::env::var("LOOM_REPO").ok();
        std::env::remove_var("LOOM_REPO");
        let got = resolve_repo(Some("   "), Path::new("/nonexistent"));
        if let Some(v) = prior {
            std::env::set_var("LOOM_REPO", v);
        }
        assert_eq!(got, String::new());
    }

    #[test]
    fn neither_body_nor_body_file_is_a_usage_error() {
        let err = run(1, None, None, Some("o/r".into()), false).unwrap_err();
        assert!(err.to_string().contains("--body"), "{err}");
    }

    #[test]
    fn an_unreadable_body_file_names_the_path() {
        let err =
            run(1, None, Some(PathBuf::from("/nonexistent/body.md")), Some("o/r".into()), false)
                .unwrap_err();
        assert!(err.to_string().contains("/nonexistent/body.md"), "{err}");
    }
}
