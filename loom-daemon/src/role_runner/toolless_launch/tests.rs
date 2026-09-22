//! Coverage for the toolless-guarded-native-launch verdict (issue #8448).
//!
//! The two end-to-end cases drive the real
//! [`ScriptRoleInvocationRunner::invoke`] with a fake `spawn-worker.sh`
//! standing in for the native harness and printing the same `--format json`
//! event stream a real one does — never by calling [`super::detect`] directly
//! to fabricate the verdict under test. That is what proves the wiring: an
//! exit-0 tick with no `loom_*` tool use comes back as a
//! [`RoleTickOutcome::Failure`], and its byte-identical tool-using twin comes
//! back as [`RoleTickOutcome::Success`].
//!
//! The remaining cases exercise [`super::detect_in`]'s stand-down conditions
//! directly, because each is about a situation the end-to-end harness cannot
//! produce (a non-native admission, a log whose anchor is missing, a stream
//! format this module cannot parse).

use super::*;
use crate::runtime_admission::{ResolvedRuntime, RuntimeSource};
use serial_test::serial;
use std::fs;
use std::os::unix::fs::PermissionsExt;

/// The exact stream shape #8448's live OpenCode 2.0.10 receipt recorded: one
/// `step_start`, one `text` event in which the model says it has no `loom_*`
/// tools, no `tool_use`, no `step_finish` — and exit 0.
const TOOLLESS_STREAM: &str = concat!(
    r#"{"type":"step_start"}"#,
    "\n",
    r#"{"type":"text","text":"I do not have tools named loom_write, loom_bash, or loom_edit"}"#,
);

/// The control: the same launch, with the binding actually loaded.
const TOOL_USING_STREAM: &str = concat!(
    r#"{"type":"step_start"}"#,
    "\n",
    r#"{"type":"tool_use","tool":"loom_edit","input":{"path":"mathx.py"}}"#,
    "\n",
    r#"{"type":"step_finish"}"#,
);

fn resolved(runtime: &str) -> ResolvedRuntime {
    ResolvedRuntime {
        role: "judge".into(),
        runtime: runtime.into(),
        source: RuntimeSource::RoleConfig,
        adapter: PathBuf::from("/nonexistent/spawn-worker.sh"),
        role_manifest: PathBuf::from("/nonexistent/judge.json"),
        runtime_manifest: PathBuf::from("/nonexistent/runtime.json"),
        suggested_worker_type: None,
        preference: None,
    }
}

const ANCHOR: &str = "2026-09-21T02:00:00+00:00";

fn log_with(anchor: &str, stream: &str) -> String {
    format!("==== loom-daemon role_runner: {anchor} role=judge ====\n{stream}\n")
}

// ---------------------------------------------------------------------------
// End-to-end: the role runner's own verdict for a real, exit-0 invocation.
// ---------------------------------------------------------------------------

fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Every env var that can override which runtime the `judge` role resolves
/// to, cleared for the scope of a test and restored on drop — mirrors
/// `provider_health_feedback::tests::EnvGuard`. A daemon-dispatched test
/// runner inherits a live `LOOM_RUNTIME` (see `builder.md` § "Your
/// Environment Is Not a Clean Shell"), which would otherwise outrank the
/// workspace's own `runtimes.roles.judge` pin and resolve the tick onto
/// `claude` instead of the native runtime under test.
const GUARDED_ENV: [&str; 2] = ["LOOM_RUNTIME", "LOOM_RUNTIME_JUDGE"];

struct EnvGuard(Vec<(&'static str, Option<String>)>);

impl EnvGuard {
    fn new() -> Self {
        let prior = GUARDED_ENV
            .iter()
            .map(|key| (*key, std::env::var(key).ok()))
            .collect();
        for key in GUARDED_ENV {
            std::env::remove_var(key);
        }
        Self(prior)
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in self.0.drain(..) {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

/// A workspace whose `judge` role is pinned to the `opencode` native runtime
/// (which `judge.json`'s only requirement, `loomControl`, admits), with a
/// fake `spawn-worker.sh` each case overwrites to script the harness's own
/// native event stream. Mirrors `provider_health_feedback::tests`'
/// `codex_judge_workspace`.
fn opencode_judge_workspace(root: &Path, stream: &str) {
    for sub in [".loom/roles", ".loom/runtimes", ".loom/scripts"] {
        fs::create_dir_all(root.join(sub)).unwrap();
    }
    fs::write(root.join(".loom/config.json"), r#"{"runtimes":{"roles":{"judge":"opencode"}}}"#)
        .unwrap();
    fs::write(
        root.join(".loom/roles/judge.json"),
        r#"{"runtimeRequirements":["loomControl"]}"#,
    )
    .unwrap();
    fs::write(
        root.join(".loom/runtimes/opencode.json"),
        r#"{"runtime":"opencode","capabilities":{"loomControl":"yes"}}"#,
    )
    .unwrap();
    // `printf '%s\n'` rather than `echo`: the stream is JSON with no shell
    // metacharacters, and this keeps the fixture's output byte-exact.
    write_executable(
        &root.join(".loom/scripts/spawn-worker.sh"),
        &format!("#!/bin/sh\nprintf '%s\\n' '{stream}'\nexit 0\n"),
    );
}

/// Both end-to-end cases drive a real `fork`/`exec` of the fake
/// `spawn-worker.sh` and assert on an exact [`RoleTickOutcome`] variant, so
/// they are the two most host-timing-sensitive tests in this module. Issue
/// #8532 reported both of them failing together in one full `cargo test -p
/// loom-daemon --lib` run on a saturated host, while adjacent clean runs
/// passed — and the two knobs below are what make a *saturated* host produce
/// the same verdict a quiet one does:
///
/// - **`with_timeout`** — the script under test is a single `printf` + `exit
///   0`, so any budget at all is generous in the happy path and the value only
///   ever matters when the host cannot schedule the child promptly. 60s buys
///   headroom over the previous 30s at zero cost to a passing run (the timer
///   is a ceiling, never a sleep).
/// - **`with_load_per_core_override(0.0)`** — without it, a ceiling hit on a
///   genuinely saturated host is *deliberately* reclassified from
///   [`RoleTickOutcome::Failure`] to [`RoleTickOutcome::LoadSkipped`]
///   (issue #6637), a third variant neither case below handles: the toolless
///   case would report "must not be Success" and the control would report "not
///   Success", both pointing at the verdict logic rather than at the host.
///   Pinning the reading makes the fallback deterministic — same reason
///   #7242 pinned it for `test_invoke_times_out_on_hung_script`.
///
/// Neither knob can mask a real regression: a child that runs and exits 0
/// still gets the full verdict, and a genuinely wrong verdict still fails.
fn judge_runner(root: &Path) -> ScriptRoleInvocationRunner {
    ScriptRoleInvocationRunner::new(root.to_path_buf())
        .with_timeout(Duration::from_secs(60))
        .with_load_per_core_override(0.0)
}

/// AC3/AC4: a guarded native role tick that exits 0 having never used a
/// `loom_*` tool is reported as a FAILED launch, not a success.
#[test]
#[serial]
fn an_exit_zero_opencode_tick_with_no_loom_tool_use_is_a_failed_tick() {
    let _env = EnvGuard::new();
    let workspace = tempfile::tempdir().unwrap();
    opencode_judge_workspace(workspace.path(), TOOLLESS_STREAM);

    let outcome = judge_runner(workspace.path()).invoke("judge", "/loom:judge");

    let RoleTickOutcome::Failure(detail) = &outcome else {
        panic!("a toolless exit-0 native tick must not be Success, got {outcome:?}");
    };
    assert!(detail.contains("toolless launch"), "{detail}");
    assert!(detail.contains("opencode"), "{detail}");
    assert!(detail.contains("loom_read/loom_write/loom_edit/loom_bash"), "{detail}");
}

/// The control the verdict has to be able to tell apart: the same runtime,
/// the same exit code, the same log file — but the binding loaded and the
/// model actually called a `loom_*` tool.
#[test]
#[serial]
fn the_same_tick_that_did_use_a_loom_tool_is_still_a_success() {
    let _env = EnvGuard::new();
    let workspace = tempfile::tempdir().unwrap();
    opencode_judge_workspace(workspace.path(), TOOL_USING_STREAM);

    let outcome = judge_runner(workspace.path()).invoke("judge", "/loom:judge");

    assert_eq!(outcome, RoleTickOutcome::Success, "{outcome:?}");
}

// ---------------------------------------------------------------------------
// Stand-down conditions.
// ---------------------------------------------------------------------------

/// A Claude/Codex tick has no `loom_*` tools and no native event stream at
/// all, so "zero `loom_*` uses" there is the healthy, universal case — this
/// verdict must never fire for one.
#[test]
fn a_non_native_runtime_is_never_judged_toolless() {
    let log = log_with(ANCHOR, TOOLLESS_STREAM);
    for runtime in ["claude", "codex", "aider"] {
        assert_eq!(detect_in(&log, Some(&resolved(runtime)), ANCHOR), None, "{runtime}");
    }
    // …and the native one on identical input does fire, so the guard above
    // is what is doing the work, not an unparseable fixture.
    assert!(detect_in(&log, Some(&resolved("opencode")), ANCHOR).is_some());
    assert!(detect_in(&log, Some(&resolved("pi")), ANCHOR).is_some());
}

/// An invocation that opted out of admission entirely (a `spawn_bin`
/// override, which is how every non-end-to-end role-runner test spawns) has
/// no resolved runtime to judge — the same guard the provider-health bridge
/// applies.
#[test]
fn an_invocation_with_no_admission_is_left_alone() {
    assert_eq!(detect_in(&log_with(ANCHOR, TOOLLESS_STREAM), None, ANCHOR), None);
}

/// The per-role log is shared and append-only. A tick whose own anchor is
/// missing cannot be scoped, so it gets no verdict — rather than inheriting
/// a neighbouring tick's events in either direction.
#[test]
fn a_tick_whose_anchor_is_absent_gets_no_verdict() {
    let log = log_with("2026-09-21T01:00:00+00:00", TOOLLESS_STREAM);
    assert_eq!(detect_in(&log, Some(&resolved("opencode")), ANCHOR), None);
    assert_eq!(detect_in(&log, Some(&resolved("opencode")), ""), None);
    assert_eq!(detect_in("", Some(&resolved("opencode")), ANCHOR), None);
}

/// Scoping runs forward from the anchor: a PREVIOUS tick's `loom_*` tool use
/// must not exonerate this tick, and this tick's own events are the only
/// ones counted.
#[test]
fn an_earlier_ticks_tool_use_does_not_exonerate_this_tick() {
    let log = format!(
        "{}{}",
        log_with("2026-09-21T01:00:00+00:00", TOOL_USING_STREAM),
        log_with(ANCHOR, TOOLLESS_STREAM)
    );
    assert!(detect_in(&log, Some(&resolved("opencode")), ANCHOR).is_some());
    // Symmetrically, anchoring on the earlier tick sees its own tool use.
    assert_eq!(detect_in(&log, Some(&resolved("opencode")), "2026-09-21T01:00:00+00:00"), None);
}

/// The conservatism that keeps this from becoming a fleet-wide outage the
/// day a harness renames its event types: a stream with no parseable native
/// events is a gap in Loom's own observation, so the tick keeps its
/// pre-#8448 success verdict.
#[test]
fn a_stream_this_module_cannot_parse_yields_no_verdict() {
    let log = log_with(ANCHOR, "Some entirely different output format\nwith no JSON events at all");
    assert_eq!(detect_in(&log, Some(&resolved("opencode")), ANCHOR), None);
}

/// A run that used only the harness's OWN unguarded tools (`write`, not
/// `loom_write`) is still toolless by this verdict — the fall-open case.
#[test]
fn a_run_using_only_unguarded_harness_tools_is_still_toolless() {
    let log = log_with(
        ANCHOR,
        concat!(
            r#"{"type":"tool_use","name":"write","input":{"filePath":"a.txt"}}"#,
            "\n",
            r#"{"type":"step_finish"}"#,
        ),
    );
    let detail = detect_in(&log, Some(&resolved("opencode")), ANCHOR).unwrap();
    assert!(detail.contains("0 loom_* tool uses"), "{detail}");
}
