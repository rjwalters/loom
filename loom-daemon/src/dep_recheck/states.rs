//! `dep-states` — terminal-state resolution for the Guide's unblock sweep
//! (#14710).
//!
//! Answers one question: is every dependency in this list terminal (CLOSED or
//! MERGED) **for the exact `OWNER/REPO#N` it was named as**? The Guide's
//! `check_and_unblock` (and its PR-side twin) used to answer it with an inline
//! `gh issue view "$n" --repo "$r" --json state` per dependency, and on
//! 2026-11-19 that read reported `2AMLogic/.loom#630` — a cross-repository
//! draft PR — as resolved while it was still OPEN, so the hold was lifted
//! (issue #14710). Nothing in a bare `state` string can tell "the state of
//! the node I asked about" from "a state some cache or shim served for a
//! different node", which is why the resolution moved daemon-side, where the
//! answer is bound to its node ([`url_names_ref`]) and every non-terminal,
//! unreadable or mismatched outcome fails closed.
//!
//! The three-way split (resolved / open / unknown) mirrors
//! [`crate::dep_classify::state`]'s `RefState` classification, and `unknown`
//! is the load-bearing bucket: an unknown dependency is never silently
//! counted as resolved — that would lift exactly the hold this subcommand
//! exists to protect.

/// One parsed `--refs` token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedRef {
    /// `N` (the invoking repo) or `OWNER/REPO#N`. `N` must be positive.
    Spec(RefSpec),
    /// Anything else, carried verbatim so it is *reported* as unknown rather
    /// than dropped: a dependency silently filtered out of the list is a
    /// false `ALL_RESOLVED=true` waiting to happen, the same
    /// worse-failure-direction argument as [`super::named`]'s permissive
    /// item matcher. (Differs from `operator-premise`'s hard `exit 1` on a
    /// bad token: that subcommand fingerprints one caller's refs, this one
    /// classifies a park record's whole dependency list and must still
    /// answer for the tokens it could parse.)
    Invalid(String),
}

/// A resolvable dependency reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefSpec {
    /// `Some("owner/name")` for a cross-repo reference written
    /// `owner/repo#N`; `None` for a bare `N`, which means the invoking repo.
    pub repo: Option<String>,
    pub number: i64,
}

impl RefSpec {
    /// How this reference reads in prose and in `RESOLVED`/`OPEN`/`UNKNOWN`:
    /// `#123` for a same-repo reference, `owner/repo#123` for a cross-repo
    /// one — [`super::named::Dep::reference`]'s convention, so the Guide's
    /// unblock announcement reads the same as it did when the loop rendered
    /// `$dr#$dn` itself.
    #[must_use]
    pub fn reference(&self) -> String {
        match self.repo.as_deref() {
            Some(r) => format!("{r}#{}", self.number),
            None => format!("#{}", self.number),
        }
    }
}

/// Parse one whitespace-separated `--refs` token.
///
/// A bare token is `N` (or `#N` — also accepted, because that is the shape
/// `park_record::BlockerRef` accepts as *input*, and a future emitter
/// switching to it must not silently turn every local dependency into an
/// eternal UNKNOWN); a qualified one is `OWNER/REPO#N` with the same charset
/// restriction [`super::named`] applies (`[A-Za-z0-9][A-Za-z0-9-]*/
/// [A-Za-z0-9._-]+`), because the repo half is fed to `gh --repo` verbatim
/// and issue bodies are untrusted input — a restricted charset that cannot
/// start with `-` cannot be smuggled through as a flag.
#[must_use]
pub fn parse_token(token: &str) -> ParsedRef {
    if let Some((repo, num)) = token.rsplit_once('#') {
        if !repo.is_empty() && super::named::is_owner_repo(repo) {
            if let Ok(n) = num.parse::<i64>() {
                if n > 0 {
                    return ParsedRef::Spec(RefSpec {
                        repo: Some(repo.to_string()),
                        number: n,
                    });
                }
            }
        }
    }
    if let Ok(n) = token.strip_prefix('#').unwrap_or(token).parse::<i64>() {
        if n > 0 {
            return ParsedRef::Spec(RefSpec {
                repo: None,
                number: n,
            });
        }
    }
    ParsedRef::Invalid(token.to_string())
}

/// Parse a whole `--refs` value: any whitespace-separated mix of `N` and
/// `OWNER/REPO#N` tokens, which is exactly the shape `parse_dependencies`
/// emits (one per line, but line boundaries are just whitespace here).
#[must_use]
pub fn parse_refs(tokens: &str) -> Vec<ParsedRef> {
    tokens.split_whitespace().map(parse_token).collect()
}

/// Where a dependency stands, once read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepState {
    /// Still open: the dependency genuinely holds.
    Open,
    /// Closed or merged: terminal.
    Resolved,
    /// Could not be determined. **Never** treated as resolved.
    Unknown,
}

impl DepState {
    /// Map a forge `state` string, matching
    /// [`crate::dep_classify::state::RefState`]'s mapping: anything that is
    /// not `OPEN`, `CLOSED` or `MERGED` — including an empty string — is
    /// `Unknown`, so an unrecognised future state fails safe rather than
    /// lifting a hold.
    #[must_use]
    pub fn from_forge_state(s: &str) -> Self {
        match s {
            "OPEN" => DepState::Open,
            "CLOSED" | "MERGED" => DepState::Resolved,
            _ => DepState::Unknown,
        }
    }
}

/// Does `url` name exactly `repo#number`? The binding check (#14710).
///
/// `gh … --json url` answers with the node's canonical URL, whose path ends
/// `/{owner}/{repo}/issues/{n}` for an issue and `/{owner}/{repo}/pull/{n}`
/// (GitHub) or `/pulls/{n}` (Gitea) for a PR. Comparing that path — not just
/// the state string — is what makes a wrong-node answer detectable: the
/// suspected #14710 cause is a cached/shed/otherwise-mismatched read for the
/// same *number* in a *different* repository (`2AMLogic/2am#630` is CLOSED;
/// `2AMLogic/.loom#630` was an open draft), which a bare state read accepts
/// without blinking. Case-insensitive, because `--repo` accepts any casing
/// and the URL carries the canonical one.
///
/// This compares against ONE slug. A renamed repository answers under its
/// *canonical* slug, so [`super::forge::BoundReader`] binds an exact-slug
/// match first and only then resolves the requested slug's canonical name
/// (REST follows renames) and binds against that — a verified redirect to
/// the same repository is the same dependency, an unrelated same-numbered
/// node in a different repository is not (the `rjwalters/loom` →
/// `2AMLogic/loom` rename vs the `2AMLogic/2am#630` misread, #14710).
#[must_use]
pub fn url_names_ref(url: &str, repo: &str, number: i64) -> bool {
    let lower = url.to_ascii_lowercase();
    let needle = format!("/{}/", repo.to_ascii_lowercase());
    // rfind, not contains: with a two-part owner/repo there is no legitimate
    // longer path that could end in the same needle, but rfind also makes a
    // nested `.../x/owner/repo/issues/N` impossible to satisfy by matching a
    // prefix — the tail after the LAST `owner/repo/` must be the whole answer.
    let Some(pos) = lower.rfind(&needle) else {
        return false;
    };
    let tail = &lower[pos + needle.len()..];
    let n = number.to_string();
    tail == format!("issues/{n}") || tail == format!("pull/{n}") || tail == format!("pulls/{n}")
}

#[cfg(test)]
mod tests;
