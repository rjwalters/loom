//! `loom-daemon forge parent body|link` (#10012 §4): the logic behind
//! `create-issue.sh --parent N`. The script only passes the flag; see
//! [`loom_daemon::star_liveness::parent_link`].

use std::io::{Read, Write};
use std::path::PathBuf;

use anyhow::{anyhow, Result};
use clap::Subcommand;
use loom_daemon::star_liveness::parent_link::{
    child_body, fetch_parent_body, link_created, StarOutcome,
};

#[derive(Subcommand, Debug)]
pub enum ParentAction {
    /// Read a child issue body on stdin, print it with `<!-- loom:parent #N -->`
    /// (and the parent's red-main marker, when it has one) appended.
    Body {
        /// The parent issue number.
        #[arg(long)]
        parent: u32,
        /// Target `owner/repo`; omitted resolves from the origin remote.
        #[arg(long, value_name = "OWNER/REPO")]
        repo: Option<String>,
    },
    /// After the create: star the child when the parent is starred, and link
    /// it as a native sub-issue (both best effort).
    Link {
        /// The parent issue number.
        #[arg(long)]
        parent: u32,
        /// The created child: its URL, `owner/repo#N`, or number.
        #[arg(long, value_name = "URL|N")]
        child: String,
        /// Target `owner/repo`; omitted resolves from the origin remote.
        #[arg(long, value_name = "OWNER/REPO")]
        repo: Option<String>,
    },
}

fn target(repo: Option<String>) -> Result<(PathBuf, String)> {
    let root = std::env::current_dir()?;
    let slug = match repo {
        Some(r) => r,
        None => loom_daemon::worktree_ops::gh::resolve_owner_repo(&root)
            .map(|(o, n)| format!("{o}/{n}"))
            .ok_or_else(|| anyhow!("cannot resolve owner/repo; pass --repo OWNER/REPO"))?,
    };
    Ok((root, slug))
}

/// The child's issue number, refusing a reference into another repository
/// (#10012 excludes cross-repo propagation): nothing is written for it.
fn child_number(child: &str, slug: &str) -> Result<u32> {
    let (child_slug, number) = loom_daemon::forge_comment::parse_issue_ref(child)
        .ok_or_else(|| anyhow!("--child: not an issue reference: {child:?}"))?;
    if let Some(named) = child_slug {
        if !named.eq_ignore_ascii_case(slug) {
            return Err(anyhow!(
                "--child {child:?} names {named}, but the target repo is {slug}: cross-repo parent links are not supported"
            ));
        }
    }
    Ok(u32::try_from(number)?)
}

pub fn handle(action: ParentAction) -> Result<()> {
    match action {
        ParentAction::Body { parent, repo } => {
            let mut body = String::new();
            std::io::stdin().read_to_string(&mut body)?;
            let parent_body = target(repo)
                .ok()
                .and_then(|(root, slug)| fetch_parent_body(&root, &slug, parent));
            if parent_body.is_none() {
                eprintln!(
                    "loom-daemon forge parent: note: could not read #{parent}; the red-main marker (if any) was not copied"
                );
            }
            let out = child_body(&body, parent, parent_body.as_deref());
            std::io::stdout().write_all(out.as_bytes())?;
            Ok(())
        }
        ParentAction::Link {
            parent,
            child,
            repo,
        } => {
            let (root, slug) = target(repo)?;
            let number = child_number(&child, &slug)?;
            match link_created(&root, &slug, parent, number)? {
                StarOutcome::Starred => {
                    eprintln!(
                        "loom-daemon forge parent: starred #{number} (parent #{parent} is starred)"
                    );
                }
                StarOutcome::AlreadyStarred | StarOutcome::ParentNotStarred => {}
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::child_number;

    #[test]
    fn bare_and_matching_references_resolve() {
        assert_eq!(child_number("8", "o/r").unwrap(), 8);
        assert_eq!(child_number("o/r#8", "o/r").unwrap(), 8);
        assert_eq!(child_number("O/R#8", "o/r").unwrap(), 8);
        assert_eq!(child_number("https://github.com/o/r/issues/8", "o/r").unwrap(), 8);
    }

    #[test]
    fn another_repository_is_rejected() {
        assert!(child_number("other/repo#8", "o/r").is_err());
        assert!(child_number("https://github.com/other/repo/issues/8", "o/r").is_err());
    }

    #[test]
    fn a_non_reference_is_rejected() {
        assert!(child_number("nope", "o/r").is_err());
    }
}
