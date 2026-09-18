//! Unit tests for [`super`] — `tokens_pool::select`.
//!
//! Extracted verbatim from `select.rs` for the same reason as
//! `bad_tokens_tests.rs` — see that file's header. `select.rs` declares it with
//! `#[cfg(test)] #[path = "select_tests.rs"] mod tests;`.

use super::*;
use std::fs;

// `SHARED_TOKENS_DIR_ENV` / `LOOM_TOKEN_SPREAD_TOP_N` are process-global;
// `#[serial]` (serial_test's default unkeyed group) serializes against
// every other unkeyed `#[serial]` test in the crate, including the
// `SHARED_TOKENS_DIR_ENV` mutations in `paths.rs`.
use serial_test::serial;

// `pub(super)` so the sibling `select_affinity_tests` module (#8146) can
// reuse these two fixtures instead of duplicating them.
pub(super) fn make_pool(names: &[&str]) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join(".loom").join("tokens");
    fs::create_dir_all(&dir).unwrap();
    for n in names {
        fs::write(dir.join(format!("{n}.token")), format!("key-{n}")).unwrap();
    }
    tmp
}

pub(super) fn pool_dir(ws: &Path) -> PathBuf {
    ws.join(".loom").join("tokens")
}

#[test]
#[serial]
fn errors_when_dir_missing() {
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var(super::super::paths::SHARED_TOKENS_DIR_ENV, "");
    let err = select_token(tmp.path(), None).unwrap_err();
    assert!(err.0.contains("does not exist"));
    std::env::remove_var(super::super::paths::SHARED_TOKENS_DIR_ENV);
}

#[test]
#[serial]
fn errors_when_no_token_files() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir_all(pool_dir(tmp.path())).unwrap();
    std::env::set_var(super::super::paths::SHARED_TOKENS_DIR_ENV, "");
    let err = select_token(tmp.path(), None).unwrap_err();
    assert!(err.0.contains("No .token files"));
    std::env::remove_var(super::super::paths::SHARED_TOKENS_DIR_ENV);
}

#[test]
fn single_token_selected_via_random_tier() {
    let tmp = make_pool(&["only"]);
    let mut rng = Rng::seeded(1);
    let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
    assert_eq!(sel.name, "only");
    assert_eq!(sel.mode, "random");
    assert_eq!(sel.key, "key-only");
    // #5609: no index.json manifest at all -> fail-open, no upstream_id.
    assert_eq!(sel.upstream_id, None);
}

#[test]
fn bad_token_is_skipped_in_random_tier() {
    let tmp = make_pool(&["a", "b"]);
    super::super::bad_tokens::mark_bad(tmp.path(), "a", "exhausted").unwrap();
    let mut rng = Rng::seeded(1);
    for _ in 0..10 {
        let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
        assert_eq!(sel.name, "b");
    }
}

/// #6030: an auth-dead account (`claude-wrapper.sh`'s
/// `"auth-dead: ..."` mark-bad reason for a 401/invalid-bearer-token
/// death) is excluded the same way an exhausted one is — and, unlike
/// exhaustion, it never times back in on the cooldown.
#[test]
fn auth_dead_token_is_skipped_in_random_tier() {
    let tmp = make_pool(&["a", "b"]);
    super::super::bad_tokens::mark_bad(tmp.path(), "a", "auth-dead: 401 Invalid bearer token")
        .unwrap();
    let mut rng = Rng::seeded(1);
    for _ in 0..10 {
        let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
        assert_eq!(sel.name, "b");
    }
}

#[test]
fn allowlist_tier_restricts_selection() {
    let tmp = make_pool(&["a", "b", "c"]);
    fs::write(pool_dir(tmp.path()).join(".allowlist"), "b\n").unwrap();
    let mut rng = Rng::seeded(1);
    for _ in 0..10 {
        let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
        assert_eq!(sel.name, "b");
        assert_eq!(sel.mode, "allowlist");
    }
}

#[test]
fn fresh_ranking_prefers_healthy_status_over_random() {
    let tmp = make_pool(&["a", "b"]);
    fs::write(pool_dir(tmp.path()).join(".ranking"), "a|available\nb|exhausted\n").unwrap();
    let mut rng = Rng::seeded(1);
    for _ in 0..5 {
        let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
        assert_eq!(sel.name, "a");
        assert_eq!(sel.mode, "ranked");
    }
}

#[test]
fn ranking_hard_excludes_exhausted_and_blocked_even_in_fallback() {
    let tmp = make_pool(&["a", "b"]);
    // No healthy entries at all; fallback pass must still exclude
    // exhausted/blocked, leaving nothing ranked -> falls to random/allow.
    fs::write(pool_dir(tmp.path()).join(".ranking"), "a|exhausted\nb|blocked\n").unwrap();
    let mut rng = Rng::seeded(1);
    // #5629: the hard exclusion now propagates to tiers 2/3 as well, so
    // there is no eligible account left and selection fails fast instead
    // of handing out an account the ranking already knows is dead.
    let err = select_token(tmp.path(), Some(&mut rng)).unwrap_err();
    assert!(err.0.contains("a: hard-excluded by .ranking status"), "{}", err.0);
    assert!(err.0.contains("b: hard-excluded by .ranking status"), "{}", err.0);
}

/// Seed a `.bad_tokens` line for `name` aged `age_secs` seconds — old
/// enough to have expired, so only the *history* remains.
fn seed_expired_entry(ws: &Path, name: &str, reason: &str, age_secs: i64) {
    let marked = (chrono::Utc::now() - chrono::Duration::seconds(age_secs))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let path = pool_dir(ws).join(".bad_tokens");
    let mut text = fs::read_to_string(&path).unwrap_or_default();
    text.push_str(&format!("{marked} {name} {reason}\n"));
    fs::write(path, text).unwrap();
}

/// #7522: a `.ranking` row still saying `blocked` — stale relative to the
/// live `.bad_tokens` state, because no probe has re-run since the
/// account's **session-limit** entry expired (its 5h window rolled over, or
/// it was `tokens unblock`'d) — no longer hard-excludes the account. The
/// fail-safe fallback tier hands it out instead of waiting on a `tokens
/// check --ranking` refresh that may not happen for a while, closing the
/// "an hour of fleet-wide starvation after every 5h boundary" gap the issue
/// reports.
#[test]
fn ranking_blocked_status_is_readmitted_once_the_session_limit_entry_expires() {
    let tmp = make_pool(&["a", "b"]);
    // "a" is exhausted (a real 7d ceiling, unrelated to .bad_tokens) and
    // must stay hard-excluded; "b"'s `.ranking` row still says `blocked`
    // from a stale probe, but the session-limit entry behind it expired 7h
    // ago. Only "b" should be selectable.
    fs::write(pool_dir(tmp.path()).join(".ranking"), "a|exhausted\nb|blocked\n").unwrap();
    seed_expired_entry(tmp.path(), "b", "exhausted: hit your session limit", 7 * 3600);
    let mut rng = Rng::seeded(1);
    let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
    assert_eq!(sel.name, "b");
}

/// #7536 review (blocker 1), restoring the #5629 coverage: a `blocked`
/// `.ranking` row is **not** always a `.bad_tokens` snapshot — `tokens
/// check` writes it for a probe that returned 401 (`auth_401`) or for a
/// credential shape mismatch (#5608) *without* calling `mark_bad`, so the
/// row's account has no `.bad_tokens` history at all. Readmitting that
/// shape would let the fail-safe retry hand out a permanently dead
/// credential, since a 401 never self-heals. It must stay hard-excluded in
/// every tier — including the fail-safe retry — and must still fail fast
/// with #4643's per-account diagnostic rather than returning a dead token.
#[test]
fn ranking_blocked_row_without_bad_tokens_history_stays_hard_excluded() {
    let tmp = make_pool(&["a"]);
    fs::write(pool_dir(tmp.path()).join(".ranking"), "a|blocked\n").unwrap();
    assert!(!pool_dir(tmp.path()).join(".bad_tokens").exists());
    let mut rng = Rng::seeded(1);
    let err = select_token(tmp.path(), Some(&mut rng)).unwrap_err();
    assert!(err.0.contains("a: hard-excluded by .ranking status"), "{}", err.0);
}

/// #7536 review (blocker 1): the same, for the shape where the *latest*
/// history entry is a non-session reason. An expired weekly-ceiling mark is
/// not evidence the 5h window rolled over, so the `blocked` row keeps its
/// hard exclusion until a probe refreshes it.
#[test]
fn ranking_blocked_row_with_non_session_history_stays_hard_excluded() {
    let tmp = make_pool(&["a"]);
    fs::write(pool_dir(tmp.path()).join(".ranking"), "a|blocked\n").unwrap();
    seed_expired_entry(tmp.path(), "a", "exhausted: used 100% of your weekly limit", 30 * 3600);
    let mut rng = Rng::seeded(1);
    let err = select_token(tmp.path(), Some(&mut rng)).unwrap_err();
    assert!(err.0.contains("a: hard-excluded by .ranking status"), "{}", err.0);
}

/// #7536 review (blocker 1): a `blocked` row backed by a *live* entry is
/// hard-excluded regardless of reason — the pre-#7522 behavior, unchanged.
#[test]
fn ranking_blocked_row_with_a_live_entry_stays_hard_excluded() {
    let tmp = make_pool(&["a"]);
    fs::write(pool_dir(tmp.path()).join(".ranking"), "a|blocked\n").unwrap();
    // Fresh session-limit mark: still inside its own 5h window.
    seed_expired_entry(tmp.path(), "a", "exhausted: hit your session limit", 60);
    let mut rng = Rng::seeded(1);
    let err = select_token(tmp.path(), Some(&mut rng)).unwrap_err();
    assert!(err.0.contains("a: hard-excluded by .ranking status"), "{}", err.0);
}

// ---- fresh-ranking hard exclusions reach tiers 2/3 (issue #5629) ----

/// #5629: a **fresh** `.ranking` marking the sole account `exhausted` must
/// not be handed out by the tier-3 random fallback. Before the fix,
/// `stale_ranking_exclusions` only produced a non-empty exclusion set for a
/// *stale* ranking, so `try_random` ran with an empty exclusion set and
/// happily returned the account the ranking already knew was dead —
/// exactly the `mode=random` spawn observed on 2026-08-07.
#[test]
fn fresh_ranking_exhausted_is_not_handed_out_by_random_tier() {
    let tmp = make_pool(&["a"]);
    fs::write(pool_dir(tmp.path()).join(".ranking"), "a|exhausted\n").unwrap();
    let mut rng = Rng::seeded(1);
    let err = select_token(tmp.path(), Some(&mut rng)).unwrap_err();
    assert!(
        err.0
            .contains("a: hard-excluded by .ranking status (exhausted)"),
        "{}",
        err.0
    );
    // The fail-safe must NOT readmit a hard-excluded account.
    assert!(err.0.contains("loom-daemon tokens check --ranking"), "{}", err.0);
}

/// #5629: with one exhausted and one eligible account in a fresh ranking,
/// the eligible account is selected — the fix must not turn a partially
/// exhausted pool into a hard failure.
#[test]
fn fresh_ranking_exhausted_skipped_in_favor_of_eligible_account() {
    let tmp = make_pool(&["a", "b"]);
    // `b` has no ranking row at all, so tier 1 has no candidate (`a` is
    // hard-excluded) and selection falls through to the random tier.
    fs::write(pool_dir(tmp.path()).join(".ranking"), "a|exhausted\n").unwrap();
    let mut rng = Rng::seeded(1);
    for _ in 0..10 {
        let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
        assert_eq!(sel.name, "b");
        assert_eq!(sel.mode, "random");
    }
}

/// #5629: the same exclusion applies to tier 2 (`.allowlist`) — an
/// allowlisted account marked `exhausted` in a fresh ranking is skipped in
/// favor of another allowlisted account.
#[test]
fn fresh_ranking_exhausted_is_excluded_from_allowlist_tier() {
    let tmp = make_pool(&["a", "b"]);
    fs::write(pool_dir(tmp.path()).join(".allowlist"), "a\nb\n").unwrap();
    fs::write(pool_dir(tmp.path()).join(".ranking"), "a|exhausted\n").unwrap();
    let mut rng = Rng::seeded(1);
    for _ in 0..10 {
        let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
        assert_eq!(sel.name, "b");
        assert_eq!(sel.mode, "allowlist");
    }
}

/// #5629 / #3894 interaction: a *stale* ranking's non-hard statuses stay
/// **advisory** (the fail-safe readmits them rather than emptying the
/// pool), while its hard statuses stay hard.
#[test]
fn stale_ranking_advisory_exclusion_still_fails_safe_alongside_hard_exclusions() {
    let tmp = make_pool(&["a", "b"]);
    let ranking = pool_dir(tmp.path()).join(".ranking");
    // `a` is hard-excluded (exhausted); `b` is only advisory-excluded
    // (rate_limited is non-healthy but not hard). Excluding both would
    // empty the pool -> the fail-safe must readmit `b` only.
    fs::write(&ranking, "a|exhausted\nb|rate_limited\n").unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(700);
    let f = fs::File::open(&ranking).unwrap();
    f.set_modified(old).unwrap();

    let mut rng = Rng::seeded(1);
    for _ in 0..10 {
        let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
        assert_eq!(sel.name, "b");
    }
}

#[test]
fn ranking_fallback_pass_admits_rate_limited_when_nothing_healthy() {
    let tmp = make_pool(&["a"]);
    fs::write(pool_dir(tmp.path()).join(".ranking"), "a|rate_limited\n").unwrap();
    let mut rng = Rng::seeded(1);
    let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
    assert_eq!(sel.name, "a");
    assert_eq!(sel.mode, "ranked");
}

#[test]
fn stale_ranking_is_ignored_by_tier1_but_excludes_from_lower_tiers() {
    let tmp = make_pool(&["a", "b"]);
    let ranking = pool_dir(tmp.path()).join(".ranking");
    fs::write(&ranking, "a|exhausted\nb|available\n").unwrap();
    // Backdate the ranking file well past the freshness window.
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(700);
    let f = fs::File::open(&ranking).unwrap();
    f.set_modified(old).unwrap();

    let mut rng = Rng::seeded(1);
    for _ in 0..10 {
        let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
        // "a" is advisory-excluded (stale ranking said exhausted); only
        // "b" should ever be picked in the lower tiers.
        assert_eq!(sel.name, "b");
    }
}

#[test]
fn stale_ranking_exclusions_fail_safe_when_pool_would_empty() {
    let tmp = make_pool(&["a"]);
    let ranking = pool_dir(tmp.path()).join(".ranking");
    // `rate_limited` is non-healthy but NOT hard-excluded, so it is only
    // advisory (#3894) — the fail-safe must readmit it.
    fs::write(&ranking, "a|rate_limited\n").unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(700);
    let f = fs::File::open(&ranking).unwrap();
    f.set_modified(old).unwrap();

    let mut rng = Rng::seeded(1);
    // Excluding "a" would empty the pool -> fail-safe retry ignoring
    // advisory exclusions must still return "a".
    let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
    assert_eq!(sel.name, "a");
}

/// #5629: the fail-safe above must NOT readmit a *hard*-excluded account.
/// `exhausted`/`blocked` are durable, account-scoped refusals — handing
/// one out "because the pool would otherwise be empty" is what burned
/// ~17 minutes per role tick on 2026-08-07.
#[test]
fn stale_ranking_fail_safe_does_not_readmit_hard_excluded_account() {
    let tmp = make_pool(&["a"]);
    let ranking = pool_dir(tmp.path()).join(".ranking");
    fs::write(&ranking, "a|exhausted\n").unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(700);
    let f = fs::File::open(&ranking).unwrap();
    f.set_modified(old).unwrap();

    let mut rng = Rng::seeded(1);
    let err = select_token(tmp.path(), Some(&mut rng)).unwrap_err();
    assert!(
        err.0
            .contains("a: hard-excluded by .ranking status (exhausted)"),
        "{}",
        err.0
    );
}

#[test]
fn all_bad_tokens_errors() {
    let tmp = make_pool(&["a", "b"]);
    super::super::bad_tokens::mark_bad(tmp.path(), "a", "x").unwrap();
    super::super::bad_tokens::mark_bad(tmp.path(), "b", "x").unwrap();
    let mut rng = Rng::seeded(1);
    let err = select_token(tmp.path(), Some(&mut rng)).unwrap_err();
    assert!(err.0.contains("marked bad"));
}

// ---- shared-pool hint on the all-excluded path (issue #6614) --------
//
// The dir-missing / no-`.token`-files paths have named a possible shared
// pool since #3938; the all-excluded path — the one actually hit when a
// stale repo-local pool shadows a healthy shared one — did not, which is
// how the #6614 incident reported "empty pool" while several healthy
// shared accounts sat one directory away.

/// A healthy shared pool that the repo-local pool shadowed must be named
/// loudly, since [`resolve_tokens_dir`] never consulted it.
#[test]
#[serial]
fn all_excluded_error_names_a_shadowing_shared_pool() {
    let tmp = make_pool(&["a", "b"]);
    super::super::bad_tokens::mark_bad(tmp.path(), "a", "x").unwrap();
    super::super::bad_tokens::mark_bad(tmp.path(), "b", "x").unwrap();

    let shared = tempfile::tempdir().unwrap();
    fs::write(shared.path().join("healthy.token"), "key-healthy").unwrap();
    std::env::set_var(super::super::paths::SHARED_TOKENS_DIR_ENV, shared.path());

    let mut rng = Rng::seeded(1);
    let err = select_token(tmp.path(), Some(&mut rng)).unwrap_err();
    std::env::remove_var(super::super::paths::SHARED_TOKENS_DIR_ENV);

    let text = err.0;
    // The pre-existing detail is unchanged …
    assert!(text.contains("marked bad"), "{text}");
    // … and the shadowed healthy pool is now named, with its path.
    assert!(text.contains("SHADOWED POOL"), "{text}");
    assert!(text.contains(&shared.path().display().to_string()), "{text}");
    assert!(text.contains("was NOT consulted"), "{text}");
}

/// When the exhausted pool IS the shared one, say so — that is a genuine
/// exhaustion, not a stale-repo-local-copy artifact, and the operator
/// should not go hunting for a shadowed alternative.
#[test]
#[serial]
fn all_excluded_error_marks_a_genuinely_exhausted_shared_pool() {
    let tmp = make_pool(&["a"]);
    super::super::bad_tokens::mark_bad(tmp.path(), "a", "x").unwrap();
    // Point the shared-pool env at the very dir that was resolved.
    std::env::set_var(super::super::paths::SHARED_TOKENS_DIR_ENV, pool_dir(tmp.path()));

    let mut rng = Rng::seeded(1);
    let err = select_token(tmp.path(), Some(&mut rng)).unwrap_err();
    std::env::remove_var(super::super::paths::SHARED_TOKENS_DIR_ENV);

    let text = err.0;
    assert!(text.contains("this IS the shared machine-level pool"), "{text}");
    assert!(!text.contains("SHADOWED POOL"), "{text}");
}

/// A configured-but-empty shared pool is reported as a non-alternative
/// rather than dangled as a false lead.
#[test]
#[serial]
fn all_excluded_error_reports_an_empty_shared_pool_as_no_alternative() {
    let tmp = make_pool(&["a"]);
    super::super::bad_tokens::mark_bad(tmp.path(), "a", "x").unwrap();

    let shared = tempfile::tempdir().unwrap();
    std::env::set_var(super::super::paths::SHARED_TOKENS_DIR_ENV, shared.path());

    let mut rng = Rng::seeded(1);
    let err = select_token(tmp.path(), Some(&mut rng)).unwrap_err();
    std::env::remove_var(super::super::paths::SHARED_TOKENS_DIR_ENV);

    let text = err.0;
    assert!(text.contains("holds no .token files either"), "{text}");
    assert!(!text.contains("SHADOWED POOL"), "{text}");
}

/// With the shared pool disabled outright there is nothing to point at,
/// and the message must not grow a dangling hint.
#[test]
#[serial]
fn all_excluded_error_omits_the_hint_when_shared_pool_is_disabled() {
    let tmp = make_pool(&["a"]);
    super::super::bad_tokens::mark_bad(tmp.path(), "a", "x").unwrap();
    std::env::set_var(super::super::paths::SHARED_TOKENS_DIR_ENV, "");

    let mut rng = Rng::seeded(1);
    let err = select_token(tmp.path(), Some(&mut rng)).unwrap_err();
    std::env::remove_var(super::super::paths::SHARED_TOKENS_DIR_ENV);

    let text = err.0;
    assert!(text.contains("marked bad"), "{text}");
    assert!(!text.contains("shared machine-level pool"), "{text}");
}

// ---- shared-pool usability, not just presence, gates the hint (#6758) ---
//
// `shadowed_shared_pool_hint` used to key entirely off `has_token_files`,
// so a shared pool whose accounts were all bad-marked/`.ranking`-excluded
// still got the "SHADOWED POOL … re-bootstrap" recommendation — misleading,
// since retiring the repo-local pool would not produce a working spawn.

/// Shared pool present with a genuinely usable account -> the existing
/// "shadowed, retire the repo-local copy" framing (unchanged wording).
#[test]
#[serial]
fn all_excluded_error_recommends_retiring_local_when_shared_pool_has_a_usable_account() {
    let tmp = make_pool(&["a", "b"]);
    super::super::bad_tokens::mark_bad(tmp.path(), "a", "x").unwrap();
    super::super::bad_tokens::mark_bad(tmp.path(), "b", "x").unwrap();

    let shared = tempfile::tempdir().unwrap();
    fs::write(shared.path().join("healthy.token"), "key-healthy").unwrap();
    fs::write(shared.path().join("also-healthy.token"), "key-also").unwrap();
    std::env::set_var(super::super::paths::SHARED_TOKENS_DIR_ENV, shared.path());

    let mut rng = Rng::seeded(1);
    let err = select_token(tmp.path(), Some(&mut rng)).unwrap_err();
    std::env::remove_var(super::super::paths::SHARED_TOKENS_DIR_ENV);

    let text = err.0;
    assert!(text.contains("SHADOWED POOL"), "{text}");
    assert!(text.contains("re-bootstrap or remove"), "{text}");
    // Issue #7527: the hint now names spawnable-account counts for both
    // pools, not just "usable: yes" — the repo-local pool has 2 accounts,
    // both bad-marked (0 usable); the shared pool has 2, both healthy.
    assert!(text.contains("this pool 0/2 usable"), "{text}");
    assert!(text.contains("shared pool 2/2 usable"), "{text}");
}

/// Shared pool present but every account is bad-marked, same as the
/// repo-local pool -> a distinct "no usable accounts anywhere" message
/// that does NOT recommend retiring the repo-local directory, since
/// re-auth is needed either way.
#[test]
#[serial]
fn all_excluded_error_reports_no_usable_accounts_anywhere_when_shared_pool_is_also_dead() {
    let tmp = make_pool(&["a", "b"]);
    super::super::bad_tokens::mark_bad(tmp.path(), "a", "x").unwrap();
    super::super::bad_tokens::mark_bad(tmp.path(), "b", "x").unwrap();

    let shared = tempfile::tempdir().unwrap();
    fs::write(shared.path().join("c.token"), "key-c").unwrap();
    std::env::set_var(super::super::paths::SHARED_TOKENS_DIR_ENV, shared.path());
    super::super::bad_tokens::mark_bad(shared.path(), "c", "auth-dead: revoked").unwrap();

    let mut rng = Rng::seeded(1);
    let err = select_token(tmp.path(), Some(&mut rng)).unwrap_err();
    std::env::remove_var(super::super::paths::SHARED_TOKENS_DIR_ENV);

    let text = err.0;
    assert!(!text.contains("SHADOWED POOL"), "{text}");
    assert!(text.contains("no usable accounts anywhere"), "{text}");
    assert!(text.contains(&shared.path().display().to_string()), "{text}");
    // Issue #7527: counts are named even in the "both dead" case — 0/2
    // repo-local, 0/1 shared.
    assert!(text.contains("this pool 0/2 usable"), "{text}");
    assert!(text.contains("shared pool 0/1 usable"), "{text}");
}

/// A shared pool with token files but every account hard-excluded by its
/// own `.ranking` (rather than bad-marked) is equally "not a real
/// alternative" — the usability check must consult `.ranking`, not only
/// `.bad_tokens`.
#[test]
#[serial]
fn all_excluded_error_reports_no_usable_accounts_when_shared_pool_ranking_hard_excludes_all() {
    let tmp = make_pool(&["a"]);
    super::super::bad_tokens::mark_bad(tmp.path(), "a", "x").unwrap();

    let shared = tempfile::tempdir().unwrap();
    fs::write(shared.path().join("c.token"), "key-c").unwrap();
    fs::write(shared.path().join(".ranking"), "c|exhausted\n").unwrap();
    std::env::set_var(super::super::paths::SHARED_TOKENS_DIR_ENV, shared.path());

    let mut rng = Rng::seeded(1);
    let err = select_token(tmp.path(), Some(&mut rng)).unwrap_err();
    std::env::remove_var(super::super::paths::SHARED_TOKENS_DIR_ENV);

    let text = err.0;
    assert!(!text.contains("SHADOWED POOL"), "{text}");
    assert!(text.contains("no usable accounts anywhere"), "{text}");
}

/// Shared pool absent (disabled via empty env override) -> unchanged
/// "not an alternative" framing; no usability computation is even
/// attempted since [`shared_tokens_dir`] short-circuits to `None`.
#[test]
#[serial]
fn all_excluded_error_shared_pool_absent_is_unaffected_by_usability_check() {
    let tmp = make_pool(&["a"]);
    super::super::bad_tokens::mark_bad(tmp.path(), "a", "x").unwrap();
    std::env::set_var(super::super::paths::SHARED_TOKENS_DIR_ENV, "");

    let mut rng = Rng::seeded(1);
    let err = select_token(tmp.path(), Some(&mut rng)).unwrap_err();
    std::env::remove_var(super::super::paths::SHARED_TOKENS_DIR_ENV);

    let text = err.0;
    assert!(!text.contains("SHADOWED POOL"), "{text}");
    assert!(!text.contains("no usable accounts anywhere"), "{text}");
    assert!(!text.contains("shared machine-level pool"), "{text}");
}

// ---- empty-pool error detail (issue #4643) ------------------------

#[test]
fn format_secs_renders_compact_durations() {
    assert_eq!(format_secs(5 * 3600 + 48 * 60), "5h48m");
    assert_eq!(format_secs(48 * 60 + 12), "48m12s");
    assert_eq!(format_secs(12), "12s");
    assert_eq!(format_secs(-5), "0s");
}

/// #4643: the empty-pool error names every excluded account, its exclusion
/// cause, the reason class (auth = permanent vs exhaustion = TTL), the
/// entry's own timestamp, the cooldown remaining, and the deciding binary.
#[test]
fn empty_pool_error_enumerates_per_token_exclusion_detail() {
    let tmp = make_pool(&["exh", "auth"]);
    // Deliberately a few seconds shy of the 1h mark rather than exactly
    // `3600` — at exactly 3600s the remaining cooldown
    // (`SESSION_WINDOW_SECS` - elapsed) lands exactly on the 4h boundary,
    // so any wall-clock drift between computing `ts` here and
    // `select_token` re-deriving "now" below can push the rendered
    // duration to "3h59m" and flake the `contains("clears in 4h")`
    // assertion below (#7792).
    let ts = (chrono::Utc::now() - chrono::Duration::seconds(3600 - 5))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(
        pool_dir(tmp.path()).join(".bad_tokens"),
        format!(
            "{ts} exh exhausted: hit your session limit\n\
             {ts} auth 401 unauthorized\n"
        ),
    )
    .unwrap();

    let mut rng = Rng::seeded(1);
    let err = select_token(tmp.path(), Some(&mut rng)).unwrap_err();
    let text = err.0;

    // Per-token lines, with class + permanence + timestamp + reason.
    assert!(text.contains("exh: bad-marked [exhaustion, TTL]"), "{text}");
    assert!(text.contains("auth: bad-marked [auth, permanent]"), "{text}");
    assert!(text.contains(&ts), "{text}");
    assert!(text.contains("exhausted: hit your session limit"), "{text}");
    assert!(text.contains("401 unauthorized"), "{text}");
    // Cooldown remaining for the TTL entry (~5h of the 6h default left),
    // and the operator action for the permanent one.
    assert!(text.contains("clears in 4h") || text.contains("clears in 5h"), "{text}");
    assert!(text.contains("needs `loom-daemon tokens unblock auth`"), "{text}");
    // Deciding binary + the cooldown knob.
    assert!(text.contains("deciding binary: loom-daemon "), "{text}");
    assert!(text.contains(crate::self_update::BUILT_COMMIT), "{text}");
    assert!(text.contains(EXHAUSTION_COOLDOWN_ENV), "{text}");
}

/// #4643: a token whose file is present but empty is reported as such,
/// not lumped in with the bad-marked ones.
#[test]
#[serial]
fn empty_pool_error_distinguishes_empty_token_files() {
    let tmp = make_pool(&["blank"]);
    fs::write(pool_dir(tmp.path()).join("blank.token"), "   \n").unwrap();
    std::env::set_var(super::super::paths::SHARED_TOKENS_DIR_ENV, "");
    let mut rng = Rng::seeded(1);
    let err = select_token(tmp.path(), Some(&mut rng)).unwrap_err();
    std::env::remove_var(super::super::paths::SHARED_TOKENS_DIR_ENV);
    assert!(err.0.contains("blank: empty .token file"), "{}", err.0);
}

/// #4643: a malformed `.bad_tokens` timestamp shows up as fail-closed
/// permanent in the detail rather than as a TTL entry with a bogus clock.
#[test]
fn empty_pool_error_shows_malformed_entry_as_permanent() {
    let tmp = make_pool(&["a"]);
    fs::write(pool_dir(tmp.path()).join(".bad_tokens"), "garbage a exhausted\n").unwrap();
    let mut rng = Rng::seeded(1);
    let err = select_token(tmp.path(), Some(&mut rng)).unwrap_err();
    assert!(
        err.0
            .contains("a: bad-marked [malformed-timestamp, permanent (fail-closed)]"),
        "{}",
        err.0
    );
}

#[test]
fn deciding_binary_identity_names_version_and_commit() {
    let id = deciding_binary_identity();
    assert!(id.starts_with("loom-daemon "), "{id}");
    assert!(id.contains(env!("CARGO_PKG_VERSION")), "{id}");
    assert!(id.contains(crate::self_update::BUILT_COMMIT), "{id}");
}

#[test]
#[serial]
fn spread_top_n_env_caps_ranked_window() {
    let tmp = make_pool(&["a", "b", "c"]);
    fs::write(pool_dir(tmp.path()).join(".ranking"), "a|available\nb|available\nc|available\n")
        .unwrap();
    std::env::set_var("LOOM_TOKEN_SPREAD_TOP_N", "1");
    // Pre-seed rotation cursor so the outcome is deterministic.
    fs::write(pool_dir(tmp.path()).join(".rotation_cursor"), "0").unwrap();
    let mut rng = Rng::seeded(1);
    let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
    std::env::remove_var("LOOM_TOKEN_SPREAD_TOP_N");
    // N=1 == greedy first-eligible: always "a".
    assert_eq!(sel.name, "a");
}

// ---- config_resolver migration (#4241) — tier precedence ---------

fn write_legacy_config(root: &Path, contents: &str) {
    let dir = root.join(".loom");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("config.json"), contents).unwrap();
}

fn write_project_config(root: &Path, contents: &str) {
    let full = root.join(crate::config_resolver::PROJECT_CONFIG_REL);
    fs::create_dir_all(full.parent().unwrap()).unwrap();
    fs::write(full, contents).unwrap();
}

#[test]
#[serial(loom_config_env)]
fn read_config_spread_top_n_legacy_tier_only() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_legacy_config(tmp.path(), r#"{"tokens": {"spreadTopN": 3}}"#);
    let n = read_config_spread_top_n(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(n, Some(3));
}

#[test]
#[serial(loom_config_env)]
fn read_config_spread_top_n_project_tier_only_is_honored() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_project_config(tmp.path(), r#"{"tokens": {"spreadTopN": 5}}"#);
    let n = read_config_spread_top_n(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(n, Some(5));
}

#[test]
#[serial(loom_config_env)]
fn read_config_spread_top_n_project_tier_overrides_legacy() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_legacy_config(tmp.path(), r#"{"tokens": {"spreadTopN": 2}}"#);
    write_project_config(tmp.path(), r#"{"tokens": {"spreadTopN": 7}}"#);
    let n = read_config_spread_top_n(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(n, Some(7));
}

#[test]
#[serial(loom_config_env)]
fn read_config_spread_top_n_missing_everywhere_is_none() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    let n = read_config_spread_top_n(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(n, None);
}

#[test]
fn read_token_file_strips_whitespace() {
    let tmp = make_pool(&["a"]);
    fs::write(pool_dir(tmp.path()).join("a.token"), "  sk-ant\noat01\t-xyz  \n").unwrap();
    let key = read_token_file(&pool_dir(tmp.path()).join("a.token")).unwrap();
    assert_eq!(key, "sk-antoat01-xyz");
}

// ---- 5h load gate (issue #4195) ----------------------------------

#[test]
fn read_ranking_parses_optional_util_field() {
    let tmp = tempfile::tempdir().unwrap();
    let ranking = tmp.path().join(".ranking");
    // 3-field, legacy 2-field, and a malformed util (-> None, "unknown").
    fs::write(&ranking, "a|available|0.70\nb|available\nc|available|bad\n").unwrap();
    let rows = read_ranking(&ranking);
    assert_eq!(rows[0], ("a".to_string(), "available".to_string(), Some(0.70)));
    assert_eq!(rows[1], ("b".to_string(), "available".to_string(), None));
    assert_eq!(rows[2], ("c".to_string(), "available".to_string(), None));
}

// ---- limit-reset field (issue #4874) -----------------------------

#[test]
fn parse_ranking_line_reads_optional_limit_reset_field() {
    // A 4-field row surfaces the reset verbatim; the 2- and 3-field legacy
    // layouts still parse with `limit_reset = None` (backward compatible).
    let full = parse_ranking_line("a|exhausted|0.00|2026-08-02T03:00:00Z").unwrap();
    assert_eq!(full.name, "a");
    assert_eq!(full.status, "exhausted");
    assert_eq!(full.util_5h, Some(0.00));
    assert_eq!(full.limit_reset.as_deref(), Some("2026-08-02T03:00:00Z"));

    assert_eq!(parse_ranking_line("b|available|0.70").unwrap().limit_reset, None);
    assert_eq!(parse_ranking_line("c|available").unwrap().limit_reset, None);
}

#[test]
fn parse_ranking_line_reset_without_util_does_not_fabricate_zero() {
    // `name|status||reset` — the reset-known/util-unknown layout. The empty
    // third field must parse back to `None`, never to `0.0`, or a fully
    // idle account would look like a measured-zero-load one.
    let row = parse_ranking_line("a|exhausted||2026-08-04T11:00:00Z").unwrap();
    assert_eq!(row.status, "exhausted");
    assert_eq!(row.util_5h, None);
    assert_eq!(row.limit_reset.as_deref(), Some("2026-08-04T11:00:00Z"));
}

#[test]
fn parse_ranking_line_4th_field_does_not_swallow_the_util() {
    // Regression guard for the `splitn(3, ..)` hazard the curator flagged:
    // with a 3-way split the 4th segment is swallowed into the utilization
    // field, so `0.00|2026-...` fails to parse as a float and the
    // utilization silently becomes `None`.
    let row = parse_ranking_line("a|exhausted|0.42|2026-08-02T03:00:00Z").unwrap();
    assert_eq!(row.util_5h, Some(0.42), "the 4th field must not swallow the util");
}

#[test]
fn parse_ranking_line_reset_is_comment_stripped_and_trimmed() {
    // `#` comments are stripped before splitting, and surrounding
    // whitespace is trimmed off the reset like every other field.
    let row = parse_ranking_line("a|exhausted|0.00| 2026-08-02T03:00:00Z  # probed").unwrap();
    assert_eq!(row.limit_reset.as_deref(), Some("2026-08-02T03:00:00Z"));
    // An empty 4th field is "unknown", not an empty string.
    assert_eq!(parse_ranking_line("a|exhausted|0.00|").unwrap().limit_reset, None);
}

/// #8058 Phase 3 (#8242) considered appending per-model-class utilization
/// columns to `.ranking`, conditional on Anthropic's usage endpoint actually
/// emitting a per-class header. A live capture on 2026-09-19 established it
/// does not (see `defaults/docs/token-pool.md` → "Per-class observability"),
/// so **no columns were added** and the four-field
/// `name|status|5h_util|limit_reset` shape is the format of record.
///
/// This test pins the compatibility reasoning behind that call, so a future
/// attempt (#8297, the claude-monitor ingest path) cannot quietly append a
/// fifth column without meeting it: `splitn(4, '|')` does not stop at the
/// fourth separator, so a fifth field is **swallowed into `limit_reset`**
/// rather than ignored. That corrupts the reset instant for every reader that
/// has not been upgraded — it is a breaking change, not an additive one, and
/// any real per-class column has to be versioned or side-carred instead.
#[test]
fn parse_ranking_line_has_no_room_for_a_fifth_per_class_column() {
    // Today's format, all four fields — the shape every writer emits.
    let row = parse_ranking_line("a|available|0.42|2026-09-20T03:00:00Z").unwrap();
    assert_eq!(row.name, "a");
    assert_eq!(row.status, "available");
    assert_eq!(row.util_5h, Some(0.42));
    assert_eq!(row.limit_reset.as_deref(), Some("2026-09-20T03:00:00Z"));

    // Every shorter legacy row still parses, unchanged — the backward
    // compatibility AC1's negative-finding path requires.
    for line in ["a|available", "a|available|0.42"] {
        let legacy = parse_ranking_line(line).unwrap();
        assert_eq!(legacy.name, "a");
        assert_eq!(legacy.status, "available");
        assert_eq!(legacy.limit_reset, None, "{line} must not fabricate a reset");
    }

    // The hazard itself: a hypothetical trailing `|opus=0.91` is NOT dropped.
    // It lands inside `limit_reset`, which then fails `parse_reset`'s shape
    // check and reads as "unknown" — so the account silently loses its reset
    // instant. This is exactly why the column was not added.
    let with_fifth = parse_ranking_line("a|available|0.42|2026-09-20T03:00:00Z|opus=0.91").unwrap();
    assert_eq!(with_fifth.util_5h, Some(0.42), "the util field survives");
    assert_ne!(
        with_fifth.limit_reset.as_deref(),
        Some("2026-09-20T03:00:00Z"),
        "a 5th column is swallowed into limit_reset, corrupting it -- \
         any per-class column must be versioned or side-carred (#8297)"
    );
}

#[test]
fn read_ranking_ignores_the_reset_field_for_selection() {
    // Selection consumes the projected triple: a 4-field row must behave
    // exactly like the same row without a reset, so adding the field
    // cannot perturb which account gets picked.
    let tmp = tempfile::tempdir().unwrap();
    let ranking = tmp.path().join(".ranking");
    fs::write(&ranking, "a|available|0.70|2026-08-02T03:00:00Z\nb|available|0.70\n").unwrap();
    let rows = read_ranking(&ranking);
    assert_eq!(rows[0], ("a".to_string(), "available".to_string(), Some(0.70)));
    assert_eq!(rows[1], ("b".to_string(), "available".to_string(), Some(0.70)));
}

#[test]
#[serial]
fn resolve_load_gate_env_and_default() {
    std::env::remove_var("LOOM_TOKEN_5H_LOAD_GATE");
    assert_eq!(resolve_load_gate(), DEFAULT_5H_LOAD_GATE);
    std::env::set_var("LOOM_TOKEN_5H_LOAD_GATE", "0.5");
    assert_eq!(resolve_load_gate(), 0.5);
    // Unparseable -> default.
    std::env::set_var("LOOM_TOKEN_5H_LOAD_GATE", "garbage");
    assert_eq!(resolve_load_gate(), DEFAULT_5H_LOAD_GATE);
    std::env::remove_var("LOOM_TOKEN_5H_LOAD_GATE");
}

#[test]
#[serial]
fn load_gate_excludes_loaded_account_in_preferred_pass() {
    std::env::remove_var("LOOM_TOKEN_5H_LOAD_GATE");
    std::env::remove_var("LOOM_TOKEN_SPREAD_TOP_N");
    let tmp = make_pool(&["a", "b"]);
    // `a` healthy but 90% 5h-loaded (>= 0.70 default gate); `b` light.
    fs::write(pool_dir(tmp.path()).join(".ranking"), "a|available|0.90\nb|available|0.10\n")
        .unwrap();
    let mut rng = Rng::seeded(1);
    for _ in 0..10 {
        let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
        assert_eq!(sel.name, "b");
        assert_eq!(sel.mode, "ranked");
    }
}

#[test]
#[serial]
fn load_gate_readmits_in_fallback_when_all_loaded() {
    std::env::remove_var("LOOM_TOKEN_5H_LOAD_GATE");
    std::env::remove_var("LOOM_TOKEN_SPREAD_TOP_N");
    let tmp = make_pool(&["a", "b"]);
    // Both over the gate -> fallback pass drops the gate; pool never
    // hard-fails on load alone.
    fs::write(pool_dir(tmp.path()).join(".ranking"), "a|available|0.95\nb|available|0.90\n")
        .unwrap();
    let mut rng = Rng::seeded(1);
    let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
    assert!(sel.name == "a" || sel.name == "b");
    assert_eq!(sel.mode, "ranked");
}

#[test]
#[serial]
fn load_gate_unknown_util_is_never_gated() {
    std::env::remove_var("LOOM_TOKEN_5H_LOAD_GATE");
    std::env::remove_var("LOOM_TOKEN_SPREAD_TOP_N");
    let tmp = make_pool(&["a", "b", "c"]);
    // a: legacy 2-field (unknown); b: malformed util (unknown); c: loaded.
    fs::write(
        pool_dir(tmp.path()).join(".ranking"),
        "a|available\nb|available|not-a-number\nc|available|0.99\n",
    )
    .unwrap();
    let mut chosen = HashSet::new();
    for _ in 0..10 {
        let mut rng = Rng::seeded(1);
        let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
        assert_eq!(sel.mode, "ranked");
        chosen.insert(sel.name);
    }
    // a and b (unknown) rotate; c (0.99 loaded) is excluded.
    assert!(chosen.contains("a") && chosen.contains("b"));
    assert!(!chosen.contains("c"));
}

#[test]
#[serial]
fn load_gate_env_override_lowers_threshold() {
    std::env::remove_var("LOOM_TOKEN_SPREAD_TOP_N");
    let tmp = make_pool(&["a", "b"]);
    fs::write(pool_dir(tmp.path()).join(".ranking"), "a|available|0.50\nb|available|0.10\n")
        .unwrap();
    std::env::set_var("LOOM_TOKEN_5H_LOAD_GATE", "0.40");
    let mut rng = Rng::seeded(1);
    for _ in 0..10 {
        let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
        assert_eq!(sel.name, "b"); // `a` now over the lowered gate.
    }
    std::env::remove_var("LOOM_TOKEN_5H_LOAD_GATE");
}

#[test]
#[serial]
fn load_gate_backward_compatible_2field_ranking() {
    std::env::remove_var("LOOM_TOKEN_5H_LOAD_GATE");
    std::env::remove_var("LOOM_TOKEN_SPREAD_TOP_N");
    let tmp = make_pool(&["a", "b"]);
    // Pure legacy 2-field file still parses + selects unchanged.
    fs::write(pool_dir(tmp.path()).join(".ranking"), "a|exhausted\nb|available\n").unwrap();
    let mut rng = Rng::seeded(1);
    let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
    assert_eq!(sel.name, "b");
    assert_eq!(sel.mode, "ranked");
}

// ---- provider filtering + upstream_id (issue #5609, design D8/D9) ----

fn write_index_json(pool: &Path, body: &str) {
    fs::write(pool.join("index.json"), body).unwrap();
}

/// A pool containing a non-claude manifest row is never selected from by
/// the Claude selector (random tier) — the row is skipped regardless of
/// its `.token` file being present on disk (defense-in-depth, D4/D8) —
/// and the eligible Claude row's `upstream_id` is carried through.
#[test]
fn non_claude_manifest_row_is_never_selected_random_tier() {
    let tmp = make_pool(&["good", "sneaky"]);
    write_index_json(
        &pool_dir(tmp.path()),
        r#"{"version":3,"accounts":[
            {"name":"sneaky","provider":"codex","upstream_id":"monitor:99","email":"s@x.com","file":"sneaky.token","source":"monitor-db","materialized":true},
            {"name":"good","provider":"claude","upstream_id":"monitor:1","email":"g@x.com","file":"good.token","source":"monitor-db","materialized":true}
        ]}"#,
    );
    let mut rng = Rng::seeded(1);
    for _ in 0..10 {
        let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
        assert_eq!(sel.name, "good");
        assert_eq!(sel.mode, "random");
        assert_eq!(sel.upstream_id.as_deref(), Some("monitor:1"));
    }
}

/// Same exclusion applies to the ranked tier — a `.ranking` row marking a
/// non-claude account `available` must not be selected.
#[test]
fn non_claude_manifest_row_is_never_selected_ranked_tier() {
    let tmp = make_pool(&["good", "sneaky"]);
    write_index_json(
        &pool_dir(tmp.path()),
        r#"{"version":3,"accounts":[
            {"name":"sneaky","provider":"codex","upstream_id":"monitor:99","email":"s@x.com","file":"sneaky.token","source":"monitor-db","materialized":true},
            {"name":"good","provider":"claude","upstream_id":"monitor:1","email":"g@x.com","file":"good.token","source":"monitor-db","materialized":true}
        ]}"#,
    );
    fs::write(pool_dir(tmp.path()).join(".ranking"), "sneaky|available\ngood|available\n").unwrap();
    let mut rng = Rng::seeded(1);
    for _ in 0..10 {
        let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
        assert_eq!(sel.name, "good");
        assert_eq!(sel.mode, "ranked");
    }
}

/// A pool whose only account is a non-claude manifest row fails closed
/// (not silently, and never falls back to picking it): the empty-pool
/// error names the provider mismatch.
#[test]
fn non_claude_only_pool_errors_with_provider_detail() {
    let tmp = make_pool(&["sneaky"]);
    write_index_json(
        &pool_dir(tmp.path()),
        r#"{"version":3,"accounts":[
            {"name":"sneaky","provider":"codex","upstream_id":"monitor:99","email":"s@x.com","file":"sneaky.token","source":"monitor-db","materialized":true}
        ]}"#,
    );
    let mut rng = Rng::seeded(1);
    let err = select_token(tmp.path(), Some(&mut rng)).unwrap_err();
    assert!(err.0.contains("sneaky: hard-excluded"), "{}", err.0);
    assert!(err.0.contains("codex"), "{}", err.0);
}

/// A pool with no `index.json` at all still selects normally — the
/// fail-open path (D6/D8): absence of a manifest row is never treated as
/// "not claude".
#[test]
fn no_manifest_file_is_fail_open() {
    let tmp = make_pool(&["only"]);
    assert!(!pool_dir(tmp.path()).join("index.json").is_file());
    let mut rng = Rng::seeded(1);
    let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
    assert_eq!(sel.name, "only");
    assert_eq!(sel.upstream_id, None);
}

/// A manifest row present for the selected name but with no
/// `upstream_id` field (e.g. a hand-edited or older row) yields `None`,
/// never a fabricated identity.
#[test]
fn manifest_row_without_upstream_id_yields_none() {
    let tmp = make_pool(&["only"]);
    write_index_json(
        &pool_dir(tmp.path()),
        r#"{"version":3,"accounts":[
            {"name":"only","provider":"claude","email":"o@x.com","file":"only.token","source":"env","materialized":true}
        ]}"#,
    );
    let mut rng = Rng::seeded(1);
    let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
    assert_eq!(sel.upstream_id, None);
}

// =========================================================================
// spawnable_pool_state / pool_clear_estimate (issue #7607)
// =========================================================================

#[test]
#[serial]
fn spawnable_pool_state_reports_usable_when_healthy() {
    let tmp = make_pool(&["a", "b"]);
    std::env::set_var(super::super::paths::SHARED_TOKENS_DIR_ENV, "");
    let state = spawnable_pool_state(tmp.path());
    assert_eq!(state.dir, pool_dir(tmp.path()));
    assert_eq!(state.total, 2);
    assert_eq!(state.usable, 2);
    std::env::remove_var(super::super::paths::SHARED_TOKENS_DIR_ENV);
}

#[test]
#[serial]
fn spawnable_pool_state_reports_zero_usable_when_every_account_bad_marked() {
    let tmp = make_pool(&["a", "b"]);
    std::env::set_var(super::super::paths::SHARED_TOKENS_DIR_ENV, "");
    fs::write(
        pool_dir(tmp.path()).join(".bad_tokens"),
        format!(
            "{} a auth failure\n{} b auth failure\n",
            chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
            chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
        ),
    )
    .unwrap();
    let state = spawnable_pool_state(tmp.path());
    assert_eq!(state.total, 2);
    assert_eq!(state.usable, 0, "both accounts are auth-bad-marked");
    std::env::remove_var(super::super::paths::SHARED_TOKENS_DIR_ENV);
}

#[test]
#[serial]
fn spawnable_pool_state_reports_zero_usable_when_ranking_hard_excludes_everything() {
    let tmp = make_pool(&["a", "b"]);
    std::env::set_var(super::super::paths::SHARED_TOKENS_DIR_ENV, "");
    fs::write(pool_dir(tmp.path()).join(".ranking"), "a|exhausted\nb|blocked\n").unwrap();
    let state = spawnable_pool_state(tmp.path());
    assert_eq!(state.total, 2);
    assert_eq!(state.usable, 0);
    std::env::remove_var(super::super::paths::SHARED_TOKENS_DIR_ENV);
}

#[test]
#[serial]
fn spawnable_pool_state_falls_back_to_shared_pool() {
    let repo = tempfile::tempdir().unwrap();
    let shared = tempfile::tempdir().unwrap();
    fs::write(shared.path().join("s.token"), "key-s").unwrap();
    std::env::set_var(super::super::paths::SHARED_TOKENS_DIR_ENV, shared.path().to_str().unwrap());
    let state = spawnable_pool_state(repo.path());
    assert_eq!(state.dir, shared.path());
    assert_eq!(state.total, 1);
    assert_eq!(state.usable, 1);
    std::env::remove_var(super::super::paths::SHARED_TOKENS_DIR_ENV);
}

#[test]
fn pool_clear_estimate_defaults_to_the_cap_when_nothing_is_computable() {
    let tmp = make_pool(&["a"]);
    // Hard-excluded by `.ranking` with no `limit_reset` field, and no
    // `.bad_tokens` entry at all -> no computable clear time anywhere.
    fs::write(pool_dir(tmp.path()).join(".ranking"), "a|exhausted\n").unwrap();
    let now = chrono::Utc::now();
    let estimate = pool_clear_estimate(&pool_dir(tmp.path()));
    let delta = (estimate - now).num_seconds();
    assert!(
        (POOL_CLEAR_ESTIMATE_CAP_SECS - 5..=POOL_CLEAR_ESTIMATE_CAP_SECS).contains(&delta),
        "expected an estimate near the {POOL_CLEAR_ESTIMATE_CAP_SECS}s cap, got {delta}s"
    );
}

#[test]
fn pool_clear_estimate_uses_the_earliest_bad_tokens_cooldown() {
    let tmp = make_pool(&["a"]);
    // A fresh exhaustion entry: cooldown_remaining_secs is close to the
    // full `exhaustion_cooldown_secs()` window (6h by default) — well
    // short of the fresh timestamp meaning "just marked, nearly the full
    // cooldown remains" and well past the 900s cap, so the estimate must
    // be clamped to the cap rather than reporting hours out.
    fs::write(
        pool_dir(tmp.path()).join(".bad_tokens"),
        format!("{} a rate_limited\n", chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ")),
    )
    .unwrap();
    let now = chrono::Utc::now();
    let estimate = pool_clear_estimate(&pool_dir(tmp.path()));
    let delta = (estimate - now).num_seconds();
    assert!(
        (POOL_CLEAR_ESTIMATE_CAP_SECS - 5..=POOL_CLEAR_ESTIMATE_CAP_SECS).contains(&delta),
        "expected the estimate clamped to the {POOL_CLEAR_ESTIMATE_CAP_SECS}s cap, got {delta}s"
    );
}

#[test]
fn pool_clear_estimate_never_reports_a_time_before_now() {
    let tmp = make_pool(&["a"]);
    // A malformed-timestamp `.ranking` reset in the distant past must
    // still clamp forward to `now`, never report a negative delta.
    fs::write(pool_dir(tmp.path()).join(".ranking"), "a|exhausted||2020-01-01T00:00:00Z\n")
        .unwrap();
    let now = chrono::Utc::now();
    let estimate = pool_clear_estimate(&pool_dir(tmp.path()));
    assert!(estimate >= now, "estimate {estimate} must never be before now {now}");
}

// =======================================================================
// #8058 — per-model-class selection
// =======================================================================
//
// Each tier gets its own pair of assertions, because each has its own
// `.bad_tokens` skip site and a fix applied to only some of them would
// silently leave the starvation in place on the other paths.

/// Tier 1 (`.ranking`): an account marked bad for `opus` is still ranked
/// for `sonnet` work, and still skipped for `opus` work.
#[test]
#[serial]
fn ranked_tier_skips_only_the_marked_class() {
    let tmp = make_pool(&["a"]);
    fs::write(pool_dir(tmp.path()).join(".ranking"), "a|available\n").unwrap();
    super::super::bad_tokens::mark_bad_for_model(
        tmp.path(),
        "a",
        "exhausted: out of usage credits",
        Some("opus"),
    )
    .unwrap();

    let mut rng = Rng::seeded(1);
    let sel = select_token_for_class(tmp.path(), Some(&mut rng), Some("sonnet")).unwrap();
    assert_eq!(sel.name, "a");
    assert_eq!(sel.mode, "ranked");

    let mut rng = Rng::seeded(1);
    let err = select_token_for_class(tmp.path(), Some(&mut rng), Some("opus")).unwrap_err();
    assert!(err.0.contains("marked bad"), "{}", err.0);
    assert!(err.0.contains("model class: opus"), "{}", err.0);
}

/// Tier 2 (`.allowlist`).
#[test]
#[serial]
fn allowlist_tier_skips_only_the_marked_class() {
    let tmp = make_pool(&["a"]);
    fs::write(pool_dir(tmp.path()).join(".allowlist"), "a\n").unwrap();
    super::super::bad_tokens::mark_bad_for_model(
        tmp.path(),
        "a",
        "exhausted: out of usage credits",
        Some("opus"),
    )
    .unwrap();

    let mut rng = Rng::seeded(1);
    let sel = select_token_for_class(tmp.path(), Some(&mut rng), Some("sonnet")).unwrap();
    assert_eq!(sel.name, "a");
    assert_eq!(sel.mode, "allowlist");

    let mut rng = Rng::seeded(1);
    assert!(select_token_for_class(tmp.path(), Some(&mut rng), Some("opus")).is_err());
}

/// Tier 3 (`mode=random`, no `.ranking` and no `.allowlist`).
#[test]
#[serial]
fn random_tier_skips_only_the_marked_class() {
    let tmp = make_pool(&["a"]);
    super::super::bad_tokens::mark_bad_for_model(
        tmp.path(),
        "a",
        "exhausted: out of usage credits",
        Some("opus"),
    )
    .unwrap();

    let mut rng = Rng::seeded(1);
    let sel = select_token_for_class(tmp.path(), Some(&mut rng), Some("sonnet")).unwrap();
    assert_eq!(sel.name, "a");
    assert_eq!(sel.mode, "random");

    let mut rng = Rng::seeded(1);
    assert!(select_token_for_class(tmp.path(), Some(&mut rng), Some("opus")).is_err());
}

/// The regression guard: `None` (and therefore plain `select_token`)
/// behaves exactly as before #8058 — a class-scoped mark still blocks the
/// account-wide question, so no existing caller is silently narrowed.
#[test]
#[serial]
fn class_less_selection_is_unchanged_by_a_class_scoped_mark() {
    let tmp = make_pool(&["a"]);
    super::super::bad_tokens::mark_bad_for_model(
        tmp.path(),
        "a",
        "exhausted: out of usage credits",
        Some("opus"),
    )
    .unwrap();

    let mut rng = Rng::seeded(1);
    let err = select_token(tmp.path(), Some(&mut rng)).unwrap_err();
    assert!(err.0.contains("marked bad"), "{}", err.0);
    assert!(!err.0.contains("model class:"), "{}", err.0);

    let mut rng = Rng::seeded(1);
    assert!(select_token_for_class(tmp.path(), Some(&mut rng), None).is_err());
}

/// A class-LESS mark keeps blocking every class — the widening direction
/// must never be narrowed by the new flag.
#[test]
#[serial]
fn class_less_mark_still_blocks_every_class() {
    let tmp = make_pool(&["a"]);
    super::super::bad_tokens::mark_bad(tmp.path(), "a", "exhausted: hit your weekly limit")
        .unwrap();
    for class in ["opus", "sonnet", "haiku", "fable"] {
        let mut rng = Rng::seeded(1);
        assert!(
            select_token_for_class(tmp.path(), Some(&mut rng), Some(class)).is_err(),
            "class-less mark must still block {class}"
        );
    }
}

/// `select_token_for_model` is the entry point `tokens select --model` uses:
/// it takes a RAW model and classifies it here, so the CLI, the wrapper and the
/// spawner all share one definition of "which class is this".
#[test]
#[serial]
fn select_token_for_model_classifies_a_pinned_id() {
    let tmp = make_pool(&["a"]);
    super::super::bad_tokens::mark_bad_for_model(
        tmp.path(),
        "a",
        "exhausted: out of usage credits",
        Some("opus"),
    )
    .unwrap();

    let mut rng = Rng::seeded(1);
    let sel =
        select_token_for_model(tmp.path(), Some(&mut rng), Some("claude-sonnet-4-6")).unwrap();
    assert_eq!(sel.name, "a");

    let mut rng = Rng::seeded(1);
    assert!(select_token_for_model(tmp.path(), Some(&mut rng), Some("claude-opus-5")).is_err());
}

/// An unrecognized model must NOT fail selection closed — it degrades to
/// account-wide behaviour. A spawn is never allowed to die because the pool
/// did not recognize a model name.
#[test]
#[serial]
fn an_unrecognized_model_degrades_to_account_wide_selection() {
    let tmp = make_pool(&["a"]);
    super::super::bad_tokens::mark_bad_for_model(
        tmp.path(),
        "a",
        "exhausted: out of usage credits",
        Some("opus"),
    )
    .unwrap();

    // Account-wide: the opus-scoped entry blocks, exactly as `select_token`.
    let mut rng = Rng::seeded(1);
    assert!(select_token_for_model(tmp.path(), Some(&mut rng), Some("gpt-5")).is_err());

    // And an unrecognized model against a CLEAN pool still selects.
    let clean = make_pool(&["b"]);
    let mut rng = Rng::seeded(1);
    let sel = select_token_for_model(clean.path(), Some(&mut rng), Some("gpt-5")).unwrap();
    assert_eq!(sel.name, "b");
}

/// An empty pool fails identically with or without a class — the flag
/// never changes the empty-pool contract.
#[test]
#[serial]
fn empty_pool_errors_identically_with_a_class() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir_all(pool_dir(tmp.path())).unwrap();
    std::env::set_var(super::super::paths::SHARED_TOKENS_DIR_ENV, "");
    let err = select_token_for_class(tmp.path(), None, Some("sonnet")).unwrap_err();
    assert!(err.0.contains("No .token files"), "{}", err.0);
    std::env::remove_var(super::super::paths::SHARED_TOKENS_DIR_ENV);
}
