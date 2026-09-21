//! Tests for [`super`] — the API-key pool's bad-marking store (#8401) and its
//! model-class scoping (#8424 item 3). Split into its own file the way
//! `registry_tests.rs` / `select_tests.rs` are.

use super::*;

/// A pool root with `names` registered under `zai` (obviously fake keys).
fn pool(names: &[&str]) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    for name in names {
        crate::api_keys_pool::registry::add(
            tmp.path(),
            "zai",
            name,
            "ZAI_API_KEY",
            "fake-key-not-a-real-credential",
            false,
        )
        .unwrap();
    }
    tmp
}

fn active(root: &Path, provider: &str, name: &str, now: u64) -> Option<BadMark> {
    active_mark(root, provider, name, now).unwrap()
}

fn active_for(
    root: &Path,
    provider: &str,
    name: &str,
    class: Option<&str>,
    now: u64,
) -> Option<BadMark> {
    active_mark_for_class(root, provider, name, class, now).unwrap()
}

#[test]
fn mark_bad_then_active_mark_reports_it_until_the_horizon() {
    let tmp = pool(&["alpha"]);
    let mark =
        mark_bad(tmp.path(), "zai", "alpha", "quota exceeded\nretry later", Some(3600)).unwrap();
    assert_eq!(mark.reason, "quota exceeded retry later");
    assert_eq!(mark.model_class, None);
    let now = mark.marked_at;
    assert!(active(tmp.path(), "zai", "alpha", now).is_some());
    assert!(active(tmp.path(), "zai", "alpha", now + 3599).is_some());
    assert!(active(tmp.path(), "zai", "alpha", now + 3601).is_none());
}

#[test]
fn a_permanent_mark_never_expires_until_unmark() {
    let tmp = pool(&["alpha"]);
    mark_bad(tmp.path(), "zai", "alpha", "permanent", None).unwrap();
    assert!(active(tmp.path(), "zai", "alpha", epoch_now() + 10_000_000).is_some());
    unmark(tmp.path(), "zai", "alpha").unwrap();
    assert!(active(tmp.path(), "zai", "alpha", epoch_now()).is_none());
}

#[test]
fn zero_cooldown_is_rejected() {
    let tmp = pool(&["alpha"]);
    let err = mark_bad(tmp.path(), "zai", "alpha", "x", Some(0)).unwrap_err();
    assert!(err.contains("unblock"), "{err}");
}

#[test]
fn unmark_on_an_unmarked_account_is_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    let err = unmark(tmp.path(), "zai", "ghost").unwrap_err();
    assert!(err.contains("no bad mark recorded"), "{err}");
}

#[test]
fn re_marking_replaces_the_previous_entry_rather_than_appending() {
    let tmp = pool(&["alpha"]);
    mark_bad(tmp.path(), "zai", "alpha", "first", Some(10)).unwrap();
    mark_bad(tmp.path(), "zai", "alpha", "second", Some(20)).unwrap();
    let marks = read_marks(tmp.path(), "zai").unwrap();
    assert_eq!(marks.len(), 1);
    assert_eq!(marks[0].reason, "second");
}

#[test]
fn marks_are_scoped_per_provider() {
    let tmp = pool(&["alpha"]);
    mark_bad(tmp.path(), "zai", "alpha", "x", Some(10)).unwrap();
    assert!(active(tmp.path(), "openai", "alpha", epoch_now()).is_none());
}

/// Judge nit (#8428): marking a name that is not registered used to
/// succeed and `mkdir` the provider directory as a side effect.
#[test]
fn marking_an_unregistered_account_is_refused_and_creates_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let err = mark_bad(tmp.path(), "zai", "ghost", "x", Some(10)).unwrap_err();
    assert!(err.contains("no such account zai/ghost"), "{err}");
    assert!(!provider_dir(tmp.path(), "zai").exists());
}

/// Judge finding 2 (#8428). Each body is a state the Judge reproduced:
/// zero-length (what a reader saw between `O_TRUNC` and `write_all`, or
/// what `ENOSPC` leaves) and a torn JSON body. Before the fix both read as
/// "no marks" (`unwrap_or_default`), and the follow-up `mark_bad` rewrote
/// the file holding only `beta` — `alpha`'s mark was gone for good.
#[test]
fn an_unparsable_marks_file_fails_closed_and_is_never_clobbered() {
    for torn in ["", "[{\"name\":\"alpha\",\"reason\":\"quota", "{}"] {
        let tmp = pool(&["alpha", "beta"]);
        mark_bad(tmp.path(), "zai", "alpha", "secret-ish reason text", Some(3600)).unwrap();
        let path = marks_path(tmp.path(), "zai");
        std::fs::write(&path, torn).unwrap();

        let err = read_marks(tmp.path(), "zai").unwrap_err();
        assert!(err.contains(&path.display().to_string()), "{err}");
        assert!(!err.contains("secret-ish"), "parse error echoed contents: {err}");
        assert!(active_mark(tmp.path(), "zai", "alpha", epoch_now()).is_err());
        // #8424: the class-scoped read fails closed the same way — an
        // unreadable store must never narrow to "this class is fine".
        assert!(is_bad_for_class(tmp.path(), "zai", "alpha", Some("glm-5"), epoch_now()).is_err());

        // Neither writer may rewrite it from an empty read.
        assert!(mark_bad(tmp.path(), "zai", "beta", "x", Some(10)).is_err());
        assert!(unmark(tmp.path(), "zai", "alpha").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), torn, "file was clobbered");
    }
}

#[test]
fn an_absent_marks_file_is_simply_no_marks() {
    let tmp = pool(&["alpha"]);
    assert_eq!(read_marks(tmp.path(), "zai").unwrap(), Vec::new());
    assert_eq!(active_mark(tmp.path(), "zai", "alpha", epoch_now()), Ok(None));
}

/// The write itself: replaced by `rename`, never truncated in place, and no
/// staging file is left behind.
#[test]
fn marks_are_written_atomically_and_leave_no_temp_file() {
    let tmp = pool(&["alpha", "beta"]);
    mark_bad(tmp.path(), "zai", "alpha", "first", Some(3600)).unwrap();
    let path = marks_path(tmp.path(), "zai");
    #[cfg(unix)]
    let inode_before = {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(&path).unwrap().ino()
    };
    mark_bad(tmp.path(), "zai", "beta", "second", Some(3600)).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let metadata = std::fs::metadata(&path).unwrap();
        assert_ne!(metadata.ino(), inode_before, "marks file was rewritten in place");
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    }
    assert_eq!(read_marks(tmp.path(), "zai").unwrap().len(), 2);
    let stranded: Vec<_> = std::fs::read_dir(provider_dir(tmp.path(), "zai"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
        .collect();
    assert!(stranded.is_empty(), "{stranded:?}");
}

// ---- Model-class scoping (#8424 item 3) ----

/// The acceptance criterion: marking one model class bad leaves a different
/// model class on the same account selectable.
#[test]
fn a_class_scoped_mark_blocks_only_that_class() {
    let tmp = pool(&["alpha"]);
    let mark = mark_bad_for_class(
        tmp.path(),
        "zai",
        "alpha",
        "flash allowance exhausted",
        Some(3600),
        Some("glm-5.3-flash"),
    )
    .unwrap();
    assert_eq!(mark.model_class.as_deref(), Some("glm-5.3-flash"));
    let now = mark.marked_at;

    // The marked class is blocked…
    assert!(active_for(tmp.path(), "zai", "alpha", Some("glm-5.3-flash"), now).is_some());
    // …a sibling class on the SAME account is not.
    assert!(active_for(tmp.path(), "zai", "alpha", Some("glm-5"), now).is_none());
    assert!(!is_bad_for_class(tmp.path(), "zai", "alpha", Some("glm-5"), now).unwrap());
    // …and the effort suffix is not a different class.
    assert!(active_for(tmp.path(), "zai", "alpha", Some("glm-5.3-flash#high"), now).is_some());
    // The account-wide question still answers "yes, marked" — the whole
    // account is not healthy, and `is_bad`'s callers mean exactly that.
    assert!(active(tmp.path(), "zai", "alpha", now).is_some());
    // The horizon still applies per mark.
    assert!(active_for(tmp.path(), "zai", "alpha", Some("glm-5.3-flash"), now + 3601).is_none());
}

/// Backward compatibility: a class-less mark is account-wide, blocking every
/// class — nothing about #8424 narrows a pre-#8424 mark.
#[test]
fn a_class_less_mark_blocks_every_class() {
    let tmp = pool(&["alpha"]);
    mark_bad(tmp.path(), "zai", "alpha", "plan exhausted", Some(3600)).unwrap();
    let now = epoch_now();
    for class in [
        None,
        Some("glm-5"),
        Some("glm-5.3-flash"),
        Some("anything-else"),
    ] {
        assert!(
            is_bad_for_class(tmp.path(), "zai", "alpha", class, now).unwrap(),
            "{class:?} escaped an account-wide mark"
        );
    }
}

/// A mark file written before this field existed still parses, and still
/// blocks account-wide.
#[test]
fn a_pre_8424_marks_file_without_the_field_still_parses_as_account_wide() {
    let tmp = pool(&["alpha"]);
    std::fs::write(
        marks_path(tmp.path(), "zai"),
        r#"[{"name":"alpha","reason":"legacy","markedAt":1,"resetsAt":null}]"#,
    )
    .unwrap();
    let marks = read_marks(tmp.path(), "zai").unwrap();
    assert_eq!(marks.len(), 1);
    assert_eq!(marks[0].model_class, None);
    assert!(is_bad_for_class(tmp.path(), "zai", "alpha", Some("glm-5"), epoch_now()).unwrap());
}

/// And a class-less mark still serialises without the new key, so the file
/// format is unchanged for every existing deployment.
#[test]
fn a_class_less_mark_serialises_without_the_new_field() {
    let tmp = pool(&["alpha"]);
    mark_bad(tmp.path(), "zai", "alpha", "x", Some(10)).unwrap();
    let body = std::fs::read_to_string(marks_path(tmp.path(), "zai")).unwrap();
    assert!(!body.contains("modelClass"), "{body}");
    mark_bad_for_class(tmp.path(), "zai", "alpha", "x", Some(10), Some("glm-5")).unwrap();
    let body = std::fs::read_to_string(marks_path(tmp.path(), "zai")).unwrap();
    assert!(body.contains("\"modelClass\": \"glm-5\""), "{body}");
}

/// Each class-scoped mark is its own entry with its own horizon, and an
/// account-wide mark coexists with them rather than replacing them.
#[test]
fn class_scoped_marks_coexist_and_age_out_independently() {
    let tmp = pool(&["alpha"]);
    mark_bad_for_class(tmp.path(), "zai", "alpha", "flash", Some(60), Some("glm-5.3-flash"))
        .unwrap();
    mark_bad_for_class(tmp.path(), "zai", "alpha", "pro", Some(7200), Some("glm-5")).unwrap();
    mark_bad(tmp.path(), "zai", "alpha", "account wide", Some(30)).unwrap();
    let marks = read_marks(tmp.path(), "zai").unwrap();
    assert_eq!(marks.len(), 3, "{marks:?}");

    let now = epoch_now();
    // After the account-wide and flash horizons pass, only `glm-5` is blocked.
    let later = now + 120;
    assert!(!is_bad_for_class(tmp.path(), "zai", "alpha", Some("glm-5.3-flash"), later).unwrap());
    assert!(is_bad_for_class(tmp.path(), "zai", "alpha", Some("glm-5"), later).unwrap());
}

/// An account-wide mark is the stronger statement, so a class-scoped read
/// surfaces it (it is what an operator has to clear) rather than the class's.
#[test]
fn an_account_wide_mark_is_reported_ahead_of_a_class_scoped_one() {
    let tmp = pool(&["alpha"]);
    mark_bad_for_class(tmp.path(), "zai", "alpha", "flash only", Some(3600), Some("glm-5"))
        .unwrap();
    mark_bad(tmp.path(), "zai", "alpha", "whole account", Some(3600)).unwrap();
    let mark = active_for(tmp.path(), "zai", "alpha", Some("glm-5"), epoch_now()).unwrap();
    assert_eq!(mark.reason, "whole account");
    assert_eq!(mark.model_class, None);
    assert_eq!(mark.class_suffix(), "");
}

/// Unblocking one class leaves the other marks alone; unblocking with no
/// class clears everything.
#[test]
fn unmark_narrows_by_class_and_clears_everything_without_one() {
    let tmp = pool(&["alpha"]);
    mark_bad_for_class(tmp.path(), "zai", "alpha", "flash", Some(3600), Some("glm-5.3-flash"))
        .unwrap();
    mark_bad_for_class(tmp.path(), "zai", "alpha", "pro", Some(3600), Some("glm-5")).unwrap();

    unmark_for_class(tmp.path(), "zai", "alpha", Some("glm-5.3-flash")).unwrap();
    let marks = read_marks(tmp.path(), "zai").unwrap();
    assert_eq!(marks.len(), 1);
    assert_eq!(marks[0].model_class.as_deref(), Some("glm-5"));

    // Clearing a class with no mark is an error naming the class.
    let err = unmark_for_class(tmp.path(), "zai", "alpha", Some("glm-5.3-flash")).unwrap_err();
    assert!(err.contains("glm-5.3-flash"), "{err}");

    unmark(tmp.path(), "zai", "alpha").unwrap();
    assert_eq!(read_marks(tmp.path(), "zai").unwrap(), Vec::new());
}

/// `remove` still forgets every mark for the account, class-scoped included,
/// so a name re-registered later does not inherit a stale class mark.
#[test]
fn removing_an_account_forgets_its_class_scoped_marks_too() {
    let tmp = pool(&["alpha"]);
    mark_bad_for_class(tmp.path(), "zai", "alpha", "flash", Some(3600), Some("glm-5.3-flash"))
        .unwrap();
    crate::api_keys_pool::registry::remove(tmp.path(), "zai", "alpha").unwrap();
    assert_eq!(read_marks(tmp.path(), "zai").unwrap(), Vec::new());
}

/// A class the caller cannot have meant is refused, never silently widened
/// into an account-wide mark.
#[test]
fn an_unusable_model_class_is_refused_rather_than_widened() {
    let tmp = pool(&["alpha"]);
    for junk in ["", "   ", "glm 5 flash", "\"glm-5\"", &"g".repeat(80)] {
        let err =
            mark_bad_for_class(tmp.path(), "zai", "alpha", "x", Some(10), Some(junk)).unwrap_err();
        assert!(err.contains("model class"), "{junk:?}: {err}");
    }
    assert_eq!(read_marks(tmp.path(), "zai").unwrap(), Vec::new());
}

#[test]
fn normalize_model_class_lowercases_trims_and_drops_the_effort_suffix() {
    assert_eq!(normalize_model_class(" GLM-5.3-Flash "), Some("glm-5.3-flash".to_string()));
    assert_eq!(normalize_model_class("glm-5.3-flash#high"), Some("glm-5.3-flash".to_string()));
    assert_eq!(
        normalize_model_class("anthropic/claude-4"),
        Some("anthropic/claude-4".to_string())
    );
    assert_eq!(normalize_model_class(""), None);
    assert_eq!(normalize_model_class("#high"), None);
    assert_eq!(normalize_model_class("has space"), None);
    assert_eq!(normalize_model_class("tab\tsep"), None);
}
