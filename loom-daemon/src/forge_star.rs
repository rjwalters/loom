//! `loom-daemon forge star <number> --direction "<operator's words>" [--unstar]`
//! — the one way an agent or bot applies or removes `loom:operator-priority`
//! on the operator's explicit direction (#9974 star-authority policy).
//!
//! Policy: the operator stars directly, via loom-ui, or by directing an agent
//! ("star #123", "file this as operator priority"). An agent never stars or
//! unstars on its own judgment. `--direction` is therefore required and
//! non-blank: it names the operator direction being executed, and it is
//! quoted in the audit comment.
//!
//! The audit comment reuses the loom-ui intent shape
//! ([`intents::marker`]), so `requested_at` becomes the item's starred-at on
//! every host through [`intents::starred_at_from_timeline`] — an
//! agent-applied star is ordered exactly like a loom-ui one.
//!
//! Idempotent: starring an already-starred item (or unstarring an unstarred
//! one) changes nothing and posts nothing, so a repeat can never move an
//! existing star's time later. Exit `0` = done (or already so), `1` = failed,
//! `3` = Gitea decline.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use crate::forge_cmd::{detect_forge, ForgeType, EX_FORGE_DECLINED};
use crate::star_liveness::forge::{GhStarForge, StarForge};
use crate::star_liveness::intents::{self, Action, ValidIntent};
use crate::work_finder::OPERATOR_PRIORITY_LABEL;
use crate::worktree_ops::gh::resolve_owner_repo;

/// The operator direction as quoted in the audit comment: one line, no HTML
/// or markdown-active characters, at most 200 chars. `None` when blank.
#[must_use]
pub fn sanitize_direction(raw: &str) -> Option<String> {
    let s: String = raw
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .filter(|c| !matches!(c, '<' | '>' | '`' | '"'))
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let s: String = s.chars().take(200).collect();
    (!s.is_empty()).then_some(s)
}

/// The audit comment for an agent-applied star change: the loom-ui intent
/// marker, then who executed it and the operator direction it executed.
#[must_use]
pub fn audit_comment(intent: &ValidIntent, direction: &str) -> String {
    let verb = match intent.action {
        Action::Star => "⭐ Starred for operator priority",
        Action::Unstar => "Unstarred (operator priority removed)",
    };
    format!(
        "{}\n{verb} by `{}` on operator direction: \"{direction}\".",
        intents::marker(intent),
        intent.requested_by
    )
}

/// What [`apply`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The label changed and the audit comment was posted.
    Changed,
    /// The item was already in the wanted state; nothing was written.
    AlreadySo,
}

/// Apply `intent` through `forge`: change the label, then post the audit
/// comment. Nothing is written when the label is already as wanted.
///
/// # Errors
/// The item does not exist, or a forge read/write failed.
pub fn apply(forge: &mut dyn StarForge, intent: &ValidIntent, direction: &str) -> Result<Outcome> {
    let Some(item) = forge.issue(intent.number)? else {
        bail!("{}#{} does not exist", intent.repo, intent.number);
    };
    let has = item.labels.iter().any(|l| l == OPERATOR_PRIORITY_LABEL);
    match intent.action {
        Action::Star if !has => forge.add_label(intent.number, OPERATOR_PRIORITY_LABEL)?,
        Action::Unstar if has => forge.remove_label(intent.number, OPERATOR_PRIORITY_LABEL)?,
        _ => return Ok(Outcome::AlreadySo),
    }
    forge.post_comment(intent.number, &audit_comment(intent, direction))?;
    Ok(Outcome::Changed)
}

/// The executing agent's name for the audit comment: `--by`, else
/// `$LOOM_ROLE`, else `agent`; reduced to `[A-Za-z0-9._-]`, 64 chars.
fn actor(by: Option<&str>) -> String {
    let raw = by
        .map(str::to_string)
        .or_else(|| std::env::var("LOOM_ROLE").ok())
        .unwrap_or_default();
    let s: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        .take(64)
        .collect();
    if s.is_empty() {
        "agent".into()
    } else {
        s
    }
}

/// Handle `loom-daemon forge star`. Exits the process.
pub fn handle(number: u32, direction: &str, unstar: bool, by: Option<&str>) -> Result<()> {
    let Some(direction) = sanitize_direction(direction) else {
        bail!(
            "--direction must quote the operator's direction; an agent never stars or \
             unstars on its own judgment (#9974)"
        );
    };
    let root = std::env::current_dir().context("could not resolve the current directory")?;
    if detect_forge(Some(&root)) == ForgeType::Gitea {
        eprintln!(
            "loom-daemon forge star: GitHub-only; apply the label and audit comment by hand."
        );
        std::process::exit(EX_FORGE_DECLINED);
    }
    let (owner, repo) =
        resolve_owner_repo(&root).context("could not resolve owner/repo from the git remotes")?;
    let now = chrono::Utc::now();
    let action = if unstar { Action::Unstar } else { Action::Star };
    let intent = ValidIntent {
        id: format!("agent-{number}-{}", now.timestamp()),
        repo: format!("{owner}/{repo}"),
        root: PathBuf::from(&root),
        number,
        action,
        requested_at: Some(now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        requested_by: actor(by),
    };
    let mut forge = GhStarForge::new(&root, &intent.repo);
    let verb = if unstar { "unstarred" } else { "starred" };
    match apply(&mut forge, &intent, &direction)? {
        Outcome::Changed => println!("#{number} {verb}; audit comment posted"),
        Outcome::AlreadySo => println!("#{number} already {verb}; nothing written"),
    }
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge_listing::RestIssue;
    use crate::star_liveness::forge::{ForgeComment, SearchHit};

    #[derive(Default)]
    struct Fake {
        labels: Vec<String>,
        posted: Vec<String>,
        writes: usize,
    }

    impl StarForge for Fake {
        fn list_open(&mut self, _: &str) -> Result<Vec<RestIssue>> {
            Ok(vec![])
        }
        fn issue(&mut self, number: u32) -> Result<Option<RestIssue>> {
            Ok(Some(RestIssue {
                number,
                title: None,
                labels: self.labels.clone(),
                created_at: None,
                updated_at: None,
                closed_at: None,
                state: "open".into(),
                body: None,
                author: None,
                is_pull_request: false,
            }))
        }
        fn comments(&mut self, _: u32) -> Result<Vec<ForgeComment>> {
            Ok(vec![])
        }
        fn search_open_issues(&mut self, _: &str) -> Result<Vec<SearchHit>> {
            Ok(vec![])
        }
        fn add_label(&mut self, _: u32, l: &str) -> Result<()> {
            self.writes += 1;
            self.labels.push(l.into());
            Ok(())
        }
        fn remove_label(&mut self, _: u32, l: &str) -> Result<()> {
            self.writes += 1;
            self.labels.retain(|x| x != l);
            Ok(())
        }
        fn post_comment(&mut self, _: u32, body: &str) -> Result<()> {
            self.posted.push(body.into());
            Ok(())
        }
    }

    fn intent(action: Action) -> ValidIntent {
        ValidIntent {
            id: "agent-7-1790000000".into(),
            repo: "o/r".into(),
            root: PathBuf::from("/tmp"),
            number: 7,
            action,
            requested_at: Some("2026-10-02T23:00:00Z".into()),
            requested_by: "builder".into(),
        }
    }

    #[test]
    fn star_labels_then_posts_an_intent_shaped_audit_comment() {
        let mut f = Fake::default();
        let out = apply(&mut f, &intent(Action::Star), "star #7").unwrap();
        assert_eq!(out, Outcome::Changed);
        assert_eq!(f.labels, vec![OPERATOR_PRIORITY_LABEL.to_string()]);
        let c = &f.posted[0];
        assert!(c.starts_with(
            "<!-- loom:operator-priority-intent=agent-7-1790000000 action=star requested_at=2026-10-02T23:00:00Z -->"
        ));
        assert!(c.contains("by `builder` on operator direction: \"star #7\"."));
        // The timeline reader takes the comment's requested_at as starred-at.
        let line = "C 2026-10-02T23:00:05Z 2026-10-02T23:00:00Z OWNER robb";
        let at = intents::starred_at_from_timeline(&format!("L 2026-10-02T23:00:01Z\n{line}"));
        assert_eq!(at.as_deref(), Some("2026-10-02T23:00:00Z"));
    }

    #[test]
    fn a_repeat_writes_nothing_so_an_existing_star_time_never_moves() {
        let mut f = Fake {
            labels: vec![OPERATOR_PRIORITY_LABEL.into()],
            ..Fake::default()
        };
        assert_eq!(apply(&mut f, &intent(Action::Star), "x").unwrap(), Outcome::AlreadySo);
        assert_eq!((f.writes, f.posted.len()), (0, 0));
        assert_eq!(apply(&mut f, &intent(Action::Unstar), "unstar #7").unwrap(), Outcome::Changed);
        assert!(f.labels.is_empty());
        assert!(f.posted[0].contains("action=unstar"));
    }

    #[test]
    fn direction_is_required_and_cannot_inject_markup() {
        assert_eq!(sanitize_direction("  \n\t "), None);
        assert_eq!(
            sanitize_direction("star <!-- x --> `#5`\n\"now\"").as_deref(),
            Some("star !-- x -- #5 now")
        );
        assert_eq!(sanitize_direction(&"a".repeat(300)).map(|s| s.len()), Some(200));
    }
}
