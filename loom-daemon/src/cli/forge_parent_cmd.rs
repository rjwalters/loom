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
            let number = loom_daemon::forge_comment::parse_issue_ref(&child)
                .map(|(_, n)| n)
                .ok_or_else(|| anyhow!("--child: not an issue reference: {child:?}"))?;
            let number = u32::try_from(number)?;
            let (root, slug) = target(repo)?;
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
