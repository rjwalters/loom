//! Ref-operand validation for forge-derived branch names (#9106).
//!
//! # The defect this exists to close
//!
//! Git's own ref-name validator **accepts a leading-dash name**:
//! `git update-ref refs/heads/--upload-pack=/tmp/x` succeeds, the push
//! succeeds, and a forge will happily host a PR whose `headRefName` is exactly
//! that. Only `git check-ref-format --branch` (a *convenience* check nothing in
//! this codebase ran) rejects it.
//!
//! Handed to git as a bare **operand**, such a name is re-parsed as a
//! **switch**:
//!
//! ```text
//! git fetch origin '--upload-pack=/tmp/payload' main   # path/file:// origin:
//!                                                      #   EXECUTES /tmp/payload
//! git fetch origin '--depth=1'                         # silently shallow-ifies
//!                                                      #   the local clone
//! git rebase <upstream> '--strategy=evil'              # exec merge-evil from
//!                                                      #   GIT_EXEC_PATH
//! ```
//!
//! An argument vector (`Command::args`, as [`crate::reconcile_stack`] uses)
//! blocks *shell* injection but **not** git-option injection — a `--*` name is
//! still a switch to the program receiving it. So the arg vector is not the
//! mitigation; this predicate is, and the `--` end-of-options separator at each
//! call site is defence in depth behind it.
//!
//! # Parity with the shell half
//!
//! The same predicate lives in `defaults/scripts/lib/default-branch.sh` as
//! `check_branch_name`. Both halves are stated once, in the same order, and
//! both are exercised by the same table of cases (here in [`tests`], and in
//! `defaults/scripts/tests/test-check-branch-name.sh`) so they cannot drift:
//!
//! 1. non-empty
//! 2. matches `^[A-Za-z0-9][A-Za-z0-9._/-]*$` — an **allowlist**, which is what
//!    rejects a leading `-` (the switch form), a `=` anywhere (the
//!    option-argument form), and every shell/glob metacharacter, whitespace
//!    byte and control byte at once
//! 3. no trailing `.` or `/`
//! 4. no empty (`//`), dot-leading (`/.`) or `..` path segment
//!
//! Steps 1/3/4 are rules `git check-ref-format` also enforces; step 2 is
//! deliberately **stricter** than git, because the failure mode here is code
//! execution rather than a malformed ref. Every legitimate Loom branch name —
//! `main`, `feature/issue-42`, `hotfix/x.y`, `a-b_c.d` — passes.

/// Why a name was refused. The `Display` text names the offending ref, so a
/// refusal is greppable in a sweep log and quotable in a forge comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefnameError {
    /// The rejected name, verbatim.
    pub name: String,
    /// The specific rule it broke, in the message's own wording.
    pub why: &'static str,
}

impl std::fmt::Display for RefnameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "'{}' is not a safe git ref operand: {}. A forge-controlled ref name git can \
             parse as a switch is an option-injection vector (#9106): on a path/file:// \
             origin 'git fetch origin --upload-pack=/tmp/x' EXECUTES /tmp/x, and '--depth=1' \
             silently shallow-ifies the clone. Refusing fail-closed rather than running git \
             on it.",
            self.name, self.why
        )
    }
}

impl std::error::Error for RefnameError {}

/// The one predicate. `Ok(())` means `name` is safe to place in a git argv as a
/// ref operand; `Err` names the rule it broke.
///
/// Call this **before** any forge-derived ref reaches a [`std::process::Command`]
/// argument — never after, and never as a "best effort" whose failure is logged
/// and ignored. The whole value of the check is that it fails closed.
///
/// ```
/// use loom_daemon::refname::check_refname;
/// assert!(check_refname("feature/issue-42").is_ok());
/// assert!(check_refname("--upload-pack=/tmp/x").is_err());
/// ```
pub fn check_refname(name: &str) -> Result<(), RefnameError> {
    let reject = |why: &'static str| {
        Err(RefnameError {
            name: name.to_string(),
            why,
        })
    };

    if name.is_empty() {
        return reject("it is empty");
    }
    if name.starts_with('-') {
        return reject(
            "it starts with '-', which git parses as a command-line SWITCH rather than a ref",
        );
    }
    if name.contains('=') {
        return reject("it contains '=', the option-argument form (e.g. --upload-pack=/tmp/x)");
    }

    // ^[A-Za-z0-9][A-Za-z0-9._/-]*$ — spelled out rather than compiled, so this
    // module carries no regex dependency and the character set is auditable by
    // reading it.
    let mut chars = name.chars();
    let first = chars.next().expect("non-empty checked above");
    if !first.is_ascii_alphanumeric() {
        return reject("its first character is not ASCII alphanumeric");
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-')) {
        return reject("it does not match ^[A-Za-z0-9][A-Za-z0-9._/-]*$");
    }

    if name.ends_with('.') || name.ends_with('/') {
        return reject("it ends in '.' or '/'");
    }
    if name.contains("//") || name.contains("/.") || name.contains("..") {
        return reject("it has an empty, dot-leading, or '..' path segment");
    }

    Ok(())
}

/// Validate several refs at once, returning the first refusal.
///
/// Convenience for a call site that must clear an entire request before
/// touching anything — see [`crate::reconcile_stack::plan`], which checks the
/// remote, default branch, child branch and parent branch as one gate so no
/// git process starts on a request that carries any unsafe name.
pub fn check_all(names: &[&str]) -> Result<(), RefnameError> {
    for name in names {
        check_refname(name)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Acceptance criterion 1 (#9106), rejected half. Every one of these is a
    /// working option-injection payload or a malformed-ref shape.
    #[test]
    fn rejects_switch_shaped_and_malformed_names() {
        for bad in [
            // The audit's verified payloads.
            "--upload-pack=/x",
            "--depth=1",
            "--strategy=evil",
            "-d",
            "=a",
            "a/=b",
            // Structural refusals.
            "",
            "-",
            "--",
            ".hidden",
            "/abs",
            "a.",
            "a/",
            "a//b",
            "a/.b",
            "a..b",
            "a b",
            "a;b",
            "a\tb",
            "a\nb",
            "a:b",
            "a?b",
            "a*b",
            "a^b",
            "a~b",
            "a\\b",
            "a@{b",
            "--filter=blob:none",
        ] {
            assert!(check_refname(bad).is_err(), "check_refname accepted an unsafe ref: {bad:?}");
        }
    }

    /// Acceptance criterion 1 (#9106), accepted half. A false rejection here
    /// would deny legitimate merges, so the list covers the branch shapes Loom
    /// actually produces.
    #[test]
    fn accepts_legitimate_branch_names() {
        for good in [
            "feature/issue-42",
            "main",
            "master",
            "hotfix/x.y",
            "a-b_c.d",
            "release/v1.2.3",
            "9x",
            "chore/resync-installed",
            "docs/onboarding-cleanup",
            "a",
        ] {
            assert!(
                check_refname(good).is_ok(),
                "check_refname rejected a legitimate ref: {good:?}"
            );
        }
    }

    #[test]
    fn error_message_names_the_offending_ref() {
        let err = check_refname("--upload-pack=/tmp/payload").unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("--upload-pack=/tmp/payload"),
            "refusal must quote the invalid ref so it is greppable: {text}"
        );
        assert!(text.contains("#9106"), "refusal must cite the issue: {text}");
    }

    #[test]
    fn check_all_returns_the_first_refusal() {
        assert!(check_all(&["main", "feature/issue-1"]).is_ok());
        let err = check_all(&["main", "--depth=1", "feature/issue-1"]).unwrap_err();
        assert_eq!(err.name, "--depth=1");
    }

    /// Non-ASCII is outside the allowlist. Stated as its own test because
    /// "alphanumeric" in Rust means *Unicode* alphanumeric unless the
    /// `is_ascii_` form is used, and the ASCII restriction is deliberate.
    #[test]
    fn rejects_non_ascii() {
        assert!(check_refname("feature/ıssue-42").is_err());
        assert!(check_refname("ｍain").is_err());
    }
}
