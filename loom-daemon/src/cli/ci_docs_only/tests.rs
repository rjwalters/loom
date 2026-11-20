#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::process::Command;

use serde_json::{json, Value};
use tempfile::TempDir;

use super::{classify, proof::Forge, CiDocsOnlyArgs, Event};

const REPO: &str = "acme/workspace";
const WORKFLOW: u64 = 18;
const REVISION: &str = "2026-11-20T19:00:00Z";
const QUEUE_PREFIX: &str = "refs/heads/gh-readonly-queue/main/pr-";

struct Fixture {
    repo: TempDir,
    receipt: TempDir,
    seed: String,
}

impl Fixture {
    fn new() -> Self {
        let repo = tempfile::tempdir().unwrap();
        let receipt = tempfile::tempdir().unwrap();
        let mut fixture = Self {
            repo,
            receipt,
            seed: String::new(),
        };
        fixture.git(&["init", "--quiet", "-b", "main"]);
        fixture.write("WORK_LOG.md", "original log\n");
        fixture.write("WORK_PLAN.md", "original plan\n");
        fixture.write("app.rs", "original code\n");
        fixture.write(".github/workflows/ci.yml", "original CI\n");
        fixture.write(
            ".loom/scripts/ci-docs-only-paths.txt",
            include_str!("../../../../defaults/scripts/ci-docs-only-paths.txt"),
        );
        fixture.seed = fixture.commit("verified push");
        fixture
    }

    fn git(&self, args: &[&str]) -> String {
        git(self.repo.path(), args)
    }

    fn write(&self, path: &str, content: &str) {
        let file = self.repo.path().join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, content).unwrap();
    }

    fn commit(&self, subject: &str) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-qm", subject, "--allow-empty"]);
        self.git(&["rev-parse", "HEAD"])
    }

    fn docs_edit(&self) -> String {
        self.write("WORK_LOG.md", "new entry\n");
        self.commit("docs edit")
    }

    fn merge(&self, branch: &str) -> String {
        self.git(&["merge", "--no-edit", "--no-ff", branch]);
        self.git(&["rev-parse", "HEAD"])
    }

    fn args(&self, event: Event, head: &str, mut payload: Value) -> CiDocsOnlyArgs {
        payload["repository"] = json!({"full_name": REPO, "default_branch": "main"});
        let event_path = self.receipt.path().join("event.json");
        std::fs::write(&event_path, payload.to_string()).unwrap();
        CiDocsOnlyArgs {
            root: self.repo.path().to_owned(),
            event,
            event_path,
            head: head.to_string(),
            repo: REPO.to_string(),
            workflow_id: WORKFLOW,
        }
    }

    fn push(&self, head: &str, before: &str, green: Option<&str>) -> Result<(), &'static str> {
        let args = self.args(
            Event::Push,
            head,
            json!({"ref":"refs/heads/main", "after":head, "before":before}),
        );
        let forge = self.forge(|route| {
            assert_eq!(
                route,
                "actions/workflows/18/runs?event=push&branch=main&status=success&per_page=1"
            );
            green.map(green_run)
        });
        classify(&args, &forge)
    }

    fn forge<'a>(&self, get: impl Fn(&str) -> Option<Value> + 'a) -> Forge<'a> {
        Forge {
            repo: REPO,
            get: Box::new(move |_, route| {
                get(route.strip_prefix("repos/acme/workspace/").unwrap())
            }),
        }
    }

    fn queue(
        &self,
        protected: &str,
        head: &str,
        stated: &str,
        alternate: QueueCase,
    ) -> Result<(), &'static str> {
        let event = json!({"action":"checks_requested", "merge_group":{
            "head_sha":head, "head_ref":format!("{QUEUE_PREFIX}81-{}", self.seed),
            "base_sha":stated, "base_ref":"refs/heads/main"
        }});
        let args = self.args(Event::MergeGroup, head, event);
        let mut detail = json!({"id":45, "target":"branch", "source":REPO, "source_type":"Repository",
            "enforcement":"active", "updated_at":REVISION});
        let mut rules = queue_rules();
        let parent = self.git(&["rev-parse", &format!("{head}^1")]);
        let mut refs = vec![git_ref(&format!("{QUEUE_PREFIX}80-{}", self.seed), &parent)];
        match alternate {
            QueueCase::Speculative => {}
            QueueCase::UnknownParent => refs.clear(),
            QueueCase::PossibleBypass => detail["bypass_actors"] = json!([{"actor_id": 1}]),
            QueueCase::ChangedRevision => detail["updated_at"] = json!("2026-11-20T19:01:00Z"),
            QueueCase::HeadGreen => {
                rules[0]["parameters"]["grouping_strategy"] = json!("HEADGREEN")
            }
            QueueCase::ChangedChecks => {
                rules[1]["parameters"]["required_status_checks"] = json!([])
            }
        }
        let forge = self.forge(|route| match route {
            "git/ref/heads/main" => Some(git_ref("refs/heads/main", protected)),
            "rules/branches/main" => Some(rules.clone()),
            "rulesets/45" => Some(detail.clone()),
            "git/matching-refs/heads/gh-readonly-queue/main/pr-" => Some(json!(refs)),
            _ => panic!("unexpected forge read: {route}"),
        });
        classify(&args, &forge)
    }

    fn attested_queue(&self) -> String {
        self.write(".loom/ci-docs-only-queue.json", &json!({
            "repository":REPO, "workflow_id":WORKFLOW, "ruleset_id":45, "ruleset_updated_at":REVISION,
            "no_bypass":true, "green_seed":self.seed, "required_checks":required_checks(),
        }).to_string());
        self.commit("trusted queue adoption")
    }
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(root)
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.com",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn green_run(head: &str) -> Value {
    json!({"workflow_runs":[{"workflow_id":WORKFLOW, "head_sha":head,
        "event":"push", "head_branch":"main", "status":"completed", "conclusion":"success",
        "repository":{"full_name":REPO}, "head_repository":{"full_name":REPO}}]})
}

fn git_ref(name: &str, sha: &str) -> Value {
    json!({"ref":name, "object":{"sha":sha, "type":"commit"}})
}

fn required_checks() -> Value {
    json!([{"context":"Project Tests", "integration_id":15368}, {"context":"loom/merge-authorization"}])
}

fn queue_rules() -> Value {
    json!([
        {"type":"merge_queue", "ruleset_id":45, "ruleset_source_type":"Repository", "ruleset_source":REPO,
            "parameters":{"grouping_strategy":"ALLGREEN", "merge_method":"MERGE"}},
        {"type":"required_status_checks", "ruleset_id":45, "ruleset_source_type":"Repository", "ruleset_source":REPO,
            "parameters":{"do_not_enforce_on_create":false,"required_status_checks":required_checks()}},
    ])
}

#[derive(Clone, Copy)]
enum QueueCase {
    Speculative,
    PossibleBypass,
    UnknownParent,
    ChangedRevision,
    HeadGreen,
    ChangedChecks,
}

#[test]
fn a_push_compares_against_last_green_not_its_before_sha() {
    let f = Fixture::new();
    let docs = f.docs_edit();
    assert_eq!(f.push(&docs, &f.seed, Some(&f.seed)), Ok(()));
    f.write("app.rs", "red or superseded code\n");
    let red = f.commit("not green");
    f.write("WORK_LOG.md", "new entry after red\n");
    let later = f.commit("another docs edit");
    assert_eq!(f.push(&later, &red, Some(&f.seed)), Err("path_outside_policy"));
    assert_eq!(f.push(&later, &red, None), Err("green_base_unavailable"));
    f.git(&["checkout", "--detach", &red]);
    assert_eq!(f.push(&red, &docs, Some(&later)), Err("base_not_ancestor"));
}

#[test]
fn the_event_and_the_checkout_must_agree_and_git_history_must_be_complete() {
    let f = Fixture::new();
    let head = f.docs_edit();
    let args = f.args(Event::Push, &head, json!({"ref":"refs/heads/other","after":head}));
    assert_eq!(classify(&args, &f.forge(|_| None)), Err("event_mismatch"));
    let args = f.args(Event::Push, &f.seed, json!({"ref":"refs/heads/main","after":f.seed}));
    assert_eq!(classify(&args, &f.forge(|_| None)), Err("history_unavailable"));
    let args = f.args(Event::Push, &head, json!({"ref":"refs/heads/main","after":head}));
    std::fs::write(f.repo.path().join(".git/shallow"), format!("{head}\n")).unwrap();
    assert_eq!(classify(&args, &f.forge(|_| None)), Err("history_unavailable"));
}

#[test]
fn the_green_receipt_must_belong_to_the_requested_workflow_and_repo() {
    let f = Fixture::new();
    let head = f.docs_edit();
    let args = f.args(Event::Push, &head, json!({"ref":"refs/heads/main","after":head}));
    for (field, value) in [
        ("workflow_id", json!(99)),
        ("head_branch", json!("other")),
        ("event", json!("pull_request")),
        ("conclusion", json!("cancelled")),
        ("head_repository", json!({"full_name":"fork/workspace"})),
        ("head_sha", json!("1111111111111111111111111111111111111111")),
    ] {
        let mut runs = green_run(&f.seed);
        runs["workflow_runs"][0][field] = value;
        assert!(classify(&args, &f.forge(|_| Some(runs.clone()))).is_err(), "{field}");
    }
    assert_eq!(
        classify(&args, &f.forge(|_| Some(json!({"workflow_runs":null})))),
        Err("green_base_unavailable")
    );
}

#[test]
fn a_pr_compares_its_tested_merge_tree_to_the_verified_merge_base() {
    let f = Fixture::new();
    f.git(&["checkout", "-qb", "pr"]);
    let branch = f.docs_edit();
    f.git(&["checkout", "-q", "main"]);
    f.write("WORK_PLAN.md", "new target plan\n");
    let target = f.commit("target moved with docs");
    let combined = f.merge("pr");
    let pull = json!({"pull_request":{
        "base":{"sha":target,"ref":"main", "repo":{"full_name":REPO}}, "head":{"sha":branch},
    }});
    let args = f.args(Event::PullRequest, &combined, pull.clone());
    let calls = f.forge(|route| {
        assert!(route.ends_with(&format!("&head_sha={}", f.seed)));
        Some(green_run(&f.seed))
    });
    assert_eq!(classify(&args, &calls), Ok(()));
    assert_eq!(
        classify(&args, &f.forge(|_| Some(green_run(&target)))),
        Err("green_base_unavailable")
    );

    f.git(&["reset", "--hard", &target]);
    f.write("app.rs", "untested target code\n");
    let updated_target = f.commit("target moved with code");
    let combined = f.merge("pr");
    let mut pull = pull;
    pull["pull_request"]["base"]["sha"] = json!(updated_target);
    let args = f.args(Event::PullRequest, &combined, pull);
    assert_eq!(classify(&args, &calls), Err("path_outside_policy"));
}

#[test]
fn a_group_uses_the_verified_first_parent_even_when_speculative() {
    let f = Fixture::new();
    let protected = f.attested_queue();
    f.git(&["checkout", "-qb", "docs"]);
    f.docs_edit();
    f.git(&["checkout", "-qb", "ordinary", &protected]);
    let ordinary = f.merge("docs");
    assert_eq!(f.queue(&protected, &ordinary, &protected, QueueCase::Speculative), Ok(()));

    f.git(&["checkout", "-qb", "first-entry", &protected]);
    f.write("app.rs", "predecessor code\n");
    f.commit("first queue entry");
    f.git(&["checkout", "-qb", "first-group", &protected]);
    let predecessor = f.merge("first-entry");
    let group = f.merge("docs");
    assert_eq!(f.queue(&protected, &group, &protected, QueueCase::Speculative), Ok(()));
    assert_eq!(f.queue(&protected, &group, &predecessor, QueueCase::Speculative), Ok(()));
    for rejection in [
        QueueCase::UnknownParent,
        QueueCase::PossibleBypass,
        QueueCase::ChangedRevision,
        QueueCase::HeadGreen,
        QueueCase::ChangedChecks,
    ] {
        assert_eq!(
            f.queue(&protected, &group, &protected, rejection),
            Err("queue_policy_unproven")
        );
    }
    assert_eq!(
        f.queue(&protected, &group, &group, QueueCase::Speculative),
        Err("queue_base_unproven")
    );
}

#[test]
fn an_unattested_queue_and_changing_predecessor_workflow_run_full_suites() {
    let f = Fixture::new();
    f.git(&["checkout", "-qb", "docs"]);
    f.docs_edit();
    f.git(&["checkout", "-q", "main"]);
    let unattested = f.merge("docs");
    assert_eq!(
        f.queue(&f.seed, &unattested, &f.seed, QueueCase::Speculative),
        Err("queue_policy_unproven")
    );
    f.git(&["reset", "--hard", &f.seed]);
    let protected = f.attested_queue();
    f.git(&["checkout", "-qb", "ci-change"]);
    f.write(".github/workflows/ci.yml", "different CI\n");
    f.commit("change workflow");
    f.git(&["checkout", "-q", "main"]);
    f.merge("ci-change");
    let group = f.merge("docs");
    assert_eq!(
        f.queue(&protected, &group, &protected, QueueCase::Speculative),
        Err("ci_input_changed")
    );
}

#[test]
fn mixed_diffs_mode_changes_renames_and_policy_edits_run_full_suites() {
    let f = Fixture::new();
    let unchanged = f.commit("same tree");
    assert_eq!(f.push(&unchanged, &f.seed, Some(&f.seed)), Err("empty_diff"));
    f.git(&["update-index", "--chmod=+x", "WORK_LOG.md"]);
    f.git(&["commit", "-qm", "mode change"]);
    let exec = f.git(&["rev-parse", "HEAD"]);
    assert_eq!(f.push(&exec, &f.seed, Some(&f.seed)), Err("nonregular_or_nonmodify"));
    f.git(&["reset", "--hard", &f.seed]);
    f.git(&["mv", "WORK_LOG.md", "renamed.md"]);
    let renamed = f.commit("renamed");
    assert_eq!(f.push(&renamed, &f.seed, Some(&f.seed)), Err("nonregular_or_nonmodify"));
    f.git(&["reset", "--hard", &f.seed]);
    std::fs::remove_file(f.repo.path().join("WORK_LOG.md")).unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("WORK_PLAN.md", f.repo.path().join("WORK_LOG.md")).unwrap();
        let symlink = f.commit("changed type");
        assert_eq!(f.push(&symlink, &f.seed, Some(&f.seed)), Err("nonregular_or_nonmodify"));
    }
    f.git(&["reset", "--hard", &f.seed]);
    f.write(".loom/scripts/ci-docs-only-paths.txt", "WORK_PLAN.md\n");
    let narrowed = f.commit("trusted narrowed policy");
    assert_eq!(f.push(&narrowed, &f.seed, Some(&f.seed)), Err("ci_input_changed"));
    let dropped = f.docs_edit();
    assert_eq!(f.push(&dropped, &narrowed, Some(&narrowed)), Err("path_outside_policy"));
    f.write(".loom/scripts/ci-docs-only-paths.txt", "app.rs\n");
    let unsupported = f.commit("unsupported widened policy");
    f.write("app.rs", "new code\n");
    let code = f.commit("code change");
    assert_eq!(f.push(&code, &unsupported, Some(&unsupported)), Err("policy_unavailable"));
}

#[test]
fn a_partial_or_ambiguous_diff_never_classifies_as_docs_only() {
    let first = "1111111111111111111111111111111111111111";
    let second = "2222222222222222222222222222222222222222";
    let good = format!(":100644 100644 {first} {second} M\0WORK_LOG.md\0");
    let policy = ["WORK_LOG.md".to_string()];
    assert_eq!(super::tree::check_diff(good.as_bytes(), &policy), Ok(()));
    for truncated in [&good[..good.len() - 1], &good[..good.len() - 12]] {
        assert!(super::tree::check_diff(truncated.as_bytes(), &policy).is_err());
    }
    let huge = good.repeat(16_384);
    assert!(super::tree::check_diff(huge.as_bytes(), &policy).is_err());
    let newline = good.replace("WORK_LOG.md", "WORK_LOG.md\napp.rs");
    assert!(super::tree::check_diff(newline.as_bytes(), &policy).is_err());
}

#[test]
fn only_the_clean_event_checkout_can_reuse_the_green_push() {
    let f = Fixture::new();
    let head = f.docs_edit();
    assert_eq!(f.push(&head, &f.seed, Some(&f.seed)), Ok(()));

    let event = f.args(Event::Push, &head, json!({"ref": "refs/heads/main", "after": head}));
    let unavailable = f.forge(|_| panic!("an unverified checkout does not consult the forge"));
    f.write("app.rs", "modified worktree code\n");
    assert_eq!(classify(&event, &unavailable), Err("checkout_unverified"));
    f.git(&["add", "app.rs"]);
    assert_eq!(classify(&event, &unavailable), Err("checkout_unverified"));

    f.git(&[
        "restore",
        "--staged",
        "--worktree",
        "--source=HEAD",
        "app.rs",
    ]);
    assert_eq!(f.push(&head, &f.seed, Some(&f.seed)), Ok(()));
    for flag in ["--assume-unchanged", "--skip-worktree"] {
        f.git(&["update-index", flag, "app.rs"]);
        f.write("app.rs", "hidden worktree change\n");
        assert_eq!(classify(&event, &unavailable), Err("checkout_unverified"));
        let clear_flag = format!("--no-{}", &flag[2..]);
        f.git(&["update-index", &clear_flag, "app.rs"]);
        f.git(&["restore", "app.rs"]);
    }

    std::fs::write(f.repo.path().join(".git/index"), b"unreadable index").unwrap();
    assert_eq!(classify(&event, &unavailable), Err("checkout_unverified"));
}

#[cfg(unix)]
#[test]
fn launcher_degrades_only_for_the_older_daemons_unsupported_command() {
    use std::os::unix::fs::PermissionsExt;

    let scratch = tempfile::tempdir().unwrap();
    let daemon = scratch.path().join("loom-daemon-fixture");
    let launcher =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../defaults/scripts/ci-docs-only.sh");
    for (body, code, stdout, diagnostic) in [
        (
            "printf '%s\\n' docs_only=true; printf '%s\\n' allowed_docs >&2",
            0,
            "docs_only=true\n",
            "allowed_docs",
        ),
        (
            "printf '%s\\n' \"error: unrecognized subcommand 'ci-docs-only'\" >&2; exit 2",
            0,
            "docs_only=false\n",
            "reason=command_unavailable",
        ),
        (
            "printf '%s\\n' 'error: unexpected argument' >&2; exit 2",
            2,
            "",
            "error: unexpected argument",
        ),
        (
            "printf '%s\\n' \"error: unrecognized subcommand 'other'\" >&2; exit 2",
            2,
            "",
            "error: unrecognized subcommand 'other'",
        ),
        (
            "printf '%s\\n' 'error: classifier crashed' >&2; exit 1",
            1,
            "",
            "error: classifier crashed",
        ),
    ] {
        std::fs::write(&daemon, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&daemon, std::fs::Permissions::from_mode(0o700)).unwrap();
        let output = Command::new("bash")
            .arg(&launcher)
            .env("LOOM_DAEMON_BIN", &daemon)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(code), "{body}");
        assert_eq!(output.stdout, stdout.as_bytes(), "{body}");
        assert!(String::from_utf8_lossy(&output.stderr).contains(diagnostic), "{body}");
    }
}
