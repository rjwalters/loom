//! #9368: a tier-3 generic-passthrough tap (`gemini`, `aider`, …) is passed
//! over when the CLI binary its adapter would exec provably does not resolve,
//! instead of being chosen and dying in `spawn-generic.sh` with exit 127.
//!
//! Hermetic by construction: what is "installed" is a [`BinaryEnv`] handed to
//! the walk — a `PATH` naming a tempdir that holds (or lacks) a stub
//! executable. No test here mutates `PATH` or `LOOM_GENERIC_CLI_BIN`, and none
//! depends on whether this host really has a `gemini` or an `aider`.
use super::availability::{availability_in, Availability, BinaryEnv, CredentialSource};
use super::resolve::{Resolution, Tap};
use super::tests::{fixture, ClearedRuntimeEnv};
use super::{resolve_runtime_in, Decision, DispatchContext, NoContainment, PreferenceSource};
use crate::role_runner::PoolHold;
use crate::runtime_admission::{resolve_and_admit, ResolvedRuntime};
use std::ffi::OsString;
use std::fs;
use std::path::Path;

/// A role with no `runtimeRequirements`, so every tier-3 runtime is admitted
/// and only availability decides the walk.
const ROLE: &str = "curator";

/// A directory standing in for one `PATH` entry.
struct Bin(tempfile::TempDir);

impl Bin {
    fn empty() -> Self {
        Self(tempfile::tempdir().unwrap())
    }

    fn holding(names: &[&str]) -> Self {
        let bin = Self::empty();
        for name in names {
            bin.add(name, 0o755);
        }
        bin
    }

    fn add(&self, name: &str, mode: u32) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = self.0.path().join(name);
        fs::write(&path, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    /// A [`BinaryEnv`] whose whole `PATH` is this directory.
    fn env(&self) -> BinaryEnv {
        BinaryEnv {
            cli_bin: None,
            path: Some(self.0.path().as_os_str().to_owned()),
        }
    }
}

/// [`fixture`] plus the `gemini` adapter stub it does not ship, the given
/// preference list, and **no Claude token pool** — so `claude` is always the
/// exhausted first tap these walks fall through from.
fn workspace(preference: &[&str]) -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let dir = fixture();
    let adapter = dir.path().join(".loom/scripts/spawn-gemini.sh");
    fs::write(&adapter, "#!/bin/sh\n").unwrap();
    fs::set_permissions(adapter, fs::Permissions::from_mode(0o755)).unwrap();
    let config = serde_json::json!({"runtimes": {"preference": preference}});
    fs::write(dir.path().join(".loom/config.json"), config.to_string()).unwrap();
    dir
}

fn walk(root: &Path, env: &BinaryEnv) -> Resolution<ResolvedRuntime> {
    let decision = resolve_runtime_in(
        root,
        ROLE,
        None,
        0,
        DispatchContext::default(),
        &mut NoContainment,
        env,
    )
    .unwrap();
    let Decision::Preference { resolution, .. } = decision else {
        panic!("expected the preference path");
    };
    resolution
}

fn marker(resolution: Resolution<ResolvedRuntime>) -> String {
    Decision::Preference {
        source: PreferenceSource::FleetPreference,
        resolution,
        backstop: None,
    }
    .marker_line()
    .unwrap()
}

/// One generic tap's availability, outside any walk.
fn generic(root: &Path, runtime: &str, env: &BinaryEnv) -> Availability {
    let admitted = resolve_and_admit(root, ROLE, Some(runtime)).unwrap();
    availability_in(root, &Tap::runtime(runtime), &admitted, 0, env)
}

fn assert_ungated(state: &Availability) {
    assert!(state.is_spawnable(), "{state:?}");
    assert_eq!(state.source(), &CredentialSource::Unobservable);
}

/// The incident: `["claude", "gemini"]`, Claude dry, no `gemini` installed.
/// The walk now yields no choice — the caller's existing fail-closed
/// hold/skip — instead of choosing a tap that cannot exec.
#[test]
#[serial_test::serial]
fn an_uninstalled_generic_tap_is_passed_over_instead_of_chosen() {
    let _env = ClearedRuntimeEnv::new();
    let dir = workspace(&["claude", "gemini"]);
    let resolution = walk(dir.path(), &Bin::empty().env());

    assert!(resolution.chosen.is_none(), "{resolution:?}");
    assert_eq!(resolution.skipped.len(), 2);
    assert!(resolution.skipped[0]
        .reason
        .summary()
        .starts_with("claude_tokens: "));
    let skip = &resolution.skipped[1];
    assert_eq!(skip.tap.runtime, "gemini");
    assert_eq!(skip.reason.kind(), "unavailable");
    let summary = skip.reason.summary();
    assert!(summary.starts_with("binary: 'gemini' not found in PATH"), "{summary}");

    let line = marker(resolution);
    assert!(line.contains("tier=none tap=none"), "{line}");
    assert!(
        line.contains(",gemini:unavailable(binary: 'gemini' not found in PATH"),
        "{line}"
    );
}

/// Installed, it is chosen exactly as before the gate existed: the marker is
/// byte-for-byte the pre-#9368 one (nothing about the binary appears in it).
#[test]
#[serial_test::serial]
fn an_installed_generic_tap_is_chosen_with_an_unchanged_marker() {
    let _env = ClearedRuntimeEnv::new();
    let dir = workspace(&["claude", "gemini"]);
    let resolution = walk(dir.path(), &Bin::holding(&["gemini"]).env());

    let chosen = resolution.chosen.as_ref().unwrap();
    assert_eq!((chosen.tier, chosen.admitted.runtime.as_str()), (1, "gemini"));
    assert_eq!(resolution.skipped.len(), 1);
    let expected = format!(
        "{}order=claude,gemini tier=1 tap=gemini skipped=claude:{} source=preference",
        super::PREFERENCE_LOG_MARKER,
        resolution.skipped[0].reason
    );
    assert_eq!(marker(resolution), expected);
    assert!(!expected.contains("binary"), "{expected}");
}

/// The gate makes a tier-3 tap *fall through* like any other: the next
/// installed one is chosen, and the marker names each higher tap's own reason.
#[test]
#[serial_test::serial]
fn the_walk_falls_through_an_uninstalled_tap_to_an_installed_one() {
    let _env = ClearedRuntimeEnv::new();
    let dir = workspace(&["claude", "gemini", "aider"]);
    let resolution = walk(dir.path(), &Bin::holding(&["aider"]).env());

    let chosen = resolution.chosen.as_ref().unwrap();
    assert_eq!((chosen.tier, chosen.admitted.runtime.as_str()), (2, "aider"));
    let line = marker(resolution);
    assert!(line.contains("order=claude,gemini,aider tier=2 tap=aider"), "{line}");
    assert!(line.contains("skipped=claude:unavailable(claude_tokens: "), "{line}");
    assert!(
        line.contains(",gemini:unavailable(binary: 'gemini' not found in PATH"),
        "{line}"
    );
}

/// `LOOM_GENERIC_CLI_BIN` wins over the manifest's `cliBin`, exactly as it
/// does at launch — in both directions.
#[test]
#[serial_test::serial]
fn the_env_pinned_binary_is_the_one_checked() {
    let _env = ClearedRuntimeEnv::new();
    let dir = workspace(&["claude", "gemini"]);

    // Manifest binary absent, pinned one present (bare name, then absolute).
    let bin = Bin::holding(&["gemini-nightly"]);
    let mut env = bin.env();
    env.cli_bin = Some(OsString::from("gemini-nightly"));
    assert_ungated(&generic(dir.path(), "gemini", &env));
    let absolute = bin.0.path().join("gemini-nightly");
    env.cli_bin = Some(absolute.clone().into_os_string());
    env.path = None;
    assert_ungated(&generic(dir.path(), "gemini", &env));

    // Manifest binary present, pinned one absent: the launch would exec the
    // pinned name, so the manifest's being installed does not help.
    let mut env = Bin::holding(&["gemini"]).env();
    env.cli_bin = Some(OsString::from("gemini-nightly"));
    let state = generic(dir.path(), "gemini", &env);
    let Availability::Exhausted {
        source,
        hold,
        detail,
    } = &state
    else {
        panic!("expected the pinned binary to be reported missing, got {state:?}");
    };
    assert_eq!(source.wire(), "binary");
    assert_eq!(hold, &PoolHold::Unprovisioned);
    assert!(detail.starts_with("'gemini-nightly' not found in PATH"), "{detail}");

    // An absolute pin is checked as-is, and says so.
    env.cli_bin = Some(absolute.with_file_name("no-such-cli").into_os_string());
    let state = generic(dir.path(), "gemini", &env);
    let detail = state.skip_reason().unwrap().summary();
    assert!(detail.contains("no-such-cli' is not an executable file"), "{detail}");
}

/// Only an *executable regular file* counts, as for `command -v`.
#[test]
#[serial_test::serial]
fn a_non_executable_file_or_a_directory_is_not_an_installed_binary() {
    let _env = ClearedRuntimeEnv::new();
    let dir = workspace(&["claude", "gemini"]);

    let bin = Bin::empty();
    bin.add("gemini", 0o644);
    assert!(!generic(dir.path(), "gemini", &bin.env()).is_spawnable());

    let bin = Bin::empty();
    fs::create_dir(bin.0.path().join("gemini")).unwrap();
    assert!(!generic(dir.path(), "gemini", &bin.env()).is_spawnable());

    // A later PATH entry that does hold it still resolves.
    let (first, second) = (Bin::empty(), Bin::holding(&["gemini"]));
    let path = std::env::join_paths([first.0.path(), second.0.path()]).unwrap();
    let env = BinaryEnv {
        cli_bin: None,
        path: Some(path),
    };
    assert_ungated(&generic(dir.path(), "gemini", &env));
}

/// The module's one rule: never pass over a tap that could have served. Every
/// case the daemon cannot actually observe stays ungated, as before #9368.
#[test]
#[serial_test::serial]
fn an_unobservable_binary_is_never_reported_missing() {
    let _env = ClearedRuntimeEnv::new();
    let dir = workspace(&["claude", "gemini"]);
    let runtimes = dir.path().join(".loom/runtimes");
    let scripts = dir.path().join(".loom/scripts");
    let empty = Bin::empty();

    // A runtime whose manifest declares no `launch.cliBin`: the name is
    // unknown, so even an empty PATH proves nothing.
    let manifest = fs::read_to_string(runtimes.join("gemini.json")).unwrap();
    let mut doc: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    doc.as_object_mut().unwrap().remove("launch");
    doc["runtime"] = "widget".into();
    fs::write(runtimes.join("widget.json"), doc.to_string()).unwrap();
    fs::copy(scripts.join("spawn-gemini.sh"), scripts.join("spawn-widget.sh")).unwrap();
    assert_ungated(&generic(dir.path(), "widget", &empty.env()));

    // An unparseable `launch` object: the launch path reports it, loudly.
    doc["launch"] = serde_json::json!({"cliBin": "widget-cli", "bogusKey": "oops"});
    fs::write(runtimes.join("widget.json"), doc.to_string()).unwrap();
    assert_ungated(&generic(dir.path(), "widget", &empty.env()));

    // No PATH at all.
    assert_ungated(&generic(dir.path(), "gemini", &BinaryEnv::default()));

    // A relative (or empty) PATH entry is searched from the *worker's* cwd,
    // which the daemon does not share.
    for relative in ["bin", ""] {
        let path = std::env::join_paths([empty.0.path(), Path::new(relative)]).unwrap();
        let env = BinaryEnv {
            cli_bin: None,
            path: Some(path),
        };
        assert_ungated(&generic(dir.path(), "gemini", &env));
    }

    // Likewise a pinned binary given as a relative path.
    let mut env = empty.env();
    env.cli_bin = Some(OsString::from("./bin/gemini"));
    assert_ungated(&generic(dir.path(), "gemini", &env));

    // And the gate did trip for the same workspace with nothing unobservable.
    assert!(!generic(dir.path(), "gemini", &empty.env()).is_spawnable());
}

/// The gate is the catch-all arm only. `claude`, `codex` and the native
/// harnesses have their own adapters and install locations (some under
/// `~/.loom/opt/`), so an empty PATH must not change their verdicts.
#[test]
#[serial_test::serial]
fn pooled_and_native_runtimes_are_not_path_checked() {
    let _env = ClearedRuntimeEnv::new();
    let dir = workspace(&["claude", "gemini"]);
    let tokens = dir.path().join(".loom/tokens");
    fs::create_dir_all(&tokens).unwrap();
    fs::write(tokens.join("account0.token"), "sk-ant-oat-fake\n").unwrap();
    let empty = Bin::empty().env();

    for runtime in ["claude", "codex", "opencode"] {
        let admitted = resolve_and_admit(dir.path(), ROLE, Some(runtime)).unwrap();
        let tap = Tap::runtime(runtime);
        let gated = availability_in(dir.path(), &tap, &admitted, 0, &empty);
        let installed = Bin::holding(&[runtime]).env();
        assert_eq!(gated, availability_in(dir.path(), &tap, &admitted, 0, &installed), "{runtime}");
        assert_ne!(gated.source(), &CredentialSource::Binary, "{runtime}");
    }
    let claude = resolve_and_admit(dir.path(), ROLE, Some("claude")).unwrap();
    assert!(availability_in(dir.path(), &Tap::runtime("claude"), &claude, 0, &empty).is_spawnable());
}

/// The shipped `gemini` manifest names its binary, and the installed copy
/// has not drifted from it — otherwise the daemon cannot learn the name and
/// the gate silently stays open.
#[test]
fn the_shipped_gemini_manifest_declares_its_binary() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let shipped = fs::read_to_string(repo.join("defaults/runtimes/gemini.json")).unwrap();
    let installed = fs::read_to_string(repo.join(".loom/runtimes/gemini.json")).unwrap();
    assert_eq!(shipped, installed, "the two gemini manifests must stay identical");
    let doc: serde_json::Value = serde_json::from_str(&shipped).unwrap();
    assert_eq!(doc["launch"]["cliBin"], "gemini");
    // spawn-gemini.sh's own default, which the manifest must not contradict.
    assert_eq!(doc["launch"]["promptFlag"], "--prompt");
}
