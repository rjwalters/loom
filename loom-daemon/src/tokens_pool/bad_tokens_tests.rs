//! Unit tests for [`super`] — `tokens_pool::bad_tokens`.
//!
//! Extracted verbatim from `bad_tokens.rs` (issue #8058) so that file need not
//! grow; a test module is the cleanest thing to move. `bad_tokens.rs` declares it with
//! `#[cfg(test)] #[path = "bad_tokens_tests.rs"] mod tests;`, so `super::*`
//! below still resolves to `bad_tokens` exactly as before.

use super::*;
use serial_test::serial;
use std::fs;

fn make_pool() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join(".loom").join("tokens");
    fs::create_dir_all(&dir).unwrap();
    // resolve_tokens_dir() only picks the per-repo pool when it holds at
    // least one `*.token` file — seed one so these tests deterministically
    // exercise the per-repo pool rather than falling back to this host's
    // real shared pool (~/.loom/tokens) when it is empty.
    fs::write(dir.join("seed.token"), "sk-ant-oat01-fake").unwrap();
    tmp
}

fn pool_dir(ws: &Path) -> PathBuf {
    ws.join(".loom").join("tokens")
}

#[test]
fn mark_and_check_bad() {
    let tmp = make_pool();
    assert!(!is_bad(tmp.path(), "agent-1"));
    mark_bad(tmp.path(), "agent-1", "exhausted: 429").unwrap();
    assert!(is_bad(tmp.path(), "agent-1"));
}

#[test]
fn word_boundary_does_not_confuse_agent_1_and_agent_10() {
    let tmp = make_pool();
    mark_bad(tmp.path(), "agent-10", "auth").unwrap();
    assert!(!is_bad(tmp.path(), "agent-1"));
    assert!(is_bad(tmp.path(), "agent-10"));
}

#[test]
fn is_bad_false_when_file_missing() {
    let tmp = make_pool();
    assert!(!is_bad(tmp.path(), "agent-1"));
}

/// #4122: an exhaustion (non-auth) entry older than the cooldown no longer
/// reports `is_bad`, while an auth entry of the same age remains permanent.
#[test]
// Reads the process-global cooldown default, so it must not overlap the
// `#[serial]` tests that mutate `EXHAUSTION_COOLDOWN_ENV`.
#[serial]
fn exhaustion_entry_expires_after_cooldown_auth_stays() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    // Both entries are 7h old — past the 6h default cooldown.
    let old = (Utc::now() - chrono::Duration::seconds(7 * 3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(
        dir.join(".bad_tokens"),
        format!(
            "{old} agent-exh exhausted: weekly limit\n\
             {old} agent-auth 401 unauthorized\n"
        ),
    )
    .unwrap();
    // Exhaustion entry has aged out — token is selectable again even though
    // the line is still on disk (cleanup has not run yet).
    assert!(!is_bad(tmp.path(), "agent-exh"));
    assert_eq!(blocking_entry(tmp.path(), "agent-exh"), None);
    // Auth entry is permanent — unaffected by the TTL.
    assert!(is_bad(tmp.path(), "agent-auth"));
    // #4643: the same scan now also explains itself. The auth entry is
    // reported as permanent with no cooldown remaining.
    let entry = blocking_entry(tmp.path(), "agent-auth").expect("auth entry blocks");
    assert_eq!(entry.class, BadReasonClass::Auth);
    assert_eq!(entry.class.permanence(), "permanent");
    assert_eq!(entry.cooldown_remaining_secs, None);
    assert_eq!(entry.reason, "401 unauthorized");
    assert_eq!(entry.timestamp, old);
}

/// #4643: a *fresh* exhaustion entry reports its class, its own timestamp,
/// and how long is left on the cooldown — the detail the empty-pool error
/// renders per token.
#[test]
// Reads the process-global cooldown default, so it must not overlap the
// `#[serial]` tests that mutate `EXHAUSTION_COOLDOWN_ENV`.
#[serial]
fn blocking_entry_reports_exhaustion_class_and_cooldown_remaining() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    // 1h old → 5h of the 6h default cooldown remain.
    let ts = (Utc::now() - chrono::Duration::seconds(3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(
        dir.join(".bad_tokens"),
        format!("{ts} agent-1 exhausted: hit your session limit\n"),
    )
    .unwrap();
    let entry = blocking_entry(tmp.path(), "agent-1").expect("fresh entry blocks");
    assert_eq!(entry.class, BadReasonClass::Exhaustion);
    assert_eq!(entry.class.label(), "exhaustion");
    assert_eq!(entry.class.permanence(), "TTL");
    assert_eq!(entry.timestamp, ts);
    assert_eq!(entry.reason, "exhausted: hit your session limit");
    let remaining = entry
        .cooldown_remaining_secs
        .expect("TTL entry has a remaining");
    // #7748: `remaining` is derived from an internal `Utc::now()` call
    // inside `blocking_entry`, taken some real time after the `Utc::now()`
    // above that produced `ts` — normally microseconds apart, but a
    // CI-runner wall-clock jump between the two calls widens that gap
    // unpredictably (observed once: `remaining=14399`, i.e. an apparent
    // ~1h jump, one second under the previous 4h floor). The lower bound
    // is deliberately loose — down to 3h instead of a tight-to-5h window —
    // to tolerate at least a ~2h clock jump between the two calls without
    // masking a real regression in the cooldown-remaining calculation
    // (which would still show up as `remaining` far outside 3h..=5h, e.g.
    // ~0 or negative, or unchanged at ~6h).
    assert!(
        (3 * 3600..=5 * 3600).contains(&remaining),
        "expected ~5h remaining, got {remaining}"
    );
}

/// #4643: the incident shape — an account whose *oldest* visible entry has
/// long expired but which was re-marked recently is still blocked, and the
/// entry reported is the deciding (fresh) one, not the stale one an
/// operator would see at the top of the file.
#[test]
// Reads the process-global cooldown default, so it must not overlap the
// `#[serial]` tests that mutate `EXHAUSTION_COOLDOWN_ENV`.
#[serial]
fn blocking_entry_reports_the_deciding_fresh_line_not_the_stale_one() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    let old = (Utc::now() - chrono::Duration::seconds(13 * 3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let fresh = (Utc::now() - chrono::Duration::seconds(600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(
        dir.join(".bad_tokens"),
        format!(
            "{old} agent-1 exhausted: hit your limit\n\
             {fresh} agent-1 exhausted: hit your weekly limit\n"
        ),
    )
    .unwrap();
    let entry = blocking_entry(tmp.path(), "agent-1").expect("re-marked account blocks");
    assert_eq!(entry.timestamp, fresh);
    assert_eq!(entry.reason, "exhausted: hit your weekly limit");
}

/// #4643: an unparseable timestamp is reported as fail-closed permanent,
/// matching [`is_bad`]'s long-standing behavior.
#[test]
fn blocking_entry_classifies_malformed_timestamp_as_permanent() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    fs::write(dir.join(".bad_tokens"), "not-a-timestamp agent-1 exhausted\n").unwrap();
    let entry = blocking_entry(tmp.path(), "agent-1").expect("malformed line fails closed");
    assert_eq!(entry.class, BadReasonClass::MalformedTimestamp);
    assert_eq!(entry.cooldown_remaining_secs, None);
    assert!(entry.class.permanence().starts_with("permanent"));
}

/// #4122: a fresh exhaustion entry still blocks (the cooldown only expires
/// aged entries).
#[test]
// Reads the process-global cooldown default, so it must not overlap the
// `#[serial]` tests that mutate `EXHAUSTION_COOLDOWN_ENV`.
#[serial]
fn fresh_exhaustion_entry_still_blocks() {
    let tmp = make_pool();
    mark_bad(tmp.path(), "agent-1", "exhausted: weekly limit").unwrap();
    assert!(is_bad(tmp.path(), "agent-1"));
}

/// #4122: a matching line with an unparseable timestamp fails closed
/// (treated as permanently bad) so a malformed entry never un-blocks a
/// token.
#[test]
fn malformed_timestamp_fails_closed() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    fs::write(dir.join(".bad_tokens"), "not-a-timestamp agent-1 exhausted\n").unwrap();
    assert!(is_bad(tmp.path(), "agent-1"));
}

#[test]
#[serial]
fn exhaustion_cooldown_env_override() {
    std::env::set_var(EXHAUSTION_COOLDOWN_ENV, "100");
    assert_eq!(exhaustion_cooldown_secs(), 100);
    std::env::set_var(EXHAUSTION_COOLDOWN_ENV, "0");
    assert_eq!(exhaustion_cooldown_secs(), DEFAULT_EXHAUSTION_COOLDOWN_SECS);
    std::env::set_var(EXHAUSTION_COOLDOWN_ENV, "garbage");
    assert_eq!(exhaustion_cooldown_secs(), DEFAULT_EXHAUSTION_COOLDOWN_SECS);
    std::env::remove_var(EXHAUSTION_COOLDOWN_ENV);
    assert_eq!(exhaustion_cooldown_secs(), DEFAULT_EXHAUSTION_COOLDOWN_SECS);
}

/// #4643: the *reported* cooldown remaining tracks the
/// [`EXHAUSTION_COOLDOWN_ENV`] override, not just the default — otherwise
/// the empty-pool error would tell an operator who shortened the cooldown
/// to wait hours that do not apply.
#[test]
#[serial]
fn blocking_entry_cooldown_remaining_honors_env_override() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    // 10 minutes old.
    let ts = (Utc::now() - chrono::Duration::seconds(600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(
        dir.join(".bad_tokens"),
        format!("{ts} agent-1 exhausted: hit your session limit\n"),
    )
    .unwrap();

    // 30-minute cooldown → ~20 minutes left (default 6h would report ~5h50m).
    std::env::set_var(EXHAUSTION_COOLDOWN_ENV, "1800");
    let entry = blocking_entry(tmp.path(), "agent-1").expect("still inside a 30m cooldown");
    let remaining = entry.cooldown_remaining_secs.expect("TTL entry");
    // 15-minute cooldown → already expired, so the token is selectable.
    std::env::set_var(EXHAUSTION_COOLDOWN_ENV, "300");
    let expired = blocking_entry(tmp.path(), "agent-1");
    std::env::remove_var(EXHAUSTION_COOLDOWN_ENV);

    assert!(
        (1100..=1200).contains(&remaining),
        "expected ~20m remaining under the override, got {remaining}s"
    );
    assert_eq!(expired, None, "a 300s cooldown should have expired a 600s-old entry");
}

#[test]
fn mark_bad_strips_newlines_from_reason() {
    let tmp = make_pool();
    mark_bad(tmp.path(), "agent-1", "line1\nline2\r\n").unwrap();
    let text = fs::read_to_string(pool_dir(tmp.path()).join(".bad_tokens")).unwrap();
    assert_eq!(text.lines().count(), 1);
    assert!(text.contains("line1 line2"));
}

#[test]
#[serial]
fn mark_bad_errors_when_dir_missing() {
    // Neither the per-repo pool nor the shared pool exists here — disable
    // the shared fallback so this doesn't resolve to this host's real
    // ~/.loom/tokens (see `super::super::paths::SHARED_TOKENS_DIR_ENV`).
    std::env::set_var(super::super::paths::SHARED_TOKENS_DIR_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    let err = mark_bad(&tmp.path().join("nope"), "agent-1", "x");
    std::env::remove_var(super::super::paths::SHARED_TOKENS_DIR_ENV);
    assert!(err.is_err());
}

#[test]
fn cleanup_drops_old_entries_keeps_fresh() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    let old = (Utc::now() - chrono::Duration::seconds(1000))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let fresh = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    fs::write(
        dir.join(".bad_tokens"),
        format!("{old} agent-old exhausted\n{fresh} agent-new exhausted\n"),
    )
    .unwrap();

    let kept = cleanup_bad_tokens(tmp.path(), 500).unwrap();
    assert_eq!(kept, 1);
    assert!(!is_bad(tmp.path(), "agent-old"));
    assert!(is_bad(tmp.path(), "agent-new"));
}

#[test]
fn cleanup_keeps_malformed_lines() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    fs::write(dir.join(".bad_tokens"), "garbage line with no timestamp\n").unwrap();
    let kept = cleanup_bad_tokens(tmp.path(), 1).unwrap();
    assert_eq!(kept, 1);
}

#[test]
fn cleanup_no_file_is_noop() {
    let tmp = make_pool();
    assert_eq!(cleanup_bad_tokens(tmp.path(), 100).unwrap(), 0);
}

/// #4643: the wired path — exactly the call `loom-daemon tokens select`
/// makes — prunes an over-age exhaustion entry off disk while leaving a
/// recent one alone. Before #4643 `cleanup_bad_tokens` had zero callers, so
/// pools accumulated expired entries indefinitely.
#[test]
// The final `is_bad` assert relies on the 600s-old "recent" entry still
// blocking under the *default* 6h cooldown, so it must not overlap the
// `#[serial]` tests that transiently shrink `EXHAUSTION_COOLDOWN_ENV` to
// 100s/300s/1800s — a concurrent run inside a sub-600s window would make
// that entry read as expired.
#[serial]
fn wired_cleanup_prunes_over_age_exhaustion_entry() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    let ancient = (Utc::now() - chrono::Duration::seconds(DEFAULT_CLEANUP_MAX_AGE_SECS + 3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let recent = (Utc::now() - chrono::Duration::seconds(600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(
        dir.join(".bad_tokens"),
        format!(
            "{ancient} agent-old exhausted: hit your session limit\n\
             {recent} agent-new exhausted: hit your session limit\n"
        ),
    )
    .unwrap();

    let kept = cleanup_bad_tokens(tmp.path(), DEFAULT_CLEANUP_MAX_AGE_SECS).unwrap();
    assert_eq!(kept, 1);
    let text = fs::read_to_string(dir.join(".bad_tokens")).unwrap();
    assert!(!text.contains("agent-old"), "over-age entry still on disk: {text}");
    assert!(text.contains("agent-new"));
    assert!(is_bad(tmp.path(), "agent-new"));
}

/// #4643: auth entries are held for [`AUTH_ENTRY_MIN_RETENTION_SECS`]
/// regardless of the requested max age — pruning one early would silently
/// readmit a broken credential, since `is_bad` treats auth as permanent.
#[test]
fn cleanup_holds_auth_entries_past_the_requested_max_age() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    // Older than the routine 8-day policy (#13893 raised it from 24h so the
    // on-disk clock can never expire a live weekly hold), far younger than
    // the 30d auth floor.
    let old = (Utc::now() - chrono::Duration::seconds(9 * 24 * 3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(
        dir.join(".bad_tokens"),
        format!(
            "{old} agent-auth 401 unauthorized\n\
             {old} agent-exh exhausted: hit your session limit\n"
        ),
    )
    .unwrap();

    let outcome = cleanup_bad_tokens_in_dir(&dir, DEFAULT_CLEANUP_MAX_AGE_SECS).unwrap();
    assert_eq!(outcome.removed, 1);
    assert_eq!(outcome.kept, 1);
    let text = fs::read_to_string(dir.join(".bad_tokens")).unwrap();
    assert!(text.contains("agent-auth"), "auth entry was pruned early: {text}");
    assert!(!text.contains("agent-exh"));
    // Read-time semantics are unchanged: auth still blocks, and the
    // pruned exhaustion entry had already stopped blocking.
    assert!(is_bad(tmp.path(), "agent-auth"));
    assert!(!is_bad(tmp.path(), "agent-exh"));
}

/// #4643: an auth entry past the 30d floor is finally reclaimed (garbage
/// collection of a credential retired a month ago), not held forever.
#[test]
fn cleanup_reclaims_auth_entries_past_the_retention_floor() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    let ancient = (Utc::now()
        - chrono::Duration::seconds(AUTH_ENTRY_MIN_RETENTION_SECS + 24 * 3600))
    .format("%Y-%m-%dT%H:%M:%SZ")
    .to_string();
    fs::write(dir.join(".bad_tokens"), format!("{ancient} agent-auth 401 unauthorized\n")).unwrap();
    let outcome = cleanup_bad_tokens_in_dir(&dir, DEFAULT_CLEANUP_MAX_AGE_SECS).unwrap();
    assert_eq!(outcome.removed, 1);
    assert_eq!(outcome.kept, 0);
}

/// #4643: nothing prunable ⇒ no rewrite at all (the `tokens select` hot
/// path must not serialize a spawn burst on the `.bad_tokens` lock).
#[test]
fn cleanup_does_not_rewrite_when_nothing_is_prunable() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    mark_bad(tmp.path(), "agent-1", "exhausted: hit your session limit").unwrap();
    let before = fs::metadata(dir.join(".bad_tokens"))
        .unwrap()
        .modified()
        .unwrap();
    let outcome = cleanup_bad_tokens_in_dir(&dir, DEFAULT_CLEANUP_MAX_AGE_SECS).unwrap();
    assert_eq!(outcome.removed, 0);
    assert_eq!(outcome.kept, 1);
    let after = fs::metadata(dir.join(".bad_tokens"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(before, after, "file was rewritten with nothing to prune");
    // No lock directory was left behind either.
    assert!(!dir.join(".bad_tokens.lock").exists());
}

#[test]
fn unblock_no_file_is_noop() {
    let tmp = make_pool();
    let out = unblock(tmp.path(), &["a".to_string()], false).unwrap();
    assert_eq!(out.removed, 0);
    assert_eq!(out.kept, 0);
    assert!(out.excluded.is_empty());
}

#[test]
fn unblock_removes_auth_reason_by_default() {
    let tmp = make_pool();
    mark_bad(tmp.path(), "a", "401 unauthorized").unwrap();
    mark_bad(tmp.path(), "b", "exhausted: 429").unwrap();
    // Only "a" is targeted, so "b" is not excluded (it was never asked for).
    let out = unblock(tmp.path(), &["a".to_string()], false).unwrap();
    assert_eq!(out.removed, 1);
    assert_eq!(out.kept, 1);
    assert!(out.excluded.is_empty());
    assert!(!is_bad(tmp.path(), "a"));
    assert!(is_bad(tmp.path(), "b"));
}

/// #4212: a named account whose only entry is non-auth ("exhausted") is
/// reported as `excluded` under the default scope — the caller fails
/// instead of silently no-op'ing.
#[test]
fn unblock_default_scope_reports_excluded_non_auth() {
    let tmp = make_pool();
    mark_bad(tmp.path(), "a", "401 unauthorized").unwrap();
    mark_bad(tmp.path(), "b", "exhausted: weekly-limit").unwrap();
    let out = unblock(tmp.path(), &["a".to_string(), "b".to_string()], false).unwrap();
    assert_eq!(out.removed, 1); // a's auth entry
    assert_eq!(out.kept, 1); // b's exhausted entry stays
    assert_eq!(out.excluded, vec!["b".to_string()]);
}

#[test]
fn unblock_all_reasons_drops_non_auth_too() {
    let tmp = make_pool();
    mark_bad(tmp.path(), "b", "exhausted: 429").unwrap();
    let out = unblock(tmp.path(), &["b".to_string()], true).unwrap();
    assert_eq!(out.removed, 1);
    assert_eq!(out.kept, 0);
    // --all-reasons never excludes anything.
    assert!(out.excluded.is_empty());
    assert!(!is_bad(tmp.path(), "b"));
}

/// #6759: `unblock` has no pool-membership requirement of its own — an
/// account whose `.token` file is already gone (e.g. a retired account
/// naming scheme) but still has a stale `.bad_tokens` entry must still be
/// clearable, on demand, without waiting for `cleanup_bad_tokens`'s
/// 30-day auto-GC floor. This is pre-existing behavior (the pool-
/// membership gate the issue reports lives one layer up, in the CLI) —
/// this test makes the guarantee explicit and regression-proof.
#[test]
fn unblock_succeeds_for_name_absent_from_pool_token_files() {
    let tmp = make_pool();
    mark_bad(tmp.path(), "retired-account", "401 unauthorized").unwrap();
    assert!(!pool_dir(tmp.path()).join("retired-account.token").exists());

    let out = unblock(tmp.path(), &["retired-account".to_string()], false).unwrap();

    assert_eq!(out.removed, 1);
    assert!(!is_bad(tmp.path(), "retired-account"));
}

/// #6759: `unblock_in_dir` is what the CLI's `--shared` path calls
/// directly (it already holds the resolved pool directory and must not
/// route it back through the workspace-anchored [`unblock`], which would
/// append `.loom/tokens` a second time). Confirms it is byte-identical
/// in behavior to the workspace wrapper for the same underlying dir.
#[test]
fn unblock_in_dir_matches_workspace_wrapper() {
    let tmp = make_pool();
    mark_bad(tmp.path(), "a", "401 unauthorized").unwrap();
    mark_bad(tmp.path(), "b", "exhausted: 429").unwrap();

    let out = unblock_in_dir(&pool_dir(tmp.path()), &["a".to_string()], false).unwrap();

    assert_eq!(out.removed, 1);
    assert_eq!(out.kept, 1);
    assert!(out.excluded.is_empty());
    assert!(!is_bad(tmp.path(), "a"));
    assert!(is_bad(tmp.path(), "b"));
}

#[test]
fn unblock_ignores_unrelated_names() {
    let tmp = make_pool();
    mark_bad(tmp.path(), "a", "auth failure").unwrap();
    let out = unblock(tmp.path(), &["c".to_string()], false).unwrap();
    assert_eq!(out.removed, 0);
    assert_eq!(out.kept, 1);
    assert!(out.excluded.is_empty());
    assert!(is_bad(tmp.path(), "a"));
}

#[test]
fn unblock_keeps_malformed_lines() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    fs::write(dir.join(".bad_tokens"), "onlyoneword\n").unwrap();
    let out = unblock(tmp.path(), &["onlyoneword".to_string()], true).unwrap();
    assert_eq!(out.removed, 0);
    assert_eq!(out.kept, 1);
    assert!(out.excluded.is_empty());
}

#[test]
fn auth_reason_regex_does_not_match_exhausted() {
    assert!(!auth_reason_regex().is_match("exhausted: weekly limit"));
    assert!(auth_reason_regex().is_match("401 Unauthorized"));
    assert!(auth_reason_regex().is_match("oauth token expired"));
}

/// #6030: `claude-wrapper.sh` marks an auth-dead account (a 401 /
/// invalid-bearer-token death — distinct from usage/plan exhaustion) with
/// an `"auth-dead: ..."` reason string. It must classify as
/// [`BadReasonClass::Auth`] (permanent, clears only via `tokens unblock`)
/// rather than [`BadReasonClass::Exhaustion`] (which would let the entry
/// silently expire on the exhaustion cooldown and readmit a still-broken
/// credential into rotation).
#[test]
fn auth_dead_reason_classifies_as_auth_not_exhaustion() {
    let tmp = make_pool();
    mark_bad(tmp.path(), "agent-1", "auth-dead: 401 Invalid bearer token").unwrap();
    assert!(is_bad(tmp.path(), "agent-1"));
    let entry = blocking_entry(tmp.path(), "agent-1").expect("auth-dead entry blocks");
    assert_eq!(entry.class, BadReasonClass::Auth);
    assert_eq!(entry.class.permanence(), "permanent");
    assert_eq!(entry.cooldown_remaining_secs, None);
}

/// #6030: [`blocking_entry_in_dir`] (the resolved-directory variant used
/// by `tokens_pool::check`) agrees with the workspace-anchored
/// [`blocking_entry`] when pointed at the same directory directly.
#[test]
fn blocking_entry_in_dir_matches_workspace_anchored_variant() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    mark_bad(tmp.path(), "agent-1", "auth-dead: 401 Invalid bearer token").unwrap();
    let via_workspace = blocking_entry(tmp.path(), "agent-1").expect("blocks");
    let via_dir = blocking_entry_in_dir(&dir, "agent-1").expect("blocks");
    assert_eq!(via_workspace, via_dir);
}

// ---- #7522: session-limit early expiry ------------------------------

#[test]
fn is_session_limit_reason_matches_session_not_concurrent_or_weekly() {
    assert!(is_session_limit_reason("exhausted: hit your session limit"));
    assert!(is_session_limit_reason("SESSION LIMIT"));
    assert!(!is_session_limit_reason("exhausted: used 100% of your weekly limit"));
    assert!(!is_session_limit_reason("exhausted: reached your Fable 5 limit"));
    assert!(!is_session_limit_reason("out of usage credits"));
    // Belt-and-suspenders: a concurrent-session capacity fault is never
    // treated as the 5h quota window, even though it also contains the
    // words "session limit" — it is a different kind of fault (and is
    // never actually marked bad in production; see
    // `sweep_registry::crash_signals::session_capacity_exclusion`).
    assert!(!is_session_limit_reason("reached your concurrent session limit"));
    assert!(!is_session_limit_reason("too many concurrent sessions"));
}

/// #7522: a session-limit entry expires as soon as `.ranking` proves the
/// account's 5h window has already rolled over — via the recorded
/// binding-window reset instant — well before the fixed 6h TTL would have
/// expired it on its own.
#[test]
// Reads the process-global cooldown default, so it must not overlap the
// `#[serial]` tests that mutate `EXHAUSTION_COOLDOWN_ENV`.
#[serial]
fn session_limit_entry_expires_early_when_window_reset_instant_has_passed() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    // Marked 1h ago — well within the 6h default cooldown.
    let marked = (Utc::now() - chrono::Duration::seconds(3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(
        dir.join(".bad_tokens"),
        format!("{marked} agent-1 exhausted: hit your session limit\n"),
    )
    .unwrap();
    // The account's window actually reset 5 minutes ago.
    let reset = (Utc::now() - chrono::Duration::seconds(300))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(dir.join(".ranking"), format!("agent-1|available|0.02|{reset}\n")).unwrap();

    assert!(
        !is_bad(tmp.path(), "agent-1"),
        "session window already reset per .ranking — must not still block"
    );
    assert_eq!(blocking_entry(tmp.path(), "agent-1"), None);
}

/// #7522: the converse — a `.ranking` row proving the window has **not**
/// yet reset (a future reset instant) must leave the entry blocking under
/// the normal fixed-TTL accounting.
#[test]
// Reads the process-global cooldown default, so it must not overlap the
// `#[serial]` tests that mutate `EXHAUSTION_COOLDOWN_ENV`.
#[serial]
fn session_limit_entry_stays_blocked_when_window_reset_instant_is_future() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    let marked = (Utc::now() - chrono::Duration::seconds(3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(
        dir.join(".bad_tokens"),
        format!("{marked} agent-1 exhausted: hit your session limit\n"),
    )
    .unwrap();
    let reset = (Utc::now() + chrono::Duration::seconds(1800))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(dir.join(".ranking"), format!("agent-1|rate_limited|0.95|{reset}\n")).unwrap();

    assert!(is_bad(tmp.path(), "agent-1"));
    let entry = blocking_entry(tmp.path(), "agent-1").expect("still blocked");
    assert_eq!(entry.class, BadReasonClass::Exhaustion);
}

/// #7522: when `.ranking` carries no explicit reset instant (a legacy
/// 2/3-field row), a probe taken AFTER the mark reporting 5h utilization
/// back under the load-gate threshold is the fallback "the window rolled
/// over" signal.
#[test]
// Reads the process-global cooldown default, so it must not overlap the
// `#[serial]` tests that mutate `EXHAUSTION_COOLDOWN_ENV`.
#[serial]
fn session_limit_entry_expires_early_via_fresh_low_util_probe_without_reset_field() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    let marked = (Utc::now() - chrono::Duration::seconds(3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(
        dir.join(".bad_tokens"),
        format!("{marked} agent-1 exhausted: hit your session limit\n"),
    )
    .unwrap();
    // Written "now" (well after `marked`), 3-field row, no reset instant.
    fs::write(dir.join(".ranking"), "agent-1|available|0.10\n").unwrap();

    assert!(!is_bad(tmp.path(), "agent-1"));
}

/// #7522: the fallback util signal only fires below the load-gate
/// threshold — a fresh-but-still-loaded probe leaves the entry blocking.
#[test]
// Reads the process-global cooldown default, so it must not overlap the
// `#[serial]` tests that mutate `EXHAUSTION_COOLDOWN_ENV`.
#[serial]
fn session_limit_entry_stays_blocked_when_fresh_probe_util_is_still_high() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    let marked = (Utc::now() - chrono::Duration::seconds(3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(
        dir.join(".bad_tokens"),
        format!("{marked} agent-1 exhausted: hit your session limit\n"),
    )
    .unwrap();
    fs::write(dir.join(".ranking"), "agent-1|rate_limited|0.85\n").unwrap();

    assert!(is_bad(tmp.path(), "agent-1"));
}

/// #7522 regression, superseded in part by #7538: a **weekly**-limit
/// exhaustion entry is ambiguous (does not match `is_session_limit_reason`),
/// so it must ignore the session-limit fast path's single-signal
/// `session_window_has_reset` / `SESSION_WINDOW_SECS` cap entirely — only a
/// reason naming the session window specifically is eligible for *that*
/// early-expiry path. It is NOT exempt from `.ranking` evidence
/// altogether, though: #7538 gives every ambiguous entry its own
/// two-signal check, and a re-probe proving the account genuinely rolled
/// over both windows (here: `available`, not `exhausted`, with low 5h
/// utilization) now releases it early too — see
/// `ambiguous_entry_low_5h_high_7d_util_is_not_released_early` for the
/// converse (still-`exhausted` status) that #4212 protects against, which
/// this reason would NOT be exempt from either.
#[test]
// Reads the process-global cooldown default, so it must not overlap the
// `#[serial]` tests that mutate `EXHAUSTION_COOLDOWN_ENV`.
#[serial]
fn weekly_limit_entry_releases_early_when_reprobe_proves_both_signals_clear() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    let marked = (Utc::now() - chrono::Duration::seconds(3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(
        dir.join(".bad_tokens"),
        format!("{marked} agent-1 exhausted: used 100% of your weekly limit\n"),
    )
    .unwrap();
    // Ranking shows the account fully reset on BOTH windows — a genuine
    // recovery, not just a 5h rollover.
    let reset = (Utc::now() - chrono::Duration::seconds(60))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(dir.join(".ranking"), format!("agent-1|available|0.01|{reset}\n")).unwrap();

    assert!(
        !is_bad(tmp.path(), "agent-1"),
        "both signals clear on re-probe — an ambiguous entry releases early too (#7538)"
    );
    assert_eq!(blocking_entry(tmp.path(), "agent-1"), None);
}

/// #7522 regression: an auth-reason entry is unaffected by `.ranking`
/// evidence — it remains permanent (clears only via `tokens unblock`).
#[test]
// Reads the process-global cooldown default, so it must not overlap the
// `#[serial]` tests that mutate `EXHAUSTION_COOLDOWN_ENV`.
#[serial]
fn auth_entry_ignores_ranking_reset_evidence_stays_permanent() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    let marked = (Utc::now() - chrono::Duration::seconds(3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(dir.join(".bad_tokens"), format!("{marked} agent-1 401 unauthorized\n")).unwrap();
    let reset = (Utc::now() - chrono::Duration::seconds(60))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(dir.join(".ranking"), format!("agent-1|available|0.01|{reset}\n")).unwrap();

    assert!(is_bad(tmp.path(), "agent-1"));
    let entry = blocking_entry(tmp.path(), "agent-1").unwrap();
    assert_eq!(entry.class, BadReasonClass::Auth);
}

// ---- #7522: the 5h session-window TTL cap ---------------------------

/// Helper: seed a `.bad_tokens` entry for `agent-1` aged `age_secs`, with
/// no `.ranking` at all — isolating the [`SESSION_WINDOW_SECS`] cap from
/// the `.ranking`-evidence early-expiry path.
fn seed_aged_entry(dir: &Path, reason: &str, age_secs: i64) {
    let marked = (Utc::now() - chrono::Duration::seconds(age_secs))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(dir.join(".bad_tokens"), format!("{marked} agent-1 {reason}\n")).unwrap();
}

/// #7522, the core regression: with NO `.ranking` evidence whatsoever
/// (the worker-host shape, where the probe short-circuits a bad-marked
/// account to a bare `<name>|blocked` row), a session-limit entry must
/// still clear on its own once the 5h window it describes has provably
/// elapsed — instead of being held the full generic 6h cooldown. That
/// hour of over-hold is exactly the "roughly an hour of fleet-wide
/// starvation at every 5h boundary" the issue measures.
#[test]
#[serial]
fn session_limit_entry_clears_at_the_5h_window_without_any_ranking_evidence() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    assert!(!dir.join(".ranking").exists());

    // 4h55m in: still inside its own window, still blocking.
    seed_aged_entry(&dir, "exhausted: hit your session limit", SESSION_WINDOW_SECS - 300);
    assert!(
        is_bad(tmp.path(), "agent-1"),
        "must not be readmitted before the window can possibly have reset"
    );

    // 5h05m in: past the window, readmitted — but still well inside the
    // 6h generic cooldown that used to gate it.
    // Compile-time proof this age really does sit in the gap between the
    // two TTLs — otherwise the assertion below would pass vacuously.
    const _: () = assert!(SESSION_WINDOW_SECS + 300 < DEFAULT_EXHAUSTION_COOLDOWN_SECS);
    seed_aged_entry(&dir, "exhausted: hit your session limit", SESSION_WINDOW_SECS + 300);
    assert!(
        !is_bad(tmp.path(), "agent-1"),
        "a session-limit entry must not outlive its own 5h window"
    );
}

/// #7522 regression: the cap is scoped to the session reason. A
/// weekly-limit entry of the identical age is still blocking — it keeps
/// the full generic cooldown.
#[test]
#[serial]
fn weekly_limit_entry_is_not_capped_at_the_session_window() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    seed_aged_entry(&dir, "exhausted: used 100% of your weekly limit", SESSION_WINDOW_SECS + 300);
    assert!(
        is_bad(tmp.path(), "agent-1"),
        "a weekly entry keeps the full generic cooldown past the 5h mark"
    );
}

/// #7522: [`SESSION_WINDOW_SECS`] is a **cap**, never a floor — an
/// operator who configures a deliberately *shorter* cooldown still gets
/// the shorter value, for session-limit entries just like any other.
#[test]
#[serial]
fn session_window_cap_never_extends_a_shorter_configured_cooldown() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    std::env::set_var(EXHAUSTION_COOLDOWN_ENV, "600"); // 10 minutes
    seed_aged_entry(&dir, "exhausted: hit your session limit", 900);
    let still_bad = is_bad(tmp.path(), "agent-1");
    std::env::remove_var(EXHAUSTION_COOLDOWN_ENV);
    assert!(
        !still_bad,
        "a 10-minute configured cooldown must not be lengthened to 5h by the cap"
    );
}

/// #7522: the operator-facing "clears in <N>" countdown is reported
/// against the EFFECTIVE (capped) cooldown, so the log line never
/// promises a longer hold than selection will actually honor. This is the
/// misleading `clears in 5h00m` the issue quotes from the role log.
#[test]
#[serial]
fn session_limit_remaining_countdown_is_reported_against_the_capped_ttl() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    seed_aged_entry(&dir, "exhausted: hit your session limit", 3600);
    let remaining = blocking_entry(tmp.path(), "agent-1")
        .unwrap()
        .cooldown_remaining_secs
        .unwrap();
    // ~4h left of the 5h window, not ~5h left of the 6h cooldown.
    assert!(
        (remaining - (SESSION_WINDOW_SECS - 3600)).abs() <= 2,
        "expected ~{} remaining, got {remaining}",
        SESSION_WINDOW_SECS - 3600
    );

    seed_aged_entry(&dir, "exhausted: used 100% of your weekly limit", 3600);
    let weekly_remaining = blocking_entry(tmp.path(), "agent-1")
        .unwrap()
        .cooldown_remaining_secs
        .unwrap();
    assert!(
        (weekly_remaining - (DEFAULT_EXHAUSTION_COOLDOWN_SECS - 3600)).abs() <= 2,
        "a weekly entry's countdown is unchanged, got {weekly_remaining}"
    );
}

// ---- #7536 review, blocker 2: reset-instant freshness ---------------

/// A `.ranking` reset instant that predates the mark describes the
/// *previous* window, not the one that produced this mark. Trusting it
/// would make a **fresh** session-limit mark an instant no-op: selection
/// hands the account back out, the child insta-crashes on the same limit,
/// the wrapper re-marks it, and the same stale row expires it again — a
/// thrash loop with no backoff. `reset > marked_at` is the gate.
#[test]
#[serial]
fn session_limit_entry_ignores_a_reset_instant_recorded_before_the_mark() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    // Marked one minute ago: a fresh session-limit hit.
    let marked = (Utc::now() - chrono::Duration::seconds(60))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(
        dir.join(".bad_tokens"),
        format!("{marked} agent-1 exhausted: hit your session limit\n"),
    )
    .unwrap();
    // `.ranking` still carries the PREVIOUS window's reset instant (2h
    // ago, i.e. before the mark). No utilization field, so the mtime-gated
    // fallback has nothing to say either.
    let stale_reset = (Utc::now() - chrono::Duration::seconds(2 * 3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(dir.join(".ranking"), format!("agent-1|blocked||{stale_reset}\n")).unwrap();

    assert!(
        is_bad(tmp.path(), "agent-1"),
        "a reset instant older than the mark cannot prove THIS window reset"
    );
}

/// The freshness gate does not swallow the util fallback: a stale reset
/// instant falls *through* to the mtime-gated utilization branch, which
/// can still prove the window rolled over.
#[test]
#[serial]
fn stale_reset_instant_falls_through_to_the_fresh_low_util_signal() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    let marked = (Utc::now() - chrono::Duration::seconds(3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(
        dir.join(".bad_tokens"),
        format!("{marked} agent-1 exhausted: hit your session limit\n"),
    )
    .unwrap();
    // Reset instant predates the mark (ignored), but the row itself was
    // written now — after the mark — and reports a low 5h utilization.
    let stale_reset = (Utc::now() - chrono::Duration::seconds(2 * 3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(dir.join(".ranking"), format!("agent-1|available|0.05|{stale_reset}\n")).unwrap();

    assert!(!is_bad(tmp.path(), "agent-1"), "a fresh low-util probe still readmits");
}

/// `check::limit_reset` records the **7d** reset for an `exhausted` row and
/// the 5h reset for every other status, so an `exhausted` row's instant
/// answers a different question and must not be read as a 5h rollover.
#[test]
#[serial]
fn exhausted_ranking_rows_reset_instant_is_not_read_as_a_session_rollover() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    let marked = (Utc::now() - chrono::Duration::seconds(3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(
        dir.join(".bad_tokens"),
        format!("{marked} agent-1 exhausted: hit your session limit\n"),
    )
    .unwrap();
    // An `exhausted` row: this instant is the 7d reset. Even though it
    // sits after the mark and has already passed, it says nothing about
    // the 5h window, and the row carries no utilization to fall back on.
    let reset_7d = (Utc::now() - chrono::Duration::seconds(60))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(dir.join(".ranking"), format!("agent-1|exhausted||{reset_7d}\n")).unwrap();

    assert!(
        is_bad(tmp.path(), "agent-1"),
        "a 7d reset instant must not expire a 5h session-limit mark"
    );
}

// ---- #7536 review, blocker 1: historical-entry lookup ---------------

/// [`latest_entry_in_dir`] reports the newest entry for an account whether
/// or not it still blocks — the distinction [`blocking_entry_in_dir`]
/// deliberately collapses.
#[test]
#[serial]
fn latest_entry_in_dir_reports_the_newest_entry_ignoring_expiry() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    let older = (Utc::now() - chrono::Duration::seconds(20 * 3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let newer = (Utc::now() - chrono::Duration::seconds(9 * 3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(
        dir.join(".bad_tokens"),
        format!(
            "{newer} agent-1 exhausted: hit your session limit\n\
             {older} agent-1 exhausted: used 100% of your weekly limit\n\
             {newer} agent-2 exhausted: hit your session limit\n"
        ),
    )
    .unwrap();

    // Both entries are long expired, so nothing blocks...
    assert_eq!(blocking_entry_in_dir(&dir, "agent-1"), None);
    // ...but the history is still readable, newest-first by timestamp and
    // NOT by file position.
    let latest = latest_entry_in_dir(&dir, "agent-1").expect("history present");
    assert_eq!(latest.reason, "exhausted: hit your session limit");
    assert_eq!(latest.timestamp, newer);
    assert!(latest_block_was_session_limit(&dir, "agent-1"));
}

/// The no-evidence cases both answer `false`: an account that was never
/// marked bad, and one whose latest mark is a dead credential. Those are
/// the shapes behind a `blocked` `.ranking` row written by a 401 probe or a
/// shape mismatch, and they must keep #5629's unconditional hard exclusion.
#[test]
#[serial]
fn latest_block_was_session_limit_is_false_without_positive_evidence() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());

    // No `.bad_tokens` file at all (the 401 / shape_mismatch shape).
    assert_eq!(latest_entry_in_dir(&dir, "agent-1"), None);
    assert!(!latest_block_was_session_limit(&dir, "agent-1"));

    // A dead credential, and a weekly ceiling — neither is a 5h window.
    let ts = (Utc::now() - chrono::Duration::seconds(9 * 3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(
        dir.join(".bad_tokens"),
        format!(
            "{ts} agent-1 auth-dead: 401 Invalid bearer token\n\
             {ts} agent-2 exhausted: used 100% of your weekly limit\n"
        ),
    )
    .unwrap();
    assert!(!latest_block_was_session_limit(&dir, "agent-1"));
    assert!(!latest_block_was_session_limit(&dir, "agent-2"));
    // An unrelated account's session-limit entry is not borrowed.
    assert!(!latest_block_was_session_limit(&dir, "agent-3"));
}

/// A later session-limit mark supersedes an earlier non-session one (the
/// live shape: every rotation appends a fresh line), and vice versa.
#[test]
#[serial]
fn latest_entry_decides_when_reasons_disagree() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    let older = (Utc::now() - chrono::Duration::seconds(20 * 3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let newer = (Utc::now() - chrono::Duration::seconds(9 * 3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();

    fs::write(
        dir.join(".bad_tokens"),
        format!(
            "{older} agent-1 exhausted: used 100% of your weekly limit\n\
             {newer} agent-1 exhausted: hit your session limit\n"
        ),
    )
    .unwrap();
    assert!(latest_block_was_session_limit(&dir, "agent-1"));

    fs::write(
        dir.join(".bad_tokens"),
        format!(
            "{older} agent-1 exhausted: hit your session limit\n\
             {newer} agent-1 exhausted: used 100% of your weekly limit\n"
        ),
    )
    .unwrap();
    assert!(!latest_block_was_session_limit(&dir, "agent-1"));
}

// -----------------------------------------------------------------
// #7538: ambiguous (non-session-limit-matching) exhaustion entries —
// two-signal early release.
// -----------------------------------------------------------------

/// #7538 core regression (the exact harm #4212 declined to risk): an
/// ambiguous-reason entry whose re-probed `.ranking` row shows LOW 5h
/// utilization but is still `exhausted` (i.e. HIGH 7d utilization — a
/// genuine weekly exhaustion that happened to be neutrally worded) must
/// NOT be released early. Releasing on 5h evidence alone would readmit a
/// still-weekly-blocked account.
#[test]
#[serial]
fn ambiguous_entry_low_5h_high_7d_util_is_not_released_early() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    // Marked 1h ago — well within the 6h default cooldown, and this
    // reason does NOT match `is_session_limit_reason`.
    let marked = (Utc::now() - chrono::Duration::seconds(3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(
        dir.join(".bad_tokens"),
        format!("{marked} agent-1 exhausted: rate-limited (daemon insta-crash, issue #123)\n"),
    )
    .unwrap();
    // Re-probed AFTER the mark: 5h utilization is low, but the account is
    // still `exhausted` — 7d utilization is still over the threshold.
    fs::write(dir.join(".ranking"), "agent-1|exhausted|0.02\n").unwrap();

    assert!(
        is_bad(tmp.path(), "agent-1"),
        "low 5h util alone must not release a still-weekly-exhausted ambiguous entry"
    );
    let entry = blocking_entry(tmp.path(), "agent-1").expect("still blocked");
    assert_eq!(entry.class, BadReasonClass::Exhaustion);
}

/// #7538: the converse — an ambiguous-reason entry whose re-probed
/// `.ranking` row shows LOW 5h AND LOW 7d utilization (not `exhausted`)
/// IS released early, the same outcome a matching session-limit entry
/// gets from its own single-signal check.
#[test]
#[serial]
fn ambiguous_entry_low_5h_and_low_7d_util_is_released_early() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    let marked = (Utc::now() - chrono::Duration::seconds(3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(
        dir.join(".bad_tokens"),
        format!("{marked} agent-1 exhausted: usage/plan limit modal (RATE_LIMIT_ABORT)\n"),
    )
    .unwrap();
    // Re-probed AFTER the mark: both signals clear — available, low 5h
    // utilization, not `exhausted`.
    fs::write(dir.join(".ranking"), "agent-1|available|0.05\n").unwrap();

    assert!(
        !is_bad(tmp.path(), "agent-1"),
        "both signals clear — the ambiguous entry should release early"
    );
    assert_eq!(blocking_entry(tmp.path(), "agent-1"), None);
}

/// #7538: the 5h signal alone is not sufficient even when it clears the
/// load gate — a `rate_limited`/`exhausted` status (7d signal not clear)
/// must still hold the entry blocked. Mirrors the "high util keeps it
/// blocked" converse test already covering the session-limit fast path.
#[test]
#[serial]
fn ambiguous_entry_low_5h_but_exhausted_status_stays_blocked() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    let marked = (Utc::now() - chrono::Duration::seconds(3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(dir.join(".bad_tokens"), format!("{marked} agent-1 exhausted: usage limit\n"))
        .unwrap();
    fs::write(dir.join(".ranking"), "agent-1|exhausted|0.01\n").unwrap();

    assert!(is_bad(tmp.path(), "agent-1"));
}

/// #7538 edge case: an ambiguous entry with NO `.ranking` row at all
/// (never probed, or probed before this change shipped) falls back to the
/// full fixed-TTL cooldown, exactly as before this change — the
/// two-signal check only ever narrows a hold, never widens one.
#[test]
#[serial]
fn ambiguous_entry_with_no_ranking_falls_back_to_fixed_ttl() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    let marked = (Utc::now() - chrono::Duration::seconds(3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(dir.join(".bad_tokens"), format!("{marked} agent-1 exhausted: hit your limit\n"))
        .unwrap();
    assert!(!dir.join(".ranking").exists());

    assert!(is_bad(tmp.path(), "agent-1"));
    let entry = blocking_entry(tmp.path(), "agent-1").expect("fixed-TTL fallback still blocks");
    assert_eq!(entry.class, BadReasonClass::Exhaustion);
    let remaining = entry
        .cooldown_remaining_secs
        .expect("TTL entry has a remaining");
    // 1h old, 6h default cooldown → ~5h remaining (not capped at the
    // session window — this reason is ambiguous, not session-limit).
    // #7748: lower bound widened from 4h to 3h — same shape (and same
    // fixture) as `blocking_entry_reports_exhaustion_class_and_cooldown_remaining`,
    // which flaked in CI on a wall-clock jump between the `Utc::now()`
    // that stamps the fixture and the internal `Utc::now()` inside
    // `blocking_entry`; see that test for the full rationale.
    assert!(
        (3 * 3600..=5 * 3600).contains(&remaining),
        "expected ~5h remaining on the unmodified fixed TTL, got {remaining}"
    );
}

/// #7538: a `.ranking` row present BEFORE the entry was marked bad (i.e.
/// stale, predating the mark) must not satisfy the freshness requirement
/// — otherwise an ambiguous entry could be released without ever having
/// been re-probed since it was marked bad.
#[test]
#[serial]
fn ambiguous_entry_ignores_a_ranking_row_that_predates_the_mark() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    // Write the (stale) low-util ranking row FIRST...
    fs::write(dir.join(".ranking"), "agent-1|available|0.01\n").unwrap();
    // ...then mark bad AFTER it, so the ranking row's mtime predates the
    // mark and must be treated as having no usable evidence.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let marked = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    fs::write(dir.join(".bad_tokens"), format!("{marked} agent-1 exhausted: hit your limit\n"))
        .unwrap();

    assert!(
        is_bad(tmp.path(), "agent-1"),
        "a stale pre-mark .ranking row must not release the entry"
    );
}

// -----------------------------------------------------------------
// #13893: a still-`exhausted` re-probe holds the entry until the
// recorded 7d reset instant instead of cycling the 6h fixed cooldown.
// -----------------------------------------------------------------

/// Compile-time proof the fixtures below really do sit outside the fixed
/// cooldown — otherwise the "held PAST the cooldown" assertions would pass
/// vacuously.
const _: () = assert!(7 * 3600 > DEFAULT_EXHAUSTION_COOLDOWN_SECS);

/// AC 1 regression pin (#13893): a fresh re-probe that is NOT `exhausted`
/// with 5h utilization under the load gate releases the entry before the 6h
/// cooldown — even when the row also carries a future `limit_reset` (for a
/// non-`exhausted` status that field is the 5h window's reset, not a 7d
/// one, so it must not be turned into a hold). This held before #13893 via
/// `ambiguous_exhaustion_has_reset` → `Some(true)` and must keep holding
/// after the evidence reader was extended.
#[test]
#[serial]
fn ambiguous_entry_still_releases_when_reprobe_is_not_exhausted_with_low_5h() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    let marked = (Utc::now() - chrono::Duration::seconds(3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(
        dir.join(".bad_tokens"),
        format!("{marked} agent-1 exhausted: rate-limited (daemon insta-crash, issue #123)\n"),
    )
    .unwrap();
    // Re-probed AFTER the mark: recovered on both windows, and the row
    // still carries a (future, 5h-window) reset instant.
    let reset = (Utc::now() + chrono::Duration::seconds(3 * 24 * 3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(dir.join(".ranking"), format!("agent-1|available|0.05|{reset}\n")).unwrap();

    assert!(
        !is_bad(tmp.path(), "agent-1"),
        "a non-exhausted re-probe with 5h under the gate must still release early"
    );
    assert_eq!(blocking_entry(tmp.path(), "agent-1"), None);
}

/// #13893 core: a fresh `exhausted` row whose `limit_reset` is a credible
/// future instant holds the entry PAST the 6h fixed cooldown, until that
/// instant — the 6h expiry → retry → insta-crash → re-mark churn in the
/// issue's evidence is exactly what this prevents. The countdown reports
/// time to the reset instant, not to the (long past) fixed cooldown.
#[test]
#[serial]
fn exhausted_reprobe_holds_entry_until_limit_reset_past_the_fixed_cooldown() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    // Marked 7h ago — the 6h default cooldown has already expired, so any
    // blocking below can only come from the #13893 hold.
    let marked = (Utc::now() - chrono::Duration::seconds(7 * 3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(
        dir.join(".bad_tokens"),
        format!("{marked} agent-1 exhausted: used 100% of your weekly limit\n"),
    )
    .unwrap();
    // Re-probed AFTER the mark: still `exhausted`, 5h util low (a
    // weekly-blocked account's 5h usage is near zero regardless), and the
    // probe recorded the 7d reset 5 days out.
    let reset = (Utc::now() + chrono::Duration::seconds(5 * 24 * 3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(dir.join(".ranking"), format!("agent-1|exhausted|0.02|{reset}\n")).unwrap();

    let entry = blocking_entry(tmp.path(), "agent-1").expect("held past the fixed cooldown");
    assert_eq!(entry.class, BadReasonClass::Exhaustion);
    assert_eq!(entry.timestamp, marked, "the held entry is the original mark");
    let remaining = entry
        .cooldown_remaining_secs
        .expect("the hold reports a countdown");
    // #7748 convention: loose lower bound to tolerate a wall-clock jump
    // between the fixture's `Utc::now()` and the internal one.
    assert!(
        ((5 * 24 * 3600) - 3600..=(5 * 24 * 3600)).contains(&remaining),
        "expected ~5d to the recorded reset instant, got {remaining}s"
    );
}

/// #13893, AC 2 for the writer's own empty-util row shape: a fresh
/// `exhausted` row that knows its 7d reset but not its 5h utilization —
/// `check::ranking_line`'s `(None, Some(r))` arm writes
/// `name|exhausted||limit_reset` — must hold past the fixed cooldown exactly
/// like a row with a utilization. The codex probe emits this shape in the
/// steady state of a weekly-blocked account whose 5h window has already
/// rolled over; gating the hold on `util_5h` would send exactly those rows
/// back to the 6h re-mark churn.
#[test]
#[serial]
fn exhausted_reprobe_without_5h_util_still_holds_until_limit_reset() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    // Marked 7h ago — past the 6h default cooldown, so any blocking below
    // can only come from the #13893 hold.
    let marked = (Utc::now() - chrono::Duration::seconds(7 * 3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(
        dir.join(".bad_tokens"),
        format!("{marked} agent-1 exhausted: used 100% of your weekly limit\n"),
    )
    .unwrap();
    // Re-probed AFTER the mark: still `exhausted`, 5h utilization unknown
    // (empty third field — `name|status||limit_reset`), 7d reset 5 days out.
    let reset = (Utc::now() + chrono::Duration::seconds(5 * 24 * 3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(dir.join(".ranking"), format!("agent-1|exhausted||{reset}\n")).unwrap();

    let entry = blocking_entry(tmp.path(), "agent-1")
        .expect("an exhausted row with unknown 5h util must hold past the fixed cooldown");
    assert_eq!(entry.class, BadReasonClass::Exhaustion);
    let remaining = entry
        .cooldown_remaining_secs
        .expect("the hold reports a countdown");
    assert!(
        ((5 * 24 * 3600) - 3600..=(5 * 24 * 3600)).contains(&remaining),
        "expected ~5d to the recorded reset instant, got {remaining}s"
    );
}

/// #13893: even while the entry is still inside its fixed cooldown, a fresh
/// `exhausted` row's countdown switches to the recorded reset instant — here
/// the instant (2h out) is EARLIER than the remaining cooldown (~5h50m), and
/// the earlier truth is what the operator is shown.
#[test]
#[serial]
fn exhausted_reprobe_countdown_reports_time_to_reset_even_inside_the_cooldown() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    let marked = (Utc::now() - chrono::Duration::seconds(600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(
        dir.join(".bad_tokens"),
        format!("{marked} agent-1 exhausted: used 100% of your weekly limit\n"),
    )
    .unwrap();
    let reset = (Utc::now() + chrono::Duration::seconds(2 * 3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(dir.join(".ranking"), format!("agent-1|exhausted|0.02|{reset}\n")).unwrap();

    let entry = blocking_entry(tmp.path(), "agent-1").expect("held to the reset instant");
    let remaining = entry.cooldown_remaining_secs.expect("countdown");
    // ~2h to the reset instant, NOT ~5h50m to the fixed cooldown.
    assert!(
        ((2 * 3600) - 600..=(2 * 3600)).contains(&remaining),
        "expected ~2h to the recorded reset instant, got {remaining}s"
    );
}

/// AC 3 (#13893): once the recorded 7d reset instant is in the past — but
/// still strictly after the mark, so it is fresh evidence for THIS mark's
/// window — the next read releases the entry.
#[test]
#[serial]
fn exhausted_reprobe_releases_once_limit_reset_has_passed() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    let marked = (Utc::now() - chrono::Duration::seconds(2 * 3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(
        dir.join(".bad_tokens"),
        format!("{marked} agent-1 exhausted: used 100% of your weekly limit\n"),
    )
    .unwrap();
    // The recorded reset passed 10 minutes ago: the 7d window the probe
    // described has rolled over.
    let reset = (Utc::now() - chrono::Duration::seconds(600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(dir.join(".ranking"), format!("agent-1|exhausted|0.02|{reset}\n")).unwrap();

    assert!(!is_bad(tmp.path(), "agent-1"), "a passed reset instant must release the entry");
    assert_eq!(blocking_entry(tmp.path(), "agent-1"), None);
}

// -----------------------------------------------------------------
// #14145: a failed probe round mid-hold must not readmit the account.
// The write-side guarantee lives in `check::write_ranking_atomic`
// (an evidence-free `error`/`blocked` round carries the previous
// `exhausted` row forward instead of overwriting it); these tests pin
// the end-to-end behaviour through that real writer.
// -----------------------------------------------------------------

/// #14145, the issue body's steps 1–3: an account held under #13893 with a
/// 7d reset days out and a 10h-old mark; one probe cycle fails (transport
/// `error`); the next read must STILL hold the account to the recorded
/// reset — not release it at the 6h fixed cooldown. Before the fix the
/// failed round overwrote `name|exhausted||<reset>` with a bare
/// `name|error`, the reader found no util and no reset, and the 10h-old
/// entry released.
#[test]
#[serial]
fn failed_probe_round_mid_hold_does_not_readmit_the_account() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    // Step 1: held, mark 10h old — past the 6h default cooldown, so any
    // blocking below can only come from the weekly hold.
    let marked = (Utc::now() - chrono::Duration::seconds(10 * 3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(
        dir.join(".bad_tokens"),
        format!("{marked} agent-1 exhausted: used 100% of your weekly limit\n"),
    )
    .unwrap();
    let reset = (Utc::now() + chrono::Duration::seconds(4 * 24 * 3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    // The steady-state hold row (mtime after the backdated mark — fresh).
    fs::write(dir.join(".ranking"), format!("agent-1|exhausted||{reset}\n")).unwrap();
    // Sanity: held before the failed round.
    assert!(
        blocking_entry(tmp.path(), "agent-1").is_some(),
        "precondition: the hold is active"
    );

    // Step 2: one probe cycle fails — the round's report carries an
    // evidence-free `error` row; run it through the REAL writer the
    // daemon's periodic refresh uses.
    let ranking_path = dir.join(".ranking");
    let report = crate::tokens_pool::check::ProbeReport {
        ranked_at: "x".into(),
        accounts: vec![crate::tokens_pool::check::AccountResult::new(
            "agent-1", "error",
        )],
    };
    crate::tokens_pool::check::write_ranking_atomic(&report, &ranking_path).unwrap();

    // The row on disk is still the exhausted hold row, not `error`.
    assert_eq!(
        fs::read_to_string(&ranking_path).unwrap(),
        format!("agent-1|exhausted||{reset}\n"),
        "a failed round must not overwrite the hold row"
    );
    // Step 3: the next read holds the account to the recorded reset.
    let entry = blocking_entry(tmp.path(), "agent-1")
        .expect("a failed probe round must not readmit an account under a weekly hold");
    assert_eq!(entry.class, BadReasonClass::Exhaustion);
    assert_eq!(entry.timestamp, marked, "the held entry is the original mark");
    let remaining = entry
        .cooldown_remaining_secs
        .expect("the hold reports a countdown");
    // ~4d to the recorded reset instant (NOT the ~0 remaining the 6h fixed
    // cooldown would report for a 10h-old mark). #7748 convention: loose
    // lower bound for a wall-clock jump between fixture and reader.
    assert!(
        ((4 * 24 * 3600) - 3600..=(4 * 24 * 3600)).contains(&remaining),
        "expected ~4d to the recorded reset instant, got {remaining}s"
    );
}

/// #14145 guard, #13893 AC 4 unchanged: with no previous `exhausted` row at
/// all (no hold evidence ever recorded), a failed round writes its `error`
/// row as before and the entry still clears at the 6h fixed cooldown — the
/// carry-forward never manufactures a hold out of nothing.
#[test]
#[serial]
fn no_evidence_at_all_still_yields_the_fixed_cooldown() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    // Marked 7h ago — past the 6h default cooldown; no `.ranking` exists.
    let marked = (Utc::now() - chrono::Duration::seconds(7 * 3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(dir.join(".bad_tokens"), format!("{marked} agent-1 exhausted: hit your limit\n"))
        .unwrap();
    assert!(
        !is_bad(tmp.path(), "agent-1"),
        "precondition: with no evidence at all the 7h-old mark is already past the 6h cooldown"
    );

    // A failed round writes the bare error row (nothing to preserve).
    let ranking_path = dir.join(".ranking");
    let report = crate::tokens_pool::check::ProbeReport {
        ranked_at: "x".into(),
        accounts: vec![crate::tokens_pool::check::AccountResult::new(
            "agent-1", "error",
        )],
    };
    crate::tokens_pool::check::write_ranking_atomic(&report, &ranking_path).unwrap();
    assert_eq!(fs::read_to_string(&ranking_path).unwrap(), "agent-1|error\n");

    assert!(
        !is_bad(tmp.path(), "agent-1"),
        "no-evidence-at-all must still clear at the 6h fixed cooldown (#13893 AC 4); \
         a landed error row must not resurrect or extend anything"
    );
    assert_eq!(blocking_entry(tmp.path(), "agent-1"), None);
}

/// AC 4 (#13893): without a credible reset instant the fixed 6h behaviour is
/// unchanged. Three shapes: an unparseable `limit_reset`, one at the mark
/// instant itself, and one before the mark (a pre-mark instant describes the
/// previous window — the same trap `session_window_has_reset` documents).
#[test]
#[serial]
fn exhausted_reprobe_without_a_credible_reset_keeps_the_fixed_cooldown() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    let marked_at = Utc::now() - chrono::Duration::seconds(3600);
    let marked = marked_at.format("%Y-%m-%dT%H:%M:%SZ").to_string();
    fs::write(
        dir.join(".bad_tokens"),
        format!("{marked} agent-1 exhausted: used 100% of your weekly limit\n"),
    )
    .unwrap();

    for reset in [
        "not-a-date".to_string(),
        marked.clone(), // at the mark
        (marked_at - chrono::Duration::seconds(3600)) // before the mark
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string(),
    ] {
        fs::write(dir.join(".ranking"), format!("agent-1|exhausted|0.02|{reset}\n")).unwrap();
        let entry = blocking_entry(tmp.path(), "agent-1")
            .unwrap_or_else(|| panic!("reset `{reset}`: entry must still block on the fixed TTL"));
        let remaining = entry
            .cooldown_remaining_secs
            .unwrap_or_else(|| panic!("reset `{reset}`: fixed-TTL entry has a countdown"));
        assert!(
            (3 * 3600..=5 * 3600).contains(&remaining),
            "reset `{reset}`: expected ~5h of the fixed TTL, got {remaining}s"
        );
    }
}

/// AC 6 (#13893): a `limit_reset` more than the 8-day horizon past now is
/// garbage — a 7d window cannot end later — so the entry falls back to the
/// fixed cooldown instead of being frozen out until a corrupt instant.
#[test]
#[serial]
fn garbage_limit_reset_beyond_the_weekly_horizon_falls_back_to_fixed_cooldown() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    let marked = (Utc::now() - chrono::Duration::seconds(3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(
        dir.join(".bad_tokens"),
        format!("{marked} agent-1 exhausted: used 100% of your weekly limit\n"),
    )
    .unwrap();
    // 9 days out — past MAX_WEEKLY_RESET_HORIZON_SECS (8d).
    let reset = (Utc::now() + chrono::Duration::seconds(9 * 24 * 3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(dir.join(".ranking"), format!("agent-1|exhausted|0.02|{reset}\n")).unwrap();

    let entry = blocking_entry(tmp.path(), "agent-1").expect("still blocked on the fixed TTL");
    let remaining = entry.cooldown_remaining_secs.expect("fixed-TTL countdown");
    assert!(
        (3 * 3600..=5 * 3600).contains(&remaining),
        "expected ~5h of the fixed TTL (not the ~9d garbage instant), got {remaining}s"
    );
}

/// #13893: an explicitly configured [`EXHAUSTION_COOLDOWN_ENV`] caps the
/// extended hold — the operator asked for that cooldown and gets it. With
/// the env at 1h and a fresh `exhausted` row whose reset is 2h out, the
/// entry clears at the env cap (~50 min from now), NOT at the reset instant
/// (~2h); the default (no env) would report the reset instant instead.
#[test]
#[serial]
fn operator_env_cooldown_caps_the_extended_weekly_hold() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    let marked = (Utc::now() - chrono::Duration::seconds(600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(
        dir.join(".bad_tokens"),
        format!("{marked} agent-1 exhausted: used 100% of your weekly limit\n"),
    )
    .unwrap();
    let reset = (Utc::now() + chrono::Duration::seconds(2 * 3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(dir.join(".ranking"), format!("agent-1|exhausted|0.02|{reset}\n")).unwrap();

    std::env::set_var(EXHAUSTION_COOLDOWN_ENV, "3600"); // 1h cap
    let entry = blocking_entry(tmp.path(), "agent-1").expect("held, but only to the env cap");
    let remaining = entry.cooldown_remaining_secs.expect("countdown");
    std::env::remove_var(EXHAUSTION_COOLDOWN_ENV);

    // mark_at + 3600 − now ≈ 3600 − 600 = ~50 min, not the ~2h reset instant.
    assert!(
        (2400..=3000).contains(&remaining),
        "expected ~50m (the env cap), got {remaining}s"
    );
}

/// #13893 retention pin: the on-disk clock must never expire a line whose
/// read-side hold can still be live — pruning one earlier would silently
/// readmit a still-exhausted account mid-hold. A weekly entry aged past the
/// pre-#13893 24h policy but inside the 8-day retention survives the routine
/// cleanup; one past the retention is still reclaimed.
#[test]
fn cleanup_retention_covers_the_extended_weekly_hold() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    let mid_hold = (Utc::now() - chrono::Duration::seconds(3 * 24 * 3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let ancient = (Utc::now() - chrono::Duration::seconds(9 * 24 * 3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(
        dir.join(".bad_tokens"),
        format!(
            "{mid_hold} agent-hold exhausted: used 100% of your weekly limit\n\
             {ancient} agent-old exhausted: used 100% of your weekly limit\n"
        ),
    )
    .unwrap();

    let outcome = cleanup_bad_tokens_in_dir(&dir, DEFAULT_CLEANUP_MAX_AGE_SECS).unwrap();
    assert_eq!(outcome.kept, 1, "a mid-hold weekly entry must survive cleanup");
    assert_eq!(outcome.removed, 1);
    let text = fs::read_to_string(dir.join(".bad_tokens")).unwrap();
    assert!(text.contains("agent-hold"), "mid-hold entry was pruned: {text}");
    assert!(!text.contains("agent-old"));
}

// -----------------------------------------------------------------
// #13942: the fleet token-probe sidecar joins `.ranking` as evidence.
// -----------------------------------------------------------------

/// Write `lines` as the pool's `.bad_tokens` and return the pool dir.
fn pool_with_bad_lines(ws: &Path, lines: &str) -> PathBuf {
    let dir = pool_dir(ws);
    fs::write(dir.join(".bad_tokens"), lines).unwrap();
    dir
}

/// One ambiguous (non-session-limit) exhaustion mark, stamped `hours` ago.
fn ambiguous_mark(hours: i64) -> String {
    (Utc::now() - chrono::Duration::seconds(hours * 3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

/// Store a fleet snapshot into `dir`'s sidecar (#13942).
fn apply_fleet_snapshot(
    dir: &Path,
    probed_at: chrono::DateTime<Utc>,
    rows: Vec<crate::tokens_pool::fleet_probe::TokenProbeAccount>,
) {
    let snapshot = crate::tokens_pool::fleet_probe::TokenProbeSnapshot {
        probed_at,
        accounts: rows,
    };
    crate::tokens_pool::fleet_probe::write_sidecar(dir, &snapshot).unwrap();
}

fn probe_account(
    name: &str,
    status: &str,
    util: Option<f64>,
) -> crate::tokens_pool::fleet_probe::TokenProbeAccount {
    crate::tokens_pool::fleet_probe::TokenProbeAccount {
        name: name.into(),
        status: status.into(),
        util_5h: util,
        limit_reset: None,
    }
}

/// AC 2 (#13942): two workers fed the same snapshot report the same
/// usable-account count. Worker B is fresh (no local `.ranking` at all —
/// the exact host the incident's 1/20 count came from); worker A still has
/// a local `.ranking` its own probe wrote, which the captain's newer probe
/// outranks. The verdicts must be identical account by account: released,
/// held to the recorded reset, and fixed-cooldown for an account the
/// snapshot has no row for.
#[test]
#[serial]
fn two_workers_fed_the_same_snapshot_agree() {
    let mark = ambiguous_mark(1); // inside the 6h cooldown: evidence decides
    let reset = (Utc::now() + chrono::Duration::seconds(4 * 24 * 3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let marks = format!(
        "{mark} agent-1 exhausted: used 100% of your weekly limit\n\
         {mark} agent-2 exhausted: used 100% of your weekly limit\n\
         {mark} agent-3 exhausted: used 100% of your weekly limit\n"
    );

    let worker_a = make_pool();
    let dir_a = pool_with_bad_lines(worker_a.path(), &marks);
    // Worker A's own last probe: stale next to the captain's.
    fs::write(dir_a.join(".ranking"), "agent-1|exhausted||1970-01-01T00:00:00Z\n").unwrap();

    let worker_b = make_pool();
    let dir_b = pool_with_bad_lines(worker_b.path(), &marks);
    // Worker B has no local evidence at all.

    // The captain's snapshot: strictly newer than A's local `.ranking`
    // mtime (written above), so it wins on both hosts.
    let snapshot_rows = vec![
        probe_account("agent-1", "available", Some(0.10)),
        crate::tokens_pool::fleet_probe::TokenProbeAccount {
            name: "agent-2".into(),
            status: "exhausted".into(),
            util_5h: None,
            limit_reset: Some(reset),
        },
        // agent-3: absent from the snapshot — no evidence either way.
    ];
    let probed_at = Utc::now() + chrono::Duration::seconds(61);
    apply_fleet_snapshot(&dir_a, probed_at, snapshot_rows.clone());
    apply_fleet_snapshot(&dir_b, probed_at, snapshot_rows);

    let usable = |dir: &Path| -> Vec<(String, bool)> {
        ["agent-1", "agent-2", "agent-3"]
            .iter()
            .map(|name| ((*name).to_string(), blocking_entry_in_dir(dir, name).is_none()))
            .collect()
    };
    let a = usable(&dir_a);
    let b = usable(&dir_b);
    assert_eq!(a, b, "two workers fed the same snapshot must agree");
    let verdict =
        |usable: &[(String, bool)], name: &str| usable.iter().find(|(n, _)| n == name).unwrap().1;
    assert!(verdict(&a, "agent-1"), "released: probe shows both windows clear");
    assert!(!verdict(&a, "agent-2"), "held: still exhausted, reset recorded");
    assert!(!verdict(&a, "agent-3"), "no row: fixed cooldown applies unchanged");
}

/// AC 3 (#13942): a snapshot older than the mark is ignored — only a probe
/// taken after the mark may release or extend the hold.
#[test]
#[serial]
fn a_snapshot_older_than_the_mark_is_ignored() {
    let tmp = make_pool();
    // Marked 1h ago; the fleet snapshot was probed 2h ago — before the mark.
    let dir = pool_with_bad_lines(
        tmp.path(),
        &format!("{} agent-1 exhausted: used 100% of your weekly limit\n", ambiguous_mark(1)),
    );
    apply_fleet_snapshot(
        &dir,
        Utc::now() - chrono::Duration::seconds(2 * 3600),
        vec![probe_account("agent-1", "available", Some(0.10))],
    );
    assert!(
        blocking_entry_in_dir(&dir, "agent-1").is_some(),
        "a pre-mark snapshot must not release the entry"
    );

    // The same shape, probed after the mark: released.
    apply_fleet_snapshot(&dir, Utc::now(), vec![probe_account("agent-1", "available", Some(0.10))]);
    assert!(
        blocking_entry_in_dir(&dir, "agent-1").is_none(),
        "a post-mark snapshot proving recovery releases it"
    );
}

/// AC 3 (#13942): the readers prefer the newer of `.ranking` and the sidecar
/// — a locally refreshed `.ranking` outranks an older fleet snapshot, so a
/// host that probes its own pool keeps today's behaviour byte-for-byte.
#[test]
#[serial]
fn a_local_ranking_newer_than_the_sidecar_still_wins() {
    let tmp = make_pool();
    let dir = pool_with_bad_lines(
        tmp.path(),
        &format!("{} agent-1 exhausted: used 100% of your weekly limit\n", ambiguous_mark(1)),
    );
    // A fleet snapshot an hour old says still-exhausted with a far reset…
    apply_fleet_snapshot(
        &dir,
        Utc::now() - chrono::Duration::seconds(3600),
        vec![crate::tokens_pool::fleet_probe::TokenProbeAccount {
            name: "agent-1".into(),
            status: "exhausted".into(),
            util_5h: None,
            limit_reset: Some(
                (Utc::now() + chrono::Duration::seconds(4 * 24 * 3600))
                    .format("%Y-%m-%dT%H:%M:%SZ")
                    .to_string(),
            ),
        }],
    );
    // …but this host's own newer probe already showed recovery.
    fs::write(dir.join(".ranking"), "agent-1|available|0.10\n").unwrap();
    assert_eq!(
        blocking_entry_in_dir(&dir, "agent-1"),
        None,
        "the newer local probe releases; the older fleet snapshot cannot hold"
    );
}
