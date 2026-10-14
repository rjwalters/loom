use serde_json::json;

use super::*;

/// A `fleet.yml`-shaped document: comments everywhere, a folded scalar, a
/// flow `{}`, records in a block list.
pub(crate) const FLEET_YML: &str = "\
# fleet.yml: the fleet model.

root: ~/src   # where clones live

repos:
  - name: app
    remote: git@example.com:acme/app.git
    # why app is first
    fleet: true
    fleet_priority: 5   # raised 2026-10-01
    purpose: The app.

  # a comment between records
  - name: \"infra\"
    fleet: true
    firewall: false

state:
  fleet:
    state: running
    since: 2026-09-30T13:18Z
    reason: >-
      Restarted after the
      long pause.
  hosts:
    build-1:
      state: running

config:
  defaults: {}
  hosts:
    build-1:
      defaults:
        autonomous:
          workFinder:
            maxConcurrent: 8
        owners:
          - acme
      # build-1 has no local tier yet
";

fn set(text: &str, path: &[Seg<'_>], v: serde_json::Value) -> String {
    let mut doc = Doc::new(text);
    doc.set(path, &v).unwrap();
    doc.finish()
}

fn remove(text: &str, path: &[Seg<'_>]) -> (bool, String) {
    let mut doc = Doc::new(text);
    let removed = doc.remove(path).unwrap();
    (removed, doc.finish())
}

#[test]
fn replacing_a_scalar_keeps_its_inline_comment_and_every_other_line() {
    let out = set(
        FLEET_YML,
        &[
            Seg::Key("repos"),
            Seg::Item {
                field: "name",
                value: "app",
            },
            Seg::Key("fleet_priority"),
        ],
        json!(40),
    );
    assert_eq!(out, FLEET_YML.replace("fleet_priority: 5   #", "fleet_priority: 40   #"));
}

#[test]
fn a_key_missing_from_a_record_goes_after_its_last_line_not_after_the_next_comment() {
    let out = set(
        FLEET_YML,
        &[
            Seg::Key("repos"),
            Seg::Item {
                field: "name",
                value: "infra",
            },
            Seg::Key("fleet_priority"),
        ],
        json!(7),
    );
    assert!(
        out.contains("  - name: \"infra\"\n    fleet: true\n    firewall: false\n    fleet_priority: 7\n\nstate:"),
        "{out}"
    );
    let out = set(
        FLEET_YML,
        &[
            Seg::Key("repos"),
            Seg::Item {
                field: "name",
                value: "app",
            },
            Seg::Key("dir"),
        ],
        json!("app-dir"),
    );
    assert!(
        out.contains("    purpose: The app.\n    dir: app-dir\n\n  # a comment between records\n"),
        "{out}"
    );
}

#[test]
fn an_unknown_record_is_a_clear_error() {
    let mut doc = Doc::new(FLEET_YML);
    let err = doc
        .set(
            &[
                Seg::Key("repos"),
                Seg::Item {
                    field: "name",
                    value: "nope",
                },
                Seg::Key("fleet_priority"),
            ],
            &json!(1),
        )
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("no repos[] record with name `nope`"),
        "{err}"
    );
}

#[test]
fn a_folded_scalar_is_replaced_whole() {
    let out = set(
        FLEET_YML,
        &[Seg::Key("state"), Seg::Key("fleet"), Seg::Key("reason")],
        json!("Paused: \"maintenance\" #2"),
    );
    assert!(
        out.contains("    since: 2026-09-30T13:18Z\n    reason: \"Paused: \\\"maintenance\\\" #2\"\n  hosts:"),
        "{out}"
    );
    assert!(!out.contains("long pause"));
}

#[test]
fn missing_mappings_are_created_under_the_nearest_existing_one() {
    let out = set(
        FLEET_YML,
        &[
            Seg::Key("state"),
            Seg::Key("hosts"),
            Seg::Key("build-2"),
            Seg::Key("state"),
        ],
        json!("paused"),
    );
    assert!(
        out.contains(
            "    build-1:\n      state: running\n    build-2:\n      state: paused\n\nconfig:"
        ),
        "{out}"
    );
}

#[test]
fn an_empty_flow_mapping_is_opened_to_take_a_key() {
    let out = set(
        FLEET_YML,
        &[
            Seg::Key("config"),
            Seg::Key("defaults"),
            Seg::Key("autonomous"),
            Seg::Key("enabled"),
        ],
        json!(true),
    );
    assert!(
        out.contains("  defaults:\n    autonomous:\n      enabled: true\n  hosts:"),
        "{out}"
    );
}

#[test]
fn a_new_subtree_is_written_in_block_style() {
    let out = set(
        FLEET_YML,
        &[
            Seg::Key("config"),
            Seg::Key("hosts"),
            Seg::Key("build-1"),
            Seg::Key("local"),
        ],
        json!({"observability": {"enabled": true, "endpoint": "https://x.example/ingest"},
               "roles": ["judge", "yes"], "empty": {}, "n": null, "ratio": 0.5}),
    );
    // `n` is a YAML 1.1 boolean: as a key it is quoted too.
    let expected = "\
            maxConcurrent: 8
        owners:
          - acme
      local:
        observability:
          enabled: true
          endpoint: \"https://x.example/ingest\"
        roles:
          - judge
          - \"yes\"
        empty: {}
        \"n\": null
        ratio: 0.5
      # build-1 has no local tier yet
";
    assert!(out.ends_with(expected), "{out}");
}

#[test]
fn a_list_is_replaced_whole_and_strings_that_need_it_are_quoted() {
    let out = set(
        FLEET_YML,
        &[
            Seg::Key("config"),
            Seg::Key("hosts"),
            Seg::Key("build-1"),
            Seg::Key("defaults"),
            Seg::Key("owners"),
        ],
        json!(["acme", "2am", "on"]),
    );
    assert!(
        out.contains("        owners:\n          - acme\n          - \"2am\"\n          - \"on\"\n      # build-1"),
        "{out}"
    );
}

#[test]
fn removing_the_last_key_of_a_mapping_leaves_an_empty_flow_mapping() {
    let path = [
        Seg::Key("config"),
        Seg::Key("hosts"),
        Seg::Key("build-1"),
        Seg::Key("defaults"),
        Seg::Key("autonomous"),
        Seg::Key("workFinder"),
        Seg::Key("maxConcurrent"),
    ];
    let (removed, out) = remove(FLEET_YML, &path);
    assert!(removed);
    assert!(
        out.contains("        autonomous:\n          workFinder: {}\n        owners:"),
        "{out}"
    );
    let (removed, again) = remove(&out, &path);
    assert!(!removed, "already gone");
    assert_eq!(again, out);
}

#[test]
fn removing_a_key_takes_its_value_lines_and_nothing_else() {
    let (removed, out) =
        remove(FLEET_YML, &[Seg::Key("state"), Seg::Key("fleet"), Seg::Key("reason")]);
    assert!(removed);
    assert_eq!(
        out,
        FLEET_YML.replace("    reason: >-\n      Restarted after the\n      long pause.\n", "")
    );
}

#[test]
fn walking_into_a_scalar_is_refused() {
    let mut doc = Doc::new(FLEET_YML);
    let err = doc
        .set(&[Seg::Key("root"), Seg::Key("x")], &json!(1))
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("`root` in fleet.yml is not a block mapping"),
        "{err}"
    );
}

#[test]
fn a_document_without_a_trailing_newline_keeps_it_that_way() {
    let out = set("a:\n  b: 1", &[Seg::Key("a"), Seg::Key("b")], json!(2));
    assert_eq!(out, "a:\n  b: 2");
    let out = set("a: 1\n", &[Seg::Key("c")], json!("x y"));
    assert_eq!(out, "a: 1\nc: \"x y\"\n");
}

#[test]
fn keys_and_values_are_matched_through_their_quoting() {
    let text = "\"odd key\": 1\n'it''s': 2\nlist:\n- name: 'a'\n  v: 1\n";
    assert_eq!(set(text, &[Seg::Key("odd key")], json!(3)), text.replace(": 1\n'", ": 3\n'"));
    assert_eq!(set(text, &[Seg::Key("it's")], json!(4)), text.replace(": 2", ": 4"));
    let out = set(
        text,
        &[
            Seg::Key("list"),
            Seg::Item {
                field: "name",
                value: "a",
            },
            Seg::Key("v"),
        ],
        json!(9),
    );
    assert_eq!(out, text.replace("v: 1", "v: 9"));
}
