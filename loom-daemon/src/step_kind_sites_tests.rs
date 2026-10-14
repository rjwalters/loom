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

/// The code lines of `text` before its first `#[cfg(test)]` (inline test
/// modules construct and match kinds freely), with comment lines blanked.
fn production_lines(text: &str) -> Vec<&str> {
    text.lines()
        .take_while(|l| l.trim() != "#[cfg(test)]")
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
        if !code.contains("SweepKind::Issue(") {
            continue;
        }
        let refutable_let = (code.starts_with("let ") && code.contains(" else"))
            || code.starts_with("if let ")
            || code.starts_with("while let ")
            || code.contains("} else if let ");
        let in_matches = code.contains("matches!(");
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

/// The consumers #11898 listed. Each must name `Step` (an arm) or ask the
/// kind's item helpers.
const KNOWN_SITES: &[&str] = &[
    "sweep_registry/watchdog.rs",
    "observability/ops/busy_slots.rs",
    "observability/fleet_state/sources.rs",
    "observability/collector/identity.rs",
    "sweep_registry/outcome_journal.rs",
    "auto_update/pause_roll/host.rs",
    "auto_update/pause_resume/host.rs",
    "sweep_registry/reaper.rs",
    "cli/cancel.rs",
    "dispatch_tap.rs",
    "sweep_registry/roll_resume.rs",
    "sweep_registry/model_dispatch.rs",
    "work_finder/forge.rs",
];

#[test]
fn every_known_site_names_step() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for site in KNOWN_SITES {
        let text = std::fs::read_to_string(src.join(site))
            .unwrap_or_else(|e| panic!("cannot read {site}: {e}"));
        let handles = [
            "SweepKind::Step",
            "item_number()",
            "issue_number()",
            "Step {",
            "step",
        ]
        .iter()
        .any(|needle| text.contains(needle));
        assert!(handles, "{site} no longer handles `SweepKind::Step`");
    }
}
