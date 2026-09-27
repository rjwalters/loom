//! `git worktree list --porcelain` parsing for `merge-pr.sh` (#8191 slice).
//!
//! Three queries, all over the same porcelain text:
//!
//! - [`primary_path`] — the PRIMARY (main) working copy's path. This is the
//!   input to the #3710 hard guard that refuses to `git worktree remove` the
//!   main checkout.
//! - [`branch_for_path`] — the branch short-name attached to a given worktree
//!   path. Read BEFORE removing a worktree (the porcelain entry vanishes with
//!   it) and used to decide which local branch to delete afterwards.
//! - [`find_by_branch`] — the worktree path with a given branch checked out.
//!   The post-merge discovery fallback when the Loom-convention path is absent.
//!
//! # Why these next
//!
//! Every consumer is one of `merge-pr.sh`'s **irreversible** post-merge steps —
//! `git worktree remove --force`, `git branch -D` — and each of the three
//! parsers has already produced a shipped defect:
//!
//! - **#3671** — the classic `awk` `exit`-triggers-`END` gotcha. The blank-line
//!   rule printed the match and called `exit`; control transferred to `END`,
//!   whose condition was still true, and printed the same value a second time.
//!   Callers got `"/path\n/path"` as a single string — a path that exists
//!   nowhere, in a warning telling an operator what to remove by hand.
//! - **#3717** — the path was parsed as `$2`, which `awk` splits at the first
//!   space. A checkout under `~/My Repos/loom` reported `/Users/x/My`, so the
//!   primary-worktree guard compared against a truncated path and did not fire.
//!   Fixed by `substr($0, 10)`; the same truncation in `find_by_branch` handed
//!   a truncated path to the removal advice.
//! - **#4171** — distinguishing "this IS the primary checkout" from "this is a
//!   removable linked worktree" at all, so the script stops suggesting
//!   `git worktree remove` against the main working copy.
//!
//! All three are bugs about *what the record-oriented parse saw*, which is the
//! class this epic exists to move out of `awk`.
//!
//! # Fidelity: these model `awk`, not "a sensible parser"
//!
//! Each function below is a literal port of its `awk` body's observable
//! output, pinned by a differential against a frozen copy of those bodies
//! (`tests/merge_pr_worktrees_differential.rs`). Where `awk` is peculiar, the
//! port is peculiar in the same way — deliberately:
//!
//! - **Records are `\n`-separated and a trailing `\r` is NOT stripped.**
//!   Rust's [`str::lines`] strips `\r\n`; `awk` with the default `RS` does
//!   not, so a CRLF stanza's `branch` value would keep its `\r` in `awk` and
//!   lose it in a `lines()`-based port. See [`records`].
//! - **`branch` is read as `$2`, `worktree` as `substr($0, 10)`.** Ref names
//!   cannot contain spaces, so whitespace splitting is safe for the branch;
//!   paths can, so the prefix is stripped by length instead.
//! - **The `branch` rule does not `next`**, so a line can set `br` and still be
//!   tested by a later rule in the same pass. (No line is both `branch …` and
//!   empty, so this is only about ordering, but the port keeps the shape.)
//! - **A path containing a literal newline still breaks the parse**, exactly as
//!   it did in `awk` (#3717's recorded caveat). `git worktree list
//!   --porcelain -z` is the real fix; making it here would change which
//!   worktrees are found, which is a behaviour change and not this port's job.
//!   The shell still runs the `git` invocation, so the flag is the shell's to
//!   change when someone takes that on.
//!
//! # Failure direction of the callers
//!
//! Two of the three degrade safely to "no answer": no branch found means no
//! branch is deleted, no worktree found means none is removed. [`primary_path`]
//! does not — an empty answer there makes the #3710 guard silently *not fire*,
//! which is the direction that destroys the main checkout. `merge-pr.sh` is
//! therefore written to distinguish "parsed, nothing matched" (this module
//! returning `None`, exit 0 and empty stdout) from "could not parse at all"
//! (a non-zero exit from the binary), and to refuse the removal on the latter.

/// `awk`'s view of the input as records, with the default `RS = "\n"`.
///
/// NOT [`str::lines`]: that also splits on `\r\n` and strips the `\r`, so a
/// CRLF porcelain stanza would yield `refs/heads/foo` here and
/// `refs/heads/foo\r` in the shell — a silent mismatch in the one comparison
/// that decides whether a branch gets deleted.
///
/// A final `\n` terminates the last record rather than starting an empty one,
/// and empty input has no records at all — both `awk`'s behaviour.
fn records(text: &str) -> impl Iterator<Item = &str> {
    let body = if text.is_empty() {
        None
    } else {
        Some(text.strip_suffix('\n').unwrap_or(text))
    };
    body.into_iter().flat_map(|b| b.split('\n'))
}

/// `$2` of a `branch <ref>` line under `awk`'s default field splitting.
///
/// Splits on space and tab ONLY — the POSIX `<blank>` set, which is what a
/// default `FS = " "` means. Notably it does NOT include `\r`:
/// [`str::split_ascii_whitespace`] does, and would silently strip a CRLF
/// record's carriage return from the ref name while `awk` (verified on both
/// BSD `awk` and `mawk`: `$2` of `branch refs/heads/main\r` is
/// `refs/heads/main\r`, `NF` is 2) keeps it. That single byte decides whether
/// `find_by_branch` matches, and a match there authorises `git branch -D`.
fn field2(rest: &str) -> &str {
    // `rest` is everything after the literal `branch `, so the first non-empty
    // blank-delimited token in it is `$2` (awk skips leading blanks).
    rest.split([' ', '\t'])
        .find(|f| !f.is_empty())
        .unwrap_or("")
}

/// The PRIMARY (main) worktree's path: the FIRST `worktree ` record.
///
/// `git` always lists the main working tree first, which is why taking the
/// first entry (`awk`'s `{ print substr($0, 10); exit }`) is the whole
/// definition. `None` when the input has no `worktree ` record at all — which
/// is what `git worktree list` failing produces, and which the caller must NOT
/// read as "the target is not the primary checkout".
#[must_use]
pub fn primary_path(porcelain: &str) -> Option<&str> {
    records(porcelain).find_map(|line| line.strip_prefix("worktree "))
}

/// The branch short-name checked out at `want_path`, `refs/heads/` stripped.
///
/// `None` for a detached or bare entry (no `branch` record ⇒ `awk`'s
/// `br != ""` is false) and for a path that appears in no stanza.
///
/// The comparison is exact string equality on the path, as in `awk`. The shell
/// canonicalises its argument with `cd … && pwd -P` before calling, because
/// `git` prints canonical paths; that canonicalisation stays in the shell,
/// where it has a real filesystem to consult.
#[must_use]
pub fn branch_for_path(porcelain: &str, want_path: &str) -> Option<String> {
    scan(porcelain, |wt, br| {
        (wt == want_path && !br.is_empty())
            .then(|| br.strip_prefix("refs/heads/").unwrap_or(br).to_string())
    })
}

/// The worktree path with `want_branch` (a short name) checked out.
///
/// Mirrors `awk -v want="refs/heads/<branch>"`: the comparison is against the
/// fully-qualified ref, so a short name that happens to equal some other ref's
/// text cannot match, and detached/bare entries (which carry no `branch`
/// record) are skipped for free.
#[must_use]
pub fn find_by_branch<'a>(porcelain: &'a str, want_branch: &str) -> Option<&'a str> {
    let want = format!("refs/heads/{want_branch}");
    scan(porcelain, |wt, br| (br == want).then_some(wt))
}

/// The shared stanza walk: `awk`'s three rules plus its `END` block.
///
/// `pick` is `awk`'s condition-and-print, evaluated with the stanza state
/// exactly where `awk` would have it. It is called on each blank record and
/// once more at end of input — and the `found` flag that #3671 added is
/// structural here rather than a variable: the first `Some` returns, so the
/// `END` arm cannot re-emit it. That is the bug becoming unrepresentable
/// rather than guarded against, which is the point of moving it.
fn scan<'a, T>(
    porcelain: &'a str,
    mut pick: impl FnMut(&'a str, &'a str) -> Option<T>,
) -> Option<T> {
    // `awk`'s uninitialised scalars compare equal to "", so an input whose
    // first record is blank tests ("", "") — not "no stanza yet".
    let mut wt = "";
    let mut br = "";
    for line in records(porcelain) {
        if let Some(path) = line.strip_prefix("worktree ") {
            wt = path;
            br = "";
            continue; // awk's `next`
        }
        if let Some(rest) = line.strip_prefix("branch ") {
            br = field2(rest);
            // No `next` in the shell either — fall through to the rules below.
        }
        if line.is_empty() {
            if let Some(found) = pick(wt, br) {
                return Some(found); // awk's `exit`
            }
        }
    }
    // awk's END: catches a final stanza with no terminating blank record. When
    // the input DID end with a blank record the state is unchanged since that
    // record was tested, so re-testing it cannot produce a second answer.
    pick(wt, br)
}

#[cfg(test)]
mod tests;
