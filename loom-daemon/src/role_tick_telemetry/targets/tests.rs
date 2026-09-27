//! Target extraction (Issue #9168): recorded-transcript fixtures plus the
//! command-shape edge cases behind them.

use super::*;
use std::path::PathBuf;

const OWN: &str = "rjwalters/loom";

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/role_tick_targets")
        .join(name)
}

/// The own-repo numbers a recorded tick transcript wrote to, via the real
/// single-pass scanner, plus how many cross-repo targets were refused.
fn fixture_targets(name: &str) -> (Vec<u32>, usize) {
    let (_, targets) = crate::role_tick_telemetry::scan_transcripts_with_targets(&[fixture(name)])
        .expect("fixture is readable");
    own_repo_numbers(&targets, OWN)
}

fn targets_of(command: &str) -> Vec<Target> {
    let mut out = BTreeSet::new();
    collect(command, &mut out);
    out.into_iter().collect()
}

fn own(number: u32) -> Target {
    Target { number, repo: None }
}

#[test]
fn single_target_from_a_judge_transcript() {
    // Reads of PR 9201 (view/diff/checks/GET api) do not count; the comment
    // and relabel do, deduplicated to one target. The heredoc body's prose
    // mentioning other write commands is not a command.
    assert_eq!(fixture_targets("judge_single.jsonl"), (vec![9201], 0));
}

#[test]
fn several_targets_from_a_champion_transcript() {
    // Compound command, merge-pr.sh, REST POST, PR URL; `--dry-run` excluded.
    assert_eq!(fixture_targets("champion_several.jsonl"), (vec![9150, 9160, 9170, 9180], 0));
}

#[test]
fn cross_repo_targets_are_refused() {
    // `-R`, an explicit API owner/repo and a foreign URL are all refused;
    // `--repo` naming the own repo (any case) is kept.
    assert_eq!(fixture_targets("curator_cross_repo.jsonl"), (vec![9168], 3));
}

#[test]
fn a_read_only_transcript_has_no_target() {
    assert_eq!(fixture_targets("guide_none.jsonl"), (vec![], 0));
}

#[test]
fn read_only_verbs_never_count() {
    for command in [
        "gh pr view 7",
        "gh pr diff 7",
        "gh pr checks 7",
        "gh pr checkout 7",
        "gh issue view 7 --comments",
        "gh issue list --label loom:issue",
        "gh api repos/{owner}/{repo}/issues/7",
        "gh api repos/{owner}/{repo}/pulls/7/files --paginate",
        "gh api -X GET repos/{owner}/{repo}/issues/7/comments",
        "gh pr edit 7 --help",
    ] {
        assert_eq!(targets_of(command), vec![], "{command}");
    }
}

#[test]
fn write_verbs_name_their_number() {
    for (command, number) in [
        ("gh issue edit 42 --add-label loom:building", 42),
        ("gh issue comment '#42' --body hi", 42),
        ("gh issue close 42 --reason completed", 42),
        ("gh pr edit --add-label loom:pr 7", 7),
        ("gh pr merge --squash 7", 7),
        ("gh pr merge -s -d 7", 7),
        ("gh pr review 7 --request-changes -b nope", 7),
        ("gh pr ready 7", 7),
        ("GH_TOKEN=x env gh pr comment 7 -b ok", 7),
        ("/usr/local/bin/gh pr comment 7 --body=ok", 7),
        ("merge-pr.sh 7", 7),
        (".loom/scripts/merge-pr.sh --worktree-path /tmp/w 7 --auto", 7),
        ("gh api repos/{owner}/{repo}/issues/7/comments -f body=hi", 7),
        ("gh api --method=PUT /repos/:owner/:repo/pulls/7/merge", 7),
        ("gh api -XPATCH https://api.github.com/repos/{owner}/{repo}/issues/7", 7),
        ("gh api -X DELETE repos/{owner}/{repo}/issues/7/labels/loom:pr", 7),
        ("gh api repos/{owner}/{repo}/issues/7/labels --input labels.json", 7),
    ] {
        assert_eq!(targets_of(command), vec![own(number)], "{command}");
    }
}

#[test]
fn unnamed_or_symbolic_arguments_yield_nothing() {
    for command in [
        "gh pr edit --add-label loom:pr",
        "gh pr comment \"$PR\" --body hi",
        "gh pr comment feature/issue-7 --body hi",
        "gh pr merge $(gh pr list --json number -q '.[0].number')",
        "gh api -X POST repos/{owner}/{repo}/issues/comments/123/reactions -f content=+1",
        "gh api -X POST repos/{owner}/repo/issues/7/labels -f labels[]=x",
        "gh api -X POST graphql -f query=mutation",
        "echo gh pr edit 7 --add-label x",
        "gh issue create --title 'gh issue edit 7'",
    ] {
        assert_eq!(targets_of(command), vec![], "{command}");
    }
}

#[test]
fn compound_commands_yield_every_target() {
    assert_eq!(
        targets_of("gh issue edit 1 --add-label x && gh pr comment 2 -b y; merge-pr.sh 3 | tee l"),
        vec![own(1), own(2), own(3)]
    );
}

#[test]
fn quoted_and_commented_text_is_not_a_command() {
    assert_eq!(
        targets_of("gh pr comment 5 --body 'then; gh pr edit 6 --add-label x' # gh issue edit 8"),
        vec![own(5)]
    );
}

#[test]
fn heredoc_bodies_are_skipped() {
    let command =
        "gh issue comment 5 --body-file - <<-'EOF'\n\tgh pr edit 6 --add-label x\n\tEOF\n\
                   gh pr edit 7 --add-label y";
    assert_eq!(targets_of(command), vec![own(5), own(7)]);
}

#[test]
fn explicit_repositories_are_carried_lowercased() {
    assert_eq!(
        targets_of("gh issue edit 3 -R Other/Repo --add-label x"),
        vec![Target {
            number: 3,
            repo: Some("other/repo".into())
        }]
    );
    // A `-R` contradicting the URL's repository is refused outright.
    assert_eq!(
        targets_of("gh pr edit https://github.com/a/b/pull/3 -R c/d --add-label x"),
        vec![]
    );
    let target = Target {
        number: 3,
        repo: Some("rjwalters/loom".into()),
    };
    assert!(target.in_repo("RJWalters/Loom"));
    assert!(!target.in_repo("2AMLogic/harness-ops"));
}
