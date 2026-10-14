//! Every `SweepKind` consumer handles `Step` (Issue #11898).
//!
//! Before `SweepKind::Step`, about twenty sites matched `SweepKind::Issue` with
//! a `let`-`else`, an `if let`, a `matches!` or a `_ =>` arm, and so dropped
//! every other kind without a word. A new kind compiles straight through such
//! a site, so the compiler cannot catch the next one. Two tests do:
//!
//! - [`no_production_site_drops_other_kinds_silently`] scans the crate's
//!   non-test source for those shapes. A site that means "an issue sweep
//!   only" says so with an exhaustive `match` naming `Step`, or asks
//!   [`SweepKind::item_number`] / [`SweepKind::issue_number`].
//! - [`every_known_site_names_step`] enumerates the sites the issue listed and
//!   checks each still handles `Step` explicitly.

use std::path::{Path, PathBuf};

fn rust_sources_under(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {dir:?}: {e}"));
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_sources_under(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The code lines of `text` up to its inline test module (`#[cfg(test)]`
/// then attributes then `mod … {`; a `#[cfg(test)] mod x;` declaration or a
/// test-only item mid-file does not end it), with comment lines blanked.
fn production_lines(text: &str) -> Vec<&str> {
    // The first item after the attributes (a multi-line `#[allow(…)]` included).
    const ITEM_STARTS: &[&str] = &["mod ", "fn ", "pub", "use ", "impl", "const ", "static "];
    let lines: Vec<&str> = text.lines().collect();
    let end = lines
        .iter()
        .enumerate()
        .find(|(i, l)| {
            l.trim() == "#[cfg(test)]"
                && lines[i + 1..]
                    .iter()
                    .take(12)
                    .map(|n| n.trim())
                    .find(|n| ITEM_STARTS.iter().any(|k| n.starts_with(k)))
                    .is_some_and(|n| n.starts_with("mod ") && n.ends_with('{'))
        })
        .map_or(lines.len(), |(i, _)| i);
    lines[..end]
        .iter()
        .map(|l| {
            let t = l.trim_start();
            if t.starts_with("//") || t.starts_with('*') {
                ""
            } else {
                l
            }
        })
        .collect()
}

/// The shapes that drop non-`Issue` kinds without naming them, found on one
/// line or (for `_ =>`) within the three lines after an `Issue` arm.
fn silent_drops(lines: &[&str]) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let code = line.trim();
        let names_issue = code.contains("SweepKind::Issue(");
        let names_prset = code.contains("SweepKind::PrSet(");
        if !names_issue && !names_prset {
            continue;
        }
        // A `PrSet`-only `if let` acts on a PR set and is not a drop; an
        // `Issue`-only one drops `Step` (and `PrSet`) silently.
        let refutable_let = names_issue
            && ((code.starts_with("let ") && code.contains(" else"))
                || code.starts_with("if let ")
                || code.starts_with("while let ")
                || code.contains("} else if let "));
        let in_matches = names_issue && code.contains("matches!(");
        // An `Issue` or `PrSet` arm followed by `_ =>` lets a new kind fall
        // into the wildcard.
        let wildcard_after = code.contains("=>")
            && lines
                .iter()
                .skip(i + 1)
                .take(3)
                .any(|next| next.trim_start().starts_with("_ =>"));
        if refutable_let || in_matches || wildcard_after {
            found.push((i + 1, code.to_string()));
        }
    }
    found
}

#[test]
fn the_scan_recognises_each_silent_shape() {
    let shapes = [
        "let SweepKind::Issue(issue) = info.kind else {",
        "if let SweepKind::Issue(n) = info.kind {",
        "if let (Some(s), SweepKind::Issue(issue)) = (&session, kind) {",
        "    && matches!(info.kind, SweepKind::Issue(_))",
    ];
    for shape in shapes {
        assert_eq!(silent_drops(&[shape]).len(), 1, "{shape}");
    }
    let wildcard = ["SweepKind::Issue(n) => Some(n),", "_ => None,"];
    assert_eq!(silent_drops(&wildcard).len(), 1);
    let prset_wildcard = ["SweepKind::PrSet(_) => None,", "_ => Some(1),"];
    assert_eq!(silent_drops(&prset_wildcard).len(), 1);
    // Acting on a PR set alone is not a drop.
    assert!(silent_drops(&["if let SweepKind::PrSet(prs) = kind {"]).is_empty());
    let exhaustive = [
        "SweepKind::Issue(n) => Some(n),",
        "SweepKind::Step { .. } | SweepKind::PrSet(_) => None,",
    ];
    assert!(silent_drops(&exhaustive).is_empty());
    assert!(silent_drops(&["kind: SweepKind::Issue(issue),"]).is_empty());
}

#[test]
fn no_production_site_drops_other_kinds_silently() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_sources_under(&src, &mut files);
    assert!(files.len() > 50, "source walk found suspiciously few files");

    let mut offenders = Vec::new();
    for path in files {
        let rel = path
            .strip_prefix(&src)
            .unwrap_or(&path)
            .display()
            .to_string();
        if rel.contains("test") {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        for (line, code) in silent_drops(&production_lines(&text)) {
            offenders.push(format!("{rel}:{line}: {code}"));
        }
    }
    assert!(
        offenders.is_empty(),
        "these sites drop `SweepKind::Step` (and `PrSet`) without naming it (#11898). Match \
         the kind exhaustively, or use `SweepKind::item_number()` / `issue_number()`:\n{}",
        offenders.join("\n")
    );
}

/// The consumers #11898 listed, each with the code that handles a step
/// there. The needle is searched in production code only (comments blanked,
/// inline tests cut), so a doc mention does not count. Most sites name the
/// kind or ask its item helpers; the roll-resume host works from the
/// manifest, so its handling is the manifest's `step`. `busy_slots.rs` reads
/// the tick's occupancy, which `work_finder/forge.rs` computes (listed).
const KNOWN_SITES: &[(&str, &[&str])] = &[
    ("sweep_registry/watchdog.rs", KIND_NEEDLES),
    ("observability/fleet_state/sources.rs", KIND_NEEDLES),
    ("observability/collector/identity.rs", KIND_NEEDLES),
    ("sweep_registry/outcome_journal.rs", KIND_NEEDLES),
    ("auto_update/pause_roll/host.rs", KIND_NEEDLES),
    ("auto_update/pause_resume/host.rs", &["item.step", "requeue_step("]),
    ("sweep_registry/reaper.rs", KIND_NEEDLES),
    ("cli/cancel.rs", KIND_NEEDLES),
    ("dispatch_tap.rs", KIND_NEEDLES),
    ("sweep_registry/roll_resume.rs", KIND_NEEDLES),
    ("sweep_registry/model_dispatch.rs", KIND_NEEDLES),
    ("work_finder/forge.rs", KIND_NEEDLES),
];

/// A `Step` arm, or one of the kind's item helpers.
const KIND_NEEDLES: &[&str] = &["SweepKind::Step", "item_number()", "issue_number()"];

#[test]
fn every_known_site_names_step() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for (site, needles) in KNOWN_SITES {
        let text = std::fs::read_to_string(src.join(site))
            .unwrap_or_else(|e| panic!("cannot read {site}: {e}"));
        let code = production_lines(&text).join("\n");
        let handles = needles.iter().any(|needle| code.contains(needle));
        assert!(handles, "{site} no longer handles `SweepKind::Step` in code ({needles:?})");
    }
}

/// The needles are not vacuous: a file that only mentions a step in prose,
/// or says `step` in passing, does not pass.
#[test]
fn a_prose_mention_does_not_count_as_handling() {
    let prose = "//! A `SweepKind::Step` is counted.\nfn f() { let step = 1; }";
    let code = production_lines(prose).join("\n");
    assert!(!KIND_NEEDLES.iter().any(|n| code.contains(n)));
}

/// Every production `kind: SweepKind::Issue(…)`, a struct constructor or an
/// `Event` field pattern, reviewed (#11898). These are the shapes the
/// silent-drop scan allows, so a reconstruction that rebuilds a live step as
/// an issue sweep (`locks.rs` before #11898) or an `Event` match whose
/// `_ =>` drops a step dispatch hides in them. A new one, or one more in a
/// listed file, fails until it is reviewed here.
const ISSUE_KIND_FIELDS: &[(&str, usize, &str)] = &[
    ("cli/dispatch.rs", 1, "the CLI dispatches an issue sweep"),
    (
        "sweep_registry/locks.rs",
        2,
        "checkpoint recovery and journal adoption rebuild issue sweeps only; a \
         live step's lock is adopted as a step (step_adopt.rs)",
    ),
    (
        "observability/collector.rs",
        2,
        "event_issue names Step (not sweep.* telemetry); map_event_to_records is only \
         reached for an event event_issue keys",
    ),
    ("observability/collector/identity.rs", 1, "observe names Step"),
    ("observability/session_output.rs", 1, "handle_event names Step"),
    (
        "observability/ops/turnaround.rs",
        2,
        "SlotLedger counts steps; TurnoverLedger is issue-only by design",
    ),
    ("safehouse/envelopes.rs", 1, "event_to_envelope names Step"),
    (
        "safehouse.rs",
        1,
        "digest batching of issue dispatches; any other dispatch goes to \
         event_to_envelope, which names Step",
    ),
];

#[test]
fn every_issue_kind_field_is_reviewed() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_sources_under(&src, &mut files);
    let mut found: Vec<(String, usize)> = Vec::new();
    for path in files {
        let rel = path
            .strip_prefix(&src)
            .unwrap_or(&path)
            .display()
            .to_string();
        if rel.contains("test") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let n = production_lines(&text)
            .iter()
            .filter(|l| {
                l.contains("kind: SweepKind::Issue(")
                    || l.contains("kind: crate::types::SweepKind::Issue(")
            })
            .count();
        if n > 0 {
            found.push((rel, n));
        }
    }
    let mut unreviewed = Vec::new();
    for (rel, n) in &found {
        match ISSUE_KIND_FIELDS.iter().find(|(f, _, _)| f == rel) {
            Some((_, allowed, _)) if n <= allowed => {}
            _ => unreviewed.push(format!("{rel}: {n}")),
        }
    }
    assert!(
        unreviewed.is_empty(),
        "new `kind: SweepKind::Issue(…)` sites: build a `Step` as a step and name it in every \
         `Event` match, then list the site in ISSUE_KIND_FIELDS (#11898):\n{}",
        unreviewed.join("\n")
    );
}
