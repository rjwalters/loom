use super::*;

// --- select_pr_status / rows_to_status / classify_pr_row (#6746) ------
//
// A branch can have more than one PR opened against it over its
// lifetime; both `gh pr list --head` and the REST `pulls?head=` list
// endpoint default to newest-created-first, so a naive "take the first
// row" reads whichever PR was opened *last*, not whichever one is
// actually relevant (e.g. a merged PR, then a later unrelated PR from
// the same branch name that closed without merging — observed live for
// `feature/issue-5179` during #6653's curation).

fn pr_row(state: &str, merged_at: Option<&str>, closed_at: Option<&str>) -> PrRow {
    // `PrRow`'s fields are private but it derives `Deserialize`, so tests
    // construct rows the same way the production code parses them: from
    // the exact `gh pr list --json number,state,mergedAt,closedAt` shape.
    let json = serde_json::json!({
        "number": 1,
        "state": state,
        "mergedAt": merged_at,
        "closedAt": closed_at,
    });
    serde_json::from_value(json).unwrap()
}

#[test]
fn rows_to_status_prefers_merged_over_a_later_closed_no_merge_row() {
    // Exact shape of the live #6746 repro: the newer row (closed, no
    // merge) sorts first; the older row (merged) sorts second. The
    // result must still be `Merged`.
    let rows = vec![
        pr_row("CLOSED", None, Some("2026-08-04T02:43:56Z")),
        pr_row(
            "CLOSED", // GitHub reports a merged PR's `state` as CLOSED too
            Some("2026-08-04T02:33:48Z"),
            None,
        ),
    ];
    let status = rows_to_status(Some(rows));
    assert_eq!(
        status,
        PrStatus::Merged {
            merged_at: "2026-08-04T02:33:48Z".to_string()
        }
    );
}

#[test]
fn rows_to_status_prefers_merged_regardless_of_row_order() {
    // Same rows, merged-first this time — order must not matter.
    let rows = vec![
        pr_row("CLOSED", Some("2026-08-04T02:33:48Z"), None),
        pr_row("CLOSED", None, Some("2026-08-04T02:43:56Z")),
    ];
    let status = rows_to_status(Some(rows));
    assert_eq!(
        status,
        PrStatus::Merged {
            merged_at: "2026-08-04T02:33:48Z".to_string()
        }
    );
}

#[test]
fn rows_to_status_prefers_open_over_closed_no_merge_when_no_merge_present() {
    // No `Merged` row anywhere: `Open` (an actively-reviewed PR) beats an
    // older `ClosedNoMerge` row. The "Merged always wins" rule alone does
    // not disambiguate this case, so the order is picked and documented
    // explicitly (test plan item 3 in the issue's curation notes).
    let rows = vec![
        pr_row("CLOSED", None, Some("2026-08-01T00:00:00Z")),
        pr_row("OPEN", None, None),
    ];
    assert_eq!(rows_to_status(Some(rows)), PrStatus::Open);
}

#[test]
fn rows_to_status_falls_back_to_first_row_when_all_unmerged_and_closed() {
    // No `Merged`, no `Open`: the first (i.e. most-recently-created,
    // given the forge's default ordering) `ClosedNoMerge` row wins.
    let rows = vec![
        pr_row("CLOSED", None, Some("2026-08-04T02:43:56Z")),
        pr_row("CLOSED", None, Some("2026-08-01T00:00:00Z")),
    ];
    assert_eq!(
        rows_to_status(Some(rows)),
        PrStatus::ClosedNoMerge {
            closed_at: Some("2026-08-04T02:43:56Z".to_string())
        }
    );
}

#[test]
fn rows_to_status_empty_rows_is_no_pr() {
    assert_eq!(rows_to_status(Some(Vec::new())), PrStatus::NoPr);
}

#[test]
fn rows_to_status_none_is_unknown() {
    assert_eq!(rows_to_status(None), PrStatus::Unknown);
}

#[test]
fn select_pr_status_prefers_merged_across_three_rows() {
    let statuses = vec![
        PrStatus::ClosedNoMerge {
            closed_at: Some("2026-08-04T02:43:56Z".to_string()),
        },
        PrStatus::Open,
        PrStatus::Merged {
            merged_at: "2026-08-04T02:33:48Z".to_string(),
        },
    ];
    assert_eq!(
        select_pr_status(statuses),
        PrStatus::Merged {
            merged_at: "2026-08-04T02:33:48Z".to_string()
        }
    );
}

#[test]
fn classify_pr_row_multi_row_rest_path_prefers_merged() {
    // REST counterpart of `rows_to_status_prefers_merged_over_a_later_closed_no_merge_row`
    // (issue #6746's third acceptance criterion) — same preference logic,
    // driven through `classify_pr_row` the way `check_pr_status_for_branch_rest`
    // now feeds `select_pr_status`.
    let statuses = vec![
        classify_pr_row("closed", None, Some("2026-08-04T02:43:56Z")),
        classify_pr_row("closed", Some("2026-08-04T02:33:48Z"), None),
    ];
    assert_eq!(
        select_pr_status(statuses),
        PrStatus::Merged {
            merged_at: "2026-08-04T02:33:48Z".to_string()
        }
    );
}

// --- confirm_destructive_action (#5736) -------------------------------

#[test]
fn confirm_destructive_action_dry_run_bypasses_prompt_regardless_of_force() {
    // Neither branch touches stdin, so these are safe to assert directly
    // without a subprocess harness (see `clean_aggressive_confirmation.rs`
    // for the closed-stdin end-to-end case).
    assert!(confirm_destructive_action(true, false));
    assert!(confirm_destructive_action(true, true));
}

#[test]
fn confirm_destructive_action_force_bypasses_prompt() {
    assert!(confirm_destructive_action(false, true));
}

#[test]
fn grace_period_not_passed_reports_remaining() {
    let now = Utc::now();
    let merged = now - chrono::Duration::seconds(100);
    let (passed, remaining) = check_grace_period(merged, 600, now);
    assert!(!passed);
    assert_eq!(remaining, 500);
}

#[test]
fn grace_period_passed_reports_zero_remaining() {
    let now = Utc::now();
    let merged = now - chrono::Duration::seconds(700);
    let (passed, remaining) = check_grace_period(merged, 600, now);
    assert!(passed);
    assert_eq!(remaining, 0);
}

#[test]
fn dir_size_human_handles_missing_dir() {
    // A missing directory contributes 0 bytes, not an error.
    assert_eq!(dir_size_human(Path::new("/does/not/exist/at/all")), "0B");
}

// --- reclaim_worktree_artifacts (#5187) ------------------------------

#[test]
fn reclaim_removes_target_and_node_modules_but_nothing_else() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("target/debug")).unwrap();
    std::fs::write(tmp.path().join("target/debug/binary"), b"x").unwrap();
    std::fs::create_dir_all(tmp.path().join("node_modules/.bin")).unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(tmp.path().join("src/main.rs"), b"fn main() {}").unwrap();
    std::fs::write(tmp.path().join("Cargo.lock"), b"lockfile").unwrap();

    let reclaimed = reclaim_worktree_artifacts(tmp.path(), false);
    let names: Vec<_> = reclaimed.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(
        names.into_iter().collect::<std::collections::HashSet<_>>(),
        std::collections::HashSet::from(["target", "node_modules"])
    );
    assert!(!tmp.path().join("target").exists());
    assert!(!tmp.path().join("node_modules").exists());
    // Everything else — git history stand-ins, source, lockfiles — is untouched.
    assert!(tmp.path().join("src/main.rs").is_file());
    assert!(tmp.path().join("Cargo.lock").is_file());
}

#[test]
fn reclaim_never_removes_a_same_named_file() {
    // `Cargo.lock` and `pnpm-lock.yaml` are both entries in
    // BUILD_ARTIFACT_PATTERNS, but they are files, not directories — the
    // reclaim pass must never `remove_dir_all` a file.
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("Cargo.lock"), b"lockfile").unwrap();
    std::fs::write(tmp.path().join("pnpm-lock.yaml"), b"lockfile").unwrap();
    std::fs::write(tmp.path().join(".loom-in-use"), b"{}").unwrap();

    let reclaimed = reclaim_worktree_artifacts(tmp.path(), false);
    assert!(reclaimed.is_empty(), "{reclaimed:?}");
    assert!(tmp.path().join("Cargo.lock").is_file());
    assert!(tmp.path().join("pnpm-lock.yaml").is_file());
    assert!(tmp.path().join(".loom-in-use").is_file());
}

#[test]
fn reclaim_dry_run_reports_without_removing() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("target")).unwrap();

    let reclaimed = reclaim_worktree_artifacts(tmp.path(), true);
    assert_eq!(reclaimed.len(), 1);
    assert_eq!(reclaimed[0].name, "target");
    assert!(tmp.path().join("target").is_dir(), "dry-run must not remove");
}

#[test]
fn reclaim_with_no_artifact_dirs_is_a_clean_no_op() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("README.md"), b"hi").unwrap();
    let reclaimed = reclaim_worktree_artifacts(tmp.path(), false);
    assert!(reclaimed.is_empty());
}

// --- sweep_primary_checkout_artifacts (#5919) ------------------------

#[test]
fn primary_checkout_sweep_removes_the_checkouts_own_build_dirs() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("target/release")).unwrap();
    std::fs::write(tmp.path().join("target/release/loom-daemon"), b"x").unwrap();
    std::fs::create_dir_all(tmp.path().join("node_modules")).unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(tmp.path().join("Cargo.toml"), b"[package]").unwrap();

    let outcomes = sweep_primary_checkout_artifacts(tmp.path(), false);
    let reclaimed: Vec<_> = outcomes
        .iter()
        .filter_map(|o| match o {
            ArtifactOutcome::Reclaimed(a) => Some(a.name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(reclaimed, vec!["target", "node_modules"]);
    assert!(!tmp.path().join("target").exists());
    assert!(!tmp.path().join("node_modules").exists());
    // The working clone itself is untouched — this is a developer's repo,
    // not a throwaway worktree.
    assert!(tmp.path().join("src").is_dir());
    assert!(tmp.path().join("Cargo.toml").is_file());
}

#[test]
fn primary_checkout_sweep_reports_absent_dirs_instead_of_failing() {
    let tmp = tempfile::tempdir().unwrap();
    let outcomes = sweep_primary_checkout_artifacts(tmp.path(), false);
    assert_eq!(outcomes.len(), PRIMARY_CHECKOUT_ARTIFACTS.len());
    assert!(outcomes
        .iter()
        .all(|o| matches!(o, ArtifactOutcome::Absent(_))));
}

#[test]
fn primary_checkout_sweep_dry_run_removes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("target")).unwrap();
    let outcomes = sweep_primary_checkout_artifacts(tmp.path(), true);
    assert!(matches!(outcomes[0], ArtifactOutcome::Reclaimed(_)));
    assert!(tmp.path().join("target").is_dir());
}

// --- live-binary protection (#6127) ----------------------------------

/// Copy the host's `sleep` into `<dir>/<name>` and run it, so a live
/// process's executable image sits inside a directory the sweep would
/// otherwise delete. Deliberately a *different* process than this test
/// binary: the pre-existing `deep_clean::exe_is_inside_artifacts` gate only
/// ever compared `current_exe()`, which is exactly the gap #6127 reports.
fn spawn_service_in(dir: &Path, name: &str) -> std::process::Child {
    let source = ["/bin/sleep", "/usr/bin/sleep"]
        .iter()
        .map(Path::new)
        .find(|p| p.is_file())
        .expect("a `sleep` binary is needed to stand in for a service");
    std::fs::create_dir_all(dir).unwrap();
    let program = dir.join(name);
    std::fs::copy(source, &program).unwrap();
    // Re-sign the relocated copy on macOS: a plain `fs::copy` of a system binary
    // carries over the original embedded code signature (bound to the source
    // path's identity), so Gatekeeper SIGKILLs the exec'd copy asynchronously —
    // `Command::spawn()` still returns `Ok`, so the "live" process can already
    // be dead by the time this test asserts on it. Same mitigation this repo
    // already applies to its own compiled test binaries via
    // `.cargo/macos-test-runner.sh` (#2298). Test-only; not a production fix.
    // See #6430 / #6343.
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("codesign")
            .args(["-f", "-s", "-", program.to_str().unwrap()])
            .status()
            .expect("failed to ad-hoc codesign test binary");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    // Retried on ETXTBSY: a concurrent test thread forking while our write
    // fd was open leaves the child holding it, and Linux then refuses to
    // exec. A harness race, not a property of the code under test.
    let mut last_err = None;
    for _ in 0..100 {
        match std::process::Command::new(&program)
            .arg("300")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(child) => {
                std::thread::sleep(std::time::Duration::from_millis(250));
                return child;
            }
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                last_err = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => panic!("stand-in service must spawn: {e}"),
        }
    }
    panic!("stand-in service never became executable: {last_err:?}");
}

#[test]
fn primary_checkout_sweep_keeps_a_dir_backing_another_processs_binary() {
    let tmp = tempfile::tempdir().unwrap();
    let mut service = spawn_service_in(&tmp.path().join("target/release"), "loom-svc-fixture");
    std::fs::create_dir_all(tmp.path().join("node_modules")).unwrap();

    let outcomes = sweep_primary_checkout_artifacts(tmp.path(), false);

    let target_survived = tmp.path().join("target").is_dir();
    let _ = service.kill();
    let _ = service.wait();

    let protected = outcomes
        .iter()
        .find_map(|o| match o {
            ArtifactOutcome::Protected(p) => Some(p),
            _ => None,
        })
        .expect("target/ must report as Protected, not Reclaimed");
    assert_eq!(protected.name, "target");
    assert!(protected.holders.iter().any(|h| h.pid == service.id()));
    assert!(
        target_survived,
        "the live service's binary must still be on disk after the sweep"
    );
    assert!(
        protected.reason().contains("live process"),
        "the skip must explain itself: {}",
        protected.reason()
    );

    // Protection is per-directory, not all-or-nothing: node_modules/ has
    // nothing running inside it and is still reclaimed.
    assert!(outcomes
        .iter()
        .any(|o| matches!(o, ArtifactOutcome::Reclaimed(a) if a.name == "node_modules")));
    assert!(!tmp.path().join("node_modules").exists());
}

#[test]
fn primary_checkout_sweep_dry_run_does_not_promise_to_remove_a_live_dir() {
    // A preview that says "Would remove target/" while a service is running
    // from it is a preview an operator would act on.
    let tmp = tempfile::tempdir().unwrap();
    let mut service = spawn_service_in(&tmp.path().join("target/release"), "loom-svc-dryrun");

    let outcomes = sweep_primary_checkout_artifacts(tmp.path(), true);

    let _ = service.kill();
    let _ = service.wait();

    assert!(matches!(outcomes[0], ArtifactOutcome::Protected(_)), "{outcomes:?}");
}

#[test]
fn the_scheduled_and_manual_deep_paths_share_one_artifact_list() {
    // The automatic pass added in #5919 must never remove more than a
    // hand-typed `clean --deep --safe` would. One list, asserted.
    assert_eq!(PRIMARY_CHECKOUT_ARTIFACTS, ["target", "node_modules"]);
}

// --- untracked-orphan worktree removal (#5177 AC5) -------------------

#[test]
fn untracked_worktree_error_recognizes_gits_message() {
    // git's actual message for a path that exists but is not a worktree.
    assert!(is_untracked_worktree_error(
        "fatal: '/repo/.loom/worktrees/issue-42' is not a working tree"
    ));
    assert!(is_untracked_worktree_error("IS NOT A WORKING TREE")); // case-insensitive
                                                                   // Any other failure must NOT be treated as an orphan directory.
    assert!(!is_untracked_worktree_error(
        "fatal: validation failed, cannot remove working tree"
    ));
    assert!(!is_untracked_worktree_error("Permission denied (os error 13)"));
    assert!(!is_untracked_worktree_error(""));
}

#[test]
fn force_remove_orphan_requires_all_three_conditions() {
    let ok = "fatal: 'x' is not a working tree";
    let other = "some other failure";
    // All three present → fall back to direct removal.
    assert!(should_force_remove_orphan_dir(ok, true, true));
    // Missing any single guard → refuse (never a blanket rm -rf).
    assert!(!should_force_remove_orphan_dir(ok, false, true)); // no sentinel
    assert!(!should_force_remove_orphan_dir(ok, true, false)); // outside root
    assert!(!should_force_remove_orphan_dir(other, true, true)); // different error
}

/// End-to-end: a directory under the managed worktree root that git no
/// longer tracks as a worktree (the #5177 "is not a working tree" orphan) is
/// removed by `cleanup_worktree` instead of erroring out.
#[test]
#[serial_test::serial]
fn cleanup_worktree_removes_untracked_orphan_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_root = tmp.path().canonicalize().unwrap();

    // A real git repo so `git worktree remove` produces the genuine
    // "is not a working tree" error rather than "not a git repository".
    assert!(Command::new("git")
        .args(["init", "-q"])
        .current_dir(&repo_root)
        .status()
        .unwrap()
        .success());

    // An orphan worktree directory under the resolved (override-aware)
    // managed root, carrying the `.loom-managed` sentinel — but with no
    // corresponding `git worktree list` entry.
    let orphan = crate::worktree_root::worktree_root(&repo_root).join("issue-999");
    std::fs::create_dir_all(&orphan).unwrap();
    std::fs::write(orphan.join(".loom-managed"), "test").unwrap();
    std::fs::write(orphan.join("some-build-artifact"), "junk").unwrap();
    assert!(orphan.is_dir());

    let result = cleanup_worktree(&repo_root, &orphan, 999, false, "test");
    assert_eq!(
        result,
        Ok(CleanupOutcome::Removed),
        "orphan removal should succeed and report Removed: {result:?}"
    );
    assert!(!orphan.exists(), "orphan directory should be gone");
}

// --- redirected cargo target dir (#7239) ------------------------------

/// End-to-end proof that the removal path itself — not just the
/// [`super::cargo_target`] decision engine's unit tests — reclaims a
/// target dir Cargo was configured to write OUTSIDE the worktree.
///
/// Uses a real `.cargo/config.toml` redirect rather than
/// `CARGO_TARGET_DIR`, because the env var is process-global and this
/// suite runs multi-threaded: a `set_var` here would leak into every
/// concurrently-running test. It is skipped outright when the ambient
/// environment already redirects, since that would short-circuit the
/// resolution this test exists to exercise.
#[test]
#[serial_test::serial]
fn cleanup_worktree_reclaims_a_redirected_cargo_target_dir() {
    if std::env::var("CARGO_TARGET_DIR").is_ok_and(|v| !v.is_empty()) {
        eprintln!("skipping: ambient CARGO_TARGET_DIR short-circuits resolution");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let repo_root = base.join("repo");
    std::fs::create_dir_all(&repo_root).unwrap();
    for args in [
        vec!["init", "-q", "-b", "main"],
        vec!["config", "user.email", "t@t"],
        vec!["config", "user.name", "t"],
    ] {
        assert!(Command::new("git")
            .args(&args)
            .current_dir(&repo_root)
            .status()
            .unwrap()
            .success());
    }
    std::fs::write(repo_root.join("README.md"), "x").unwrap();
    for args in [vec!["add", "."], vec!["commit", "-qm", "init"]] {
        assert!(Command::new("git")
            .args(&args)
            .current_dir(&repo_root)
            .status()
            .unwrap()
            .success());
    }

    let worktree = crate::worktree_root::worktree_root(&repo_root).join("issue-7239");
    std::fs::create_dir_all(worktree.parent().unwrap()).unwrap();
    assert!(Command::new("git")
        .args(["worktree", "add", "-q", "-b", "feature/issue-7239"])
        .arg(&worktree)
        .current_dir(&repo_root)
        .status()
        .unwrap()
        .success());
    std::fs::write(worktree.join(".loom-managed"), "test").unwrap();

    // A minimal cargo workspace inside the worktree whose build output is
    // redirected to an external volume-style path — the exact shape that
    // leaked hundreds of GB on a live host.
    let external = base.join("cargo-target/issue-7239");
    std::fs::create_dir_all(external.join("debug")).unwrap();
    std::fs::write(external.join("debug/artifact.bin"), vec![0u8; 4096]).unwrap();
    std::fs::create_dir_all(worktree.join("src")).unwrap();
    std::fs::write(worktree.join("src/lib.rs"), "").unwrap();
    std::fs::write(
        worktree.join("Cargo.toml"),
        "[package]\nname = \"leaky\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(worktree.join(".cargo")).unwrap();
    std::fs::write(
        worktree.join(".cargo/config.toml"),
        format!("[build]\ntarget-dir = \"{}\"\n", external.display()),
    )
    .unwrap();

    // Sanity-check the resolver actually sees the redirect before relying
    // on the removal path to act on it — otherwise a resolution failure
    // would make this test vacuously "pass" on the `Inside` branch.
    let resolved = super::super::cargo_target::resolve_for_worktree(&worktree);
    if resolved.canonicalize().unwrap_or(resolved.clone()) != external {
        eprintln!("skipping: cargo could not resolve the redirect (resolved {resolved:?})");
        return;
    }

    let result = cleanup_worktree(&repo_root, &worktree, 7239, false, "test");
    assert_eq!(result, Ok(CleanupOutcome::Removed), "{result:?}");
    assert!(!worktree.exists(), "worktree should be gone");
    assert!(
        !external.exists(),
        "the redirected target dir should have been reclaimed with the worktree"
    );
}

/// The safety half of the same path: a redirected dir another LIVE
/// worktree also builds into is left completely alone.
#[test]
#[serial_test::serial]
fn cleanup_worktree_keeps_a_target_dir_a_live_worktree_still_shares() {
    if std::env::var("CARGO_TARGET_DIR").is_ok_and(|v| !v.is_empty()) {
        eprintln!("skipping: ambient CARGO_TARGET_DIR short-circuits resolution");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let repo_root = base.join("repo");
    std::fs::create_dir_all(&repo_root).unwrap();
    for args in [
        vec!["init", "-q", "-b", "main"],
        vec!["config", "user.email", "t@t"],
        vec!["config", "user.name", "t"],
    ] {
        assert!(Command::new("git")
            .args(&args)
            .current_dir(&repo_root)
            .status()
            .unwrap()
            .success());
    }
    std::fs::write(repo_root.join("README.md"), "x").unwrap();
    for args in [vec!["add", "."], vec!["commit", "-qm", "init"]] {
        assert!(Command::new("git")
            .args(&args)
            .current_dir(&repo_root)
            .status()
            .unwrap()
            .success());
    }

    // One shared target dir for the whole machine — the host-optimize
    // convention, and the case where an over-eager reclaim is data loss.
    let shared = base.join("cargo-target");
    std::fs::create_dir_all(shared.join("debug")).unwrap();
    std::fs::write(shared.join("debug/artifact.bin"), vec![0u8; 4096]).unwrap();

    let mut made = Vec::new();
    for issue in [7239u32, 7240] {
        let wt = crate::worktree_root::worktree_root(&repo_root).join(format!("issue-{issue}"));
        std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
        assert!(Command::new("git")
            .args([
                "worktree",
                "add",
                "-q",
                "-b",
                &format!("feature/issue-{issue}")
            ])
            .arg(&wt)
            .current_dir(&repo_root)
            .status()
            .unwrap()
            .success());
        std::fs::write(wt.join(".loom-managed"), "test").unwrap();
        std::fs::create_dir_all(wt.join("src")).unwrap();
        std::fs::write(wt.join("src/lib.rs"), "").unwrap();
        std::fs::write(
            wt.join("Cargo.toml"),
            format!("[package]\nname = \"w{issue}\"\nversion = \"0.0.0\"\nedition = \"2021\"\n"),
        )
        .unwrap();
        std::fs::create_dir_all(wt.join(".cargo")).unwrap();
        std::fs::write(
            wt.join(".cargo/config.toml"),
            format!("[build]\ntarget-dir = \"{}\"\n", shared.display()),
        )
        .unwrap();
        made.push(wt);
    }

    let resolved = super::super::cargo_target::resolve_for_worktree(&made[0]);
    if resolved.canonicalize().unwrap_or(resolved.clone()) != shared {
        eprintln!("skipping: cargo could not resolve the redirect (resolved {resolved:?})");
        return;
    }

    let result = cleanup_worktree(&repo_root, &made[0], 7239, false, "test");
    assert_eq!(result, Ok(CleanupOutcome::Removed), "{result:?}");
    assert!(!made[0].exists(), "the removed worktree should be gone");
    assert!(
        shared.join("debug/artifact.bin").is_file(),
        "the shared target dir must survive — issue-7240 is still building into it"
    );
    assert!(made[1].is_dir(), "the sibling worktree is untouched");
}

// --- already-gone stale registration (#5895) --------------------------

/// The second orphan shape from #5895: `git worktree remove` fails with
/// "is not a working tree" (never registered, or already deregistered)
/// AND the directory does not exist on disk either — there is nothing
/// unsafe left to remove, so this must succeed as `AlreadyGone`, not
/// error out the way it did before this fix.
#[test]
#[serial_test::serial]
fn cleanup_worktree_treats_missing_directory_as_already_gone() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_root = tmp.path().canonicalize().unwrap();
    assert!(Command::new("git")
        .args(["init", "-q"])
        .current_dir(&repo_root)
        .status()
        .unwrap()
        .success());

    let gone = crate::worktree_root::worktree_root(&repo_root).join("issue-4525");
    assert!(!gone.exists(), "fixture must not exist on disk");

    let result = cleanup_worktree(&repo_root, &gone, 4525, false, "test");
    assert_eq!(
        result,
        Ok(CleanupOutcome::AlreadyGone),
        "a stale registration with no directory on disk must not error: {result:?}"
    );
}

/// AC1: a worktree registration whose directory has already been deleted
/// by something other than `git worktree remove` never becomes a
/// `read_dir` entry, so `cleanup_worktree`'s per-entry backstop alone
/// can't reach it — `clean_worktrees` must proactively `git worktree
/// prune` before enumerating so the stale metadata doesn't linger
/// forever (this is the literal repro from the issue: `.git/worktrees/*`
/// entries surviving indefinitely once their directory is gone).
#[test]
#[serial_test::serial]
fn clean_worktrees_prunes_a_registration_whose_directory_is_already_gone() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_root = tmp.path().canonicalize().unwrap();
    assert!(Command::new("git")
        .args(["init", "-q"])
        .current_dir(&repo_root)
        .status()
        .unwrap()
        .success());
    // CI runners have no global git identity — set one explicitly or
    // `git commit` refuses with "Please tell me who you are."
    git(&repo_root, &["config", "user.email", "loom@example.com"]);
    git(&repo_root, &["config", "user.name", "Loom Test"]);
    assert!(Command::new("git")
        .args(["commit", "--allow-empty", "-q", "-m", "init"])
        .current_dir(&repo_root)
        .status()
        .unwrap()
        .success());

    let worktrees_dir = crate::worktree_root::worktree_root(&repo_root);
    std::fs::create_dir_all(&worktrees_dir).unwrap();
    let wt_path = worktrees_dir.join("issue-777");
    assert!(Command::new("git")
        .args(["worktree", "add", "-q"])
        .arg(&wt_path)
        .args(["-b", "wt-777-branch"])
        .current_dir(&repo_root)
        .status()
        .unwrap()
        .success());

    // Removed by something other than `git worktree remove` (a manual
    // `rm -rf`, a reaper that deleted the directory without
    // deregistering, an interrupted sweep — see the issue body). The
    // registration is left behind and, because the directory is gone,
    // invisible to `clean_worktrees`'s `read_dir`-based enumeration.
    std::fs::remove_dir_all(&wt_path).unwrap();

    let list_before = String::from_utf8_lossy(
        &Command::new("git")
            .args(["worktree", "list", "--porcelain"])
            .current_dir(&repo_root)
            .output()
            .unwrap()
            .stdout,
    )
    .to_string();
    assert!(
        list_before.contains("issue-777"),
        "fixture must still be registered before the run: {list_before}"
    );

    let mut stats = CleanupStats::default();
    let opts = CleanOptions::default();
    clean_worktrees(&repo_root, &mut stats, &opts);

    let list_after = String::from_utf8_lossy(
        &Command::new("git")
            .args(["worktree", "list", "--porcelain"])
            .current_dir(&repo_root)
            .output()
            .unwrap()
            .stdout,
    )
    .to_string();
    assert!(
        !list_after.contains("issue-777"),
        "stale registration should be pruned before enumeration, not left to rot: {list_after}"
    );
    assert_eq!(
        stats.errors, 0,
        "a directory that never became a read_dir entry must never surface as an error"
    );
}

#[test]
fn record_cleanup_result_buckets_already_gone_as_not_an_error() {
    let mut stats = CleanupStats::default();
    record_cleanup_result(
        &mut stats,
        Path::new("/tmp/issue-4525"),
        Ok(CleanupOutcome::AlreadyGone),
    );
    assert_eq!(stats.stale_worktree_registrations, 1);
    assert_eq!(stats.cleaned_worktrees, 0);
    assert_eq!(stats.errors, 0);
}

#[test]
fn record_cleanup_result_counts_removed_and_errors_separately() {
    let mut stats = CleanupStats::default();
    record_cleanup_result(&mut stats, Path::new("/tmp/issue-1"), Ok(CleanupOutcome::Removed));
    record_cleanup_result(&mut stats, Path::new("/tmp/issue-2"), Err("boom".to_string()));
    assert_eq!(stats.cleaned_worktrees, 1);
    assert_eq!(stats.errors, 1);
    assert_eq!(stats.stale_worktree_registrations, 0);
}

/// A removal failure that is NOT the untracked-orphan signature must still
/// error (and must never trigger the direct-removal fallback), even for a
/// managed path under the root.
#[test]
#[serial_test::serial]
fn cleanup_worktree_does_not_force_remove_on_other_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_root = tmp.path().canonicalize().unwrap();
    // Not a git repository at all → `git worktree remove` fails with a
    // "not a git repository" error, which is NOT the orphan signature.
    let managed = crate::worktree_root::worktree_root(&repo_root).join("issue-1000");
    std::fs::create_dir_all(&managed).unwrap();
    std::fs::write(managed.join(".loom-managed"), "test").unwrap();

    let result = cleanup_worktree(&repo_root, &managed, 1000, false, "test");
    assert!(result.is_err(), "non-orphan failure must propagate");
    assert!(managed.exists(), "directory must be left in place on a non-orphan failure");
}

// --- pr-<N> worktree cleanup (#5939) ----------------------------------

/// A `pr-<N>` worktree's branch is read from the worktree itself
/// (`current_branch`), not constructed from `pr_num` — unlike
/// `cleanup_worktree`, which always has a `feature/issue-<N>` name to
/// build.
#[test]
#[serial_test::serial]
fn cleanup_pr_worktree_deletes_the_worktrees_own_checked_out_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_root = tmp.path().canonicalize().unwrap();
    assert!(Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .current_dir(&repo_root)
        .status()
        .unwrap()
        .success());
    // A minimal commit so `git worktree add` has something to branch from.
    std::fs::write(repo_root.join("README.md"), "x").unwrap();
    assert!(Command::new("git")
        .args(["add", "."])
        .current_dir(&repo_root)
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .args([
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "init"
        ])
        .current_dir(&repo_root)
        .status()
        .unwrap()
        .success());

    let wt_path = crate::worktree_root::worktree_root(&repo_root).join("pr-777");
    assert!(Command::new("git")
        .args(["worktree", "add", "-b", "some-external-fork-branch"])
        .arg(&wt_path)
        .current_dir(&repo_root)
        .status()
        .unwrap()
        .success());
    std::fs::write(wt_path.join(".loom-managed"), "test").unwrap();

    // The merged PR's head SHA == the local branch tip: the #4100 safety
    // criterion holds, so the force-delete is provably lossless.
    let tip = local_branch_tip(&repo_root, "some-external-fork-branch").unwrap();
    let result = cleanup_pr_worktree(&repo_root, &wt_path, 777, false, "test", Some(tip.as_str()));
    assert!(result.is_ok(), "{result:?}");
    assert!(!wt_path.exists());

    let branches = Command::new("git")
        .args(["branch", "--list", "some-external-fork-branch"])
        .current_dir(&repo_root)
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&branches.stdout).trim().is_empty(),
        "the PR worktree's own branch must be deleted, not left behind"
    );

    // #5950: the removal is in the ledger. This is the invariant that
    // makes both an entry and its absence evidence — before the #5939
    // review this was the one deletion path that wrote nothing.
    let ledger_path = crate::worktree_ops::removal_log::ledger_path(&repo_root);
    let ledger = std::fs::read_to_string(&ledger_path)
        .expect("the pr-<N> removal must append a ledger line");
    assert_eq!(ledger.lines().count(), 1, "exactly one line per removal: {ledger}");
    assert!(ledger.contains(r#""mechanism":"test""#), "{ledger}");
    assert!(ledger.contains(r#""branch":"some-external-fork-branch""#), "{ledger}");
    assert!(ledger.contains("classify_pr_worktree=Remove"), "{ledger}");
    assert!(ledger.contains("pr-777"), "{ledger}");
}

/// #6264 AC3: confirm `classify_pr_worktree`/`cleanup_pr_worktree` reap a
/// `pr-<N>` worktree stuck on a **detached HEAD** — the exact state
/// observed in the reported incident (`git worktree list` showing
/// `pr-111 ... (detached HEAD)`), reproduced by #6264's investigation as
/// the result of `pr-worktree.sh`'s `gh pr checkout --force` failing when
/// the PR's branch is already checked out in another worktree — the same
/// as a normal named-branch `pr-<N>` worktree
/// ([`cleanup_pr_worktree_deletes_the_worktrees_own_checked_out_branch`]
/// immediately above). This is a "confirm, don't build" AC: the
/// classifier and remover are already branch-state-independent (keyed by
/// worktree PATH + the PR's own status, resolved directly by PR number —
/// see the module doc comment on `PrWorktreeProbes`), so this test is
/// expected to pass unmodified; it exists to make that guarantee
/// permanent rather than merely observed once during code review.
#[test]
#[serial_test::serial]
fn classify_and_cleanup_pr_worktree_treat_a_detached_head_the_same_as_a_named_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_root = tmp.path().canonicalize().unwrap();
    assert!(Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .current_dir(&repo_root)
        .status()
        .unwrap()
        .success());
    std::fs::write(repo_root.join("README.md"), "x").unwrap();
    assert!(Command::new("git")
        .args(["add", "."])
        .current_dir(&repo_root)
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .args([
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "init"
        ])
        .current_dir(&repo_root)
        .status()
        .unwrap()
        .success());

    // Mirrors pr-worktree.sh's own creation shape: `git worktree add
    // --detach` at a commit, with NO subsequent branch switch — modeling
    // the collision case where `gh pr checkout --force` failed and left
    // the worktree parked on detached HEAD instead of the PR's branch.
    let wt_path = crate::worktree_root::worktree_root(&repo_root).join("pr-888");
    assert!(Command::new("git")
        .args(["worktree", "add", "--detach"])
        .arg(&wt_path)
        .arg("main")
        .current_dir(&repo_root)
        .status()
        .unwrap()
        .success());
    std::fs::write(wt_path.join(".loom-managed"), "test").unwrap();

    // Sanity-check the fixture actually reproduces detached HEAD (i.e.
    // this test would fail loudly if the git invocation above ever
    // stopped doing what the comment claims).
    assert_eq!(
            current_branch(&wt_path),
            None,
            "fixture must be on a detached HEAD, matching the incident's own `git worktree list` output"
        );

    // classify_pr_worktree: a merged, grace-period-elapsed PR must decide
    // Remove for the detached worktree exactly as it would for a named
    // branch — no code path here consults the branch at all. Reuses the
    // production reaper's own option set (`safe: true`,
    // `require_managed_sentinel: true`) rather than hand-rolling one, so
    // this test exercises the same gates the live daemon reaper does.
    let opts = crate::worktree_reaper::reaper_clean_options(0);
    let merged_at = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
    let pr_status = |_: u32| PrStatus::Merged {
        merged_at: merged_at.clone(),
    };
    let probes = PrWorktreeProbes {
        in_use_marker: &|_: &Path| None,
        processes_using: &|_: &Path| Vec::new(),
        editable_installs: &|_: &Path| Vec::new(),
        is_managed: &|p: &Path| is_loom_managed(p),
        pr_status: &pr_status,
        branch_reachable_from_remotes: &|_: &Path| true,
        uncommitted: &check_uncommitted_or_untracked_changes,
        now: chrono::Utc::now(),
    };
    let decision = classify_pr_worktree(&wt_path, 888, &opts, &probes);
    assert_eq!(
            decision,
            WorktreeDecision::Remove,
            "a merged PR's detached-HEAD pr-<N> worktree must classify as Remove, same as a named-branch one: {decision:?}"
        );

    // cleanup_pr_worktree: actually removes it. No branch-delete is
    // attempted (there is no branch — current_branch returned None
    // above), only `git worktree remove --force` runs; that alone must
    // still succeed and the directory must be gone.
    let result = cleanup_pr_worktree(&repo_root, &wt_path, 888, false, "test", None);
    assert!(result.is_ok(), "{result:?}");
    assert!(!wt_path.exists(), "detached-HEAD pr-<N> worktree must be removed");
}

/// The #5939-review fix: a `pr-<N>` worktree whose local branch tip is NOT
/// what the forge merged carries commits nobody pushed. The worktree is
/// still removed (its contents are reclaimable), but the branch — the only
/// surviving reference to those commits — must NOT be force-deleted.
///
/// Before this, `git branch -d` was tried and *any* failure escalated to
/// `-D`. Since this repo squash-merges, `-d` fails essentially always, so
/// `-D` was the normal path, not the exception.
#[test]
#[serial_test::serial]
fn cleanup_pr_worktree_keeps_a_branch_whose_tip_is_not_the_merged_head() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_root = tmp.path().canonicalize().unwrap();
    assert!(Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .current_dir(&repo_root)
        .status()
        .unwrap()
        .success());
    std::fs::write(repo_root.join("README.md"), "x").unwrap();
    assert!(Command::new("git")
        .args(["add", "."])
        .current_dir(&repo_root)
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .args([
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "init"
        ])
        .current_dir(&repo_root)
        .status()
        .unwrap()
        .success());

    let wt_path = crate::worktree_root::worktree_root(&repo_root).join("pr-4242");
    assert!(Command::new("git")
        .args(["worktree", "add", "-b", "contributor/feature"])
        .arg(&wt_path)
        .current_dir(&repo_root)
        .status()
        .unwrap()
        .success());
    std::fs::write(wt_path.join(".loom-managed"), "test").unwrap();

    // A local commit that was never pushed — the branch tip now differs
    // from whatever the forge merged.
    std::fs::write(wt_path.join("unpushed.txt"), "work nobody has a copy of").unwrap();
    assert!(Command::new("git")
        .args(["add", "."])
        .current_dir(&wt_path)
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .args([
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "local only"
        ])
        .current_dir(&wt_path)
        .status()
        .unwrap()
        .success());

    let result = cleanup_pr_worktree(
        &repo_root,
        &wt_path,
        4242,
        false,
        "test",
        Some("0000000000000000000000000000000000000000"),
    );
    assert!(result.is_ok(), "{result:?}");
    assert!(!wt_path.exists(), "the worktree itself is still reclaimed");

    let branches = Command::new("git")
        .args(["branch", "--list", "contributor/feature"])
        .current_dir(&repo_root)
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&branches.stdout).contains("contributor/feature"),
        "a branch whose tip is not the merged head must survive — it is the only \
             reference to the unpushed commit"
    );
}

/// An unresolvable head SHA (`None` — the forge probe failed) is the safe
/// side, not a licence to force-delete.
#[test]
fn branch_delete_mode_never_forces_without_a_matching_tip() {
    assert_eq!(
        branch_delete_mode("feature/x", Some("abc123"), Some("abc123")),
        BranchDeleteMode::ForceSafe
    );
    assert_eq!(
        branch_delete_mode("feature/x", Some("abc123"), Some("def456")),
        BranchDeleteMode::SafeOnly
    );
    assert_eq!(
        branch_delete_mode("feature/x", Some("abc123"), None),
        BranchDeleteMode::SafeOnly
    );
    assert_eq!(
        branch_delete_mode("feature/x", None, Some("abc123")),
        BranchDeleteMode::SafeOnly
    );
    assert_eq!(branch_delete_mode("feature/x", None, None), BranchDeleteMode::SafeOnly);
    // An empty local tip is not a match against an empty expectation.
    assert_eq!(branch_delete_mode("feature/x", Some(""), Some("")), BranchDeleteMode::SafeOnly);
    // The protected-name floor wins over an otherwise-safe match.
    assert_eq!(
        branch_delete_mode("develop", Some("abc123"), Some("abc123")),
        BranchDeleteMode::Refuse
    );
}

/// Same untracked-orphan fallback `cleanup_worktree` has (#5177), exercised
/// through the `pr-<N>` path.
#[test]
#[serial_test::serial]
fn cleanup_pr_worktree_removes_untracked_orphan_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_root = tmp.path().canonicalize().unwrap();
    assert!(Command::new("git")
        .args(["init", "-q"])
        .current_dir(&repo_root)
        .status()
        .unwrap()
        .success());

    let orphan = crate::worktree_root::worktree_root(&repo_root).join("pr-999");
    std::fs::create_dir_all(&orphan).unwrap();
    std::fs::write(orphan.join(".loom-managed"), "test").unwrap();
    assert!(orphan.is_dir());

    let result = cleanup_pr_worktree(&repo_root, &orphan, 999, false, "test", None);
    assert!(result.is_ok(), "orphan removal should succeed: {result:?}");
    assert!(!orphan.exists(), "orphan directory should be gone");
}

#[test]
fn cleanup_pr_worktree_dry_run_makes_no_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let wt_path = tmp.path().join("pr-42");
    std::fs::create_dir_all(&wt_path).unwrap();
    let result = cleanup_pr_worktree(tmp.path(), &wt_path, 42, true, "test", None);
    assert!(result.is_ok());
    assert!(wt_path.exists(), "dry-run must not remove anything");
}

#[test]
fn current_branch_returns_none_for_a_non_git_directory() {
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(current_branch(tmp.path()), None);
}

/// The `pr-<N>` path is the only one whose branch name comes from outside
/// Loom, so it is the only one that needs a protected-name floor before
/// `git branch -D`.
#[test]
fn integration_branches_are_never_deletion_candidates() {
    for protected in [
        // The original four.
        "main",
        "master",
        "develop",
        "trunk",
        // Widened by the #5939 review — an allowlist of four names left
        // `staging`, `release/1.x` and `gh-pages` force-deletable.
        "development",
        "staging",
        "stage",
        "production",
        "prod",
        "gh-pages",
        "release/1.x",
        "releases/2026-08",
        "hotfix/2.3",
        "support/1.0",
        "maint/4",
        "stable/v2",
    ] {
        assert!(is_protected_branch_name(protected), "{protected}");
    }
    for deletable in [
        "feature/issue-5014",
        "docs/guide-update-20260810-005516",
        "hygiene/repo-all-20260731",
        "main-thing",
        "staging-area",
        "fix/release-notes",
    ] {
        assert!(!is_protected_branch_name(deletable), "{deletable}");
    }
}

/// A `pr-<N>` worktree parked on an integration branch is still removed —
/// only the branch survives.
#[test]
#[serial_test::serial]
fn cleanup_pr_worktree_removes_the_worktree_but_spares_an_integration_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_root = tmp.path().canonicalize().unwrap();
    assert!(Command::new("git")
        .args(["init", "-q", "-b", "primary"])
        .current_dir(&repo_root)
        .status()
        .unwrap()
        .success());
    std::fs::write(repo_root.join("README.md"), "x").unwrap();
    assert!(Command::new("git")
        .args(["add", "."])
        .current_dir(&repo_root)
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .args([
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "init"
        ])
        .current_dir(&repo_root)
        .status()
        .unwrap()
        .success());

    // `develop` exists but is NOT the primary checkout's branch, so git's
    // own "checked out elsewhere" refusal would not save it.
    let wt_path = crate::worktree_root::worktree_root(&repo_root).join("pr-888");
    assert!(Command::new("git")
        .args(["worktree", "add", "-b", "develop"])
        .arg(&wt_path)
        .current_dir(&repo_root)
        .status()
        .unwrap()
        .success());
    std::fs::write(wt_path.join(".loom-managed"), "test").unwrap();

    let result = cleanup_pr_worktree(&repo_root, &wt_path, 888, false, "test", None);
    assert!(result.is_ok(), "{result:?}");
    assert!(!wt_path.exists(), "the worktree itself is still removed");

    let branches = Command::new("git")
        .args(["branch", "--list", "develop"])
        .current_dir(&repo_root)
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&branches.stdout).contains("develop"),
        "an integration branch must survive the pr-<N> worktree that held it"
    );
}

// --- tmux cleanup safety gates (#4890) -------------------------------

#[test]
fn tmux_safe_mode_skips_even_an_unattached_session() {
    // `--safe` is documented as "merged-PR-only mode", but a tmux session
    // has no PR association at all — the core of #4890 is that `--safe`
    // must not silently kill tmux sessions just because they are not
    // attached right now.
    assert_eq!(classify_tmux_session(true, false, false), TmuxDecision::SkipSafeMode);
}

#[test]
fn tmux_safe_mode_skips_an_attached_session_too() {
    assert_eq!(classify_tmux_session(true, true, false), TmuxDecision::SkipSafeMode);
}

#[test]
fn tmux_force_overrides_safe_mode() {
    // `--safe --force` together is the existing "trust me" combination
    // used elsewhere in this module (e.g. the grace-period/uncommitted
    // gates in `classify_worktree`).
    assert_eq!(classify_tmux_session(true, false, true), TmuxDecision::Kill);
}

#[test]
fn tmux_attached_session_skipped_outside_safe_mode() {
    // A live operator terminal (attached client) must never be killed
    // without an explicit opt-in, even in plain (non-`--safe`) mode.
    assert_eq!(classify_tmux_session(false, true, false), TmuxDecision::SkipAttached);
}

#[test]
fn tmux_force_overrides_attached_gate() {
    assert_eq!(classify_tmux_session(false, true, true), TmuxDecision::Kill);
}

#[test]
fn tmux_unattached_session_killed_by_default() {
    assert_eq!(classify_tmux_session(false, false, false), TmuxDecision::Kill);
}

#[test]
fn clear_stale_locks_no_dir_is_zero() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(clear_stale_spawn_loop_locks(dir.path(), true), 0);
}

// --- sweep-checkpoint transient pruning (#4450) ---------------------

const HOUR: u64 = 3600;

/// Build a checkpoint-dir fixture and return `(tempdir, checkpoint_dir)`.
fn sweep_fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let ckpt = sweep_checkpoint_dir(dir.path());
    std::fs::create_dir_all(&ckpt).unwrap();
    (dir, ckpt)
}

fn register_run(repo_root: &Path, run_id: &str, pid: u32) {
    let reg = sweep_run_registry_dir(repo_root);
    std::fs::create_dir_all(&reg).unwrap();
    std::fs::write(
        reg.join(format!("{run_id}.json")),
        format!(r#"{{"run_id": "{run_id}", "pid": {pid}, "timestamp": "now"}}"#),
    )
    .unwrap();
}

/// Run the pass with a clock advanced by `age_hours`, so every fixture
/// file reads as exactly that old without touching filesystem mtimes.
fn run_transients(
    repo_root: &Path,
    dry_run: bool,
    age_hours: u64,
    alive: &[u32],
    states: &[(u32, &str)],
) -> CleanupStats {
    let mut stats = CleanupStats::default();
    let alive: Vec<u32> = alive.to_vec();
    let states: Vec<(u32, String)> = states.iter().map(|(n, s)| (*n, (*s).to_string())).collect();
    let pid_alive = |pid: u32| alive.contains(&pid);
    let issue_state = |issue: u32| {
        states
            .iter()
            .find(|(n, _)| *n == issue)
            .map_or_else(|| "UNKNOWN".to_string(), |(_, s)| s.clone())
    };
    let env = SweepTransientEnv {
        now: SystemTime::now() + Duration::from_secs(age_hours * HOUR),
        min_age: Duration::from_secs(SWEEP_TRANSIENT_MIN_AGE_SECS),
        pid_alive: &pid_alive,
        issue_state: &issue_state,
    };
    clean_sweep_transients_with(repo_root, &mut stats, dry_run, &env);
    stats
}

#[test]
fn sweep_transients_missing_dir_is_a_noop() {
    let dir = tempfile::tempdir().unwrap();
    let stats = run_transients(dir.path(), false, 100, &[], &[]);
    assert_eq!(stats.cleaned_sweep_baselines, 0);
    assert_eq!(stats.cleaned_sweep_checkpoints, 0);
    assert_eq!(stats.errors, 0);
}

#[test]
fn sweep_transients_prunes_orphan_baseline_past_threshold() {
    let (dir, ckpt) = sweep_fixture();
    let orphan = ckpt.join("main-clean-baseline-sweep-dead.txt");
    std::fs::write(&orphan, "").unwrap();
    let stats = run_transients(dir.path(), false, 100, &[], &[]);
    assert!(!orphan.exists());
    assert_eq!(stats.cleaned_sweep_baselines, 1);
}

#[test]
fn sweep_transients_keeps_young_orphan_baseline() {
    let (dir, ckpt) = sweep_fixture();
    let young = ckpt.join("main-clean-baseline-sweep-dead.txt");
    std::fs::write(&young, "").unwrap();
    let stats = run_transients(dir.path(), false, 1, &[], &[]);
    assert!(young.exists(), "mtime guard must spare a young baseline");
    assert_eq!(stats.cleaned_sweep_baselines, 0);
    assert_eq!(stats.kept_sweep_transients, 1);
}

#[test]
fn sweep_transients_keep_live_run_baseline_regardless_of_age() {
    let (dir, ckpt) = sweep_fixture();
    let live = ckpt.join("main-clean-baseline-sweep-live.txt");
    std::fs::write(&live, "").unwrap();
    register_run(dir.path(), "sweep-live", 4242);
    // 1000h old, but the registered PID is alive.
    let stats = run_transients(dir.path(), false, 1000, &[4242], &[]);
    assert!(live.exists(), "a live run's baseline must never be pruned");
    assert_eq!(stats.cleaned_sweep_baselines, 0);
    assert_eq!(stats.kept_sweep_transients, 1);
}

/// #4691: a run whose PID exists but cannot be signalled by this process
/// (`kill(2)` → `EPERM`) is LIVE. Wiring the real
/// [`crate::sweep_registry::is_pid_alive_with`] decision core in here — with
/// only the raw syscall mocked — proves the production `pid_alive` closure,
/// not just the test double, keeps such a baseline.
#[cfg(unix)]
#[test]
fn sweep_transients_keep_baseline_of_unsignallable_but_live_run() {
    use crate::sweep_registry::{is_pid_alive_with, EPERM};

    let (dir, ckpt) = sweep_fixture();
    let baseline = ckpt.join("main-clean-baseline-sweep-eperm.txt");
    std::fs::write(&baseline, "").unwrap();
    register_run(dir.path(), "sweep-eperm", 4242);

    let pid_alive = |pid: u32| is_pid_alive_with(pid, |_| Err(EPERM));
    let issue_state = |_: u32| "UNKNOWN".to_string();
    let mut stats = CleanupStats::default();
    let env = SweepTransientEnv {
        // 1000h old: only the liveness verdict can spare it.
        now: SystemTime::now() + Duration::from_secs(1000 * HOUR),
        min_age: Duration::from_secs(SWEEP_TRANSIENT_MIN_AGE_SECS),
        pid_alive: &pid_alive,
        issue_state: &issue_state,
    };
    clean_sweep_transients_with(dir.path(), &mut stats, false, &env);

    assert!(
        baseline.exists(),
        "an unsignallable (EPERM) PID means the sweep is still running — \
             its baseline must not be pruned"
    );
    assert_eq!(stats.cleaned_sweep_baselines, 0);
    assert_eq!(stats.kept_sweep_transients, 1);
}

/// The ESRCH counterpart of the test above: the same wiring, but the raw
/// syscall reports "no such process" — the one failure mode that really does
/// authorize pruning. Guards against an over-broad #4691 fix that makes
/// every `kill(2)` failure mean "alive" and silently reinstates the leak.
#[cfg(unix)]
#[test]
fn sweep_transients_still_prune_baseline_when_pid_is_esrch() {
    use crate::sweep_registry::is_pid_alive_with;
    const ESRCH: i32 = 3;

    let (dir, ckpt) = sweep_fixture();
    let baseline = ckpt.join("main-clean-baseline-sweep-gone.txt");
    std::fs::write(&baseline, "").unwrap();
    register_run(dir.path(), "sweep-gone", 4242);

    let pid_alive = |pid: u32| is_pid_alive_with(pid, |_| Err(ESRCH));
    let issue_state = |_: u32| "UNKNOWN".to_string();
    let mut stats = CleanupStats::default();
    let env = SweepTransientEnv {
        now: SystemTime::now() + Duration::from_secs(1000 * HOUR),
        min_age: Duration::from_secs(SWEEP_TRANSIENT_MIN_AGE_SECS),
        pid_alive: &pid_alive,
        issue_state: &issue_state,
    };
    clean_sweep_transients_with(dir.path(), &mut stats, false, &env);

    assert!(!baseline.exists(), "ESRCH means gone — prune must still fire");
    assert_eq!(stats.cleaned_sweep_baselines, 1);
}

#[test]
fn sweep_transients_prunes_registered_but_dead_pid_baseline() {
    let (dir, ckpt) = sweep_fixture();
    let dead = ckpt.join("main-clean-baseline-sweep-crashed.txt");
    std::fs::write(&dead, "").unwrap();
    // Registry entry survives a SIGKILL — the PID liveness check is what
    // distinguishes it from a running sweep.
    register_run(dir.path(), "sweep-crashed", 999_999);
    let stats = run_transients(dir.path(), false, 100, &[], &[]);
    assert!(!dead.exists());
    assert_eq!(stats.cleaned_sweep_baselines, 1);
}

#[test]
fn sweep_transients_keeps_baseline_with_unparseable_registry_entry() {
    let (dir, ckpt) = sweep_fixture();
    let path = ckpt.join("main-clean-baseline-sweep-corrupt.txt");
    std::fs::write(&path, "").unwrap();
    let reg = sweep_run_registry_dir(dir.path());
    std::fs::create_dir_all(&reg).unwrap();
    std::fs::write(reg.join("sweep-corrupt.json"), "{not json").unwrap();
    let stats = run_transients(dir.path(), false, 100, &[], &[]);
    assert!(path.exists(), "corrupt registry entry must fail safe (keep)");
    assert_eq!(stats.cleaned_sweep_baselines, 0);
}

#[test]
fn sweep_transients_removes_legacy_unkeyed_baselines() {
    let (dir, ckpt) = sweep_fixture();
    let legacy = ckpt.join("main-clean-baseline.txt");
    std::fs::write(&legacy, "").unwrap();
    let older = dir.path().join(".loom").join("main-clean-baseline.txt");
    std::fs::write(&older, "").unwrap();
    // Age 0: the legacy files have no owner, so the threshold does not apply.
    let stats = run_transients(dir.path(), false, 0, &[], &[]);
    assert!(!legacy.exists());
    assert!(!older.exists());
    assert_eq!(stats.cleaned_sweep_baselines, 2);
}

#[test]
fn sweep_transients_ignores_unrelated_files() {
    let (dir, ckpt) = sweep_fixture();
    let other = ckpt.join("notes.txt");
    std::fs::write(&other, "").unwrap();
    let weird = ckpt.join("main-clean-baseline-sweep-x.json");
    std::fs::write(&weird, "").unwrap();
    let stats = run_transients(dir.path(), false, 1000, &[], &[]);
    assert!(other.exists());
    assert!(weird.exists());
    assert_eq!(stats.cleaned_sweep_baselines, 0);
    assert_eq!(stats.cleaned_sweep_checkpoints, 0);
}

#[test]
fn sweep_transients_dry_run_deletes_nothing_but_counts() {
    let (dir, ckpt) = sweep_fixture();
    let baseline = ckpt.join("main-clean-baseline-sweep-dead.txt");
    std::fs::write(&baseline, "").unwrap();
    let legacy = ckpt.join("main-clean-baseline.txt");
    std::fs::write(&legacy, "").unwrap();
    let checkpoint = ckpt.join("issue-3784.json");
    std::fs::write(&checkpoint, "{}").unwrap();
    let stats = run_transients(dir.path(), true, 100, &[], &[(3784, "CLOSED")]);
    assert!(baseline.exists());
    assert!(legacy.exists());
    assert!(checkpoint.exists());
    assert_eq!(stats.cleaned_sweep_baselines, 2);
    assert_eq!(stats.cleaned_sweep_checkpoints, 1);
}

#[test]
fn sweep_transients_prunes_closed_issue_checkpoint_only() {
    let (dir, ckpt) = sweep_fixture();
    let closed = ckpt.join("issue-3784.json");
    let open = ckpt.join("issue-4450.json");
    let unknown = ckpt.join("issue-4451.json");
    for p in [&closed, &open, &unknown] {
        std::fs::write(p, "{}").unwrap();
    }
    let stats = run_transients(dir.path(), false, 100, &[], &[(3784, "CLOSED"), (4450, "OPEN")]);
    assert!(!closed.exists());
    assert!(open.exists(), "OPEN issue checkpoint must be kept");
    assert!(unknown.exists(), "an unverified issue state must never delete");
    assert_eq!(stats.cleaned_sweep_checkpoints, 1);
    assert_eq!(stats.kept_sweep_transients, 2);
}

#[test]
fn sweep_transients_keeps_young_closed_issue_checkpoint() {
    let (dir, ckpt) = sweep_fixture();
    let closed = ckpt.join("issue-3784.json");
    std::fs::write(&closed, "{}").unwrap();
    let stats = run_transients(dir.path(), false, 1, &[], &[(3784, "CLOSED")]);
    assert!(closed.exists(), "age gate also bounds forge probes");
    assert_eq!(stats.cleaned_sweep_checkpoints, 0);
}

#[test]
fn sweep_transients_keeps_checkpoint_of_in_flight_sweep() {
    let (dir, ckpt) = sweep_fixture();
    let inflight = ckpt.join("issue-3784.json");
    std::fs::write(&inflight, "{}").unwrap();
    // A daemon-owned sweep holds a claim lock for this issue.
    std::fs::create_dir_all(super::super::liveness::locks_dir(dir.path()).join("issue-3784"))
        .unwrap();
    let stats = run_transients(dir.path(), false, 1000, &[], &[(3784, "CLOSED")]);
    assert!(
        inflight.exists(),
        "an in-flight sweep's checkpoint must survive even when its issue is CLOSED"
    );
    assert_eq!(stats.cleaned_sweep_checkpoints, 0);
    assert_eq!(stats.kept_sweep_transients, 1);
}

// --- `.loom/logs/*.log` retention (#6655) -----------------------------

#[test]
fn log_file_issue_number_extracts_known_naming_conventions() {
    assert_eq!(log_file_issue_number("sweep-issue-6655.log"), Some(6655));
    assert_eq!(log_file_issue_number("loom-daemon-sweep-issue-4275-1785379543.log"), Some(4275));
    assert_eq!(log_file_issue_number("issue-123-shepherd.log"), Some(123));
}

#[test]
fn log_file_issue_number_none_for_singleton_logs() {
    assert_eq!(log_file_issue_number("role-judge.log"), None);
    assert_eq!(log_file_issue_number("guard-decisions.log"), None);
    assert_eq!(log_file_issue_number("daemon-start.log"), None);
    assert_eq!(log_file_issue_number("hook-errors.log"), None);
    assert_eq!(log_file_issue_number("main-quarantine.log"), None);
    assert_eq!(log_file_issue_number("worktree-removals.log"), None);
}

#[test]
#[serial_test::serial]
fn resolve_log_retention_days_env_overrides_config_and_default() {
    std::env::remove_var(LOG_RETENTION_DAYS_ENV);
    let dir = tempfile::tempdir().unwrap();

    // No config, no env -> built-in default.
    assert_eq!(resolve_log_retention_days(dir.path()), DEFAULT_LOG_RETENTION_DAYS);

    // Config sets a value -> config wins over the default.
    std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
    std::fs::write(dir.path().join(".loom/config.json"), r#"{"logs":{"retentionDays":10}}"#)
        .unwrap();
    assert_eq!(resolve_log_retention_days(dir.path()), 10);

    // Env set -> env wins over config.
    std::env::set_var(LOG_RETENTION_DAYS_ENV, "3");
    assert_eq!(resolve_log_retention_days(dir.path()), 3);

    // A non-positive/malformed env value falls through to config, not to
    // "disabled".
    std::env::set_var(LOG_RETENTION_DAYS_ENV, "0");
    assert_eq!(resolve_log_retention_days(dir.path()), 10);
    std::env::set_var(LOG_RETENTION_DAYS_ENV, "not-a-number");
    assert_eq!(resolve_log_retention_days(dir.path()), 10);

    std::env::remove_var(LOG_RETENTION_DAYS_ENV);
}

/// Build a `.loom/logs/` fixture and return `(tempdir, logs_dir)`.
fn logs_fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let logs = dir.path().join(".loom").join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    (dir, logs)
}

/// Run [`clean_log_files_with`] with a clock advanced by `age_hours`, a
/// retention window of `retention_days`, and the given live-issue /
/// checkpoint fixtures — mirrors `run_transients` above.
fn run_log_retention(
    repo_root: &Path,
    dry_run: bool,
    age_hours: u64,
    retention_days: u64,
    live: &[u32],
    checkpointed: &[u32],
) -> CleanupStats {
    let mut stats = CleanupStats::default();
    let live_issues: std::collections::HashSet<u32> = live.iter().copied().collect();
    let checkpointed: Vec<u32> = checkpointed.to_vec();
    let checkpoint_exists = |issue: u32| checkpointed.contains(&issue);
    let env = LogRetentionEnv {
        now: SystemTime::now() + Duration::from_secs(age_hours * HOUR),
        retention: Duration::from_secs(retention_days * 24 * HOUR),
        live_issues: &live_issues,
        checkpoint_exists: &checkpoint_exists,
    };
    clean_log_files_with(repo_root, &mut stats, dry_run, &env);
    stats
}

#[test]
fn log_retention_missing_dir_is_a_noop() {
    let dir = tempfile::tempdir().unwrap();
    let stats = run_log_retention(dir.path(), false, 1000, 30, &[], &[]);
    assert_eq!(stats.cleaned_log_files, 0);
    assert_eq!(stats.errors, 0);
}

#[test]
fn log_retention_prunes_stale_issue_log_past_threshold() {
    let (dir, logs) = logs_fixture();
    let stale = logs.join("sweep-issue-1111.log");
    std::fs::write(&stale, "log\n").unwrap();
    let stats = run_log_retention(dir.path(), false, 31 * 24, 30, &[], &[]);
    assert!(!stale.exists());
    assert_eq!(stats.cleaned_log_files, 1);
}

#[test]
fn log_retention_keeps_young_issue_log() {
    let (dir, logs) = logs_fixture();
    let young = logs.join("sweep-issue-2222.log");
    std::fs::write(&young, "log\n").unwrap();
    let stats = run_log_retention(dir.path(), false, 1, 30, &[], &[]);
    assert!(young.exists(), "mtime guard must spare a young log");
    assert_eq!(stats.cleaned_log_files, 0);
    assert_eq!(stats.kept_log_files, 1);
}

#[test]
fn log_retention_keeps_live_sweep_log_regardless_of_age() {
    let (dir, logs) = logs_fixture();
    let live = logs.join("sweep-issue-3333.log");
    std::fs::write(&live, "log\n").unwrap();
    let stats = run_log_retention(dir.path(), false, 1000 * 24, 30, &[3333], &[]);
    assert!(live.exists(), "a live sweep's log must never be pruned");
    assert_eq!(stats.cleaned_log_files, 0);
    assert_eq!(stats.kept_log_files, 1);
}

#[test]
fn log_retention_keeps_checkpointed_log_regardless_of_age() {
    let (dir, logs) = logs_fixture();
    let checkpointed = logs.join("loom-daemon-sweep-issue-4444-1785379543.log");
    std::fs::write(&checkpointed, "log\n").unwrap();
    let stats = run_log_retention(dir.path(), false, 1000 * 24, 30, &[], &[4444]);
    assert!(checkpointed.exists(), "a resumable checkpoint's log must never be pruned");
    assert_eq!(stats.cleaned_log_files, 0);
    assert_eq!(stats.kept_log_files, 1);
}

#[test]
fn log_retention_never_touches_singleton_logs_regardless_of_age() {
    let (dir, logs) = logs_fixture();
    let role_log = logs.join("role-judge.log");
    std::fs::write(&role_log, "log\n").unwrap();
    let stats = run_log_retention(dir.path(), false, 1000 * 24, 30, &[], &[]);
    assert!(role_log.exists(), "singleton/accumulator logs are out of scope for this pass");
    assert_eq!(stats.cleaned_log_files, 0);
    assert_eq!(stats.kept_log_files, 0);
}

#[test]
fn log_retention_never_recurses_into_subdirectories() {
    let (dir, logs) = logs_fixture();
    let dated = logs.join("2026-01-01");
    std::fs::create_dir_all(&dated).unwrap();
    let nested = dated.join("issue-5555-shepherd.log");
    std::fs::write(&nested, "log\n").unwrap();
    let stats = run_log_retention(dir.path(), false, 1000 * 24, 30, &[], &[]);
    assert!(nested.exists(), "dated archive subdirectories belong to archive-logs.sh");
    assert_eq!(stats.cleaned_log_files, 0);
}

#[test]
fn log_retention_dry_run_lists_without_deleting() {
    let (dir, logs) = logs_fixture();
    let stale = logs.join("sweep-issue-6666.log");
    std::fs::write(&stale, "log\n").unwrap();
    let stats = run_log_retention(dir.path(), true, 31 * 24, 30, &[], &[]);
    assert!(stale.exists(), "--dry-run must never delete");
    assert_eq!(stats.cleaned_log_files, 1);
}

// --- actionable error reporting (#4877) ------------------------------

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "git {args:?} failed in {}", dir.display());
}

/// A recorded error must name the target, the operation, and the cause —
/// the three things `Errors: 1` on its own withholds.
#[test]
fn record_error_names_target_operation_and_cause() {
    let mut stats = CleanupStats::default();
    stats.record_error("branch feature/issue-42", "git branch -D", "error: branch not found");
    assert_eq!(stats.errors, 1);
    assert_eq!(
        stats.error_details,
        vec![
            "git branch -D failed for branch feature/issue-42: error: branch not found".to_string()
        ]
    );
}

#[test]
fn error_line_without_a_cause_still_names_target_and_operation() {
    assert_eq!(
        error_line("/tmp/wt", "git worktree remove --force", "  "),
        "git worktree remove --force failed for /tmp/wt"
    );
}

/// The reported original symptom: a failed `git branch -D` bumped the
/// counter and printed nothing. The failure must now surface a diagnostic
/// naming the branch *and* git's own message.
#[test]
fn failed_branch_delete_reports_branch_and_git_error() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);

    let branch = "feature/issue-4877";
    let cause = force_delete_branch(dir.path(), branch)
        .expect_err("deleting a nonexistent branch must fail");

    let mut stats = CleanupStats::default();
    stats.record_error(&format!("branch {branch}"), "git branch -D", &cause);

    assert_eq!(stats.errors, 1);
    let detail = &stats.error_details[0];
    assert!(detail.contains(branch), "diagnostic must name the branch: {detail}");
    assert!(detail.contains("git branch -D"), "diagnostic must name the operation: {detail}");
    assert!(
        detail.to_lowercase().contains("not found"),
        "diagnostic must carry git's underlying error: {detail}"
    );
}

/// A successful `git branch -D` must stay silent (no error inflation).
#[test]
fn successful_branch_delete_records_no_error() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "--initial-branch=main"]);
    git(dir.path(), &["config", "user.email", "loom@example.com"]);
    git(dir.path(), &["config", "user.name", "Loom Test"]);
    git(dir.path(), &["commit", "-q", "--allow-empty", "-m", "seed"]);
    git(dir.path(), &["branch", "feature/issue-4877"]);

    assert!(force_delete_branch(dir.path(), "feature/issue-4877").is_ok());
}

// --- `--safe --branches-only` reachability gate (#5737) --------------

#[test]
fn classify_stale_branch_outside_safe_mode_always_removes() {
    // Pre-#5737 behavior: absence of a tracking branch is sufficient on
    // its own outside `--safe`, regardless of reachability/PR status.
    assert_eq!(classify_stale_branch(false, false, false), StaleBranchDecision::Remove);
    assert_eq!(classify_stale_branch(false, true, true), StaleBranchDecision::Remove);
}

#[test]
fn classify_stale_branch_safe_mode_requires_reachability_or_merged_pr() {
    assert_eq!(classify_stale_branch(true, true, false), StaleBranchDecision::Remove);
    assert_eq!(classify_stale_branch(true, false, true), StaleBranchDecision::Remove);
}

#[test]
fn classify_stale_branch_safe_mode_keeps_truly_unreachable_work() {
    // The #5737 repro: no remote ref holds these commits and no PR
    // merged them - deleting would destroy the only copy.
    assert_eq!(classify_stale_branch(true, false, false), StaleBranchDecision::KeepUnreachable);
}

#[test]
fn retained_prefix_matches_backup_and_preserve() {
    assert_eq!(retained_prefix("backup/issue-4749-doctor-rebase"), Some("backup/"));
    assert_eq!(retained_prefix("preserve-bf0d1b83-version-bump"), Some("preserve-"));
    assert_eq!(retained_prefix("feature/issue-42"), None);
}

fn local_branch_names(repo_root: &Path) -> Vec<String> {
    String::from_utf8_lossy(
        &Command::new("git")
            .args(["branch", "--format=%(refname:short)"])
            .current_dir(repo_root)
            .output()
            .unwrap()
            .stdout,
    )
    .lines()
    .map(str::to_string)
    .collect()
}

/// Regression lock for issue #5737's own repro + suggested AC: a
/// local-only branch holding an unpushed commit and no tracking branch
/// must survive `--safe --branches-only`, while a branch whose commits
/// are already reachable via another remote ref (the "PR merged, remote
/// auto-deleted" shape) is still deleted.
#[test]
fn safe_branch_cleanup_keeps_unpushed_work_but_deletes_reachable_stale_branch() {
    let origin_dir = tempfile::tempdir().unwrap();
    git(origin_dir.path(), &["init", "-q", "--bare"]);

    let repo_dir = tempfile::tempdir().unwrap();
    let repo_root = repo_dir.path();
    git(repo_root, &["init", "-q", "--initial-branch=main"]);
    git(repo_root, &["config", "user.email", "loom@example.com"]);
    git(repo_root, &["config", "user.name", "Loom Test"]);
    git(repo_root, &["commit", "-q", "--allow-empty", "-m", "seed"]);
    git(
        repo_root,
        &[
            "remote",
            "add",
            "origin",
            origin_dir.path().to_str().unwrap(),
        ],
    );
    git(repo_root, &["push", "-q", "origin", "main"]);

    // A branch whose tip is already reachable via origin/main (e.g. its
    // own remote branch was auto-deleted after a merge) - no NEW
    // commits, so deleting it loses nothing.
    git(repo_root, &["branch", "already-landed"]);

    // A branch holding a genuinely unpushed commit: no tracking branch,
    // AND its commit is reachable from no remote ref at all. This is
    // the #5737 repro - it must survive `--safe`.
    git(repo_root, &["checkout", "-q", "-b", "unpushed-work"]);
    git(repo_root, &["commit", "-q", "--allow-empty", "-m", "never pushed"]);
    git(repo_root, &["checkout", "-q", "main"]);

    let mut stats = CleanupStats::default();
    let opts = CleanOptions {
        safe: true,
        ..CleanOptions::default()
    };
    clean_branches(repo_root, &mut stats, &opts);

    let remaining = local_branch_names(repo_root);
    assert!(
        !remaining.contains(&"already-landed".to_string()),
        "a branch already reachable from another remote ref must still be deleted: {remaining:?}"
    );
    assert!(
        remaining.contains(&"unpushed-work".to_string()),
        "unpushed work with no remote ref anywhere must survive --safe: {remaining:?}"
    );
    assert_eq!(stats.cleaned_branches, 1);
    assert_eq!(stats.kept_branches, 1);
}

/// `backup/`/`preserve-` prefixed branches are retained by default under
/// `--safe`, even without any reachability computation needed to save
/// them.
#[test]
fn safe_branch_cleanup_retains_backup_prefixed_branch_by_default() {
    let repo_dir = tempfile::tempdir().unwrap();
    let repo_root = repo_dir.path();
    git(repo_root, &["init", "-q", "--initial-branch=main"]);
    git(repo_root, &["config", "user.email", "loom@example.com"]);
    git(repo_root, &["config", "user.name", "Loom Test"]);
    git(repo_root, &["commit", "-q", "--allow-empty", "-m", "seed"]);
    git(repo_root, &["branch", "backup/issue-4749-doctor-rebase"]);

    let mut stats = CleanupStats::default();
    let opts = CleanOptions {
        safe: true,
        ..CleanOptions::default()
    };
    clean_branches(repo_root, &mut stats, &opts);

    let remaining = local_branch_names(repo_root);
    assert!(
        remaining.contains(&"backup/issue-4749-doctor-rebase".to_string()),
        "backup/ prefix must be retained under --safe: {remaining:?}"
    );
    assert_eq!(stats.kept_branches, 1);
    assert_eq!(stats.cleaned_branches, 0);
}

/// `--safe --force` (the same "trust me" combination used elsewhere in
/// this module) overrides the retain-prefix gate specifically - proven
/// by pairing the prefix with commits that ARE independently reachable
/// (so the underlying safety net alone would already permit removal;
/// only the prefix gate is what changes with `--force`).
#[test]
fn safe_force_overrides_retain_prefix_gate() {
    let origin_dir = tempfile::tempdir().unwrap();
    git(origin_dir.path(), &["init", "-q", "--bare"]);

    let repo_dir = tempfile::tempdir().unwrap();
    let repo_root = repo_dir.path();
    git(repo_root, &["init", "-q", "--initial-branch=main"]);
    git(repo_root, &["config", "user.email", "loom@example.com"]);
    git(repo_root, &["config", "user.name", "Loom Test"]);
    git(repo_root, &["commit", "-q", "--allow-empty", "-m", "seed"]);
    git(
        repo_root,
        &[
            "remote",
            "add",
            "origin",
            origin_dir.path().to_str().unwrap(),
        ],
    );
    git(repo_root, &["push", "-q", "origin", "main"]);
    // Same tip as origin/main -> reachable from a remote ref.
    git(repo_root, &["branch", "backup/reachable-but-prefixed"]);

    // Without --force: the prefix gate wins even though the branch is
    // independently reachable.
    let mut stats = CleanupStats::default();
    let opts = CleanOptions {
        safe: true,
        ..CleanOptions::default()
    };
    clean_branches(repo_root, &mut stats, &opts);
    assert!(
        local_branch_names(repo_root).contains(&"backup/reachable-but-prefixed".to_string()),
        "prefix gate must retain the branch without --force"
    );
    assert_eq!(stats.kept_branches, 1);

    // With --force: the prefix gate no longer applies, and the branch's
    // own reachability already permits removal.
    let mut stats = CleanupStats::default();
    let opts = CleanOptions {
        safe: true,
        force: true,
        ..CleanOptions::default()
    };
    clean_branches(repo_root, &mut stats, &opts);
    assert!(
        !local_branch_names(repo_root).contains(&"backup/reachable-but-prefixed".to_string()),
        "--force must override the retain-prefix gate"
    );
    assert_eq!(stats.cleaned_branches, 1);
}

/// AC #3: deletion output must print the branch SHA (matching the
/// worktree half's `HEAD=<sha> (recoverable via `git reflog`)` hint).
#[test]
fn branch_sha_and_hint_render_the_head_commit() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "--initial-branch=main"]);
    git(dir.path(), &["config", "user.email", "loom@example.com"]);
    git(dir.path(), &["config", "user.name", "Loom Test"]);
    git(dir.path(), &["commit", "-q", "--allow-empty", "-m", "seed"]);

    let sha = branch_sha(dir.path(), "main").expect("HEAD must resolve");
    assert_eq!(sha.len(), 12, "short SHA must be 12 chars: {sha}");

    let hint = sha_hint(dir.path(), "main");
    assert!(hint.contains(&sha), "hint must carry the SHA: {hint}");
    assert!(hint.contains("recoverable via"), "hint must name the recovery path: {hint}");
    assert!(hint.contains("git reflog"), "hint must name git reflog: {hint}");
}

#[test]
fn branch_sha_is_none_for_a_nonexistent_branch() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    assert!(branch_sha(dir.path(), "does-not-exist").is_none());
    assert_eq!(sha_hint(dir.path(), "does-not-exist"), "");
}

/// AC #3: a run that recorded errors must not read like a clean one.
#[test]
fn completion_line_differs_when_errors_occurred() {
    let clean_run = completion_line("Cleanup", false, 0);
    let errored_run = completion_line("Cleanup", false, 1);
    assert_eq!(clean_run, "Cleanup complete!");
    assert_ne!(clean_run, errored_run);
    assert!(errored_run.contains('1'), "closing line must carry the count: {errored_run}");
    assert!(errored_run.contains("error"), "closing line must say error: {errored_run}");
    // Plural agreement, and the same rule for aggressive mode.
    assert!(completion_line("Cleanup", false, 2).contains("2 errors"));
    assert!(completion_line("Aggressive cleanup", false, 0).starts_with("Aggressive cleanup"));
    assert_ne!(
        completion_line("Aggressive cleanup", false, 0),
        completion_line("Aggressive cleanup", false, 3)
    );
}

/// A dry run that hit errors (e.g. an unresolvable PR status) must also
/// read differently from a clean dry run.
#[test]
fn completion_line_dry_run_reflects_errors() {
    let clean_run = completion_line("Cleanup", true, 0);
    let errored_run = completion_line("Cleanup", true, 1);
    assert_eq!(clean_run, "Dry run complete - no changes made");
    assert_ne!(clean_run, errored_run);
    assert!(errored_run.contains("1 error"), "{errored_run}");
}

/// AC #4 regression lock: the exit status distinguishes "completed with
/// errors" from "completed cleanly".
#[test]
fn exit_code_is_nonzero_exactly_when_errors_occurred() {
    assert_eq!(exit_code(0), 0);
    assert_eq!(exit_code(1), 1);
    assert_eq!(exit_code(7), 1);
}

/// End-to-end lock on the clean-run contract: a pass with nothing to do
/// returns 0. Scoped to `worktrees_only` so the pass touches nothing
/// outside the temp repo (no tmux sockets, no branches).
#[test]
fn run_clean_returns_zero_when_no_errors_occurred() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    let opts = CleanOptions {
        force: true,
        worktrees_only: true,
        ..CleanOptions::default()
    };
    assert_eq!(run_clean(dir.path(), &opts), 0);
}

#[test]
fn clear_stale_locks_keeps_live_and_removes_dead() {
    let dir = tempfile::tempdir().unwrap();
    let locks = spawn_loop_locks_dir(dir.path());
    std::fs::create_dir_all(locks.join("issue-1")).unwrap();
    std::fs::create_dir_all(locks.join("issue-2")).unwrap();
    std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
    std::fs::write(
        dir.path().join(".loom").join("spawn-loop-state.json"),
        r#"{"running": [{"issue": 1, "pid": 1}]}"#,
    )
    .unwrap();
    let removed = clear_stale_spawn_loop_locks(dir.path(), false);
    assert_eq!(removed, 1);
    assert!(locks.join("issue-1").exists());
    assert!(!locks.join("issue-2").exists());
}

// --- quarantine_dirty_worktree (#6653) --------------------------------

fn init_repo_with_seed_commit(dir: &Path) {
    git(dir, &["init", "-q", "--initial-branch=main"]);
    git(dir, &["config", "user.email", "loom@example.com"]);
    git(dir, &["config", "user.name", "Loom Test"]);
    git(dir, &["commit", "-q", "--allow-empty", "-m", "seed"]);
}

#[test]
fn quarantine_dirty_worktree_stashes_uncommitted_and_untracked_changes() {
    let dir = tempfile::tempdir().unwrap();
    init_repo_with_seed_commit(dir.path());
    std::fs::write(dir.path().join("tracked.txt"), "v1").unwrap();
    git(dir.path(), &["add", "tracked.txt"]);
    git(dir.path(), &["commit", "-q", "-m", "add tracked"]);
    std::fs::write(dir.path().join("tracked.txt"), "v2 (uncommitted)").unwrap();
    std::fs::write(dir.path().join("untracked.txt"), "new file").unwrap();

    let sha = quarantine_dirty_worktree(dir.path(), "issue=6653 reason=test")
        .expect("dirty worktree must be quarantined");
    assert!(!sha.trim().is_empty());

    // The working tree is clean again — the dirt moved into the stash.
    assert!(!check_uncommitted_or_untracked_changes(dir.path()));
    assert_eq!(std::fs::read_to_string(dir.path().join("tracked.txt")).unwrap(), "v1");
    assert!(!dir.path().join("untracked.txt").exists());

    // The stash carries the load-bearing `loom-quarantine:` label so the
    // existing stash_retirement / quarantine_stash_status machinery can
    // find it.
    let list = String::from_utf8_lossy(
        &Command::new("git")
            .args(["log", "-g", "--format=%gs", "refs/stash"])
            .current_dir(dir.path())
            .output()
            .unwrap()
            .stdout,
    )
    .to_string();
    assert!(list.contains("loom-quarantine: issue=6653 reason=test"), "{list}");
}

#[test]
fn quarantine_dirty_worktree_returns_none_when_nothing_to_stash() {
    let dir = tempfile::tempdir().unwrap();
    init_repo_with_seed_commit(dir.path());

    assert_eq!(quarantine_dirty_worktree(dir.path(), "issue=1 reason=test"), None);
}
