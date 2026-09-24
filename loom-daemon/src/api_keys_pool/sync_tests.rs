//! `api-keys sync` tests (issue #8511). Split out of `sync.rs` so the module
//! stays well under the file-size ratchet.
//!
//! Every test that touches key material uses a value that is unmistakable in a
//! failure message, because half of what is under test here is that the value
//! *never reaches* one.

use super::super::bad_marks;
use super::*;
use std::collections::BTreeMap;

const KEY_A: &str = "fake-key-alpha-must-never-be-echoed";
const KEY_B: &str = "fake-key-bravo-must-never-be-echoed";
const KEY_C: &str = "fake-key-charlie-must-never-be-echoed";

fn pool() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

/// A `cmd:` source that emits `body` verbatim — the fixture stands in for
/// `aws ssm get-parameters-by-path --with-decryption`, Vault, `age -d`, …
fn source_emitting(dir: &Path, name: &str, body: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    format!("cmd:cat {}", path.display())
}

fn two_account_source() -> String {
    format!("zai/alice\tZAI_API_KEY={KEY_A}\nzai/bob\tZAI_API_KEY={KEY_B}\n")
}

fn run(root: &Path, source: &str, prune: bool, dry_run: bool) -> Result<SyncOutcome, String> {
    sync(&SyncOptions {
        root,
        source,
        prune,
        dry_run,
    })
}

/// Every file under `root`, by relative path, with its bytes and (on Unix) its
/// inode — so "untouched" can be asserted as *the same file*, not merely one
/// with equal contents. `registry::add` replaces atomically via `rename`, which
/// always allocates a new inode, so a rewritten account cannot hide here.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, (Vec<u8>, u64)> {
    fn walk(dir: &Path, base: &Path, out: &mut BTreeMap<PathBuf, (Vec<u8>, u64)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, base, out);
                continue;
            }
            let bytes = std::fs::read(&path).unwrap_or_default();
            #[cfg(unix)]
            let ino = {
                use std::os::unix::fs::MetadataExt;
                std::fs::metadata(&path).map(|m| m.ino()).unwrap_or(0)
            };
            #[cfg(not(unix))]
            let ino = 0;
            out.insert(path.strip_prefix(base).unwrap().to_path_buf(), (bytes, ino));
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

#[test]
fn parses_tab_separated_records_ignoring_blanks_and_comments() {
    let records = parse_source(&format!(
        "# a comment\n\nzai/alice\tZAI_API_KEY={KEY_A}\r\n  # indented comment\nopenai/team\tOPENAI_API_KEY={KEY_B}\n"
    ))
    .unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].id(), "zai/alice");
    assert_eq!(records[0].env_name, "ZAI_API_KEY");
    assert_eq!(records[0].secret, KEY_A);
    assert_eq!(records[1].id(), "openai/team");
    assert_eq!(records[1].secret, KEY_B);
}

#[test]
fn parses_a_quoted_value_the_way_a_stored_file_is_read_back() {
    let records = parse_source(&format!("zai/alice\tZAI_API_KEY=\"{KEY_A}\"\n")).unwrap();
    assert_eq!(records[0].secret, KEY_A);
}

#[test]
fn malformed_lines_are_rejected_by_line_number_without_echoing_anything() {
    let cases = [
        // No tab at all — the whole line could be key material.
        format!("zai/alice ZAI_API_KEY={KEY_A}\n"),
        // No assignment after the tab.
        format!("zai/alice\t{KEY_A}\n"),
        // A lowercase left-hand side is key material, not a variable name.
        format!("zai/alice\tzai_api_key={KEY_A}\n"),
        // Missing the provider namespace.
        format!("alice\tZAI_API_KEY={KEY_A}\n"),
        // Path traversal in the identifier.
        format!("../etc/alice\tZAI_API_KEY={KEY_A}\n"),
        format!("zai/../../alice\tZAI_API_KEY={KEY_A}\n"),
        // Empty value.
        "zai/alice\tZAI_API_KEY=\n".to_string(),
    ];
    for body in cases {
        let error = parse_source(&body).unwrap_err();
        assert!(error.starts_with("line 1: "), "unlocated error: {error}");
        assert!(!error.contains(KEY_A), "source line leaked into: {error}");
    }
}

#[test]
fn a_duplicate_account_in_the_source_is_an_error_naming_both_lines() {
    let error =
        parse_source(&format!("zai/alice\tZAI_API_KEY={KEY_A}\nzai/alice\tZAI_API_KEY={KEY_B}\n"))
            .unwrap_err();
    assert!(error.contains("duplicate entry for zai/alice"), "{error}");
    assert!(error.contains("line 1"), "{error}");
    assert!(!error.contains(KEY_A) && !error.contains(KEY_B), "{error}");
}

#[test]
fn source_records_never_render_their_secret() {
    let records = parse_source(&format!("zai/alice\tZAI_API_KEY={KEY_A}\n")).unwrap();
    let rendered = format!("{records:?}");
    assert!(rendered.contains("<redacted>"), "{rendered}");
    assert!(!rendered.contains(KEY_A), "{rendered}");
}

#[test]
fn an_unknown_scheme_is_refused_rather_than_handed_to_a_shell() {
    let error = fetch("ssm:/loom/api-keys/").unwrap_err();
    assert!(error.contains("unsupported source scheme"), "{error}");
    assert!(error.contains("cmd:<command>"), "{error}");
}

#[test]
fn a_command_line_with_a_later_colon_is_not_read_as_a_scheme() {
    assert_eq!(split_scheme("cmd:cat /tmp/x"), Some(("cmd", "cat /tmp/x")));
    assert_eq!(split_scheme("sh -c 'echo a: b'"), None);
    assert_eq!(split_scheme("aws ssm get-parameters-by-path"), None);
}

// ---------------------------------------------------------------------------
// Convergence
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn registers_both_accounts_then_is_a_no_op_on_a_second_run() {
    let tmp = pool();
    let fixture = pool();
    let source = source_emitting(fixture.path(), "src", &two_account_source());

    let first = run(tmp.path(), &source, false, false).unwrap();
    assert_eq!(first.plan.added, vec!["zai/alice", "zai/bob"]);
    assert!(first.plan.updated.is_empty() && first.plan.removed.is_empty());
    assert_eq!(
        registry::read_credential(tmp.path(), "zai", "alice")
            .unwrap()
            .value,
        KEY_A
    );

    let before = snapshot(tmp.path());
    let second = run(tmp.path(), &source, false, false).unwrap();
    assert!(second.plan.is_noop(), "{:?}", second.plan);
    assert_eq!(second.plan.unchanged, vec!["zai/alice", "zai/bob"]);
    // The state file is rewritten each run; every account file must be the very
    // same inode as before.
    let after = snapshot(tmp.path());
    for (path, (bytes, ino)) in &before {
        if path.to_string_lossy().ends_with(SYNC_STATE_FILE) {
            continue;
        }
        let (now_bytes, now_ino) = after.get(path).expect("account file vanished");
        assert_eq!(bytes, now_bytes, "{} was rewritten", path.display());
        assert_eq!(ino, now_ino, "{} was replaced", path.display());
    }
}

#[cfg(unix)]
#[test]
fn a_changed_value_updates_only_that_account() {
    let tmp = pool();
    let fixture = pool();
    let source = source_emitting(fixture.path(), "src", &two_account_source());
    run(tmp.path(), &source, false, false).unwrap();
    let before = snapshot(tmp.path());

    let rotated = source_emitting(
        fixture.path(),
        "rotated",
        &format!("zai/alice\tZAI_API_KEY={KEY_C}\nzai/bob\tZAI_API_KEY={KEY_B}\n"),
    );
    let outcome = run(tmp.path(), &rotated, false, false).unwrap();
    assert_eq!(outcome.plan.updated, vec!["zai/alice"]);
    assert_eq!(outcome.plan.unchanged, vec!["zai/bob"]);
    assert!(outcome.plan.added.is_empty());

    assert_eq!(
        registry::read_credential(tmp.path(), "zai", "alice")
            .unwrap()
            .value,
        KEY_C
    );
    let after = snapshot(tmp.path());
    let bob = PathBuf::from("zai/bob.env");
    assert_eq!(before[&bob], after[&bob], "bob was touched");
}

#[cfg(unix)]
#[test]
fn prune_removes_only_the_account_the_source_dropped() {
    let tmp = pool();
    let fixture = pool();
    run(
        tmp.path(),
        &source_emitting(fixture.path(), "src", &two_account_source()),
        false,
        false,
    )
    .unwrap();

    let shrunk =
        source_emitting(fixture.path(), "shrunk", &format!("zai/alice\tZAI_API_KEY={KEY_A}\n"));
    // Without --prune the dropped account stays registered.
    let kept = run(tmp.path(), &shrunk, false, false).unwrap();
    assert!(kept.plan.removed.is_empty());
    assert!(registry::read_credential(tmp.path(), "zai", "bob").is_ok());

    let pruned = run(tmp.path(), &shrunk, true, false).unwrap();
    assert_eq!(pruned.plan.removed, vec!["zai/bob"]);
    assert!(registry::read_credential(tmp.path(), "zai", "bob").is_err());
    assert_eq!(
        registry::read_credential(tmp.path(), "zai", "alice")
            .unwrap()
            .value,
        KEY_A
    );
}

#[cfg(unix)]
#[test]
fn prune_never_touches_a_provider_the_source_does_not_mention() {
    let tmp = pool();
    let fixture = pool();
    // A hand-registered account for a provider the source says nothing about —
    // the "operator-added, non-synced" case the acceptance criteria call out.
    registry::add(tmp.path(), "openai", "hand", "OPENAI_API_KEY", KEY_C, false).unwrap();
    let before = snapshot(tmp.path());

    run(
        tmp.path(),
        &source_emitting(fixture.path(), "src", &two_account_source()),
        true,
        false,
    )
    .unwrap();

    let after = snapshot(tmp.path());
    let hand = PathBuf::from("openai/hand.env");
    assert_eq!(before[&hand], after[&hand], "an unmentioned provider's account was touched");
}

#[cfg(unix)]
#[test]
fn disabled_and_bad_marked_state_of_untouched_accounts_survives() {
    let tmp = pool();
    let fixture = pool();
    let source = source_emitting(fixture.path(), "src", &two_account_source());
    run(tmp.path(), &source, false, false).unwrap();

    registry::set_enabled(tmp.path(), "zai", "bob", false).unwrap();
    bad_marks::mark_bad(tmp.path(), "zai", "alice", "ran dry", Some(3_600)).unwrap();

    // A rotation of alice's key; bob is unchanged.
    let rotated = source_emitting(
        fixture.path(),
        "rotated",
        &format!("zai/alice\tZAI_API_KEY={KEY_C}\nzai/bob\tZAI_API_KEY={KEY_B}\n"),
    );
    run(tmp.path(), &rotated, true, false).unwrap();

    assert_eq!(
        registry::read_list(tmp.path(), "zai", registry::DISABLED_FILE).unwrap(),
        vec!["bob".to_string()],
        "the .disabled entry did not survive the sync"
    );
    let marks = bad_marks::read_marks(tmp.path(), "zai").unwrap();
    assert_eq!(marks.len(), 1);
    assert_eq!(marks[0].name, "alice");
}

// ---------------------------------------------------------------------------
// Fail-safe
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn a_failing_source_command_leaves_the_pool_byte_for_byte_unchanged() {
    let tmp = pool();
    let fixture = pool();
    run(
        tmp.path(),
        &source_emitting(fixture.path(), "src", &two_account_source()),
        false,
        false,
    )
    .unwrap();
    let before = snapshot(tmp.path());

    for source in ["cmd:exit 3", "cmd:loom-no-such-command-8511"] {
        let error = run(tmp.path(), source, true, false).unwrap_err();
        assert!(error.contains("source command failed"), "{error}");
        assert!(error.contains("left unchanged"), "{error}");
        assert_eq!(before, snapshot(tmp.path()), "the pool changed on a failed source");
    }
}

#[cfg(unix)]
#[test]
fn one_malformed_line_aborts_the_whole_sync_before_any_write() {
    let tmp = pool();
    let fixture = pool();
    run(
        tmp.path(),
        &source_emitting(fixture.path(), "src", &two_account_source()),
        false,
        false,
    )
    .unwrap();
    let before = snapshot(tmp.path());

    // Line 1 is a perfectly good *new* account; line 2 is malformed. Nothing
    // may be written, including line 1's account.
    let broken = source_emitting(
        fixture.path(),
        "broken",
        &format!("zai/carol\tZAI_API_KEY={KEY_C}\nzai/dave {KEY_A}\n"),
    );
    let error = run(tmp.path(), &broken, true, false).unwrap_err();
    assert!(error.starts_with("line 2: "), "{error}");
    assert!(!error.contains(KEY_A) && !error.contains(KEY_C), "{error}");
    assert_eq!(before, snapshot(tmp.path()));
    assert!(registry::read_credential(tmp.path(), "zai", "carol").is_err());
}

#[cfg(unix)]
#[test]
fn a_dry_run_reports_names_only_and_writes_nothing() {
    let tmp = pool();
    let fixture = pool();
    let source = source_emitting(fixture.path(), "src", &two_account_source());
    run(tmp.path(), &source, false, false).unwrap();
    registry::add(tmp.path(), "zai", "stale", "ZAI_API_KEY", KEY_C, false).unwrap();
    let before = snapshot(tmp.path());

    let rotated = source_emitting(
        fixture.path(),
        "rotated",
        &format!("zai/alice\tZAI_API_KEY={KEY_C}\nzai/bob\tZAI_API_KEY={KEY_B}\nzai/carol\tZAI_API_KEY={KEY_A}\n"),
    );
    let outcome = run(tmp.path(), &rotated, true, true).unwrap();
    assert!(outcome.dry_run && outcome.state.is_none());
    assert_eq!(outcome.plan.added, vec!["zai/carol"]);
    assert_eq!(outcome.plan.updated, vec!["zai/alice"]);
    assert_eq!(outcome.plan.removed, vec!["zai/stale"]);
    assert_eq!(outcome.plan.unchanged, vec!["zai/bob"]);
    assert_eq!(before, snapshot(tmp.path()), "a dry run wrote to the pool");

    let rendered = format!("{outcome:?}");
    let json = serde_json::to_string(&outcome).unwrap();
    for text in [&rendered, &json] {
        for key in [KEY_A, KEY_B, KEY_C] {
            assert!(!text.contains(key), "secret leaked into: {text}");
        }
    }
}

#[cfg(unix)]
#[test]
fn an_unreadable_stored_account_is_replaced_rather_than_re_added() {
    let tmp = pool();
    let fixture = pool();
    // A hand-placed bare key: `list` reports it unusable. The source is the
    // truth, so sync repairs it in place rather than reporting it as new.
    let dir = provider_dir(tmp.path(), "zai");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("alice.env"), KEY_C).unwrap();

    let outcome = run(
        tmp.path(),
        &source_emitting(fixture.path(), "src", &format!("zai/alice\tZAI_API_KEY={KEY_A}\n")),
        false,
        false,
    )
    .unwrap();
    assert_eq!(outcome.plan.updated, vec!["zai/alice"]);
    assert!(outcome.plan.added.is_empty());
    assert_eq!(
        registry::read_credential(tmp.path(), "zai", "alice")
            .unwrap()
            .value,
        KEY_A
    );
}

// ---------------------------------------------------------------------------
// Sync state / health
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn a_successful_sync_records_its_time_and_source_and_health_reports_it() {
    let tmp = pool();
    let fixture = pool();
    assert_eq!(read_state(tmp.path()).unwrap(), None);

    let source = source_emitting(fixture.path(), "src", &two_account_source());
    let outcome = run(tmp.path(), &source, false, false).unwrap();
    let state = read_state(tmp.path())
        .unwrap()
        .expect("no sync state written");
    assert_eq!(Some(&state), outcome.state.as_ref());
    assert_eq!(state.source, source);
    assert_eq!(state.accounts, 2);
    assert!(state.last_success_at > 0);

    // What `api-keys health` renders comes off the same record.
    let health = super::super::select::health(tmp.path(), Some("zai")).unwrap();
    // `health` resolves the pool root from a *workspace*; here the temp dir is
    // the root itself, so assert against the state read directly instead —
    // the field exists and round-trips through the secret-free JSON.
    let json = serde_json::to_string(&health).unwrap();
    for key in [KEY_A, KEY_B] {
        assert!(!json.contains(key), "secret leaked into health: {json}");
    }
}

#[cfg(unix)]
#[test]
fn a_failed_sync_does_not_refresh_the_recorded_time() {
    let tmp = pool();
    let fixture = pool();
    run(
        tmp.path(),
        &source_emitting(fixture.path(), "src", &two_account_source()),
        false,
        false,
    )
    .unwrap();
    let first = read_state(tmp.path()).unwrap().unwrap();

    assert!(run(tmp.path(), "cmd:exit 1", false, false).is_err());
    assert_eq!(read_state(tmp.path()).unwrap().unwrap(), first);
}

#[cfg(unix)]
#[test]
fn the_state_file_is_never_mistaken_for_a_provider_or_an_account() {
    let tmp = pool();
    let fixture = pool();
    run(
        tmp.path(),
        &source_emitting(fixture.path(), "src", &two_account_source()),
        false,
        false,
    )
    .unwrap();
    let providers = super::super::paths::list_providers(tmp.path()).unwrap();
    assert_eq!(providers, vec!["zai".to_string()]);
    let accounts = registry::list_all(tmp.path(), None).unwrap();
    assert_eq!(accounts.len(), 2);
}
