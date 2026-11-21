//! Repository-transfer regressions (#14158), run by the existing Rust CI suite.
//! Reuse the native audit's parser/evaluator; no production command or shell
//! checker is needed to pin the behavior of this repository's five guards.

use super::expr::{self, Context, Truth, Value};
use super::yaml::{self, Node};

const CANONICAL: &str = "2AMLogic/loom";
const FORMER: &str = "rjwalters/loom";
const TAG: &str = "v0.19.1311";

fn workflows() -> (Node, Node) {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    let read = |name| yaml::parse(&std::fs::read_to_string(root.join(name)).unwrap()).unwrap();
    (read(".github/workflows/ci.yml"), read(".github/workflows/release.yml"))
}

fn at<'a>(node: &'a Node, path: &[&str]) -> &'a Node {
    path.iter()
        .fold(node, |n, key| n.get(key).unwrap_or_else(|| panic!("missing {key}")))
}

fn push_step(job: &Node) -> &Node {
    let steps: Vec<_> = at(job, &["steps"])
        .items()
        .iter()
        .filter(|step| {
            step.get("with")
                .and_then(|with| with.get("push"))
                .and_then(Node::as_str)
                == Some("true")
        })
        .collect();
    assert_eq!(steps.len(), 1, "each image job has one publishing step");
    steps[0]
}

fn guards(ci: &Node, release: &Node) -> Vec<(&'static str, String)> {
    let mut guards = vec![
        ("worker-buildcache", at(ci, &["jobs", "worker-buildcache", "if"])),
        (
            "release-stamp",
            at(release, &["jobs", "build-daemon", "env", "LOOM_RELEASE_BUILD_TAG"]),
        ),
    ];
    for job in [
        "build-worker-image",
        "build-session-image",
        "build-native-image",
    ] {
        guards.push((job, at(push_step(at(release, &["jobs", job])), &["if"])));
    }
    guards
        .into_iter()
        .map(|(id, condition)| (id, condition.as_str().unwrap().into()))
        .collect()
}

struct Scenario<'a> {
    repo: &'a str,
    event: &'a str,
    git_ref: &'a str,
    publish: &'a str,
    build: &'a str,
    changes: &'a str,
    docker: &'a str,
}

impl Default for Scenario<'_> {
    fn default() -> Self {
        Self {
            repo: CANONICAL,
            event: "push",
            git_ref: "refs/heads/main",
            publish: "true",
            build: "success",
            changes: "success",
            docker: "true",
        }
    }
}

impl Context for Scenario<'_> {
    fn lookup(&self, path: &[String]) -> Value {
        Value::Str(
            match path.join(".").as_str() {
                "github.repository" => self.repo,
                "github.event_name" => self.event,
                "github.ref" => self.git_ref,
                "needs.resolve.outputs.publish" => self.publish,
                "needs.resolve.outputs.tag" => TAG,
                "needs.release-build.result" => self.build,
                "needs.changes.result" => self.changes,
                "needs.changes.outputs.docker" => self.docker,
                _ => return Value::Unknown,
            }
            .into(),
        )
    }
}

fn eval(condition: &str, scenario: &Scenario<'_>) -> Value {
    expr::parse_condition(condition).unwrap().eval(scenario)
}

fn canonical_rejections(guards: &[(&'static str, String)]) -> Vec<&'static str> {
    guards
        .iter()
        .filter_map(|(id, condition)| {
            (eval(condition, &Scenario::default()).truth() != Truth::True).then_some(*id)
        })
        .collect()
}

#[test]
fn workflow_only_changes_select_the_native_regression_suite() {
    let (ci, _) = workflows();
    let filters = yaml::parse(
        at(&ci, &["jobs", "changes", "env", "PR_PATH_FILTERS"])
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let backend: Vec<_> = at(&filters, &["backend"])
        .items()
        .iter()
        .filter_map(Node::as_str)
        .collect();
    for workflow in [".github/workflows/ci.yml", ".github/workflows/release.yml"] {
        assert!(backend.contains(&workflow), "{workflow} must run the native guard tests");
    }
}

#[test]
fn all_five_actual_guards_enable_the_canonical_repository_and_refuse_forks() {
    let (ci, release) = workflows();
    let guards = guards(&ci, &release);
    assert_eq!(guards.len(), 5);
    assert_eq!(canonical_rejections(&guards), Vec::<&str>::new());
    for repo in [FORMER, "contributor/loom", "2AMLogic/loom-fork"] {
        for (id, condition) in &guards {
            assert_eq!(
                eval(
                    condition,
                    &Scenario {
                        repo,
                        ..Scenario::default()
                    }
                )
                .truth(),
                Truth::False,
                "{id}: {repo}"
            );
        }
    }
    assert_eq!(eval(&guards[1].1, &Scenario::default()), Value::Str(TAG.into()));
}

#[test]
fn each_former_owner_mutation_rejects_only_the_changed_guard() {
    let (ci, release) = workflows();
    let baseline = guards(&ci, &release);
    // Establish a valid five-guard baseline first. Missing unrelated guards
    // cannot make this negative test pass, unlike the removed shell fixture.
    assert!(canonical_rejections(&baseline).is_empty());
    for index in 0..baseline.len() {
        let mut changed = baseline.clone();
        changed[index].1 = changed[index].1.replacen(CANONICAL, FORMER, 1);
        assert_ne!(changed[index].1, baseline[index].1);
        assert_eq!(canonical_rejections(&changed), vec![baseline[index].0]);
    }
}

#[test]
fn buildcache_keeps_push_main_build_and_change_detection_conditions() {
    let (ci, release) = workflows();
    let condition = &guards(&ci, &release)[0].1;
    for scenario in [
        Scenario {
            event: "pull_request",
            ..Scenario::default()
        },
        Scenario {
            event: "merge_group",
            ..Scenario::default()
        },
        Scenario {
            git_ref: "refs/heads/feature",
            ..Scenario::default()
        },
        Scenario {
            build: "failure",
            ..Scenario::default()
        },
        Scenario {
            build: "skipped",
            ..Scenario::default()
        },
        Scenario {
            docker: "false",
            ..Scenario::default()
        },
    ] {
        assert_eq!(eval(condition, &scenario).truth(), Truth::False);
    }
    // A failed detector must not suppress the cache build after a good build.
    assert_eq!(
        eval(
            condition,
            &Scenario {
                changes: "failure",
                docker: "false",
                ..Scenario::default()
            }
        )
        .truth(),
        Truth::True
    );
    let writers: Vec<_> = at(&ci, &["jobs"])
        .entries()
        .iter()
        .filter_map(|job| {
            (job.value
                .get("permissions")
                .and_then(|p| p.get("packages"))
                .and_then(Node::as_str)
                == Some("write"))
            .then_some(job.key.as_str())
        })
        .collect();
    assert_eq!(writers, vec!["worker-buildcache"]);
    assert_ne!(
        ci.get("permissions")
            .and_then(|p| p.get("packages"))
            .and_then(Node::as_str),
        Some("write")
    );
}

#[test]
fn release_guards_preserve_publish_decisions_and_exclude_pr_and_merge_group_events() {
    let (ci, release) = workflows();
    let guards = guards(&ci, &release);
    for event in ["release", "workflow_run", "workflow_dispatch"] {
        assert!(at(&release, &["on"]).get(event).is_some());
        for publish in ["true", "false", "skip", ""] {
            let scenario = Scenario {
                event,
                publish,
                ..Scenario::default()
            };
            for (id, condition) in &guards[1..] {
                assert_eq!(
                    eval(condition, &scenario).truth(),
                    if publish == "true" {
                        Truth::True
                    } else {
                        Truth::False
                    },
                    "{id}: {event}/{publish}"
                );
            }
            assert_eq!(
                eval(&guards[1].1, &scenario),
                Value::Str(if publish == "true" { TAG } else { "" }.into())
            );
        }
    }
    for event in ["pull_request", "pull_request_target", "merge_group"] {
        assert!(
            at(&release, &["on"]).get(event).is_none(),
            "release credentials must not be available on {event}"
        );
    }
}

#[test]
fn images_publish_under_the_lowercased_repository_owner_namespace() {
    let (ci, release) = workflows();
    assert_eq!(
        at(&ci, &["jobs", "worker-buildcache", "env", "BUILDCACHE_REF"]).as_str(),
        Some("ghcr.io/2amlogic/loom-worker:buildcache")
    );
    for (job, image) in [
        ("build-worker-image", "loom-worker"),
        ("build-session-image", "loom-worker-session"),
        ("build-native-image", "loom-worker-native"),
    ] {
        let tags = at(push_step(at(&release, &["jobs", job])), &["with", "tags"])
            .as_str()
            .unwrap();
        assert_eq!(tags.lines().count(), 2);
        // The namespace is derived from the repository owner (#14419), never
        // a hardcoded account: the pre-transfer `rjwalters` path is denied.
        assert!(tags
            .lines()
            .all(|tag| tag
                .starts_with(&format!("ghcr.io/${{{{ steps.ghcr.outputs.owner }}}}/{image}:"))));
        let owner_step = at(at(&release, &["jobs", job]), &["steps"])
            .items()
            .iter()
            .find(|step| step.get("id").and_then(Node::as_str) == Some("ghcr"))
            .unwrap_or_else(|| panic!("{job} must resolve the GHCR namespace"));
        assert!(
            at(owner_step, &["run"])
                .as_str()
                .unwrap()
                .contains("${GITHUB_REPOSITORY_OWNER,,}"),
            "{job}: the owner must be lower-cased"
        );
    }
}
