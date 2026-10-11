//! The pre-merge `workflow` token-scope guard (#10539).
//!
//! GitHub refuses (403) to merge a PR that creates or updates
//! `.github/workflows/*` when the OAuth token in use lacks the `workflow`
//! scope. Champion's critical-file hold routes exactly these PRs to a human
//! merge, so without this guard the hand-off ends in a raw API 403. [`assess`]
//! turns that into an up-front refusal naming the fix.
//!
//! # Fail open, except for one case
//!
//! Block ONLY when the PR touches `.github/workflows/` AND the token's
//! `X-OAuth-Scopes` response header is PRESENT and lacks `workflow`. A missing
//! header (App installation and fine-grained tokens send none) is "unknown":
//! the real merge path decides. Lookup failures are the caller's to treat the
//! same way. No I/O here — the caller supplies the file list and the raw
//! `gh api -i user` response.

/// Env escape hatch: set to `1` to skip the guard entirely.
pub const SKIP_ENV: &str = "LOOM_SKIP_WORKFLOW_SCOPE_CHECK";

/// The command that fixes a token lacking the scope.
pub const FIX_COMMAND: &str = "gh auth refresh -h github.com -s workflow";

/// What the guard decided.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Proceed: the PR needs no scope, or the scope is present or unknown.
    Proceed,
    /// Refuse the merge, with the message to show.
    Blocked(String),
}

/// Whether any changed path is under `.github/workflows/`.
#[must_use]
pub fn touches_workflows(files: &str) -> bool {
    files
        .lines()
        .any(|l| l.trim().starts_with(".github/workflows/"))
}

/// The scopes named by an `X-OAuth-Scopes` header in a raw HTTP response
/// (`gh api -i`): `None` when the header is absent, `Some(list)` otherwise
/// (possibly empty for a token with no scopes).
#[must_use]
pub fn scopes_header(response: &str) -> Option<Vec<String>> {
    for line in response.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            break; // end of headers
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("x-oauth-scopes") {
                return Some(
                    value
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect(),
                );
            }
        }
    }
    None
}

/// Decide whether to block the merge of `pr`.
#[must_use]
pub fn assess(pr: &str, files: &str, user_response: &str) -> Verdict {
    if !touches_workflows(files) {
        return Verdict::Proceed;
    }
    let Some(scopes) = scopes_header(user_response) else {
        return Verdict::Proceed;
    };
    if scopes.iter().any(|s| s == "workflow") {
        return Verdict::Proceed;
    }
    let have = if scopes.is_empty() {
        "none".to_string()
    } else {
        scopes.join(", ")
    };
    Verdict::Blocked(format!(
        "Merge refused: PR #{pr} changes .github/workflows/ but the active gh token \
         lacks the 'workflow' scope (scopes: {have}), so GitHub would reject the merge \
         with a 403 (#10539). Fix: {FIX_COMMAND}  (then re-run). Bypass: {SKIP_ENV}=1."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const WF: &str = "README.md\n.github/workflows/deploy.yml\n";
    const NO_WF: &str = "README.md\nsrc/main.rs\n";

    fn resp(scopes: Option<&str>) -> String {
        let mut s = String::from("HTTP/2.0 200 OK\r\nContent-Type: application/json\r\n");
        if let Some(v) = scopes {
            s.push_str(&format!("X-Oauth-Scopes: {v}\r\n"));
        }
        s.push_str("\r\n{\"login\":\"x\"}");
        s
    }

    #[test]
    fn blocks_workflow_pr_without_scope() {
        let v = assess("7", WF, &resp(Some("admin:public_key, gist, read:org, repo")));
        match v {
            Verdict::Blocked(m) => {
                assert!(m.contains(FIX_COMMAND));
                assert!(m.contains("#7"));
            }
            Verdict::Proceed => panic!("expected block"),
        }
    }

    #[test]
    fn blocks_when_header_present_but_empty() {
        assert!(matches!(assess("7", WF, &resp(Some(""))), Verdict::Blocked(_)));
    }

    #[test]
    fn proceeds_when_scope_present() {
        assert_eq!(assess("7", WF, &resp(Some("repo, workflow"))), Verdict::Proceed);
    }

    #[test]
    fn proceeds_without_workflow_files() {
        assert_eq!(assess("7", NO_WF, &resp(Some("repo"))), Verdict::Proceed);
    }

    #[test]
    fn proceeds_when_header_absent() {
        assert_eq!(assess("7", WF, &resp(None)), Verdict::Proceed);
    }

    #[test]
    fn header_in_body_is_not_a_header() {
        let r = "HTTP/2.0 200 OK\r\n\r\nx-oauth-scopes: repo\r\n";
        assert_eq!(scopes_header(r), None);
    }

    #[test]
    fn scope_name_must_match_exactly() {
        assert!(matches!(
            assess("7", WF, &resp(Some("repo, workflow-extra"))),
            Verdict::Blocked(_)
        ));
    }
}
