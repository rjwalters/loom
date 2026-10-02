//! Differential test: `merge-pr.sh`'s `_revalidate_merge_guards` (`--auto`'s
//! post-wait re-validation, #8410/#8896) as it stands NOW — a thin seam over
//! `loom-daemon merge-pr revalidate` — against the frozen function it replaced
//! (`defaults/docs/verification-recipes.md` §6; the #8191 slice that moved its
//! reads into `loom_daemon::merge_pr::revalidate`).
//!
//! # Both sides are the real thing, driven the same way
//!
//! The retired side sources `tests/fixtures/merge-pr-revalidate-retired.sh`;
//! the live side extracts `_revalidate_merge_guards` from
//! `defaults/scripts/merge-pr.sh` itself and runs it with `LOOM_DAEMON_BIN`
//! pinned to the binary under test. Both run under the script's own
//! `set -euo pipefail` against the SAME recording stubs for the five names the
//! function calls, so what is compared is the observable outcome of the whole
//! function — exit code, which stub fired, the label set the guards would see —
//! not a paraphrase of either half. That also puts the shell seam's own
//! parsing (the `case` over exit code and sentinel, the `$nl` split) under
//! test, which a port-only differential could not see.
//!
//! # Every difference must belong to a class recognised by MECHANISM
//!
//! - **equal**: the common case, and the only one allowed for a payload inside
//!   the contract [`CONTRACT_JQ`] states independently of the port.
//! - **`retired_faulted`**: the retired function died of a `jq` error under
//!   `set -e` (exit 5 — the "re-queue, not a failure" code — with no message).
//!   The live side must refuse with #8896's message instead.
//! - **`port_stricter`**: the retired function answered, but the payload is
//!   outside the contract (a half-walkable label array, an absent `labels`
//!   key, a non-hex SHA, a non-boolean `merged`). The live side must refuse
//!   with #8896's message — never answer differently in a permissive direction.
//!
//! Each class must be reached, and every verdict arm must be reached among the
//! equal cases, so a corpus that quietly stopped exercising one half fails
//! here rather than going green.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const PRE: &str = "490b79f1d";
const PR: &str = "8220";

fn manifest() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn fixture_path() -> PathBuf {
    manifest().join("tests/fixtures/merge-pr-revalidate-retired.sh")
}

fn merge_pr_path() -> PathBuf {
    manifest().join("../defaults/scripts/merge-pr.sh")
}

/// The forge contract the port answers inside, stated independently of it.
/// Mirrors the retired function's lazy read order: `merged` first, then the
/// head, then — only when the head did not move — the labels.
const CONTRACT_JQ: &str = r#"
# \A/\z, not ^/$: Oniguruma's `$` also matches before a trailing newline.
def hexsha: type == "string" and test("\\A[0-9a-fA-F]+\\z");
def labels_ok: has("labels")
  and (.labels | type == "array" or type == "null")
  and all(.labels[]?; type == "object"
        and (.name | type == "null" or (type == "string" and (explode | any(. == 0) | not))));
type == "object"
and (.merged | type == "null" or type == "boolean")
and (.merged == true
  or ((.head | type == "null")
      or ((.head | type == "object") and (.head.sha | type == "null" or . == "" or hexsha)))
    and ((.head.sha? // "") as $s
         | $s == "" or ($pre != "" and $s != $pre) or labels_ok))
"#;

/// The five names the function calls, as recording stubs. `PAYLOAD` and
/// `FETCH_RC` model the forge read, including the `|| echo '{}'` fallback a
/// failed read falls into.
const STUBS: &str = r#"
REPO_NWO="rjwalters/loom"; GH="gh"
forge_get_pr_nocache() { printf '%s' "$PAYLOAD"; return "$FETCH_RC"; }
error() { printf 'ERROR\t%s' "$*"; exit 1; }
error_head_moved() { printf 'HEAD-MOVED\t%s\t%s\t%s' "$1" "$2" "$3"; exit 3; }
_check_loom_pr_label() { printf 'GUARDS\t%s\t' "$PR_LABELS"; }
_check_verdict_label_contradiction() { printf 'CONTRA'; }
"#;

/// Runs every case through one bash process: `<source-the-function>` then,
/// per case, a subshell under `set -euo pipefail` — the script's own options.
fn run_side(load: &'static str, cases: &[Case]) -> Vec<(i32, String)> {
    // Chunked across threads: each chunk is its own bash process, and the
    // per-case cost is process spawns (three `jq`s retired, one daemon live).
    let chunk = cases.len().div_ceil(8).max(1);
    let handles: Vec<_> = cases
        .chunks(chunk)
        .map(|c| {
            let c = c.to_vec();
            std::thread::spawn(move || run_chunk(load, &c))
        })
        .collect();
    handles
        .into_iter()
        .flat_map(|h| h.join().unwrap())
        .collect()
}

fn run_chunk(load: &str, cases: &[Case]) -> Vec<(i32, String)> {
    let driver = format!(
        r#"{STUBS}
{load}
while IFS= read -r -d '' FETCH_RC && IFS= read -r -d '' MERGE_PRECONDITION_SHA && IFS= read -r -d '' PAYLOAD; do
  PR_NUMBER="{PR}"; PR_LABELS="UNSET"
  # NOT `out="$(…)" || rc=$?`: a subshell inside an `||` list runs with
  # errexit IGNORED — even its own `set -e` — so the retired body's jq faults
  # would never fire. Production calls the function bare, so `set -e` is live
  # there; the exit code rides out after a \001 instead.
  res="$( ( set -euo pipefail; _revalidate_merge_guards ) 2>/dev/null; printf '\001%s' "$?" )"
  soh=$'\001'; rc="${{res##*"$soh"}}"; out="${{res%"$soh"*}}"
  printf '%s\0%s\0' "$rc" "$out"
done
"#
    );
    let mut child = Command::new("bash")
        .args(["-c", &driver, "driver"])
        .arg(fixture_path())
        .arg(merge_pr_path())
        .env("LC_ALL", "C")
        .env("LOOM_DAEMON_BIN", env!("CARGO_BIN_EXE_loom-daemon"))
        .env_remove("LOOM_DAEMON_SELF_BIN")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("bash spawned");
    let mut input = Vec::new();
    for c in cases {
        input.extend_from_slice(c.fetch_rc.as_bytes());
        input.push(0);
        input.extend_from_slice(c.pre.as_bytes());
        input.push(0);
        input.extend_from_slice(c.payload.as_bytes());
        input.push(0);
    }
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let out = child.wait_with_output().unwrap();
    writer.join().unwrap().unwrap();
    assert!(out.status.success(), "driver failed: {}", String::from_utf8_lossy(&out.stderr));
    let mut parts: Vec<String> = out
        .stdout
        .split(|b| *b == 0)
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect();
    assert_eq!(parts.pop().as_deref(), Some(""), "frame must be NUL-terminated");
    assert_eq!(parts.len(), cases.len() * 2, "one (rc, out) pair per case");
    parts
        .chunks(2)
        .map(|p| (p[0].parse().expect("numeric rc"), p[1].clone()))
        .collect()
}

fn retired_load() -> &'static str {
    r#"source "$1""#
}

fn live_load() -> &'static str {
    r#"eval "$(awk '/^_revalidate_merge_guards\(\) \{/{f=1} f; f && /^}/{exit}' "$2")""#
}

#[derive(Clone)]
struct Case {
    fetch_rc: String,
    pre: String,
    payload: String,
}

fn corpus() -> Vec<Case> {
    let merged = [
        "",
        r#""merged":null,"#,
        r#""merged":false,"#,
        r#""merged":true,"#,
        r#""merged":"true","#,
        r#""merged":1,"#,
    ];
    let head = [
        "".to_string(),
        r#""head":null,"#.to_string(),
        r#""head":{},"#.to_string(),
        r#""head":{"sha":null},"#.to_string(),
        r#""head":{"sha":""},"#.to_string(),
        format!(r#""head":{{"sha":"{PRE}"}},"#),
        r#""head":{"sha":"ab58dd87d"},"#.to_string(),
        format!(r#""head":{{"sha":"{}"}},"#, PRE.to_uppercase()),
        r#""head":{"sha":123},"#.to_string(),
        format!(r#""head":{{"sha":"{PRE}\n"}},"#),
        r#""head":"x","#.to_string(),
    ];
    let labels = [
        "",
        r#""labels":null,"#,
        r#""labels":[],"#,
        r#""labels":[{"name":"loom:pr"}],"#,
        r#""labels":[{"name":"loom:pr"},{"name":"loom:changes-requested"}],"#,
        r#""labels":[{"name":null},{"color":"f00"},{"name":"loom:pr"}],"#,
        r#""labels":[{"name":""},{"name":"a"}],"#,
        r#""labels":[{"name":"a"},{"name":""}],"#,
        r#""labels":[{"name":"a\nb"},{"name":"c\n"}],"#,
        r#""labels":[{"name":"loom:pr"},"s",{"name":"loom:changes-requested"}],"#,
        r#""labels":"loom:pr","#,
        r#""labels":[{"name":1}],"#,
        r#""labels":[{"name":"é ✓ x"}],"#,
        r#""labels":[{"name":"a\u0000b"}],"#,
        r#""labels":[{"name":"z"},{"name":"a"},{"name":"z"}],"#,
    ];
    let mut out = Vec::new();
    for pre in ["", PRE] {
        for (mi, m) in merged.iter().enumerate() {
            for h in &head {
                for (li, l) in labels.iter().enumerate() {
                    // Full cross for the two "not merged" shapes that reach the
                    // label read; the other four stop at `merged` (or refuse
                    // there), so three label shapes — absent, healthy, and
                    // half-walkable — are enough to show the order holds.
                    if mi != 0 && mi != 2 && ![0, 3, 9].contains(&li) {
                        continue;
                    }
                    let body = format!("{m}{h}{l}");
                    let payload = format!("{{{}\"number\":{PR}}}", body);
                    out.push(Case {
                        fetch_rc: "0".into(),
                        pre: pre.into(),
                        payload,
                    });
                }
            }
        }
        // Raw shapes: what an unhealthy forge read actually hands back.
        let pr_ok = format!(
            r#"{{"head":{{"sha":"{PRE}"}},"merged":false,"labels":[{{"name":"loom:pr"}}]}}"#
        );
        let raws = [
            String::new(),
            "garbage".into(),
            "<html><body>502 Bad Gateway</body></html>".into(),
            "null".into(),
            "[]".into(),
            format!("[{pr_ok}]"),
            r#"{"message":"Server Error","documentation_url":"https://docs.github.com"}"#.into(),
            pr_ok[..pr_ok.len() - 5].to_string(),
            format!("{pr_ok}\n{pr_ok}"),
            format!("  \n{pr_ok}\n\n"),
            format!(r#"{{"merged":false,"merged":true,"head":{{"sha":"{PRE}"}},"labels":[]}}"#),
            format!(
                r#"{{"head":{{"sha":"ab58dd87d"}},"head":{{"sha":"{PRE}"}},"labels":[{{"name":"loom:pr"}}]}}"#
            ),
        ];
        for raw in &raws {
            for rc in ["0", "1"] {
                out.push(Case {
                    fetch_rc: rc.into(),
                    pre: pre.into(),
                    payload: raw.clone(),
                });
            }
        }
    }
    out
}

/// What the live function saw, modelled on the stdin the seam pipes to the
/// verb: the payload after `$(…)` (or after the `|| echo '{}'` fallback).
fn effective_payload(c: &Case) -> String {
    let mut s = c.payload.clone();
    if c.fetch_rc != "0" {
        s.push_str("{}");
    }
    s.trim_end_matches('\n').to_string()
}

fn inside_contract(c: &Case) -> bool {
    let raw = effective_payload(c);
    let mut docs = serde_json::Deserializer::from_str(&raw).into_iter::<serde_json::Value>();
    if !(matches!(docs.next(), Some(Ok(_))) && docs.next().is_none()) {
        return false;
    }
    let mut child = Command::new("jq")
        .args(["-e", "--arg", "pre", &c.pre, CONTRACT_JQ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("jq spawned");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(raw.as_bytes())
        .unwrap();
    child.wait().unwrap().success()
}

fn unreadable_refusal(rc: i32, out: &str) -> bool {
    rc == 1 && out.starts_with(&format!("ERROR\tMerge blocked: could not re-read PR #{PR} "))
}

#[test]
fn live_seam_matches_the_retired_function_or_refuses_by_a_named_class() {
    let cases = corpus();
    let retired = run_side(retired_load(), &cases);
    let live = run_side(live_load(), &cases);

    let (mut equal, mut retired_faulted, mut port_stricter) = (0usize, 0usize, 0usize);
    let (mut merged, mut guards, mut moved, mut unreadable) = (0usize, 0usize, 0usize, 0usize);
    for (i, c) in cases.iter().enumerate() {
        let (r_rc, r_out) = &retired[i];
        let (l_rc, l_out) = &live[i];
        let ctx = || {
            format!("case {i}: pre={:?} fetch_rc={} payload={:?}\n  retired: rc={r_rc} {r_out:?}\n  live:    rc={l_rc} {l_out:?}", c.pre, c.fetch_rc, c.payload)
        };
        if (r_rc, r_out) == (l_rc, l_out) {
            equal += 1;
            match (*l_rc, l_out.as_str()) {
                (0, "") => merged += 1,
                (0, o) if o.starts_with("GUARDS\t") => guards += 1,
                (3, o) if o.starts_with("HEAD-MOVED\t") => moved += 1,
                (1, o) if unreadable_refusal(1, o) => unreadable += 1,
                _ => panic!("equal but unclassified outcome — {}", ctx()),
            }
            continue;
        }
        assert!(
            unreadable_refusal(*l_rc, l_out),
            "the live side may only ever differ by REFUSING with #8896's message — {}",
            ctx()
        );
        if ![0, 1, 3].contains(r_rc) {
            retired_faulted += 1;
        } else {
            assert!(
                !inside_contract(c),
                "a payload INSIDE the forge contract must get the retired answer — {}",
                ctx()
            );
            port_stricter += 1;
        }
    }

    assert_eq!(equal + retired_faulted + port_stricter, cases.len());
    eprintln!(
        "revalidate differential: {} cases — equal {equal} (merged {merged}, guards {guards}, \
         head-moved {moved}, unreadable {unreadable}), retired_faulted {retired_faulted}, \
         port_stricter {port_stricter}",
        cases.len()
    );
    for (name, n) in [
        ("merged", merged),
        ("guards", guards),
        ("head-moved", moved),
        ("unreadable", unreadable),
        ("retired_faulted", retired_faulted),
        ("port_stricter", port_stricter),
    ] {
        assert!(n > 0, "the corpus no longer reaches the `{name}` outcome");
    }
}

/// The seam fails CLOSED on every way the verb can be unanswerable, through
/// the real function under the script's own `set -e` — the call shape #8191's
/// slice-1 review showed a unit test of the verb alone cannot vouch for.
#[test]
fn an_unanswerable_verb_refuses_the_merge_through_the_real_caller() {
    let dir = std::env::temp_dir().join(format!("revalidate-skew-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let old = dir.join("old-daemon");
    std::fs::write(&old, "#!/bin/sh\necho \"error: unrecognized subcommand '$2'\" >&2\nexit 2\n")
        .unwrap();
    let silent = dir.join("silent-daemon");
    std::fs::write(&silent, "#!/bin/sh\nexit 0\n").unwrap();
    let noexec = dir.join("noexec-daemon");
    std::fs::write(&noexec, "#!/bin/sh\necho 'LOOM-REVALIDATE MERGED'\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for p in [&old, &silent] {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::set_permissions(&noexec, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    let missing = dir.join("no-such-daemon");
    let payload =
        format!(r#"{{"head":{{"sha":"{PRE}"}},"merged":false,"labels":[{{"name":"loom:pr"}}]}}"#);
    for bin in [&missing, &old, &silent, &noexec] {
        let driver = format!(
            r#"{STUBS}
eval "$(awk '/^_revalidate_merge_guards\(\) \{{/{{f=1}} f; f && /^}}/{{exit}}' "$1")"
PR_NUMBER="{PR}"; PAYLOAD='{payload}'; FETCH_RC=0; MERGE_PRECONDITION_SHA="{PRE}"
set -euo pipefail
_revalidate_merge_guards
echo "REACHED-MERGE"
"#
        );
        let out = Command::new("bash")
            .args(["-c", &driver, "driver"])
            .arg(merge_pr_path())
            .env("LOOM_DAEMON_BIN", bin)
            .env_remove("LOOM_DAEMON_SELF_BIN")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert_eq!(out.status.code(), Some(1), "{bin:?}: {stdout}");
        assert!(!stdout.contains("REACHED-MERGE"), "{bin:?}: {stdout}");
        assert!(
            !stdout.contains("GUARDS"),
            "{bin:?} must not run the guards on a guess: {stdout}"
        );
        assert!(
            stdout.contains("post-wait re-validation (#8410) could not run"),
            "{bin:?}: {stdout}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
