#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

const WT: &str = "/work/repo/.loom/worktrees/issue-1";

fn ctx() -> Context {
    Context {
        cwd: PathBuf::from(WT),
        home: Some(PathBuf::from("/home/agent")),
        tmpdir: Some(PathBuf::from("/var/folders/xy/T")),
        inherited: None,
        worktree: Some(PathBuf::from(WT)),
        main_root: Some(PathBuf::from("/work/repo")),
        owned: Vec::new(),
    }
}

fn with_inherited(v: &str) -> Context {
    Context {
        inherited: Some(v.to_string()),
        ..ctx()
    }
}

#[track_caller]
fn denied(cmd: &str, c: &Context, root: Root, source: Source) {
    let d = evaluate(cmd, c).unwrap_or_else(|| panic!("expected a deny for: {cmd}"));
    assert_eq!((d.root, d.source), (root, source), "{cmd}");
}

#[track_caller]
fn allowed(cmd: &str, c: &Context) {
    assert_eq!(evaluate(cmd, c), None, "expected an allow for: {cmd}");
}

// ---- the acceptance matrix (#11582) -----------------------------------------

#[test]
fn unset_allows() {
    allowed("cargo test -p loom-daemon", &ctx());
    allowed("cargo build --release", &ctx());
}

#[test]
fn loom_targets_dir_allows() {
    allowed("CARGO_TARGET_DIR=/work/repo/.loom/targets/x cargo test", &ctx());
    allowed("cargo test", &with_inherited("/work/repo/.loom/targets/judge-123"));
}

#[test]
fn worktree_target_allows() {
    allowed(&format!("CARGO_TARGET_DIR={WT}/target cargo test"), &ctx());
    allowed("CARGO_TARGET_DIR=\"$PWD/target\" cargo test", &ctx());
    allowed("CARGO_TARGET_DIR=\"$(pwd)/target\" cargo test", &ctx());
    allowed("cargo test --target-dir target", &ctx());
    allowed(
        &format!("WORKTREE_ABS={WT}; CARGO_TARGET_DIR=\"$WORKTREE_ABS/target\" cargo test"),
        &ctx(),
    );
}

#[test]
fn forbidden_roots_deny() {
    denied("CARGO_TARGET_DIR=/tmp/x cargo test", &ctx(), Root::Tmp, Source::Inline);
    denied("CARGO_TARGET_DIR=/private/tmp/x cargo test", &ctx(), Root::Tmp, Source::Inline);
    denied(
        "CARGO_TARGET_DIR=~/.cache/x-target cargo build",
        &ctx(),
        Root::HomeCache,
        Source::Inline,
    );
    denied("CARGO_TARGET_DIR=$HOME/x cargo check", &ctx(), Root::Home, Source::Inline);
    denied("CARGO_TARGET_DIR=${HOME}/x cargo check", &ctx(), Root::Home, Source::Inline);
    denied(
        "CARGO_TARGET_DIR=/work/repo/.loom/target-9 cargo test",
        &ctx(),
        Root::LegacyLoomTarget,
        Source::Inline,
    );
    denied(
        "CARGO_TARGET_DIR=$TMPDIR/cargo cargo clippy",
        &ctx(),
        Root::TmpDir,
        Source::Inline,
    );
}

#[test]
fn legacy_loom_target_denies_even_inside_the_worktree() {
    denied(
        &format!("CARGO_TARGET_DIR={WT}/.loom/target-3 cargo test"),
        &ctx(),
        Root::LegacyLoomTarget,
        Source::Inline,
    );
    denied(
        "cargo test --target-dir .loom/target-3",
        &ctx(),
        Root::LegacyLoomTarget,
        Source::TargetDirFlag,
    );
}

#[test]
fn target_dir_flag_denies() {
    denied("cargo test --target-dir /tmp/x", &ctx(), Root::Tmp, Source::TargetDirFlag);
    denied("cargo test --target-dir=/tmp/x", &ctx(), Root::Tmp, Source::TargetDirFlag);
    denied(
        "cargo +nightly build --locked --target-dir /tmp/x",
        &ctx(),
        Root::Tmp,
        Source::TargetDirFlag,
    );
    // The flag wins over an allowed env value.
    denied(
        "CARGO_TARGET_DIR=/work/repo/.loom/targets/x cargo build --target-dir /tmp/x",
        &ctx(),
        Root::Tmp,
        Source::TargetDirFlag,
    );
    // A flag after `--` belongs to the test binary, not cargo.
    allowed("cargo test -- --target-dir /tmp/x", &ctx());
}

#[test]
fn env_wrapper_denies() {
    denied("env CARGO_TARGET_DIR=/tmp/x cargo test", &ctx(), Root::Tmp, Source::EnvWrapper);
    denied(
        "timeout 600 env CARGO_TARGET_DIR=/tmp/x cargo nextest run",
        &ctx(),
        Root::Tmp,
        Source::EnvWrapper,
    );
    denied(
        "nohup env -u FOO CARGO_TARGET_DIR=/tmp/x cargo test",
        &ctx(),
        Root::Tmp,
        Source::EnvWrapper,
    );
}

// ---- inherited / exported / unset -------------------------------------------

#[test]
fn inherited_forbidden_value_denies_a_build() {
    let c = with_inherited("/home/agent/.cache/cargo-target");
    denied("cargo build", &c, Root::HomeCache, Source::Inherited);
    denied("cd loom-daemon && cargo test", &c, Root::HomeCache, Source::Inherited);
    allowed("cargo --version", &c);
    allowed("cargo fmt --all", &c);
    allowed("git status", &c);
}

#[test]
fn an_override_or_unset_replaces_the_inherited_value() {
    let c = with_inherited("/home/agent/.cache/cargo-target");
    allowed(&format!("CARGO_TARGET_DIR={WT}/target cargo build"), &c);
    allowed("unset CARGO_TARGET_DIR; cargo build", &c);
    allowed("env -u CARGO_TARGET_DIR cargo build", &c);
    allowed("env -i PATH=/usr/bin cargo build", &c);
}

#[test]
fn export_earlier_in_the_command_counts() {
    denied(
        "export CARGO_TARGET_DIR=/tmp/t && cargo build",
        &ctx(),
        Root::Tmp,
        Source::Export,
    );
    allowed("export CARGO_TARGET_DIR=/tmp/t; unset CARGO_TARGET_DIR; cargo build", &ctx());
}

#[test]
fn mktemp_resolves_under_tmpdir() {
    denied(
        "T=$(mktemp -d); CARGO_TARGET_DIR=$T cargo build",
        &ctx(),
        Root::TmpDir,
        Source::Inline,
    );
    denied(
        "CARGO_TARGET_DIR=\"$(mktemp -d)\" cargo test",
        &ctx(),
        Root::TmpDir,
        Source::Inline,
    );
}

#[test]
fn make_is_judged_only_on_an_explicit_value() {
    denied("make test CARGO_TARGET_DIR=/tmp/x", &ctx(), Root::Tmp, Source::MakeArg);
    denied("CARGO_TARGET_DIR=/tmp/x make test", &ctx(), Root::Tmp, Source::Inline);
    allowed("make test", &with_inherited("/tmp/shared"));
}

#[test]
fn shell_wrappers_and_cd_are_followed() {
    denied(
        "bash -c 'CARGO_TARGET_DIR=/tmp/x cargo build'",
        &ctx(),
        Root::Tmp,
        Source::Inline,
    );
    denied(
        "cd /tmp && cargo build --target-dir t",
        &ctx(),
        Root::Tmp,
        Source::TargetDirFlag,
    );
    denied("(cd /tmp; CARGO_TARGET_DIR=x cargo test)", &ctx(), Root::Tmp, Source::Inline);
}

// ---- what must stay an allow ------------------------------------------------

#[test]
fn prose_and_unresolvable_values_allow() {
    allowed("echo CARGO_TARGET_DIR=/tmp/x cargo build", &ctx());
    allowed("grep -r 'CARGO_TARGET_DIR=/tmp/x cargo test' docs/", &ctx());
    allowed("cat > notes.md <<'EOF'\nCARGO_TARGET_DIR=/tmp/x cargo build\nEOF", &ctx());
    allowed(
        "cat > notes.md <<EOF\nCARGO_TARGET_DIR=/tmp/x cargo build\nEOF\ncargo test",
        &ctx(),
    );
    allowed("CARGO_TARGET_DIR=$SOMETHING_FROM_AN_EARLIER_CALL cargo build", &ctx());
    allowed("CARGO_TARGET_DIR='$HOME/x' cargo build", &ctx());
    allowed("CARGO_TARGET_DIR=\"$(some_helper)\" cargo build", &ctx());
    allowed("CARGO_TARGET_DIR= cargo build", &ctx());
    // Not one of the forbidden roots: an operator's deliberate shared volume.
    allowed("CARGO_TARGET_DIR=/Volumes/Big/cargo-target cargo build", &ctx());
    allowed("cargo install --path . --target-dir /tmp/x", &ctx());
    allowed("cargo build 2>&1 | tail -5", &with_inherited("/work/repo/.loom/targets/a"));
}

#[test]
fn the_per_worktree_dir_is_owned_wherever_it_lives() {
    let c = Context {
        owned: vec![PathBuf::from("/home/agent/.cache/ct/wt/issue-1")],
        ..with_inherited("/home/agent/.cache/ct/wt/issue-1")
    };
    allowed("cargo test", &c);
    denied(
        "CARGO_TARGET_DIR=/home/agent/.cache/ct/other cargo test",
        &c,
        Root::HomeCache,
        Source::Inline,
    );
}

#[test]
fn a_worktree_under_home_or_tmpdir_is_still_the_worktree() {
    let c = Context {
        cwd: PathBuf::from("/home/agent/src/repo"),
        worktree: Some(PathBuf::from("/home/agent/src/repo")),
        main_root: Some(PathBuf::from("/home/agent/src/repo")),
        ..ctx()
    };
    allowed("cargo test --target-dir target", &c);
    allowed("CARGO_TARGET_DIR=/home/agent/src/repo/.loom/targets/a cargo test", &c);
    denied("cargo test --target-dir ../target", &c, Root::Home, Source::TargetDirFlag);
}

// ---- toggle, message, telemetry ----------------------------------------------

#[test]
fn toggle_reads_env_then_config() {
    assert!(toggle_enabled(None, None));
    assert!(!toggle_enabled(Some("0"), None));
    assert!(!toggle_enabled(Some("false"), Some(&Value::Bool(true))));
    assert!(toggle_enabled(Some("1"), Some(&Value::Bool(false))));
    assert!(!toggle_enabled(None, Some(&Value::Bool(false))));
    assert!(!toggle_enabled(None, Some(&Value::String("false".into()))));
    assert!(toggle_enabled(Some("maybe"), None));
}

#[test]
fn check_consults_the_toggle_only_on_a_deny() {
    let mut called = false;
    assert!(check("cargo test", &ctx(), || {
        called = true;
        true
    })
    .is_none());
    assert!(!called, "an allow must not read config");
    assert!(check("CARGO_TARGET_DIR=/tmp/x cargo test", &ctx(), || false).is_none());
    assert!(check("CARGO_TARGET_DIR=/tmp/x cargo test", &ctx(), || true).is_some());
}

#[test]
fn deny_message_names_the_loom_owned_alternative() {
    let d = evaluate("CARGO_TARGET_DIR=/tmp/x cargo test", &ctx()).unwrap();
    let msg = deny_reason(&d);
    assert!(msg.starts_with(DENY_PREFIX), "{msg}");
    assert!(msg.contains(&format!("CARGO_TARGET_DIR=\"{WT}/target\"")), "{msg}");
    assert!(msg.contains("LOOM_GUARD_CARGO_TARGET_DIR=0"), "{msg}");

    let c = with_inherited("/work/repo/.loom/targets/judge-7");
    let d = evaluate("CARGO_TARGET_DIR=/tmp/x cargo test", &c).unwrap();
    let msg = deny_reason(&d);
    assert!(msg.contains("/work/repo/.loom/targets/judge-7"), "{msg}");
    assert!(msg.contains("Drop the override"), "{msg}");
}

#[test]
fn denial_span_carries_role_root_and_source_only() {
    let d = evaluate("CARGO_TARGET_DIR=~/.cache/doctor-1-target cargo test", &ctx()).unwrap();
    let parent = TraceContext::derived("test", &["parent"]);
    let at = Utc::now();
    let span = denial_span(&d, Some("doctor"), at, Some(&parent));
    assert_eq!(span.name.as_str(), "guard.cargo_target_dir.denied");
    assert_eq!(span.parent_span_id.as_ref(), Some(&parent.span_id));
    assert_eq!(span.context.trace_id, parent.trace_id);
    assert!(span.validate().is_ok());
    let bounded = span.clone().bounded();
    assert_eq!(bounded.attributes.get(ATTR_ROOT).map(String::as_str), Some("~/.cache"));
    assert_eq!(bounded.attributes.get(ATTR_SOURCE).map(String::as_str), Some("inline"));
    assert_eq!(bounded.attributes.get("loom.role").map(String::as_str), Some("doctor"));
    for value in span.attributes.values() {
        assert!(!value.contains("doctor-1-target"), "the path leaked: {value}");
        assert!(!value.contains("cargo test"), "the command leaked: {value}");
    }
    // No parent: its own root; a malformed role is dropped.
    let root = denial_span(&d, Some("bad role; rm"), at, None);
    assert!(root.parent_span_id.is_none());
    assert!(!root.attributes.contains_key("loom.role"));
}

#[test]
fn git_roots_follows_a_linked_worktree_to_its_main_checkout() {
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join("repo");
    let wt = main.join(".loom/worktrees/issue-1");
    std::fs::create_dir_all(main.join(".git/worktrees/issue-1")).unwrap();
    std::fs::create_dir_all(wt.join("sub")).unwrap();
    std::fs::write(
        wt.join(".git"),
        format!("gitdir: {}\n", main.join(".git/worktrees/issue-1").display()),
    )
    .unwrap();
    let (w, m) = git_roots(&wt.join("sub"));
    assert_eq!(w.as_deref(), Some(normalize(&wt).as_path()));
    assert_eq!(m.as_deref(), Some(normalize(&main).as_path()));
    let (w, m) = git_roots(&main);
    assert_eq!((w.as_deref(), m.as_deref()), (Some(main.as_path()), Some(main.as_path())));
}

#[test]
fn cd_into_a_sibling_repo_uses_that_repos_roots() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let sibling = home.join("GitHub/anvil");
    std::fs::create_dir_all(sibling.join(".git")).unwrap();
    let c = Context {
        home: Some(home),
        ..ctx()
    };
    let cmd = format!("cd {} && CARGO_TARGET_DIR=$PWD/target cargo build", sibling.display());
    assert!(evaluate(&cmd, &c).is_none());
    // A plain directory that is no working tree stays under `~`.
    let plain = tmp.path().join("home/scratch");
    std::fs::create_dir_all(&plain).unwrap();
    let cmd = format!("cd {} && CARGO_TARGET_DIR=$PWD/target cargo build", plain.display());
    assert_eq!(evaluate(&cmd, &c).unwrap().root, Root::Home);
}

#[test]
fn main_checkout_target_is_called_shared_not_unowned() {
    let mut c = ctx();
    c.home = Some(PathBuf::from("/work"));
    let d = evaluate("CARGO_TARGET_DIR=/work/repo/target cargo test", &c).unwrap();
    let msg = deny_reason(&d);
    assert!(msg.contains("shared by every worktree"), "{msg}");
    assert!(!msg.contains("Nothing owns"), "{msg}");
    let d = evaluate("CARGO_TARGET_DIR=/tmp/x cargo test", &c).unwrap();
    assert!(deny_reason(&d).contains("Nothing owns"));
}
