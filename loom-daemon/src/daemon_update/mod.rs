//! `loom-daemon daemon-update` — the implementation behind
//! `.loom/scripts/cli/loom-daemon-update.sh` (#8088, epic #7810's third and
//! last port).
//!
//! # What it does
//!
//! Detect whether the resolved `loom-daemon` is stale, get a fresh one (a
//! verified GitHub Release artifact when one resolves, else `cargo build
//! --release` on a freshly ff-synced checkout), provision it, and restart the
//! running daemon **with exactly the flags it already had** — never more,
//! never fewer. A daemon that was NOT running is left stopped: this never
//! widens FLAGS-OFF by starting autonomy that was not already running.
//!
//! # The hazard this port had to design for
//!
//! The shell wrapper was a *different process* from the daemon it replaced.
//! This subcommand is not: `daemon-update` runs inside a `loom-daemon` binary,
//! and the file it provisions over is very often the binary that is executing
//! right now. See [`selfrepl`] for the three-part answer — why the unlink-and-
//! replace path is the only safe write, why a version check after the roll
//! must read the NEW process rather than a cached resolution, and which
//! existing helper is the wrong one to reach for.
//!
//! # Exit codes (contract — the dispatcher, the daemon and the retained suite
//! all branch on these)
//!
//! | code | meaning |
//! |------|---------|
//! | 0 | up to date (no-op), or rebuild+provision+restart succeeded |
//! | 1 | usage error / not a source checkout / build or provision failure / the ff-only sync could not apply / an artifact verification failure / `--fetch` with no resolvable artifact |
//! | 3 | (`--check` only) update available |
//! | 4 | build verification FAILED — the built binary embeds the wrong commit |
//! | 5 | post-provision verification FAILED — the destination is not what this run produced |
//! | 6 | supervised restart REFUSED by the running (old) binary |
//! | 7 | restart ACK'd but the supervisor never relaunched, and the self-heal also failed |
//! | 8 | drain fail-safe preserved — not a failure |
//!
//! The full behavioural reference is `--help`, rendered from `help.txt`, which
//! is the script's own leading comment block verbatim.

pub mod args;
pub mod artifact;
pub mod entry_points;
pub mod notice;
pub mod out;
pub mod paths;
pub mod provision;
pub mod rebuild;
pub mod relaunch;
pub mod restart;
pub mod restart_flow;
pub mod selfrepl;
pub mod supervisor;
pub mod sync;
pub mod util;
pub mod verify;

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use args::{Args, FetchMode};
use supervisor::{Detected, Manager};

/// What `$0` was for the shell: the entry point an operator actually typed.
///
/// The stub exports it, because `current_exe()` reports
/// `~/.local/bin/loom-daemon` on every normal install and no message should
/// tell an operator to re-run *that*.
static ARGV0: OnceLock<String> = OnceLock::new();

/// The scratch dirs/files the shell's `_LOOM_FETCH_TMPDIRS` + `trap … EXIT`
/// owned. Every exit path goes through [`exit`], which drains this first, so
/// an abort never leaves an unverified artifact or a build-JSON temp file
/// lying around.
static TMPDIRS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Add a path to the cleanup set.
pub fn register_tmpdir(path: PathBuf) {
    if let Ok(mut set) = TMPDIRS.lock() {
        set.push(path);
    }
}

/// `mktemp "${TMPDIR:-/tmp}/<prefix>.XXXXXX"` — a unique scratch path,
/// registered for cleanup. The file itself is created by the caller.
#[must_use]
pub fn scratch_file(prefix: &str) -> PathBuf {
    let dir = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let path = dir.join(format!("{prefix}.{}{:06}", std::process::id(), nanos % 1_000_000));
    register_tmpdir(path.clone());
    path
}

/// The shell's `_cleanup_fetch_tmpdirs` + `exit`, as one call.
///
/// Every `exit` in this module goes through here. A bare `std::process::exit`
/// would skip the cleanup the shell's `trap … EXIT` performed on EVERY exit
/// path, including the hard aborts.
pub fn exit(code: i32) -> ! {
    if let Ok(set) = TMPDIRS.lock() {
        for path in set.iter() {
            let _ = std::fs::remove_dir_all(path);
            let _ = std::fs::remove_file(path);
        }
    }
    std::process::exit(code)
}

/// `$(basename "$0")`.
#[must_use]
pub fn argv0_basename() -> String {
    let argv0 = ARGV0
        .get()
        .map(String::as_str)
        .unwrap_or("loom-daemon-update.sh");
    Path::new(argv0)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| argv0.to_string())
}

/// Never returns.
pub fn run(argv: &[String], argv0: &str) -> ! {
    let _ = ARGV0.set(argv0.to_string());

    let a = Args::parse(argv);

    // `--resolve-json` (#7609): stdout is reserved for the single JSON object,
    // so every informational line from here on is diverted to stderr. Doing it
    // once, here, is what lets the resolution block far below be shared
    // verbatim with the ordinary update path instead of being duplicated
    // behind a "quiet" flag.
    if a.resolve_json {
        out::divert_stdout_to_stderr();
    }

    let repo_root = resolve_repo_root(&a);
    let state = Stage::build(&a, repo_root);
    stage_two(&a, state)
}

/// `REPO_ROOT` + `DAEMON_STATE_HOME`, after the #5140 self-location fallback
/// and the #4229 machine-mode override.
struct RepoRoots {
    repo_root: PathBuf,
    daemon_state_home: PathBuf,
    machine_checkout: Option<PathBuf>,
}

fn resolve_repo_root(_a: &Args) -> RepoRoots {
    let pwd = std::env::var("PWD").unwrap_or_else(|_| {
        std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    });
    let mut repo_root = paths::find_repo_root(None);

    // ---- self-location fallback (#5140) ----
    // This script rebuilds FROM SOURCE, and its own location is unambiguous
    // when it is invoked by absolute path from outside any checkout (the
    // reported case: `bash ~/GitHub/loom/.loom/scripts/cli/loom-daemon-update.sh`
    // from $HOME). Announced on stderr, never silent: choosing a different
    // checkout than $PWD implies is exactly the kind of thing an operator must
    // be able to see in the log.
    //
    // Deliberately scoped to the "no checkout at all" case. When $PWD DOES
    // resolve to a Loom checkout that simply has no `loom-daemon/` crate (a
    // consumer repo), the refusal stands — retargeting the machine checkout
    // from inside another repo is opt-in via LOOM_MACHINE_CHECKOUT, not a
    // guess.
    //
    // The shell derived this from its own `$SCRIPT_DIR`. The binary cannot:
    // `current_exe()` points at `~/.local/bin/loom-daemon`, where no checkout
    // lives. The stub exports its directory as `LOOM_UPDATE_CLI_DIR`, which is
    // the same value the shell had.
    let self_repo_root = util::env_non_empty("LOOM_UPDATE_CLI_DIR")
        .map(PathBuf::from)
        .and_then(|d| paths::find_repo_root(Some(&d)));
    let script_dir_display =
        util::env_non_empty("LOOM_UPDATE_CLI_DIR").unwrap_or_else(|| "<unknown>".to_string());
    if repo_root.is_none() && paths::is_loom_source_checkout(self_repo_root.as_deref()) {
        let chosen = self_repo_root.clone().unwrap_or_default();
        out::warn(&format!(
            "$PWD ({pwd}) is not inside a Loom source checkout; using this script's own checkout: {}",
            chosen.display()
        ));
        repo_root = self_repo_root;
    }

    // ---- machine-mode source-tree override (Epic #3835 Phase 3b, #4229) ----
    // Gap 1: this rebuilds FROM SOURCE and used to resolve that source tree by
    // walking up from $PWD — so from a consumer repo, or a non-repo directory,
    // it refused with "only works inside a Loom source checkout" even though
    // the `loom` dispatcher had ALREADY resolved and validated the machine
    // checkout before exec'ing here.
    let machine_checkout = util::env_non_empty("LOOM_MACHINE_CHECKOUT").map(PathBuf::from);
    if let Some(mc) = &machine_checkout {
        if !mc.is_dir() {
            out::err(&format!("LOOM_MACHINE_CHECKOUT does not exist: {}", mc.display()));
            exit(1);
        }
        return RepoRoots {
            repo_root: mc.clone(),
            daemon_state_home: util::home().join(".loom"),
            machine_checkout,
        };
    }
    match repo_root {
        Some(root) => RepoRoots {
            daemon_state_home: root.join(".loom"),
            repo_root: root,
            machine_checkout: None,
        },
        None => {
            // #5140: name what was searched and what is required, so this
            // never reads as "your checkout is broken" when the real answer is
            // "you are standing in the wrong directory".
            out::err(&format!(
                "Not in a Loom workspace: neither $PWD ({pwd}) nor this script's own location ({script_dir_display}) is inside a Loom checkout."
            ));
            out::say_err("A Loom checkout is a directory containing BOTH .git and .loom/ (a bare ~/.loom, e.g. the token pool, is not one).");
            out::say_err(
                "cd into a Loom source checkout, or set LOOM_MACHINE_CHECKOUT=<path-to-checkout>.",
            );
            exit(1);
        }
    }
}

/// Everything resolved before the first decision is made.
struct Stage {
    roots: RepoRoots,
    daemon_dir: PathBuf,
    flags_file: PathBuf,
    start_script: PathBuf,
    stop_script: PathBuf,
    sync: sync::SyncState,
    sup: Detected,
    installed_commit: String,
    installed_version: String,
    source_commit: String,
    source_version: String,
    update_needed: bool,
    artifact_mode: bool,
    artifact_tag: String,
    artifact_version: String,
    artifact_target: String,
    artifact_fallback_reason: String,
    fetch_repo_slug: String,
    fetch_latest_version: String,
    fetch_release_behind_source: bool,
    drain: bool,
    drain_defaulted: bool,
    drain_poll_secs: String,
}

impl Stage {
    fn build(a: &Args, roots: RepoRoots) -> Stage {
        let daemon_dir = roots.repo_root.join("loom-daemon");
        if !daemon_dir.join("Cargo.toml").is_file() {
            out::err(&format!(
                "No loom-daemon/Cargo.toml found at {} (repo root resolved as {}).",
                daemon_dir.display(),
                roots.repo_root.display()
            ));
            out::say_err("loom-daemon-update.sh rebuilds FROM SOURCE and only works inside a Loom source checkout.");
            out::say_err(
                "cd into a Loom source checkout, or set LOOM_MACHINE_CHECKOUT=<path-to-checkout>.",
            );
            exit(1);
        }

        // ---- pid-file resolution (#6386) ----
        // LOOM_PID_FILE is TIER 1 here for the SAME reason (and with the same
        // precedence) as in loom-daemon-stop.sh: this restarts the daemon by
        // invoking that stop script, so if the two disagreed about which pid
        // file is "the daemon", the restart would plan against one file while
        // the stop it delegates to acts on another. FLAGS_FILE stays
        // state-home-derived — LOOM_PID_FILE names the pid file, not the state
        // home.
        let pid_file = util::env_non_empty("LOOM_PID_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| roots.daemon_state_home.join(".daemon.pid"));
        let flags_file = roots.daemon_state_home.join(".daemon.flags");

        let start_script =
            paths::resolve_lifecycle_script(&roots.repo_root, "loom-daemon-start.sh");
        let stop_script = paths::resolve_lifecycle_script(&roots.repo_root, "loom-daemon-stop.sh");
        let (Some(start_script), Some(stop_script)) = (start_script, stop_script) else {
            out::err(&format!(
                "Could not resolve loom-daemon-start.sh / loom-daemon-stop.sh under {} (.loom/scripts/cli or defaults/scripts/cli).",
                roots.repo_root.display()
            ));
            exit(1);
        };

        // --resolve-json is strictly read-only (#7609): it must never `git
        // fetch` or fast-forward the operator's checkout just to answer "what
        // is the latest release artifact?", which has nothing to do with the
        // local source tree.
        let mut sync_state = sync::SyncState::new();
        if !a.resolve_json && !sync::sync_with_origin(&roots.repo_root, a, &mut sync_state) {
            exit(1);
        }

        let sup = Detected::probe(&pid_file);

        // ---- staleness detection ----
        let daemon_bin = crate::daemon_start::paths::locate_daemon_bin(&roots.repo_root);
        // Compared against the binary the detected supervisor actually
        // launches, falling back to the PATH-resolved one only when no
        // supervisor is detected or its exec path could not be determined
        // (#6009) — PATH resolution alone cannot notice a supervisor pointed
        // at a different absolute path than the PATH-resolved one.
        let (staleness_bin, staleness_source) = match sup.supervisor_bin.as_ref() {
            Some(b) => (Some(b.clone()), format!("{} supervisor config", sup.manager.word())),
            None => (daemon_bin.clone(), "PATH resolution".to_string()),
        };

        let mut installed_commit = "unknown".to_string();
        let mut installed_version = String::new();
        if let Some(bin) = staleness_bin.as_ref().filter(|b| util::is_executable(b)) {
            let output = util::version_output(bin);
            let extracted = util::extract_commit(&output);
            if !extracted.is_empty() {
                installed_commit = extracted;
            }
            installed_version = util::extract_version(&output);
        }

        let source_commit = util::git(&roots.repo_root, &["rev-parse", "--short", "HEAD"])
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unknown".to_string());
        // The source tree's own VERSION file (#5517 keeps this in sync with
        // Cargo.toml et al.) — used purely for the #6010 gap-visibility note.
        let source_version = std::fs::read_to_string(roots.repo_root.join("VERSION"))
            .map(|t| t.chars().filter(|c| !c.is_whitespace()).collect::<String>())
            .unwrap_or_default();

        out::say(&format!(
            "Installed binary: {} (commit {installed_commit}, resolved via {staleness_source})",
            staleness_bin
                .as_ref()
                .map(|b| b.display().to_string())
                .unwrap_or_else(|| "<none found>".to_string())
        ));
        // Divergence advisory (#6009 AC2), compared through the realpath
        // helper so the two spellings of the SAME file (the common
        // `/usr/local/bin/loom-daemon` symlink into `~/.local/bin`) are not
        // reported as a divergence — only a genuinely different file is. The
        // helper answers "" for a path that does not exist (a supervisor
        // config still pointing at a deleted binary), so each side falls back
        // to its raw spelling rather than collapsing to `"" == ""`.
        if let (Some(sup_bin), Some(path_bin)) = (sup.supervisor_bin.as_ref(), daemon_bin.as_ref())
        {
            let mut sup_real = util::realpath(sup_bin);
            if sup_real.is_empty() {
                sup_real = sup_bin.display().to_string();
            }
            let mut path_real = util::realpath(path_bin);
            if path_real.is_empty() {
                path_real = path_bin.display().to_string();
            }
            if sup_real != path_real {
                let manager = sup.manager.word();
                out::warn(&format!(
                    "PATH-resolved loom-daemon ({}) is NOT the binary {manager} will actually launch ({}) — the staleness comparison above deliberately used the {manager}-managed binary, not the PATH one, since only the former is what the running daemon comes back on. If the PATH one is a leftover entry point, the stale-entry-point advisory below says whether --prune-stale-entry-points can remove it.",
                    path_bin.display(),
                    sup_bin.display()
                ));
            }
        }
        out::say(&format!("Source tree HEAD:  {source_commit}"));
        if let Some(mc) = roots.machine_checkout.as_ref() {
            let _ = mc;
            out::say(&format!(
                "Source tree:       {} (machine checkout, LOOM_MACHINE_CHECKOUT)",
                roots.repo_root.display()
            ));
        }
        if sync_state.ff_synced {
            out::say(&format!(
                "Source tree:       fast-forwarded to origin/{} before this run (#4330).",
                sync_state.default_branch
            ));
        }

        let mut update_needed = false;
        if staleness_bin.is_none() {
            out::say(&format!(
                "No loom-daemon binary currently resolvable (checked: {staleness_source}) — a build is needed. PATH search checked:"
            ));
            for line in crate::daemon_start::paths::bin_search_paths(&roots.repo_root) {
                out::say(&format!("  - {line}"));
            }
            update_needed = true;
        } else if installed_commit == "unknown" || source_commit == "unknown" {
            out::warn(&format!(
                "Could not determine one or both commits (installed={installed_commit}, source={source_commit}) — staleness unknown; treating as needing a rebuild to be safe."
            ));
            update_needed = true;
        } else if installed_commit != source_commit {
            update_needed = true;
        }

        // ---- artifact-fetch resolution (Epic #4990 Phase 3, #5020) ----
        // Read-only resolution (no downloads yet). When a newer release
        // resolves for this host's platform it takes precedence over the
        // source-commit comparison above. Any resolution failure softly falls
        // back — UNLESS the operator forced `--fetch`, which is checked
        // further below once UPDATE_NEEDED is known.
        let mut artifact_mode = false;
        let mut artifact_tag = String::new();
        let mut artifact_version = String::new();
        let mut artifact_target = String::new();
        let mut artifact_fallback_reason = String::new();
        let mut fetch_repo_slug = String::new();
        let mut fetch_latest_version = String::new();
        // Gap-visibility (#6010): true when the newest resolved release is
        // behind the CURRENT source tree's VERSION — independent of
        // ARTIFACT_MODE, which only compares against the (possibly much older)
        // INSTALLED_VERSION. This is the condition that made `--fetch`
        // unusable fleet-wide once releases fell behind `main`.
        let mut fetch_release_behind_source = false;
        if a.fetch_mode != FetchMode::Off {
            let r = artifact::fetch_resolve_latest(&roots.repo_root);
            if r.ok {
                fetch_repo_slug = r.repo_slug.clone();
                fetch_latest_version = r.latest_version.clone();
                let installed_for_cmp = if installed_version.is_empty() {
                    "0.0.0"
                } else {
                    installed_version.as_str()
                };
                let cmp = util::semver_compare(&r.latest_version, installed_for_cmp);
                // Strictly newer wins. An EQUAL version only wins under an
                // explicit --fetch: `--force` alone keeps its established
                // meaning ("rebuild this checkout even though it isn't
                // stale"), which an operator running it inside a source tree
                // would be surprised to see silently turn into a download.
                let newer = cmp == std::cmp::Ordering::Greater
                    || (cmp == std::cmp::Ordering::Equal && a.fetch_mode == FetchMode::Force);
                if newer {
                    artifact_mode = true;
                    artifact_tag = r.latest_tag.clone();
                    artifact_version = r.latest_version.clone();
                    artifact_target = r.target.clone();
                    update_needed = true;
                    out::say(&format!(
                        "Release artifact available: {artifact_tag} (target {artifact_target}) — preferring fetch over a local rebuild."
                    ));
                } else {
                    out::say(&format!(
                        "Latest release {} ({}) is not newer than the installed version ({}) — nothing to fetch; falling back to the local source-tree comparison.",
                        r.latest_tag,
                        r.latest_version,
                        if installed_version.is_empty() { "unknown" } else { installed_version.as_str() }
                    ));
                }
                if !source_version.is_empty()
                    && util::semver_compare(&r.latest_version, &source_version)
                        == std::cmp::Ordering::Less
                {
                    fetch_release_behind_source = true;
                    out::warn(&format!(
                        "Artifact path cannot reach current source: newest release {} ({}) is behind this source tree's VERSION ({source_version}) — a forced '--fetch' will hard-fail until a release >= {source_version} is cut; '--no-fetch' (source build) remains available in the meantime.",
                        r.latest_tag, r.latest_version
                    ));
                }
            } else {
                artifact_fallback_reason = r.reason.clone();
                out::warn(&format!(
                    "Artifact-fetch: {artifact_fallback_reason} — falling back to the local source-build path."
                ));
            }
        }

        // ---- --resolve-json: report the resolution and exit (#7609) ----
        // Deliberately placed immediately after the resolution block above and
        // BEFORE anything that writes (prune, provision, build, restart) —
        // this mode exists to answer a question, never to act on the answer.
        if a.resolve_json {
            resolve_json_and_exit(a, &roots.repo_root, staleness_bin.as_deref());
        }

        // ---- --prune-stale-entry-points: standalone action, then exit ----
        // Checked BEFORE the advisory below (skipping it, not running it
        // first) — this flag exists precisely so an operator does not have to
        // read the warning and act on it by hand.
        if a.prune_stale {
            exit(i32::from(!entry_points::prune_stale(daemon_bin.as_deref())));
        }

        // Advisory only, and deliberately placed here so it is reported on
        // EVERY path — --check, --dry-run, an up-to-date no-op, and a full
        // rebuild alike. A stale entry point is invisible precisely when the
        // daemon looks healthy (#4079).
        entry_points::warn_stale(daemon_bin.as_deref());

        // ---- drain-restart default selection (Issue #5138) ----
        // On systemd an IMMEDIATE (non-drained) restart is actively
        // destructive (#5119): the daemon exits 0, but its role-run/sweep
        // children remain in the unit's cgroup, so the stop job can sit in
        // `deactivating` past TimeoutStopSec while systemd SIGKILLs them,
        // landing the unit in `failed` with `Restart=on-success` never firing
        // — a real outage, not merely lossy telemetry. On launchd/pidfile an
        // immediate restart is "only" lossy (#5084), so the pre-#5138 default
        // is unchanged there.
        let mut drain = a.drain;
        let mut drain_defaulted = false;
        if !a.restart_now && !a.drain && sup.manager == Manager::Systemd {
            drain = true;
            drain_defaulted = true;
        }
        if drain && sup.manager != Manager::Launchd && sup.manager != Manager::Systemd {
            out::warn("--drain (or LOOM_DAEMON_UPDATE_DRAIN=1) was given, but loom-daemon is not launchd- or systemd-managed — there is no supervisor to relaunch it, so drain mode has no effect here. Proceeding with the ordinary stop+start restart.");
        }
        // A drain can legitimately take up to its own --timeout (daemon
        // default 1800s) before it either relaunches or hits the fail-safe, so
        // the fast ~30s LOOM_DAEMON_RESTART_POLL_SECS default used for an
        // immediate restart would false-negative on every real drain. Mirrors
        // fleet/drain.rs's own WAIT_EXIT_GRACE_SECS=60 pattern.
        let drain_poll_secs = if drain {
            util::env_non_empty("LOOM_DAEMON_DRAIN_POLL_SECS").unwrap_or_else(|| {
                let base = a
                    .drain_timeout
                    .as_deref()
                    .and_then(|t| t.parse::<u64>().ok())
                    .unwrap_or(1800);
                (base + 60).to_string()
            })
        } else {
            String::new()
        };

        Stage {
            roots,
            daemon_dir,
            flags_file,
            start_script,
            stop_script,
            sync: sync_state,
            sup,
            installed_commit,
            installed_version,
            source_commit,
            source_version,
            update_needed,
            artifact_mode,
            artifact_tag,
            artifact_version,
            artifact_target,
            artifact_fallback_reason,
            fetch_repo_slug,
            fetch_latest_version,
            fetch_release_behind_source,
            drain,
            drain_defaulted,
            drain_poll_secs,
        }
    }

    fn final_line(&self) -> notice::FinalLine<'_> {
        notice::FinalLine {
            artifact_mode: self.artifact_mode,
            artifact_tag: &self.artifact_tag,
            artifact_version: &self.artifact_version,
            artifact_target: &self.artifact_target,
            default_branch: &self.sync.default_branch,
            origin_commit: &self.sync.origin_commit,
        }
    }
}

/// `--resolve-json` — delegate the resolution and exit with its own code.
///
/// The shell `exec`'d `loom-daemon release-resolve` so the subcommand's exit
/// code reached the caller unmodified: `1` here means "no artifact resolved",
/// which the daemon's auto-update tick reads as DATA and falls back to its
/// source path on. Remapping it would turn an ordinary outcome into a failure.
/// Calling the library in-process preserves that exactly — and removes the
/// shell's "no loom-daemon binary implementing release-resolve could be
/// resolved" branch, which cannot occur when the resolver and the caller are
/// the same binary.
fn resolve_json_and_exit(a: &Args, repo_root: &Path, installed_bin: Option<&Path>) -> ! {
    use crate::release_resolve::{build_time_repo, emit, resolve, Inputs, Resolution};

    let inputs = Inputs {
        repo_root,
        target_override: std::env::var("LOOM_DAEMON_UPDATE_TARGET").ok(),
        repo_override: std::env::var("LOOM_DAEMON_UPDATE_GH_REPO").ok(),
        machine_checkout: std::env::var("LOOM_MACHINE_CHECKOUT")
            .ok()
            .map(PathBuf::from),
        build_time_repo: build_time_repo(),
        installed_bin: installed_bin.map(Path::to_path_buf),
        fetch_disabled: a.fetch_mode == FetchMode::Off
            || std::env::var("LOOM_DAEMON_UPDATE_FETCH")
                .map(|v| matches!(v.trim(), "0" | "false" | "no" | "off"))
                .unwrap_or(false),
    };
    let resolution = resolve(&inputs);
    // Exactly one line on stdout, and nothing else ever written there.
    println!("{}", emit::to_json(&resolution));
    exit(match resolution {
        Resolution::Resolved(_) => 0,
        Resolution::Unresolved(_) => 1,
    })
}

/// From `--check` onwards.
fn stage_two(a: &Args, s: Stage) -> ! {
    // ---- --check: report only, no writes ----
    if a.check_only {
        out::say(&s.sup.describe_manager());
        if s.fetch_release_behind_source {
            out::warn(&format!(
                "Release gap: installed {}, newest release {}, source {} — the artifact-fetch path cannot reach current source until a release >= {} is cut.",
                none_or(&s.installed_version),
                none_or(&s.fetch_latest_version),
                none_or(&s.source_version),
                s.source_version
            ));
        }
        if s.update_needed {
            if s.artifact_mode {
                out::warn(&format!(
                    "Update available via release artifact {} (installed={}, latest={}, target={}).",
                    s.artifact_tag,
                    none_or(&s.installed_version),
                    s.artifact_version,
                    s.artifact_target
                ));
            } else {
                out::warn(&format!(
                    "Update available (installed={}, source={}).",
                    s.installed_commit, s.source_commit
                ));
            }
            exit(3);
        }
        out::ok(&format!(
            "loom-daemon binary is already up to date with source HEAD ({}).",
            s.source_commit
        ));
        notice::print_final_installed_line(&s.final_line(), &s.source_commit);
        exit(0);
    }

    let mut update_needed = s.update_needed;
    if a.force && !update_needed {
        out::say("--force given: rebuilding even though the binary already matches source HEAD.");
        update_needed = true;
    }

    if !update_needed {
        // UPDATE_NEEDED compares the installed binary against the CURRENT
        // HEAD. When the checkout is behind origin, a real run fast-forwards
        // first, so HEAD — and therefore that comparison — would change before
        // anything is built. Reporting a bare "Nothing to do" here would hide
        // the pending ff-sync from exactly the mode whose job is to print the
        // plan.
        if a.dry_run
            && !a.allow_stale
            && !s.sync.default_branch.is_empty()
            && s.sync.origin_behind_count > 0
        {
            out::say(&format!(
                "[dry-run] Plan includes fast-forwarding local {0} to origin/{0} ({1} commit(s) behind) before building; the up-to-date check below is against the CURRENT HEAD and may change once that ff-merge applies.",
                s.sync.default_branch, s.sync.origin_behind_count
            ));
        }
        out::ok(&format!(
            "loom-daemon binary is already up to date with source HEAD ({}). Nothing to do.",
            s.source_commit
        ));
        notice::print_final_installed_line(&s.final_line(), &s.source_commit);
        exit(0);
    }

    // An update IS needed at this point. --fetch means "I know a release
    // artifact should exist; don't silently fall back to building from source"
    // — refuse rather than mask a resolution failure.
    if a.fetch_mode == FetchMode::Force && !s.artifact_mode {
        let reason = if s.artifact_fallback_reason.is_empty() {
            String::new()
        } else {
            format!(" ({})", s.artifact_fallback_reason)
        };
        out::err(&format!(
            "--fetch (or LOOM_DAEMON_UPDATE_FETCH=1) was given but no usable release artifact was resolved{reason}."
        ));
        if s.fetch_release_behind_source {
            out::err(&format!(
                "Cause: the newest release ({}) is behind this source tree's VERSION ({}) — no release has been cut yet for the current tree (#6010).",
                none_or(&s.fetch_latest_version),
                none_or(&s.source_version)
            ));
        }
        out::err("Refusing to silently fall back to a source build; re-run without --fetch to allow that, or resolve the cause above.");
        exit(1);
    }

    // ---- resolve the restart plan up front (read-only; safe for --dry-run) ----
    // The flags below are only consulted for the pid-file/nohup restart path —
    // a launchd- or systemd-managed restart replays flags from the plist/unit,
    // not from this file.
    let mut restart_args: Vec<String> = Vec::new();
    let mut flags_from_file = false;
    if s.flags_file.is_file() {
        flags_from_file = true;
        if let Ok(text) = std::fs::read_to_string(&s.flags_file) {
            restart_args.extend(read_flags_file(&text));
        }
    }
    let flags_source = if flags_from_file {
        s.flags_file.display().to_string()
    } else {
        "none (defaulting to FLAGS-OFF bare restart)".to_string()
    };
    let provision_target = provision::provision_target();

    if a.dry_run {
        print_dry_run_plan(a, &s, &provision_target, &restart_args, &flags_source);
        exit(0);
    }

    // ---- rebuild (source) OR fetch (artifact, Epic #4990 Phase 3, #5020) ----
    let (new_bin, built_commit, artifact_version_output, artifact_had_authority) = if s
        .artifact_mode
    {
        out::say("");
        out::say(&format!(
            "Fetching loom-daemon release artifact {} (target {}) from {}...",
            s.artifact_tag, s.artifact_target, s.fetch_repo_slug
        ));
        match artifact::fetch_and_verify_artifact(
            &s.roots.repo_root,
            &s.artifact_target,
            &s.fetch_repo_slug,
            &s.artifact_tag,
        ) {
            Ok(fetched) => {
                // NOTE: the source path's exit-4 "build verification" (built
                // commit == source HEAD) deliberately has NO artifact-mode
                // equivalent — it guards a build.rs staleness defect that
                // cannot exist for a binary this host did not compile. The
                // artifact's integrity was established by the unconditional
                // checksum + present-signature verification, and its arrival
                // at the destination is asserted post-provision.
                let commit_clause = if fetched.commit.is_empty() {
                    String::new()
                } else {
                    format!(", commit {}", fetched.commit)
                };
                out::ok(&format!(
                    "Fetched + verified: {} (release {}{commit_clause})",
                    fetched.bin.display(),
                    s.artifact_tag
                ));
                (fetched.bin, fetched.commit, fetched.version_output, fetched.had_authority)
            }
            Err(artifact::FetchFailure::Verification) => exit(1),
            Err(artifact::FetchFailure::Download) => {
                out::err("Artifact download failed (see above) — the running daemon (if any) was left untouched.");
                exit(1);
            }
        }
    } else {
        if rebuild::ensure_cargo_on_path().is_err() {
            exit(1);
        }
        let new_bin = match rebuild::rebuild(&s.daemon_dir, &s.roots.repo_root) {
            Ok(b) => b,
            Err(_) => exit(1),
        };
        let built_commit = rebuild::verify_built_commit(&new_bin, &s.source_commit);
        (new_bin, built_commit, String::new(), None)
    };

    // ---- sign (Darwin-only, best-effort, non-fatal, #4016) ----
    let provision_script = provision::provision_script(&s.roots.repo_root);
    if let Some(script) = provision_script.as_ref() {
        if !s.artifact_mode {
            provision::sign_daemon_binary(script, &new_bin);
        }
    }

    // ---- provision ----
    let run_verifiers = |dest: Option<&Path>| {
        if s.artifact_mode {
            verify::verify_destination_artifact(
                dest,
                &artifact_version_output,
                artifact_had_authority,
            );
        } else {
            verify::verify_destination_binary(dest, &s.source_commit);
        }
        // #6009: also confirm the SUPERVISOR's own config points at this exact
        // destination, not a different (stale) path.
        verify::verify_supervisor_matches_provisioned(dest, &s.sup);
    };

    if let Some(explicit) = util::env_non_empty("LOOM_DAEMON_BIN") {
        // Explicit operator override — provision directly to that exact path
        // (the one loom-daemon-start.sh will resolve to next), rather than the
        // machine-level default.
        let dest = PathBuf::from(&explicit);
        selfrepl::announce_if_self(&dest);
        if provision::install_to(&new_bin, &dest) {
            out::ok(&format!("Provisioned loom-daemon -> {}", dest.display()));
        } else {
            out::err(&format!("Failed to provision to LOOM_DAEMON_BIN={}", dest.display()));
            exit(1);
        }
        run_verifiers(Some(&dest));
    } else if let Some(script) = provision_script.as_ref() {
        // The machine-level destination is the one that IS this binary on a
        // normal fleet host — see `selfrepl` for why the write below is an
        // unlink-and-create and why every check after it re-execs the path.
        selfrepl::announce_if_self(&provision_target);
        match provision::provision_machine_daemon(script, &new_bin, &s.roots.repo_root) {
            provision::ProvisionOutcome::Provisioned(dest) => {
                // provision_machine_daemon exports the destination it wrote to
                // (even on the version-equality short-circuit) — verify that
                // destination is the expected build so the short-circuit can no
                // longer produce a silent no-op on a real roll (#4053).
                let dest_path = PathBuf::from(&dest);
                run_verifiers(if dest.is_empty() {
                    None
                } else {
                    Some(&dest_path)
                });
            }
            provision::ProvisionOutcome::Failed => {
                // Hard-fail: a soft warn here (the pre-#4053 behaviour) left
                // the exit code at 0, which is exactly the "reports success
                // while shipping nothing" defect this closes.
                out::err(&format!(
                    "Machine-level provisioning FAILED (see above). Refusing to report success; the freshly-built binary is at {0} — set LOOM_DAEMON_BIN={0} to use it directly.",
                    new_bin.display()
                ));
                exit(1);
            }
            provision::ProvisionOutcome::NotDefined => {
                provision::warn_no_provision_script(&new_bin);
            }
        }
    } else {
        provision::warn_no_provision_script(&new_bin);
    }

    restart_flow::run(restart_flow::Plan {
        args: a,
        sup: &s.sup,
        manager: s.sup.manager,
        provision_target: &provision_target,
        start_script: &s.start_script,
        stop_script: &s.stop_script,
        flags_file: &s.flags_file,
        flags_from_file,
        flags_source: &flags_source,
        restart_args: &restart_args,
        drain: s.drain,
        drain_defaulted: s.drain_defaulted,
        drain_poll_secs: &s.drain_poll_secs,
        built_commit: &built_commit,
        final_line: s.final_line(),
    })
}

/// `${VAR:-unknown}` for the handful of places the script rendered an
/// unresolved value as the literal word `unknown`.
fn none_or(value: &str) -> &str {
    if value.is_empty() {
        "unknown"
    } else {
        value
    }
}

fn print_dry_run_plan(
    a: &Args,
    s: &Stage,
    provision_target: &Path,
    restart_args: &[String],
    flags_source: &str,
) {
    out::say("");
    if s.artifact_mode {
        out::say(&format!(
            "[dry-run] Would fetch + verify release artifact {} (target {}) from {} — checksum unconditional, signature verified when present. No 'cargo build' would run.",
            s.artifact_tag, s.artifact_target, s.fetch_repo_slug
        ));
    } else {
        if a.allow_stale {
            out::say("[dry-run] --allow-stale given: would build the current checkout as-is (no fetch/ff-merge).");
        } else if !s.sync.default_branch.is_empty() && s.sync.origin_behind_count > 0 {
            out::say(&format!(
                "[dry-run] Plan includes fast-forwarding local {0} to origin/{0} ({1} commit(s) behind) before building; would abort instead of building stale if the ff-merge cannot apply.",
                s.sync.default_branch, s.sync.origin_behind_count
            ));
        }
        if !s.artifact_fallback_reason.is_empty() {
            out::say(&format!(
                "[dry-run] Artifact-fetch was not used: {}.",
                s.artifact_fallback_reason
            ));
        }
        out::say(&format!(
            "[dry-run] Would run: (cd {} && cargo build --release --message-format=json-render-diagnostics)",
            s.daemon_dir.display()
        ));
    }
    out::say(&format!(
        "[dry-run] Would provision the fresh binary to: {}",
        provision_target.display()
    ));
    let invoke = restart::build_restart_invoke_args(
        s.drain,
        a.drain_timeout.as_deref(),
        a.force_after_timeout,
    )
    .join(" ");
    if a.no_restart {
        out::say(
            "[dry-run] --no-restart given: would leave the running daemon (if any) untouched.",
        );
    } else if s.sup.manager == Manager::Launchd {
        if s.drain {
            out::say(&format!(
                "[dry-run] loom-daemon is launchd-managed (label {}) — would restart via '{} {invoke}' (Issue #5138, the #4090 drain primitive): pauses dispatch, waits for in-flight sweeps to finish (preserving sweep.completed/sweep.outcome telemetry, #5084), THEN relaunches. A drain timeout without --force-after-timeout leaves the pre-update binary running (fail-safe, exit 8) instead of cancelling sweeps.",
                s.sup.launchd_label,
                provision_target.display()
            ));
        } else {
            out::say(&format!(
                "[dry-run] loom-daemon is launchd-managed (label {}) — would restart via '{} restart' (the #4077 supervised primitive); .daemon.flags is NOT consulted (the plist's EnvironmentVariables carries the equivalent config).",
                s.sup.launchd_label,
                provision_target.display()
            ));
        }
    } else if s.sup.manager == Manager::Systemd {
        if s.drain {
            if s.drain_defaulted {
                out::say(&format!(
                    "[dry-run] loom-daemon is systemd-managed (unit {}) — would restart via '{} {invoke}', the systemd DEFAULT since Issue #5138 (an immediate restart there can kill in-flight sweeps and land the unit in 'failed', #5119): pauses dispatch, waits for in-flight sweeps to finish, THEN relaunches. Pass --restart-now to opt back into an immediate (non-drained) restart.",
                    s.sup.systemd_unit,
                    provision_target.display()
                ));
            } else {
                out::say(&format!(
                    "[dry-run] loom-daemon is systemd-managed (unit {}) — would restart via '{} {invoke}' (Issue #5138, the #4090 drain primitive): pauses dispatch, waits for in-flight sweeps to finish, THEN relaunches. A drain timeout without --force-after-timeout leaves the pre-update binary running (fail-safe, exit 8) instead of cancelling sweeps.",
                    s.sup.systemd_unit,
                    provision_target.display()
                ));
            }
        } else {
            out::say(&format!(
                "[dry-run] loom-daemon is systemd-managed (unit {}) — --restart-now given: would restart IMMEDIATELY (non-drained) via '{} restart', which can kill in-flight sweeps and land the unit in 'failed' if any are running (#5119).",
                s.sup.systemd_unit,
                provision_target.display()
            ));
        }
    } else if s.sup.was_running {
        out::say(&format!(
            "[dry-run] Would stop + restart loom-daemon with flags from {flags_source}: {}",
            args_or_none(restart_args)
        ));
    } else {
        out::say("[dry-run] loom-daemon is not currently running — would NOT start it (this script never widens FLAGS-OFF by starting autonomy that wasn't already running).");
    }
}

/// `while IFS= read -r line; do [[ -z "$line" ]] && continue; …; done < "$FLAGS_FILE"`
///
/// Two properties of that loop are load-bearing, and the idiomatic Rust
/// spelling (`text.lines().filter(|l| !l.is_empty())`) gets ONE of them wrong:
///
/// * `IFS=` (empty) means `read` performs **no** word splitting and strips
///   **no** leading or trailing whitespace, so `"  --work-finder"` is replayed
///   with its spaces. `lines()` agrees. Kept.
/// * `read` returns non-zero at EOF **without a delimiter**, so a final line
///   that is not newline-terminated never enters the loop body — the shell
///   silently DROPS it. `lines()` yields it. That is the divergence
///   `tests/differential_daemon_update.rs` found (case
///   `D/--dry-run/no-trailing-newline/live`), and none of the three retained
///   suites writes an unterminated `.daemon.flags`.
///
/// Preserved bug-for-bug, deliberately, on two grounds. A port is not the
/// place to change behaviour: folded into a 6,000-line rewrite, a "fix" here
/// is indistinguishable from a defect, and there is no test that could tell a
/// reviewer which it was. And the direction the shell errs in is the fail-safe
/// one for THIS file — it replays FEWER autonomy flags than the file names,
/// never more, which is the same direction as the script's own "never widens
/// FLAGS-OFF" contract. `loom-daemon-start.sh` writes this file with a
/// trailing newline on every line, so the case only arises for a hand-edited
/// file. Changing it is a separate, visible decision.
fn read_flags_file(text: &str) -> Vec<String> {
    text.split_inclusive('\n')
        .filter_map(|line| line.strip_suffix('\n'))
        .filter(|line| !line.is_empty())
        .map(std::string::ToString::to_string)
        .collect()
}

/// `${RESTART_ARGS[*]:-<none>}` — the joined flags, or the literal `<none>`.
pub fn args_or_none(restart_args: &[String]) -> String {
    if restart_args.is_empty() {
        "<none>".to_string()
    } else {
        restart_args.join(" ")
    }
}

/// `${RESTART_ARGS[*]:-}` — the joined flags, or the empty string.
pub fn args_or_empty(restart_args: &[String]) -> String {
    restart_args.join(" ")
}
