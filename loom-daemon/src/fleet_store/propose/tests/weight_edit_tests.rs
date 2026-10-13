use super::*;

const SAMPLE: &str = "\
# fleet.yml
root: /srv/src

repos:
  # comment kept verbatim
  - name: app
    dir: app
    fleet: true
    weight: 5   # why 5
  - name: infra
    dir: infra
    fleet: true
    firewall: false

state:
  fleet:
    state: running
";

#[test]
fn updates_an_existing_weight_in_place() {
    let out = edit(SAMPLE, "app", 20).unwrap();
    let expected = SAMPLE.replace("weight: 5 ", "weight: 20 ");
    assert_eq!(out, expected);
}

#[test]
fn inserts_weight_when_the_record_has_none() {
    let out = edit(SAMPLE, "infra", 7).unwrap();
    assert!(out.contains(
        "  - name: infra\n    dir: infra\n    fleet: true\n    firewall: false\n    \
         weight: 7\n\nstate:\n"
    ));
    // The other record, and everything else, is untouched.
    assert!(out.contains("weight: 5   # why 5"));
}

#[test]
fn never_touches_fleet_or_firewall_flags() {
    let out = edit(SAMPLE, "infra", 7).unwrap();
    assert!(out.contains("fleet: true\n    firewall: false\n"));
}

#[test]
fn unknown_repo_name_is_a_clear_error() {
    let err = edit(SAMPLE, "nope", 1).unwrap_err();
    assert!(
        err.to_string()
            .contains("no repos[] record with name `nope`"),
        "{err}"
    );
}

#[test]
fn preserves_comments_and_a_trailing_newline() {
    let out = edit(SAMPLE, "app", 1).unwrap();
    assert!(out.contains("# comment kept verbatim"));
    assert!(out.ends_with('\n'));
}
