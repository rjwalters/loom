use serde_json::json;

use super::parse;

#[test]
fn block_mappings_sequences_and_scalar_types() {
    let v = parse(
        "# header\n---\nroot: ~/GitHub\ncount: 42\nneg: -3\nratio: 1.5\non: true\noff: False\nnothing: ~\nempty:\nwhen: 2026-09-30T01:30Z\nlist:\n  - a\n  - 2\n",
    )
    .unwrap();
    assert_eq!(
        v,
        json!({
            "root": "~/GitHub", "count": 42, "neg": -3, "ratio": 1.5, "on": true, "off": false,
            "nothing": null, "empty": null, "when": "2026-09-30T01:30Z", "list": ["a", 2]
        })
    );
}

#[test]
fn sequence_of_mappings_at_either_indentation() {
    let nested = "repos:\n  - name: a\n    fleet: true\n  - name: b\n    dir: bee\n";
    let compact = "repos:\n- name: a\n  fleet: true\n- name: b\n  dir: bee\n";
    let want = json!({"repos": [{"name": "a", "fleet": true}, {"name": "b", "dir": "bee"}]});
    assert_eq!(parse(nested).unwrap(), want);
    assert_eq!(parse(compact).unwrap(), want);
}

#[test]
fn comments_quotes_and_colons_in_values() {
    let v = parse(concat!(
        "remote: git@github.com:acme/app.git   # ssh\n",
        "purpose: The team's engine; not a # comment start\n",
        "quoted: \"a: b # not a comment\"  # but this is\n",
        "single: 'it''s'\n",
        "escaped: \"tab\\there \\u00e9\"\n",
        "\"quoted key\": 1\n",
        "url: https://example.com/x\n",
    ))
    .unwrap();
    assert_eq!(v["remote"], "git@github.com:acme/app.git");
    assert_eq!(v["purpose"], "The team's engine; not a");
    assert_eq!(v["quoted"], "a: b # not a comment");
    assert_eq!(v["single"], "it's");
    assert_eq!(v["escaped"], "tab\there é");
    assert_eq!(v["quoted key"], 1);
    assert_eq!(v["url"], "https://example.com/x");
}

#[test]
fn flow_collections() {
    let v =
        parse("consumes: [gf-a, gf-b, \"c d\"]   # why\nempty: []\nmap: {k: v, n: 2}\n").unwrap();
    assert_eq!(v["consumes"], json!(["gf-a", "gf-b", "c d"]));
    assert_eq!(v["empty"], json!([]));
    assert_eq!(v["map"], json!({"k": "v", "n": 2}));
}

#[test]
fn block_scalars_fold_and_chomp() {
    let v = parse(concat!(
        "fleet:\n",
        "  reason: >-\n",
        "    Clearing the backlog\n",
        "    and moving config.\n",
        "\n",
        "  literal: |\n",
        "    line one\n",
        "      indented\n",
        "  kept: |+\n",
        "    x\n",
        "\n",
        "hosts: {}\n",
    ))
    .unwrap();
    assert_eq!(v["fleet"]["reason"], "Clearing the backlog and moving config.");
    assert_eq!(v["fleet"]["literal"], "line one\n  indented\n");
    assert_eq!(v["fleet"]["kept"], "x\n\n");
    assert_eq!(v["hosts"], json!({}));
}

#[test]
fn a_realistic_state_file_parses() {
    let v = parse(concat!(
        "fleet:\n",
        "  state: stopped\n",
        "  since: 2026-09-30T01:30Z\n",
        "  reason: >-\n",
        "    Clearing the PR backlog (main moved faster than a 15-min CI, so\n",
        "    the freshness guard refused most PRs).\n",
        "\n",
        "hosts:\n",
        "  build-7:\n",
        "    state: stopped      # forced drain-and-exit 01:54Z; booted out\n",
        "  build-8:\n",
        "    state: stopped\n",
    ))
    .unwrap();
    assert_eq!(v["hosts"]["build-7"]["state"], "stopped");
    assert!(v["fleet"]["reason"]
        .as_str()
        .unwrap()
        .ends_with("refused most PRs)."));
}

#[test]
fn unsupported_or_ambiguous_input_fails_closed() {
    for bad in [
        "a: &anchor 1\n",
        "a: *alias\n",
        "a: !tag 1\n",
        "a: 1\na: 2\n",
        "a: 1\n\tb: 2\n",
        "a:\n\t- x\n",
        "a: plain\n  continued\n",
        "a: 1\n---\nb: 2\n",
        "%YAML 1.2\na: 1\n",
        "a: [1, 2\n",
        "a: \"unterminated\n",
        "a: \"x\" trailing\n",
        "list:\n  - a\n   - b\n",
    ] {
        assert!(parse(bad).is_err(), "must refuse: {bad:?}");
    }
}

#[test]
fn empty_document_is_null() {
    assert_eq!(parse("").unwrap(), serde_json::Value::Null);
    assert_eq!(parse("# only a comment\n\n").unwrap(), serde_json::Value::Null);
}
