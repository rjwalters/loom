//! Registry tests (issue #8401). Split out of `registry.rs` so the module
//! stays well under the file-size ratchet.

use super::*;

const FAKE_KEY: &str = "fake-key-must-never-be-echoed";

fn pool() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[test]
fn add_writes_a_0600_single_assignment_file() {
    let tmp = pool();
    let account = add(tmp.path(), "zai", "alpha", "ZAI_API_KEY", FAKE_KEY, false).unwrap();
    assert_eq!(account.name, "alpha");
    assert_eq!(account.env_name.as_deref(), Some("ZAI_API_KEY"));
    assert!(account.enabled && account.selectable() && account.permissions_ok);
    let body = fs::read_to_string(&account.path).unwrap();
    assert_eq!(body, format!("ZAI_API_KEY={FAKE_KEY}\n"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&account.path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "account file must be 0600, got {mode:o}");
    }
}

#[test]
fn add_refuses_to_clobber_without_force() {
    let tmp = pool();
    add(tmp.path(), "zai", "alpha", "ZAI_API_KEY", FAKE_KEY, false).unwrap();
    let err = add(tmp.path(), "zai", "alpha", "ZAI_API_KEY", "other", false).unwrap_err();
    assert!(err.contains("already exists"), "{err}");
    add(tmp.path(), "zai", "alpha", "ZAI_API_KEY", "other-key", true).unwrap();
    assert_eq!(read_credential(tmp.path(), "zai", "alpha").unwrap().value, "other-key");
}

#[test]
fn add_rejects_path_traversal_and_empty_or_multiline_keys() {
    let tmp = pool();
    assert!(add(tmp.path(), "../etc", "a", "K", FAKE_KEY, false).is_err());
    assert!(add(tmp.path(), "zai", "../../a", "K", FAKE_KEY, false).is_err());
    assert!(add(tmp.path(), "zai", "a", "ZAI_API_KEY", "   ", false).is_err());
    assert!(add(tmp.path(), "zai", "a", "ZAI_API_KEY", "one\ntwo", false).is_err());
    assert!(!tmp.path().join("../etc").exists());
}

#[test]
fn listing_and_errors_never_carry_key_material() {
    let tmp = pool();
    add(tmp.path(), "zai", "alpha", "ZAI_API_KEY", FAKE_KEY, false).unwrap();
    fs::write(provider_dir(tmp.path(), "zai").join("broken.env"), FAKE_KEY).unwrap();

    let accounts = list_all(tmp.path(), None).unwrap();
    let rendered = format!("{accounts:?}");
    let json = serde_json::to_string(&accounts).unwrap();
    for text in [&rendered, &json] {
        assert!(!text.contains(FAKE_KEY), "secret leaked into: {text}");
    }
    let broken = accounts.iter().find(|a| a.name == "broken").unwrap();
    assert_eq!(broken.ineligible, Some(Ineligible::Malformed));
    assert!(!broken.problem.as_deref().unwrap().contains(FAKE_KEY));
}

#[test]
fn credential_debug_redacts_the_value() {
    let tmp = pool();
    add(tmp.path(), "zai", "alpha", "ZAI_API_KEY", FAKE_KEY, false).unwrap();
    let credential = read_credential(tmp.path(), "zai", "alpha").unwrap();
    assert_eq!(credential.value, FAKE_KEY);
    let rendered = format!("{credential:?}");
    assert!(rendered.contains("<redacted>"), "{rendered}");
    assert!(!rendered.contains(FAKE_KEY), "{rendered}");
}

#[test]
fn disable_and_enable_flip_selectability_without_touching_the_key() {
    let tmp = pool();
    add(tmp.path(), "zai", "alpha", "ZAI_API_KEY", FAKE_KEY, false).unwrap();
    let disabled = set_enabled(tmp.path(), "zai", "alpha", false).unwrap();
    assert!(!disabled.enabled);
    assert_eq!(disabled.ineligible, Some(Ineligible::Disabled));
    assert_eq!(read_credential(tmp.path(), "zai", "alpha").unwrap().value, FAKE_KEY);
    let enabled = set_enabled(tmp.path(), "zai", "alpha", true).unwrap();
    assert!(enabled.enabled && enabled.selectable());
    // An empty control file is removed rather than left as an empty artifact.
    assert!(!provider_dir(tmp.path(), "zai").join(DISABLED_FILE).exists());
}

#[test]
fn enable_on_an_unknown_account_lists_what_is_registered() {
    let tmp = pool();
    add(tmp.path(), "zai", "alpha", "ZAI_API_KEY", FAKE_KEY, false).unwrap();
    let err = set_enabled(tmp.path(), "zai", "ghost", true).unwrap_err();
    assert!(err.contains("no such account zai/ghost"), "{err}");
    assert!(err.contains("alpha"), "{err}");
}

#[test]
fn remove_deletes_the_file_and_clears_its_disabled_entry() {
    let tmp = pool();
    add(tmp.path(), "zai", "alpha", "ZAI_API_KEY", FAKE_KEY, false).unwrap();
    add(tmp.path(), "zai", "beta", "ZAI_API_KEY", FAKE_KEY, false).unwrap();
    set_enabled(tmp.path(), "zai", "alpha", false).unwrap();
    remove(tmp.path(), "zai", "alpha").unwrap();
    assert!(!provider_dir(tmp.path(), "zai").join("alpha.env").exists());
    assert!(read_list(tmp.path(), "zai", DISABLED_FILE)
        .unwrap()
        .is_empty());
    assert!(remove(tmp.path(), "zai", "alpha").is_err());
    assert_eq!(list_provider(tmp.path(), "zai").unwrap().len(), 1);
}

#[test]
fn provider_is_pooled_only_once_an_account_exists() {
    let tmp = pool();
    assert!(!provider_is_pooled(tmp.path(), "zai").unwrap());
    fs::create_dir_all(provider_dir(tmp.path(), "zai")).unwrap();
    assert!(!provider_is_pooled(tmp.path(), "zai").unwrap(), "an empty dir is not a pool");
    add(tmp.path(), "zai", "alpha", "ZAI_API_KEY", FAKE_KEY, false).unwrap();
    assert!(provider_is_pooled(tmp.path(), "zai").unwrap());
}

#[test]
fn parse_entry_accepts_comments_exports_and_quotes_but_not_two_keys() {
    let one = parse_entry("# comment\n\nexport ZAI_API_KEY=\"quoted-value\"\n").unwrap();
    assert_eq!(one.env_name, "ZAI_API_KEY");
    assert_eq!(one.value, "quoted-value");
    assert_eq!(parse_entry("K='single'").unwrap().value, "single");
    assert!(parse_entry("A=1\nB=2")
        .unwrap_err()
        .contains("more than one"));
    assert!(parse_entry("K=").unwrap_err().contains("empty value"));
    assert!(parse_entry("# only a comment").is_err());
    assert!(parse_entry("not-an-assignment").is_err());
}

#[test]
fn list_all_spans_providers_and_can_be_filtered() {
    let tmp = pool();
    add(tmp.path(), "zai", "alpha", "ZAI_API_KEY", FAKE_KEY, false).unwrap();
    add(tmp.path(), "openai", "work", "OPENAI_API_KEY", FAKE_KEY, false).unwrap();
    let all = list_all(tmp.path(), None).unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].provider, "openai", "providers are listed in sorted order");
    assert_eq!(list_all(tmp.path(), Some("zai")).unwrap().len(), 1);
    assert!(list_all(tmp.path(), Some("absent")).unwrap().is_empty());
}

// =============================================================================
// Regression tests for the PR #8428 review. Each encodes a reproduction from
// the Judge's verdict and fails against the pre-fix code.
// =============================================================================

/// `chmod` for the duration of a test, restored on drop so `tempfile` can
/// always clean up.
#[cfg(unix)]
struct ModeGuard(PathBuf);

#[cfg(unix)]
impl ModeGuard {
    fn set(path: &Path, mode: u32) -> Self {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
        Self(path.to_path_buf())
    }
}

#[cfg(unix)]
impl Drop for ModeGuard {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700));
    }
}

/// Finding 1: an unreadable provider directory is an error, never "unpooled"
/// and never an empty listing.
#[cfg(unix)]
#[test]
fn an_unreadable_provider_dir_is_not_reported_as_unpooled() {
    let tmp = pool();
    add(tmp.path(), "zai", "alpha", "ZAI_API_KEY", FAKE_KEY, false).unwrap();
    set_enabled(tmp.path(), "zai", "alpha", false).unwrap();
    let dir = provider_dir(tmp.path(), "zai");
    let _guard = ModeGuard::set(&dir, 0o000);
    if fs::read_dir(&dir).is_ok() {
        return; // running as root: permission bits do not apply
    }
    assert!(provider_is_pooled(tmp.path(), "zai").is_err());
    assert!(list_provider(tmp.path(), "zai").is_err());
    assert!(list_all(tmp.path(), None).is_err());
    let err = set_enabled(tmp.path(), "zai", "alpha", true).unwrap_err();
    assert!(err.contains("cannot be read"), "{err}");
}

/// Finding 4, verbatim from the verdict: a hand-placed bare base64 key with
/// `=` padding used to parse as *name = the key*, and `list` printed
/// `variable=RkFLRUtFWW1hdGVyaWFsMDAwNQ` for a `selectable` account.
#[test]
fn a_bare_key_with_padding_is_never_echoed_as_a_variable_name() {
    const BARE: &str = "RkFLRUtFWW1hdGVyaWFsMDAwNQ==";
    const STEM: &str = "RkFLRUtFWW1hdGVyaWFsMDAwNQ";
    // Base32 is all-uppercase, so the case rule alone does not catch it; the
    // "value is only padding" rule does.
    const BASE32: &str = "IZAUWRKLIVMW2YLUMVZGSYLM====";
    let tmp = pool();
    let dir = provider_dir(tmp.path(), "zai");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("handmade.env"), format!("{BARE}\n")).unwrap();
    fs::write(dir.join("handmade32.env"), format!("{BASE32}\n")).unwrap();

    let accounts = list_all(tmp.path(), None).unwrap();
    assert_eq!(accounts.len(), 2);
    for account in &accounts {
        assert_eq!(account.ineligible, Some(Ineligible::Malformed), "{account:?}");
        assert_eq!(account.env_name, None, "{account:?}");
    }
    let rendered = format!("{accounts:?}{}", serde_json::to_string(&accounts).unwrap());
    for secret in [BARE, STEM, BASE32, BASE32.trim_end_matches('=')] {
        assert!(!rendered.contains(secret), "key material echoed: {rendered}");
    }
    for name in ["handmade", "handmade32"] {
        let err = read_credential(tmp.path(), "zai", name).unwrap_err();
        assert!(!err.contains(STEM) && !err.contains("IZAUWRKL"), "{err}");
    }
}

#[test]
fn add_refuses_a_variable_name_it_could_not_read_back() {
    let tmp = pool();
    let err = add(tmp.path(), "zai", "alpha", "zai_api_key", FAKE_KEY, false).unwrap_err();
    assert!(err.contains("UPPER_SNAKE_CASE"), "{err}");
    assert!(!err.contains("zai_api_key"), "{err}");
    assert!(!provider_dir(tmp.path(), "zai").join("alpha.env").exists());
}

/// Finding 2(a): `add --force` replaces by `rename`; a reader can never see a
/// truncated account file, and no staging file is left behind.
#[cfg(unix)]
#[test]
fn write_secret_replaces_atomically_and_stays_0600() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let tmp = pool();
    let account = add(tmp.path(), "zai", "alpha", "ZAI_API_KEY", FAKE_KEY, false).unwrap();
    let before = fs::metadata(&account.path).unwrap().ino();
    add(tmp.path(), "zai", "alpha", "ZAI_API_KEY", "fake-replacement-key", true).unwrap();
    let after = fs::metadata(&account.path).unwrap();
    assert_ne!(after.ino(), before, "account file was rewritten in place");
    assert_eq!(after.permissions().mode() & 0o777, 0o600);
    let names: Vec<String> = fs::read_dir(provider_dir(tmp.path(), "zai"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(names.iter().all(|n| !n.contains(".tmp-")), "{names:?}");
}

/// A failed write must leave the previous contents intact (the `ENOSPC` /
/// crash half of finding 2). Forced here with a read-only directory.
#[cfg(unix)]
#[test]
fn a_failed_write_leaves_the_previous_file_intact() {
    let tmp = pool();
    add(tmp.path(), "zai", "alpha", "ZAI_API_KEY", FAKE_KEY, false).unwrap();
    let dir = provider_dir(tmp.path(), "zai");
    let path = dir.join("alpha.env");
    let _guard = ModeGuard::set(&dir, 0o500);
    if fs::write(dir.join("probe"), "x").is_ok() {
        return; // running as root
    }
    assert!(write_secret(&path, "ZAI_API_KEY=fake-never-lands\n").is_err());
    assert_eq!(fs::read_to_string(&path).unwrap(), format!("ZAI_API_KEY={FAKE_KEY}\n"));
}

/// Finding 2(c): `.disabled` gets the same atomic `0600` write, and a
/// `.disabled` that exists but cannot be read withholds the account instead of
/// re-enabling it.
#[cfg(unix)]
#[test]
fn an_unreadable_disabled_list_withholds_accounts_rather_than_enabling_them() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = pool();
    add(tmp.path(), "zai", "alpha", "ZAI_API_KEY", FAKE_KEY, false).unwrap();
    add(tmp.path(), "zai", "beta", "ZAI_API_KEY", FAKE_KEY, false).unwrap();
    set_enabled(tmp.path(), "zai", "alpha", false).unwrap();
    let disabled = provider_dir(tmp.path(), "zai").join(DISABLED_FILE);
    assert_eq!(fs::metadata(&disabled).unwrap().permissions().mode() & 0o777, 0o600);

    let _guard = ModeGuard::set(&disabled, 0o000);
    if fs::read_to_string(&disabled).is_ok() {
        return; // running as root
    }
    for account in list_provider(tmp.path(), "zai").unwrap() {
        assert_eq!(account.ineligible, Some(Ineligible::Unverifiable), "{account:?}");
        assert!(!account.selectable());
    }
    // And a writer must not rebuild the list from an empty read.
    assert!(set_enabled(tmp.path(), "zai", "beta", false).is_err());
}

/// An unparsable marks file withholds every account of the provider (finding
/// 2(b), seen from the registry's side).
#[test]
fn an_unparsable_marks_file_withholds_accounts() {
    let tmp = pool();
    add(tmp.path(), "zai", "alpha", "ZAI_API_KEY", FAKE_KEY, false).unwrap();
    bad_marks::mark_bad(tmp.path(), "zai", "alpha", "simulated", Some(3600)).unwrap();
    fs::write(provider_dir(tmp.path(), "zai").join(bad_marks::BAD_MARKS_FILE), "").unwrap();
    let account = &list_provider(tmp.path(), "zai").unwrap()[0];
    assert_eq!(account.ineligible, Some(Ineligible::Unverifiable));
    assert!(account
        .problem
        .as_deref()
        .unwrap()
        .contains("not a valid bad-marks file"));
}

/// Judge nit: `remove` left the bad mark and `.allowlist` entry behind, so an
/// account re-registered under the same name inherited them.
#[test]
fn remove_drops_every_piece_of_per_account_state() {
    let tmp = pool();
    add(tmp.path(), "zai", "alpha", "ZAI_API_KEY", FAKE_KEY, false).unwrap();
    add(tmp.path(), "zai", "beta", "ZAI_API_KEY", FAKE_KEY, false).unwrap();
    bad_marks::mark_bad(tmp.path(), "zai", "alpha", "simulated", None).unwrap();
    bad_marks::mark_bad(tmp.path(), "zai", "beta", "simulated", None).unwrap();
    fs::write(provider_dir(tmp.path(), "zai").join(ALLOWLIST_FILE), "alpha\nbeta\n").unwrap();
    remove(tmp.path(), "zai", "alpha").unwrap();
    assert_eq!(read_list(tmp.path(), "zai", ALLOWLIST_FILE).unwrap(), vec!["beta"]);
    let marks = bad_marks::read_marks(tmp.path(), "zai").unwrap();
    assert_eq!(marks.len(), 1);
    assert_eq!(marks[0].name, "beta", "the other account's mark must survive");
    let again = add(tmp.path(), "zai", "alpha", "ZAI_API_KEY", FAKE_KEY, false).unwrap();
    assert!(again.selectable(), "{again:?}");
}
