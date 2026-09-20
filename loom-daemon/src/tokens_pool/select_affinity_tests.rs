//! Unit tests for the prompt-cache affinity preference tier (issue #8146) as
//! it behaves *inside* [`super`]'s 3-tier selection algorithm.
//!
//! A sibling of [`super::tests`] rather than a section of it, per
//! `.loom/docs/file-size-policy.md`: new code goes in a new module instead of
//! growing an over-threshold file. The pool fixtures are shared — see
//! [`super::tests::make_pool`].
//!
//! `super::super::affinity`'s own unit tests cover the preference's bounds in
//! isolation (TTL, quota guard, role scope, config precedence, state-file
//! hygiene). What is tested HERE is the property that made this issue
//! `complex`: the preference must only ever reorder accounts the existing
//! 3-tier algorithm already admitted, and must leave an unconfigured pool
//! bit-for-bit unchanged.

use super::tests::{make_pool, pool_dir};
use super::*;
use std::fs;
use std::path::Path;

use super::super::affinity as cache_affinity;

/// Enable affinity for `ws` via a real `.loom/config.json` — no process-global
/// env var, so these tests need no `#[serial]` and cannot perturb a concurrent
/// test's selection.
fn enable_affinity(ws: &Path, extra_keys: &str) {
    fs::write(
        ws.join(".loom").join("config.json"),
        format!(r#"{{"tokens": {{"cacheAffinity": {{"enabled": true{extra_keys}}}}}}}"#),
    )
    .unwrap();
}

/// Write an affinity record for `(ws, role)` aged `age_secs`, bypassing
/// `Affinity::record` so the test controls the timestamp.
fn seed_affinity(ws: &Path, role: &str, account: &str, age_secs: i64) {
    let at = (chrono::Utc::now() - chrono::Duration::seconds(age_secs))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let body = serde_json::json!({
        cache_affinity::affinity_key(ws, role): { "account": account, "at": at },
    });
    fs::write(pool_dir(ws).join(cache_affinity::STATE_FILE), format!("{body}\n")).unwrap();
}

/// Pin the rotation cursor so a tier-1/tier-3 draw is deterministic.
fn pin_rotation_cursor(ws: &Path) {
    fs::write(pool_dir(ws).join(".rotation_cursor"), "0").unwrap();
}

/// Give every named account a measured 5h utilization. The quota guard
/// *requires* one before it will express any preference at all (see
/// `affinity::under_quota_guard`), so every test below that expects affinity
/// to actually fire — and, just as importantly, every test that expects some
/// *other* rule to be what blocks it — has to rank its accounts. Without this
/// the guard alone would withdraw the preference and the exclusion assertions
/// would pass vacuously.
fn rank_all(ws: &Path, names: &[&str], util: f64) {
    let body: String = names
        .iter()
        .map(|n| format!("{n}|available|{util}\n"))
        .collect();
    fs::write(pool_dir(ws).join(".ranking"), body).unwrap();
}

/// Backdate `.ranking` past `RANKING_FRESH_SECONDS` so tier 1 steps aside and
/// the allowlist/random tiers run — while the utilization figures stay
/// readable for the quota guard, which (unlike tier 1) ignores ranking age.
fn make_ranking_stale(ws: &Path) {
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(700);
    fs::File::open(pool_dir(ws).join(".ranking"))
        .unwrap()
        .set_modified(old)
        .unwrap();
}

#[test]
fn affinity_reuses_the_account_that_last_warmed_this_repo_role() {
    let tmp = make_pool(&["a", "b", "c", "d"]);
    enable_affinity(tmp.path(), "");
    rank_all(tmp.path(), &["a", "b", "c", "d"], 0.10);
    let mut rng = Rng::seeded(7);
    let first = select_token_for_role(tmp.path(), Some(&mut rng), Some("judge")).unwrap();
    for _ in 0..10 {
        let sel = select_token_for_role(tmp.path(), Some(&mut rng), Some("judge")).unwrap();
        assert_eq!(
            sel.name, first.name,
            "every subsequent judge spawn must land on the account that warmed the cache"
        );
    }
    // The key is (workspace, role): a second role gets its own record and is
    // likewise stable, rather than inheriting judge's.
    let guide = select_token_for_role(tmp.path(), Some(&mut rng), Some("guide")).unwrap();
    for _ in 0..5 {
        let sel = select_token_for_role(tmp.path(), Some(&mut rng), Some("guide")).unwrap();
        assert_eq!(sel.name, guide.name);
    }
    assert_eq!(
        cache_affinity::Affinity::resolve(tmp.path(), &pool_dir(tmp.path()), Some("judge"))
            .preferred(),
        Some(first.name.as_str()),
        "the judge record must survive intervening spawns of another role"
    );
}

#[test]
fn affinity_is_honored_inside_the_ranked_tier() {
    let tmp = make_pool(&["a", "b", "c"]);
    enable_affinity(tmp.path(), "");
    fs::write(
        pool_dir(tmp.path()).join(".ranking"),
        "a|available|0.10\nb|available|0.10\nc|available|0.10\n",
    )
    .unwrap();
    seed_affinity(tmp.path(), "judge", "c", 60);
    let mut rng = Rng::seeded(3);
    for _ in 0..5 {
        let sel = select_token_for_role(tmp.path(), Some(&mut rng), Some("judge")).unwrap();
        assert_eq!(sel.name, "c");
        assert_eq!(sel.mode, "ranked", "affinity must not change which tier fired");
    }
}

#[test]
fn affinity_never_selects_a_bad_marked_account() {
    let tmp = make_pool(&["a", "b"]);
    enable_affinity(tmp.path(), "");
    // `a` is ranked and comfortably under the quota guard, so the guard is NOT
    // what withholds it — being bad-marked is.
    rank_all(tmp.path(), &["a", "b"], 0.10);
    super::super::bad_tokens::mark_bad(tmp.path(), "a", "exhausted").unwrap();
    seed_affinity(tmp.path(), "judge", "a", 60);
    let mut rng = Rng::seeded(1);
    for _ in 0..10 {
        let sel = select_token_for_role(tmp.path(), Some(&mut rng), Some("judge")).unwrap();
        assert_eq!(sel.name, "b", "a bad-marked affine account must still be passed over");
    }
}

#[test]
fn affinity_never_selects_a_ranking_hard_excluded_account() {
    let tmp = make_pool(&["a", "b"]);
    enable_affinity(tmp.path(), "");
    // `a` is the affine account AND `exhausted` — the #5629 hard set, which no
    // tier and no fail-safe readmits. Its utilization is deliberately low so
    // the quota guard admits the preference and hard exclusion is provably the
    // rule doing the work.
    fs::write(pool_dir(tmp.path()).join(".ranking"), "a|exhausted|0.10\nb|available|0.10\n")
        .unwrap();
    seed_affinity(tmp.path(), "judge", "a", 60);
    let mut rng = Rng::seeded(1);
    for _ in 0..10 {
        let sel = select_token_for_role(tmp.path(), Some(&mut rng), Some("judge")).unwrap();
        assert_eq!(sel.name, "b");
    }
    // …and with `b` gone too, selection fails fast rather than letting
    // affinity resurrect the hard-excluded account.
    fs::write(pool_dir(tmp.path()).join(".ranking"), "a|exhausted\nb|exhausted\n").unwrap();
    let err = select_token_for_role(tmp.path(), Some(&mut rng), Some("judge")).unwrap_err();
    assert!(err.0.contains("a: hard-excluded by .ranking status"), "{}", err.0);
}

#[test]
fn affinity_never_selects_an_account_outside_the_allowlist() {
    let tmp = make_pool(&["a", "b", "c"]);
    enable_affinity(tmp.path(), "");
    // A *stale* ranking: tier 1 steps aside so the allowlist tier is the one
    // that fires, while `c` still carries a low utilization so the quota guard
    // admits the preference — leaving `.allowlist` as the only thing that can
    // hold `c` back.
    rank_all(tmp.path(), &["a", "b", "c"], 0.10);
    make_ranking_stale(tmp.path());
    fs::write(pool_dir(tmp.path()).join(".allowlist"), "b\n").unwrap();
    seed_affinity(tmp.path(), "judge", "c", 60);
    let mut rng = Rng::seeded(1);
    for _ in 0..10 {
        let sel = select_token_for_role(tmp.path(), Some(&mut rng), Some("judge")).unwrap();
        assert_eq!(sel.name, "b", "a pinned-out affine account must stay pinned out");
        assert_eq!(sel.mode, "allowlist");
    }
    // Control: add `c` to the allowlist and the very same preference is
    // honoured — proving the loop above was the pin, not a withdrawn guard.
    // Re-seeded because each draw above recorded `b`, the account it actually
    // used (see `affinity_steps_aside_once_the_quota_guard_trips`).
    fs::write(pool_dir(tmp.path()).join(".allowlist"), "b\nc\n").unwrap();
    seed_affinity(tmp.path(), "judge", "c", 60);
    let sel = select_token_for_role(tmp.path(), Some(&mut rng), Some("judge")).unwrap();
    assert_eq!(sel.name, "c");
    assert_eq!(sel.mode, "allowlist");
}

#[test]
fn affinity_steps_aside_once_the_quota_guard_trips() {
    let tmp = make_pool(&["a", "b"]);
    enable_affinity(tmp.path(), "");
    // `a` is affine but past the default 0.50 quota guard (and still under the
    // 0.70 tier-1 load gate, so it remains *selectable* — only the preference
    // is withdrawn).
    fs::write(pool_dir(tmp.path()).join(".ranking"), "a|available|0.60\nb|available|0.10\n")
        .unwrap();
    seed_affinity(tmp.path(), "judge", "a", 60);
    pin_rotation_cursor(tmp.path());
    let mut rng = Rng::seeded(11);
    let drawn: Vec<String> = (0..3)
        .map(|_| {
            select_token_for_role(tmp.path(), Some(&mut rng), Some("judge"))
                .unwrap()
                .name
        })
        .collect();
    assert!(
        drawn.iter().any(|n| n == "b"),
        "the rotation cursor must still spread once the quota guard withdraws the \
         preference, got {drawn:?}"
    );
    // Control: drop `a` back under the threshold and the identical record now
    // pins every draw to it. Without this the assertion above would also hold
    // if affinity were simply broken.
    //
    // The re-seed is required, not cosmetic: each draw above *re-recorded* the
    // account it actually landed on, which is the behavior that makes the
    // preference self-correcting — once the guard pushes a key off a loaded
    // account, the key adopts its replacement instead of waiting for the
    // original to cool down.
    fs::write(pool_dir(tmp.path()).join(".ranking"), "a|available|0.40\nb|available|0.10\n")
        .unwrap();
    seed_affinity(tmp.path(), "judge", "a", 60);
    for _ in 0..5 {
        let sel = select_token_for_role(tmp.path(), Some(&mut rng), Some("judge")).unwrap();
        assert_eq!(sel.name, "a", "under the guard, the affine account is preferred");
    }
}

/// The threshold is a strict `<`, so an account exactly *at* it is withheld.
/// Both neighbours of the boundary are asserted, because an off-by-one here is
/// precisely the "mis-tuned quota guard" defect that would pass a looser test.
#[test]
fn quota_guard_boundary_is_exclusive() {
    for (util, expected) in [("0.49", "a"), ("0.50", "b")] {
        let tmp = make_pool(&["a", "b"]);
        enable_affinity(tmp.path(), "");
        // `b` is listed first and the cursor is pinned to 0, so *rotation*
        // yields `b` while *affinity* yields `a`. Without that split the two
        // outcomes would be indistinguishable and the boundary untested.
        fs::write(
            pool_dir(tmp.path()).join(".ranking"),
            format!("b|available|0.10\na|available|{util}\n"),
        )
        .unwrap();
        seed_affinity(tmp.path(), "judge", "a", 60);
        pin_rotation_cursor(tmp.path());
        let mut rng = Rng::seeded(2);
        let sel = select_token_for_role(tmp.path(), Some(&mut rng), Some("judge")).unwrap();
        assert_eq!(sel.name, expected, "at 5h_util={util} the pick must be {expected}");
    }
}

/// The record always follows the account that *actually ran*, never the one
/// that was merely preferred. This is what keeps a withdrawn preference from
/// costing two cache misses instead of one: the moment some rule pushes a key
/// off its affine account, the replacement becomes the new affine account and
/// the key warms up there, rather than thrashing back on every tick.
#[test]
fn the_record_follows_the_account_that_actually_ran() {
    let tmp = make_pool(&["a", "b"]);
    enable_affinity(tmp.path(), "");
    rank_all(tmp.path(), &["a", "b"], 0.10);
    // `a` is affine but bad-marked, so `b` is what the pool hands out.
    super::super::bad_tokens::mark_bad(tmp.path(), "a", "exhausted").unwrap();
    seed_affinity(tmp.path(), "judge", "a", 60);
    let mut rng = Rng::seeded(6);
    let sel = select_token_for_role(tmp.path(), Some(&mut rng), Some("judge")).unwrap();
    assert_eq!(sel.name, "b");
    assert_eq!(
        cache_affinity::Affinity::resolve(tmp.path(), &pool_dir(tmp.path()), Some("judge"))
            .preferred(),
        Some("b"),
        "the key must adopt the account it actually ran on, not keep pointing at `a`"
    );
}

/// Selection must never *fail* because affinity is on: with the affine account
/// the only one left and every rule against it, the pool still hands out what
/// it would have handed out anyway.
#[test]
fn affinity_never_turns_a_live_pool_into_an_empty_one() {
    let tmp = make_pool(&["a"]);
    enable_affinity(tmp.path(), "");
    // `a` is affine, is the only account, and is over the quota guard.
    fs::write(pool_dir(tmp.path()).join(".ranking"), "a|available|0.95\n").unwrap();
    seed_affinity(tmp.path(), "judge", "a", 60);
    let mut rng = Rng::seeded(4);
    let sel = select_token_for_role(tmp.path(), Some(&mut rng), Some("judge")).unwrap();
    assert_eq!(sel.name, "a", "withdrawing the preference must not withdraw the account");
}

/// A single draw against two byte-identical pools — one with affinity
/// configured, one without — must pick the same account, which is the
/// operational meaning of "the feature is inert".
fn single_draw(enable: bool, seeded_record: Option<(&str, i64)>) -> String {
    let tmp = make_pool(&["a", "b", "c"]);
    pin_rotation_cursor(tmp.path());
    rank_all(tmp.path(), &["a", "b", "c"], 0.10);
    if enable {
        enable_affinity(tmp.path(), "");
    }
    if let Some((account, age)) = seeded_record {
        seed_affinity(tmp.path(), "judge", account, age);
    }
    let mut rng = Rng::seeded(5);
    select_token_for_role(tmp.path(), Some(&mut rng), Some("judge"))
        .unwrap()
        .name
}

#[test]
fn affinity_past_the_ttl_leaves_selection_unchanged() {
    let expired = (cache_affinity::DEFAULT_TTL_SECS as i64) + 60;
    assert_eq!(
        single_draw(true, Some(("c", expired))),
        single_draw(false, None),
        "a record older than the TTL must contribute nothing to the pick"
    );
    // Control: the same record inside the window DOES steer the pick, so the
    // equality above is the TTL working, not the harness failing to seed.
    assert_eq!(single_draw(true, Some(("c", 60))), "c");
}

#[test]
fn unconfigured_pool_is_untouched_and_grows_no_state_file() {
    let tmp = make_pool(&["a", "b", "c"]);
    pin_rotation_cursor(tmp.path());
    rank_all(tmp.path(), &["a", "b", "c"], 0.10);
    let mut rng = Rng::seeded(5);
    // Same seed + same pinned cursor + same ranking as `single_draw` — a
    // role-carrying call on an unconfigured pool is the same pick as the
    // role-less legacy call.
    let with_role = select_token_for_role(tmp.path(), Some(&mut rng), Some("judge")).unwrap();
    assert_eq!(with_role.name, single_draw(false, None));
    assert!(
        !pool_dir(tmp.path())
            .join(cache_affinity::STATE_FILE)
            .exists(),
        "an unconfigured pool must never grow a {} file",
        cache_affinity::STATE_FILE
    );
    // …and neither does the legacy entry point, whatever the config says.
    enable_affinity(tmp.path(), "");
    let mut rng = Rng::seeded(5);
    select_token(tmp.path(), Some(&mut rng)).unwrap();
    assert!(!pool_dir(tmp.path())
        .join(cache_affinity::STATE_FILE)
        .exists());
}
