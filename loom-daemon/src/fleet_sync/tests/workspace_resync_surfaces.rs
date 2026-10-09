//! The workspace resync over the surfaces beyond the installer step
//! (#10895): they are exported from the default branch, diffed and committed
//! like any payload file, and a release that changes none of them still
//! writes nothing.

use super::*;

const CURRENT: Seed = Seed {
    version: "0.19.800",
    requires: Some("0.19.772"),
    current: true,
    ignore_bin: false,
};

const SKILL: &str = ".agents/skills/loom-builder/SKILL.md";

/// The release under test, plus a description for its one role, so the
/// payload generates an agent skill.
fn skilled_defaults(fx: &Fixture) -> PathBuf {
    let d = fx.tmp.path().canonicalize().unwrap().join("defaults-new");
    write(&d.join("roles/builder.json"), "{\"description\": \"Builds things\"}\n");
    d
}

/// A repo current on every surface except `.agents/skills/`, which it lacks.
fn repo_missing_only_its_skills(fx: &Fixture, defaults: &Path) {
    let payload = payload_from(defaults);
    fx.push_from_seed("install the release, minus its skills", |seed| {
        write(&seed.join(".claude/README.md"), "theirs\n");
        let outcome = resync_workspace_with(&payload, seed).unwrap();
        assert!(matches!(outcome, ResyncOutcome::Applied { .. }), "{outcome:?}");
        assert!(seed.join(SKILL).is_file(), "the payload generates the skill");
        fs::remove_dir_all(seed.join(".agents")).unwrap();
        // An older stamp that records no file, as on a repo no installer ever
        // gave the skills to: the daemon's resync has to record them itself.
        write(&seed.join(INSTALL_METADATA_PATH), &metadata(CURRENT.version, CURRENT.requires));
    });
}

#[test]
fn a_repo_whose_only_drift_is_its_skills_gets_exactly_one_commit() {
    let fx = Fixture::new(CURRENT);
    let defaults = skilled_defaults(&fx);
    repo_missing_only_its_skills(&fx, &defaults);
    let commits = fx.origin_commits();

    let host = Host::running(&fx, "host-a", RUNNING, defaults);
    let pass = host.pass();
    let report = only(&pass);
    assert_eq!(report.state, WState::W0, "{report:?}");
    assert_eq!(fx.origin_commits(), commits + 1, "one resync commit");
    assert!(fx.origin_file(SKILL).contains("name: loom-builder"));
    let mut changed: Vec<String> = git(
        &fx.origin,
        &[
            "diff",
            "--name-only",
            "refs/heads/main~1",
            "refs/heads/main",
        ],
    )
    .lines()
    .map(str::to_string)
    .collect();
    changed.sort();
    assert_eq!(changed, vec![SKILL.to_string(), INSTALL_METADATA_PATH.to_string()]);
    // The backfilled skill is recorded, so a later release can retire it.
    assert!(fx.origin_file(INSTALL_METADATA_PATH).contains(SKILL));
    // `.claude/README.md` is still "theirs" only because this fixture's
    // payload does not ship the file. The real payload does ship it, and a
    // resync overwrites an existing one (`Rule::CopyIfPresent`).
    assert_eq!(fx.origin_file(".claude/README.md"), "theirs");

    // The next tick, and the one after a restart, write nothing more.
    host.advance(INTERVAL);
    assert_eq!(only(&host.pass()).state, WState::W0);
    *host.memory.borrow_mut() = Memory::default();
    assert_eq!(only(&host.pass()).state, WState::W0);
    assert_eq!(fx.origin_commits(), commits + 1);
}

#[test]
fn a_release_that_changes_no_surface_makes_no_claim_and_no_commit() {
    let fx = Fixture::new(CURRENT);
    let defaults = skilled_defaults(&fx);
    let payload = payload_from(&defaults);
    fx.push_from_seed("install every surface of the release", |seed| {
        write(&seed.join(".claude/README.md"), "theirs\n");
        write(&seed.join(".loom/CLAUDE.md"), "no template in this payload\n");
        let outcome = resync_workspace_with(&payload, seed).unwrap();
        assert!(matches!(outcome, ResyncOutcome::Applied { .. }), "{outcome:?}");
    });
    let commits = fx.origin_commits();

    // A newer daemon whose payload is the same files: a daemon-only release.
    let host = Host::running(&fx, "host-a", "0.19.881", defaults);
    let pass = host.pass();
    let report = only(&pass);
    assert_eq!(report.state, WState::W0, "{report:?}");
    assert_eq!(fx.origin_commits(), commits, "no commit");
    assert!(fx.forge.calls.borrow().is_empty(), "no claim was asked for");
    assert!(host.resync_worktrees().is_empty());
}
