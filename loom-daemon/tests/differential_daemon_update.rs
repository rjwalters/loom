//! Differential test: the ported `daemon-update` against a frozen oracle of
//! the **pre-port shell's** answers (epic #7810, #8088).
//!
//! # Why a retained suite is not enough
//!
//! Three suites are retained and run unchanged against this port
//! (`test-loom-daemon-update.sh` and its `-resolve-json` / `-fetch`
//! siblings). That is a good proof and #8011 showed it is not a sufficient
//! one: the `dep_recheck` port shipped three silent divergences while its
//! retained suite was 104/104 green, because a retained suite proves only what
//! its author thought to write down — and nobody writes down the input they
//! did not imagine.
//!
//! This port's own grammar is exactly where that ceiling bites. The script's
//! argument loop reads `LOOM_DAEMON_UPDATE_FETCH` for a falsy set *first* and
//! a truthy set *second*, so a value in neither set leaves `auto` in place;
//! no retained assertion passes a value in neither set (`maybe`, `2`, `TRUE`
//! and a space-padded `1` are all in this corpus). `--timeout` is validated by
//! the script's own `^[0-9]+$` regex, so `007` must be ACCEPTED and `0x1e`
//! refused; no retained assertion passes either. Those are the inputs this
//! corpus generates from the grammar rather than from memory.
//!
//! # What this corpus deliberately does NOT reach, and what covers it instead
//!
//! Naming the holes is part of the proof — an unstated hole reads as coverage.
//!
//! | Not reached | Why | Covered by |
//! |---|---|---|
//! | the launchd / systemd restart renderers (and therefore `--timeout`'s carry-through into `restart --drain --timeout <SECS>`) | reaching them needs a live `launchctl` (Darwin-only) or `systemctl --user` (Linux-only), which is exactly the host dependence the pinning table below exists to remove | `test-loom-daemon-update.sh` scenarios 26-34, which stub `systemctl` on the host under test |
//! | artifact-fetch mode's SUCCESS path (only its fallback reasons are reached) | it needs a fake `gh` **and** `jq` on `PATH`, and `jq` is not at a fixed path on both a macOS host and a Linux runner | `test-loom-daemon-update-fetch.sh` (#8028) and `-resolve-json.sh` (#7977), both retained and both driving this port |
//! | every mutating path — rebuild, provision, restart, exit codes 4/5/6/7/8 | an inspection-mode corpus is side-effect-free by construction, which is what makes it freezable | `test-loom-daemon-update.sh`, which drives real flows against fake binaries |
//!
//! # What is compared
//!
//! Each case is a complete, hermetic invocation of the two **pure-inspection
//! modes** — `--check` and `--dry-run` — plus the argument loop's own
//! terminal paths (`--help`, an unknown flag, a bad `--timeout`, the
//! `--drain`/`--restart-now` conflict). Those are the widest surface the
//! script has that is documented to write nothing: between them one
//! invocation runs the flag loop, the whole environment-precedence block, the
//! repo-root walk, the source-checkout gate, the binary resolution and
//! version/commit extraction, the staleness comparison, the artifact-mode
//! decision and its fallback reason, the `.daemon.flags` replay resolution,
//! the drain/restart planning, the stale-entry-point scan and both the
//! `--check` and `--dry-run` renderers.
//!
//! stdout, stderr and the exit code are all captured and compared. The
//! mutating paths (rebuild, fetch, provision, restart — exit codes 4/5/6/7/8)
//! are deliberately *not* reachable from an inspection mode; they are covered
//! by the retained suites, which drive real builds against fake binaries.
//!
//! # Host-portability is a property of the CORPUS, not an accident
//!
//! The frozen answers must replay identically on a macOS developer host and a
//! Linux CI runner, so every host-dependent input the script reads is pinned
//! by the fixture, not left to the host:
//!
//! | Input | Pinned by |
//! |---|---|
//! | `uname -s -m` → release target triple | `LOOM_DAEMON_UPDATE_TARGET` |
//! | launchd ownership (Darwin) | `LOOM_DAEMON_LAUNCHD=0` |
//! | `systemd --user` ownership (Linux) | `LOOM_DAEMON_SYSTEMD=0` |
//! | `$PATH` (and therefore the stale-entry-point scan, and which optional tools resolve) | the fixture's own `bin/` + a mirror of the real system PATH with `gh`/`jq` excluded — never the host's real `/usr/bin`, `/bin`, … directly (see [`safe_system_path`]) |
//! | `$HOME`, `$TMPDIR`, the provisioning destination | fixture dirs |
//! | the source checkout's `HEAD` | an empty commit with pinned author/committer/date, so the sha is a constant |
//!
//! [`the_oracle_is_host_portable`] asserts this rather than trusting it: it
//! fails if any frozen answer carries a host-specific token (a real `/Users`
//! or `/home` path, `Darwin`, a machine-specific triple).
//!
//! # Why an oracle file rather than running the shell here
//!
//! The pre-port shell is gone from the tree (that is the point of the port).
//! Recovering it needs `git show` against a pinned rev, which breaks under
//! CI's default shallow checkout. So its answers are frozen once, by the
//! command in the fixture's `_meta` record, and this test needs no `bash` at
//! all.
//!
//! Regenerate (only when the oracle's provenance legitimately changes):
//!
//! ```sh
//! mkdir -p defaults/scripts/.oracle-cli
//! git show <pre-port-rev>:defaults/scripts/cli/loom-daemon-update.sh \
//!   > defaults/scripts/.oracle-cli/loom-daemon-update.sh
//! LOOM_UPDATE_ORACLE_SHELL=$PWD/defaults/scripts/.oracle-cli/loom-daemon-update.sh \
//! LOOM_UPDATE_ORACLE_BASH=/opt/homebrew/bin/bash \
//!   cargo test -p loom-daemon --test differential_daemon_update -- --ignored --nocapture
//! rm -r defaults/scripts/.oracle-cli
//! ```
//!
//! Three constraints on that path, each of which a shortcut gets wrong:
//!
//! * it must be a SIBLING of the real `defaults/scripts/lib/`, because the
//!   script sources `../lib/*.sh` relative to itself — hence the extra
//!   directory rather than a flat dotfile;
//! * its BASENAME must be `loom-daemon-update.sh`, because two remediation
//!   messages print `$(basename "$0")` while the port prints the basename of
//!   `$LOOM_UPDATE_ARGV0`. A `.oracle-update.sh` oracle froze `Or run:
//!   .oracle-update.sh --prune-stale-entry-points`, which the port then
//!   "diverged" from — an artefact of the harness, not of either
//!   implementation, and exactly the trap §6 names when a harness
//!   re-implements part of what it measures;
//! * it must NOT be `defaults/scripts/cli/loom-daemon-update.sh` itself, which
//!   is the stub under test.
//!
//! `LOOM_UPDATE_ORACLE_BASH` must name a bash ≥ 5.2 — the version is recorded
//! in `_meta` because a bash-3.2 oracle would answer differently
//! (`${var//pat/&}` changed meaning in 5.2) and CI's bash is 5.x.
//!
//! # This green number was made red on purpose
//!
//! A suite that cannot be made to fail is not evidence
//! (`verification-recipes.md` §6, Cause 1). Four mutations were applied to the
//! port and the corpus replayed against each, from a 0-unexplained baseline:
//!
//! | mutation | unexplained cases |
//! |---|---|
//! | the staleness comparison inverted (`installed_commit == source_commit`) | 258 / 338 |
//! | `lib/default-branch.sh`'s advisory printed instead of discarded | 266 / 338 |
//! | `read_flags_file` reverted to `text.lines()` (the EOF tolerance) | 1 / 338 |
//! | `raw_tail` reverted to clap's parsed vector (the `--` escape) | 1 / 338 |
//!
//! The last three are not hypothetical — they are divergences this test
//! FOUND, on its first honest run against the port, with all three retained
//! suites green:
//!
//! * the port reproduced `loom_default_branch`'s three-line "could not
//!   determine the default branch" advisory, where the script's single call
//!   site discarded it with `2>/dev/null`. Three stderr lines on every
//!   `--check`/`--dry-run` in a checkout with no resolvable origin;
//! * `text.lines()` yields a final line with no trailing newline; `while IFS=
//!   read -r line` does not, so the shell silently DROPPED the last flag of an
//!   unterminated `.daemon.flags`. Preserved bug-for-bug — see
//!   `daemon_update::read_flags_file` for why, and for the direction of its
//!   risk;
//! * clap consumed a bare `--` as its own end-of-options escape, so
//!   `loom-daemon-update.sh --` reached the argument loop as NO arguments and
//!   ran a full update where the shell refused with exit 1. A usage error
//!   turned into an action.
//!
//! Two of those three are single-case findings. That is the point of
//! generating the corpus from the grammar: nobody writes an assertion for a
//! bare `--`, and the cost of missing it is not proportional to how rare the
//! input is.
//!
//! # What a failure means
//!
//! `UNEXPLAINED` means the port differs from the retired shell in a way nobody
//! has classified. Read the case, decide whether the new behaviour is right,
//! then either fix the port or add the class **with** its mechanism. Do not
//! widen a class to silence it — a class recognised by a property of the input
//! rather than by its mechanism is a hole shaped like a class
//! (`verification-recipes.md` §6).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Frozen answers from the pre-port shell. See the `_meta` record.
const ORACLE: &str = include_str!("fixtures/daemon_update_shell_oracle.jsonl");

/// The `$0` every case is run with, on both sides.
///
/// The shell printed `$(basename "$0")` in two remediation messages; the port
/// prints the basename of `$LOOM_UPDATE_ARGV0`, which the stub exports for
/// exactly this reason. Pinning it to a sentinel keeps the two comparable
/// without post-hoc rewriting of the output.
const ARGV0: &str = "loom-daemon-update.sh";

/// Provenance text for the `_meta` record, documenting the nominal system
/// PATH a real host's `daemon-update` runs under, and the set
/// [`safe_system_path`] mirrors (minus [`HIDDEN_TOOLS`]). **Not used
/// directly to build the case's actual runtime PATH** — trusting these
/// directories literally is what let a real `gh` leak into the differential
/// replay on hosts (this one included) that install the `gh` CLI into
/// `/usr/bin`.
const FIXED_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// The release target triple every case resolves, pinned so `uname -s -m`
/// never reaches the answer. A real triple rather than a sentinel, because the
/// script's own mapping table is what a sentinel would bypass.
const FIXED_TARGET: &str = "aarch64-unknown-linux-gnu";

/// The commit a "stale" fixture's installed binary reports. Seven hex
/// characters, the shape `extract_commit`'s `grep -oE 'commit [0-9a-f]+'`
/// matches, and deliberately not a prefix of any real sha.
const STALE_COMMIT: &str = "dead1ee";

/// The version a fixture's installed binary reports unless a case overrides
/// it.
const INSTALLED_VERSION: &str = "0.15.0";

// ---------------------------------------------------------------------------
// The corpus
// ---------------------------------------------------------------------------

/// What the fixture's installed `loom-daemon` answers `--version` with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Installed {
    /// A commit equal to the fixture checkout's `HEAD` — i.e. up to date.
    Current,
    /// A commit that is not `HEAD` — i.e. a rebuild would change the binary.
    Stale,
    /// `--version` output with no `commit <hex>` clause at all, the shape an
    /// ancient binary (or a wrapper) produces. `extract_commit` yields "" and
    /// the staleness comparison has one side missing.
    NoCommit,
    /// No binary resolves at all.
    Absent,
}

/// The shape of the checkout the case runs in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Repo {
    /// `.git` + `.loom/` + `loom-daemon/Cargo.toml` — a Loom source checkout.
    Source,
    /// `.git` + `.loom/` but no `loom-daemon/Cargo.toml`: a Loom checkout that
    /// is not a source checkout. The script refuses (exit 1).
    NotSource,
}

// There is deliberately NO "not a Loom checkout at all" shape here, and the
// reason is a finding rather than an omission. When the repo-root walk finds
// nothing, #5140's fallback resolves the checkout the ENTRY POINT itself lives
// in — for the oracle that is the generating host's own loom clone, and its
// answer carried that clone's absolute path, its HEAD and its
// commits-behind-origin count. Those are properties of the generating host,
// not of either implementation, so no frozen answer for that shape could ever
// replay anywhere else. It is covered by `test-loom-daemon-update.sh`, which
// runs on the host under test rather than against frozen text.

/// One invocation, fully specified. Everything a case needs is in the fixture,
/// so both sides read the same bytes and the harness cannot lie about which
/// side moved (`verification-recipes.md` §6: generate the corpus ONCE).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Case {
    id: String,
    argv: Vec<String>,
    /// Extra environment on top of the fixed base. A value of `null` in the
    /// fixture means "explicitly absent", which is how the corpus
    /// distinguishes an unset `LOOM_DAEMON_UPDATE_FETCH` from an empty one.
    env: BTreeMap<String, Option<String>>,
    installed: Installed,
    repo: Repo,
    /// Contents of `.loom/.daemon.flags`, or `None` for "no such file".
    flags_file: Option<String>,
    /// Whether the fixture presents a LIVE daemon.
    ///
    /// The pid written is the harness process's own — alive for the whole
    /// run, owned by us (so `kill -0` succeeds rather than returning EPERM the
    /// way pid 1 would), and never printed by either implementation. That last
    /// property is what lets the answers be frozen: the pid differs between
    /// generation and replay, the *answer* does not.
    ///
    /// Without this dimension the `.daemon.flags` replay is unreachable —
    /// `RESTART_ARGS` is only rendered on the was-running branch — so group D
    /// would be sixteen identical answers proving nothing.
    running: bool,
    /// Names of `loom-*` executables planted in the fixture's own `bin/`,
    /// which the stale-entry-point scan walks when it is not skipped.
    stale_entry_points: Vec<String>,
    /// Run from OUTSIDE any checkout, so the #5140 self-location fallback is
    /// what resolves the source tree — and reach the checkout through a
    /// SYMLINK, so the answer distinguishes a logical path from its target.
    ///
    /// Added by #8088 after the port shipped a divergence here that this
    /// corpus was structurally unable to see: `LOOM_UPDATE_CLI_DIR` was not an
    /// input the corpus generated *at all*, which is §6's "generate every
    /// input FIELD, not just the interesting one". The field defaults to
    /// `false` when a frozen record omits it, so the 338 records generated
    /// before it existed stay byte-identical — an additive extension, not a
    /// regeneration.
    ///
    /// The two sides are made comparable by giving them the same directory:
    /// the shell derived it from its own `$SCRIPT_DIR`, so [`run_case`] plants
    /// a copy of the oracle shell inside the fixture and invokes it through
    /// the symlink; the port reads `LOOM_UPDATE_CLI_DIR`, which the stub
    /// exports from exactly that `$SCRIPT_DIR`. Neither side is told anything
    /// the other is not.
    self_located: bool,
}

/// What one side answered.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Answer {
    rc: i32,
    stdout: String,
    stderr: String,
}

fn json_str(v: &serde_json::Value, k: &str) -> Option<String> {
    v.get(k)?.as_str().map(std::string::ToString::to_string)
}

fn parse_installed(s: &str) -> Installed {
    match s {
        "current" => Installed::Current,
        "stale" => Installed::Stale,
        "nocommit" => Installed::NoCommit,
        "absent" => Installed::Absent,
        other => panic!("unknown installed shape {other:?}"),
    }
}

fn installed_name(i: Installed) -> &'static str {
    match i {
        Installed::Current => "current",
        Installed::Stale => "stale",
        Installed::NoCommit => "nocommit",
        Installed::Absent => "absent",
    }
}

fn parse_repo(s: &str) -> Repo {
    match s {
        "source" => Repo::Source,
        "notsource" => Repo::NotSource,
        other => panic!("unknown repo shape {other:?}"),
    }
}

fn repo_name(r: Repo) -> &'static str {
    match r {
        Repo::Source => "source",
        Repo::NotSource => "notsource",
    }
}

fn load_oracle() -> (serde_json::Value, Vec<(Case, Answer)>) {
    let mut meta = serde_json::Value::Null;
    let mut out = Vec::new();
    for (i, line) in ORACLE.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let v: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("oracle line {}: {e}", i + 1));
        if let Some(m) = v.get("_meta") {
            meta = m.clone();
            continue;
        }
        let mut env = BTreeMap::new();
        if let Some(obj) = v["env"].as_object() {
            for (k, val) in obj {
                env.insert(
                    k.clone(),
                    if val.is_null() {
                        None
                    } else {
                        Some(val.as_str().unwrap_or_default().to_string())
                    },
                );
            }
        }
        let case = Case {
            id: json_str(&v, "id").expect("case id"),
            argv: v["argv"]
                .as_array()
                .expect("argv")
                .iter()
                .map(|a| a.as_str().unwrap_or_default().to_string())
                .collect(),
            env,
            installed: parse_installed(&json_str(&v, "installed").expect("installed")),
            repo: parse_repo(&json_str(&v, "repo").expect("repo")),
            flags_file: json_str(&v, "flags_file"),
            running: v["running"].as_bool().unwrap_or(false),
            stale_entry_points: v["stale_entry_points"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|s| s.as_str().unwrap_or_default().to_string())
                        .collect()
                })
                .unwrap_or_default(),
            // Absent in every record frozen before #8088 added the dimension;
            // `false` is what those runs actually did.
            self_located: v["self_located"].as_bool().unwrap_or(false),
        };
        let answer = Answer {
            rc: i32::try_from(v["rc"].as_i64().expect("rc")).expect("rc fits i32"),
            stdout: json_str(&v, "stdout").unwrap_or_default(),
            stderr: json_str(&v, "stderr").unwrap_or_default(),
        };
        out.push((case, answer));
    }
    assert!(!meta.is_null(), "oracle carries no _meta provenance record");
    (meta, out)
}

// ---------------------------------------------------------------------------
// Running a case
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}

/// Tools that must stay UNRESOLVABLE from a case's PATH for the corpus to be
/// deterministic across hosts. Both are read-only inspection queries
/// (`have("gh")`, `have("jq")`) whose only effect is which TEXT a fallback
/// reason renders as — never actually invoked — so hiding the real ones
/// costs nothing. See the module doc's "What this corpus deliberately does
/// NOT reach" table: reaching artifact-fetch's SUCCESS path needs a fake `gh`
/// **and** `jq`, which is exactly why the corpus is built to keep both out of
/// reach instead.
const HIDDEN_TOOLS: &[&str] = &["gh", "jq"];

/// A read-only mirror of the real system PATH (`/usr/bin`, `/bin`, …),
/// EXCLUDING [`HIDDEN_TOOLS`] — built ONCE and shared by every case, since
/// it depends only on the host, never on a case's own state.
///
/// Trusting the raw system directories directly (the previous design) does
/// not hold: this host, and apparently some CI runners too, install the `gh`
/// CLI straight into `/usr/bin`, so a case's PATH built from those
/// directories can silently gain a working `gh` depending on what the host
/// happens to have — exactly the divergence class #8088's differential run
/// caught (frozen answers assumed `gh` absent; a host with `/usr/bin/gh`
/// disagreed). But the fixture's shell side is a REAL bash script that calls
/// ordinary coreutils (`dirname`, `basename`, `cat`, `sed`, …) throughout, so
/// unlike [`HIDDEN_TOOLS`] those cannot simply be dropped from PATH either —
/// a mirror that carries everything else through by symlink, built once, is
/// what keeps both properties: the standard toolchain works, and the two
/// deliberately-hidden tools never resolve no matter what the host has
/// installed at those paths.
fn safe_system_path() -> &'static Path {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::temp_dir()
            .join(format!("loom-differential-daemon-update-safe-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("safe system-path dir");
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for sysdir in FIXED_PATH.split(':') {
            let Ok(entries) = std::fs::read_dir(sysdir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if HIDDEN_TOOLS.contains(&name.as_str()) || !seen.insert(name.clone()) {
                    continue;
                }
                if !entry.path().is_file() {
                    continue;
                }
                #[cfg(unix)]
                let _ = std::os::unix::fs::symlink(entry.path(), dir.join(&name));
            }
        }
        dir
    })
}

/// The fixture checkout's `HEAD`, built to be a CONSTANT.
///
/// An empty commit's tree is the well-known empty-tree object, so pinning the
/// author, the committer and both dates makes the commit object — and
/// therefore its sha — byte-identical on every host and every run. Without
/// this the source-HEAD commit would be a fresh random sha per case and no
/// answer containing it could be frozen.
fn init_pinned_repo(repo: &Path) -> String {
    let run = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(repo)
            // A host's global/system git config must not reach the fixture:
            // an `init.defaultBranch`, a `commit.gpgsign` or a `user.name`
            // would all move the sha this function exists to pin.
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "loom differential")
            .env("GIT_AUTHOR_EMAIL", "diff@example.invalid")
            .env("GIT_COMMITTER_NAME", "loom differential")
            .env("GIT_COMMITTER_EMAIL", "diff@example.invalid")
            .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00+0000")
            .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00+0000")
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    run(&["init", "-q", "-b", "main"]);
    run(&["commit", "-q", "--allow-empty", "-m", "fixture"]);
    run(&["rev-parse", "HEAD"])
}

/// A fake `loom-daemon` that answers `--version` and refuses every subcommand.
///
/// Refusing subcommands rather than falling through to a daemon body is the
/// #4799 lesson the retained suite's fixture learned the hard way: a fixture
/// that loops in the foreground on an unrecognised subcommand wedges its
/// caller instead of answering it.
fn write_fake_daemon(path: &Path, version_line: &str) {
    std::fs::write(
        path,
        format!(
            "#!/bin/sh\n\
             if [ \"$1\" = \"--version\" ]; then echo '{version_line}'; exit 0; fi\n\
             echo \"fake loom-daemon: unsupported subcommand: $*\" >&2\n\
             exit 1\n"
        ),
    )
    .expect("fake daemon");
    make_executable(path);
}

/// Build the fixture for one case under `root` and return the environment, the
/// working directory, and the pinned HEAD sha both sides run with.
///
/// Identical for the shell (at generation) and the port (at replay) — it is
/// one function precisely so a later edit cannot move one side without the
/// other.
fn materialise(case: &Case, root: &Path) -> (BTreeMap<String, String>, PathBuf, String) {
    let home = root.join("home");
    let tmp = root.join("tmp");
    let bin = root.join("bin");
    let dest = root.join("dest");
    let repo = root.join("repo");
    for d in [&home, &tmp, &bin, &dest, &repo] {
        std::fs::create_dir_all(d).expect("fixture dir");
    }

    let head = init_pinned_repo(&repo);
    let short = &head[..7];

    {
        std::fs::create_dir_all(repo.join(".loom/scripts/cli")).expect("fixture .loom");
        // The script resolves (and refuses without) a start and a stop script
        // under the repo root. An inspection mode never execs either, so a
        // marker suffices — and a marker rather than a copy of the real thing
        // keeps the fixture from depending on those scripts' contents.
        for name in ["loom-daemon-start.sh", "loom-daemon-stop.sh"] {
            let p = repo.join(".loom/scripts/cli").join(name);
            std::fs::write(&p, "#!/bin/sh\nexit 0\n").expect("lifecycle stub");
            make_executable(&p);
        }
    }
    if case.repo == Repo::Source {
        std::fs::create_dir_all(repo.join("loom-daemon")).expect("crate dir");
        std::fs::write(
            repo.join("loom-daemon/Cargo.toml"),
            "[package]\nname = \"loom-daemon\"\nversion = \"0.0.0\"\n",
        )
        .expect("Cargo.toml");
    }
    if let Some(text) = &case.flags_file {
        std::fs::create_dir_all(repo.join(".loom")).expect(".loom");
        std::fs::write(repo.join(".loom/.daemon.flags"), text).expect("flags file");
    }

    let fake = root.join("fake-loom-daemon");
    match case.installed {
        Installed::Current => write_fake_daemon(
            &fake,
            &format!(
                "loom-daemon {INSTALLED_VERSION} (commit {short}, built 2026-01-01T00:00:00Z)"
            ),
        ),
        Installed::Stale => write_fake_daemon(
            &fake,
            &format!(
                "loom-daemon {INSTALLED_VERSION} (commit {STALE_COMMIT}, built 2026-01-01T00:00:00Z)"
            ),
        ),
        Installed::NoCommit => {
            write_fake_daemon(&fake, &format!("loom-daemon {INSTALLED_VERSION}"));
        }
        Installed::Absent => {}
    }

    for name in &case.stale_entry_points {
        let p = bin.join(name);
        std::fs::write(&p, "#!/bin/sh\nexit 0\n").expect("entry point");
        make_executable(&p);
    }

    let mut env: BTreeMap<String, String> = BTreeMap::new();
    env.insert("HOME".into(), home.display().to_string());
    env.insert("PATH".into(), format!("{}:{}", bin.display(), safe_system_path().display()));
    env.insert("TMPDIR".into(), tmp.display().to_string());
    env.insert("LOOM_UPDATE_ARGV0".into(), ARGV0.into());
    // Both supervisors OFF: this is what makes the answers the same on a macOS
    // host and a Linux runner (see the module doc's pinning table).
    env.insert("LOOM_DAEMON_LAUNCHD".into(), "0".into());
    env.insert("LOOM_DAEMON_SYSTEMD".into(), "0".into());
    env.insert("LOOM_DAEMON_UPDATE_TARGET".into(), FIXED_TARGET.into());
    env.insert("LOOM_DAEMON_BIN_DIR".into(), dest.display().to_string());
    let pid_file = root.join("running.pid");
    if case.running {
        std::fs::write(&pid_file, format!("{}\n", std::process::id())).expect("pid file");
    }
    env.insert("LOOM_PID_FILE".into(), pid_file.display().to_string());
    // Default OFF; group G turns it back on for the cases that exercise it, so
    // ~700 bytes of identical advisory is not frozen into every answer.
    env.insert("LOOM_SKIP_STALE_ENTRY_POINT_CHECK".into(), "1".into());
    if case.installed != Installed::Absent {
        env.insert("LOOM_DAEMON_BIN".into(), fake.display().to_string());
    }
    // Git identity, so a case that shells out to git inside the fixture gets
    // the same answers the pinned init did.
    env.insert("GIT_CONFIG_GLOBAL".into(), "/dev/null".into());
    env.insert("GIT_CONFIG_SYSTEM".into(), "/dev/null".into());

    // ---- #5140 self-location fallback, reached through a symlink ----
    //
    // `cwd` becomes a directory that is NOT inside any checkout, so neither
    // implementation can resolve the source tree from `$PWD` and both fall
    // back to "the checkout this entry point lives in". That entry point is
    // addressed through `link`, a second name for `repo` — the same shape
    // `/var` has for `/private/var` on every macOS host, made explicit so the
    // case behaves identically on a Linux runner.
    let mut cwd = repo.clone();
    if case.self_located {
        let outside = root.join("outside");
        std::fs::create_dir_all(&outside).expect("outside dir");
        // A bare `.loom/` with no `.git` beside it: machine state, not a
        // checkout. This is #5140's own reported shape, and it makes the case
        // assert the `.git`-alongside-`.loom` pairing at the same time.
        std::fs::create_dir_all(outside.join(".loom")).expect("outside .loom");
        cwd = outside;

        let link = root.join("link");
        let _ = std::fs::remove_file(&link);
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo, &link).expect("checkout symlink");
        env.insert(
            "LOOM_UPDATE_CLI_DIR".into(),
            link.join(".loom/scripts/cli").display().to_string(),
        );
    }

    for (k, v) in &case.env {
        match v {
            Some(v) => {
                env.insert(k.clone(), v.clone());
            }
            None => {
                env.remove(k);
            }
        }
    }

    (env, cwd, head)
}

/// Replace every fixture-local absolute path and every run-varying token with
/// a stable placeholder.
///
/// Without this the answers would encode one machine's temp directory and
/// could never be frozen.
fn normalise(text: &str, root: &Path, head: &str) -> String {
    let root_s = root.display().to_string();
    let mut out = text.replace(&root_s, "{ROOT}");
    // The `/private` form gets its OWN placeholder, never `{ROOT}` (#8088).
    //
    // This used to collapse both spellings to `{ROOT}`, on the reasoning that
    // macOS resolves `/var` to `/private/var` so a fixture path "can come back
    // either way". That is true, and it is precisely the difference that
    // matters: a port which canonicalises where the shell used bash's logical
    // `cd … && pwd` answers with the symlink TARGET, and the old normaliser
    // rewrote that divergence into agreement before the comparison ever saw
    // it. `daemon_update::paths::find_repo_root` shipped exactly that bug and
    // this harness was structurally unable to report it — §6's "normalised
    // away a real divergence" (Cause 1), the same shape as `merge-pr`'s
    // `parse::<u64>().ok()`.
    //
    // A distinct marker keeps the answers freezable (neither side's temp path
    // is baked in) while leaving the two spellings distinguishable, so the
    // side that moved is the side that shows up.
    if let Some(stripped) = root_s.strip_prefix("/private") {
        out = out.replace(stripped, "{ROOT_LOGICAL}");
    } else {
        out = out.replace(&format!("/private{root_s}"), "{ROOT_PHYSICAL}");
    }
    // The pinned HEAD is a constant for a given git version, but git's
    // *abbreviation length* is not specified (`core.abbrev`, and git scales it
    // with object count), so mask the sha rather than freezing seven
    // particular characters. Longest first.
    //
    // The floor is SEVEN, not git's theoretical minimum of four: a four-
    // character replacement is short enough to collide with unrelated hex
    // elsewhere in the output and would rewrite text that has nothing to do
    // with the checkout's HEAD — a normaliser that hides real differences is
    // worse than one that misses a case.
    out = out.replace(head, "{HEAD}");
    for len in (7..=12).rev() {
        if head.len() >= len {
            out = out.replace(&head[..len], "{HEAD}");
        }
    }
    // `loom_locate_daemon_bin`'s resolution trace reports the resolved
    // binary's mtime — the one thing in the output that is a property of WHEN
    // the case ran rather than of what the implementation did.
    static MTIME: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = MTIME.get_or_init(|| {
        regex::Regex::new(r"mtime: \d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}")
            .expect("static mtime pattern")
    });
    re.replace_all(&out, "mtime: {MTIME}").into_owned()
}

/// Where a case's fixture tree is built.
///
/// Deliberately under the runner's cache dir rather than `$TMPDIR`: several of
/// this script's advisories key on a scratch-shaped path, and a fixture rooted
/// in the system temp dir would trip them in EVERY case — which costs twice
/// (the corpus can no longer tell "the advisory fired correctly" from "the
/// advisory always fires", and the identical warning is frozen into every
/// answer).
fn fixture_base() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    assert!(
        !home.is_empty() && !home.starts_with("/tmp") && !home.starts_with("/var/tmp"),
        "this suite needs a non-scratch $HOME to build fixtures under; got {home:?}. \
         A scratch-shaped base would make the scratch-path advisories fire in every \
         case and the frozen answers would no longer match."
    );
    let base = PathBuf::from(home).join(".cache/loom-differential-daemon-update");
    std::fs::create_dir_all(&base).expect("fixture base");
    base
}

/// Run one case against an arbitrary program, in a fresh fixture.
fn run_case(case: &Case, prog: &Path, leading_args: &[String]) -> Answer {
    let root = tempfile::Builder::new()
        .prefix("case-")
        .tempdir_in(fixture_base())
        .expect("tempdir");
    let (env, cwd, head) = materialise(case, root.path());

    // The shell side of a `self_located` case must be invoked from INSIDE the
    // fixture checkout, because the retired script derived its fallback from
    // its own `$SCRIPT_DIR` — a property of where the file sits, which no
    // environment variable can tell it. Planting a copy at the path the port
    // is handed in `LOOM_UPDATE_CLI_DIR`, and invoking it through the same
    // symlink, is what makes the two answers about the same directory.
    //
    // Replay does none of this: the port's `leading_args` is `["daemon-update"]`
    // — non-empty, but not a FILE path — so the check below (rather than a
    // plain non-empty check, which a self-located port replay would also
    // satisfy and then fail trying to `fs::copy` a subcommand name) correctly
    // skips this whole block and no `bash` is needed (the property the frozen
    // oracle exists to preserve).
    let mut leading: Vec<String> = leading_args.to_vec();
    let leading_is_oracle_shell = leading.first().is_some_and(|s| Path::new(s).is_file());
    if case.self_located && leading_is_oracle_shell {
        let oracle = PathBuf::from(&leading[0]);
        let cli = root.path().join("repo/.loom/scripts/cli");
        let lib = root.path().join("repo/.loom/scripts/lib");
        std::fs::create_dir_all(&lib).expect("fixture lib");
        let planted = cli.join("loom-daemon-update.sh");
        std::fs::copy(&oracle, &planted).expect("plant oracle shell");
        make_executable(&planted);
        // Whatever the retired script sourced from `../lib/`, copied wholesale
        // rather than by name so this cannot drift out from under the case.
        let oracle_lib = oracle
            .parent()
            .and_then(Path::parent)
            .map(|p| p.join("lib"))
            .expect("oracle shell has a ../lib sibling");
        for entry in std::fs::read_dir(&oracle_lib).expect("read oracle lib") {
            let entry = entry.expect("lib entry");
            if entry.path().extension().is_some_and(|e| e == "sh") {
                std::fs::copy(entry.path(), lib.join(entry.file_name())).expect("copy lib");
            }
        }
        leading[0] = root
            .path()
            .join("link/.loom/scripts/cli/loom-daemon-update.sh")
            .display()
            .to_string();
    }

    let mut cmd = Command::new(prog);
    cmd.args(&leading);
    cmd.args(&case.argv);
    cmd.env_clear();
    for (k, v) in &env {
        cmd.env(k, v);
    }
    cmd.current_dir(&cwd);
    let out = cmd.output().expect("spawn");
    Answer {
        rc: out.status.code().unwrap_or(-1),
        stdout: normalise(&String::from_utf8_lossy(&out.stdout), root.path(), &head),
        stderr: normalise(&String::from_utf8_lossy(&out.stderr), root.path(), &head),
    }
}

/// The binary under test.
///
/// `CARGO_BIN_EXE_loom-daemon` rather than a path derived from
/// `current_exe()`: Cargo guarantees it names the binary built from *this*
/// compilation, so a shared `$CARGO_TARGET_DIR` (the arrangement on this
/// fleet) cannot answer with another checkout's build — the #8176 ambiguity
/// that made two unrelated verifications report regressions that did not
/// exist.
fn port_bin() -> PathBuf {
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_loom-daemon"));
    assert!(
        bin.is_file(),
        "no loom-daemon binary at {} — run `cargo test -p loom-daemon`, which builds it first",
        bin.display()
    );
    bin
}

// ---------------------------------------------------------------------------
// Divergence classes — recognised by MECHANISM
// ---------------------------------------------------------------------------

/// No divergence class is recorded today: every frozen answer matches the
/// port byte for byte.
///
/// The enum exists rather than being elided because the *shape* is the
/// contract — a future divergence gets a variant whose doc comment states its
/// MECHANISM and the direction of its risk, and [`classify`] must compute what
/// that mechanism can produce and require the observed difference to be
/// exactly that. A class recognised by a property of the input ("this case has
/// a newline in it") is a hole shaped like a class.
#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
enum Divergence {}

/// Classify one difference, or return `Err` for "unexplained".
///
/// stdout and stderr are classified independently — a half that no class
/// explains is a finding whatever the other half did.
#[allow(clippy::unnecessary_wraps)]
fn classify(shell: &Answer, port: &Answer) -> Result<Vec<Divergence>, ()> {
    if shell.rc != port.rc || shell.stdout != port.stdout || shell.stderr != port.stderr {
        return Err(());
    }
    Ok(Vec::new())
}

// ---------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------

#[test]
fn port_matches_the_frozen_shell_oracle() {
    let (meta, cases) = load_oracle();
    assert_eq!(
        meta["argv0"].as_str(),
        Some(ARGV0),
        "the oracle was generated with a different $0 sentinel than this test replays with"
    );
    assert_eq!(
        meta["fixed_path"].as_str(),
        Some(FIXED_PATH),
        "the oracle was generated under a different PATH than this test replays with"
    );
    assert_eq!(
        meta["fixed_target"].as_str(),
        Some(FIXED_TARGET),
        "the oracle was generated against a different release target triple"
    );

    let bin = port_bin();
    let leading = vec!["daemon-update".to_string()];
    let mut compared = 0usize;
    let mut classified: BTreeMap<String, usize> = BTreeMap::new();
    let mut unexplained: Vec<String> = Vec::new();

    for (case, shell) in &cases {
        let port = run_case(case, &bin, &leading);
        compared += 1;
        if &port == shell {
            continue;
        }
        match classify(shell, &port) {
            Ok(ds) => {
                for d in ds {
                    *classified.entry(format!("{d:?}")).or_default() += 1;
                }
            }
            Err(()) => unexplained.push(format!(
                "\ncase {} argv={:?}\n  rc     shell={} port={}\n  stdout shell:\n{}\n  stdout port:\n{}\n  stderr shell:\n{}\n  stderr port:\n{}",
                case.id, case.argv, shell.rc, port.rc, shell.stdout, port.stdout, shell.stderr, port.stderr
            )),
        }
    }

    // Assert HOW MANY comparisons ran, not just that none failed: a corpus
    // that silently stopped loading would otherwise report a perfect green
    // zero (`verification-recipes.md` §6, Cause 1).
    assert_eq!(compared, cases.len(), "not every frozen case was replayed");
    assert!(
        compared >= 250,
        "the corpus shrank to {compared} cases — a smaller corpus is a weaker proof, \
         so shrinking it is a decision to record, not a default"
    );
    assert!(
        unexplained.is_empty(),
        "{} of {compared} cases diverge from the retired shell with no recorded class \
         (classified: {classified:?}):{}",
        unexplained.len(),
        unexplained.join("")
    );
}

/// The corpus's DISCRIMINATING POWER, measured rather than assumed.
///
/// "300 cases" says nothing about which behaviour they reach. Each floor below
/// counts the frozen answers that a specific, named mutation of the port would
/// change — so a corpus that quietly stopped exercising a field fails here
/// instead of staying green (`verification-recipes.md` §6).
///
/// Every floor is deliberately well below the observed count: this asserts the
/// corpus still reaches the behaviour, not the exact shape of today's output.
#[test]
fn the_corpus_reaches_every_behaviour_it_claims_to() {
    let (_meta, cases) = load_oracle();
    let mut check_update_available = 0; // mutation: --check stops exiting 3
    let mut check_up_to_date = 0; // mutation: the up-to-date branch inverted
    let mut dry_run_plan = 0; // mutation: the dry-run renderer gutted
    let mut dry_run_build_line = 0; // mutation: the cargo-build plan line dropped
    let mut artifact_fallback = 0; // mutation: the soft-fallback reason dropped
    let mut fetch_forced_refusal = 0; // mutation: --fetch stops hard-failing
    let mut flags_replay = 0; // mutation: .daemon.flags stops being read
    let mut flags_none = 0; // mutation: the FLAGS-OFF default text changed
    let mut not_running = 0; // mutation: "would NOT start it" removed
    let mut drain_no_supervisor = 0; // mutation: the no-supervisor drain advisory dropped
    let mut drain_conflict = 0; // mutation: the mutual-exclusion check removed
    let mut timeout_accepted = 0; // mutation: the ^[0-9]+$ regex anchored wrong
    let mut timeout_refused = 0; // mutation: the same, in the other direction
    let mut usage_refusal = 0; // mutation: unknown-flag rc or text changed
    let mut help_banner = 0; // mutation: help.txt diverges from the banner
    let mut stale_entry_points = 0; // mutation: the #4079 scan removed
    let mut not_a_source_checkout = 0; // mutation: the source-checkout gate removed
    let mut machine_checkout_missing = 0; // mutation: LOOM_MACHINE_CHECKOUT unvalidated
    let mut allow_stale = 0; // mutation: --allow-stale stops being accepted
    let mut no_restart = 0; // mutation: --no-restart stops being honoured
                            // mutation: find_repo_root canonicalises instead of using bash's logical
                            // `cd … && pwd` (the #8088 divergence). Counted by the LOGICAL spelling
                            // appearing in the announcement, which is the whole answer to "which of
                            // the two names for this directory did the implementation report?".
    let mut self_location_logical = 0;

    for (case, a) in &cases {
        let all = format!("{}\n{}", a.stdout, a.stderr);
        if a.rc == 3 && all.contains("Update available") {
            check_update_available += 1;
        }
        if a.rc == 0 && all.contains("already up to date with source HEAD") {
            check_up_to_date += 1;
        }
        if all.contains("[dry-run]") {
            dry_run_plan += 1;
        }
        if all.contains("[dry-run] Would run: (cd ") && all.contains("cargo build --release") {
            dry_run_build_line += 1;
        }
        if all.contains("Artifact-fetch: ")
            && all.contains("falling back to the local source-build path")
        {
            artifact_fallback += 1;
        }
        if all.contains("Refusing to silently fall back to a source build") {
            fetch_forced_refusal += 1;
        }
        if all.contains("Would stop + restart loom-daemon with flags from") {
            flags_replay += 1;
        }
        if all.contains("defaulting to FLAGS-OFF bare restart") {
            flags_none += 1;
        }
        if all.contains("would NOT start it") {
            not_running += 1;
        }
        if all.contains("there is no supervisor to relaunch it, so drain mode has no effect here") {
            drain_no_supervisor += 1;
        }
        if a.rc == 1 && all.contains("--drain and --restart-now are mutually exclusive") {
            drain_conflict += 1;
        }
        // The leading-zero value the regex must ACCEPT: the case asked for it
        // and the run did not refuse.
        if case.argv.iter().any(|t| t == "007") && a.rc != 1 {
            timeout_accepted += 1;
        }
        if all.contains("--timeout requires a numeric SECS argument") {
            timeout_refused += 1;
        }
        if a.rc == 1 && all.contains("Unknown option") {
            usage_refusal += 1;
        }
        if all.contains("Self-update the RAW loom-daemon process") {
            help_banner += 1;
        }
        if all.contains("do NOT resolve to the current loom-daemon binary") {
            stale_entry_points += 1;
        }
        if all.contains("No loom-daemon/Cargo.toml found at") {
            not_a_source_checkout += 1;
        }
        if all.contains("LOOM_MACHINE_CHECKOUT does not exist") {
            machine_checkout_missing += 1;
        }
        if case.argv.iter().any(|f| f == "--allow-stale") && a.rc != 1 {
            allow_stale += 1;
        }
        if all.contains("--no-restart given") {
            no_restart += 1;
        }
        // `{ROOT}/link` is the symlink the case was addressed through;
        // `{ROOT}/repo` is what it resolves to. A canonicalising port prints
        // the second, so requiring the first is a check on the MECHANISM
        // (which name was reported) rather than on a property of the input.
        if case.self_located
            && all.contains("using this script's own checkout: {ROOT}/link")
            && !all.contains("using this script's own checkout: {ROOT}/repo")
        {
            self_location_logical += 1;
        }
    }

    // Each floor is roughly half the count observed when the oracle was
    // frozen: high enough that a behaviour dropping out of the corpus fails
    // here, low enough that it does not re-freeze today's exact shape.
    for (name, count, floor) in [
        ("--check reports an update (exit 3)", check_update_available, 40),
        ("--check reports up to date (exit 0)", check_up_to_date, 5),
        ("the dry-run renderer", dry_run_plan, 80),
        ("the dry-run cargo-build line", dry_run_build_line, 80),
        ("artifact-fetch's soft fallback", artifact_fallback, 20),
        ("--fetch's hard refusal", fetch_forced_refusal, 8),
        (".daemon.flags replay", flags_replay, 50),
        ("the FLAGS-OFF bare-restart default", flags_none, 1),
        ("the was-not-running guardrail", not_running, 25),
        ("the no-supervisor drain advisory", drain_no_supervisor, 15),
        ("--drain/--restart-now mutual exclusion", drain_conflict, 15),
        ("--timeout ACCEPTS a leading-zero value", timeout_accepted, 10),
        ("--timeout REFUSES a non-numeric value", timeout_refused, 4),
        ("the unknown-flag refusal", usage_refusal, 4),
        ("the --help banner", help_banner, 2),
        ("the stale-entry-point scan", stale_entry_points, 3),
        ("the source-checkout gate", not_a_source_checkout, 2),
        ("the LOOM_MACHINE_CHECKOUT existence check", machine_checkout_missing, 2),
        ("--allow-stale", allow_stale, 4),
        ("--no-restart", no_restart, 2),
        ("the #5140 self-location fallback, reported LOGICALLY", self_location_logical, 4),
    ] {
        assert!(
            count >= floor,
            "the corpus reaches '{name}' in only {count} frozen answers (floor {floor}). \
             A mutation to that behaviour could pass unnoticed — extend the corpus rather \
             than lowering the floor."
        );
    }
}

/// The frozen answers must be replayable on a host other than the one that
/// generated them.
///
/// This is the assertion behind the module doc's pinning table. Without it,
/// "it passes on my machine" and "it passes in CI" are two different claims
/// and only the first has been checked — and the failure mode is a CI-only
/// red on a PR that touched nothing.
#[test]
fn the_oracle_is_host_portable() {
    let (_meta, cases) = load_oracle();
    // Tokens that could only come from the generating host leaking into an
    // answer. `{ROOT}`/`{HEAD}`/`{MTIME}` are the placeholders that replace
    // the legitimate ones.
    let forbidden = [
        "/Users/",
        "/home/",
        "/root/",
        "aarch64-apple-darwin",
        "x86_64-apple-darwin",
        "Darwin",
        "launchctl",
        "systemctl",
    ];
    let mut leaks = Vec::new();
    for (case, a) in &cases {
        // The `--help` banner is the ONE exempt answer, and the exemption is
        // narrow on purpose: it is a static document (compiled in by
        // `include_str!`; read from the script's own comment block before the
        // port) whose JOB is to document the platform knobs, so it names
        // `Darwin`, `launchctl`, `systemctl` and every release target triple
        // by construction. None of that is derived from the generating host,
        // so it replays identically everywhere -- which is the property this
        // test is actually about. Exempting the whole answer rather than
        // weakening the token list keeps the check strict for every other
        // case.
        if a.stdout
            .contains("loom-daemon-update.sh - Self-update the RAW loom-daemon process")
        {
            continue;
        }
        for token in forbidden {
            if a.stdout.contains(token) || a.stderr.contains(token) {
                leaks.push(format!("{}: {token}", case.id));
            }
        }
    }
    assert!(
        leaks.is_empty(),
        "frozen answers carry host-specific tokens, so this oracle cannot replay on \
         another host — pin the input that produced them rather than deleting the check: {leaks:?}"
    );
}

// ---------------------------------------------------------------------------
// Corpus construction + oracle generation (ignored by default)
// ---------------------------------------------------------------------------
//
// Split into `differential_daemon_update/corpus.rs` under file-size-policy's
// 1000-code-line ratchet (#8088's own port pushed this file over threshold).
//
// An integration-test file is its own crate root, so plain `mod corpus;`
// would resolve beside it (`tests/corpus.rs`) — which Cargo's own test
// auto-discovery would then also treat as a second, independent test binary.
// The explicit `#[path]` keeps the submodule under `differential_daemon_update/`,
// invisible to that discovery (Cargo only globs `tests/*.rs`, not `tests/*/*.rs`).
#[path = "differential_daemon_update/corpus.rs"]
mod corpus;
