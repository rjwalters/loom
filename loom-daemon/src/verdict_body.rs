//! Is a verdict comment body a rationale at all? (#9258)
//!
//! A pull request merged on an approval whose whole body was the two
//! characters `@-`: an agent ran `gh pr comment --body @-` (or `gh api -f
//! body=@-`), which posts the literal string instead of reading stdin. Only
//! `gh api -F body=@-` (capital F) reads stdin. The intended body, and the
//! `loom:verdict-sha` marker it would have carried, never reached the forge.
//!
//! `post-verdict.sh` pipes the resolved body (from `--body`, `--body-file
//! PATH` or `--body-file -`) through `loom-daemon forge verdict-body-check`
//! before it posts anything. The rules live here so the script holds no
//! body-parsing logic of its own (`.loom/docs/shell-language-policy.md`).
//!
//! A body is rejected when, after trimming, it:
//!
//! 1. is empty or whitespace only;
//! 2. is exactly `-` (a `--body -` that meant "read stdin");
//! 3. is one `@`-prefixed token with no whitespace in it (`@-`, `@path`,
//!    `@~/review.md`): the literal-`@` mistake in any spelling;
//! 4. has fewer than [`MIN_NON_WS_CHARS`] non-whitespace characters.
//!
//! Prose that merely starts with an `@mention` (`@reviewer this looks good
//! because ...`) passes: it has whitespace, so it is not a lone token, and it
//! is long enough to be a sentence.

/// The fewest non-whitespace characters a verdict body may carry. Twenty is
/// about one short sentence ("Tests cover the fix."); anything shorter
/// cannot explain a verdict, and the shell artifacts this guards against
/// (`-`, `@-`, `@/tmp/x`) are all well under it.
pub const MIN_NON_WS_CHARS: usize = 20;

/// The first token of every answer, so a caller accepts only a positive
/// signal (an older binary or a crash prints something else).
pub const SENTINEL: &str = "LOOM-VERDICT-BODY";

/// Exit code for a rejected body.
pub const EXIT_REJECT: i32 = 1;

/// Why a body is not a verdict rationale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    /// Empty or whitespace only.
    Empty,
    /// Exactly `-`.
    Dash,
    /// A single `@`-prefixed token (`@-`, `@path`).
    AtToken,
    /// Fewer than [`MIN_NON_WS_CHARS`] non-whitespace characters.
    TooShort(usize),
}

impl Reject {
    /// One-line, human-readable reason.
    #[must_use]
    pub fn reason(self) -> String {
        match self {
            Self::Empty => "the body is empty or whitespace only".to_string(),
            Self::Dash => "the body is exactly '-': '--body -' posts a literal dash, it does \
                           not read stdin (use --body-file -)"
                .to_string(),
            Self::AtToken => "the body is a lone '@' token ('@-' or '@path'): '--body @path' \
                              posts the literal string and reads nothing (use --body-file \
                              <path> or --body-file -)"
                .to_string(),
            Self::TooShort(n) => format!(
                "the body has {n} non-whitespace characters, fewer than {MIN_NON_WS_CHARS}: \
                 too short to be a verdict rationale"
            ),
        }
    }
}

/// Classify `body`. `Ok(())` means it may be posted as a verdict rationale.
///
/// # Errors
///
/// Returns the first [`Reject`] class the body falls into.
pub fn check(body: &str) -> Result<(), Reject> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Err(Reject::Empty);
    }
    if trimmed == "-" {
        return Err(Reject::Dash);
    }
    if trimmed.starts_with('@') && !trimmed.chars().any(char::is_whitespace) {
        return Err(Reject::AtToken);
    }
    let n = trimmed.chars().filter(|c| !c.is_whitespace()).count();
    if n < MIN_NON_WS_CHARS {
        return Err(Reject::TooShort(n));
    }
    Ok(())
}

/// The answer line and exit code for `forge verdict-body-check`.
#[must_use]
pub fn render(result: Result<(), Reject>) -> (String, i32) {
    match result {
        Ok(()) => (format!("{SENTINEL} OK"), 0),
        Err(r) => (format!("{SENTINEL} REJECT {}", r.reason()), EXIT_REJECT),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_whitespace_bodies_are_rejected() {
        assert_eq!(check(""), Err(Reject::Empty));
        assert_eq!(check("   \n\t  "), Err(Reject::Empty));
    }

    #[test]
    fn a_lone_dash_is_rejected() {
        assert_eq!(check("-"), Err(Reject::Dash));
        assert_eq!(check("  -\n"), Err(Reject::Dash));
    }

    #[test]
    fn a_lone_at_token_is_rejected_in_every_spelling() {
        for body in [
            "@-",
            " @-\n",
            "@/tmp/x",
            "@~/review.md",
            "@./r.md",
            "@review.md",
        ] {
            assert_eq!(check(body), Err(Reject::AtToken), "{body:?}");
        }
        // A long path is still a lone token, whatever its length.
        let long = format!("@/tmp/{}", "a".repeat(60));
        assert_eq!(check(&long), Err(Reject::AtToken));
    }

    #[test]
    fn a_short_body_is_rejected_with_its_count() {
        assert_eq!(check("LGTM"), Err(Reject::TooShort(4)));
        assert_eq!(check("Approved."), Err(Reject::TooShort(9)));
        // Whitespace does not count toward the minimum.
        assert_eq!(check("a b c d e f g h i j"), Err(Reject::TooShort(10)));
    }

    #[test]
    fn a_real_rationale_passes() {
        assert_eq!(check("Tests cover the fix and CI is green."), Ok(()));
        assert_eq!(
            check("## Approved\n\nThe change is scoped to the guard.\n- tests added\n"),
            Ok(())
        );
        // Exactly the minimum passes.
        assert_eq!(check(&"x".repeat(MIN_NON_WS_CHARS)), Ok(()));
        assert_eq!(check(&"x".repeat(MIN_NON_WS_CHARS - 1)), Err(Reject::TooShort(19)));
    }

    #[test]
    fn at_mention_prose_passes() {
        assert_eq!(check("@reviewer this looks good because the tests cover it"), Ok(()));
        assert_eq!(check("@-reviewer the dash handle is fine in a sentence too"), Ok(()));
    }

    #[test]
    fn render_prints_the_sentinel_and_exit_code() {
        assert_eq!(render(Ok(())), ("LOOM-VERDICT-BODY OK".to_string(), 0));
        let (line, code) = render(check("@-"));
        assert!(line.starts_with("LOOM-VERDICT-BODY REJECT "), "{line}");
        assert!(line.contains("'@-'"), "{line}");
        assert_eq!(code, EXIT_REJECT);
        assert!(render(check("-")).0.contains("--body-file -"));
        assert!(render(check("short")).0.contains("fewer than 20"));
    }
}
