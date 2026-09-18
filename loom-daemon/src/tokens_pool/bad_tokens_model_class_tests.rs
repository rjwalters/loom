//! Per-model-class `.bad_tokens` marks (issue #8058) — the read/write rules
//! that let an account bad-marked for one model class keep serving another.
//!
//! Its own file rather than more lines in `bad_tokens_tests.rs`: that file is
//! already at `scripts/check-file-size-budget.sh`'s threshold, and this is a
//! self-contained behaviour with its own fixtures. `bad_tokens.rs` declares
//! both with `#[path]` module declarations, so `super::*` below resolves to
//! `bad_tokens` in either file.

use super::*;
use serial_test::serial;
use std::fs;

fn make_pool() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join(".loom").join("tokens");
    fs::create_dir_all(&dir).unwrap();
    // `resolve_tokens_dir()` only picks the per-repo pool when it holds at
    // least one `*.token` file — seed one so these tests deterministically
    // exercise the per-repo pool rather than this host's real shared pool.
    fs::write(dir.join("seed.token"), "sk-ant-oat01-fake").unwrap();
    tmp
}

fn pool_dir(ws: &Path) -> PathBuf {
    ws.join(".loom").join("tokens")
}

#[test]
fn model_class_of_resolves_aliases_and_pinned_ids() {
    assert_eq!(model_class_of("opus").as_deref(), Some("opus"));
    assert_eq!(model_class_of("claude-sonnet-4-6").as_deref(), Some("sonnet"));
    assert_eq!(model_class_of("claude-fable-5").as_deref(), Some("fable"));
    assert_eq!(model_class_of("claude-3-5-haiku").as_deref(), Some("haiku"));
    // Unrecognized / empty is None, never an error — the caller then
    // degrades to account-wide behaviour.
    assert_eq!(model_class_of("gpt-5"), None);
    assert_eq!(model_class_of(""), None);
    assert_eq!(model_class_of("   "), None);
}

#[test]
fn scoped_reason_round_trips_through_reason_model_class() {
    let scoped = scoped_reason("exhausted: out of usage credits", Some("opus"));
    assert_eq!(scoped, "exhausted: out of usage credits [model-class:opus]");
    assert_eq!(reason_model_class(&scoped), Some("opus"));

    // No class ⇒ the reason is returned byte-identical (back-compat: a
    // class-less mark must look exactly like a pre-#8058 one).
    assert_eq!(scoped_reason("exhausted: weekly limit", None), "exhausted: weekly limit");
    assert_eq!(scoped_reason("exhausted: weekly limit", Some("  ")), "exhausted: weekly limit");
    assert_eq!(reason_model_class("exhausted: weekly limit"), None);

    // A malformed marker reads as class-less — the fail-safe direction.
    assert_eq!(reason_model_class("exhausted: x [model-class:opus"), None);
    assert_eq!(reason_model_class("exhausted: x [model-class:]"), None);
}

/// The core #8058 rule: a class-less entry blocks EVERY class (and the
/// account-wide query), a class-scoped entry blocks only its own class.
#[test]
fn class_less_entry_blocks_every_class() {
    let tmp = make_pool();
    mark_bad(tmp.path(), "agent-1", "exhausted: weekly limit").unwrap();
    for class in [
        None,
        Some("opus"),
        Some("sonnet"),
        Some("haiku"),
        Some("fable"),
    ] {
        assert!(
            is_bad_for_class(tmp.path(), "agent-1", class),
            "class-less entry must block {class:?}"
        );
    }
}

#[test]
fn class_scoped_entry_blocks_only_its_own_class() {
    let tmp = make_pool();
    mark_bad_for_model(
        tmp.path(),
        "agent-1",
        "exhausted: out of usage credits",
        Some("claude-opus-5"),
    )
    .unwrap();

    assert!(
        is_bad_for_class(tmp.path(), "agent-1", Some("opus")),
        "an opus-scoped entry must block opus"
    );
    for other in ["sonnet", "haiku", "fable"] {
        assert!(
            !is_bad_for_class(tmp.path(), "agent-1", Some(other)),
            "an opus-scoped entry must NOT block {other}"
        );
    }
    // `is_bad`'s account-wide meaning is unchanged: the account IS marked
    // bad, and every caller outside the pool still sees that.
    assert!(
        is_bad(tmp.path(), "agent-1"),
        "is_bad must keep its account-wide meaning for a class-scoped entry"
    );
}

/// An unrecognized model can never produce a narrower mark than pre-#8058:
/// it falls back to a class-less (account-wide) entry.
#[test]
fn unrecognized_model_writes_a_class_less_entry() {
    let tmp = make_pool();
    mark_bad_for_model(tmp.path(), "agent-1", "exhausted: whatever", Some("gpt-5")).unwrap();
    let text = fs::read_to_string(pool_dir(tmp.path()).join(".bad_tokens")).unwrap();
    assert!(
        !text.contains(MODEL_CLASS_MARKER_PREFIX),
        "an unrecognized model must not write a class marker: {text}"
    );
    assert!(is_bad_for_class(tmp.path(), "agent-1", Some("sonnet")));
}

/// The `.bad_tokens` line format is unchanged — the marker rides inside
/// the existing free-form reason field, so a pre-#8058 reader parsing
/// `<ts> <name> <reason...>` still sees exactly three fields.
#[test]
fn class_marker_keeps_the_line_format_backward_compatible() {
    let tmp = make_pool();
    mark_bad_for_model(tmp.path(), "agent-1", "exhausted: credits", Some("opus")).unwrap();
    let text = fs::read_to_string(pool_dir(tmp.path()).join(".bad_tokens")).unwrap();
    let line = text.lines().next().unwrap();
    let mut parts = line.splitn(3, ' ');
    let ts = parts.next().unwrap();
    assert!(
        chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%SZ").is_ok(),
        "first field must still be the ISO-8601 timestamp: {line}"
    );
    assert_eq!(parts.next(), Some("agent-1"));
    assert_eq!(parts.next(), Some("exhausted: credits [model-class:opus]"));
    assert_eq!(text.lines().count(), 1, "still exactly one line per mark");
}

/// Auth wins over the class filter: a revoked credential is revoked for
/// every model class, even if something wrote a class marker onto it.
#[test]
fn auth_entry_blocks_every_class_even_when_class_marked() {
    let tmp = make_pool();
    mark_bad_for_model(tmp.path(), "agent-1", "auth-dead: 401 invalid bearer token", Some("opus"))
        .unwrap();
    for class in [None, Some("opus"), Some("sonnet")] {
        let entry = blocking_entry_for_class(tmp.path(), "agent-1", class).unwrap();
        assert_eq!(
            entry.class,
            BadReasonClass::Auth,
            "auth must win over the class filter for {class:?}"
        );
    }
}

/// A class-scoped exhaustion entry still expires on the ordinary
/// exhaustion cooldown — class scoping changes WHO it blocks, not how long.
#[test]
#[serial]
fn class_scoped_entry_expires_on_the_exhaustion_cooldown() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    let old = (Utc::now() - chrono::Duration::seconds(7 * 3600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(
        dir.join(".bad_tokens"),
        format!("{old} agent-1 exhausted: out of usage credits [model-class:opus]\n"),
    )
    .unwrap();
    assert!(
        !is_bad_for_class(tmp.path(), "agent-1", Some("opus")),
        "a 7h-old class-scoped exhaustion entry must have expired"
    );
    assert!(!is_bad(tmp.path(), "agent-1"));
}

/// A file mixing a class-scoped and a class-less line for the same account
/// blocks every class — the class-less line is the wider of the two and
/// always wins.
#[test]
#[serial]
fn mixed_class_less_and_class_scoped_lines_block_every_class() {
    let tmp = make_pool();
    let dir = pool_dir(tmp.path());
    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    fs::write(
        dir.join(".bad_tokens"),
        format!(
            "{now} agent-1 exhausted: out of usage credits [model-class:opus]\n\
             {now} agent-1 exhausted: hit your weekly limit\n"
        ),
    )
    .unwrap();
    for class in [None, Some("opus"), Some("sonnet"), Some("haiku")] {
        assert!(
            is_bad_for_class(tmp.path(), "agent-1", class),
            "a class-less line in the file must block {class:?}"
        );
    }
}

/// `claude-wrapper.sh` has no classifier of its own, so it writes the RAW
/// resolved model into the marker. The reader normalizes it, so
/// `[model-class:claude-opus-5]` and `[model-class:opus]` mean the same thing.
#[test]
fn a_raw_model_id_in_the_marker_is_normalized_on_read() {
    let tmp = make_pool();
    mark_bad(
        tmp.path(),
        "agent-1",
        "exhausted: out of usage credits [model-class:claude-opus-5]",
    )
    .unwrap();
    assert!(is_bad_for_class(tmp.path(), "agent-1", Some("opus")));
    assert!(!is_bad_for_class(tmp.path(), "agent-1", Some("sonnet")));
    assert!(is_bad(tmp.path(), "agent-1"));
}

/// A marker naming something the classifier cannot resolve reads as
/// CLASS-LESS — it blocks everything. The failure direction has to be
/// widening: a garbled marker must never be able to narrow an entry to
/// nothing and quietly readmit a dead account.
#[test]
fn an_unresolvable_marker_blocks_every_class() {
    let tmp = make_pool();
    mark_bad(tmp.path(), "agent-1", "exhausted: x [model-class:gpt-5]").unwrap();
    for class in [
        None,
        Some("opus"),
        Some("sonnet"),
        Some("haiku"),
        Some("fable"),
    ] {
        assert!(
            is_bad_for_class(tmp.path(), "agent-1", class),
            "an unresolvable marker must still block {class:?}"
        );
    }
}

/// The queried class is matched case-insensitively, and an unknown queried
/// class simply matches nothing class-scoped (it still sees class-less
/// lines).
#[test]
fn class_match_is_case_insensitive() {
    let tmp = make_pool();
    mark_bad(tmp.path(), "agent-1", "exhausted: credits [model-class:Opus]").unwrap();
    assert!(is_bad_for_class(tmp.path(), "agent-1", Some("opus")));
    assert!(!is_bad_for_class(tmp.path(), "agent-1", Some("sonnet")));
}
