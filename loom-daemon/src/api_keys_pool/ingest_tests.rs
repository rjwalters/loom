//! Tests for [`super`] — post-hoc automatic bad-marking from a launch log
//! (#8424 item 1), including the auth-vs-exhaustion split (item 5) and the
//! model-class scoping of an automatic mark (item 3).

use super::*;
use crate::api_keys_pool::classify::CAPTURED_OPENCODE_AUTH_EVENT;
use crate::api_keys_pool::{paths, registry, select};

const PROVIDER: &str = "loomtest";
const ANCHOR: &str = "==== loom-daemon dispatch: sweep_id=sweep-issue-8424-1 ====";

/// A workspace with `names` registered under [`PROVIDER`] in its per-repo
/// pool — the same layout `worker_spawn::credential` selects from.
fn workspace(names: &[&str]) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let root = paths::per_repo_api_keys_dir(tmp.path());
    for name in names {
        registry::add(&root, PROVIDER, name, "LOOM_TEST_KEY_8424", "fake-key", false).unwrap();
    }
    tmp
}

fn pool_root(workspace: &Path) -> std::path::PathBuf {
    paths::per_repo_api_keys_dir(workspace)
}

/// The exact line `worker_spawn::run` writes before `exec`ing a native
/// harness (same keys, same order).
fn launch_line(source: &str, account: Option<&str>, model: &str) -> String {
    format!(
        "{LAUNCH_RECORD_PREFIX}{}",
        serde_json::json!({
            "schema": 1,
            "runtime": "opencode",
            "provider": "zai-coding-plan",
            "model": model,
            "profile": "zai-flash",
            "effort": null,
            "credentialSource": source,
            "credentialProvider": if source == "pool" { Some(PROVIDER) } else { None },
            "credentialAccount": account,
            "usage": "native-json-events",
            "billing": "not-measured",
        })
    )
}

/// A whole retained log: dispatch anchor, Loom's markers, then the harness's
/// own output.
fn log(source: &str, account: Option<&str>, model: &str, harness_output: &str) -> String {
    format!(
        "{ANCHOR}\n{}\nspawn-worker: runtime=opencode (from config)\n# LOOM_CLI_START \
         runtime=opencode\n{harness_output}\n",
        launch_line(source, account, model)
    )
}

fn active(workspace: &Path, name: &str, class: Option<&str>) -> Option<BadMark> {
    bad_marks::active_mark_for_class(
        &pool_root(workspace),
        PROVIDER,
        name,
        class,
        bad_marks::epoch_now(),
    )
    .unwrap()
}

/// The acceptance criterion for item 1: a real failed spawn's log results in
/// an automatic `mark_bad` call, and the account drops out of selection.
#[test]
fn an_exhaustion_in_a_launch_log_bad_marks_the_pool_account_automatically() {
    let tmp = workspace(&["alpha", "beta"]);
    let contents = log(
        "pool",
        Some("alpha"),
        "glm-5.3-flash",
        "Error: insufficient balance for this account",
    );

    let feedback = ingest_launch_log(tmp.path(), &contents, ANCHOR, Some(1)).unwrap();
    assert_eq!(feedback.provider, PROVIDER);
    assert_eq!(feedback.account, "alpha");
    assert_eq!(feedback.classification, Classification::Exhausted);
    assert!(feedback.mark.is_some(), "{}", feedback.detail);
    assert!(feedback.detail.contains("bad-marked"), "{}", feedback.detail);

    // It is a real mark, and selection now skips the marked account.
    assert!(active(tmp.path(), "alpha", Some("glm-5.3-flash")).is_some());
    let chosen = select::select_api_key_for(
        tmp.path(),
        PROVIDER,
        Some("LOOM_TEST_KEY_8424"),
        Some("glm-5.3-flash"),
        None,
    )
    .unwrap();
    assert_eq!(chosen.name, "beta");
}

/// #8424 item 5 / AC 5, end to end: the real captured `provider.auth` 401
/// event is surfaced as a credential failure and the account is **not**
/// marked, so no exhaustion reset horizon is applied to a healthy key.
#[test]
fn the_captured_401_event_is_surfaced_but_never_bad_marked() {
    let tmp = workspace(&["alpha"]);
    let contents = log("pool", Some("alpha"), "glm-5.3-flash", CAPTURED_OPENCODE_AUTH_EVENT);

    let feedback = ingest_launch_log(tmp.path(), &contents, ANCHOR, Some(1)).unwrap();
    assert_eq!(feedback.classification, Classification::CredentialFailure);
    assert!(feedback.mark.is_none());
    assert!(feedback.detail.contains("NOT bad-marked"), "{}", feedback.detail);

    // No mark of any scope exists, and the account stays selectable.
    assert!(active(tmp.path(), "alpha", None).is_none());
    assert_eq!(bad_marks::read_marks(&pool_root(tmp.path()), PROVIDER).unwrap(), Vec::new());
    assert!(select::select_api_key(tmp.path(), PROVIDER, None).is_ok());
}

/// Guard 1: a credential the operator exported for a one-off run is never
/// bad-marked, even on an identical exhaustion.
#[test]
fn an_env_supplied_credential_is_never_marked() {
    let tmp = workspace(&["alpha"]);
    let contents = log("env", None, "glm-5.3-flash", "Error: insufficient balance");
    assert!(ingest_launch_log(tmp.path(), &contents, ANCHOR, Some(1)).is_none());
    assert_eq!(bad_marks::read_marks(&pool_root(tmp.path()), PROVIDER).unwrap(), Vec::new());
}

/// Guard 3: a run the harness completed is not evidence about an allowance,
/// however many rate-limit banners its log quotes.
#[test]
fn an_exit_zero_run_is_never_marked() {
    let tmp = workspace(&["alpha"]);
    let contents = log(
        "pool",
        Some("alpha"),
        "glm-5.3-flash",
        "warning: rate limit hit, retrying\nall done",
    );
    assert!(ingest_launch_log(tmp.path(), &contents, ANCHOR, Some(0)).is_none());
    // The same log with a failing exit IS ingested.
    let feedback = ingest_launch_log(tmp.path(), &contents, ANCHOR, Some(1)).unwrap();
    assert_eq!(feedback.classification, Classification::RateLimited);
}

/// Region scoping: a previous launch's failure in the same append-only log is
/// never attributed to this run.
#[test]
fn a_previous_launchs_failure_is_not_attributed_to_this_run() {
    let tmp = workspace(&["alpha"]);
    let old = log("pool", Some("alpha"), "glm-5.3-flash", "Error: insufficient balance");
    let fresh_anchor = "==== loom-daemon dispatch: sweep_id=sweep-issue-8424-2 ====";
    let contents = format!(
        "{old}{fresh_anchor}\n{}\nall done\n",
        launch_line("pool", Some("alpha"), "glm-5.3-flash")
    );
    assert!(ingest_launch_log(tmp.path(), &contents, fresh_anchor, Some(1)).is_none());
    // An anchor that never appears is no opinion, not "read the whole file".
    assert!(ingest_launch_log(tmp.path(), &contents, "sweep_id=absent", Some(1)).is_none());
    assert_eq!(bad_marks::read_marks(&pool_root(tmp.path()), PROVIDER).unwrap(), Vec::new());
}

#[test]
fn an_unrecognised_failure_is_no_opinion() {
    let tmp = workspace(&["alpha"]);
    for output in [
        "connection reset by peer",
        "",
        "panicked at src/main.rs:429:13",
    ] {
        let contents = log("pool", Some("alpha"), "glm-5.3-flash", output);
        assert!(
            ingest_launch_log(tmp.path(), &contents, ANCHOR, Some(1)).is_none(),
            "{output:?}"
        );
    }
    // A log with no launch record at all is likewise no opinion.
    let bare = format!("{ANCHOR}\nError: insufficient balance\n");
    assert!(ingest_launch_log(tmp.path(), &bare, ANCHOR, Some(1)).is_none());
}

/// #8521: the anchored region is the run's **entire transcript**, so the
/// agent's own words are in it. Both native harnesses are launched in a JSON
/// event mode (`pi --print --mode json`, `opencode run --format json`, see
/// `worker_spawn::harness`), which is what makes the agent's prose separable:
/// it arrives inside non-`error` events. An agent that quotes an exhaustion
/// phrase — this repo's own `judge.md` GraphQL rate-limit signature table
/// holds three of the classifier's needles verbatim — and then exits nonzero
/// must NOT bad-mark the account it ran on.
#[test]
fn an_exhaustion_needle_in_the_agents_own_transcript_never_bad_marks() {
    let tmp = workspace(&["alpha"]);
    let transcript = [
        r#"{"type":"text","text":"judge.md's signature table lists quota exceeded, rate limit and too many requests as the GraphQL exhaustion signatures."}"#,
        r#"{"type":"tool_use","tool":"loom_read","input":{"path":"judge.md"}}"#,
        r#"{"type":"step_finish","tool":"loom_read"}"#,
        // The harness's own statement of what went wrong is unrelated: a 403
        // is deliberately left unclassified (see `classify_error_event`).
        r#"{"type":"error","error":{"type":"provider.http","status":403}}"#,
    ]
    .join("\n");
    let contents = log("pool", Some("alpha"), "glm-5.3-flash", &transcript);

    assert!(
        ingest_launch_log(tmp.path(), &contents, ANCHOR, Some(1)).is_none(),
        "the agent's own transcript must not be read as the provider speaking"
    );
    assert_eq!(bad_marks::read_marks(&pool_root(tmp.path()), PROVIDER).unwrap(), Vec::new());
    assert!(active(tmp.path(), "alpha", None).is_none());
    assert!(select::select_api_key(tmp.path(), PROVIDER, None).is_ok());
}

/// The other half of #8521: narrowing the prose fallback must not cost a true
/// positive. Each of these is the provider/harness speaking, not the agent.
#[test]
fn a_genuine_provider_signal_still_bad_marks_after_the_transcript_narrowing() {
    // 1. Plain adapter/CLI stderr prose — not part of any event stream.
    let tmp = workspace(&["alpha"]);
    let contents = log(
        "pool",
        Some("alpha"),
        "glm-5.3-flash",
        "{\"type\":\"text\",\"text\":\"starting work\"}\nError: insufficient balance",
    );
    let feedback = ingest_launch_log(tmp.path(), &contents, ANCHOR, Some(1)).unwrap();
    assert_eq!(feedback.classification, Classification::Exhausted);
    assert!(feedback.mark.is_some(), "{}", feedback.detail);

    // 2. A structured error event the classifier reads structurally.
    let tmp = workspace(&["alpha"]);
    let contents = log(
        "pool",
        Some("alpha"),
        "glm-5.3-flash",
        r#"{"type":"error","error":{"type":"provider.http","status":402}}"#,
    );
    let feedback = ingest_launch_log(tmp.path(), &contents, ANCHOR, Some(1)).unwrap();
    assert_eq!(feedback.classification, Classification::Exhausted);
    assert!(feedback.mark.is_some(), "{}", feedback.detail);

    // 3. An error event whose *status* is unrecognised but whose message is
    //    the provider's own words: an error event is the harness speaking, so
    //    its prose is still read.
    let tmp = workspace(&["alpha"]);
    let contents = log(
        "pool",
        Some("alpha"),
        "glm-5.3-flash",
        r#"{"type":"error","error":{"type":"provider.http","message":"quota exceeded for coding plan","status":400}}"#,
    );
    let feedback = ingest_launch_log(tmp.path(), &contents, ANCHOR, Some(1)).unwrap();
    assert_eq!(feedback.classification, Classification::Exhausted);
    assert!(feedback.mark.is_some(), "{}", feedback.detail);

    // 4. A raw provider error body echoed into the log: JSON, but not a
    //    harness event at all (no `type`), so it is provider text.
    let tmp = workspace(&["alpha"]);
    let contents = log(
        "pool",
        Some("alpha"),
        "glm-5.3-flash",
        r#"{"error":{"code":"1113","message":"balance exhausted"}}"#,
    );
    let feedback = ingest_launch_log(tmp.path(), &contents, ANCHOR, Some(1)).unwrap();
    assert_eq!(feedback.classification, Classification::Exhausted);
    assert!(feedback.mark.is_some(), "{}", feedback.detail);
}

/// #8424 item 3 through the automatic path: the mark is scoped to the model
/// the launch record names, so the same account stays selectable for another
/// model class.
#[test]
fn an_automatic_mark_is_scoped_to_the_launch_records_model_class() {
    let tmp = workspace(&["alpha"]);
    let contents = log("pool", Some("alpha"), "glm-5.3-flash#high", "Error: insufficient balance");
    let feedback = ingest_launch_log(tmp.path(), &contents, ANCHOR, Some(1)).unwrap();
    assert_eq!(feedback.model_class.as_deref(), Some("glm-5.3-flash"));
    assert_eq!(feedback.mark.unwrap().model_class.as_deref(), Some("glm-5.3-flash"));

    assert!(active(tmp.path(), "alpha", Some("glm-5.3-flash")).is_some());
    assert!(active(tmp.path(), "alpha", Some("glm-5")).is_none());
    // The only account in the pool is still selectable for the other class…
    assert_eq!(
        select::select_api_key_for(
            tmp.path(),
            PROVIDER,
            Some("LOOM_TEST_KEY_8424"),
            Some("glm-5"),
            None
        )
        .unwrap()
        .name,
        "alpha"
    );
    // …and is not, for the marked one.
    assert!(select::select_api_key_for(
        tmp.path(),
        PROVIDER,
        Some("LOOM_TEST_KEY_8424"),
        Some("glm-5.3-flash"),
        None
    )
    .is_err());
}

/// With no usable model in the record, the automatic mark is account-wide —
/// never silently narrowed to a class nothing named.
#[test]
fn without_a_usable_model_the_automatic_mark_is_account_wide() {
    let tmp = workspace(&["alpha"]);
    let contents = log("pool", Some("alpha"), "   ", "Error: insufficient balance");
    let feedback = ingest_launch_log(tmp.path(), &contents, ANCHOR, Some(1)).unwrap();
    assert_eq!(feedback.model_class, None);
    assert!(feedback.detail.contains("account-wide"), "{}", feedback.detail);
    for class in [None, Some("glm-5"), Some("glm-5.3-flash")] {
        assert!(active(tmp.path(), "alpha", class).is_some(), "{class:?}");
    }
}

/// The last launch record in a region wins: a re-dispatch inside one sweep
/// must mark the account the failing launch actually used.
#[test]
fn the_last_launch_record_in_the_region_wins() {
    let tmp = workspace(&["alpha", "beta"]);
    let contents = format!(
        "{ANCHOR}\n{}\nretrying on another account\n{}\nError: insufficient balance\n",
        launch_line("pool", Some("alpha"), "glm-5.3-flash"),
        launch_line("pool", Some("beta"), "glm-5.3-flash"),
    );
    let feedback = ingest_launch_log(tmp.path(), &contents, ANCHOR, Some(1)).unwrap();
    assert_eq!(feedback.account, "beta");
    assert!(active(tmp.path(), "alpha", None).is_none());
    assert!(active(tmp.path(), "beta", None).is_some());
}

/// Marking an account that is no longer registered fails the write, and that
/// failure is reported rather than panicking or changing the outcome.
#[test]
fn a_failed_write_is_reported_not_propagated() {
    let tmp = workspace(&[]);
    let contents = log("pool", Some("ghost"), "glm-5.3-flash", "Error: insufficient balance");
    let feedback = ingest_launch_log(tmp.path(), &contents, ANCHOR, Some(1)).unwrap();
    assert!(feedback.mark.is_none());
    assert!(feedback.detail.contains("could not bad-mark"), "{}", feedback.detail);
}

#[test]
fn the_path_wrapper_treats_an_unreadable_log_as_no_opinion() {
    let tmp = workspace(&["alpha"]);
    let missing = tmp.path().join("never-written.log");
    assert!(ingest_launch_log_at(tmp.path(), &missing, ANCHOR, Some(1)).is_none());

    let written = tmp.path().join("worker.log");
    std::fs::write(
        &written,
        log("pool", Some("alpha"), "glm-5.3-flash", "Error: insufficient balance"),
    )
    .unwrap();
    let feedback = ingest_launch_log_at(tmp.path(), &written, ANCHOR, Some(1)).unwrap();
    assert_eq!(feedback.account, "alpha");
}

/// An empty anchor means "this whole log is one launch" — the shape a caller
/// with a per-launch `--log` file has.
#[test]
fn an_empty_anchor_reads_the_whole_log() {
    let tmp = workspace(&["alpha"]);
    let contents = format!(
        "{}\nError: insufficient balance\n",
        launch_line("pool", Some("alpha"), "glm-5.3-flash")
    );
    let feedback = ingest_launch_log(tmp.path(), &contents, "", Some(1)).unwrap();
    assert_eq!(feedback.account, "alpha");
}

#[test]
fn parse_launch_record_reads_the_pool_namespace_not_the_harness_provider() {
    let record = parse_launch_record(&launch_line("pool", Some("alpha"), "glm-5")).unwrap();
    assert_eq!(record.provider.as_deref(), Some(PROVIDER));
    assert!(record.is_pool_selected());
    assert_eq!(record.account.as_deref(), Some("alpha"));
    assert_eq!(record.model.as_deref(), Some("glm-5"));

    // A malformed record is simply not a record.
    assert!(parse_launch_record("# LOOM_LAUNCH {not json").is_none());
    assert!(parse_launch_record("nothing here").is_none());
}

/// The detail line is safe to log: it names the account, never key material.
#[test]
fn the_detail_line_never_carries_key_material() {
    let tmp = workspace(&["alpha"]);
    let contents = log("pool", Some("alpha"), "glm-5.3-flash", "Error: insufficient balance");
    let feedback = ingest_launch_log(tmp.path(), &contents, ANCHOR, Some(1)).unwrap();
    assert!(!feedback.detail.contains("fake-key"), "{}", feedback.detail);
    assert!(!format!("{feedback:?}").contains("fake-key"), "{feedback:?}");
}

/// #8563 acceptance criterion: a Kimi credential failure/exhaustion must
/// never bad-mark a Claude or Codex account. The three pools are
/// structurally separate stores keyed by provider (this pool's own
/// `<provider>/.bad_accounts.json`, Claude's `.loom/tokens/.bad_tokens`,
/// Codex's `.loom/account-health.json`) and `ingest_launch_log`'s only
/// filesystem write is [`bad_marks::mark_bad_for_class`] scoped to whatever
/// `credentialProvider` the launch record names — so this pins that in one
/// test rather than leaving it to hold only by inspection. The Kimi account
/// and the (never-created) Claude/Codex accounts deliberately share a name,
/// so a name collision across pools cannot masquerade as isolation.
#[test]
fn a_kimi_failure_never_touches_the_claude_or_codex_pools() {
    use crate::api_keys_pool::classify::{
        CAPTURED_KIMI_NO_MODEL_CONFIGURED, CAPTURED_KIMI_RATE_LIMIT_EXHAUSTED,
    };
    use crate::tokens_pool::account_registry::{AccountId, AccountProvider};

    const KIMI: &str = "kimi";
    let tmp = tempfile::tempdir().unwrap();
    let root = paths::per_repo_api_keys_dir(tmp.path());
    registry::add(&root, KIMI, "alpha", "KIMI_MODEL_API_KEY", "fake-kimi-secret", false).unwrap();

    let launch_kimi = |harness_output: &str| {
        format!(
            "{ANCHOR}\n{LAUNCH_RECORD_PREFIX}{}\n{harness_output}\n",
            serde_json::json!({
                "schema": 1,
                "runtime": "kimi",
                "provider": "moonshot",
                "model": "kimi-k2",
                "profile": "example-kimi-moonshot-api",
                "effort": null,
                "credentialSource": "pool",
                "credentialProvider": KIMI,
                "credentialAccount": "alpha",
                "usage": "native-json-events",
                "billing": "not-measured",
            })
        )
    };

    // Both real Kimi captures — the credential failure and the exhausted
    // rate-limit ladder — are ingested against the same account name.
    for output in [
        CAPTURED_KIMI_NO_MODEL_CONFIGURED,
        CAPTURED_KIMI_RATE_LIMIT_EXHAUSTED,
    ] {
        let contents = launch_kimi(output);
        let feedback = ingest_launch_log(tmp.path(), &contents, ANCHOR, Some(1));
        assert!(feedback.is_some(), "{output:?} should classify");
    }

    // The only write landed in this pool's own per-provider file...
    assert!(bad_marks::read_marks(&pool_root(tmp.path()), KIMI).is_ok());
    // ...and nothing ever reached Claude's or Codex's separate stores, which
    // this isolated workspace never had a reason to create.
    assert!(!tmp
        .path()
        .join(".loom")
        .join("tokens")
        .join(".bad_tokens")
        .exists());
    assert!(!tmp
        .path()
        .join(".loom")
        .join("account-health.json")
        .exists());
    assert!(!crate::tokens_pool::bad_tokens::is_bad(tmp.path(), "alpha"));
    assert!(crate::tokens_pool::health::account_health(
        tmp.path(),
        &AccountId {
            provider: AccountProvider::Codex,
            name: "alpha".to_string()
        }
    )
    .unwrap()
    .is_none());
}
