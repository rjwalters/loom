//! Equivalence kind 3 (Issue #9416): **the PR's own patch is byte-identical**
//! across the head move — the rebase-onto-a-newer-base case.
//!
//! # The test
//!
//! The operator's rule offers two interchangeable checks for this row:
//! `git range-diff <old-base>..<reviewed> <new-base>..<new>` showing every
//! commit as `=`, **or** the merge-base-relative diffs being byte-for-byte
//! equal. This module implements the second, computed by the forge:
//!
//! ```text
//! compare/{base-branch}...{reviewed}   ==   compare/{base-branch}...{head}
//! ```
//!
//! A three-dot compare diffs `merge-base(base, X)` against `X`, which IS the
//! merge-base-relative diff the rule names: for the reviewed head the merge
//! base is the old base, for the new head it is the new base, because a base
//! branch only ever moves forward. (That also makes the pair race-free: the two
//! requests may see different base tips, and `merge-base(tip, X)` is the same
//! commit for every tip descending from it.)
//!
//! # Why not `range-diff`, and why not local git
//!
//! - `range-diff` has no caller in this repo, and its answer is a **parsed
//!   marker column** (`=`/`!`/`<`/`>`) whose pairing is heuristic
//!   (`--creation-factor`). An ambiguous or unparsable row would have to fail
//!   closed anyway, and a squash during the rebase shows as `!`/`<`/`>` even
//!   when the net patch is untouched. The cumulative diff comparison is the
//!   literal statement of the rule — "the PR's own patch is unchanged" — and
//!   has no parsing ambiguity.
//! - A rebase **orphans** the reviewed head. An unreachable commit is not
//!   fetchable from the forge, so a local-git implementation of this kind would
//!   answer `Indeterminate` on precisely the hosts that need it. The compare
//!   endpoint has no such limit: it serves any commit the repository still
//!   holds, which includes every head a PR has ever had.
//!
//! # What "byte-identical" means here, concretely
//!
//! For each changed file the two responses must agree on: `filename`,
//! `previous_filename` (so a rename is not laundered), `status`, `sha` (the
//! **resulting blob id** — identical content, which is what closes the binary
//! hole a text-only comparison would leave), and `patch` (the unified diff
//! text, which encodes the *prior* content through its context and `-` lines).
//! Entries are sorted by filename first, because the endpoint's order is not a
//! documented guarantee.
//!
//! Two consequences worth stating, both deliberate and both conservative:
//!
//! - If the base changed a file the PR also touches, `sha`'s neighbouring index
//!   information and the patch's own context differ, so the kind **refutes** and
//!   the PR is re-reviewed. That is the correct answer: the reviewer read the
//!   patch against different surrounding content.
//! - If both diffs are **empty**, the kind answers `Indeterminate`, not
//!   `Proven`. Two empty three-dot diffs only say "each head equals its own
//!   merge base" — and those merge bases can differ, so the trees can differ.
//!   This is the same class of hole PR #9581 found in reading `files: []`
//!   without a `status`.
//!
//! # Fail closed
//!
//! A `gh` failure, an unparsable response, a response missing its `files` key, a
//! `files` array at the endpoint's 300-entry cap (it may be truncated, so the
//! file set is unknown), or **any** changed file whose `patch` the endpoint
//! omitted (binary content, or a diff too large to serialize — no byte evidence
//! either way) all answer [`Evidence::Indeterminate`].

use std::path::Path;

use serde::Deserialize;

use super::Evidence;

/// GitHub's compare endpoint returns at most this many file entries per page.
/// A response AT the cap may be truncated, so the file set is unknown and no
/// byte comparison over it can be trusted.
const FILES_PAGE_CAP: usize = 300;

#[derive(Deserialize)]
struct Compare {
    /// REQUIRED: a response without it is no answer at all, never an assumed
    /// empty diff (the fail-open arm PR #9581 removed from the tree kind).
    files: Vec<CompareFile>,
}

#[derive(Deserialize)]
struct CompareFile {
    filename: String,
    status: String,
    /// The resulting blob id at the compared head.
    #[serde(default)]
    sha: Option<String>,
    /// The unified diff text. Omitted by the endpoint for binary content and
    /// for diffs too large to serialize.
    #[serde(default)]
    patch: Option<String>,
    #[serde(default)]
    previous_filename: Option<String>,
}

/// One file's full identity, as compared between the two sides.
type FileIdentity = (String, String, String, String, String);

/// Fetch and canonicalize one side's merge-base-relative diff.
fn side(
    gh_bin: &Path,
    cwd: Option<&Path>,
    base_ref: &str,
    head: &str,
) -> Option<Vec<FileIdentity>> {
    let body = super::gh_api(
        gh_bin,
        cwd,
        &format!("repos/{{owner}}/{{repo}}/compare/{base_ref}...{head}"),
    )?;
    let parsed: Compare = serde_json::from_slice(&body).ok()?;
    if parsed.files.len() >= FILES_PAGE_CAP {
        return None;
    }
    let mut rows: Vec<FileIdentity> = Vec::with_capacity(parsed.files.len());
    for f in parsed.files {
        // No patch text => no byte evidence. Refusing here is what keeps a
        // binary change (whose diff the endpoint renders as nothing at all)
        // from reading as "identical" against a different binary change.
        let patch = f.patch?;
        let sha = f.sha?;
        rows.push((f.filename, f.previous_filename.unwrap_or_default(), f.status, sha, patch));
    }
    // The endpoint's ordering is not a documented guarantee; sort so the
    // comparison is about content, not about response order.
    rows.sort();
    Some(rows)
}

/// Is the PR's own patch byte-identical before and after the head move?
pub fn evidence(
    gh_bin: &Path,
    cwd: Option<&Path>,
    reviewed: &str,
    head: &str,
    base_ref: &str,
) -> Evidence {
    if !super::is_sha(reviewed) || !super::is_sha(head) || !super::is_safe_ref(base_ref) {
        return Evidence::Indeterminate;
    }
    let (Some(before), Some(after)) =
        (side(gh_bin, cwd, base_ref, reviewed), side(gh_bin, cwd, base_ref, head))
    else {
        return Evidence::Indeterminate;
    };
    // Two empty diffs prove nothing: each head merely equals its own merge
    // base, and those merge bases can differ. See the module doc.
    if before.is_empty() && after.is_empty() {
        return Evidence::Indeterminate;
    }
    if before == after {
        Evidence::Proven
    } else {
        Evidence::Refuted
    }
}
