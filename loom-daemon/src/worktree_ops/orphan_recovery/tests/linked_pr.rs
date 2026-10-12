//! Orphan recovery against a fake forge: an open linked PR as liveness
//! evidence (#5511), lease-record freshness (#6286) and stale PR-side claim
//! recovery (#6167). Split out of `orphan_recovery.rs` when #9548 registered
//! these fixtures as writable checkouts (the label writers run the real
//! write-scope check), so the over-threshold parent shrinks rather than grows
//! (`.loom/docs/file-size-policy.md`).

use super::*;
// #9548: gate-reaching tests hold the default serial key; see `crate::write_scope_test_support`.

// ------------------------------------------------------------------
// #5511: an open linked PR is liveness evidence
// ------------------------------------------------------------------

/// A closes-graph GraphQL payload with the given node list.
fn closes_graph(nodes: &str) -> String {
    format!(
        r#"{{"data":{{"repository":{{"issue":{{"closedByPullRequestsReferences":{{"nodes":[{nodes}]}}}}}}}}}}"#
    )
}

/// Install a fake `gh` (via `LOOM_GH_BIN`, never `PATH` — see
/// [`super::gh`]'s `gh_bin`) that answers every call orphan recovery makes
/// for one stale `loom:building` issue (#5511's fixture):
///
/// - `issue list` -> exactly issue #5501, `loom:building`
/// - `api repos/.../events` -> a 2020 `loom:building` timestamp, so every
///   staleness threshold is comfortably exceeded
/// - `repo view` -> `rjwalters/loom` (owner/repo resolution for the probe)
/// - `api graphql` -> `graphql_payload` with exit `graphql_exit` (the
///   closes-graph open-linked-PR probe)
///
/// Returns a [`FakeGh`] guard that clears `LOOM_GH_BIN` on drop, carrying
/// the path of a log file recording every `gh` invocation so a test can
/// assert that `issue edit` was — or was NOT — reached.
///
/// Callers MUST be `#[serial(loom_config_env)]`, never bare `#[serial]` —
/// see the module-level test-isolation invariant (#8480).
#[cfg(unix)]
fn install_fake_gh(dir: &Path, graphql_payload: &str, graphql_exit: i32) -> FakeGh {
    use std::os::unix::fs::PermissionsExt;

    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let log = dir.join("gh-invocations.log");
    let script = format!(
        "#!/bin/sh\n\
         echo \"$@\" >> '{log}'\n\
         if [ \"$1\" = \"issue\" ] && [ \"$2\" = \"list\" ]; then\n\
         printf '%s' '[{{\"number\":5501,\"title\":\"live work\"}}]'\n\
         exit 0\n\
         fi\n\
         if [ \"$1\" = \"repo\" ]; then printf 'rjwalters/loom\\n'; exit 0; fi\n\
         if [ \"$1\" = \"api\" ] && [ \"$2\" = \"graphql\" ]; then\n\
         printf '%s' '{payload}'\n\
         exit {exit_code}\n\
         fi\n\
         case \"$*\" in */comments*) exit 0;; esac; if [ \"$1\" = \"api\" ]; then printf '2020-01-01T00:00:00Z\\n'; exit 0; fi\n\
         exit 0\n",
        log = log.display(),
        payload = graphql_payload,
        exit_code = graphql_exit,
    );
    let fake_gh = bin.join("gh");
    std::fs::write(&fake_gh, script).unwrap();
    std::fs::set_permissions(&fake_gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    // #9548: `dir` is a registered, writable checkout, and the `gh` the
    // label writers' write-scope check probes with (`LOOM_GH_BIN`) answers
    // for it with push, handing every other call to the fake above.
    let ws = WritableRoot::register_with_gh(dir, &fake_gh);
    std::env::set_var("LOOM_GH_BIN", &ws.gh);
    FakeGh { log, _ws: ws }
}

/// RAII guard for [`install_fake_gh`]: unsets `LOOM_GH_BIN` on drop so a
/// failing assertion cannot leak the fake into the next test.
struct FakeGh {
    log: PathBuf,
    _ws: WritableRoot,
}

impl FakeGh {
    /// Every `gh` invocation the fixture saw, one per line.
    fn calls(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

impl Drop for FakeGh {
    fn drop(&mut self) {
        std::env::remove_var("LOOM_GH_BIN");
    }
}

/// Evidence shaped like the #5501 incident: a liveness source exists (so the
/// #3651 short-circuit does not fire), but the issue is in none of the live
/// sets and has no journal record -> `no_spawn_loop_entry`, past threshold.
fn evidence_without_the_issue() -> LivenessEvidence {
    LivenessEvidence {
        available: true,
        sources: vec!["spawn-loop-state.json"],
        ..Default::default()
    }
}

/// (a) The #5501 regression: an issue whose only sign of life is an OPEN
/// linked PR must never be flagged orphaned, however stale its label and
/// however absent its claim lock / journal entry.
#[cfg(unix)]
#[test]
#[serial(loom_config_env)]
fn open_linked_pr_is_never_flagged_orphaned() {
    open_linked_pr_is_never_flagged_orphaned_body();
}

#[serial]
fn open_linked_pr_is_never_flagged_orphaned_body() {
    let dir = tempdir().unwrap();
    let _gh = install_fake_gh(dir.path(), &closes_graph(r#"{"number":5507,"state":"OPEN"}"#), 0);

    let mut result = OrphanRecoveryResult::default();
    check_untracked_building(&evidence_without_the_issue(), &mut result, dir.path(), 600, false);

    assert!(
        result.orphaned.is_empty(),
        "an issue with an open linked PR must not be orphaned: {:?}",
        result.orphaned
    );
    assert!(!result.assessment_failed());
}

/// (a, defense in depth) `recover_issue` is a `pub` entry point that can be
/// called without the flagging pass — it must refuse the label flip on its
/// own when an open linked PR exists.
#[cfg(unix)]
#[test]
#[serial(loom_config_env)]
fn recover_issue_refuses_to_reset_an_issue_with_an_open_linked_pr() {
    recover_issue_refuses_to_reset_an_issue_with_an_open_linked_pr_body();
}

#[serial]
fn recover_issue_refuses_to_reset_an_issue_with_an_open_linked_pr_body() {
    let dir = tempdir().unwrap();
    let gh = install_fake_gh(dir.path(), &closes_graph(r#"{"number":5507,"state":"OPEN"}"#), 0);

    let mut result = OrphanRecoveryResult::default();
    recover_issue(dir.path(), 5501, "no_spawn_loop_entry", &mut result, 600);

    let calls = gh.calls();
    assert!(
        !calls.contains("issue edit"),
        "recover_issue must not flip labels while a linked PR is open; gh calls:\n{calls}"
    );
    assert!(result.recovered.is_empty());
}

/// (b) Unchanged behavior: a verified absence of any linked PR still lets
/// recovery proceed exactly as before.
#[cfg(unix)]
#[test]
#[serial(loom_config_env)]
fn no_linked_pr_still_orphans_and_resets() {
    no_linked_pr_still_orphans_and_resets_body();
}

#[serial]
fn no_linked_pr_still_orphans_and_resets_body() {
    let dir = tempdir().unwrap();
    let gh = install_fake_gh(dir.path(), &closes_graph(""), 0);

    let mut result = OrphanRecoveryResult::default();
    check_untracked_building(&evidence_without_the_issue(), &mut result, dir.path(), 600, false);
    let mut recovery = OrphanRecoveryResult::default();
    recover_issue(dir.path(), 5501, "no_spawn_loop_entry", &mut recovery, 600);

    assert_eq!(result.orphaned.len(), 1, "{:?}", result.orphaned);
    assert_eq!(result.orphaned[0].issue, Some(5501));
    assert_eq!(result.orphaned[0].reason, "no_spawn_loop_entry");
    assert!(
        gh.calls().contains("issue edit"),
        "a verified absence of a linked PR must still reset the label"
    );
    assert!(recovery
        .recovered
        .iter()
        .any(|r| r.action == "reset_issue_label"));
}

/// (c) Fail-safe: a probe failure (forge outage / wedged `gh`) is NOT a
/// verified absence, so it must block the reset rather than greenlight it.
#[cfg(unix)]
#[test]
#[serial(loom_config_env)]
fn pr_probe_failure_blocks_recovery() {
    pr_probe_failure_blocks_recovery_body();
}

#[serial]
fn pr_probe_failure_blocks_recovery_body() {
    let dir = tempdir().unwrap();
    let gh = install_fake_gh(dir.path(), "gh: rate limit exceeded", 1);

    let mut result = OrphanRecoveryResult::default();
    check_untracked_building(&evidence_without_the_issue(), &mut result, dir.path(), 600, false);
    let mut recovery = OrphanRecoveryResult::default();
    recover_issue(dir.path(), 5501, "no_spawn_loop_entry", &mut recovery, 600);

    assert!(
        result.orphaned.is_empty(),
        "an unverifiable PR probe must fail toward ALIVE (#3651/#5511)"
    );
    assert!(
        !gh.calls().contains("issue edit"),
        "an unverifiable PR probe must not flip labels"
    );
    assert!(recovery.recovered.is_empty());
}

/// (d) Only `state == "OPEN"` counts. A MERGED PR still comes back from the
/// closes-graph even with `includeClosedPrs:false`, so treating any linked
/// PR as "open" would wedge orphan recovery forever on every issue whose PR
/// already merged.
#[cfg(unix)]
#[test]
#[serial(loom_config_env)]
fn merged_linked_pr_does_not_block_recovery() {
    merged_linked_pr_does_not_block_recovery_body();
}

#[serial]
fn merged_linked_pr_does_not_block_recovery_body() {
    let dir = tempdir().unwrap();
    let gh = install_fake_gh(dir.path(), &closes_graph(r#"{"number":5507,"state":"MERGED"}"#), 0);

    let mut result = OrphanRecoveryResult::default();
    check_untracked_building(&evidence_without_the_issue(), &mut result, dir.path(), 600, false);
    let mut recovery = OrphanRecoveryResult::default();
    recover_issue(dir.path(), 5501, "no_spawn_loop_entry", &mut recovery, 600);

    assert_eq!(
        result.orphaned.len(),
        1,
        "a MERGED linked PR is not an open PR: {:?}",
        result.orphaned
    );
    assert!(gh.calls().contains("issue edit"));
    assert!(recovery
        .recovered
        .iter()
        .any(|r| r.action == "reset_issue_label"));
}

/// [`install_fake_gh`] whose fake also serves the open-PR listing `rows`
/// (#10514 leg 0), and whose closes-graph arm refuses (exit 97).
#[cfg(unix)]
fn install_fake_gh_with_listing(
    dir: &Path,
    rows: &[crate::claim_reconciliation::open_pr_listing::test_support::Row],
) -> FakeGh {
    let gh = install_fake_gh(dir, "FORBIDDEN", 97);
    let fake = dir.join("bin").join("gh");
    let script = std::fs::read_to_string(&fake).unwrap();
    let (head, rest) = script.split_once('\n').unwrap();
    let (log_line, rest) = rest.split_once('\n').unwrap();
    let arm = crate::claim_reconciliation::open_pr_listing::test_support::pulls_arm(rows);
    std::fs::write(&fake, format!("{head}\n{log_line}\n{arm}{rest}")).unwrap();
    gh
}

/// (e) #10514: orphan recovery's probe answers from the open-PR listing
/// alone — a `Closes #5501` PR blocks the reset, an empty listing allows it,
/// and neither spawns the closes-graph nor walks the timeline.
#[cfg(unix)]
#[test]
#[serial(loom_config_env)]
fn the_open_pr_listing_answers_without_graphql() {
    the_open_pr_listing_answers_without_graphql_body();
}

#[serial]
fn the_open_pr_listing_answers_without_graphql_body() {
    use crate::claim_reconciliation::open_pr_listing::test_support::row;
    let linked = [row(5507, &[])
        .head("topic")
        .repo("rjwalters/loom")
        .body("Closes #5501")];
    for (rows, resets) in [(&linked[..], false), (&[][..], true)] {
        let dir = tempdir().unwrap();
        let gh = install_fake_gh_with_listing(dir.path(), rows);
        let mut recovery = OrphanRecoveryResult::default();
        recover_issue(dir.path(), 5501, "no_spawn_loop_entry", &mut recovery, 600);
        let calls = gh.calls();
        assert_eq!(calls.contains("issue edit"), resets, "{calls}");
        assert!(calls.contains("pulls?state=open"), "{calls}");
        assert!(!calls.contains("graphql") && !calls.contains("timeline"), "{calls}");
    }
}

/// #9548 negative control: the (b) fixture, but the credential only has
/// `pull`. The real write-scope check refuses the label writer, so the
/// reset never reaches `gh issue edit`.
#[cfg(unix)]
#[test]
#[serial(loom_config_env)]
fn a_read_only_credential_blocks_the_label_reset() {
    a_read_only_credential_blocks_the_label_reset_body();
}

#[serial]
fn a_read_only_credential_blocks_the_label_reset_body() {
    let dir = tempdir().unwrap();
    let gh = install_fake_gh(dir.path(), &closes_graph(""), 0);
    let read_only = WritableRoot::read_only(dir.path(), Some(&dir.path().join("bin/gh")));
    std::env::set_var("LOOM_GH_BIN", &read_only.gh);

    let mut recovery = OrphanRecoveryResult::default();
    recover_issue(dir.path(), 5501, "no_spawn_loop_entry", &mut recovery, 600);

    assert!(
        !gh.calls().contains("issue edit"),
        "a pull-only credential must not flip labels; gh calls:\n{}",
        gh.calls()
    );
    assert!(!recovery
        .recovered
        .iter()
        .any(|r| r.action == "reset_issue_label"));
}

// ------------------------------------------------------------------
// #6286 (Epic #6165 Phase 2): lease-record freshness in `recover-orphans`
// ------------------------------------------------------------------

/// Same shape as [`install_fake_gh`], but the REST comments endpoint
/// (`.../issues/<N>/comments`, [`gh::freshest_lease_updated_at`]) is
/// answered separately from the generic `api` fallback (`events` /
/// label-age lookup) so a test can control lease freshness independently
/// of label age. `lease_updated_at: None` emulates no lease comment at
/// all (empty stdout, matching the real `// empty` jq fallback).
#[cfg(unix)]
fn install_fake_gh_with_lease(
    dir: &Path,
    graphql_payload: &str,
    graphql_exit: i32,
    lease_updated_at: Option<&str>,
) -> FakeGh {
    use std::os::unix::fs::PermissionsExt;

    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let log = dir.join("gh-invocations.log");
    let lease_stdout = match lease_updated_at {
        Some(ts) => with_fleet_author(&format!("printf '%s' '{{\"updated_at\":\"{ts}\"}}'")),
        None => "true".to_string(),
    };
    let script = format!(
        "#!/bin/sh\n\
         echo \"$@\" >> '{log}'\n\
         if [ \"$1\" = \"issue\" ] && [ \"$2\" = \"list\" ]; then\n\
         printf '%s' '[{{\"number\":5501,\"title\":\"live work\"}}]'\n\
         exit 0\n\
         fi\n\
         if [ \"$1\" = \"repo\" ]; then printf 'rjwalters/loom\\n'; exit 0; fi\n\
         if [ \"$1\" = \"api\" ] && [ \"$2\" = \"graphql\" ]; then\n\
         printf '%s' '{payload}'\n\
         exit {exit_code}\n\
         fi\n\
         case \"$*\" in\n\
         */comments*)\n\
         {lease_stdout}\n\
         exit 0\n\
         ;;\n\
         esac\n\
         if [ \"$1\" = \"api\" ]; then printf '2020-01-01T00:00:00Z\\n'; exit 0; fi\n\
         exit 0\n",
        log = log.display(),
        payload = graphql_payload,
        exit_code = graphql_exit,
    );
    let fake_gh = bin.join("gh");
    std::fs::write(&fake_gh, script).unwrap();
    std::fs::set_permissions(&fake_gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    // #9548: `dir` is a registered, writable checkout, and the `gh` the
    // label writers' write-scope check probes with (`LOOM_GH_BIN`) answers
    // for it with push, handing every other call to the fake above.
    let ws = WritableRoot::register_with_gh(dir, &fake_gh);
    std::env::set_var("LOOM_GH_BIN", &ws.gh);
    FakeGh { log, _ws: ws }
}

/// Core #6286 regression: a fresh lease record must block orphaning even
/// though the #5511 open-PR gate has already verified there is no linked
/// PR — the last pre-existing gate that would otherwise let the reset
/// proceed.
#[cfg(unix)]
#[test]
#[serial(loom_config_env)]
fn fresh_lease_blocks_recovery_even_with_no_linked_pr() {
    fresh_lease_blocks_recovery_even_with_no_linked_pr_body();
}

#[serial]
fn fresh_lease_blocks_recovery_even_with_no_linked_pr_body() {
    let dir = tempdir().unwrap();
    // Renewed 2 minutes ago -- well within the 15-minute default TTL.
    let lease_ts = (chrono::Utc::now() - chrono::Duration::minutes(2)).to_rfc3339();
    let gh = install_fake_gh_with_lease(dir.path(), &closes_graph(""), 0, Some(&lease_ts));

    let mut result = OrphanRecoveryResult::default();
    check_untracked_building(&evidence_without_the_issue(), &mut result, dir.path(), 600, false);

    assert!(
        result.orphaned.is_empty(),
        "a fresh lease record must block orphaning despite the verified absence of a \
         linked PR (#6286): {:?}",
        result.orphaned
    );
    assert_eq!(result.watched.len(), 1, "{:?}", result.watched);
    assert_eq!(result.watched[0].issue, 5501);
    assert_eq!(result.watched[0].reason, "lease_fresh");
    assert!(
        !gh.calls().contains("issue edit"),
        "no label flip while the lease is fresh; gh calls:\n{}",
        gh.calls()
    );
}

/// The fail-safe must not become "never reclaim": a genuinely expired
/// lease must not block the pre-existing recovery flow.
#[cfg(unix)]
#[test]
#[serial(loom_config_env)]
fn expired_lease_does_not_block_recovery() {
    expired_lease_does_not_block_recovery_body();
}

#[serial]
fn expired_lease_does_not_block_recovery_body() {
    let dir = tempdir().unwrap();
    // Last renewed 30 minutes ago -- well past the 15-minute default TTL.
    let lease_ts = (chrono::Utc::now() - chrono::Duration::minutes(30)).to_rfc3339();
    let gh = install_fake_gh_with_lease(dir.path(), &closes_graph(""), 0, Some(&lease_ts));

    let mut result = OrphanRecoveryResult::default();
    check_untracked_building(&evidence_without_the_issue(), &mut result, dir.path(), 600, false);

    assert_eq!(result.orphaned.len(), 1, "{:?}", result.orphaned);
    assert_eq!(result.orphaned[0].issue, Some(5501));
    let mut recovery = OrphanRecoveryResult::default();
    recover_issue(dir.path(), 5501, "no_spawn_loop_entry", &mut recovery, 600);
    assert!(gh.calls().contains("issue edit"));
}

/// #10570: the #10161 shape seen from the RECOVERING host. An attended claim
/// made elsewhere leaves nothing on this host's disk (no claim file, no
/// journal, no spawn-loop entry) and has no PR yet; its only liveness signal
/// is the lease its deferred publisher put on the forge once a released
/// sweep's leftover lease aged out. While that lease is renewed the claim
/// survives every pass; once the session ends and the lease goes stale, the
/// same pass reclaims it under the unchanged gates.
#[cfg(unix)]
#[test]
#[serial(loom_config_env)]
fn a_remote_attended_lease_holds_the_claim_until_it_goes_stale() {
    a_remote_attended_lease_holds_the_claim_until_it_goes_stale_body();
}

#[serial]
fn a_remote_attended_lease_holds_the_claim_until_it_goes_stale_body() {
    use std::os::unix::fs::PermissionsExt;

    // A second repository root: nothing local vouches for the claim.
    let dir = tempdir().unwrap();
    assert!(!has_valid_claim(dir.path(), 5501));
    // The forge's view of the lease comment's `updated_at`, mutable per pass.
    let forge_lease = dir.path().join("forge-lease-updated-at");
    let set_lease_age = |minutes: i64| {
        let ts = (chrono::Utc::now() - chrono::Duration::minutes(minutes)).to_rfc3339();
        std::fs::write(&forge_lease, ts).unwrap();
    };
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let log = dir.path().join("gh-invocations.log");
    let lease_stdout = with_fleet_author(&format!(
        r#"printf '{{"updated_at":"%s"}}' "$(cat '{}')""#,
        forge_lease.display()
    ));
    let script = format!(
        "#!/bin/sh\n\
         echo \"$@\" >> '{log}'\n\
         if [ \"$1\" = \"issue\" ] && [ \"$2\" = \"list\" ]; then\n\
         printf '%s' '[{{\"number\":5501,\"title\":\"attended work\"}}]'\n\
         exit 0\n\
         fi\n\
         if [ \"$1\" = \"repo\" ]; then printf 'rjwalters/loom\\n'; exit 0; fi\n\
         if [ \"$1\" = \"api\" ] && [ \"$2\" = \"graphql\" ]; then\n\
         printf '%s' '{payload}'\n\
         exit 0\n\
         fi\n\
         case \"$*\" in */comments*) {lease_stdout}; exit 0;; esac\n\
         if [ \"$1\" = \"api\" ]; then printf '2020-01-01T00:00:00Z\\n'; exit 0; fi\n\
         exit 0\n",
        log = log.display(),
        payload = closes_graph(""),
    );
    let fake_gh = bin.join("gh");
    std::fs::write(&fake_gh, script).unwrap();
    std::fs::set_permissions(&fake_gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let ws = WritableRoot::register_with_gh(dir.path(), &fake_gh);
    std::env::set_var("LOOM_GH_BIN", &ws.gh);
    let gh = FakeGh { log, _ws: ws };

    // Published, then renewed every few minutes while the session lives.
    for age in [1, 4] {
        set_lease_age(age);
        let mut result = OrphanRecoveryResult::default();
        check_untracked_building(
            &evidence_without_the_issue(),
            &mut result,
            dir.path(),
            600,
            false,
        );
        assert!(result.orphaned.is_empty(), "lease {age}m old: {:?}", result.orphaned);
        assert_eq!(result.watched.len(), 1, "{:?}", result.watched);
        assert_eq!(result.watched[0].reason, "lease_fresh");
    }
    assert!(!gh.calls().contains("issue edit"), "{}", gh.calls());

    // The session ended; renewal stopped and the lease aged past its TTL.
    set_lease_age(30);
    let mut result = OrphanRecoveryResult::default();
    check_untracked_building(&evidence_without_the_issue(), &mut result, dir.path(), 600, false);
    assert_eq!(result.orphaned.len(), 1, "{:?}", result.orphaned);
    assert_eq!(result.orphaned[0].reason, "no_spawn_loop_entry");
    let mut recovery = OrphanRecoveryResult::default();
    recover_issue(dir.path(), 5501, "no_spawn_loop_entry", &mut recovery, 600);
    assert!(gh.calls().contains("issue edit"), "{}", gh.calls());
}

/// Same shape as [`install_fake_gh_with_lease`], but the `.../comments`
/// lease probe FAILS outright (non-zero exit) instead of answering
/// (successfully) with either a timestamp or nothing — modeling a
/// transient `gh api` error (rate limit, timeout, a forge hiccup) DURING
/// `recover-orphans`, as opposed to a successful read that legitimately
/// found no lease comment.
#[cfg(unix)]
fn install_fake_gh_with_failing_lease_probe(
    dir: &Path,
    graphql_payload: &str,
    graphql_exit: i32,
) -> FakeGh {
    use std::os::unix::fs::PermissionsExt;

    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let log = dir.join("gh-invocations.log");
    let script = format!(
        "#!/bin/sh\n\
         echo \"$@\" >> '{log}'\n\
         if [ \"$1\" = \"issue\" ] && [ \"$2\" = \"list\" ]; then\n\
         printf '%s' '[{{\"number\":5501,\"title\":\"live work\"}}]'\n\
         exit 0\n\
         fi\n\
         if [ \"$1\" = \"repo\" ]; then printf 'rjwalters/loom\\n'; exit 0; fi\n\
         if [ \"$1\" = \"api\" ] && [ \"$2\" = \"graphql\" ]; then\n\
         printf '%s' '{payload}'\n\
         exit {exit_code}\n\
         fi\n\
         case \"$*\" in\n\
         */comments*)\n\
         echo 'simulated transient gh api failure (rate limit / timeout)' >&2\n\
         exit 1\n\
         ;;\n\
         esac\n\
         if [ \"$1\" = \"api\" ]; then printf '2020-01-01T00:00:00Z\\n'; exit 0; fi\n\
         exit 0\n",
        log = log.display(),
        payload = graphql_payload,
        exit_code = graphql_exit,
    );
    let fake_gh = bin.join("gh");
    std::fs::write(&fake_gh, script).unwrap();
    std::fs::set_permissions(&fake_gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    // #9548: `dir` is a registered, writable checkout, and the `gh` the
    // label writers' write-scope check probes with (`LOOM_GH_BIN`) answers
    // for it with push, handing every other call to the fake above.
    let ws = WritableRoot::register_with_gh(dir, &fake_gh);
    std::env::set_var("LOOM_GH_BIN", &ws.gh);
    FakeGh { log, _ws: ws }
}

/// Issue #7596 regression (mirrors #7591 / PR #7597's
/// `reconcile_workspace_keeps_claim_when_lease_probe_read_fails`): a
/// lease-freshness probe READ FAILURE must never be treated as "no lease
/// evidence" (which does not block a reset) -- it must block the reset
/// exactly like a found, fresh lease already does. Before this fix,
/// `gh::freshest_lease_updated_at` collapsed "no lease comment found" and
/// "the `gh api` call itself failed" into the same `None`, and
/// `lease_blocks_reset` treated both as "does not block the reset" --
/// fail-open, letting a transient forge read failure make a live claim
/// look orphaned.
#[cfg(unix)]
#[test]
#[serial(loom_config_env)]
fn lease_probe_read_failure_blocks_recovery_even_with_no_linked_pr() {
    lease_probe_read_failure_blocks_recovery_even_with_no_linked_pr_body();
}

#[serial]
fn lease_probe_read_failure_blocks_recovery_even_with_no_linked_pr_body() {
    let dir = tempdir().unwrap();
    let gh = install_fake_gh_with_failing_lease_probe(dir.path(), &closes_graph(""), 0);

    let mut result = OrphanRecoveryResult::default();
    check_untracked_building(&evidence_without_the_issue(), &mut result, dir.path(), 600, false);

    assert!(
        result.orphaned.is_empty(),
        "a FAILED lease-freshness probe must refuse to flag the claim orphaned -- an \
         unverifiable read is not evidence the lease is absent, and treating it as such is \
         the #7591/#7596 fail-open bug: {:?}",
        result.orphaned
    );
    assert_eq!(result.watched.len(), 1, "{:?}", result.watched);
    assert_eq!(result.watched[0].issue, 5501);
    assert!(
        !gh.calls().contains("issue edit"),
        "no label flip while the lease probe is unverifiable; gh calls:\n{}",
        gh.calls()
    );
    assert!(
        gh.calls().contains("comments"),
        "the lease-comments endpoint must actually have been consulted (and observed to \
         fail); gh calls:\n{}",
        gh.calls()
    );
}

#[test]
fn format_result_json_round_trips_totals() {
    let mut result = OrphanRecoveryResult::default();
    result.orphaned.push(OrphanEntry {
        kind: "untracked_building",
        issue: Some(1),
        pr: None,
        pid: None,
        title: Some("t".to_string()),
        reason: "no_spawn_loop_entry".to_string(),
        age_seconds: None,
    });
    let json = format_result_json(&result);
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["total_orphaned"], 1);
}

// ------------------------------------------------------------------
// #6167: stale PR-side `loom:reviewing`/`loom:treating` claim recovery
// ------------------------------------------------------------------

/// Install a fake `gh` (via `LOOM_GH_BIN`) that answers every call
/// [`check_stale_pr_claims`]'s underlying
/// `claim_reconciliation::forge::reconcile_pr_claims_report` makes for
/// one PR: the REST open-PR listing (#10349: the PR carries
/// `loom:reviewing`), `pr view` (labels for the safety-net backfill
/// check), and a catch-all `exit 0` for the `api .../timeline` /
/// `api .../comments` freshness probes (so `decide_pr` falls back to
/// `updatedAt` — deliberate, keeps this fixture from needing to model
/// the claim-labeled-at/comment freshness signal).
#[cfg(unix)]
fn install_fake_gh_pr(
    dir: &Path,
    pr_number: u32,
    updated_at: &str,
    head_ref_name: &str,
    extra_labels: &[&str],
) -> FakeGh {
    use std::os::unix::fs::PermissionsExt;

    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let log = dir.join("gh-invocations-pr.log");
    let labels_json = extra_labels
        .iter()
        .map(|l| format!(r#"{{"name":"{l}"}}"#))
        .collect::<Vec<_>>()
        .join(",");
    // #10349: the claim passes read the REST open-PR listing.
    let pulls = {
        use crate::claim_reconciliation::open_pr_listing::test_support::{pulls_arm, row};
        pulls_arm(&[row(pr_number, &["loom:reviewing"])
            .head(head_ref_name)
            .updated(updated_at)])
    };
    let script = format!(
        "#!/usr/bin/env bash\n\
         printf '%s\\n' \"$*\" >> '{log}'\n\
         {pulls}\
         if [ \"$1\" = \"pr\" ] && [ \"$2\" = \"view\" ]; then\n\
         echo '{{\"labels\":[{labels_json}]}}'\n\
         exit 0\n\
         fi\n\
         exit 0\n",
        log = log.display(),
    );
    let fake_gh = bin.join("gh");
    std::fs::write(&fake_gh, script).unwrap();
    std::fs::set_permissions(&fake_gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    // #9548: `dir` is a registered, writable checkout, and the `gh` the
    // label writers' write-scope check probes with (`LOOM_GH_BIN`) answers
    // for it with push, handing every other call to the fake above.
    let ws = WritableRoot::register_with_gh(dir, &fake_gh);
    std::env::set_var("LOOM_GH_BIN", &ws.gh);
    FakeGh { log, _ws: ws }
}

/// Dry-run (`recover=false`) reports a stale `loom:reviewing` claim
/// without issuing any mutating `gh` call (AC1 detection + the
/// `recover-orphans` dry-run contract).
#[cfg(unix)]
#[test]
#[serial(loom_config_env)]
fn check_stale_pr_claims_dry_run_reports_without_mutating() {
    check_stale_pr_claims_dry_run_reports_without_mutating_body();
}

#[serial]
fn check_stale_pr_claims_dry_run_reports_without_mutating_body() {
    let dir = tempdir().unwrap();
    let old = (chrono::Utc::now() - chrono::Duration::minutes(90)).to_rfc3339();
    let gh = install_fake_gh_pr(dir.path(), 700, &old, "some-random-branch", &[]);

    let mut result = OrphanRecoveryResult::default();
    with_isolated_journal_path(dir.path(), || {
        check_stale_pr_claims(dir.path(), &mut result, false);
    });

    assert!(
        result
            .orphaned
            .iter()
            .any(|o| o.kind == "stale_reviewing_pr" && o.pr == Some(700)),
        "expected a stale_reviewing_pr orphan for PR #700: {:?}",
        result.orphaned
    );
    assert!(
        result.recovered.is_empty(),
        "dry-run must never reclaim: {:?}",
        result.recovered
    );
    assert!(
        !gh.calls().contains("--remove-label"),
        "dry-run must not remove any claim label: {}",
        gh.calls()
    );
}

/// `recover=true` reclaims a stale `loom:reviewing` claim and (no state
/// label present) backfills `loom:review-requested`, mirroring
/// `forge::reclaim_pr`'s safety net (AC1 recovery).
#[cfg(unix)]
#[test]
#[serial(loom_config_env)]
fn check_stale_pr_claims_recover_reclaims_and_backfills() {
    check_stale_pr_claims_recover_reclaims_and_backfills_body();
}

#[serial]
fn check_stale_pr_claims_recover_reclaims_and_backfills_body() {
    let dir = tempdir().unwrap();
    let old = (chrono::Utc::now() - chrono::Duration::minutes(90)).to_rfc3339();
    let gh = install_fake_gh_pr(dir.path(), 701, &old, "some-random-branch", &[]);

    let mut result = OrphanRecoveryResult::default();
    with_isolated_journal_path(dir.path(), || {
        check_stale_pr_claims(dir.path(), &mut result, true);
    });

    assert!(
        result
            .recovered
            .iter()
            .any(|r| r.action == "reclaim_pr_claim" && r.pr == Some(701)),
        "expected a reclaim_pr_claim recovery entry for PR #701: {:?}",
        result.recovered
    );
    let calls = gh.calls();
    assert!(
        calls.contains("pr edit 701 --remove-label loom:reviewing"),
        "expected loom:reviewing to be removed from #701; got: {calls:?}"
    );
    assert!(
        calls.contains("pr edit 701 --add-label loom:review-requested"),
        "expected the safety net to add loom:review-requested to #701; got: {calls:?}"
    );
}

/// A fresh claim (age well under the staleness threshold) must never be
/// reclaimed — the never-strip-a-live-worker discipline (AC2), reusing
/// the identical `decide_pr` liveness/staleness logic the daemon
/// backstop and judge.md's own check already share.
#[cfg(unix)]
#[test]
#[serial(loom_config_env)]
fn check_stale_pr_claims_never_reclaims_a_fresh_claim() {
    check_stale_pr_claims_never_reclaims_a_fresh_claim_body();
}

#[serial]
fn check_stale_pr_claims_never_reclaims_a_fresh_claim_body() {
    let dir = tempdir().unwrap();
    let fresh = chrono::Utc::now().to_rfc3339();
    let gh = install_fake_gh_pr(
        dir.path(),
        702,
        &fresh,
        "some-random-branch",
        &["loom:review-requested"],
    );

    let mut result = OrphanRecoveryResult::default();
    with_isolated_journal_path(dir.path(), || {
        check_stale_pr_claims(dir.path(), &mut result, true);
    });

    assert!(
        result.orphaned.is_empty(),
        "a fresh PR-side claim must not be flagged orphaned: {:?}",
        result.orphaned
    );
    assert!(
        result.recovered.is_empty(),
        "a fresh PR-side claim must never be reclaimed: {:?}",
        result.recovered
    );
    assert!(
        !gh.calls().contains("--remove-label"),
        "no claim label should have been removed: {}",
        gh.calls()
    );
}
