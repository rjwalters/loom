//! Target extraction (Issue #9168): recorded-transcript fixtures plus the
//! command-shape edge cases behind them.

use super::*;
use std::path::{Path, PathBuf};

const OWN: &str = "rjwalters/loom";
/// Where the recorded ticks' sessions started (lexical only: it need not exist).
const ROOT: &str = "/work/loom";

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
    own_repo_numbers(&targets, OWN, Path::new(ROOT))
}

/// The targets of one command in a fresh session.
fn targets_of(command: &str) -> Vec<Target> {
    session_targets(&[command])
}

/// The targets of several tool calls of one transcript, in order.
fn session_targets(commands: &[&str]) -> Vec<Target> {
    let mut session = Session::default();
    let mut out = BTreeSet::new();
    for command in commands {
        collect(command, &mut session, &mut out);
    }
    out.into_iter().collect()
}

fn own(number: u32) -> Target {
    Target::own(number)
}

fn named(number: u32, repo: &str) -> Target {
    Target {
        number,
        repo: Some(repo.into()),
        cwd: None,
    }
}

fn in_dir(number: u32, dir: &str) -> Target {
    Target {
        number,
        repo: None,
        cwd: Some(dir.into()),
    }
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
        vec![named(3, "other/repo")]
    );
    // A `-R` contradicting the URL's repository is refused outright.
    assert_eq!(
        targets_of("gh pr edit https://github.com/a/b/pull/3 -R c/d --add-label x"),
        vec![]
    );
    let target = named(3, "rjwalters/loom");
    assert!(target.in_repo("RJWalters/Loom", Path::new(ROOT)));
    assert!(!target.in_repo("2AMLogic/harness-ops", Path::new(ROOT)));
}

// ------------------------------------------------------------------------
// #9180: heredocs in quoted substitutions, glued heredocs, GH_REPO and cd.
// ------------------------------------------------------------------------

#[test]
fn a_quoted_heredoc_body_with_odd_quotes_is_not_parsed() {
    // The dominant verdict form, with an odd number of `"` and markdown
    // table rows naming write commands: only the real comment and relabel.
    assert_eq!(fixture_targets("judge_quoted_heredoc.jsonl"), (vec![9301, 9302], 0));
}

#[test]
fn a_glued_or_quoted_heredoc_body_is_not_parsed() {
    assert_eq!(fixture_targets("builder_glued_heredoc.jsonl"), (vec![9310, 9311, 9312], 0));
}

#[test]
fn another_repo_or_checkout_selected_outside_flags_is_refused() {
    // GH_REPO (prefix and exported), `cd` out of the root, an unknowable
    // `cd`, and GH_HOST: only the commands provably acting on this repo.
    assert_eq!(fixture_targets("champion_other_repo.jsonl"), (vec![9320, 9321, 9322, 9323], 3));
}

#[test]
fn substitution_contents_are_commands_and_their_words_name_nothing() {
    for (command, expected) in [
        ("echo \"merged: $(gh pr merge 5 --squash)\"", vec![own(5)]),
        ("out=`gh issue close 6`", vec![own(6)]),
        ("gh pr comment \"$(echo 5)\" --body hi", vec![]),
        ("gh pr comment $(echo 5) --body hi", vec![]),
        (
            "gh pr comment 5 --body \"$(cat <<'EOF'\nsay \"hi\n)\ngh pr edit 6\nEOF\n)\"",
            vec![own(5)],
        ),
        (
            "gh pr comment 5 --body \"$(printf '%s' \"a) b\")\"; gh pr edit 7",
            vec![own(5), own(7)],
        ),
    ] {
        assert_eq!(targets_of(command), expected, "{command}");
    }
}

#[test]
fn every_heredoc_spelling_is_skipped() {
    for command in [
        "cat<<EOF >/tmp/b\ngh pr edit 6\nEOF\ngh pr edit 7",
        "cat<<-EOF\n\tgh pr edit 6\n\tEOF\ngh pr edit 7",
        "cat <<\"EOF\"\ngh pr edit 6\nEOF\ngh pr edit 7",
        "cat <<\\EOF\ngh pr edit 6\nEOF\ngh pr edit 7",
        "cat <<'END NOTE'\ngh pr edit 6\nEND NOTE\ngh pr edit 7",
        "x=$(cat<<'EOF'\ngh pr edit 6 \"\nEOF\n); gh pr edit 7",
        "cat <<A <<B\ngh pr edit 6\nA\ngh pr edit 8\nB\ngh pr edit 7",
    ] {
        assert_eq!(targets_of(command), vec![own(7)], "{command:?}");
    }
    // A body without its terminator hides the rest — never exposes it.
    assert_eq!(targets_of("cat <<EOF\ngh pr edit 6\nEOF)\ngh pr edit 7"), vec![]);
    // `<<<` is a here-string, not a here-document.
    assert_eq!(
        targets_of("gh pr comment 5 --body-file - <<< ok\ngh pr edit 7"),
        vec![own(5), own(7)]
    );
}

#[test]
fn gh_repo_selects_the_repository_like_a_flag() {
    assert_eq!(
        targets_of("GH_REPO=Other/Repo gh pr edit 5 --add-label x"),
        vec![named(5, "other/repo")]
    );
    assert_eq!(targets_of("env GH_REPO=github.com/o/r gh pr edit 5"), vec![named(5, "o/r")]);
    // `-R` wins over GH_REPO, as in gh.
    assert_eq!(
        targets_of("GH_REPO=o/r gh pr edit 5 -R rjwalters/loom"),
        vec![named(5, "rjwalters/loom")]
    );
    // A prefix lasts one command; a bare or exported assignment the session.
    assert_eq!(
        targets_of("GH_REPO=o/r gh pr edit 5; gh pr edit 6"),
        vec![named(5, "o/r"), own(6)]
    );
    for set in [
        "GH_REPO=o/r",
        "export GH_REPO=o/r",
        "declare -x GH_REPO=o/r",
    ] {
        assert_eq!(session_targets(&[set, "gh pr edit 6"]), vec![named(6, "o/r")], "{set}");
    }
    assert_eq!(
        session_targets(&["export GH_REPO=o/r", "unset GH_REPO", "gh pr edit 6"]),
        vec![own(6)]
    );
    // `env VAR=x` alone only prints: nothing persists.
    assert_eq!(session_targets(&["env GH_REPO=o/r", "gh pr edit 6"]), vec![own(6)]);
    // Symbolic, host-qualified or otherwise unreadable scope: no target.
    for command in [
        "GH_REPO=$REPO gh pr edit 5",
        "GH_REPO=\"$(git remote get-url origin)\" gh pr edit 5",
        "GH_REPO=ghe.example.com/o/r gh pr edit 5",
        "GH_HOST=ghe.example.com gh pr edit 5 -R rjwalters/loom",
        "GIT_DIR=/else/.git gh pr edit 5",
        "export GIT_WORK_TREE=/else && merge-pr.sh 5",
    ] {
        assert_eq!(targets_of(command), vec![], "{command}");
    }
    assert_eq!(targets_of("GH_HOST=github.com gh pr edit 5"), vec![own(5)]);
}

#[test]
fn cd_moves_unqualified_targets_but_not_named_ones() {
    assert_eq!(targets_of("cd ../other && gh pr edit 5"), vec![in_dir(5, "../other")]);
    assert_eq!(
        session_targets(&["cd .loom/worktrees/issue-7", "cd sub", "merge-pr.sh 7"]),
        vec![in_dir(7, ".loom/worktrees/issue-7/sub")]
    );
    assert_eq!(targets_of("cd -P /abs/x && gh pr edit 5"), vec![in_dir(5, "/abs/x")]);
    // An explicit repository does not depend on the directory.
    assert_eq!(
        targets_of("cd ../other && gh pr edit 5 -R rjwalters/loom"),
        vec![named(5, "rjwalters/loom")]
    );
    // Unknowable directories drop every unqualified target until an
    // absolute `cd` makes the directory known again.
    for cd in [
        "cd",
        "cd -",
        "cd ~/x",
        "cd $DIR",
        "cd \"$(git rev-parse --show-toplevel)\"",
        "popd",
        "pushd +1",
    ] {
        assert_eq!(
            session_targets(&[cd, "gh pr edit 5", "cd sub && gh pr edit 6"]),
            vec![],
            "{cd}"
        );
        assert_eq!(
            session_targets(&[cd, "cd /work/loom", "gh pr edit 5"]),
            vec![in_dir(5, "/work/loom")],
            "{cd}"
        );
    }
}

#[test]
fn a_directory_is_in_the_root_only_below_it_and_outside_other_checkouts() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    for dir in [".loom/worktrees/issue-7/.git", "vendor/other/.git", "src"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    let root_str = root.to_str().unwrap();
    for (dir, inside) in [
        (".", true),
        ("src/../src", true),
        (".loom/worktrees/issue-7", true),
        (".loom/worktrees/issue-7/loom-daemon", true),
        (root_str, true),
        ("vendor", true),
        ("vendor/other", false),
        ("vendor/other/src", false),
        ("..", false),
        ("../other", false),
        ("src/../../x", false),
        ("/elsewhere", false),
    ] {
        assert_eq!(in_dir(1, dir).in_repo(OWN, root), inside, "{dir}");
    }
}
