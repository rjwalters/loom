//! Whose comments the liveness pass believes.
//!
//! Every decision this module family draws from a comment (an escalation
//! marker that suppresses an ask, an intent marker whose `requested_at` sets
//! starred-at order, a merge-refusal report that names an incident) is a
//! decision an outside commenter could otherwise forge on a public repo. A
//! comment counts only when its author is:
//!
//! - a repo insider by `author_association` (`OWNER`, `MEMBER`,
//!   `COLLABORATOR`), the people who can change labels anyway;
//! - the fleet's GitHub App (`loom-fleet-dispatch`, any numbered pool member,
//!   compared with [`crate::dep_recheck::extract::normalise_login`] so the
//!   `app/` and `[bot]` spellings match), which GitHub reports as
//!   `CONTRIBUTOR`; or
//! - this daemon's own forge identity, when the forge could name it.

use super::forge::ForgeComment;

/// `author_association` values that can already write labels.
pub const TRUSTED_ASSOCIATIONS: &[&str] = &["OWNER", "MEMBER", "COLLABORATOR"];

/// Whether a comment by `login` with `association` is believed.
#[must_use]
pub fn trusted_author(
    login: Option<&str>,
    association: Option<&str>,
    self_login: Option<&str>,
) -> bool {
    if association.is_some_and(|a| {
        TRUSTED_ASSOCIATIONS
            .iter()
            .any(|t| a.eq_ignore_ascii_case(t))
    }) {
        return true;
    }
    let Some(login) = login.filter(|l| !l.trim().is_empty()) else {
        return false;
    };
    let norm = crate::dep_recheck::extract::normalise_login(login);
    if norm.starts_with(crate::dep_recheck::extract::DEFAULT_BOT_LOGIN) {
        return true;
    }
    self_login.is_some_and(|me| crate::dep_recheck::extract::normalise_login(me) == norm)
}

/// Whether `comment` is believed.
#[must_use]
pub fn trusted(comment: &ForgeComment, self_login: Option<&str>) -> bool {
    trusted_author(comment.author.as_deref(), comment.author_association.as_deref(), self_login)
}

/// The believed comments, in order.
#[must_use]
pub fn only_trusted(comments: &[ForgeComment], self_login: Option<&str>) -> Vec<ForgeComment> {
    comments
        .iter()
        .filter(|c| trusted(c, self_login))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::trusted_author;

    #[test]
    fn insiders_the_fleet_app_and_self_are_trusted_outsiders_are_not() {
        assert!(trusted_author(Some("turian"), Some("COLLABORATOR"), None));
        assert!(trusted_author(Some("x"), Some("owner"), None));
        assert!(trusted_author(Some("loom-fleet-dispatch-1[bot]"), Some("CONTRIBUTOR"), None));
        assert!(trusted_author(Some("app/loom-fleet-dispatch"), None, None));
        assert!(trusted_author(Some("robb-bot"), Some("NONE"), Some("robb-bot")));
        assert!(!trusted_author(Some("outsider"), Some("NONE"), Some("robb-bot")));
        assert!(!trusted_author(Some("drive-by"), Some("CONTRIBUTOR"), None));
        assert!(!trusted_author(None, None, None));
    }
}
