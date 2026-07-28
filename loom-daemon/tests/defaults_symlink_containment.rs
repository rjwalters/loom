//! Guards the self-containment invariant of `defaults/` (Issue #4097).
//!
//! `defaults/roles/*.md` — and, as of #4097, `defaults/docs/{daemon-reference,
//! troubleshooting,build-gate,github-authentication,forge-authentication,
//! tool-use-concurrency-errors}.md` — ship as relative symlinks (e.g.
//! `defaults/roles/curator.md -> ../.claude/commands/loom/curator.md`). Every
//! one of them MUST resolve to a path INSIDE `defaults/`: `resolve_defaults_path`
//! (`src/init/git.rs`) supports a bundled-resource layout where `defaults/` is
//! copied standalone into a `Contents/Resources` directory, and a symlink
//! escaping `defaults/` (e.g. `defaults/docs/X -> ../../.loom/docs/X`) would
//! dangle there even though it resolves fine in this checkout.
//!
//! This is a static assertion over the repo tree, not daemon behavior — no
//! `TestDaemon` needed.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};

fn defaults_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../defaults")
}

/// Recursively collect every symlink under `dir`.
fn collect_symlinks(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries =
        fs::read_dir(dir).unwrap_or_else(|e| panic!("failed to read {}: {e}", dir.display()));
    for entry in entries {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)
            .unwrap_or_else(|e| panic!("failed to stat {}: {e}", path.display()));
        if meta.is_symlink() {
            out.push(path);
        } else if meta.is_dir() {
            collect_symlinks(&path, out);
        }
    }
}

#[test]
fn test_all_defaults_symlinks_resolve_inside_defaults() {
    let defaults = defaults_dir()
        .canonicalize()
        .expect("defaults/ must exist at the repo root");

    let mut symlinks = Vec::new();
    collect_symlinks(&defaults, &mut symlinks);
    assert!(
        !symlinks.is_empty(),
        "expected at least one symlink under defaults/ (the defaults/roles/ \
         convention) — if this legitimately dropped to zero, this test's \
         premise needs revisiting, not silent deletion"
    );

    let mut escaping = Vec::new();
    for link in &symlinks {
        match link.canonicalize() {
            Ok(resolved) if resolved.starts_with(&defaults) => {}
            Ok(resolved) => escaping.push(format!(
                "{} -> {} (escapes defaults/)",
                link.display(),
                resolved.display()
            )),
            Err(e) => escaping.push(format!("{} -> UNRESOLVABLE ({e})", link.display())),
        }
    }

    assert!(
        escaping.is_empty(),
        "symlink(s) under defaults/ must resolve to a path INSIDE defaults/ — \
         the bundled-resource layout (`resolve_defaults_path` in \
         src/init/git.rs) copies defaults/ standalone, so an escaping symlink \
         dangles there even though it resolves in this checkout:\n{}",
        escaping.join("\n")
    );
}
