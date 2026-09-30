use super::*;

fn lines_of(text: &str) -> Vec<String> {
    text.lines().map(str::to_string).collect()
}

#[test]
fn finds_a_top_level_block_and_its_child_indent() {
    let text = "fleet:\n  state: running\n  since: x\nhosts:\n  build-2:\n    state: paused\n";
    let lines = lines_of(text);
    let fleet = find_block(&lines, 0, lines.len(), 0, "fleet").unwrap();
    assert_eq!(fleet.start, 1);
    assert_eq!(fleet.end, 3);
    assert_eq!(fleet.child_indent, 2);

    let hosts = find_block(&lines, 0, lines.len(), 0, "hosts").unwrap();
    assert_eq!(hosts.start, 4);
    assert_eq!(hosts.end, 6);
    assert_eq!(hosts.child_indent, 2);

    let build2 = find_block(&lines, hosts.start, hosts.end, hosts.child_indent, "build-2").unwrap();
    assert_eq!(build2.child_indent, 4);
}

#[test]
fn missing_block_is_none() {
    let lines = lines_of("fleet:\n  state: running\n");
    assert!(find_block(&lines, 0, lines.len(), 0, "hosts").is_none());
}

#[test]
fn find_or_append_top_block_creates_when_absent() {
    let mut lines = lines_of("fleet:\n  state: running\n");
    let hosts = find_or_append_top_block(&mut lines, 0, "hosts");
    assert_eq!(lines, vec!["fleet:", "  state: running", "hosts:"]);
    assert_eq!(hosts.start, 3);
    assert_eq!(hosts.end, 3);
    assert_eq!(hosts.child_indent, 2);

    // Idempotent: calling it again finds the block just created, unchanged.
    let again = find_or_append_top_block(&mut lines, 0, "hosts");
    assert_eq!(lines.len(), 3);
    assert_eq!(again.start, hosts.start);
}

#[test]
fn append_mapping_adds_one_header_line_at_the_blocks_end() {
    let mut lines = lines_of("hosts:\n  build-1:\n    state: running\n");
    let mut hosts = find_block(&lines, 0, lines.len(), 0, "hosts").unwrap();
    let build2 = append_mapping(&mut lines, &mut hosts, "build-2");
    assert_eq!(lines, vec!["hosts:", "  build-1:", "    state: running", "  build-2:"]);
    assert_eq!(build2.start, 4);
    assert_eq!(build2.child_indent, 4);
    assert_eq!(hosts.end, 4);
}

#[test]
fn set_scalar_replaces_an_existing_key_in_place_touching_nothing_else() {
    let mut lines = lines_of("fleet:\n  state: running\n  since: old\n");
    let mut block = find_block(&lines, 0, lines.len(), 0, "fleet").unwrap();
    set_scalar(&mut lines, &mut block, "state", "paused");
    assert_eq!(lines, vec!["fleet:", "  state: paused", "  since: old"]);
}

#[test]
fn set_scalar_appends_a_missing_key_as_the_blocks_last_line() {
    let mut lines = lines_of("fleet:\n  state: running\n");
    let mut block = find_block(&lines, 0, lines.len(), 0, "fleet").unwrap();
    set_scalar(&mut lines, &mut block, "by", "operator");
    assert_eq!(lines, vec!["fleet:", "  state: running", "  by: operator"]);
    assert_eq!(block.end, 3);
}

#[test]
fn set_scalar_quoted_escapes_quotes_and_backslashes() {
    let mut lines = lines_of("fleet:\n  state: running\n");
    let mut block = find_block(&lines, 0, lines.len(), 0, "fleet").unwrap();
    set_scalar_quoted(&mut lines, &mut block, "reason", "a \"quote\" and a \\backslash");
    assert_eq!(lines[2], "  reason: \"a \\\"quote\\\" and a \\\\backslash\"");
}
