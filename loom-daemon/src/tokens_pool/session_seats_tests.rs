use super::*;
use crate::tokens_pool::account_registry::{AccountId, CredentialKind, InventoryProvenance};

fn account(profile: &str, name: &str, enabled: bool) -> AccountDescriptor {
    AccountDescriptor {
        id: AccountId {
            provider: AccountProvider::Codex,
            name: name.to_string(),
        },
        credential_kind: CredentialKind::CodexHome,
        credential_reference: PathBuf::from(profile),
        enabled,
        provenance: InventoryProvenance::Shared,
        email: None,
    }
}

/// Profiles under `/managed/` are session-managed.
fn managed(profile: &Path) -> bool {
    profile.starts_with("/managed")
}

#[test]
fn a_seat_listed_only_by_another_root_is_still_a_seat() {
    // The daemon's own root has no session-managed account (#10600 gap a).
    let daemon_root = [account("/plain/a", "a", true)];
    let other_root = [account("/managed/b", "b", true)];
    let seats = seats_from(&[&daemon_root, &other_root], &managed);
    assert_eq!(seats.len(), 1);
    assert_eq!(seats[0].account, "b");
    assert_eq!(seats[0].container, "loom-codex-session-b");
}

#[test]
fn disabled_in_any_root_is_no_seat() {
    let one = [account("/managed/one/b", "b", true)];
    let two = [account("/managed/two/b", "b", false)];
    assert!(seats_from(&[&one, &two], &managed).is_empty());
}

#[test]
fn one_seat_per_account_with_every_profile() {
    let one = [account("/managed/one/b", "b", true)];
    let two = [
        account("/plain/two/b", "b", true),
        account("/managed/two/c", "c", true),
    ];
    let seats = seats_from(&[&one, &two], &managed);
    assert_eq!(seats.len(), 2);
    assert_eq!(
        seats[0].profiles,
        vec![
            PathBuf::from("/managed/one/b"),
            PathBuf::from("/plain/two/b")
        ],
        "a hold or removal record in any root's profile applies"
    );
    assert_eq!(seats[1].account, "c");
}

#[test]
fn a_disabled_root_entry_alone_is_not_a_seat() {
    let one = [account("/managed/b", "b", false)];
    assert!(seats_from(&[&one], &managed).is_empty());
}
