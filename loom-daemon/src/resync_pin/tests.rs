//! Tests for `.loom/resync-pin-base` fork-point provenance (issue #8726).
//!
//! The A/B tests build disposable git repositories: a Loom *source* checkout
//! with a pinned file at revision A, a consumer workspace pinned against it,
//! then upstream advances to B. The recorded base must stay A, and the
//! re-evaluation must be a plain `git diff A B -- <path>`.

use super::*;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

fn run_git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .output()
        .expect("run git");
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn write(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

fn commit_all(dir: &Path, msg: &str) -> String {
    run_git(dir, &["add", "-A"]);
    run_git(dir, &["commit", "-qm", msg]);
    run_git(dir, &["rev-parse", "HEAD"])
}

const ROLE_SRC: &str = "defaults/.claude/commands/loom/curator.md";
const ROLE_A: &str = "line 1\nline 2\nline 3\n";
const ROLE_B: &str = "line 1\nline 2 upstream fix\nline 3\nline 4 new\n";

/// A Loom source checkout whose `defaults/roles/curator.md` is a symlink into
/// `defaults/.claude/commands/loom/` (the real layout), committed at A.
fn source_repo() -> (TempDir, String) {
    let tmp = TempDir::new().unwrap();
    let s = tmp.path();
    run_git(s, &["init", "-q"]);
    write(&s.join(ROLE_SRC), ROLE_A);
    fs::create_dir_all(s.join("defaults/roles")).unwrap();
    std::os::unix::fs::symlink(
        "../.claude/commands/loom/curator.md",
        s.join("defaults/roles/curator.md"),
    )
    .unwrap();
    let a = commit_all(s, "A");
    (tmp, a)
}

/// A consumer workspace whose installed role is A plus a local patch, with
/// install metadata recording A and `.loom/loom-source-path` -> `source`.
fn consumer(source: &Path, installed_commit: &str) -> TempDir {
    let tmp = TempDir::new().unwrap();
    let w = tmp.path();
    write(&w.join(".loom/roles/curator.md"), &format!("{ROLE_A}local patch\n"));
    write(&w.join(".loom/loom-source-path"), &format!("{}\n", source.display()));
    write(
        &w.join(".loom/install-metadata.json"),
        &format!(r#"{{"loom_commit": "{installed_commit}", "installed_files": []}}"#),
    );
    tmp
}

fn opts(path: &str) -> AddOptions {
    AddOptions {
        path: path.into(),
        ..Default::default()
    }
}

fn recorded(r: &AddReport) -> &BaseEntry {
    match &r.base {
        BaseOutcome::Recorded(e) => e,
        other => panic!("expected Recorded, got {other:?}"),
    }
}

// ---- labels and source mapping --------------------------------------------

#[test]
fn canonical_label_matches_resync_rel_forms() {
    assert_eq!(canonical_label("roles/curator.md"), "roles/curator.md");
    assert_eq!(canonical_label(".loom/roles/curator.md"), "roles/curator.md");
    assert_eq!(canonical_label("./.loom/hooks/x.sh"), "hooks/x.sh");
    assert_eq!(canonical_label("  commands/loom/builder.md "), "commands/loom/builder.md");
    // Top-level `.loom/` files keep their prefix, exactly as `is_ignored()`
    // refuses to collapse them onto a bare top-level name.
    assert_eq!(canonical_label(".loom/CLAUDE.md"), ".loom/CLAUDE.md");
    assert_eq!(canonical_label("./.loom/pricing.json"), ".loom/pricing.json");
}

#[test]
fn source_and_installed_paths_follow_resync_surface_map() {
    let cases = [
        ("roles/curator.md", "defaults/roles/curator.md", ".loom/roles/curator.md"),
        ("bin/loom", "defaults/.loom/bin/loom", ".loom/bin/loom"),
        (
            "commands/loom/x.md",
            "defaults/.claude/commands/loom/x.md",
            ".claude/commands/loom/x.md",
        ),
        (
            "agents-skills/a/SKILL.md",
            "defaults/.agents/skills/a/SKILL.md",
            ".agents/skills/a/SKILL.md",
        ),
        (".loom/pricing.json", "defaults/pricing.json", ".loom/pricing.json"),
        (".claude/README.md", "defaults/.claude/README.md", ".claude/README.md"),
    ];
    for (label, src, inst) in cases {
        assert_eq!(default_source_path(label).as_deref(), Some(src), "{label}");
        assert_eq!(installed_path(label), inst, "{label}");
    }
    assert_eq!(default_source_path(".loom/CLAUDE.md"), None);
    assert_eq!(default_source_path("project/helper.sh"), None);
}

/// Drift guard: every mapped `defaults/` directory exists in this source
/// checkout, and resync-installed.sh still syncs each one under the label
/// prefix the map assumes.
#[test]
fn surface_map_matches_this_source_checkout() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let Ok(script) = fs::read_to_string(root.join("defaults/scripts/resync-installed.sh")) else {
        return; // not a source checkout
    };
    for (label, src, _) in PREFIX_MAP {
        assert!(root.join(src).is_dir(), "{src} missing for label prefix {label}");
        let src_dir = src.trim_start_matches("defaults/").trim_end_matches('/');
        assert!(
            script.contains(&format!("$DEFAULTS_DIR/{src_dir}")),
            "resync-installed.sh no longer syncs $DEFAULTS_DIR/{src_dir}"
        );
    }
    for (_, src) in EXACT_MAP {
        assert!(root.join(src).is_file(), "{src} missing");
    }
}

// ---- sidecar format ------------------------------------------------------

#[test]
fn sidecar_round_trips_and_preserves_unknown_lines() {
    let sha = "a".repeat(40);
    let text =
        format!("# header\n\nroles/x.md\t{sha}\tdefaults/roles/x.md\texplicit\nnot a valid line\n");
    let sc = Sidecar::parse(&text);
    assert_eq!(sc.render(), text, "rewrite must be byte-identical");
    assert_eq!(sc.get("roles/x.md").unwrap().sha, sha);
    assert_eq!(sc.get(".loom/roles/x.md"), None, "lookups use canonical labels");
    assert_eq!(sc.malformed(), vec!["not a valid line"]);
}

#[test]
fn sidecar_rejects_abbreviated_shas() {
    let sc = Sidecar::parse("roles/x.md\tabc1234\tdefaults/roles/x.md\texplicit\n");
    assert_eq!(sc.entries().count(), 0);
    assert_eq!(sc.malformed().len(), 1);
}

#[test]
fn invalid_pin_paths_are_rejected_before_anything_is_written() {
    let tmp = TempDir::new().unwrap();
    for bad in ["", "/etc/passwd", "roles/../x.md", "a b.md", "x.md#c"] {
        assert!(add_pin(tmp.path(), &opts(bad)).is_err(), "{bad:?}");
    }
    assert!(!tmp.path().join(IGNORE_FILE).exists());
}

// ---- A/B provenance ------------------------------------------------------

#[test]
fn pin_records_base_a_and_it_survives_upstream_moving_to_b() {
    let (src, a) = source_repo();
    let ws = consumer(src.path(), &a);
    let w = ws.path();
    write(&w.join(IGNORE_FILE), "# repo pins\nhooks/post-worktree.sh  # local\n");

    let r = add_pin(w, &opts(".loom/roles/curator.md")).unwrap();
    assert!(r.pinned_now);
    let e = recorded(&r);
    assert_eq!(e.sha, a);
    assert_eq!(e.via, "install-metadata");
    assert_eq!(e.source_path, ROLE_SRC, "symlinked source resolved to its target");
    assert_eq!(
        r.content_note.as_deref(),
        Some("installed file differs from the base (local patch)")
    );
    // Appended in canonical form; existing comments untouched.
    assert_eq!(
        fs::read_to_string(w.join(IGNORE_FILE)).unwrap(),
        "# repo pins\nhooks/post-worktree.sh  # local\nroles/curator.md\n"
    );

    // Upstream moves to B; a resync re-stamps install metadata to B.
    write(&src.path().join(ROLE_SRC), ROLE_B);
    let b = commit_all(src.path(), "B");
    write(
        &w.join(".loom/install-metadata.json"),
        &format!(r#"{{"loom_commit": "{b}", "installed_files": []}}"#),
    );
    let before = fs::read(w.join(BASE_FILE)).unwrap();

    // Re-running the pin operation keeps the recorded base.
    let again = add_pin(w, &opts("roles/curator.md")).unwrap();
    assert!(!again.pinned_now);
    assert!(matches!(&again.base, BaseOutcome::Kept(k) if k.sha == a));
    assert_eq!(fs::read(w.join(BASE_FILE)).unwrap(), before);

    // Re-evaluation is a diff from the fork point, not a bisect-by-cmp.
    let report = status(w, None, "HEAD");
    let pin = report
        .pins
        .iter()
        .find(|p| p.label == "roles/curator.md")
        .unwrap();
    match &pin.drift {
        Drift::Measured {
            commits,
            added,
            removed,
            upstream_path,
            diff_cmd,
        } => {
            assert_eq!((*commits, *added, *removed), (1, 2, 1));
            assert_eq!(upstream_path.as_deref(), Some(ROLE_SRC));
            assert!(diff_cmd.ends_with(&format!("diff {a} {b} -- {ROLE_SRC}")), "{diff_cmd}");
        }
        other => panic!("expected measured drift, got {other:?}"),
    }
    let diff = run_git(src.path(), &["diff", &a, &b, "--", ROLE_SRC]);
    assert!(diff.contains("+line 2 upstream fix") && diff.contains("+line 4 new"), "{diff}");

    // The hand-written legacy pin keeps matching and reports unknown, never zero.
    let legacy = report
        .pins
        .iter()
        .find(|p| p.label == "hooks/post-worktree.sh")
        .unwrap();
    assert!(legacy.base.is_none());
    assert!(matches!(&legacy.drift, Drift::Unknown(w) if w.contains("legacy pin")));
    assert!(report
        .render()
        .contains("drift: 1 upstream commit(s), +2/-1 lines"));
}

#[test]
fn explicit_base_and_replace_base() {
    let (src, a) = source_repo();
    write(&src.path().join(ROLE_SRC), ROLE_B);
    let b = commit_all(src.path(), "B");
    let ws = consumer(src.path(), &b);
    let w = ws.path();

    let r = add_pin(
        w,
        &AddOptions {
            base: Some(a[..10].to_string()),
            ..opts("roles/curator.md")
        },
    )
    .unwrap();
    assert_eq!(recorded(&r).sha, a, "abbreviated rev expanded to the full sha");
    assert_eq!(recorded(&r).via, "explicit");

    // Without --replace-base an explicit --base still does not overwrite.
    let kept = add_pin(
        w,
        &AddOptions {
            base: Some(b.clone()),
            ..opts("roles/curator.md")
        },
    )
    .unwrap();
    assert!(matches!(kept.base, BaseOutcome::Kept(ref k) if k.sha == a));

    let replaced = add_pin(
        w,
        &AddOptions {
            base: Some(b.clone()),
            replace_base: true,
            ..opts("roles/curator.md")
        },
    )
    .unwrap();
    assert_eq!(recorded(&replaced).sha, b);
    assert_eq!(Sidecar::load(w).entries().count(), 1);

    // Base == upstream is a *measured* zero, backed by a resolved commit.
    let st = status(w, None, "HEAD");
    assert!(matches!(
        st.pins[0].drift,
        Drift::Measured {
            commits: 0,
            added: 0,
            removed: 0,
            ..
        }
    ));
}

#[test]
fn legacy_pin_without_explicit_base_is_not_backfilled_from_install_metadata() {
    let (src, a) = source_repo();
    let ws = consumer(src.path(), &a);
    let w = ws.path();
    write(&w.join(IGNORE_FILE), ".loom/roles/curator.md\n");

    let r = add_pin(w, &opts("roles/curator.md")).unwrap();
    assert!(!r.pinned_now);
    assert!(matches!(&r.base, BaseOutcome::Unknown(m) if m.contains("already pinned")));
    assert!(!w.join(BASE_FILE).exists(), "no fabricated base");
    assert_eq!(fs::read_to_string(w.join(IGNORE_FILE)).unwrap(), ".loom/roles/curator.md\n");

    // Supplying the fork point explicitly backfills it.
    let r = add_pin(
        w,
        &AddOptions {
            base: Some(a.clone()),
            ..opts("roles/curator.md")
        },
    )
    .unwrap();
    assert_eq!(recorded(&r).sha, a);
}

#[test]
fn unresolvable_revision_keeps_the_pin_but_records_nothing() {
    let (src, _a) = source_repo();
    let ws = consumer(src.path(), "unknown");
    let w = ws.path();

    let r = add_pin(w, &opts("roles/curator.md")).unwrap();
    assert!(r.pinned_now, "pin protection written even though the base is unknown");
    assert!(matches!(&r.base, BaseOutcome::Unknown(m) if m.contains("does not resolve")));
    assert!(!w.join(BASE_FILE).exists());

    let bogus = "f".repeat(40);
    let r = add_pin(
        w,
        &AddOptions {
            base: Some(bogus),
            ..opts("roles/curator.md")
        },
    )
    .unwrap();
    assert!(matches!(r.base, BaseOutcome::Unknown(_)));
    assert!(!w.join(BASE_FILE).exists());
}

#[test]
fn missing_source_checkout_is_unknown_never_zero() {
    let (src, a) = source_repo();
    let ws = consumer(src.path(), &a);
    let w = ws.path();
    fs::remove_file(w.join(".loom/loom-source-path")).unwrap();

    let r = add_pin(w, &opts("roles/curator.md")).unwrap();
    assert!(r.pinned_now);
    assert!(matches!(&r.base, BaseOutcome::Unknown(m) if m.contains("--source")));

    // A base recorded earlier whose commit the available checkout lacks.
    let sha = "e".repeat(40);
    write(&w.join(BASE_FILE), &format!("roles/curator.md\t{sha}\t{ROLE_SRC}\texplicit\n"));
    let st = status(w, None, "HEAD");
    assert!(matches!(&st.pins[0].drift, Drift::Unknown(m) if m.contains("--source")));
    let st = status(w, Some(src.path()), "HEAD");
    assert!(matches!(&st.pins[0].drift, Drift::Unknown(m) if m.contains("not in source checkout")));
    assert!(!st.render().contains("drift: 0"));
}

#[test]
fn unmapped_path_needs_explicit_source_path_and_reports_orphans() {
    let (src, a) = source_repo();
    let ws = consumer(src.path(), &a);
    let w = ws.path();

    let r = add_pin(w, &opts("project/helper.sh")).unwrap();
    assert!(matches!(&r.base, BaseOutcome::Unknown(m) if m.contains("--source-path")));

    let r = add_pin(
        w,
        &AddOptions {
            source_path: Some(ROLE_SRC.into()),
            ..opts(".loom/CLAUDE.md")
        },
    )
    .unwrap();
    assert_eq!(recorded(&r).label, ".loom/CLAUDE.md");

    // Dropping the pin by hand leaves an orphaned base entry, which is reported.
    write(&w.join(IGNORE_FILE), "project/helper.sh\n");
    let st = status(w, None, "HEAD");
    assert_eq!(st.orphaned.len(), 1);
    assert!(st.render().contains("orphaned base entry"));
}

// ---- shell matcher parity and a real resync ------------------------------

fn resync_script() -> Option<PathBuf> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()?
        .join("defaults/scripts/resync-installed.sh");
    p.is_file().then_some(p)
}

/// Run the real, extracted `is_ignored()` against `ignore_file` for `rel`.
fn shell_is_ignored(script: &Path, ignore_file: &Path, rel: &str) -> bool {
    let driver = r#"
        set -euo pipefail
        FN="$(sed -n '/^is_ignored() {/,/^}/p' "$1")"
        [[ -n "$FN" ]] || exit 2
        eval "$FN"; IGNORE_FILE="$2"; is_ignored "$3"
    "#;
    let st = Command::new("bash")
        .args(["-c", driver, "bash"])
        .arg(script)
        .arg(ignore_file)
        .arg(rel)
        .status()
        .unwrap();
    match st.code() {
        Some(0) => true,
        Some(1) => false,
        other => panic!("shell driver failed: {other:?}"),
    }
}

/// Every pin `add_pin` writes is matched by the vendored shell matcher for
/// the `rel` resync passes — the sidecar never changes pin semantics.
#[test]
fn pins_written_by_add_match_the_shell_is_ignored() {
    let Some(script) = resync_script() else {
        return;
    };
    let cases = [
        (".loom/roles/curator.md", "roles/curator.md"),
        ("./.loom/hooks/post-worktree.sh", "hooks/post-worktree.sh"),
        ("commands/loom/builder.md", "commands/loom/builder.md"),
        (".loom/CLAUDE.md", ".loom/CLAUDE.md"),
        ("./.loom/pricing.json", ".loom/pricing.json"),
    ];
    for (input, rel) in cases {
        let tmp = TempDir::new().unwrap();
        write(&tmp.path().join(IGNORE_FILE), "# existing\nscripts/a.sh # c\n\n");
        add_pin(tmp.path(), &opts(input)).unwrap();
        let file = tmp.path().join(IGNORE_FILE);
        assert!(shell_is_ignored(&script, &file, rel), "{input} -> {rel}");
        assert!(shell_is_ignored(&script, &file, "scripts/a.sh"), "existing pin still matches");
        assert!(!shell_is_ignored(&script, &file, "roles/other.md"));
    }
}

/// End to end with the real `resync-installed.sh`, in the consumer topology
/// (separate source checkout reached via `.loom/loom-source-path`): two
/// resyncs after upstream moves leave the pinned file and the recorded base
/// byte-identical, while an unpinned sibling still updates.
#[test]
fn repeated_real_resync_preserves_pin_and_recorded_base() {
    let Some(script) = resync_script() else {
        return;
    };
    let src = TempDir::new().unwrap();
    let s = src.path();
    run_git(s, &["init", "-q"]);
    write(&s.join("defaults/scripts/noop.sh"), "true\n");
    write(&s.join("defaults/roles/curator.md"), ROLE_A);
    write(&s.join("defaults/roles/builder.md"), "B1\n");
    write(&s.join("package.json"), "{\n  \"version\": \"9.9.9\"\n}\n");
    let a = commit_all(s, "A");

    let ws = TempDir::new().unwrap();
    let w = ws.path();
    run_git(w, &["init", "-q"]);
    write(&w.join(".loom/scripts/noop.sh"), "true\n");
    write(&w.join(".loom/roles/curator.md"), &format!("{ROLE_A}local patch\n"));
    write(&w.join(".loom/roles/builder.md"), "B1\n");
    write(&w.join(".loom/loom-source-path"), &format!("{}\n", s.display()));
    let meta = format!(
        r#"{{"loom_version": "9.9.9", "loom_commit": "{a}", "loom_source": "{}", "installed_files": []}}"#,
        s.display()
    );
    write(&w.join(".loom/install-metadata.json"), &meta);

    let rep = add_pin(w, &opts("roles/curator.md")).unwrap();
    assert_eq!(recorded(&rep).sha, a);
    assert_eq!(recorded(&rep).source_path, "defaults/roles/curator.md");
    commit_all(w, "chore: install Loom v0.0.0");

    write(&s.join("defaults/roles/curator.md"), ROLE_B);
    write(&s.join("defaults/roles/builder.md"), "B2\n");
    let b = commit_all(s, "B");

    let base_before = fs::read(w.join(BASE_FILE)).unwrap();
    let pinned_before = fs::read(w.join(".loom/roles/curator.md")).unwrap();
    for _ in 0..2 {
        let out = Command::new("bash")
            .arg(&script)
            .current_dir(w)
            .output()
            .unwrap();
        let log = String::from_utf8_lossy(&out.stdout).to_string()
            + &String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "resync failed:\n{log}");
    }
    assert_eq!(fs::read(w.join(BASE_FILE)).unwrap(), base_before, "base rewritten by resync");
    assert_eq!(fs::read(w.join(".loom/roles/curator.md")).unwrap(), pinned_before);
    assert_eq!(fs::read_to_string(w.join(".loom/roles/builder.md")).unwrap(), "B2\n");

    let st = status(w, None, "HEAD");
    match &st.pins[0].drift {
        Drift::Measured {
            commits: 1,
            added: 2,
            removed: 1,
            diff_cmd,
            ..
        } => {
            assert!(diff_cmd.ends_with(&format!("diff {a} {b} -- defaults/roles/curator.md")));
        }
        other => panic!("expected measured drift, got {other:?}"),
    }
}
