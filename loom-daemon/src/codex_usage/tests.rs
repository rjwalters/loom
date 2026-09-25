//! Tests for the Codex rollout-store reader (Issue #8594).
//!
//! The fixture `$CODEX_HOME` is built from the **live** layout on this host
//! (surveyed 2026-09-22, see the module doc's "Schema provenance": 140 rollout
//! files carrying 12,001 `token_count` events of which 11,868 have a non-null
//! `info`, written by `codex` 0.46.0 and 0.154.0), *including*
//! the credential-bearing siblings — `auth.json`, `config.toml`,
//! `history.jsonl`, `sqlite/codex-dev.db` — that sit beside `sessions/` in the
//! real directory. So the credential-isolation tests exercise the same shape a
//! production read would meet, not a sanitised stand-in.

use super::*;
use std::io::Write as _;

/// A secret planted in every credential-bearing sibling of `sessions/`. No
/// value this module returns may ever contain it.
const PLANTED_SECRET: &str = "sk-fake-codex-secret-should-never-be-read";

/// One `token_count` event body, in the live shape (`total_token_usage` is
/// CUMULATIVE — see the module doc).
fn token_count(input: i64, cached: i64, output: i64, reasoning: i64) -> String {
    serde_json::json!({
        "timestamp": "2026-09-21T02:06:34.117Z",
        "type": "event_msg",
        "payload": {
            "type": "token_count",
            "info": {
                "total_token_usage": {
                    "input_tokens": input,
                    "cached_input_tokens": cached,
                    "output_tokens": output,
                    "reasoning_output_tokens": reasoning,
                    "total_tokens": input + output,
                },
                "last_token_usage": {
                    "input_tokens": input,
                    "cached_input_tokens": cached,
                    "output_tokens": output,
                    "reasoning_output_tokens": reasoning,
                    "total_tokens": input + output,
                },
                "model_context_window": 272_000,
            },
            "rate_limits": { "limit_id": "premium" },
        },
    })
    .to_string()
}

/// A `session_meta` line in the live 0.154.0 shape.
fn session_meta(cwd: &str, created: &str, id: &str, provider: Option<&str>) -> String {
    serde_json::json!({
        "timestamp": created,
        "type": "session_meta",
        "payload": {
            "session_id": id,
            "id": id,
            "timestamp": created,
            "cwd": cwd,
            "originator": "codex_exec",
            "cli_version": "0.154.0",
            "model_provider": provider,
            // The real record carries the whole system prompt here; planting
            // the secret proves the reader copies nothing it did not name.
            "base_instructions": { "text": PLANTED_SECRET },
        },
    })
    .to_string()
}

/// A `turn_context` line naming the model in force for the turns that follow.
fn turn_context(model: &str, cwd: &str) -> String {
    serde_json::json!({
        "timestamp": "2026-09-21T02:06:32.000Z",
        "type": "turn_context",
        "payload": { "cwd": cwd, "model": model, "effort": "low", "summary": "auto" },
    })
    .to_string()
}

/// A complete single-model rollout: meta, turn context, then N cumulative
/// `token_count` events — each written TWICE, exactly as the live store does.
fn rollout(
    cwd: &str,
    created: &str,
    id: &str,
    model: &str,
    steps: &[(i64, i64, i64, i64)],
) -> Vec<String> {
    let mut lines = vec![
        session_meta(cwd, created, id, Some("openai")),
        turn_context(model, cwd),
    ];
    for (input, cached, output, reasoning) in steps {
        let event = token_count(*input, *cached, *output, *reasoning);
        lines.push(event.clone());
        lines.push(event);
    }
    lines
}

/// Build a fixture `$CODEX_HOME` with the credential-bearing siblings the real
/// one has, and `rollouts` planted under `sessions/<Y>/<M>/<D>/`.
///
/// Returns the home path. The caller owns the `TempDir`.
fn seed_codex_home(root: &Path, rollouts: &[(&str, &str, Vec<String>)]) -> PathBuf {
    let home = root.join(".codex");
    std::fs::create_dir_all(home.join("sqlite")).unwrap();
    // Every credential-bearing sibling the live directory has, each poisoned.
    for name in [
        "auth.json",
        "config.toml",
        "history.jsonl",
        "models_cache.json",
    ] {
        std::fs::write(home.join(name), format!("{{\"token\":\"{PLANTED_SECRET}\"}}")).unwrap();
    }
    std::fs::write(home.join("sqlite/codex-dev.db"), PLANTED_SECRET).unwrap();
    std::fs::write(home.join("goals_1.sqlite"), PLANTED_SECRET).unwrap();
    // …and a decoy INSIDE `sessions/`, to prove the filename gate matters and
    // not merely the directory one.
    std::fs::create_dir_all(home.join(SESSIONS_DIR)).unwrap();
    std::fs::write(home.join(SESSIONS_DIR).join("auth.json"), PLANTED_SECRET).unwrap();
    for (date_dir, filename, lines) in rollouts {
        let dir = home.join(SESSIONS_DIR).join(date_dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut file = std::fs::File::create(dir.join(filename)).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
    }
    home
}

fn instant(raw: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(raw)
        .unwrap()
        .with_timezone(&Utc)
}

// ---------------------------------------------------------------------------
// Security contract: `sessions/**/rollout-*.jsonl`, ever
// ---------------------------------------------------------------------------

/// The reader module's own source with every comment line removed, so a
/// security scan asserts on the CODE and is not defeated (or tripped) by the
/// module doc, which legitimately names the credential-bearing siblings it
/// exists to avoid.
fn reader_code() -> String {
    include_str!("../codex_usage.rs")
        .lines()
        .filter(|line| {
            let t = line.trim_start();
            !(t.starts_with("//") || t.starts_with("/*") || t.starts_with('*'))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_only_file_open_in_this_module_is_the_guarded_rollout_reader() {
    // Behavioural tests can only prove what the returned rows *contain*; this
    // proves the module never grows a second file-open at all. `include_str!`
    // reads this module's own sibling source, so a future edit that adds a
    // read of `$CODEX_HOME`'s auth store fails here with the reason — the
    // counterpart of `opencode_usage`'s `.prepare(` scan.
    let code = reader_code();
    for idiom in [
        "File::open(",
        "File::create(",
        "read_to_string(",
        "fs::read(",
        "OpenOptions",
        "include_str!",
    ] {
        let count = code.matches(idiom).count();
        let allowed = usize::from(idiom == "File::open(");
        assert_eq!(
            count, allowed,
            "this module's code may contain exactly {allowed} `{idiom}` call(s): every \
             file read must go through `read_rollout`, which refuses any path \
             `is_rollout_path` rejects — $CODEX_HOME holds an auth store, a config \
             file, a full prompt history and several SQLite databases as siblings of \
             the sessions/ tree"
        );
    }
    // The single open must sit inside `read_rollout`, after the authorization
    // gate — asserted as an ordered pair within one function body, so hoisting
    // the open above the gate (or into another function) fails here.
    let body = code
        .split_once("fn read_rollout(")
        .expect("read_rollout must exist")
        .1;
    let body = body.split_once("\nfn ").map_or(body, |(head, _)| head);
    let gate = body
        .find("if !is_rollout_path(")
        .expect("read_rollout must gate on is_rollout_path");
    let open = body.find("File::open(").expect("the one open lives here");
    assert!(gate < open, "the authorization gate must precede the open");
    // No credential-bearing basename may appear in the module's CODE: the
    // reader has no legitimate reason to name one, so naming one is the tell.
    let lowered = code.to_ascii_lowercase();
    for forbidden in [
        "auth.json",
        "credential",
        "config.toml",
        "history.jsonl",
        ".sqlite",
    ] {
        assert!(
            !lowered.contains(forbidden),
            "this module's code must never name {forbidden}: every path it opens is a \
             `sessions/**/rollout-*.jsonl`"
        );
    }
}

#[test]
fn is_rollout_path_refuses_every_credential_bearing_sibling_and_decoy() {
    let home = Path::new("/h/.codex");
    let sessions = home.join(SESSIONS_DIR).join("2026/09/20");
    assert!(is_rollout_path(home, &sessions.join("rollout-x-1.jsonl")));
    for refused in [
        home.join("auth.json"),
        home.join("config.toml"),
        home.join("history.jsonl"),
        home.join("sqlite/codex-dev.db"),
        home.join("goals_1.sqlite"),
        // inside sessions/, but not a rollout name
        home.join(SESSIONS_DIR).join("auth.json"),
        sessions.join("auth.json"),
        sessions.join("rollout-x-1.json"),
        sessions.join("notarollout-x.jsonl"),
        // right name, wrong tree
        home.join("cache/rollout-x-1.jsonl"),
        PathBuf::from("/elsewhere/rollout-x-1.jsonl"),
    ] {
        assert!(!is_rollout_path(home, &refused), "{}", refused.display());
    }
}

#[test]
fn read_rollout_refuses_an_unauthorized_path_even_when_handed_one_directly() {
    let tmp = tempfile::tempdir().unwrap();
    let home = seed_codex_home(tmp.path(), &[]);
    // Every poisoned sibling exists on disk and is perfectly readable — the
    // refusal is the gate's, not the filesystem's.
    for path in [
        home.join("auth.json"),
        home.join("history.jsonl"),
        home.join(SESSIONS_DIR).join("auth.json"),
    ] {
        assert!(path.is_file(), "fixture must exist: {}", path.display());
        assert_eq!(read_rollout(&home, &path), None, "{}", path.display());
    }
}

#[serial_test::serial(codex_home_env)]
#[test]
fn discovery_returns_only_rollouts_and_never_a_poisoned_sibling() {
    let tmp = tempfile::tempdir().unwrap();
    let home = seed_codex_home(
        tmp.path(),
        &[(
            "2026/09/20",
            "rollout-2026-09-20T19-06-31-aaaa.jsonl",
            rollout("/w", "2026-09-21T02:06:31Z", "aaaa", "gpt-5.6-sol", &[(10, 4, 5, 2)]),
        )],
    );
    let found = with_codex_home(&home, || discover_rollouts(None, None));
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].ends_with("rollout-2026-09-20T19-06-31-aaaa.jsonl"));
}

#[serial_test::serial(codex_home_env)]
#[test]
fn a_planted_secret_never_surfaces_in_the_returned_rows_or_their_totals() {
    let tmp = tempfile::tempdir().unwrap();
    let home = seed_codex_home(
        tmp.path(),
        &[(
            "2026/09/20",
            "rollout-a.jsonl",
            rollout("/w", "2026-09-21T02:06:31Z", "aaaa", "gpt-5.6-sol", &[(90, 40, 20, 8)]),
        )],
    );
    let filter = SessionFilter::directories(&[PathBuf::from("/w")]);
    let rows = with_codex_home(&home, || sessions(&filter, None, None));
    assert_eq!(rows.len(), 1, "{rows:?}");
    let rendered = format!("{rows:?}");
    assert!(
        !rendered.contains(PLANTED_SECRET),
        "no credential-bearing byte may reach a returned row: {rendered}"
    );
    let totals = fold_sessions(rows).unwrap();
    assert!(!format!("{totals:?}").contains(PLANTED_SECRET));
    // And the secret really was there to be leaked.
    assert!(std::fs::read_to_string(home.join("auth.json"))
        .unwrap()
        .contains(PLANTED_SECRET));
}

// ---------------------------------------------------------------------------
// Decoding: the cumulative/duplicated/nested-counter contract
// ---------------------------------------------------------------------------

#[test]
fn duplicate_token_count_events_are_counted_once_because_totals_are_cumulative() {
    // Live store emits each `token_count` twice in a row with identical
    // contents (module doc's provenance survey). Delta accumulation makes the
    // duplicate contribute exactly 0.
    let lines = rollout(
        "/w",
        "2026-09-21T02:06:31Z",
        "aaaa",
        "gpt-5.6-sol",
        &[(9_090, 5_888, 418, 320), (22_409, 11_776, 528, 384)],
    );
    let rows = fold_rollout(&lines, &SessionFilter::directories(&[PathBuf::from("/w")]), None);
    assert_eq!(rows.len(), 1, "{rows:?}");
    // The FINAL cumulative reading, not a sum over the (duplicated) events.
    assert_eq!(rows[0].cache_read, 11_776);
    assert_eq!(rows[0].input, 22_409 - 11_776);
    assert_eq!(rows[0].output, 528);
}

#[test]
fn reasoning_is_not_added_to_output_and_cached_input_is_not_counted_twice() {
    // Verified live: reasoning_output_tokens > output_tokens in 0/11868
    // samples, cached_input_tokens > input_tokens in 0/11868. So reasoning is
    // a SUBSET of output (unlike OpenCode's separate `tokens_reasoning`), and
    // cached input is a subset of input.
    let lines =
        rollout("/w", "2026-09-21T02:06:31Z", "aaaa", "gpt-5.6-sol", &[(9_090, 5_888, 418, 320)]);
    let rows = fold_rollout(&lines, &SessionFilter::directories(&[PathBuf::from("/w")]), None);
    assert_eq!(rows[0].output, 418, "reasoning (320) must NOT be added on top");
    assert_eq!(rows[0].input, 3_202, "9090 - 5888");
    assert_eq!(rows[0].cache_read, 5_888);
    // Nothing is lost: the disjoint parts re-add to Codex's own input total.
    assert_eq!(rows[0].input + rows[0].cache_read, 9_090);
}

#[test]
fn a_mid_session_model_switch_splits_usage_by_the_turn_context_in_force() {
    let cwd = "/w";
    let mut lines = vec![
        session_meta(cwd, "2026-09-21T02:06:31Z", "aaaa", Some("openai")),
        turn_context("gpt-5.6-sol", cwd),
        token_count(100, 0, 10, 0),
        token_count(100, 0, 10, 0),
    ];
    lines.push(turn_context("gpt-5", cwd));
    lines.push(token_count(250, 0, 25, 0));
    lines.push(token_count(250, 0, 25, 0));
    let rows = fold_rollout(&lines, &SessionFilter::directories(&[PathBuf::from(cwd)]), None);
    assert_eq!(rows.len(), 2, "one row per model: {rows:?}");
    let by_model: BTreeMap<&str, &CodexSessionUsage> =
        rows.iter().map(|r| (r.model.as_str(), r)).collect();
    assert_eq!(by_model["gpt-5.6-sol"].input, 100);
    assert_eq!(by_model["gpt-5.6-sol"].output, 10);
    // The SECOND model is credited only the delta, never the running total.
    assert_eq!(by_model["gpt-5"].input, 150);
    assert_eq!(by_model["gpt-5"].output, 15);
}

#[test]
fn usage_before_any_turn_context_is_dropped_rather_than_given_a_guessed_model() {
    let lines = vec![
        session_meta("/w", "2026-09-21T02:06:31Z", "aaaa", Some("openai")),
        token_count(500, 0, 50, 0),
    ];
    assert!(
        fold_rollout(&lines, &SessionFilter::directories(&[PathBuf::from("/w")]), None).is_empty(),
        "never guess a model name"
    );
}

#[test]
fn a_null_info_token_count_is_silence_and_advances_nothing() {
    // Observed live on a session that produced no usage: `info: null`.
    let mut lines = vec![
        session_meta("/w", "2026-09-21T02:06:31Z", "aaaa", Some("openai")),
        turn_context("gpt-5.6-sol", "/w"),
        serde_json::json!({"type":"event_msg","payload":{"type":"token_count","info":null}})
            .to_string(),
    ];
    assert!(
        fold_rollout(&lines, &SessionFilter::directories(&[PathBuf::from("/w")]), None).is_empty(),
        "an all-silence rollout publishes no zero-token badge"
    );
    // …and it does not perturb a later real reading.
    lines.push(token_count(100, 0, 10, 0));
    let rows = fold_rollout(&lines, &SessionFilter::directories(&[PathBuf::from("/w")]), None);
    assert_eq!(rows[0].input, 100);
}

#[test]
fn a_counter_that_goes_backwards_never_subtracts_a_siblings_spend() {
    let cwd = "/w";
    let lines = vec![
        session_meta(cwd, "2026-09-21T02:06:31Z", "aaaa", Some("openai")),
        turn_context("gpt-5.6-sol", cwd),
        token_count(1_000, 0, 100, 0),
        turn_context("gpt-5", cwd),
        // A rebase/compaction reading LOWER than the running total.
        token_count(200, 0, 20, 0),
    ];
    let rows = fold_rollout(&lines, &SessionFilter::directories(&[PathBuf::from(cwd)]), None);
    // `gpt-5` earns 0 rather than -800, so it is dropped as usage-free;
    // `gpt-5.6-sol` keeps every token it really burned.
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].model, "gpt-5.6-sol");
    assert_eq!(rows[0].input, 1_000);
}

#[test]
fn a_rollout_with_no_readable_provider_still_reports_its_tokens() {
    // Pre-0.154.0 rollouts carry no `model_provider`.
    let mut lines = vec![
        session_meta("/w", "2026-09-21T02:06:31Z", "aaaa", None),
        turn_context("gpt-5", "/w"),
        token_count(100, 0, 10, 0),
    ];
    lines.push(token_count(100, 0, 10, 0));
    let rows = fold_rollout(&lines, &SessionFilter::directories(&[PathBuf::from("/w")]), None);
    assert_eq!(rows[0].provider, None, "omitted, never guessed as \"openai\"");
    assert_eq!(rows[0].input, 100);
}

#[test]
fn malformed_lines_are_skipped_rather_than_failing_the_whole_rollout() {
    let mut lines = vec!["not json at all".to_string(), "{".to_string()];
    lines.extend(rollout("/w", "2026-09-21T02:06:31Z", "aaaa", "gpt-5", &[(100, 0, 10, 0)]));
    lines.push("{\"type\":\"world_state\"}".to_string());
    let rows = fold_rollout(&lines, &SessionFilter::directories(&[PathBuf::from("/w")]), None);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].input, 100);
}

// ---------------------------------------------------------------------------
// Attribution: directory, window, and exact session id
// ---------------------------------------------------------------------------

#[test]
fn a_session_in_another_directory_is_never_attributed() {
    let lines = rollout("/other", "2026-09-21T02:06:31Z", "aaaa", "gpt-5", &[(100, 0, 10, 0)]);
    assert!(
        fold_rollout(&lines, &SessionFilter::directories(&[PathBuf::from("/w")]), None).is_empty()
    );
}

#[test]
fn an_empty_filter_matches_nothing_rather_than_every_session_on_the_host() {
    let lines = rollout("/w", "2026-09-21T02:06:31Z", "aaaa", "gpt-5", &[(100, 0, 10, 0)]);
    assert!(
        fold_rollout(&lines, &SessionFilter::default(), None).is_empty(),
        "\"attribute everything\" is never what a caller means"
    );
}

#[test]
fn the_window_is_applied_inclusively_to_the_sessions_creation_instant() {
    let lines = rollout("/w", "2026-09-21T02:06:31Z", "aaaa", "gpt-5", &[(100, 0, 10, 0)]);
    let filter = SessionFilter::directories(&[PathBuf::from("/w")]);
    let created = instant("2026-09-21T02:06:31Z");
    assert_eq!(fold_rollout(&lines, &filter, Some((created, created))).len(), 1);
    assert!(fold_rollout(
        &lines,
        &filter,
        Some((created + Duration::seconds(1), created + Duration::hours(1)))
    )
    .is_empty());
    assert!(fold_rollout(
        &lines,
        &filter,
        Some((created - Duration::hours(1), created - Duration::seconds(1)))
    )
    .is_empty());
}

#[test]
fn an_explicit_session_id_set_is_a_strictly_narrower_key_than_directory_alone() {
    // #8507's deferred refinement: the store carries the exact session id, so
    // a caller that has one can attribute on it instead of a wall-clock guess.
    let lines = rollout("/w", "2026-09-21T02:06:31Z", "wanted", "gpt-5", &[(100, 0, 10, 0)]);
    let dir = vec![PathBuf::from("/w")];
    let matching = SessionFilter {
        directories: dir.clone(),
        ids: vec!["wanted".to_string()],
    };
    assert_eq!(fold_rollout(&lines, &matching, None).len(), 1);
    let other = SessionFilter {
        directories: dir,
        ids: vec!["some-other-session".to_string()],
    };
    assert!(
        fold_rollout(&lines, &other, None).is_empty(),
        "an id set must exclude a same-directory, same-window sibling"
    );
    // The id is reported, so a caller can correlate without re-reading.
    assert_eq!(
        fold_rollout(&lines, &matching, None)[0]
            .session_id
            .as_deref(),
        Some("wanted")
    );
}

#[test]
fn an_id_only_filter_attributes_across_directories() {
    let lines = rollout("/anywhere", "2026-09-21T02:06:31Z", "wanted", "gpt-5", &[(100, 0, 10, 0)]);
    let filter = SessionFilter {
        directories: Vec::new(),
        ids: vec!["wanted".to_string()],
    };
    assert_eq!(fold_rollout(&lines, &filter, None).len(), 1);
}

// ---------------------------------------------------------------------------
// End-to-end over a fixture $CODEX_HOME
// ---------------------------------------------------------------------------

#[serial_test::serial(codex_home_env)]
#[test]
fn tokens_by_model_folds_every_attributable_rollout_in_the_store() {
    let tmp = tempfile::tempdir().unwrap();
    let home = seed_codex_home(
        tmp.path(),
        &[
            (
                "2026/09/20",
                "rollout-a.jsonl",
                rollout("/w", "2026-09-21T02:00:00Z", "a", "gpt-5", &[(1_000, 400, 100, 40)]),
            ),
            (
                "2026/09/20",
                "rollout-b.jsonl",
                rollout(
                    "/w/.loom/worktrees/issue-8594",
                    "2026-09-21T02:30:00Z",
                    "b",
                    "gpt-5",
                    &[(2_000, 800, 200, 80)],
                ),
            ),
            (
                "2026/09/20",
                "rollout-c.jsonl",
                rollout(
                    "/someone-elses-repo",
                    "2026-09-21T02:15:00Z",
                    "c",
                    "gpt-5",
                    &[(9, 0, 9, 0)],
                ),
            ),
        ],
    );
    let filter = SessionFilter::directories(&[
        PathBuf::from("/w"),
        PathBuf::from("/w/.loom/worktrees/issue-8594"),
    ]);
    let window = Some((instant("2026-09-21T01:00:00Z"), instant("2026-09-21T03:00:00Z")));
    let totals = with_codex_home(&home, || tokens_by_model(&filter, window, None)).unwrap();
    assert_eq!(totals.len(), 1, "{totals:?}");
    assert_eq!(totals[0].model, "gpt-5");
    assert_eq!(totals[0].speed, "standard");
    assert_eq!(totals[0].service_tier, "standard");
    assert_eq!(totals[0].cache_read, 400 + 800);
    assert_eq!(totals[0].input, (1_000 - 400) + (2_000 - 800));
    assert_eq!(totals[0].output, 100 + 200);
    // Codex reports no cache-write counter; a fabricated split would be
    // priced as real spend downstream.
    assert_eq!(totals[0].cache_write_5m, 0);
    assert_eq!(totals[0].cache_write_1h, 0);
}

#[serial_test::serial(codex_home_env)]
#[test]
fn an_empty_store_is_none_not_an_empty_vec() {
    let tmp = tempfile::tempdir().unwrap();
    let home = seed_codex_home(tmp.path(), &[]);
    let filter = SessionFilter::directories(&[PathBuf::from("/w")]);
    assert_eq!(with_codex_home(&home, || tokens_by_model(&filter, None, None)), None);
}

#[serial_test::serial(codex_home_env)]
#[test]
fn a_missing_codex_home_is_none_not_a_panic() {
    let tmp = tempfile::tempdir().unwrap();
    let absent = tmp.path().join("no-such-codex-home");
    let filter = SessionFilter::directories(&[PathBuf::from("/w")]);
    assert_eq!(with_codex_home(&absent, || tokens_by_model(&filter, None, None)), None);
    assert!(with_codex_home(&absent, || discover_rollouts(None, None)).is_empty());
}

#[serial_test::serial(codex_home_env)]
#[test]
fn the_date_partitioned_scan_pads_the_window_by_a_day_for_local_time_naming() {
    // Codex names `sessions/<Y>/<M>/<D>/` by the host's LOCAL date while the
    // window is UTC — this host's own store has a rollout named
    // `2026/09/20/rollout-2026-09-20T19-06-31-…` whose session_meta timestamp
    // is `2026-09-21T02:06:31Z`. Without the pad the directory would be
    // skipped and the session lost.
    let tmp = tempfile::tempdir().unwrap();
    let home = seed_codex_home(
        tmp.path(),
        &[(
            "2026/09/20",
            "rollout-2026-09-20T19-06-31-aaaa.jsonl",
            rollout("/w", "2026-09-21T02:06:31Z", "aaaa", "gpt-5", &[(100, 0, 10, 0)]),
        )],
    );
    let filter = SessionFilter::directories(&[PathBuf::from("/w")]);
    let window = Some((instant("2026-09-21T02:00:00Z"), instant("2026-09-21T03:00:00Z")));
    assert!(with_codex_home(&home, || tokens_by_model(&filter, window, None)).is_some());
    // A window a week away still prunes the directory away entirely.
    let far = Some((instant("2026-10-01T00:00:00Z"), instant("2026-10-01T01:00:00Z")));
    assert!(with_codex_home(&home, || discover_rollouts(None, far)).is_empty());
}

#[serial_test::serial(codex_home_env)]
#[test]
fn non_date_shaped_children_of_sessions_are_never_descended_into() {
    let tmp = tempfile::tempdir().unwrap();
    let home = seed_codex_home(tmp.path(), &[]);
    // A directory that is not `<YYYY>` — even holding a well-named rollout.
    let sneaky = home.join(SESSIONS_DIR).join("backup").join("09").join("20");
    std::fs::create_dir_all(&sneaky).unwrap();
    std::fs::write(sneaky.join("rollout-x.jsonl"), "{}").unwrap();
    assert!(with_codex_home(&home, || discover_rollouts(None, None)).is_empty());
}

#[serial_test::serial(codex_home_env)]
#[test]
fn codex_home_prefers_the_loom_override_then_codex_own_then_the_default() {
    clear_home_env();
    assert_eq!(codex_home(Some(Path::new("/h"))), Some(PathBuf::from("/h/.codex")));
    std::env::set_var(CODEX_NATIVE_HOME_ENV, "/native/codex");
    assert_eq!(codex_home(Some(Path::new("/h"))), Some(PathBuf::from("/native/codex")));
    std::env::set_var(CODEX_HOME_ENV, "/loom/codex");
    assert_eq!(codex_home(Some(Path::new("/h"))), Some(PathBuf::from("/loom/codex")));
    // A blank override is not an override.
    std::env::set_var(CODEX_HOME_ENV, "");
    assert_eq!(codex_home(Some(Path::new("/h"))), Some(PathBuf::from("/native/codex")));
}

// ---------------------------------------------------------------------------
// Env plumbing for the tests above
// ---------------------------------------------------------------------------

/// Run `f` with [`CODEX_HOME_ENV`] pinned to `home` and Codex's own override
/// cleared.
///
/// Both vars are process-global. The suite runs under `cargo nextest` (one
/// process per test — see `lib.rs`'s "Test isolation convention"), and every
/// caller additionally carries `#[serial_test::serial(codex_home_env)]` so a
/// plain `cargo test` run is safe too: the same discipline
/// `opencode_usage`'s tests apply to `LOOM_OPENCODE_DB`.
fn with_codex_home<T>(home: &Path, f: impl FnOnce() -> T) -> T {
    clear_home_env();
    std::env::set_var(CODEX_HOME_ENV, home);
    let out = f();
    clear_home_env();
    out
}

fn clear_home_env() {
    std::env::remove_var(CODEX_HOME_ENV);
    std::env::remove_var(CODEX_NATIVE_HOME_ENV);
}
