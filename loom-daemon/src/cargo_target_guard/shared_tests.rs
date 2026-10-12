#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;

use super::super::{check_all, evaluate_all, Verdict};
use super::*;

const WT: &str = "/work/repo/.loom/worktrees/issue-1";
const SHARED: &str = "/Volumes/Stripe/cargo-target";

fn configured(dir: &str) -> Option<Configured> {
    Some(Configured {
        dir: PathBuf::from(dir),
        origin: Origin::File(PathBuf::from("/home/agent/.cargo/config.toml")),
    })
}

fn ctx(config: Option<Configured>) -> Context {
    Context {
        cwd: PathBuf::from(WT),
        home: Some(PathBuf::from("/home/agent")),
        tmpdir: Some(PathBuf::from("/var/folders/xy/T")),
        inherited: None,
        worktree: Some(PathBuf::from(WT)),
        main_root: Some(PathBuf::from("/work/repo")),
        owned: Vec::new(),
        configured: config,
    }
}

#[track_caller]
fn asks(cmd: &str, c: &Context) -> SharedAsk {
    let (deny, ask) = evaluate_all(cmd, c);
    assert_eq!(deny, None, "{cmd}");
    ask.unwrap_or_else(|| panic!("expected an ask for: {cmd}"))
}

#[track_caller]
fn silent(cmd: &str, c: &Context) {
    assert_eq!(evaluate_all(cmd, c).1, None, "expected no ask for: {cmd}");
}

// ---- parsing -----------------------------------------------------------------

#[test]
fn reads_target_dir_from_the_build_table_or_a_dotted_key() {
    let v = |b: &str| build_target_dir_value(b);
    assert_eq!(v("[build]\ntarget-dir = \"/x\"\n").as_deref(), Some("/x"));
    assert_eq!(v("[build] # c\ntarget-dir='/y' # z\n").as_deref(), Some("/y"));
    assert_eq!(v("build.target-dir = \"/z\"\n[env]\n").as_deref(), Some("/z"));
    // Another table's key, a key after [build] closes, and an empty value.
    assert_eq!(v("[env]\ntarget-dir = \"/no\"\n"), None);
    assert_eq!(v("[build]\njobs = 4\n[profile.dev]\ntarget-dir = \"/no\"\n"), None);
    assert_eq!(v("[target.'cfg(all())']\nbuild.target-dir = \"/no\"\n"), None);
    assert_eq!(v("[build]\ntarget-dir = \"\"\n"), None);
    assert_eq!(v("# [build]\n"), None);
}

// ---- cargo's precedence ------------------------------------------------------

fn files(entries: &[(&str, &str)]) -> BTreeMap<PathBuf, String> {
    entries
        .iter()
        .map(|(p, b)| (PathBuf::from(p), (*b).to_string()))
        .collect()
}

fn resolve(
    fs: &BTreeMap<PathBuf, String>,
    env: Option<&str>,
    home: Option<&str>,
) -> Option<Configured> {
    configured_target(Path::new(WT).join("loom-daemon").as_path(), env, home.map(Path::new), &|p| {
        fs.get(p).cloned()
    })
}

#[test]
fn env_beats_every_config_file_and_anchors_at_cwd() {
    let fs = files(&[("/work/.cargo/config.toml", "[build]\ntarget-dir = \"/a\"\n")]);
    let got = resolve(&fs, Some("rel"), None).unwrap();
    assert_eq!(got.dir, Path::new(WT).join("loom-daemon/rel"));
    assert_eq!(got.origin, Origin::Env);
    // An empty env value is not set.
    assert_eq!(resolve(&fs, Some(""), None).unwrap().dir, PathBuf::from("/a"));
}

#[test]
fn the_closest_config_wins_and_a_relative_value_anchors_at_its_parent() {
    let fs = files(&[
        ("/work/.cargo/config.toml", "[build]\ntarget-dir = \"/far\"\n"),
        ("/work/repo/.cargo/config", "[build]\ntarget-dir = \"../near\"\n"),
    ]);
    let got = resolve(&fs, None, None).unwrap();
    assert_eq!(got.dir, PathBuf::from("/work/near"));
    assert_eq!(got.origin, Origin::File(PathBuf::from("/work/repo/.cargo/config")));
}

#[test]
fn a_config_without_target_dir_does_not_stop_the_walk() {
    // The repo's own tracked .cargo/config.toml sets other keys only.
    let fs = files(&[
        (&format!("{WT}/.cargo/config.toml"), "[build]\n[alias]\nx = \"y\"\n"),
        (
            "/home/agent/.cargo/config.toml",
            &format!("[build]\ntarget-dir = \"{SHARED}\"\n"),
        ),
    ]);
    assert_eq!(resolve(&fs, None, None), None, "CARGO_HOME not consulted");
    let got = resolve(&fs, None, Some("/home/agent/.cargo")).unwrap();
    assert_eq!(got.dir, PathBuf::from(SHARED));
    assert_eq!(got.origin, Origin::File(PathBuf::from("/home/agent/.cargo/config.toml")));
}

#[test]
fn nothing_configured_is_none() {
    assert_eq!(resolve(&files(&[]), None, Some("/home/agent/.cargo")), None);
}

// ---- the verdict -------------------------------------------------------------

#[test]
fn a_bare_build_into_a_shared_config_dir_asks() {
    let c = ctx(configured(SHARED));
    let a = asks("cargo test -p loom-daemon", &c);
    assert_eq!(a.target, PathBuf::from(SHARED));
    assert_eq!(a.use_instead, Path::new(WT).join("target"));
    asks("cargo +nightly build --release", &c);
    asks("cd loom-daemon && cargo clippy --all-targets", &c);
    asks("bash -c 'cargo nextest run'", &c);
    asks("timeout 600 cargo t", &c);
    let reason = ask_reason(&a);
    assert!(reason.starts_with(ASK_PREFIX), "{reason}");
    assert!(reason.contains(&format!("CARGO_TARGET_DIR=\"{WT}/target\"")), "{reason}");
}

#[test]
fn the_ask_names_the_per_worktree_marker_dir_when_there_is_one() {
    let c = Context {
        owned: vec![PathBuf::from("/Volumes/Stripe/cargo-target/wt/issue-1")],
        ..ctx(configured(SHARED))
    };
    let a = asks("cargo test", &c);
    assert_eq!(a.use_instead, PathBuf::from("/Volumes/Stripe/cargo-target/wt/issue-1"));
    assert!(ask_reason(&a).contains("CARGO_TARGET_DIR=\"/Volumes/Stripe/cargo-target/wt/issue-1\""));
}

#[test]
fn an_explicit_target_dir_never_asks() {
    let c = ctx(configured(SHARED));
    silent(&format!("CARGO_TARGET_DIR={WT}/target cargo test"), &c);
    silent(&format!("env CARGO_TARGET_DIR={WT}/t cargo test"), &c);
    silent(&format!("export CARGO_TARGET_DIR={WT}/t; cargo test"), &c);
    silent("cargo test --target-dir target", &c);
    silent("CARGO_TARGET_DIR=$UNKNOWN cargo test", &c);
    let inherited = Context {
        inherited: Some("/work/repo/.loom/targets/b".into()),
        ..ctx(configured(SHARED))
    };
    silent("cargo test", &inherited);
}

#[test]
fn an_owned_configured_dir_or_no_config_never_asks() {
    silent("cargo test", &ctx(None));
    silent("cargo test", &ctx(configured(&format!("{WT}/target"))));
    silent("cargo test", &ctx(configured("/work/repo/.loom/targets/x")));
    let marker = Context {
        owned: vec![PathBuf::from("/Volumes/Stripe/cargo-target/wt/issue-1")],
        ..ctx(configured("/Volumes/Stripe/cargo-target/wt/issue-1"))
    };
    silent("cargo test", &marker);
}

#[test]
fn non_builds_and_make_never_ask() {
    let c = ctx(configured(SHARED));
    silent("cargo fmt --all", &c);
    silent("cargo metadata --format-version 1", &c);
    silent("make test", &c);
    silent("echo cargo test", &c);
}

#[test]
fn deny_wins_and_each_verdict_reads_its_own_toggle() {
    let c = ctx(configured(SHARED));
    let all_on = |_: &str, _: &str| true;
    assert!(matches!(check_all("cargo test", &c, all_on), Some(Verdict::Ask(_))));
    assert!(matches!(
        check_all("CARGO_TARGET_DIR=/tmp/x cargo test", &c, all_on),
        Some(Verdict::Deny(_))
    ));
    let ask_off = |k: &str, _: &str| k != TOGGLE_KEY;
    assert_eq!(check_all("cargo test", &c, ask_off), None);
    assert!(matches!(
        check_all("CARGO_TARGET_DIR=/tmp/x cargo test", &c, ask_off),
        Some(Verdict::Deny(_))
    ));
    let deny_off = |k: &str, _: &str| k == TOGGLE_KEY;
    assert!(matches!(check_all("cargo test", &c, deny_off), Some(Verdict::Ask(_))));
}
