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
//!
//! The advisory's remediation text follows the same line (#11069): it
//! recommends `rm` only for the Python console scripts the prune would
//! remove, and says what every other flagged entry is instead. Provisioning's
//! own files beside the resolved binary — the rollback copy `<dest>.previous`
//! and the record `<dest>.install-state.json` (#10983) — are never flagged at
//! all: deleting the rollback copy is exactly what an operator following an
//! `rm` hint would have done.

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
        // #11069: say what it is. This used to read only "not a loom-daemon
        // PATH shim", which next to an `rm` hint read as "delete me".
        "executable that is neither a loom-daemon PATH shim nor a Python console script"
    }
}

/// One `loom-*` executable found on `$PATH`, already classified.
struct Entry {
    path: PathBuf,
    name: String,
}

/// `dir` canonicalised with the leaf name appended verbatim — the identity
/// two spellings of one directory entry share. The leaf is NOT followed: a
/// `loom-*` symlink that happens to point at the rollback copy is its own
/// entry, not the rollback copy.
fn entry_location(dir: &Path, name: &std::ffi::OsStr) -> Option<PathBuf> {
    std::fs::canonicalize(dir).ok().map(|d| d.join(name))
}

/// The files provisioning keeps beside the resolved daemon binary (#10983),
/// as [`entry_location`]s: `<dest>.previous` (the rollback copy) and
/// `<dest>.install-state.json` (the install record). Both names come from
/// [`txn`](super::provision::txn) itself, so a rename there cannot leave this
/// scan flagging the renamed file.
///
/// Derived from the resolved path as given AND from its realpath, because the
/// binary may be reached through a symlink while provisioning wrote beside
/// the real file.
///
/// Provisioning's temp files need no entry here: every one is named
/// `.<base>.loom-install.<pid>.<n>.<nanos>` (`create_staging_file`, also used
/// for the `.previous` and record writes), and that leading dot means the
/// `loom-*` scan never sees them. The `staging_files_are_never_scanned` test
/// pins this against a file `provision::stage` really creates.
fn provisioning_artifacts(resolved: Option<&Path>) -> Vec<PathBuf> {
    use super::provision::txn;
    let Some(resolved) = resolved.filter(|p| !p.as_os_str().is_empty()) else {
        return Vec::new();
    };
    let mut dests = vec![resolved.to_path_buf()];
    let real = util::realpath(resolved);
    if !real.is_empty() && Path::new(&real) != resolved {
        dests.push(PathBuf::from(real));
    }
    let mut out = Vec::new();
    for dest in &dests {
        for artifact in [txn::previous_path(dest), txn::record_path(dest)] {
            let (Some(dir), Some(name)) = (artifact.parent(), artifact.file_name()) else {
                continue;
            };
            let dir = if dir.as_os_str().is_empty() {
                Path::new(".")
            } else {
                dir
            };
            if let Some(loc) = entry_location(dir, name) {
                if !out.contains(&loc) {
                    out.push(loc);
                }
            }
        }
    }
    out
}

/// Walk a `$PATH` value exactly as the shell did: an empty element means `.`,
/// non-directories are skipped, and repeated entries are deduped by their
/// resolved path so one file is never reported twice. Provisioning's own
/// files beside `resolved` ([`provisioning_artifacts`]) are skipped.
fn scan_path(raw: &str, resolved: Option<&Path>) -> Vec<Entry> {
    let artifacts = provisioning_artifacts(resolved);
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
            if !artifacts.is_empty()
                && entry_location(dir_path, std::ffi::OsStr::new(&name))
                    .is_some_and(|loc| artifacts.contains(&loc))
            {
                continue; // provisioning's rollback copy or record (#11069)
            }
            found.push(Entry { path, name });
        }
    }
    found
}

/// What a flagged entry is — which decides the remediation the advisory may
/// offer for it (#11069).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StaleKind {
    /// Exactly what the prune removes; the only kind `rm` is suggested for.
    PythonConsoleScript,
    /// An auto-generated shim that execs some OTHER `loom-daemon`. Needs
    /// re-provisioning, not deletion; never pruned.
    StaleShim,
    /// Anything else named `loom-*`: not Loom's to remove.
    Other,
}

/// One flagged entry: its warning line and its kind.
struct Stale {
    line: String,
    kind: StaleKind,
}

/// The advisory's findings for one `$PATH` value: the flagged entries, and
/// every `loom-daemon` on PATH in order.
fn find_stale(resolved: Option<&Path>, raw_path: &str) -> (Vec<Stale>, Vec<PathBuf>) {
    let resolved_real = resolved.map(util::realpath).unwrap_or_default();
    let resolved_display = resolved
        .map(|p| p.display().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "<none>".to_string());

    let mut stale: Vec<Stale> = Vec::new();
    let mut daemon_hits: Vec<PathBuf> = Vec::new();

    for entry in scan_path(raw_path, resolved) {
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
                stale.push(Stale {
                    line: format!(
                        "{} — PATH shim execs {}, which is NOT the resolved binary ({resolved_display})",
                        entry.path.display(),
                        target.display()
                    ),
                    kind: StaleKind::StaleShim,
                });
                continue;
            }
        }
        let what = describe(&entry.path);
        let kind = if what == PYTHON_CONSOLE_SCRIPT {
            StaleKind::PythonConsoleScript
        } else {
            StaleKind::Other
        };
        stale.push(Stale {
            line: format!("{} — {what}", entry.path.display()),
            kind,
        });
    }
    (stale, daemon_hits)
}

/// The advisory block for `stale` (empty when nothing was flagged).
///
/// `rm` is suggested ONLY when at least one entry is a Python console script,
/// and only for those: the prune removes nothing else, so a hint to delete a
/// shim or an unrelated binary would ask the operator to do by hand what the
/// tool deliberately refuses to (#11069 — the rollback copy was one such).
fn advisory_lines(stale: &[Stale], argv0: &str) -> Vec<String> {
    if stale.is_empty() {
        return Vec::new();
    }
    let count = |k: StaleKind| stale.iter().filter(|s| s.kind == k).count();
    let mut lines = vec![format!(
        "Stale 'loom-*' entry points found on PATH ({}):",
        stale.len()
    )];
    lines.extend(stale.iter().map(|s| format!("  - {}", s.line)));
    lines.push(
        "These do NOT resolve to the current loom-daemon binary, and can shadow its entry points (incident #4079)."
            .to_string(),
    );
    if count(StaleKind::PythonConsoleScript) > 0 {
        lines.push(
            "The Python console scripts are frozen leftovers of Loom's retired Python package (epic #4081 Phase 4, #4557); nothing regenerates them."
                .to_string(),
        );
        lines.push(
            "Remove the Python console scripts, e.g.:  rm <path>    (or 'pipx uninstall loom-tools')"
                .to_string(),
        );
        lines.push(format!(
            "Or run:  {argv0} --prune-stale-entry-points   (removes exactly the stale Python console scripts above, #5139)."
        ));
    }
    if count(StaleKind::StaleShim) > 0 {
        lines.push(
            "A stale PATH shim needs re-provisioning, not deletion; --prune-stale-entry-points leaves it alone."
                .to_string(),
        );
    }
    if count(StaleKind::Other) > 0 {
        lines.push(
            "Entries that are not Python console scripts or shims were not installed as Loom entry points, and --prune-stale-entry-points leaves them alone."
                .to_string(),
        );
        lines.push("Find out what each one is before removing anything.".to_string());
    }
    lines.push("Suppress this check with LOOM_SKIP_STALE_ENTRY_POINT_CHECK=1.".to_string());
    lines
}

/// `warn_stale_entry_points <resolved_daemon_bin>` — advisory only.
pub fn warn_stale(resolved: Option<&Path>) {
    if util::env_truthy("LOOM_SKIP_STALE_ENTRY_POINT_CHECK") {
        return;
    }
    let raw_path = std::env::var("PATH").unwrap_or_default();
    let (stale, daemon_hits) = find_stale(resolved, &raw_path);

    for line in advisory_lines(&stale, &super::argv0_basename()) {
        out::warn(&line);
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
    let raw_path = std::env::var("PATH").unwrap_or_default();
    remove_all(&prune_candidates(resolved, &raw_path))
}

/// What the prune removes for one `$PATH` value: exactly the entries
/// classified as a Python console script. `resolved` is not consulted for
/// classification (a shim is skipped outright, whatever it points at); it
/// only names the provisioning files the scan skips.
fn prune_candidates(resolved: Option<&Path>, raw_path: &str) -> Vec<PathBuf> {
    let mut to_remove: Vec<PathBuf> = Vec::new();
    for entry in scan_path(raw_path, resolved) {
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
    to_remove
}

/// Remove every path in `to_remove`, reporting each; `false` if any failed.
fn remove_all(to_remove: &[PathBuf]) -> bool {
    if to_remove.is_empty() {
        out::ok("No stale Python console-script entry points found on PATH — nothing to prune.");
        return true;
    }

    out::say(&format!(
        "Pruning {} stale Python console-script entry point(s):",
        to_remove.len()
    ));
    let mut failures = 0usize;
    for path in to_remove {
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
        assert_eq!(
            describe(&plain),
            "executable that is neither a loom-daemon PATH shim nor a Python console script"
        );

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

    // ---- #11069: provisioning artifacts and the remediation text ----

    const ELF: &[u8] = &[0x7f, b'E', b'L', b'F', 0, 0, 0, 0];

    fn exe(path: &Path, bytes: &[u8]) {
        std::fs::write(path, bytes).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn bin_dir() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        (tmp, bin)
    }

    fn path_of(dirs: &[&Path]) -> String {
        dirs.iter()
            .map(|d| d.display().to_string())
            .collect::<Vec<_>>()
            .join(":")
    }

    fn advisory(resolved: Option<&Path>, raw_path: &str) -> String {
        let (stale, _) = find_stale(resolved, raw_path);
        advisory_lines(&stale, "loom-daemon-update.sh").join("\n")
    }

    #[test]
    fn the_rollback_copy_and_install_record_are_never_flagged() {
        let (_tmp, bin) = bin_dir();
        let daemon = bin.join("loom-daemon");
        exe(&daemon, ELF);
        // The rollback copy is a byte-for-byte binary, mode 755 (#10983). The
        // record is not executable in practice; it is made so here to prove
        // it is skipped by name, not by mode.
        exe(&bin.join("loom-daemon.previous"), ELF);
        exe(&bin.join("loom-daemon.install-state.json"), b"{}\n");
        let raw = path_of(&[&bin]);

        let (stale, hits) = find_stale(Some(&daemon), &raw);
        assert!(
            stale.is_empty(),
            "flagged: {:?}",
            stale.iter().map(|s| &s.line).collect::<Vec<_>>()
        );
        assert_eq!(hits, vec![daemon.clone()]);
        assert_eq!(advisory(Some(&daemon), &raw), "");
        assert!(prune_candidates(Some(&daemon), &raw).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn the_exclusion_follows_a_symlinked_resolved_binary_to_its_real_directory() {
        let (tmp, bin) = bin_dir();
        let daemon = bin.join("loom-daemon");
        exe(&daemon, ELF);
        exe(&bin.join("loom-daemon.previous"), ELF);
        let link_dir = tmp.path().join("usr-local-bin");
        std::fs::create_dir_all(&link_dir).unwrap();
        let link = link_dir.join("loom-daemon");
        std::os::unix::fs::symlink(&daemon, &link).unwrap();

        let (stale, _) = find_stale(Some(&link), &path_of(&[&bin]));
        assert!(stale.is_empty());
    }

    #[test]
    fn the_exclusion_is_derived_from_the_resolved_binary_not_a_suffix_match() {
        let (tmp, bin) = bin_dir();
        let previous = bin.join("loom-daemon.previous");
        exe(&previous, ELF);
        let raw = path_of(&[&bin]);

        // No resolved binary, or one somewhere else: the same file is just an
        // unrelated `loom-*` executable — still reported, never with `rm`.
        let elsewhere = tmp.path().join("elsewhere/loom-daemon");
        for resolved in [None, Some(elsewhere.as_path())] {
            let text = advisory(resolved, &raw);
            assert!(text.contains(&format!("{} — executable that is neither", previous.display())));
            assert!(!text.contains("rm <path>"), "{text}");
            assert!(prune_candidates(resolved, &raw).is_empty());
        }
    }

    #[test]
    fn staging_files_are_never_scanned() {
        // A real temp file from provisioning's own `stage`, left behind as a
        // killed install would leave it: executable, beside the destination.
        let (tmp, bin) = bin_dir();
        let src = tmp.path().join("candidate");
        exe(&src, ELF);
        let staged = super::super::provision::stage(&src, &bin.join("loom-daemon")).unwrap();
        assert_eq!(staged.parent(), Some(bin.as_path()));
        assert!(util::is_executable(&staged));

        let (stale, hits) = find_stale(None, &path_of(&[&bin]));
        assert!(stale.is_empty());
        assert!(hits.is_empty());
    }

    #[test]
    fn a_stale_python_console_script_still_warns_and_is_still_pruned() {
        let (_tmp, bin) = bin_dir();
        let daemon = bin.join("loom-daemon");
        exe(&daemon, ELF);
        exe(&bin.join("loom-daemon.previous"), ELF);
        let script = bin.join("loom-tokens");
        exe(&script, b"#!/usr/bin/python3\nimport sys\nsys.exit(0)\n");
        let raw = path_of(&[&bin]);

        let text = advisory(Some(&daemon), &raw);
        assert!(
            text.contains(&format!("{} — {PYTHON_CONSOLE_SCRIPT}", script.display())),
            "{text}"
        );
        assert!(text.contains("Remove the Python console scripts, e.g.:  rm <path>"), "{text}");
        assert!(text.contains("loom-daemon-update.sh --prune-stale-entry-points"), "{text}");
        assert!(!text.contains("loom-daemon.previous"), "{text}");

        let candidates = prune_candidates(Some(&daemon), &raw);
        assert_eq!(candidates, vec![script.clone()]);
        assert!(remove_all(&candidates));
        assert!(!script.exists());
        assert!(
            bin.join("loom-daemon.previous").exists(),
            "the prune must keep the rollback copy"
        );
        assert!(daemon.exists());
    }

    #[test]
    fn an_unrelated_binary_is_described_and_never_told_to_be_removed() {
        let (_tmp, bin) = bin_dir();
        let daemon = bin.join("loom-daemon");
        exe(&daemon, ELF);
        let foo = bin.join("loom-foo");
        exe(&foo, ELF);
        let raw = path_of(&[&bin]);

        let text = advisory(Some(&daemon), &raw);
        assert!(
            text.contains(&format!(
                "{} — executable that is neither a loom-daemon PATH shim nor a Python console script",
                foo.display()
            )),
            "{text}"
        );
        assert!(text.contains("--prune-stale-entry-points leaves them alone"), "{text}");
        assert!(text.contains("Find out what each one is before removing anything."), "{text}");
        assert!(!text.contains("rm <path>"), "{text}");
        assert!(!text.contains("pipx uninstall"), "{text}");
        assert!(!text.contains("Or run:"), "{text}");
        assert!(prune_candidates(Some(&daemon), &raw).is_empty());
    }

    #[test]
    fn a_stale_shim_is_sent_to_re_provisioning_not_rm() {
        let (tmp, bin) = bin_dir();
        let daemon = bin.join("loom-daemon");
        exe(&daemon, ELF);
        let old = tmp.path().join("old-bin");
        std::fs::create_dir_all(&old).unwrap();
        exe(&old.join("loom-daemon"), ELF);
        let shim = old.join("loom-clean");
        exe(
            &shim,
            b"#!/usr/bin/env bash\n# Auto-generated PATH shim\nexec \"$(dirname \"$0\")/loom-daemon\" clean \"$@\"\n",
        );
        let raw = path_of(&[&bin, &old]);

        let text = advisory(Some(&daemon), &raw);
        assert!(text.contains("PATH shim execs"), "{text}");
        assert!(text.contains("needs re-provisioning, not deletion"), "{text}");
        assert!(!text.contains("rm <path>"), "{text}");
        assert!(prune_candidates(Some(&daemon), &raw).is_empty());
    }
}
