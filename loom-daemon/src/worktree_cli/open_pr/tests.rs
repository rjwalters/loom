//! Unit coverage for the pure halves of the #7765 forge round-trip (#8195
//! slice 15) — remote-URL classification and forge-JSON parsing, neither of
//! which need a git process or a network.
//!
//! End-to-end behaviour through the real `worktree.sh` (the four scenarios a
//! `gh` stand-in drives — cross-repo refusal, same-repo fetch+reuse, genuine
//! unavailability, and the #7863 local-only-clone regression) is pinned by
//! `defaults/scripts/tests/test-worktree-forge-pr-check.sh`, which this port
//! must keep passing unchanged.

use super::*;

// ---------------------------------------------------------------------------
// could_be_forge — mirrors `_worktree_repo_has_forge_remote`'s case pattern
// ---------------------------------------------------------------------------

#[test]
fn https_url_could_be_forge() {
    assert!(could_be_forge("https://github.com/rjwalters/loom.git"));
}

#[test]
fn scp_like_url_could_be_forge() {
    assert!(could_be_forge("git@github.com:rjwalters/loom.git"));
}

#[test]
fn ssh_scheme_url_could_be_forge() {
    assert!(could_be_forge("ssh://git@github.com/rjwalters/loom.git"));
}

#[test]
fn empty_url_is_not_a_forge() {
    assert!(!could_be_forge(""));
}

#[test]
fn file_scheme_is_not_a_forge() {
    assert!(!could_be_forge("file:///tmp/origin.git"));
}

#[test]
fn absolute_path_is_not_a_forge() {
    assert!(!could_be_forge("/tmp/loom-wtforge.abc123/origin.git"));
}

#[test]
fn relative_path_is_not_a_forge() {
    assert!(!could_be_forge("./origin.git"));
    assert!(!could_be_forge("../origin.git"));
}

#[test]
fn tilde_relative_path_is_not_a_forge() {
    assert!(!could_be_forge("~/repos/origin.git"));
}

// ---------------------------------------------------------------------------
// classify_failure — the #7863 regression's exact two substrings
// ---------------------------------------------------------------------------

#[test]
fn no_known_github_host_is_no_forge_remote() {
    let stderr = "none of the git remotes configured for this repository point to a known GitHub host. To tell gh about a new GitHub host, please use `gh auth login`";
    assert_eq!(classify_failure(stderr), Status::NoForgeRemote);
}

#[test]
fn gitea_decline_is_no_forge_remote() {
    let stderr =
        "loom-daemon forge: gitea is not handled natively; falling back to the caller's shell path";
    assert_eq!(classify_failure(stderr), Status::NoForgeRemote);
}

#[test]
fn unauthenticated_bailout_is_unavailable_not_no_forge_remote() {
    // The #7863 shape exactly: gh bails out before it can say "no known
    // GitHub host" at all, so this text must NOT be misfiled as
    // NoForgeRemote (that misfiling is the historical regression).
    let stderr = "To get started with GitHub CLI, please run:  gh auth login\nAlternatively, populate the GH_TOKEN environment variable with a GitHub API authentication token.";
    assert_eq!(classify_failure(stderr), Status::Unavailable);
}

#[test]
fn rate_limit_is_unavailable() {
    assert_eq!(
        classify_failure("gh: API rate limit exceeded for this token"),
        Status::Unavailable
    );
}

#[test]
fn classify_failure_is_case_insensitive() {
    let stderr =
        "NONE OF THE GIT REMOTES CONFIGURED FOR THIS REPOSITORY POINT TO A KNOWN GITHUB HOST";
    assert_eq!(classify_failure(stderr), Status::NoForgeRemote);
}

// ---------------------------------------------------------------------------
// parse_probe — the real forge shape, and its degenerate cases
// ---------------------------------------------------------------------------

#[test]
fn parses_a_cross_repo_open_pr() {
    let text = r#"[{"number": 1234, "isCrossRepository": true, "headRepository": {"nameWithOwner": "forkuser/loom"}, "headRefName": "feature/issue-77", "url": "https://github.com/rjwalters/loom/pull/1234"}]"#;
    let probe = parse_probe(text);
    assert_eq!(probe.status, Status::Found);
    let pr = probe.pr.expect("found probe carries a PR");
    assert_eq!(pr.number, "1234");
    assert!(pr.is_cross_repo);
    assert_eq!(pr.head_repo, "forkuser/loom");
    assert_eq!(pr.head_ref, "feature/issue-77");
    assert_eq!(pr.url, "https://github.com/rjwalters/loom/pull/1234");
}

#[test]
fn parses_a_same_repo_open_pr() {
    let text = r#"[{"number": 999, "isCrossRepository": false, "headRepository": {"nameWithOwner": "rjwalters/loom"}, "headRefName": "feature/issue-77", "url": "https://github.com/rjwalters/loom/pull/999"}]"#;
    let probe = parse_probe(text);
    assert_eq!(probe.status, Status::Found);
    assert!(!probe.pr.unwrap().is_cross_repo);
}

#[test]
fn empty_array_is_not_found() {
    assert_eq!(parse_probe("[]").status, Status::NotFound);
    assert!(parse_probe("[]").pr.is_none());
}

#[test]
fn unparseable_text_is_unavailable() {
    assert_eq!(parse_probe("not json").status, Status::Unavailable);
    assert_eq!(parse_probe("").status, Status::Unavailable);
}

#[test]
fn non_array_json_is_unavailable() {
    assert_eq!(parse_probe(r#"{"error": "nope"}"#).status, Status::Unavailable);
}

#[test]
fn missing_optional_fields_degrade_to_empty_not_a_parse_failure() {
    let text = r#"[{"number": 5}]"#;
    let probe = parse_probe(text);
    assert_eq!(probe.status, Status::Found);
    let pr = probe.pr.unwrap();
    assert_eq!(pr.number, "5");
    assert!(!pr.is_cross_repo);
    assert_eq!(pr.head_repo, "");
    assert_eq!(pr.head_ref, "");
    assert_eq!(pr.url, "");
}

// ---------------------------------------------------------------------------
// query — the branch-empty short-circuit the shell's own guard relies on
// ---------------------------------------------------------------------------

#[test]
fn empty_branch_is_unavailable_without_touching_git() {
    // A directory that is not even a git repo: if this reached `git remote`
    // it would fail loudly rather than degrade cleanly, so a status other
    // than Unavailable here would mean the empty-branch short-circuit was
    // removed.
    let probe = query(Path::new("/nonexistent/not-a-repo"), "");
    assert_eq!(probe.status, Status::Unavailable);
    assert!(probe.pr.is_none());
}
