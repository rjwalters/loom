use super::*;

#[test]
fn zai_seats_carry_the_key_in_process_but_rows_never_do() {
    use std::os::unix::fs::PermissionsExt;
    let workspace = tempfile::tempdir().unwrap();
    let dir = workspace.path().join(".loom/api-keys/zai");
    std::fs::create_dir_all(&dir).unwrap();
    let secret = "sk-test-secret-value-1234567890";
    let file = dir.join("seat-a.env");
    std::fs::write(&file, format!("ZAI_API_KEY={secret}\n")).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();

    let seats = zai_seats(workspace.path());
    assert_eq!(seats.len(), 1);
    assert_eq!(seats[0].name, "seat-a");
    assert_eq!(seats[0].key.as_deref(), Some(secret));

    // The row built for it names the seat and nothing secret.
    let row = zai::account_state("seat-a", None, false, None);
    let rendered = format!("{row:?}");
    assert!(!rendered.contains(secret));
    assert_eq!(row.usage_fraction, None);
}

#[test]
fn no_zai_seats_when_unprovisioned() {
    let workspace = tempfile::tempdir().unwrap();
    assert!(zai_seats(workspace.path()).is_empty());
}

#[tokio::test]
async fn unreadable_zai_credential_still_yields_an_unknown_row() {
    use std::os::unix::fs::PermissionsExt;
    let workspace = tempfile::tempdir().unwrap();
    let dir = workspace.path().join(".loom/api-keys/zai");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("seat-bad.env");
    std::fs::write(&file, "ZAI_API_KEY=sk-test-unused\n").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();

    // An enabled, well-formed seat whose credential read then fails.
    let seats = zai_seats_with(workspace.path(), |_, _, _| Err("unreadable".to_string()));
    let seat = seats.iter().find(|s| s.name == "seat-bad");
    let seat = seat.expect("unreadable seat must not be dropped");
    assert!(seat.key.is_none());

    let rows = zai::sample(seats).await;
    let row = rows.iter().find(|r| r.account == "seat-bad").unwrap();
    assert_eq!(row.usage_fraction, None);
    assert_eq!(row.usage_fraction_weekly, None);
    assert!(!row.exhausted);
}

#[tokio::test]
async fn malformed_zai_seat_is_kept_as_an_unknown_row() {
    use std::os::unix::fs::PermissionsExt;
    let workspace = tempfile::tempdir().unwrap();
    let dir = workspace.path().join(".loom/api-keys/zai");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("seat-garbled.env");
    std::fs::write(&file, "not an env line\n").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();

    let seats = zai_seats(workspace.path());
    let seat = seats.iter().find(|s| s.name == "seat-garbled");
    assert!(seat
        .expect("malformed seat must not be dropped")
        .key
        .is_none());

    let rows = zai::sample(seats).await;
    let row = rows.iter().find(|r| r.account == "seat-garbled").unwrap();
    assert_eq!(row.usage_fraction, None);
    assert_eq!(row.usage_fraction_weekly, None);
}
