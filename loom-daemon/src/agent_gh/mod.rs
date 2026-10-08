//! The agent `gh` front: plain `gh` reads ETag-revalidated by default (#10331).
//!
//! Dispatched workers get `gh` → `loom-daemon` first on `PATH`
//! ([`session_path`], applied by `worker_spawn`); interactive sessions get the
//! same order through a SessionStart hook ([`session_env`], #10516). `loom-daemon` started under
//! the name `gh` — or explicitly as `loom-daemon gh <gh argv…>` — lands in
//! [`run`]. Per call:
//!
//! - [`classify::classify`] picks [`classify::Route::EtagView`] /
//!   [`classify::Route::EtagList`] for the `issue|pr view|list --json …`
//!   shapes the in-repo ETag modules reproduce exactly,
//!   [`classify::Route::EtagChecks`] for the `pr checks <N>` shapes
//!   [`pr_checks`] reproduces from REST (#10516), and
//!   [`classify::Route::Passthrough`] for everything else.
//! - An ETag route is served in-process by [`crate::forge_cached_view`] /
//!   [`crate::forge_cached_list`]: a conditional `gh api` request whose `304`
//!   proves the stored body current and costs no primary quota. It is
//!   **never stale**, so plain-`gh` gating reads (ADR-0021) stay correct.
//!   There is deliberately no identical-call TTL here; that stays opt-in via
//!   `gh-cached`.
//! - Passthrough — and any ETag route the module declines at run time —
//!   execs `next_gh` ([`next_gh::resolve`]: `LOOM_GH_BIN`, else the next `gh`
//!   on `PATH`, e.g. the managed launcher #9987) with argv, streams and exit
//!   status byte-identical ([`crate::gh_invocation::transparent::exec`]).
//! - Escape hatch: `LOOM_GH_NO_CACHE=1` (also `GH_CACHE_DISABLE=1`,
//!   `LOOM_ETAG_LIST_DISABLE=1`). Env-only on purpose: plain `gh` rejects an
//!   unknown flag, so a `--fresh` would break every host without the front.
//! - [`SENTINEL_ENV`] marks a call already inside a front, so a nested
//!   invocation passes straight through; past [`MAX_DEPTH`] it refuses (a
//!   resolution loop).
//!
//! Reads served here are recorded against caller [`STATS_CALLER`] in
//! `forge_call_stats` (`loom-daemon status` forge-calls row), so the `304`
//! share is measurable. Every passthrough is one row too ([`ledger`], W5):
//! `agent.gh.<command>`, with the session's role and credential, so agent
//! spend lands in the same per-bucket rollup (`loom-daemon forge calls`).
//! Both kinds of row are stamped with the agent role and `served` /
//! `passthrough` ([`crate::forge_call_stats::agent`], #10607).
//! `forge_etag_store::fetch_conditional` already routes served reads to a
//! repo's reader App when one is configured (#9537).

pub mod classify;
pub mod go_sort;
pub mod ledger;
pub mod next_gh;
pub mod pr_checks;
pub mod session_env;

#[cfg(test)]
mod tests;

use std::ffi::{OsStr, OsString};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use classify::Route;

/// Front recursion depth, exported to every `gh` the front execs.
pub const SENTINEL_ENV: &str = "LOOM_GH_FRONT_ACTIVE";

/// Nested fronts beyond this are a resolution loop, not a real call.
const MAX_DEPTH: u32 = 8;

/// `forge_call_stats` caller for the front's conditional reads.
pub const STATS_CALLER: &str = "agent_gh_front";

/// Opt-out for the worker `PATH` prepend.
pub const OPT_OUT_ENV: &str = "LOOM_GH_SHIM";

/// When this process is the front — started as `gh`, or as `loom-daemon gh …`
/// / `loom-daemon gh-shim …` — run it and exit. Called before clap parsing,
/// so `gh`'s own flags never meet `loom-daemon`'s parser.
pub fn dispatch_if_front() {
    let argv: Vec<OsString> = std::env::args_os().collect();
    let Some(argv0) = argv.first() else { return };
    let code = if Path::new(argv0).file_name() == Some(OsStr::new("gh")) {
        run(&argv[1..])
    } else {
        match argv.get(1).and_then(|a| a.to_str()) {
            Some("gh") => run(&argv[2..]),
            Some("gh-shim") => shim_command(&argv[2..]),
            _ => return,
        }
    };
    std::process::exit(code);
}

/// `loom-daemon gh-shim path|session-env|status`.
///
/// - `path`: create the shim directory and print it.
/// - `session-env`: the SessionStart hook's half (#10516) — see [`session_env`].
/// - `status`: which `gh` this shell actually resolves — see [`session_env`].
fn shim_command(args: &[OsString]) -> i32 {
    match args.first().and_then(|a| a.to_str()) {
        Some("path") if args.len() == 1 => match ensure_shim_dir() {
            Ok(dir) => {
                println!("{}", dir.display());
                0
            }
            Err(e) => {
                eprintln!("loom-daemon gh-shim path: {e}");
                1
            }
        },
        Some("session-env") if args.len() == 1 => session_env::run(),
        Some("status") if args.len() == 1 => session_env::status(),
        _ => {
            eprintln!(
                "usage: loom-daemon gh-shim path|session-env|status\n  \
                 path         print a directory holding `gh` -> loom-daemon; put it first on PATH to \
                 route plain `gh` reads through the ETag cache (#10331)\n  \
                 session-env  (SessionStart hook) put that directory first on PATH via \
                 $CLAUDE_ENV_FILE (#10516)\n  \
                 status       print `front|launcher|bypassed: <gh>` for this shell's PATH"
            );
            2
        }
    }
}

fn depth() -> u32 {
    std::env::var(SENTINEL_ENV)
        .ok()
        .and_then(|d| d.parse().ok())
        .unwrap_or(0)
}

fn env_on(key: &str) -> bool {
    std::env::var(key).is_ok_and(|v| v == "1")
}

/// Any of the documented "force a real call" switches.
fn no_cache() -> bool {
    [
        "LOOM_GH_NO_CACHE",
        "GH_CACHE_DISABLE",
        "LOOM_ETAG_LIST_DISABLE",
    ]
    .iter()
    .any(|k| env_on(k))
}

/// A served call: what `gh` would have written and its exit status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Served {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}

impl Served {
    fn ok(stdout: String) -> Self {
        Self {
            stdout,
            stderr: String::new(),
            code: 0,
        }
    }
}

/// The front proper. Returns the exit code to exit with (a passthrough
/// replaces this process and never returns).
#[must_use]
pub fn run(raw: &[OsString]) -> i32 {
    let depth = depth();
    if depth >= MAX_DEPTH {
        eprintln!("gh (loom front): {SENTINEL_ENV}={depth}: refusing a gh resolution loop");
        return 127;
    }
    let Some(next) = next_gh::resolve() else {
        eprintln!(
            "gh: command not found (loom gh front #10331: no other gh on PATH or LOOM_GH_BIN)"
        );
        return 127;
    };
    if depth == 0 {
        // #10432: a role tick's forge writes (claims, verdict labels, merges),
        // journaled for its `pick.decision`. Parses argv only; a no-op outside
        // a role tick.
        crate::observability::pick_journal::record_gh_actions(raw);
        // #10607: every row this process writes — served (the facade's) or
        // passthrough (`ledger`) — carries the agent role and `served` /
        // `passthrough` (`ag` / `vi`).
        crate::forge_call_stats::agent::set_agent_role(std::env::var("LOOM_ROLE").ok().as_deref());
        if let Some(out) = serve(raw, &next) {
            record("revalidated", raw);
            let mut stdout = std::io::stdout().lock();
            // A closed pipe (`gh … | head -1`) is the reader's choice.
            let _ = stdout.write_all(out.stdout.as_bytes());
            let _ = stdout.flush();
            let _ = std::io::stderr().lock().write_all(out.stderr.as_bytes());
            return out.code;
        }
        record("bypass", raw);
        // W5: one ledger row per passthrough, written before the exec that
        // replaces this process. It can never fail or delay the call.
        ledger::book(raw, std::env::current_dir().ok().as_deref());
    }
    let err = crate::gh_invocation::transparent::exec(
        &next,
        raw,
        &[(SENTINEL_ENV, OsString::from((depth + 1).to_string()))],
    );
    eprintln!("gh (loom front): failed to exec {}: {err}", next.display());
    127
}

/// The ETag-served output, or `None` to pass through. Every failure of the
/// cache layer lands here as `None`: caching is never a correctness mechanism.
fn serve(raw: &[OsString], next: &Path) -> Option<Served> {
    if no_cache() || std::io::stdout().is_terminal() {
        return None; // A TTY gets gh's human/colour output, which we do not reproduce.
    }
    let args = raw
        .iter()
        .map(|a| a.to_str().map(str::to_string))
        .collect::<Option<Vec<String>>>()?;
    let route = classify::classify(&args);
    if route == Route::Passthrough
        || crate::forge_cmd::detect_forge(None) == crate::forge_cmd::ForgeType::Gitea
        || std::env::var("GH_HOST").is_ok_and(|h| !h.is_empty() && h != "github.com")
    {
        return None;
    }
    let cwd = std::env::current_dir().ok();
    match route {
        Route::EtagView(entity) => {
            let served = pin_repo(args[1..].to_vec(), cwd.as_deref())?;
            crate::forge_cached_view::build_output_via(STATS_CALLER, entity.as_str(), &served, next)
                .map(Served::ok)
        }
        Route::EtagChecks => {
            if gh_output_altered() {
                return None;
            }
            let served = pin_repo(args[2..].to_vec(), cwd.as_deref())?;
            pr_checks::serve(&served, next, cwd.as_deref())
        }
        Route::EtagList(entity, served) => {
            let jq = served
                .iter()
                .any(|a| a == "--jq" || a == "-q" || a.starts_with("--jq="));
            let served = pin_repo(served, cwd.as_deref())?;
            let listing = crate::forge_cached_list::build_served_via(
                STATS_CALLER,
                entity.as_str(),
                &served,
                next,
            )?;
            // #10432: a role tick's candidate listing, journaled for its
            // `pick.decision` (a no-op outside a role tick).
            crate::observability::pick_journal::record_listing(&listing);
            let out = listing.render()?;
            if jq {
                return Some(Served::ok(out));
            }
            // The listing module pretty-prints; `gh --json` off a TTY is compact.
            let v: serde_json::Value = serde_json::from_str(&out).ok()?;
            serde_json::to_string(&v)
                .ok()
                .map(|s| Served::ok(format!("{s}\n")))
        }
        Route::Passthrough => None,
    }
}

/// Environment that makes `gh` print something else off a TTY: a forced
/// TTY, forced colour (colourised `--json`), or debug output on stderr.
fn gh_output_altered() -> bool {
    let set = |k: &str| std::env::var(k).is_ok_and(|v| !v.is_empty());
    set("GH_FORCE_TTY")
        || set("GH_DEBUG")
        || set("DEBUG")
        || std::env::var("CLICOLOR_FORCE").is_ok_and(|v| !v.is_empty() && v != "0")
}

/// Name the repo `gh` itself would use, as an explicit `--repo`, so the ETag
/// modules never fall back to `LOOM_REPO` where `gh` would not. `None` (pass
/// through) whenever that repo is not unambiguous.
fn pin_repo(mut args: Vec<String>, cwd: Option<&Path>) -> Option<Vec<String>> {
    let has_repo = args
        .iter()
        .any(|a| a == "-R" || a == "--repo" || a.starts_with("--repo="));
    if has_repo {
        return Some(args);
    }
    let repo = match std::env::var("GH_REPO").ok().filter(|r| !r.is_empty()) {
        Some(r) => classify::is_slug(&r).then_some(r)?,
        None => implicit_repo(&git_remote_config(cwd?)?)?,
    };
    args.extend(["--repo".to_string(), repo]);
    Some(args)
}

/// `git config --get-regexp ^remote\.` in `cwd`.
fn git_remote_config(cwd: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["config", "--get-regexp", r"^remote\."])
        .current_dir(cwd)
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The `owner/name` `gh` resolves from a checkout's remotes, when that is
/// unambiguous: exactly one remote, named `origin`, on `github.com`, with no
/// `gh repo set-default` override (`remote.origin.gh-resolved`). With several
/// remotes `gh` prefers `upstream`/`github` and honours set-default — pass
/// those through rather than guess.
#[must_use]
pub fn implicit_repo(remote_config: &str) -> Option<String> {
    let mut url = None;
    for line in remote_config.lines() {
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        let rest = key.strip_prefix("remote.")?;
        let (name, field) = rest.rsplit_once('.')?;
        if name != "origin" {
            return None;
        }
        match field {
            "url" if url.replace(value.to_string()).is_some() => return None,
            "gh-resolved" if value != "base" => return None,
            _ => {}
        }
    }
    let (host, nwo) = crate::forge_etag_store::parse_remote_url(&url?)?;
    (host == "github.com" && classify::is_slug(&nwo)).then_some(nwo)
}

/// Append one `x-loom-cache` record to `$GH_CACHE_OUTCOME_LOG` (the opt-in
/// log `gh-cached` writes), naming only the command and verb — never the
/// argv, which can carry bodies.
fn record(outcome: &str, raw: &[OsString]) {
    let Some(path) = std::env::var_os("GH_CACHE_OUTCOME_LOG").filter(|p| !p.is_empty()) else {
        return;
    };
    let command: Vec<String> = raw
        .iter()
        .take(2)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let line = serde_json::json!({
        "x-loom-cache": outcome,
        "source": STATS_CALLER,
        "command": command.join(" "),
    });
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(PathBuf::from(path))
    {
        let _ = writeln!(f, "{line}");
    }
}

/// Create (or refresh) the shim directory — `gh` → this binary — and return
/// it. Keyed by this binary's path, so two `loom-daemon` builds never fight
/// over one link. `LOOM_GH_SHIM_BASE` overrides the base (tests).
///
/// # Errors
///
/// When the directory cannot be made private or the link cannot be written.
pub fn ensure_shim_dir() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let base = std::env::var("LOOM_GH_SHIM_BASE")
        .ok()
        .filter(|b| !b.is_empty())
        .map_or_else(crate::forge_etag_store::host_tmp_base, PathBuf::from);
    let dir = base.join(format!(
        "loom-gh-shim-{}",
        crate::short_hash::short_sha16(&exe.display().to_string())
    ));
    if !crate::forge_etag_store::private_dir(&dir, true) {
        return Err(std::io::Error::other(format!(
            "refusing untrusted shim dir {}",
            dir.display()
        )));
    }
    let link = dir.join("gh");
    if std::fs::read_link(&link).ok().as_deref() == Some(exe.as_path()) {
        return Ok(dir);
    }
    let tmp = dir.join(format!(".gh-{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    #[cfg(unix)]
    std::os::unix::fs::symlink(&exe, &tmp)?;
    #[cfg(not(unix))]
    std::fs::copy(&exe, &tmp)?;
    std::fs::rename(&tmp, &link)?;
    Ok(dir)
}

/// The front's shim dir, or `None`: opted out (`LOOM_GH_SHIM=0`), not
/// running as `loom-daemon` (a test harness), or the dir could not be made.
#[must_use]
pub fn front_dir() -> Option<PathBuf> {
    if std::env::var(OPT_OUT_ENV).is_ok_and(|v| v == "0") {
        return None;
    }
    let exe = std::env::current_exe().ok()?;
    if exe.file_name() != Some(OsStr::new("loom-daemon")) {
        return None;
    }
    ensure_shim_dir().ok()
}

/// The `PATH` a dispatched worker gets: the shim dir first, then `current`.
/// `None` leaves `PATH` alone (see [`front_dir`]).
#[must_use]
pub fn worker_path(current: Option<&OsStr>) -> Option<OsString> {
    prepend_path(&front_dir()?, current)
}

/// The one `gh` ordering shared by a dispatched worker and an interactive
/// session (#10516): the managed launcher (#9987) first when a policy resolves
/// one, then the front, then `current` — so with no policy the front's own
/// reads still reach whatever `gh` came first before (e.g. the 2am telemetry
/// shim) as their `next_gh`. `None` leaves `PATH` alone.
#[must_use]
pub fn session_path(
    current: Option<&OsStr>,
    egress: Option<&crate::forge_egress::worker_env::WorkerEgress>,
) -> Option<OsString> {
    compose_path(
        front_dir().as_deref(),
        egress.and_then(crate::forge_egress::worker_env::WorkerEgress::launcher_dir),
        current,
    )
}

/// [`session_path`] with its inputs injected: `front` prepended to `current`,
/// then `launcher` on top. `None` when neither applies.
#[must_use]
pub fn compose_path(
    front: Option<&Path>,
    launcher: Option<&Path>,
    current: Option<&OsStr>,
) -> Option<OsString> {
    let mut path: Option<OsString> = None;
    for dir in [front, launcher].into_iter().flatten() {
        if let Some(next) = prepend_path(dir, path.as_deref().or(current)) {
            path = Some(next);
        }
    }
    path
}

/// `dir` first, then `current` minus any earlier copy of `dir`.
#[must_use]
pub fn prepend_path(dir: &Path, current: Option<&OsStr>) -> Option<OsString> {
    let rest = current.map(std::env::split_paths).into_iter().flatten();
    let dirs = std::iter::once(dir.to_path_buf()).chain(rest.filter(|p| p != dir));
    std::env::join_paths(dirs).ok()
}
