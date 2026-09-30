use super::probe::{classify_repo_permissions, Permission, PermissionProbe};
use super::target::{gh_target, parse_remote_config, parse_remote_url};
use super::*;
use std::cell::Cell;

// ---- gh's base-repo resolution --------------------------------------------

#[test]
fn remote_urls_parse_in_every_common_shape() {
    for (url, host, nwo) in [
        ("git@github.com:acme/widgets.git", "github.com", "acme/widgets"),
        ("git@github.com-work:acme/widgets", "github.com-work", "acme/widgets"),
        ("https://github.com/acme/widgets.git", "github.com", "acme/widgets"),
        ("https://x-access-token@github.com/acme/widgets/", "github.com", "acme/widgets"),
        ("ssh://git@github.com:22/acme/widgets.git", "github.com", "acme/widgets"),
        ("https://GHE.example.com/acme/widgets", "ghe.example.com", "acme/widgets"),
    ] {
        assert_eq!(parse_remote_url(url), Some((host.to_string(), nwo.to_string())), "{url}");
    }
    for bad in [
        "/srv/git/widgets.git",
        "file:///srv/widgets",
        "https://github.com/acme",
        "",
    ] {
        assert_eq!(parse_remote_url(bad), None, "{bad}");
    }
}

fn remotes(config: &str) -> Vec<super::target::Remote> {
    parse_remote_config(config)
}

const FORK: &str = "remote.origin.url https://github.com/me/widgets.git\n\
     remote.origin.fetch +refs/heads/*:refs/remotes/origin/*\n\
     remote.upstream.url https://github.com/acme/widgets.git\n";

#[test]
fn an_upstream_remote_outranks_origin_as_gh_does() {
    let t = gh_target(&remotes(FORK), None).unwrap();
    assert_eq!(t.nwo, "acme/widgets");
    assert_eq!(t.via, "remote `upstream`");
}

#[test]
fn github_outranks_origin_and_other_names_do_not() {
    let r = remotes(
        "remote.fork.url git@github.com:me/w.git\nremote.origin.url git@github.com:acme/w.git\n",
    );
    assert_eq!(gh_target(&r, None).unwrap().nwo, "acme/w", "`fork` ranks below origin");
    let r = remotes(
        "remote.origin.url git@github.com:acme/w.git\nremote.github.url git@github.com:x/w.git\n",
    );
    assert_eq!(gh_target(&r, None).unwrap().nwo, "x/w");
}

#[test]
fn a_set_default_pin_and_gh_repo_override_the_ranking() {
    let pinned = format!("{FORK}remote.origin.gh-resolved base\n");
    assert_eq!(gh_target(&remotes(&pinned), None).unwrap().nwo, "me/widgets");
    let named = format!("{FORK}remote.upstream.gh-resolved me/widgets\n");
    assert_eq!(gh_target(&remotes(&named), None).unwrap().nwo, "me/widgets");
    let t = gh_target(&remotes(FORK), Some("github.com/other/thing")).unwrap();
    assert_eq!(t.nwo, "other/thing");
}

#[test]
fn remotes_on_another_host_are_not_candidates() {
    let r = remotes(
        "remote.origin.url git@github.com:acme/w.git\n\
         remote.upstream.url https://gitlab.com/else/w.git\n",
    );
    assert_eq!(gh_target(&r, None).unwrap().nwo, "acme/w");
    assert_eq!(gh_target(&[], None), None);
}

// ---- the decision ----------------------------------------------------------

struct FakeProbe {
    answer: Permission,
    calls: Cell<u32>,
}

impl FakeProbe {
    fn new(answer: Permission) -> Self {
        Self {
            answer,
            calls: Cell::new(0),
        }
    }
}

impl PermissionProbe for FakeProbe {
    fn permission(&self, _repo: &str) -> Permission {
        self.calls.set(self.calls.get() + 1);
        self.answer.clone()
    }
}

fn target(nwo: &str) -> Option<GhTarget> {
    Some(GhTarget {
        nwo: nwo.into(),
        via: "remote `upstream`".into(),
    })
}

fn run(
    t: Option<GhTarget>,
    explicit: bool,
    origin: Option<&str>,
    managed: &[&str],
    probe: &FakeProbe,
) -> Verdict {
    let m = |r: &str| managed.iter().any(|x| x.eq_ignore_ascii_case(r));
    decide(
        &Inputs {
            target: t,
            explicit,
            origin,
            managed: &m,
        },
        probe,
    )
}

#[test]
fn a_managed_writable_origin_is_allowed_case_insensitively() {
    let p = FakeProbe::new(Permission::Write);
    assert_eq!(
        run(target("Acme/Widgets"), false, Some("acme/widgets"), &["acme/widgets"], &p),
        Verdict::Allow("Acme/Widgets".into())
    );
}

#[test]
fn an_upstream_target_is_refused_before_any_probe() {
    let p = FakeProbe::new(Permission::Write);
    let v = run(target("acme/widgets"), false, Some("me/widgets"), &["acme/widgets"], &p);
    let Verdict::Deny(why) = v else {
        panic!("upstream must be refused")
    };
    assert!(why.contains("not its origin me/widgets"), "{why}");
    assert!(why.contains("gh repo set-default me/widgets"), "{why}");
    assert_eq!(p.calls.get(), 0, "no forge call for a mis-targeted checkout");
}

#[test]
fn an_unmanaged_repo_is_refused_even_with_write_access() {
    let p = FakeProbe::new(Permission::Write);
    let v = run(target("other/repo"), true, Some("acme/widgets"), &["acme/widgets"], &p);
    assert!(
        matches!(&v, Verdict::Deny(w) if w.contains("not a repository this installation manages"))
    );
    assert_eq!(p.calls.get(), 0);
}

#[test]
fn read_access_and_an_unverifiable_probe_both_fail_closed() {
    let read = FakeProbe::new(Permission::Insufficient("repository role `pull`".into()));
    let v = run(target("acme/widgets"), false, Some("acme/widgets"), &["acme/widgets"], &read);
    assert!(matches!(&v, Verdict::Deny(w) if w.contains("cannot write") && w.contains("pull")));
    let down = FakeProbe::new(Permission::Unknown("HTTP 502".into()));
    let v = run(target("acme/widgets"), false, Some("acme/widgets"), &["acme/widgets"], &down);
    assert!(matches!(&v, Verdict::Deny(w) if w.contains("could not verify")));
}

#[test]
fn no_target_or_no_origin_is_refused() {
    let p = FakeProbe::new(Permission::Write);
    assert!(!run(None, false, None, &[], &p).is_allowed());
    assert!(!run(target("acme/widgets"), false, None, &["acme/widgets"], &p).is_allowed());
}

#[test]
fn repo_permissions_classify_user_roles() {
    assert_eq!(classify_repo_permissions(r#"{"push":true}"#), Some(Permission::Write));
    assert_eq!(
        classify_repo_permissions(r#"{"maintain":true,"push":false}"#),
        Some(Permission::Write)
    );
    assert_eq!(
        classify_repo_permissions(r#"{"admin":false,"push":false,"triage":true,"pull":true}"#),
        Some(Permission::Insufficient("repository role `triage`".into()))
    );
    // An App installation token's all-false shape is not a verdict on its own.
    assert_eq!(
        classify_repo_permissions(r#"{"admin":false,"maintain":false,"pull":false,"push":false}"#),
        Some(Permission::Insufficient("repository role `none`".into()))
    );
    assert_eq!(classify_repo_permissions("not json"), None);
}

// ---- a real checkout -------------------------------------------------------

fn git(dir: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "git {args:?}");
}

#[test]
fn a_fork_checkout_with_an_upstream_remote_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    git(
        dir.path(),
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/me/widgets.git",
        ],
    );
    git(
        dir.path(),
        &[
            "remote",
            "add",
            "upstream",
            "https://github.com/acme/widgets.git",
        ],
    );
    std::fs::create_dir(dir.path().join(".loom")).unwrap();
    test_override::real(true);
    let v = root_writable(dir.path());
    test_override::real(false);
    let Verdict::Deny(why) = v else {
        panic!("a write resolved through `upstream` must be refused, got {v:?}")
    };
    assert!(
        why.contains("acme/widgets") && why.contains("not its origin me/widgets"),
        "{why}"
    );
}

#[test]
fn the_cache_answers_repeat_questions_without_a_second_probe() {
    let dir = tempfile::tempdir().unwrap();
    // SAFETY-free: the variable is read only by this module's cache.
    std::env::set_var("LOOM_WRITE_SCOPE_CACHE_DIR", dir.path().join("c"));
    probe::clear_memory();
    let cached = probe::Cached {
        inner: FakeProbe::new(Permission::Write),
        key_dir: Some(dir.path().join("cred-a")),
    };
    assert_eq!(cached.permission("acme/cache-test"), Permission::Write);
    assert_eq!(cached.permission("acme/cache-test"), Permission::Write);
    assert_eq!(cached.inner.calls.get(), 1, "memory hit");
    probe::clear_memory();
    assert_eq!(cached.permission("acme/cache-test"), Permission::Write);
    assert_eq!(cached.inner.calls.get(), 1, "disk hit");
    let other = probe::Cached {
        inner: FakeProbe::new(Permission::Insufficient("x".into())),
        key_dir: Some(dir.path().join("cred-b")),
    };
    assert!(matches!(other.permission("acme/cache-test"), Permission::Insufficient(_)));
    assert_eq!(other.inner.calls.get(), 1, "a different credential probes again");
    std::env::remove_var("LOOM_WRITE_SCOPE_CACHE_DIR");
}

// ---- structural: every daemon write path is scoped -------------------------

/// How a reviewed file's forge writes are scoped.
enum Scope {
    /// Calls `write_scope::` itself before writing.
    Gated,
    /// Reached only through the named file, which is `Gated`.
    Via(&'static str, &'static str),
    /// Matches the pattern but is not a forge write.
    NotAWrite(&'static str),
}

/// Structural guard (#9548): a daemon source file that issues a forge write
/// (a comment, a label edit, a REST mutation, a GraphQL mutation) must be
/// reviewed into this list, and a file that claims to be gated must actually
/// call `write_scope`. A new write path therefore cannot silently skip the
/// managed-repo + WRITE check.
#[test]
fn daemon_write_paths_are_scoped() {
    use Scope::{Gated, NotAWrite, Via};
    const PASS: &str = "claim_reconciliation/pass_loop.rs";
    const DISPATCH: &str = "sweep_registry/private_dispatch.rs";
    let reviewed: &[(&str, Scope)] = &[
        (PASS, Gated),
        ("claim_reconciliation.rs", Via(PASS, "reclaim + anchor passes")),
        ("claim_reconciliation/verdict_invalidation.rs", Via(PASS, "verdict pass")),
        ("claim_reconciliation/review_conflict.rs", Via(PASS, "conflict pass")),
        ("claim_reconciliation/pass_loop/building_heal.rs", Via(PASS, "heal pass")),
        (
            "forge_disable_auto_merge.rs",
            Via(PASS, "verdict pass; shell guard vets its own call"),
        ),
        ("quarantine_reconciliation.rs", Gated),
        ("worktree_ops/gh.rs", Gated),
        ("worktree_ops/orphan_recovery.rs", Via("worktree_ops/gh.rs", "writes via gh.rs")),
        ("star_liveness/task.rs", Gated),
        (
            "star_liveness/forge.rs",
            Via("star_liveness/task.rs", "repos pass the task's gate"),
        ),
        ("role_runner/roster.rs", Gated),
        ("dep_classify/forge.rs", Gated),
        ("cli/notify_cleared_blockers.rs", Gated),
        (DISPATCH, Gated),
        ("work_finder/pool_preflight.rs", Gated),
        (
            "sweep_registry/guards.rs",
            Via(DISPATCH, "claim flip + lease of a dispatched sweep"),
        ),
        ("sweep_registry/watchdog.rs", Via(DISPATCH, "acts on dispatched sweeps")),
        ("sweep_registry/restore_to_ready.rs", Via(DISPATCH, "acts on dispatched sweeps")),
        ("sweep_registry/quarantine.rs", Via(DISPATCH, "acts on dispatched sweeps")),
        (
            "sweep_registry/prless_retry/hold.rs",
            Via(DISPATCH, "acts on dispatched sweeps"),
        ),
        (
            "sweep_registry/outcome_journal/writeback.rs",
            Via(DISPATCH, "dispatched sweeps"),
        ),
        (
            "script_helpers/validate_phase.rs",
            Via(DISPATCH, "runs inside a dispatched sweep"),
        ),
        (
            "merge_pr/redate.rs",
            NotAWrite("reached only from merge-pr.sh, which vets REPO_NWO"),
        ),
        ("forge_cmd.rs", NotAWrite("operator/shell verbs; shell callers vet the repo")),
        (
            "fleet/drain.rs",
            NotAWrite("operator-run `fleet drain` on the operator's own fleet"),
        ),
        (
            "watchdog/peer_coord.rs",
            NotAWrite("follow-up on the issue this watchdog filed"),
        ),
        (
            "dep_recheck/decide.rs",
            NotAWrite("action names; writes go through dep_classify"),
        ),
        ("role_tick_telemetry.rs", NotAWrite("classifies commands, runs none")),
        ("role_tick_telemetry/targets.rs", NotAWrite("classifies commands, runs none")),
        ("terminal.rs", NotAWrite("tmux flags")),
        ("tokens_pool/check.rs", NotAWrite("Anthropic API, not the forge")),
        ("worker_spawn/egress_proxy/server.rs", NotAWrite("HTTP method check in a proxy")),
    ];
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let writes = regex::Regex::new(
        r#""--add-label"|"--remove-label"|"comment"|"--method"|"-X"|"(POST|PATCH|PUT|DELETE)"|mutation\s*\(\$|mutation\s*\{"#,
    )
    .unwrap();
    let is_test = |rel: &str| {
        rel.ends_with("tests.rs")
            || rel.contains("/tests/")
            || rel.starts_with("tests/")
            || rel.contains("test_support")
    };
    let mut unreviewed = Vec::new();
    let mut stack = vec![src.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let rel = path
                .strip_prefix(&src)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            if !is_test(&rel) && writes.is_match(&text) && !reviewed.iter().any(|(f, _)| *f == rel)
            {
                unreviewed.push(rel);
            }
        }
    }
    assert!(
        unreviewed.is_empty(),
        "new daemon forge-write paths must be scoped by crate::write_scope (#9548) and listed \
         here: {unreviewed:?}"
    );
    let gated = |f: &str| {
        std::fs::read_to_string(src.join(f))
            .unwrap_or_default()
            .contains("write_scope::")
    };
    let matches =
        |f: &str| writes.is_match(&std::fs::read_to_string(src.join(f)).unwrap_or_default());
    for (file, scope) in reviewed {
        match scope {
            Gated => assert!(gated(file), "{file} is listed as gated but never calls write_scope"),
            Via(parent, why) => {
                assert!(
                    gated(parent)
                        && reviewed
                            .iter()
                            .any(|(f, s)| f == parent && matches!(s, Gated)),
                    "{file} ({why}) is reached via {parent}, which must be a gated entry"
                );
                assert!(matches(file), "stale entry: {file} ({why}) no longer writes");
            }
            NotAWrite(why) => assert!(matches(file), "stale entry: {file} ({why})"),
        }
    }
    // The reconciliation gate sits in front of every pass it covers.
    let pass = std::fs::read_to_string(src.join(PASS)).unwrap();
    let gate_at = pass.find("write_scope::gate_root").unwrap();
    let first_pass = pass.find("forge::reconcile_workspace(").unwrap();
    assert!(gate_at < first_pass, "the write-scope gate must precede the passes");
}
