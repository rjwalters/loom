//! Differential test: the ported `daemon-start` against a frozen oracle of the
//! **pre-port shell's** answers (epic #7810, #8087).
//!
//! # Why a retained suite is not enough
//!
//! `defaults/scripts/tests/test-loom-daemon-start.sh` is retained and runs its
//! 143 assertions unchanged against this port. That is a good proof and #8011
//! showed it is not a sufficient one: the `dep_recheck` port shipped three
//! silent divergences while its retained suite was 104/104 green, because a
//! retained suite proves only what its author thought to write down.
//!
//! This port's own history says the same thing louder. The first full run of
//! the retained suite against the Rust was **126/143**, and the largest single
//! cause was one character — `if want == On { "1" } else { "1" }` — i.e. the
//! FLAGS-OFF default inverted, the exact silent-autonomy-change #5409 and the
//! 2026-07-30 incident are about. The retained suite did catch that one. The
//! question this file answers is what it would NOT have caught, and the answer
//! is everything nobody thought to assert: a value containing a newline, an
//! `LOOM_=x` key, a prior plist carrying a poisoned session key, a
//! `--from-config --no-health-gate` pairing against a marker that records
//! `health_gate=1`.
//!
//! # What is compared
//!
//! Each case is a complete, hermetic invocation of the **two pure-inspection
//! modes** (`--print-plist` / `--print-unit`): a fixed environment, a fixture
//! `$HOME`, a fixture repo root, an optional prior installed plist/unit and an
//! optional autonomy marker. Those two modes are the widest side-effect-free
//! surface the script has — one invocation runs the argument loop, the whole
//! autonomy-env resolution (the FLAGS-OFF contract itself), the
//! autonomy-downgrade detector, the scratch-workdir advisory, the
//! agent-session guard, the deterministic-PATH resolver, the `env`-harvest and
//! both renderers, and the dropped-env-key carry-forward merge.
//!
//! stdout, stderr and the exit code are all captured and compared. The refusal
//! paths (exit 1) are deliberately *not* reachable from an inspection mode —
//! they are covered by the retained suite, which drives real starts.
//!
//! # Why an oracle file rather than running the shell here
//!
//! The pre-port shell is gone from the tree (that is the point of the port).
//! Recovering it needs `git show` against a pinned rev, which breaks under CI's
//! default shallow checkout. So its answers are frozen once, by the command in
//! the fixture's `_meta` record, and this test needs no `bash` at all.
//!
//! Regenerate (only when the oracle's provenance legitimately changes):
//!
//! ```sh
//! git show <pre-port-rev>:defaults/scripts/cli/loom-daemon-start.sh \
//!   > defaults/scripts/cli/.oracle-start.sh
//! LOOM_START_ORACLE_SHELL=$PWD/defaults/scripts/cli/.oracle-start.sh \
//!   cargo test -p loom-daemon --test differential_daemon_start -- --ignored --nocapture
//! rm defaults/scripts/cli/.oracle-start.sh
//! ```
//!
//! The oracle script MUST live beside the real `defaults/scripts/lib/` because
//! it sources `../lib/*.sh` relative to itself.
//!
//! # This green number was made red on purpose
//!
//! A suite that cannot be made to fail is not evidence
//! (`verification-recipes.md` §6, Cause 1). Three mutations were applied to the
//! port and the corpus replayed against each, from a 0-unexplained baseline:
//!
//! | mutation | unexplained cases |
//! |---|---|
//! | the FLAGS-OFF default inverted (`else { "1" }`) | 190 / 535 |
//! | `is_session_scoped_env_key` gutted to `false` (the #6568 strip) | 32 / 535 |
//! | `forced.join(",")` reverted to `", "` | 65 / 535 |
//!
//! The third is not hypothetical: it is the divergence this test **found**. The
//! shell joined with `"$(IFS=', '; echo "${FORCED_DESC[*]}")"`, and `${arr[*]}`
//! uses only the FIRST character of `$IFS` — so the port's idiomatic `", "` was
//! wrong and the retained suite was green on it, because no assertion read that
//! line with both loops forced.
//!
//! # What a failure means
//!
//! `UNEXPLAINED` means the port differs from the retired shell in a way nobody
//! has classified. Read the case, decide whether the new behaviour is right,
//! then either fix the port or add the class **with** its reasoning. Do not
//! widen a class to silence it — a class recognised by a property of the input
//! rather than by its mechanism is a hole shaped like a class
//! (`verification-recipes.md` §6).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Frozen answers from the pre-port shell. See the `_meta` record.
const ORACLE: &str = include_str!("fixtures/daemon_start_shell_oracle.jsonl");

/// The `$0` every case is run with, on both sides.
///
/// The shell printed `"$0"`; the port prints `$LOOM_START_ARGV0`, which the stub
/// exports for exactly this reason. Pinning it to a sentinel rather than to a
/// real path keeps the two comparable without post-hoc rewriting of the output.
const ARGV0: &str = "{ARGV0}";

/// The PATH both sides run under. Fixed, because the rendered plist PATH is
/// deterministic by design (#4172) and the harness must not be the thing that
/// makes it vary.
const FIXED_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

// ---------------------------------------------------------------------------
// The corpus
// ---------------------------------------------------------------------------

/// One invocation, fully specified. Everything a case needs is in the fixture,
/// so both sides read the same bytes and the harness cannot lie about which
/// side moved (`verification-recipes.md` §6: generate the corpus ONCE).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Case {
    id: String,
    argv: Vec<String>,
    /// Extra environment on top of the fixed base. A value of `null` in the
    /// fixture means "explicitly absent", which is how the corpus distinguishes
    /// an unset `LOOM_WORK_FINDER` from an empty one.
    env: BTreeMap<String, Option<String>>,
    /// Text of the installed plist, placed at `$HOME/Library/LaunchAgents/<label>.plist`.
    prior_plist: Option<String>,
    /// Text of the installed unit, placed at `$HOME/.config/systemd/user/<unit>`.
    prior_unit: Option<String>,
    /// Text of the autonomy-desired marker.
    marker: Option<String>,
    /// Run from a `/tmp`-shaped repo root instead of the normal fixture root,
    /// to reach the scratch-workdir advisory (#6568 ask 3).
    scratch_root: bool,
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
            prior_plist: json_str(&v, "prior_plist"),
            prior_unit: json_str(&v, "prior_unit"),
            marker: json_str(&v, "marker"),
            scratch_root: v["scratch_root"].as_bool().unwrap_or(false),
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

/// Build the fixture for one case under `root` and return the environment and
/// working directory both sides run with.
///
/// Identical for the shell (at generation) and the port (at replay) — it is one
/// function precisely so a later edit cannot move one side without the other.
fn materialise(case: &Case, root: &Path) -> (BTreeMap<String, String>, PathBuf) {
    let home = root.join("home");
    let state = root.join("state");
    let tmp = root.join("tmp");
    let repo = if case.scratch_root {
        root.join("tmp/pr9999-checkout")
    } else {
        root.join("repo")
    };
    for d in [&home, &state, &tmp, &repo] {
        std::fs::create_dir_all(d).expect("fixture dir");
    }
    std::fs::create_dir_all(repo.join(".loom")).expect("fixture .loom");

    // A fake daemon binary: the start path never runs it in an inspection mode,
    // but `loom_locate_daemon_bin` requires an executable file to resolve.
    let fake = root.join("fake-loom-daemon");
    std::fs::write(&fake, "#!/bin/sh\nexit 0\n").expect("fake daemon");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    let mut env: BTreeMap<String, String> = BTreeMap::new();
    env.insert("HOME".into(), home.display().to_string());
    env.insert("PATH".into(), FIXED_PATH.into());
    env.insert("TMPDIR".into(), tmp.display().to_string());
    env.insert("LOOM_SOCKET_PATH".into(), state.join("loom-daemon.sock").display().to_string());
    env.insert("LOOM_DAEMON_BIN".into(), fake.display().to_string());
    env.insert("LOOM_START_ARGV0".into(), ARGV0.into());
    // Keep every case out of the real per-user supervisor identity even though
    // an inspection mode writes nothing: a default label would make the prior
    // plist path depend on the fixture $HOME only, and the corpus deliberately
    // includes cases that DO leave it unset (the session guard branches on it).
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

    if let Some(text) = &case.prior_plist {
        let label = env
            .get("LOOM_LAUNCHD_LABEL")
            .cloned()
            .unwrap_or_else(|| "com.rjwalters.loom-daemon".to_string());
        let dir = home.join("Library/LaunchAgents");
        std::fs::create_dir_all(&dir).expect("LaunchAgents dir");
        std::fs::write(dir.join(format!("{label}.plist")), text).expect("prior plist");
    }
    if let Some(text) = &case.prior_unit {
        let unit = env
            .get("LOOM_SYSTEMD_UNIT")
            .cloned()
            .unwrap_or_else(|| "loom-daemon.service".to_string());
        let dir = home.join(".config/systemd/user");
        std::fs::create_dir_all(&dir).expect("unit dir");
        std::fs::write(dir.join(unit), text).expect("prior unit");
    }
    if let Some(text) = &case.marker {
        std::fs::write(state.join("autonomy-desired"), text).expect("marker");
    }

    (env, repo)
}

/// Replace every fixture-local absolute path with a stable placeholder.
///
/// Without this the answers would encode one machine's temp directory and could
/// never be frozen. Longest-first so `{ROOT}/home` is not rewritten to
/// `{ROOT}/home` twice over a shorter prefix.
fn normalise(text: &str, root: &Path) -> String {
    let root_s = root.display().to_string();
    let mut out = text.replace(&root_s, "{ROOT}");
    // macOS resolves /var to /private/var, so a fixture under /var/folders can
    // come back either way from the two implementations' path handling.
    if let Some(stripped) = root_s.strip_prefix("/private") {
        out = out.replace(stripped, "{ROOT}");
    }
    // `loom_locate_daemon_bin`'s resolution trace reports the resolved binary's
    // mtime. The fixture's fake daemon is written at fixture-build time, so that
    // stamp is the one thing in the output that is a property of WHEN the case
    // ran rather than of what the implementation did.
    static MTIME: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = MTIME.get_or_init(|| {
        regex::Regex::new(r"mtime: \d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}")
            .expect("static mtime pattern")
    });
    re.replace_all(&out, "mtime: {MTIME}").into_owned()
}

/// Where a case's fixture tree is built — deliberately **not** under `$TMPDIR`.
///
/// `warn_scratch_workdir_drift` treats `$TMPDIR/*`, `/tmp/*`, `/var/folders/*`,
/// `*-checkout` and `*/.loom/worktrees/*` as scratch. A fixture rooted in the
/// system temp dir therefore trips that advisory in EVERY case, which costs
/// twice: the corpus can no longer tell "the advisory fired correctly" from
/// "the advisory always fires" (dimension E stops discriminating), and ~700
/// bytes of identical warning is frozen into every one of the 400+ answers.
///
/// So the tree goes under the runner's cache dir, and dimension E's scratch
/// cases opt IN by rooting the repo at `<root>/tmp/pr9999-checkout`.
fn fixture_base() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    assert!(
        !home.is_empty() && !home.starts_with("/tmp") && !home.starts_with("/var/tmp"),
        "this suite needs a non-scratch $HOME to build fixtures under; got {home:?}. \
         A scratch-shaped base would make the scratch-workdir advisory fire in every \
         case and the frozen answers would no longer match."
    );
    let base = PathBuf::from(home).join(".cache/loom-differential-daemon-start");
    std::fs::create_dir_all(&base).expect("fixture base");
    base
}

/// Run one case against an arbitrary program, in a fresh fixture.
fn run_case(case: &Case, prog: &Path, leading_args: &[String]) -> Answer {
    let root = tempfile::Builder::new()
        .prefix("case-")
        .tempdir_in(fixture_base())
        .expect("tempdir");
    let (env, cwd) = materialise(case, root.path());

    let mut cmd = Command::new(prog);
    cmd.args(leading_args);
    cmd.args(&case.argv);
    cmd.env_clear();
    for (k, v) in &env {
        cmd.env(k, v);
    }
    cmd.current_dir(&cwd);
    let out = cmd.output().expect("spawn");
    Answer {
        rc: out.status.code().unwrap_or(-1),
        stdout: normalise(&String::from_utf8_lossy(&out.stdout), root.path()),
        stderr: normalise(&String::from_utf8_lossy(&out.stderr), root.path()),
    }
}

/// The binary under test.
///
/// `CARGO_BIN_EXE_loom-daemon` rather than a path derived from
/// `current_exe()`: Cargo guarantees it names the binary built from *this*
/// compilation, so a shared `$CARGO_TARGET_DIR` (the arrangement on this fleet)
/// cannot answer with another checkout's build — the #8176 ambiguity that made
/// two unrelated verifications report regressions that did not exist.
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

#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
enum Divergence {
    /// The shell's `xml_escape()` was **broken on bash ≥ 5.2** and the port is
    /// correct.
    ///
    /// Mechanism: bash 5.2 made `&` in the replacement half of
    /// `${var//pat/repl}` expand to the matched text, the way `sed` always has.
    /// `s="${s//</&lt;}"` therefore substitutes `<` → `<` + `lt;`, so the value
    /// `<a & b>` rendered as `<lt;a &amp; b>gt;` — a raw `<` inside a plist
    /// `<string>`, i.e. XML `launchd` cannot parse. (`&` → `&amp;` survived by
    /// coincidence: the match it re-inserts *is* `&`.)
    ///
    /// Recognised by replaying that exact substitution over the port's answer:
    /// the shell's output must be what bash 5.2 would have produced FROM the
    /// port's correctly-escaped text. A differently-escaped value, or a value
    /// that differs for any other reason, is not this class.
    ///
    /// Kept as a fix, deliberately: preserving it would mean shipping a plist
    /// the OS rejects whenever a forwarded variable contains `<` or `>`. The
    /// exposure is real but narrow — `GH_TOKEN` and the `LOOM_*` flags do not
    /// normally carry XML metacharacters — which is why it survived unnoticed.
    XmlEscapeBashMatchRef,
    /// The carry-forward warning names the scratch file it merged into, and the
    /// two implementations have different scratch files to name.
    ///
    /// Mechanism: the shell rendered to `mktemp "$TMPDIR/loom-print-plist.XXXXXX"`
    /// and named that path, so its own output is **not reproducible run to
    /// run** — the suffix is random. The port performs the same merge in memory
    /// (nothing is written by a mode documented as having no side effects) and
    /// keeps the message shape with the template unexpanded.
    ///
    /// Recognised by rewriting exactly the six-character suffix of
    /// `loom-print-{plist,unit}.` on both sides and requiring everything else
    /// to match byte for byte, with the shell's suffix required to be a real
    /// mktemp draw (not the literal template). No other path is rewritten.
    ///
    /// Direction of risk: the named path is a temp file both implementations
    /// delete (the port never creates one), it appears only in an advisory, and
    /// no caller parses it.
    MergeScratchName,
    /// The rendered environment entries carry the SAME key/value multiset in a
    /// different ORDER.
    ///
    /// Mechanism, not coincidence: the shell harvested through `env`, a CHILD
    /// of bash, whose environment bash rebuilds from its own variable hash
    /// table — so the order is bash's hash order. The port walks `environ`
    /// directly via `std::env::vars_os`. Neither order is specified anywhere
    /// and nothing consumes it: launchd reads a `<dict>` and systemd reads
    /// `Environment=` lines, both unordered, and the retained suite asserts
    /// key/value pairs rather than positions.
    ///
    /// The class is computed, not asserted: the two documents must be identical
    /// once the environment block is compared as a multiset AND everything
    /// outside that block matches byte for byte. A single changed value, a
    /// missing key or a different `<key>Label</key>` therefore cannot hide in
    /// here.
    EnvOrder,
}

/// Split a rendered document into (env entries, everything else).
///
/// `launchd`: the `<key>K</key>\n<string>V</string>` pairs inside the
/// `EnvironmentVariables` dict. `systemd`: the `Environment=` lines. The
/// "everything else" half keeps line order, with the env block removed.
fn split_env_block(text: &str) -> (Vec<String>, Vec<String>) {
    let mut entries = Vec::new();
    let mut rest = Vec::new();
    if text.contains("<key>EnvironmentVariables</key>") {
        let lines: Vec<&str> = text.lines().collect();
        let mut i = 0;
        let mut in_env = false;
        while i < lines.len() {
            let line = lines[i];
            if line.contains("<key>EnvironmentVariables</key>") {
                in_env = true;
                rest.push(line.to_string());
                i += 1;
                continue;
            }
            if in_env && line.trim() == "</dict>" {
                in_env = false;
                rest.push(line.to_string());
                i += 1;
                continue;
            }
            if in_env && line.trim_start().starts_with("<key>") {
                // A key line and its value line are ONE entry: comparing them
                // separately would let a value move between keys unnoticed.
                let value = lines.get(i + 1).copied().unwrap_or_default();
                entries.push(format!("{line}\n{value}"));
                i += 2;
                continue;
            }
            rest.push(line.to_string());
            i += 1;
        }
    } else {
        for line in text.lines() {
            if line.starts_with("Environment=") {
                entries.push(line.to_string());
            } else {
                rest.push(line.to_string());
            }
        }
    }
    (entries, rest)
}

fn multiset(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

/// What bash ≥ 5.2 makes of the shell's three `${s//pat/repl}` substitutions.
///
/// A literal transliteration of the RETIRED shell's `xml_escape()` under bash
/// 5.2's `&`-is-the-match rule — **not** of the port's escaper. It models the
/// oracle, so "sync it to the implementation" is exactly the edit that would
/// silently delete this check: if the port's escaping ever changes, this test
/// going red is the point.
fn bash52_xml_escape(s: &str) -> String {
    // `&` → `&amp;`: the `&` in the replacement re-inserts the match, giving
    // `&` + `amp;`, which is the intended text by coincidence.
    let s = s.replace('&', "&amp;");
    // `<` → `&lt;`: the `&` re-inserts `<`, giving `<lt;`. Same for `>`.
    let s = s.replace('<', "<lt;");
    s.replace('>', ">gt;")
}

/// Undo the port's escaping to recover the raw value, so the shell's answer can
/// be predicted from it.
fn unescape_xml(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// Rewrite the random half of a `mktemp` draw, and nothing else.
fn mask_merge_scratch(text: &str) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"loom-print-(plist|unit)\.[A-Za-z0-9]{6}")
            .expect("static mktemp pattern")
    });
    re.replace_all(text, "loom-print-$1.{SCRATCH}").into_owned()
}

/// Classify one difference, or return `Err` for "unexplained".
///
/// stdout and stderr are classified independently — one case can legitimately
/// carry a stdout class and a stderr class at once (a re-render whose carry-
/// forward advisory names a scratch file AND whose env block is in bash's hash
/// order) — and a half that no class explains is a finding whatever the other
/// half did.
///
/// Every arm computes what its mechanism can produce and requires the observed
/// difference to be exactly that — never "this case has an X in it".
fn classify(shell: &Answer, port: &Answer) -> Result<Vec<Divergence>, ()> {
    if shell.rc != port.rc {
        return Err(());
    }
    let mut found = Vec::new();
    if shell.stderr != port.stderr {
        found.push(classify_stderr(shell, port).ok_or(())?);
    }
    if shell.stdout != port.stdout {
        found.extend(classify_stdout(shell, port).ok_or(())?);
    }
    Ok(found)
}

/// MergeScratchName: identical once the six random characters are masked, and
/// the shell's draw must be a real one (the port emits the template).
fn classify_stderr(shell: &Answer, port: &Answer) -> Option<Divergence> {
    let shell_drew = shell.stderr.contains("loom-print-") && !shell.stderr.contains(".XXXXXX");
    let port_kept_template = port.stderr.contains(".XXXXXX");
    if shell_drew
        && port_kept_template
        && mask_merge_scratch(&shell.stderr) == mask_merge_scratch(&port.stderr)
    {
        return Some(Divergence::MergeScratchName);
    }
    None
}

/// Classify a stdout difference as one OR BOTH of the two rendering classes.
///
/// They compose: a case whose forwarded value contains `<` reaches the escaping
/// class, and the env block of that same document is still in bash's hash
/// order. Returning one and stopping would have made whichever came second look
/// unexplained (it did, for exactly one case, until this composed).
fn classify_stdout(shell: &Answer, port: &Answer) -> Option<Vec<Divergence>> {
    // XmlEscapeBashMatchRef: the shell's document must be exactly what bash 5.2
    // would have produced from the port's own (correctly escaped) values.
    let shell_from_port: String = port
        .stdout
        .lines()
        .map(|line| {
            let t = line.trim_start();
            if let (Some(open), Some(close)) = (t.find("<string>"), t.rfind("</string>")) {
                if open < close {
                    let indent = &line[..line.len() - t.len()];
                    let raw = unescape_xml(&t[open + "<string>".len()..close]);
                    return format!("{indent}<string>{}</string>", bash52_xml_escape(&raw));
                }
            }
            line.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");
    // `lines()` drops the trailing newline; put it back before comparing.
    let shell_from_port = if port.stdout.ends_with('\n') {
        format!("{shell_from_port}\n")
    } else {
        shell_from_port
    };
    let escaping_moved = shell_from_port != port.stdout;
    if shell_from_port == shell.stdout {
        return Some(vec![Divergence::XmlEscapeBashMatchRef]);
    }

    // Order-insensitive comparison, against the ESCAPING-MODELLED port output so
    // the two classes can hold at once. `escaping_moved` gates whether the
    // escaping class is actually claimed: modelling a document with no
    // metacharacter in it is the identity, and claiming a class that changed
    // nothing would inflate its count.
    let (s_env, s_rest) = split_env_block(&shell.stdout);
    let (p_env, p_rest) = split_env_block(&shell_from_port);
    if s_rest == p_rest && multiset(s_env.clone()) == multiset(p_env.clone()) && s_env != p_env {
        let mut found = vec![Divergence::EnvOrder];
        if escaping_moved {
            found.push(Divergence::XmlEscapeBashMatchRef);
        }
        return Some(found);
    }
    None
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

    let bin = port_bin();
    let leading = vec!["daemon-start".to_string()];
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

    // Assert HOW MANY comparisons ran, not just that none failed: a corpus that
    // silently stopped loading would otherwise report a perfect green zero.
    assert_eq!(compared, cases.len(), "not every frozen case was replayed");
    assert!(
        compared >= 500,
        "the corpus shrank to {compared} cases — it was frozen at 535; a smaller \
         corpus is a weaker proof, so shrinking it is a decision to record, not a default"
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
/// "600 cases" says nothing about which behaviour they reach. Each floor below
/// counts the frozen answers that a specific, named mutation of the port would
/// change — so a corpus that quietly stopped exercising a field fails here
/// instead of staying green (`verification-recipes.md` §6).
///
/// Every floor is deliberately well below the observed count: this asserts the
/// corpus still reaches the behaviour, not the exact shape of today's output.
#[test]
fn the_corpus_reaches_every_behaviour_it_claims_to() {
    let (_meta, cases) = load_oracle();
    let mut flags_off = 0; // mutation: the FLAGS-OFF default flipped to 1
    let mut opted_in = 0; // mutation: --work-finder stopped forcing 1
    let mut config_driven = 0; // mutation: --from-config stopped leaving it unset
    let mut downgrade = 0; // mutation: the downgrade detector removed
    let mut session_guard = 0; // mutation: the session-context guard removed
    let mut session_strip = 0; // mutation: the #6568 env strip removed
    let mut carry_forward = 0; // mutation: the #5344 carry-forward merge removed
    let mut scratch = 0; // mutation: the scratch-workdir advisory removed
    let mut units = 0; // the whole systemd renderer
    let mut path_override = 0; // mutation: LOOM_DAEMON_PATH ignored

    for (case, a) in &cases {
        let wf0 = a
            .stdout
            .contains("<key>LOOM_WORK_FINDER</key>\n        <string>0</string>")
            || a.stdout.contains("Environment=LOOM_WORK_FINDER=0");
        let wf1 = a
            .stdout
            .contains("<key>LOOM_WORK_FINDER</key>\n        <string>1</string>")
            || a.stdout.contains("Environment=LOOM_WORK_FINDER=1");
        let wf_absent = !a.stdout.contains("LOOM_WORK_FINDER");
        let explicit = case.argv.iter().any(|f| f == "--work-finder");
        let pre_exported = case
            .env
            .get("LOOM_WORK_FINDER")
            .is_some_and(std::option::Option::is_some);
        if wf0 && !pre_exported && !case.argv.iter().any(|f| f == "--no-work-finder") {
            flags_off += 1;
        }
        if wf1 && explicit && !pre_exported {
            opted_in += 1;
        }
        if wf_absent && case.argv.iter().any(|f| f == "--from-config") {
            config_driven += 1;
        }
        if a.stderr.contains("WARNING: autonomy downgrade") {
            downgrade += 1;
        }
        if a.stderr.contains("agent-session context detected") {
            session_guard += 1;
        }
        // A case whose ENV carries a session key and whose RENDER does not.
        if case.env.iter().any(|(k, v)| {
            v.is_some()
                && (k.starts_with("LOOM_SWEEP_")
                    || k == "LOOM_ROLE"
                    || k == "LOOM_TERMINAL_ID"
                    || k == "LOOM_RUNTIME")
        }) && !a.stdout.contains("LOOM_TERMINAL_ID")
            && !a.stdout.contains("LOOM_SWEEP_")
        {
            session_strip += 1;
        }
        if a.stderr.contains("Carrying the installed value") {
            carry_forward += 1;
        }
        if a.stderr.contains("SCRATCH / temporary directory") {
            scratch += 1;
        }
        if a.stdout.contains("[Service]") {
            units += 1;
        }
        if case
            .env
            .get("LOOM_DAEMON_PATH")
            .is_some_and(std::option::Option::is_some)
            && a.stderr.contains("full override via LOOM_DAEMON_PATH")
        {
            path_override += 1;
        }
    }

    for (name, count, floor) in [
        ("FLAGS-OFF default reached", flags_off, 100),
        ("explicit opt-in reached", opted_in, 40),
        ("--from-config leaves it unset", config_driven, 20),
        ("autonomy-downgrade warning reached", downgrade, 20),
        ("agent-session guard reached", session_guard, 4),
        ("agent-session env strip reached", session_strip, 4),
        ("dropped-key carry-forward reached", carry_forward, 4),
        ("scratch-workdir advisory reached", scratch, 4),
        ("systemd unit renderer reached", units, 100),
        ("LOOM_DAEMON_PATH override reached", path_override, 2),
    ] {
        assert!(
            count >= floor,
            "the corpus reaches '{name}' in only {count} frozen answers (floor {floor}). \
             A mutation to that behaviour could pass unnoticed — extend the corpus rather \
             than lowering the floor."
        );
    }
}

// ---------------------------------------------------------------------------
// Corpus construction + oracle generation (ignored by default)
// ---------------------------------------------------------------------------

/// Build the corpus from the grammar the script parses, not from memory.
///
/// Deterministic and order-stable: the fixture is regenerated by replaying this
/// exact function, so a diff of the fixture is a diff of this function.
fn build_corpus() -> Vec<Case> {
    let mut cases = Vec::new();
    let mut push = |id: String,
                    argv: Vec<&str>,
                    env: Vec<(&str, Option<&str>)>,
                    prior_plist: Option<String>,
                    prior_unit: Option<String>,
                    marker: Option<String>,
                    scratch_root: bool| {
        cases.push(Case {
            id,
            argv: argv
                .into_iter()
                .map(std::string::ToString::to_string)
                .collect(),
            env: env
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.map(std::string::ToString::to_string)))
                .collect(),
            prior_plist,
            prior_unit,
            marker,
            scratch_root,
        });
    };

    let wf_flags = [None, Some("--work-finder"), Some("--no-work-finder")];
    let hg_flags = [None, Some("--health-gate"), Some("--no-health-gate")];
    let modes = ["--print-plist", "--print-unit"];

    // A. The autonomy matrix: every flag pairing against the pre-exported
    //    values, in both renderers. This is the FLAGS-OFF contract itself.
    //
    //    The pre-export axis is the full `{unset, empty, "0", "1"}` grammar on
    //    LOOM_WORK_FINDER, because `${VAR:-default}` treats empty as unset and
    //    an exported "0" is the "explicit, not silent" signal the downgrade
    //    check keys on. It is `{unset, "1"}` on LOOM_MAIN_HEALTH_GATE: the two
    //    loops run through one shared `export_default`, so the remaining
    //    spellings are re-tested on the same code path rather than a new one,
    //    and each extra combination freezes another ~1.3 KB plist into the
    //    oracle. The measured floors in
    //    [`the_corpus_reaches_every_behaviour_it_claims_to`] are what holds
    //    this trade honest.
    let pre_wf = [None, Some(""), Some("0"), Some("1")];
    let pre_hg = [None, Some("1")];
    for mode in modes {
        for wf in wf_flags {
            for hg in hg_flags {
                for from_config in [false, true] {
                    for pwf in pre_wf {
                        for phg in pre_hg {
                            let mut argv = vec![mode];
                            if from_config {
                                argv.push("--from-config");
                            }
                            if let Some(f) = wf {
                                argv.push(f);
                            }
                            if let Some(f) = hg {
                                argv.push(f);
                            }
                            let id = format!(
                                "A/{mode}/{}/{}/{}/{}/{}",
                                wf.unwrap_or("-"),
                                hg.unwrap_or("-"),
                                if from_config { "cfg" } else { "-" },
                                pwf.map_or("unset", |v| if v.is_empty() { "empty" } else { v }),
                                phg.unwrap_or("unset"),
                            );
                            push(
                                id,
                                argv,
                                vec![
                                    ("LOOM_WORK_FINDER", pwf),
                                    ("LOOM_MAIN_HEALTH_GATE", phg),
                                    ("LOOM_LAUNCHD_LABEL", Some("com.example.diff")),
                                    ("LOOM_SYSTEMD_UNIT", Some("loom-diff.service")),
                                ],
                                None,
                                None,
                                None,
                                false,
                            );
                        }
                    }
                }
            }
        }
    }

    // B. The downgrade detector: prior installed value × marker shape × flags.
    let prior_plist_with = |wf: &str| {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\">\n<dict>\n\
             \x20   <key>EnvironmentVariables</key>\n    <dict>\n\
             \x20       <key>LOOM_WORK_FINDER</key>\n        <string>{wf}</string>\n\
             \x20       <key>LOOM_MAIN_HEALTH_GATE</key>\n        <string>{wf}</string>\n\
             \x20   </dict>\n</dict>\n</plist>\n"
        )
    };
    let prior_unit_with = |wf: &str| {
        format!(
            "[Service]\nEnvironment=LOOM_WORK_FINDER={wf}\nEnvironment=LOOM_MAIN_HEALTH_GATE={wf}\n"
        )
    };
    let markers = [
        ("absent", None),
        ("old", Some("# loom autonomy-desired marker\nuse_launchd=false\n".to_string())),
        ("wf0", Some("work_finder=0\nhealth_gate=0\n".to_string())),
        ("wf1", Some("work_finder=1\nhealth_gate=0\n".to_string())),
    ];
    for mode in modes {
        for wf in wf_flags {
            for from_config in [false, true] {
                for (mname, marker) in &markers {
                    for prior in ["absent", "0", "1"] {
                        let mut argv = vec![mode];
                        if from_config {
                            argv.push("--from-config");
                        }
                        if let Some(f) = wf {
                            argv.push(f);
                        }
                        let (pp, pu) = match (mode, prior) {
                            (_, "absent") => (None, None),
                            ("--print-plist", v) => (Some(prior_plist_with(v)), None),
                            (_, v) => (None, Some(prior_unit_with(v))),
                        };
                        push(
                            format!(
                                "B/{mode}/{}/{}/{mname}/prior{prior}",
                                wf.unwrap_or("-"),
                                if from_config { "cfg" } else { "-" }
                            ),
                            argv,
                            vec![
                                ("LOOM_LAUNCHD_LABEL", Some("com.example.diff")),
                                ("LOOM_SYSTEMD_UNIT", Some("loom-diff.service")),
                            ],
                            pp,
                            pu,
                            marker.clone(),
                            false,
                        );
                    }
                }
            }
        }
    }

    // C. The env-harvest grammar: what `env | grep -E '^(LOOM_…|GH_TOKEN|…)='`
    //    admits, and what it does to a value the harness can construct.
    let env_grammar: Vec<(&str, &str, &str)> = vec![
        ("plain", "LOOM_EXTRA", "value"),
        ("empty-value", "LOOM_EMPTY", ""),
        ("equals-in-value", "LOOM_EQ", "a=b=c"),
        ("newline-in-value", "LOOM_NL", "first\nLOOM_SNEAK=second"),
        ("xml-metachars", "LOOM_XML", "<a & b> \"q\" 'r'"),
        ("percent-b", "LOOM_PCT", "100%b\\n\\t"),
        ("trailing-space", "LOOM_WS", "  padded  "),
        ("unicode", "LOOM_UNI", "café—ünïcode"),
        ("bare-prefix", "LOOM_", "empty-key-suffix"),
        ("digits", "LOOM_9", "9"),
        ("underscore", "LOOM__", "u"),
        ("not-matched-prefix", "XLOOM_A", "no"),
        ("token-gh", "GH_TOKEN", "ghp_example"),
        ("token-gitea", "GITEA_TOKEN", "gitea_example"),
        ("token-forge", "FORGE_TOKEN", "forge_example"),
        ("token-near-miss", "GH_TOKENX", "no"),
        ("supervisor-collision", "LOOM_DAEMON_SUPERVISOR", "someone-else"),
        ("workspace", "LOOM_WORKSPACE", "/somewhere/else"),
        ("dollar", "LOOM_DOLLAR", "$HOME $(id -u) `id -u`"),
        ("backslash", "LOOM_BS", "a\\b\\\\c"),
    ];
    for mode in modes {
        for (name, key, value) in &env_grammar {
            push(
                format!("C/{mode}/{name}"),
                vec![mode],
                vec![
                    (key, Some(value)),
                    ("LOOM_LAUNCHD_LABEL", Some("com.example.diff")),
                    ("LOOM_SYSTEMD_UNIT", Some("loom-diff.service")),
                ],
                None,
                None,
                None,
                false,
            );
        }
    }

    // D. The agent-session guard and the #6568 strip.
    for mode in modes {
        for label in [None, Some("com.example.diff")] {
            for session in [
                vec![("LOOM_ROLE", "sweep-lifecycle")],
                vec![
                    ("LOOM_TERMINAL_ID", "t-1"),
                    ("LOOM_SWEEP_CLAIM_OWNED", "8087"),
                ],
                vec![
                    ("LOOM_ROLE", "builder"),
                    ("LOOM_RUNTIME", "claude"),
                    ("LOOM_SWEEP_CPU_BUDGET_CORES", "2"),
                ],
            ] {
                for allow in [None, Some("1")] {
                    let mut env: Vec<(&str, Option<&str>)> =
                        session.iter().map(|(k, v)| (*k, Some(*v))).collect();
                    env.push(("LOOM_LAUNCHD_LABEL", label));
                    env.push(("LOOM_SYSTEMD_UNIT", label.map(|_| "loom-diff.service")));
                    env.push(("LOOM_ALLOW_SESSION_DAEMON_START", allow));
                    push(
                        format!(
                            "D/{mode}/{}/{}/{}",
                            label.unwrap_or("nolabel"),
                            session.len(),
                            allow.unwrap_or("noallow")
                        ),
                        vec![mode],
                        env,
                        None,
                        None,
                        None,
                        false,
                    );
                }
            }
        }
    }

    // E. The scratch-workdir advisory (#6568 ask 3).
    for mode in modes {
        for workspace in [None, Some("/tmp/scratch-workspace"), Some("/home/u/real")] {
            for scratch_root in [false, true] {
                push(
                    format!(
                        "E/{mode}/{}/{}",
                        workspace.unwrap_or("nows"),
                        if scratch_root { "scratch" } else { "normal" }
                    ),
                    vec![mode],
                    vec![
                        ("LOOM_WORKSPACE", workspace),
                        ("LOOM_LAUNCHD_LABEL", Some("com.example.diff")),
                        ("LOOM_SYSTEMD_UNIT", Some("loom-diff.service")),
                    ],
                    None,
                    None,
                    None,
                    scratch_root,
                );
            }
        }
    }

    // F. The dropped-env-key detector and its carry-forward merge (#4522/#5344),
    //    including a prior file poisoned with agent-session keys (#6568), which
    //    the merge must PURGE rather than faithfully re-inject.
    let rich_plist =
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\">\n<dict>\n\
         \x20   <key>EnvironmentVariables</key>\n    <dict>\n\
         \x20       <key>LOOM_SAFEHOUSE_ROOM</key>\n        <string>!room:example</string>\n\
         \x20       <key>LOOM_WORK_FINDER</key>\n        <string>1</string>\n\
         \x20       <key>LOOM_TERMINAL_ID</key>\n        <string>poisoned</string>\n\
         \x20       <key>LOOM_SWEEP_CLAIM_OWNED</key>\n        <string>6388</string>\n\
         \x20   </dict>\n</dict>\n</plist>\n";
    let rich_unit = "[Service]\nEnvironment=LOOM_SAFEHOUSE_ROOM=!room:example\n\
         Environment=LOOM_WORK_FINDER=1\nEnvironment=LOOM_ROLE=poisoned\n\
         Environment=LOOM_SWEEP_CLAIM_OWNED=6388\n";
    for mode in modes {
        for force_env in [false, true] {
            for extra_flag in [None, Some("--work-finder")] {
                let mut argv = vec![mode];
                if force_env {
                    argv.push("--force-env");
                }
                if let Some(f) = extra_flag {
                    argv.push(f);
                }
                let (pp, pu) = if mode == "--print-plist" {
                    (Some(rich_plist.to_string()), None)
                } else {
                    (None, Some(rich_unit.to_string()))
                };
                push(
                    format!(
                        "F/{mode}/{}/{}",
                        if force_env { "force" } else { "keep" },
                        extra_flag.unwrap_or("-")
                    ),
                    argv,
                    vec![
                        ("LOOM_LAUNCHD_LABEL", Some("com.example.diff")),
                        ("LOOM_SYSTEMD_UNIT", Some("loom-diff.service")),
                    ],
                    pp,
                    pu,
                    None,
                    false,
                );
            }
        }
    }

    // G. The deterministic-PATH resolver (#4172) and its two overrides.
    for mode in modes {
        for (name, path, extra) in [
            ("default", None, None),
            ("override", Some("/only/this"), None),
            ("extra", None, Some("/extra/one:/extra/two")),
            ("both", Some("/only/this"), Some("/extra/one")),
            ("empty-override", Some(""), None),
        ] {
            push(
                format!("G/{mode}/{name}"),
                vec![mode],
                vec![
                    ("LOOM_DAEMON_PATH", path),
                    ("LOOM_DAEMON_PATH_EXTRA", extra),
                    ("LOOM_LAUNCHD_LABEL", Some("com.example.diff")),
                    ("LOOM_SYSTEMD_UNIT", Some("loom-diff.service")),
                ],
                None,
                None,
                None,
                false,
            );
        }
    }

    // H. The argument loop itself: aliases, unknown flags, `--help`, and the
    //    order-independence of the two inspection modes.
    for argv in [
        vec!["--help"],
        vec!["-h"],
        vec!["--nope"],
        vec!["--print-plist", "--print-unit"],
        vec!["--print-unit", "--print-plist"],
        vec![
            "--print-plist",
            "--from-config",
            "--work-finder",
            "--no-health-gate",
        ],
        vec![
            "--print-unit",
            "--force-env",
            "--no-work-finder",
            "--health-gate",
        ],
        vec!["--print-plist", "--work-finder", "--work-finder"],
        vec!["--print-plist", "--no-launchd", "--no-systemd"],
    ] {
        push(
            format!("H/{}", argv.join("+")),
            argv,
            vec![
                ("LOOM_LAUNCHD_LABEL", Some("com.example.diff")),
                ("LOOM_SYSTEMD_UNIT", Some("loom-diff.service")),
            ],
            None,
            None,
            None,
            false,
        );
    }

    cases
}

/// Regenerate `fixtures/daemon_start_shell_oracle.jsonl` from the pre-port
/// shell. Ignored by default; see the module docs for the exact command.
#[test]
#[ignore = "regenerates the frozen oracle; needs the pre-port shell via LOOM_START_ORACLE_SHELL"]
fn regenerate_oracle() {
    let shell = std::env::var("LOOM_START_ORACLE_SHELL")
        .expect("set LOOM_START_ORACLE_SHELL to the pre-port loom-daemon-start.sh");
    let shell = PathBuf::from(shell);
    assert!(shell.is_file(), "no such shell: {}", shell.display());

    // The bash the oracle was produced by is provenance, not trivia: one of the
    // recorded divergences (`XmlEscapeBashMatchRef`) exists only because bash
    // 5.2 changed what `&` means in the replacement half of `${var//pat/repl}`.
    // An oracle regenerated under bash 5.1 would answer differently.
    // The rev the reference shell came from — the one thing that makes these
    // answers attributable. `git show <rev>:defaults/scripts/cli/loom-daemon-start.sh`
    // recovers exactly the implementation that produced them.
    let reference_rev = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    let bash_version = Command::new("/bin/bash")
        .args(["-c", "echo -n \"$BASH_VERSION\""])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();

    let cases = build_corpus();
    let mut out = String::new();
    out.push_str(&serde_json::json!({"_meta": {
        "subject": "loom-daemon daemon-start (defaults/scripts/cli/loom-daemon-start.sh)",
        "reference_impl": shell.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
        "reference_rev": reference_rev,
        "bash_version": bash_version,
        "generated_by": "cargo test -p loom-daemon --test differential_daemon_start -- --ignored regenerate_oracle",
        "argv0": ARGV0,
        "fixed_path": FIXED_PATH,
        "cases": cases.len(),
        "note": "Answers are the PRE-PORT shell's. Paths are normalised to {ROOT}. \
                 Regenerating this file against the PORT would make the test tautological.",
    }}).to_string());
    out.push('\n');

    for case in &cases {
        let answer = run_case(case, Path::new("/bin/bash"), &[shell.display().to_string()]);
        let env: serde_json::Map<String, serde_json::Value> = case
            .env
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    v.clone()
                        .map_or(serde_json::Value::Null, serde_json::Value::String),
                )
            })
            .collect();
        let mut rec = serde_json::json!({
            "id": case.id,
            "argv": case.argv,
            "env": env,
            "scratch_root": case.scratch_root,
            "rc": answer.rc,
            "stdout": answer.stdout,
            "stderr": answer.stderr,
        });
        if let Some(t) = &case.prior_plist {
            rec["prior_plist"] = serde_json::Value::String(t.clone());
        }
        if let Some(t) = &case.prior_unit {
            rec["prior_unit"] = serde_json::Value::String(t.clone());
        }
        if let Some(t) = &case.marker {
            rec["marker"] = serde_json::Value::String(t.clone());
        }
        out.push_str(&rec.to_string());
        out.push('\n');
    }

    let dest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/daemon_start_shell_oracle.jsonl");
    std::fs::write(&dest, out).expect("write oracle");
    println!("wrote {} cases to {}", cases.len(), dest.display());
}

/// The frozen corpus must still be the corpus [`build_corpus`] describes.
///
/// Without this, an edit to the generator and a stale fixture drift apart
/// silently: the replay would keep passing against yesterday's cases while the
/// grammar it claims to cover had moved.
#[test]
fn the_frozen_cases_are_the_ones_the_generator_builds() {
    let (_meta, frozen) = load_oracle();
    let built = build_corpus();
    assert_eq!(
        frozen.len(),
        built.len(),
        "the frozen oracle has {} cases and the generator now builds {} — regenerate it \
         (see the module docs) rather than editing the fixture by hand",
        frozen.len(),
        built.len()
    );
    for ((frozen_case, _), built_case) in frozen.iter().zip(built.iter()) {
        assert_eq!(
            frozen_case, built_case,
            "frozen case {} no longer matches the generator",
            frozen_case.id
        );
    }
}
