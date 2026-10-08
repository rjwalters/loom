use super::*;

fn take(label: &str, on: &str, number: u32) -> ClaimEdit {
    ClaimEdit::Take(ClaimBreadcrumb {
        label: label.to_string(),
        on: on.to_string(),
        number,
        at: None,
    })
}

#[test]
fn the_claim_command_of_each_role_is_recognised() {
    assert_eq!(
        parse_claim_edits(r#"gh pr edit 599 --add-label "loom:reviewing""#),
        vec![take("loom:reviewing", "pr", 599)]
    );
    assert_eq!(
        parse_claim_edits("gh pr edit 588 -R o/r --add-label=loom:treating"),
        vec![take("loom:treating", "pr", 588)]
    );
    assert_eq!(
        parse_claim_edits("gh issue edit '#42' --add-label loom:curating,enhancement"),
        vec![take("loom:curating", "issue", 42)]
    );
    // A number held in a variable assigned in the same command.
    assert_eq!(
        parse_claim_edits("N=77; gh pr edit $N --add-label loom:reviewing && echo ok"),
        vec![take("loom:reviewing", "pr", 77)]
    );
}

#[test]
fn a_release_and_a_swap_are_recognised_in_order() {
    assert_eq!(
        parse_claim_edits(
            r#"gh pr edit 599 --remove-label "loom:reviewing" --add-label "loom:pr""#
        ),
        vec![ClaimEdit::Release {
            label: "loom:reviewing".to_string(),
            number: 599
        }]
    );
}

#[test]
fn commands_that_take_no_claim_are_ignored() {
    for command in [
        "gh pr edit 5 --add-label loom:pr",
        "gh pr view 5 --json labels",
        "gh pr edit $UNKNOWN --add-label loom:reviewing",
        "echo gh pr edit 5",
        "git commit -m 'add loom:reviewing docs'",
    ] {
        assert_eq!(parse_claim_edits(command), Vec::new(), "{command}");
    }
}

fn payload(command: &str) -> serde_json::Value {
    serde_json::json!({ "tool_name": "Bash", "tool_input": { "command": command } })
}

#[test]
fn the_hook_records_on_the_post_event_and_clears_on_release() {
    let dir = tempfile::tempdir().unwrap();
    let claim = payload("gh pr edit 599 --add-label loom:reviewing");
    // Claude delivers a post event: the pre event records nothing.
    observe(dir.path(), "PreToolUse", &claim, true);
    assert_eq!(read(dir.path()), None);
    observe(dir.path(), "PostToolUse", &claim, true);
    let recorded = read(dir.path()).unwrap();
    assert_eq!(
        (recorded.label.as_str(), recorded.on.as_str(), recorded.number),
        ("loom:reviewing", "pr", 599)
    );
    assert!(recorded.at.is_some());
    // Releasing another PR's label leaves it; releasing this one clears it.
    let other = payload("gh pr edit 600 --remove-label loom:reviewing");
    observe(dir.path(), "PostToolUse", &other, true);
    assert!(read(dir.path()).is_some());
    let release = payload("gh pr edit 599 --remove-label loom:reviewing --add-label loom:pr");
    observe(dir.path(), "PostToolUse", &release, true);
    assert_eq!(read(dir.path()), None);
}

#[test]
fn a_runtime_with_no_post_event_records_on_the_pre_event() {
    let dir = tempfile::tempdir().unwrap();
    let claim = payload("gh pr edit 12 --add-label loom:treating");
    observe(dir.path(), "PreToolUse", &claim, false);
    assert_eq!(read(dir.path()).unwrap().number, 12);
}

#[test]
fn posting_a_verdict_clears_a_review_claim() {
    let dir = tempfile::tempdir().unwrap();
    observe(
        dir.path(),
        "PostToolUse",
        &payload("gh pr edit 9 --add-label loom:reviewing"),
        true,
    );
    observe(
        dir.path(),
        "PostToolUse",
        &payload("./.loom/scripts/post-verdict.sh approve 9 --body-file v.md"),
        true,
    );
    assert_eq!(read(dir.path()), None);
}

#[test]
fn a_breadcrumb_round_trips_through_the_manifest_and_is_carried_on_resume() {
    let from = tempfile::tempdir().unwrap();
    let to = tempfile::tempdir().unwrap();
    let claim = ClaimBreadcrumb {
        label: "loom:treating".to_string(),
        on: "pr".to_string(),
        number: 31,
        at: None,
    };
    write(from.path(), &claim).unwrap();
    carry(from.path(), to.path());
    assert_eq!(read(to.path()), Some(claim.clone()));
    assert_eq!(ClaimBreadcrumb::from_manifest(&claim.manifest_value()), Some(claim));
    // A sweep's claim has no number and is not a role breadcrumb.
    let sweep = serde_json::json!({ "label": "loom:building", "on": "issue" });
    assert_eq!(ClaimBreadcrumb::from_manifest(&sweep), None);
}
