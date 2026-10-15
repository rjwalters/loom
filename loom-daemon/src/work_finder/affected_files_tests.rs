//! Coverage for the same-tick affected-files overlap gate (#9781). The
//! tick-level cases drive `tick_multi_with_sharding` with minimal fakes.

use std::collections::BTreeSet;

use super::super::*;
use super::{affected_surface, overlap, OverlapGate};

fn body(paths: &[&str]) -> String {
    let list: String = paths.iter().map(|p| format!("- `{p}` — edit\n")).collect();
    format!("intro\n\n### Affected Files\n\n{list}\n### Other\n\n- `ignored/outside.rs`\n")
}

fn set(paths: &[&str]) -> BTreeSet<String> {
    paths.iter().map(|p| (*p).to_string()).collect()
}

#[test]
fn parser_extracts_backticked_paths_and_treats_the_rest_as_unknown() {
    let s = affected_surface(&body(&["a/b.rs", "c.md"])).unwrap();
    assert_eq!(s, set(&["a/b.rs", "c.md"]), "section only; identifiers excluded");
    assert_eq!(affected_surface("### Affected Files\n\nTo be determined\n"), None);
    assert_eq!(affected_surface("no section, `a/b.rs` here"), None);
    assert_eq!(affected_surface("## Affected Files\n\n- a/b.rs unquoted\n"), None);
    assert_eq!(affected_surface("## Affected Files\n- `shape_queue` and `Qd::X`\n"), None);
    assert_eq!(
        affected_surface("## Affected Files\n- `src/x.rs:120`\n").unwrap(),
        set(&["src/x.rs"])
    );
}

#[test]
fn slashless_spans_need_a_known_file_extension() {
    let s = affected_surface(
        "## Affected Files\n- `WorkItem.body` via `PickSkipReason.ALL` since `v0.19.1`\n- `CLAUDE.md`, `Cargo.toml`\n",
    )
    .unwrap();
    assert_eq!(s, set(&["CLAUDE.md", "Cargo.toml"]), "identifiers and versions are not paths");
    assert_eq!(affected_surface("## Affected Files\n- `WorkItem.body`\n"), None);
    assert_eq!(
        affected_surface("## Affected Files\n- `src/Type.weird`\n").unwrap(),
        set(&["src/Type.weird"]),
        "a `/` alone makes it a path"
    );
}

#[test]
fn overlap_is_the_sorted_intersection() {
    assert_eq!(overlap(&set(&["b", "a", "c"]), &set(&["c", "a", "z"])), vec!["a", "c"]);
}

fn cand(number: u32, level: crate::priority_pick::Level) -> PriorityCandidate {
    PriorityCandidate {
        workspace_idx: 0,
        workspace_weight: 0,
        created_at: None,
        number,
        level,
        main_red_fix: false,
    }
}

#[test]
fn the_gate_reports_overlap_for_every_level() {
    use crate::priority_pick::Level;
    let mut gate = OverlapGate::default();
    gate.note(0, 9, Some(&body(&["a.rs"])), true);
    gate.note(0, 1, Some(&body(&["a.rs", "b.rs"])), false);
    gate.note(0, 2, Some(&body(&["b.rs"])), false);
    for level in [Level::Default, Level::Important, Level::VeryImportant] {
        assert_eq!(gate.overlapping(&cand(1, level)), vec!["a.rs"], "{level:?}");
    }
    gate.occupy(&cand(1, Level::VeryImportant));
    assert_eq!(gate.overlapping(&cand(2, Level::Default)), vec!["b.rs"]);
}

#[test]
fn a_red_main_fix_candidate_skips_the_gate_but_still_occupies() {
    use crate::priority_pick::Level;
    let mut gate = OverlapGate::default();
    gate.note(0, 9, Some(&body(&["a.rs"])), true);
    gate.note(0, 1, Some(&body(&["a.rs", "b.rs"])), false);
    gate.note(0, 2, Some(&body(&["b.rs"])), false);
    let fix = PriorityCandidate {
        main_red_fix: true,
        ..cand(1, Level::Default)
    };
    assert!(gate.overlapping(&fix).is_empty());
    assert_eq!(gate.overlapping(&cand(1, Level::Default)), vec!["a.rs"]);
    gate.occupy(&fix);
    assert_eq!(gate.overlapping(&cand(2, Level::VeryImportant)), vec!["b.rs"]);
}

#[test]
fn extensionless_root_files_are_paths_but_identifiers_are_not() {
    let s = affected_surface(
        "## Affected Files\n- `Dockerfile`, `Makefile`, `VERSION`, `./LICENSE`\n- `version`, `Makefiles`, `dockerfile`\n",
    )
    .unwrap();
    assert_eq!(s, set(&["Dockerfile", "Makefile", "VERSION", "LICENSE"]));
    assert_eq!(affected_surface("## Affected Files\n- `version` and `Makefiles`\n"), None);
}
