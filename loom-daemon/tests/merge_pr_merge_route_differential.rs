//! Differential test: `merge-pr.sh`'s synchronous merge-retry loop as it is
//! NOW (extracted from the live script, delegating its per-attempt route to
//! `loom-daemon merge-pr merge-route`) against the frozen loop it replaced
//! (`tests/fixtures/merge-pr-merge-route-retired.sh`) — #8191 slice.
//!
//! This compares the CALL SHAPE, not just the Rust function: both loops run
//! under bash with the same recording stubs for every side effect
//! (`forge_merge_pr`, `forge_get_pr_nocache`, `forge_update_branch`, `sleep`,
//! `_refresh_precondition_sha`, `_head_moved_or_resync`, the narration
//! helpers and `error`), driven by the same scripted forge, with the real
//! binary (behind a recording wrapper for `record-rework`) answering
//! `classify-response` and `merge-route`. Every scenario's trace — each merge
//! call and the precondition SHA it gated on, each sleep, sync, head re-read,
//! rework marker, narrated line and refusal — and its exit code must be
//! identical byte for byte.
//!
//! A second pass makes the wrapper refuse `merge-route` (a binary predating
//! the verb) and checks the live loop's fail-open fallback reproduces the
//! retired CONTROL FLOW exactly — same merges, sleeps, syncs, re-reads,
//! backoff durations and exit code — with only the wording allowed to differ.
//!
//! One once-generated corpus (verification-recipes §6) feeds both sides.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fixture_path() -> PathBuf {
    manifest().join("tests/fixtures/merge-pr-merge-route-retired.sh")
}

fn merge_pr_path() -> PathBuf {
    manifest().join("../defaults/scripts/merge-pr.sh")
}

/// The bash driver shared by both sides. `$1` = retired|live, `$2` = fixture,
/// `$3` = live merge-pr.sh, `$4` = scenario dir.
const DRIVER: &str = r#"
set -euo pipefail
mode="$1"; fixture="$2"; mp="$3"; D="$4"; T="$D/trace"
info() { printf 'info\t%s\n' "$*" >>"$T"; }
success() { printf 'success\t%s\n' "$*" >>"$T"; }
warning() { printf 'warning\t%s\n' "$*" >>"$T"; }
error() { printf 'ERROR\t%s\n' "$*" >>"$T"; exit 1; }
sleep() { printf 'SLEEP %s\n' "$1" >>"$T"; }
_next() { local n; n=$(( $(cat "$D/$1.n" 2>/dev/null || echo 0) + 1 )); echo "$n" >"$D/$1.n"; echo "$n"; }
forge_merge_pr() { local n; n="$(_next merge)"; printf 'MERGE %s %s\n' "$n" "$3" >>"$T"; [[ -f "$D/merge.$n" ]] || return 0; cat "$D/merge.$n"; return 1; }
forge_get_pr_nocache() { local n; n="$(_next get)"; if [[ -f "$D/get.$n" ]]; then echo '{"merged":true}'; else echo '{}'; fi; }
forge_update_branch() { echo UPDATE >>"$T"; [[ ! -f "$D/update-fails" ]] || { echo "boom: HTTP 422"; return 1; }; }
_refresh_precondition_sha() { echo REFRESH >>"$T"; MERGE_PRECONDITION_SHA="synced$MERGE_ATTEMPT"; }
_head_moved_or_resync() { echo HEAD-MISMATCH >>"$T"; exit 3; }
_mp_daemon_roll_hint() { :; }
eval "$(grep '^_classify_merge_response()' "$mp")"
eval "$(grep '^_mr_say()' "$mp")"
LOOM_DAEMON_BIN="$D/bin"
REPO_NWO=o/r; PR_NUMBER=42; GH=gh; PR_BRANCH=feature/x; REPO_ROOT=/repo
MERGE_PRECONDITION_SHA=approved; REPO_MERGE_METHOD=squash; SCRIPT_DIR=/s
if [[ "$mode" == retired ]]; then
  # shellcheck source=/dev/null
  source "$fixture"; _retired_merge_loop
else
  body="$(awk '/^MAX_MERGE_RETRIES=/{g=1} g{print} g&&/^done$/{exit}' "$mp")"
  [[ -n "$body" ]] || { echo "could not extract the live loop" >&2; exit 99; }
  eval "_live_merge_loop() {
$body
}"
  _live_merge_loop
fi
echo LOOP-END >>"$T"
"#;

struct Scenario {
    responses: Vec<Vec<u8>>,
    /// 1-based `forge_get_pr_nocache` calls that report `merged: true`.
    merged_reads: Vec<usize>,
    update_fails: bool,
}

fn run_side(mode: &str, sc: &Scenario, no_route: bool, root: &Path, tag: &str) -> (i32, Vec<u8>) {
    let d = root.join(tag);
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    for (i, r) in sc.responses.iter().enumerate() {
        fs::write(d.join(format!("merge.{}", i + 1)), r).unwrap();
    }
    for n in &sc.merged_reads {
        fs::write(d.join(format!("get.{n}")), "").unwrap();
    }
    if sc.update_fails {
        fs::write(d.join("update-fails"), "").unwrap();
    }
    let real = env!("CARGO_BIN_EXE_loom-daemon");
    let refuse = if no_route {
        "[[ \"$1 $2\" != \"merge-pr merge-route\" ]] || exit 2\n"
    } else {
        ""
    };
    let wrapper = format!(
        "#!/usr/bin/env bash\nif [[ \"$1\" == record-rework ]]; then shift; printf 'REWORK %s\\n' \"$*\" >>\"$(dirname \"$0\")/trace\"; exit 0; fi\n{refuse}exec '{real}' \"$@\"\n"
    );
    let bin = d.join("bin");
    fs::write(&bin, wrapper).unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    let out = Command::new("bash")
        .args(["-c", DRIVER, "driver", mode])
        .arg(fixture_path())
        .arg(merge_pr_path())
        .arg(&d)
        .output()
        .expect("bash ran");
    assert_ne!(
        out.status.code(),
        Some(99),
        "driver could not extract the live loop: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let trace = fs::read(d.join("trace")).unwrap_or_default();
    (out.status.code().unwrap_or(-1), trace)
}

/// The degraded (no `merge-route`) comparison keeps every side effect and
/// drops only wording: narration lines go, a refusal keeps only that it
/// happened, and a rework marker keeps only the duration it recorded.
fn control_flow(trace: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(trace)
        .lines()
        .filter(|l| {
            !(l.starts_with("info\t") || l.starts_with("success\t") || l.starts_with("warning\t"))
        })
        .map(|l| {
            if l.starts_with("ERROR\t") {
                "ERROR".to_string()
            } else if let Some(rest) = l.strip_prefix("REWORK ") {
                format!(
                    "REWORK {}",
                    rest.rsplit("--duration-sec").next().unwrap_or("").trim()
                )
            } else {
                l.to_string()
            }
        })
        // A refusal's continuation lines (a multi-line response) are wording.
        .filter(|l| {
            l == "ERROR"
                || l == "LOOP-END"
                || l == "UPDATE"
                || l == "REFRESH"
                || l == "HEAD-MISMATCH"
                || l.starts_with("MERGE ")
                || l.starts_with("SLEEP ")
                || l.starts_with("REWORK ")
        })
        .collect()
}

fn corpus() -> Vec<Scenario> {
    let all: Vec<Vec<u8>> = vec![
        b"Merge already in progress (HTTP 405)".to_vec(),
        b"gh: Base branch was modified. Review and try the merge again. (HTTP 405)".to_vec(),
        b"gh: Head branch was modified. Review and try the merge again. (HTTP 409)".to_vec(),
        b"gh: Pull Request is not mergeable (HTTP 405)".to_vec(),
        Vec::new(),
        b"Error: first line\n\\n backslash, 'sq' \"dq\" $(not run) `nor this`\nBase branch was modified".to_vec(),
        b"\xff\xfe not utf-8 \xc3: Base branch was modified".to_vec(),
        b"oops\nBEFORE\tinjected\nREWORK\tinjected".to_vec(),
    ];
    // Every route's interesting interactions are reachable within three
    // attempts from the first four (405, stale base, moved head, other); the
    // adversarial spellings only need to meet each route once or twice.
    let core: Vec<Vec<u8>> = all[..4].to_vec();
    let mut seqs: Vec<Vec<Vec<u8>>> = Vec::new();
    for a in &all {
        seqs.push(vec![a.clone()]);
        for b in &all {
            seqs.push(vec![a.clone(), b.clone()]);
        }
    }
    for a in &core {
        for b in &core {
            for c in &core {
                seqs.push(vec![a.clone(), b.clone(), c.clone()]);
            }
        }
    }
    let is_405 = |r: &[u8]| String::from_utf8_lossy(r).contains("Merge already in progress");
    let is_base = |r: &[u8]| String::from_utf8_lossy(r).contains("Base branch was modified");
    let mut out = Vec::new();
    for s in seqs {
        let len = s.len();
        let first_405 = is_405(&s[0]);
        let syncs = s.iter().any(|r| is_base(r));
        let mut push = |merged_reads: Vec<usize>, update_fails: bool| {
            out.push(Scenario {
                responses: s.clone(),
                merged_reads,
                update_fails,
            });
        };
        push(vec![], false);
        // forge_get_pr_nocache call 1 is the first failure's merged-despite-
        // error re-read; call 2 is a leading 405's post-wait re-read.
        if len == 1 {
            push(vec![1], false);
        }
        if first_405 {
            push(vec![2], false);
            push(vec![3], false);
        }
        if syncs && len <= 2 {
            push(vec![], true);
        }
    }
    out
}

/// Run every scenario through `f` on a few threads; each scenario gets its
/// own directories, so the runs are independent.
fn par_each(f: impl Fn(usize, &Scenario) -> u8 + Sync) -> Vec<u8> {
    let corpus = corpus();
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(8));
    let chunk = corpus.len().div_ceil(threads);
    std::thread::scope(|scope| {
        let handles: Vec<_> = corpus
            .chunks(chunk)
            .enumerate()
            .map(|(c, part)| {
                let f = &f;
                scope.spawn(move || {
                    part.iter()
                        .enumerate()
                        .map(|(j, sc)| f(c * chunk + j, sc))
                        .collect::<Vec<u8>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().expect("scenario thread panicked"))
            .collect()
    })
}

const SYNC: u8 = 1;
const AWAIT: u8 = 2;
const FAIL: u8 = 4;
const HEAD: u8 = 8;

#[test]
fn live_loop_and_retired_loop_agree_byte_for_byte() {
    let tmp = tempfile::tempdir().unwrap();
    let seen = par_each(|i, sc| {
        let (rc_old, t_old) = run_side("retired", sc, false, tmp.path(), &format!("{i}-old"));
        let (rc_new, t_new) = run_side("live", sc, false, tmp.path(), &format!("{i}-new"));
        assert_eq!(
            (rc_old, String::from_utf8_lossy(&t_old)),
            (rc_new, String::from_utf8_lossy(&t_new)),
            "scenario {i} diverged (responses {:?}, merged reads {:?}, update fails {})",
            sc.responses
                .iter()
                .map(|r| String::from_utf8_lossy(r).into_owned())
                .collect::<Vec<_>>(),
            sc.merged_reads,
            sc.update_fails
        );
        assert_eq!(t_old, t_new, "scenario {i}: traces differ in raw bytes");
        let t = String::from_utf8_lossy(&t_new);
        let mut m = 0;
        if t.contains("UPDATE") {
            m |= SYNC;
        }
        if t.contains("HTTP 405), waiting") {
            m |= AWAIT;
        }
        if rc_new == 1 {
            m |= FAIL;
        }
        if rc_new == 3 {
            m |= HEAD;
        }
        m
    });
    let n = |bit: u8| seen.iter().filter(|m| *m & bit != 0).count();
    let (syncs, awaits, fails, heads) = (n(SYNC), n(AWAIT), n(FAIL), n(HEAD));
    assert!(
        syncs > 40 && awaits > 40 && fails > 60 && heads > 30,
        "coverage floor: syncs={syncs} awaits={awaits} fails={fails} heads={heads} of {}",
        seen.len()
    );
}

#[test]
fn without_merge_route_the_live_loop_keeps_the_retired_control_flow() {
    let tmp = tempfile::tempdir().unwrap();
    let seen = par_each(|i, sc| {
        let (rc_old, t_old) = run_side("retired", sc, false, tmp.path(), &format!("{i}-old"));
        let (rc_new, t_new) = run_side("live", sc, true, tmp.path(), &format!("{i}-new"));
        assert_eq!(
            (rc_old, control_flow(&t_old)),
            (rc_new, control_flow(&t_new)),
            "degraded scenario {i} diverged in control flow"
        );
        // Every refusal in this mode went through the unanswered verb, so the
        // operator must have been told why the wording is generic.
        assert!(
            rc_new != 1
                || String::from_utf8_lossy(&t_new).contains("'merge-pr merge-route' gave no route"),
            "degraded scenario {i}: a refusal must be preceded by the no-route warning"
        );
        1
    });
    assert!(seen.len() > 150, "corpus shrank: {}", seen.len());
}
