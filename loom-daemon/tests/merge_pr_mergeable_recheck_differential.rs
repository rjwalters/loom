//! Differential test: the Rust port of `merge-pr.sh`'s stale-cached-mergeable
//! recheck decision must agree with the shell, byte for byte, on a shared
//! corpus (`defaults/docs/verification-recipes.md` §6 — the #8191 slice that
//! moved `_recheck_mergeable_before_refusal`'s terminal classification into
//! `loom_daemon::merge_pr::mergeable_recheck`).
//!
//! # The corpus is generated once and fed to both sides
//!
//! The shell side runs the REAL frozen function, sourced from
//! `tests/fixtures/merge-pr-mergeable-recheck-retired.sh` — a byte-for-byte
//! copy of `_recheck_mergeable_before_refusal` as it stood immediately before
//! the port — against real git fixtures and a canned `.mergeable` sequence,
//! exactly the harness shape the retained suite uses. The Rust side derives
//! the same observations and calls [`loom_daemon::merge_pr::mergeable_recheck::decide`].
//! The harness cannot lie about which side moved.
//!
//! # What it proves
//!
//! That the port did not change WHICH `<action>:<reason>` line the caller
//! branches on, nor the reason text the #6978 telemetry derives
//! `retries_used` from (`recheck #N`). A diverged `refuse-stale` /
//! `refuse-conflict` split here is an operator sent to rebase a branch that
//! merges cleanly (or merged into a real conflict) — the exact #6104 / #5995
//! incident class this decision exists to prevent.
//!
//! # Coverage floor
//!
//! §6 also demands the corpus's discriminating power, not just its size: the
//! test fails unless every one of the five distinct reason templates was
//! reached at least once (early-resolve, refs-unavailable, fetch-failed,
//! tree-clean, tree-conflict) AND the `recheck #N` interpolation was seen
//! with at least two different `N` values.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use loom_daemon::merge_pr::mergeable_recheck::{decide, Evidence, TreeOutcome};

fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/merge-pr-mergeable-recheck-retired.sh")
}

/// One shared git scenario, built once per run and reused by every case:
/// a bare `origin` plus a work clone with a clean branch (disjoint file) and
/// a conflicting branch (same line as a post-branch base edit) — the same
/// shape the retained suite builds. `no_origin` is a repo with no remote at
/// all, for the fetch-failure cases.
struct Fixtures {
    root: PathBuf,
    work: PathBuf,
    no_origin: PathBuf,
}

fn git(dir: &Path, args: &[&str]) {
    let rc = Command::new("git")
        .args(["-C", dir.to_str().unwrap()])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git ran");
    assert!(rc.success(), "git {args:?} in {dir:?} failed");
}

fn build_fixtures() -> Fixtures {
    let root =
        std::env::temp_dir().join(format!("loom-mergeable-recheck-diff-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();

    let origin = root.join("origin.git");
    git(&root, &["init", "-q", "--bare", origin.to_str().unwrap()]);

    let work = root.join("work");
    git(&root, &["init", "-q", work.to_str().unwrap()]);
    git(&work, &["config", "user.email", "test@example.com"]);
    git(&work, &["config", "user.name", "Test"]);
    git(&work, &["remote", "add", "origin", origin.to_str().unwrap()]);

    fs::write(work.join("README.md"), "hello\n").unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "initial"]);
    git(&work, &["branch", "-M", "main"]);
    git(&work, &["push", "-q", "-u", "origin", "main"]);

    // clean-merge branch: touches an unrelated file.
    git(&work, &["checkout", "-q", "-b", "feature/clean"]);
    fs::write(work.join("new-file.txt"), "unrelated addition\n").unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "clean addition"]);
    git(&work, &["push", "-q", "-u", "origin", "feature/clean"]);
    git(&work, &["checkout", "-q", "main"]);

    // conflicting branch: edits the SAME line main also edits afterwards.
    git(&work, &["checkout", "-q", "-b", "feature/conflict"]);
    fs::write(work.join("README.md"), "feature version\n").unwrap();
    git(&work, &["commit", "-q", "-am", "feature edits README"]);
    git(&work, &["push", "-q", "-u", "origin", "feature/conflict"]);
    git(&work, &["checkout", "-q", "main"]);
    fs::write(work.join("README.md"), "main version\n").unwrap();
    git(&work, &["commit", "-q", "-am", "main edits README (diverges)"]);
    git(&work, &["push", "-q", "origin", "main"]);

    // A repo with no remote: every `git fetch origin ...` fails.
    let no_origin = root.join("no-origin");
    git(&root, &["init", "-q", no_origin.to_str().unwrap()]);

    Fixtures {
        root,
        work,
        no_origin,
    }
}

impl Drop for Fixtures {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Drive the frozen shell function: canned `.mergeable` sequence (one value
/// per recheck, `false` once exhausted — the stub's own tail behaviour),
/// fresh counter file, delay 0.
fn run_frozen_shell(
    fx: &Fixtures,
    sequence: &[&str],
    base: &str,
    head: &str,
    repo_root: &Path,
    retries: u32,
) -> String {
    let seq_file = fx.root.join("seq.txt");
    fs::write(&seq_file, sequence.join("\n") + "\n").unwrap();
    let counter = fx.root.join("counter.txt");
    fs::write(&counter, "0\n").unwrap();

    // A plain string, NOT format!: the stub's JSON quoting must survive to
    // bash verbatim (double quotes so `$val` expands, inner quotes escaped),
    // and format!'s brace-doubling makes that far too easy to get wrong.
    let driver = String::from(
        r#"set -euo pipefail
SEQ_FILE="$1"; COUNTER="$2"; BASE="$3"; HEAD="$4"; ROOT="$5"; RETRIES="$6"
forge_get_pr_nocache() {
    local idx val total
    idx="$(cat "$COUNTER")"; echo $((idx + 1)) > "$COUNTER"
    total="$(grep -c . "$SEQ_FILE" || true)"
    val="false"
    if [[ "$idx" -lt "$total" ]]; then val="$(sed -n "$((idx + 1))p" "$SEQ_FILE")"; fi
    if [[ "$val" == "null" ]]; then echo '{"mergeable":null}'; else echo "{\"mergeable\":$val}"; fi
}
source "$7"
_recheck_mergeable_before_refusal "owner/repo" 1 "gh" "$BASE" "$HEAD" "$ROOT" "$RETRIES" 0
"#,
    );
    let driver_path = fx.root.join("driver.sh");
    fs::write(&driver_path, driver).unwrap();

    let out = Command::new("bash")
        .arg(&driver_path)
        .args([
            seq_file.to_str().unwrap(),
            counter.to_str().unwrap(),
            base,
            head,
            repo_root.to_str().unwrap(),
            &retries.to_string(),
            fixture_path().to_str().unwrap(),
        ])
        .output()
        .expect("bash ran the frozen shell side");
    assert!(
        out.status.success(),
        "frozen shell side failed: {}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Derive the same observations the shell loop made and classify them with
/// the Rust kernel.
fn run_rust(sequence: &[&str], base: &str, head: &str, repo_root: &Path, retries: u32) -> String {
    // The loop: re-read up to `retries` times; resolve on the first `true`.
    let mut resolved_attempt = None;
    for (idx, val) in sequence.iter().enumerate() {
        if (idx as u32) >= retries {
            break;
        }
        if *val == "true" {
            resolved_attempt = Some(idx as u32 + 1);
            break;
        }
    }
    // The stub answers `false` once the sequence is exhausted, so an
    // early-terminating shorter sequence behaves like one padded with falses.
    let refs_available = !base.is_empty() && !head.is_empty();
    let (fetch_ok, tree) = if resolved_attempt.is_some() || !refs_available {
        (false, TreeOutcome::NotRun)
    } else {
        let fetch = Command::new("git")
            .args([
                "-C",
                repo_root.to_str().unwrap(),
                "fetch",
                "-q",
                "origin",
                base,
                head,
            ])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !fetch {
            (false, TreeOutcome::NotRun)
        } else {
            let clean = Command::new("git")
                .args([
                    "-C",
                    repo_root.to_str().unwrap(),
                    "merge-tree",
                    "--write-tree",
                    &format!("origin/{base}"),
                    &format!("origin/{head}"),
                ])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            (
                true,
                if clean {
                    TreeOutcome::Clean
                } else {
                    TreeOutcome::Conflict
                },
            )
        }
    };
    decide(&Evidence {
        resolved_attempt,
        retries,
        refs_available,
        fetch_ok,
        tree,
        base_ref: base.to_string(),
        head_ref: head.to_string(),
    })
}

#[test]
fn frozen_shell_and_rust_agree_byte_for_byte() {
    let fx = build_fixtures();

    // (sequence, base, head, repo, retries) — every branch of the decision,
    // the boundary retry counts, both refs-missing variants, the null
    // (still-computing) tail, and the sequence-exhausted stub tail.
    let cases: Vec<(Vec<&str>, &str, &str, PathBuf, u32)> = vec![
        // early resolve at attempt 1/2/3 of 3
        (vec!["true"], "main", "feature/clean", fx.work.clone(), 3),
        (vec!["false", "true"], "main", "feature/clean", fx.work.clone(), 3),
        (vec!["false", "false", "true"], "main", "feature/clean", fx.work.clone(), 3),
        // never resolves; clean tree corroborates, across retry counts
        (vec!["false", "false"], "main", "feature/clean", fx.work.clone(), 2),
        (vec!["false"], "main", "feature/clean", fx.work.clone(), 1),
        (vec!["false", "false", "false"], "main", "feature/clean", fx.work.clone(), 3),
        // never resolves; real conflict confirmed, across retry counts
        (vec!["false", "false"], "main", "feature/conflict", fx.work.clone(), 2),
        (vec!["false"], "main", "feature/conflict", fx.work.clone(), 1),
        (vec!["false", "false", "false"], "main", "feature/conflict", fx.work.clone(), 3),
        // mergeable=null (still computing) never resolves — corroborates
        (vec!["null", "null"], "main", "feature/clean", fx.work.clone(), 2),
        (vec!["null", "null"], "main", "feature/conflict", fx.work.clone(), 2),
        // sequence shorter than the retry budget: stub tail answers false
        (vec![], "main", "feature/clean", fx.work.clone(), 3),
        // refs unavailable (both variants)
        (vec!["false"], "", "feature/clean", fx.work.clone(), 1),
        (vec!["false", "false"], "main", "", fx.work.clone(), 2),
        // fetch failure (no such remote)
        (vec!["false"], "main", "feature/clean", fx.no_origin.clone(), 1),
        (vec!["false", "false"], "main", "feature/clean", fx.no_origin.clone(), 2),
        // a named ref the remote does not have also fails the fetch
        (vec!["false"], "main", "feature/nonexistent", fx.work.clone(), 1),
    ];

    let mut reached: Vec<&str> = Vec::new();
    let mut resolved_ns: Vec<u32> = Vec::new();
    for (sequence, base, head, repo, retries) in &cases {
        let shell_out = run_frozen_shell(&fx, sequence, base, head, repo, *retries);
        let rust_out = run_rust(sequence, base, head, repo, *retries);
        assert_eq!(
            shell_out, rust_out,
            "divergence on case sequence={sequence:?} base={base:?} head={head:?} repo={} retries={retries}",
            repo.display(),
        );
        for marker in [
            "recheck #",
            "ref unavailable",
            "could not fetch",
            "is clean — proceeding",
            "genuinely conflicts",
        ] {
            if rust_out.contains(marker) {
                reached.push(marker);
            }
        }
        if let Some(n) = rust_out
            .split("recheck #")
            .nth(1)
            .and_then(|rest| rest.chars().next())
        {
            if n.is_ascii_digit() {
                resolved_ns.push(n.to_digit(10).unwrap());
            }
        }
    }

    // Coverage floor (§6: discriminating power, not just size).
    for marker in [
        "recheck #",
        "ref unavailable",
        "could not fetch",
        "is clean — proceeding",
        "genuinely conflicts",
    ] {
        assert!(
            reached.contains(&marker),
            "corpus never reached the {marker:?} reason template — it does not exercise this branch"
        );
    }
    resolved_ns.sort();
    resolved_ns.dedup();
    assert!(
        resolved_ns.len() >= 2,
        "corpus exercised `recheck #N` with fewer than two distinct N values: {resolved_ns:?}"
    );
}
