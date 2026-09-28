//! The stale `loom-*` PATH entry-point advisory (#4079/#4557) and the opt-in
//! prune that acts on it (#5139).
//!
//! THE INCIDENT THIS EXISTS FOR: a `pip install -e loom-tools` from months
//! earlier had left FROZEN console scripts in `~/.local/bin`. They outlived
//! the Python package, kept shadowing the Rust binary's own PATH entry points,
//! and so operators and agents silently ran ancient logic while `loom-daemon
//! --version` reported a fresh build. Epic #4081 Phase 4 (#4557) deleted the
//! package outright, which makes every surviving `loom-*` console script pure
//! hazard: nothing regenerates or updates them ever again.
//!
//! The advisory is WARNING ONLY on every ordinary run — it never deletes,
//! never mutates PATH, and never changes the exit code. The prune is narrower
//! than the advisory on purpose: only entries classified **exactly** as a
//! Python console script are removed, and an auto-generated bash shim is never
//! a candidate even when the advisory reports it as stale, because a stale
//! shim needs re-provisioning, not deletion.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::out;
use super::util;

/// `STALE_ENTRY_POINT_ALLOWLIST` — `loom-*` names that are not daemon entry
/// points and must not be flagged.
///
/// Empty as of #4970, which retired `loom-search`, the one entry it ever held.
/// Kept as a hook (and as the reason the membership test below exists at all)
/// for any future legitimate non-daemon `loom-*` console script.
const ALLOWLIST: &[&str] = &[];

/// The exact classification string the prune keys off. A near-miss here is a
/// silent no-op prune, so it is a constant shared by both readers rather than
/// two string literals that can drift.
const PYTHON_CONSOLE_SCRIPT: &str = "Python console script (stale pip/pipx editable install)";

/// `_lde_shim_target <path>` — for an auto-generated PATH shim, the
/// `loom-daemon` binary it execs (its sibling); `None` for anything else.
fn shim_target(path: &Path) -> Option<PathBuf> {
    // `grep -Iq .` is the portable "is this a text file?" test: `-I` treats a
    // binary file as non-matching, so a compiled binary is never a shim.
    let bytes = std::fs::read(path).ok()?;
    if bytes.is_empty() || bytes.contains(&0u8) {
        return None;
    }
    let text = String::from_utf8_lossy(&bytes);
    let looks_like_shim =
        text.lines().any(line_execs_loom_daemon) || text.contains("Auto-generated PATH shim");
    if !looks_like_shim {
        return None;
    }
    Some(path.parent().unwrap_or(Path::new(".")).join("loom-daemon"))
}

/// `grep -q 'exec .*/loom-daemon"\? '` — an `exec`, then anything, then a path
/// ending in `/loom-daemon`, then an OPTIONAL closing double quote, then a
/// SPACE. The trailing space is part of the pattern: a shim always passes
/// arguments, so `exec .../loom-daemon` on its own at end-of-line did not
/// match and must not start matching now.
fn line_execs_loom_daemon(line: &str) -> bool {
    let Some(after_exec) = line.find("exec ").map(|i| &line[i + "exec ".len()..]) else {
        return false;
    };
    // `.*` is greedy but the whole thing is an unanchored search, so any
    // occurrence suffices: scan every `/loom-daemon` and test what follows.
    let mut from = 0;
    while let Some(rel) = after_exec[from..].find("/loom-daemon") {
        let idx = from + rel + "/loom-daemon".len();
        let rest = &after_exec[idx..];
        if rest.starts_with(' ') || rest.starts_with("\" ") {
            return true;
        }
        from = idx;
    }
    false
}

/// `_lde_describe <path>` — the one-phrase classification for the warning line.
fn describe(path: &Path) -> &'static str {
    let first_line = std::fs::read(path)
        .ok()
        .map(|b| {
            let text = String::from_utf8_lossy(&b).to_string();
            text.lines().next().unwrap_or_default().to_string()
        })
        .unwrap_or_default();
    if first_line.contains("python") {
        PYTHON_CONSOLE_SCRIPT
    } else if first_line.starts_with("#!") {
        "script, not a loom-daemon shim"
    } else {
        "not a loom-daemon PATH shim"
    }
}

/// One `loom-*` executable found on `$PATH`, already classified.
struct Entry {
    path: PathBuf,
    name: String,
}

/// Walk `$PATH` exactly as the shell did: an empty element means `.`,
/// non-directories are skipped, and repeated entries are deduped by their
/// resolved path so one file is never reported twice.
fn scan_path() -> Vec<Entry> {
    let raw = std::env::var("PATH").unwrap_or_default();
    let mut seen_dirs: Vec<String> = Vec::new();
    let mut found = Vec::new();
    for element in raw.split(':') {
        let dir = if element.is_empty() { "." } else { element };
        let dir_path = Path::new(dir);
        if !dir_path.is_dir() {
            continue;
        }
        let dir_real = util::realpath(dir_path);
        if seen_dirs.contains(&dir_real) {
            continue;
        }
        seen_dirs.push(dir_real);

        let Ok(read) = std::fs::read_dir(dir_path) else {
            continue;
        };
        // bash's `for entry in "$dir"/loom-*` expands in sorted order.
        let mut names: Vec<String> = read
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with("loom-"))
            .collect();
        names.sort();
        for name in names {
            let path = dir_path.join(&name);
            // `[[ -f "$entry" && -x "$entry" ]]` — a regular file, executable.
            if !path.is_file() || !util::is_executable(&path) {
                continue;
            }
            found.push(Entry { path, name });
        }
    }
    found
}

/// `warn_stale_entry_points <resolved_daemon_bin>` — advisory only.
pub fn warn_stale(resolved: Option<&Path>) {
    if util::env_truthy("LOOM_SKIP_STALE_ENTRY_POINT_CHECK") {
        return;
    }
    let resolved_real = resolved.map(util::realpath).unwrap_or_default();
    let resolved_display = resolved
        .map(|p| p.display().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "<none>".to_string());

    let mut stale_lines: Vec<String> = Vec::new();
    let mut daemon_hits: Vec<PathBuf> = Vec::new();

    for entry in scan_path() {
        if entry.name == "loom-daemon" {
            daemon_hits.push(entry.path);
            continue;
        }
        if ALLOWLIST.contains(&entry.name.as_str()) {
            continue;
        }
        if let Some(target) = shim_target(&entry.path) {
            if util::is_executable(&target) {
                let shim_real = util::realpath(&target);
                if !resolved_real.is_empty() && shim_real == resolved_real {
                    continue; // a current shim pointing at the resolved binary
                }
                stale_lines.push(format!(
                    "{} — PATH shim execs {}, which is NOT the resolved binary ({resolved_display})",
                    entry.path.display(),
                    target.display()
                ));
                continue;
            }
        }
        stale_lines.push(format!("{} — {}", entry.path.display(), describe(&entry.path)));
    }

    if !stale_lines.is_empty() {
        out::warn(&format!("Stale 'loom-*' entry points found on PATH ({}):", stale_lines.len()));
        for line in &stale_lines {
            out::warn(&format!("  - {line}"));
        }
        out::warn("These do NOT resolve to the current loom-daemon binary. Loom's Python package");
        out::warn(
            "was retired (epic #4081 Phase 4, #4557), so nothing regenerates them — they are",
        );
        out::warn("frozen and will shadow the real binary's entry points (incident #4079).");
        out::warn("Remove them, e.g.:  rm <path>    (or 'pipx uninstall loom-tools')");
        out::warn(&format!(
            "Or run:  {} --prune-stale-entry-points   (removes exactly the stale Python console scripts above, #5139).",
            super::argv0_basename()
        ));
        out::warn("Suppress this check with LOOM_SKIP_STALE_ENTRY_POINT_CHECK=1.");
    }

    // A second, distinct hazard: more than one `loom-daemon` on PATH. The
    // first wins for every caller that resolves by name, so later ones are
    // shadowed — exactly the ambiguity #4079 made costly.
    if daemon_hits.len() > 1 {
        out::warn("Multiple 'loom-daemon' binaries on PATH — the FIRST shadows the rest:");
        for hit in &daemon_hits {
            out::warn(&format!("  - {} ({})", hit.display(), first_version_line(hit)));
        }
        out::warn(&format!(
            "Callers resolving 'loom-daemon' by name get {}. Remove the others",
            daemon_hits[0].display()
        ));
        out::warn("or pin LOOM_DAEMON_BIN explicitly.");
    }
}

/// `$("$hit" --version 2>/dev/null | head -n1 || echo 'version unreadable')`.
///
/// Under `set -o pipefail` the pipeline's status is the daemon's, not `head`'s,
/// so the `||` fires whenever `--version` itself failed — and it fires
/// *alongside* whatever `head` already printed rather than instead of it. Both
/// halves are reproduced; a "just print the first line" simplification would
/// lose the fallback text on a binary that exits non-zero.
fn first_version_line(bin: &Path) -> String {
    let output = Command::new(bin)
        .arg("--version")
        .stderr(Stdio::null())
        .output();
    let (ok, first) = match output {
        Ok(o) => (
            o.status.success(),
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .next()
                .unwrap_or_default()
                .to_string(),
        ),
        Err(_) => (false, String::new()),
    };
    if ok {
        return first;
    }
    if first.is_empty() {
        "version unreadable".to_string()
    } else {
        format!("{first}\nversion unreadable")
    }
}

/// `prune_stale_entry_points <resolved_daemon_bin>` — `true` for both the
/// successful and the nothing-to-do cases, `false` when any `rm` failed
/// (permissions, a race), which the caller turns into exit 1.
///
/// A plain `bool` rather than `Result<(), ()>`: the shell returned a bare exit
/// status, every failure is reported before this returns, and there is no
/// error value for a `Result` to carry.
#[must_use]
pub fn prune_stale(resolved: Option<&Path>) -> bool {
    if util::env_truthy("LOOM_SKIP_STALE_ENTRY_POINT_CHECK") {
        out::say(
            "LOOM_SKIP_STALE_ENTRY_POINT_CHECK is set — leaving all 'loom-*' PATH entries untouched.",
        );
        return true;
    }
    // Not consulted for classification (a shim is skipped outright, whatever
    // it points at), but resolved for parity with warn_stale.
    let _resolved_real = resolved.map(util::realpath).unwrap_or_default();

    let mut to_remove: Vec<PathBuf> = Vec::new();
    for entry in scan_path() {
        if entry.name == "loom-daemon" {
            continue;
        }
        if ALLOWLIST.contains(&entry.name.as_str()) {
            continue;
        }
        // ANY shim — current OR stale — is never a prune candidate.
        if shim_target(&entry.path).is_some() {
            continue;
        }
        if describe(&entry.path) == PYTHON_CONSOLE_SCRIPT {
            to_remove.push(entry.path);
        }
    }

    if to_remove.is_empty() {
        out::ok("No stale Python console-script entry points found on PATH — nothing to prune.");
        return true;
    }

    out::say(&format!(
        "Pruning {} stale Python console-script entry point(s):",
        to_remove.len()
    ));
    let mut failures = 0usize;
    for path in &to_remove {
        // `rm -f` succeeds on an already-absent path.
        let removed = match std::fs::remove_file(path) {
            Ok(()) => true,
            Err(e) => e.kind() == std::io::ErrorKind::NotFound,
        };
        if removed {
            out::ok(&format!("  removed: {}", path.display()));
        } else {
            out::err(&format!("  FAILED to remove: {}", path.display()));
            failures += 1;
        }
    }
    if failures > 0 {
        out::err(&format!(
            "Pruning finished with {failures} failure(s) above — check permissions on those paths."
        ));
        return false;
    }
    out::ok(&format!(
        "Pruned {} stale entry point(s). Re-run with --check (or the check on the next update) to confirm the advisory is now silent.",
        to_remove.len()
    ));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shim_pattern_requires_a_space_after_the_binary() {
        // `'exec .*/loom-daemon"\? '` — the trailing space is in the pattern.
        assert!(line_execs_loom_daemon(r#"exec "$(dirname "$0")/loom-daemon" clean "$@""#));
        assert!(line_execs_loom_daemon("exec /usr/bin/loom-daemon status"));
        assert!(!line_execs_loom_daemon("exec /usr/bin/loom-daemon"));
        assert!(!line_execs_loom_daemon("# mentions /loom-daemon but no exec"));
    }

    #[test]
    fn classification_keys_off_the_shebang_line_only() {
        let tmp = std::env::temp_dir().join(format!("loom-update-lde-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let py = tmp.join("loom-old");
        std::fs::write(&py, "#!/usr/bin/python3\nprint('x')\n").unwrap();
        assert_eq!(describe(&py), PYTHON_CONSOLE_SCRIPT);

        let sh = tmp.join("loom-other");
        std::fs::write(&sh, "#!/usr/bin/env bash\necho hi\n").unwrap();
        assert_eq!(describe(&sh), "script, not a loom-daemon shim");

        let plain = tmp.join("loom-plain");
        std::fs::write(&plain, "not a script at all\n").unwrap();
        assert_eq!(describe(&plain), "not a loom-daemon PATH shim");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_binary_file_is_never_a_shim() {
        let tmp = std::env::temp_dir().join(format!("loom-update-bin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let bin = tmp.join("loom-compiled");
        std::fs::write(&bin, [0x7f, b'E', b'L', b'F', 0, 0, 0, 0]).unwrap();
        assert!(shim_target(&bin).is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
