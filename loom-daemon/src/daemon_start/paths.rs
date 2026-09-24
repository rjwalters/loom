//! Repo-root discovery, daemon-binary location and the deterministic plist
//! `PATH` — the three resolutions everything downstream hangs off.

use std::path::{Path, PathBuf};

use super::out;

/// `find_repo_root()` — walk up from `$PWD` looking for a `.loom` directory,
/// following a linked worktree's `.git` **file** to its main checkout.
///
/// Returns `None` where the shell echoed the empty string.
///
/// `$PWD` is read from the environment before falling back to
/// [`std::env::current_dir`]: bash exports the *logical* cwd (symlinks
/// unresolved) and the stub hands it straight to us, whereas `current_dir`
/// returns the resolved physical path. On a host where the working directory is
/// reached through a symlink the two differ, and the resolved one would place
/// the pid file, flags file and rendered `WorkingDirectory=` under a path the
/// operator never typed. The environment value is only trusted when it names
/// the same directory we are actually in.
#[must_use]
pub fn find_repo_root() -> Option<PathBuf> {
    let start = logical_cwd()?;
    let mut dir = start.as_path();
    while dir != Path::new("/") {
        if dir.join(".loom").is_dir() {
            return Some(dir.to_path_buf());
        }
        let dot_git = dir.join(".git");
        if dot_git.is_file() {
            if let Ok(text) = std::fs::read_to_string(&dot_git) {
                // `sed 's/^gitdir: //'` is per-line and unanchored at the end;
                // `$( )` then strips the trailing newline. A single-line
                // `.git` file — the only shape git writes — reduces to this.
                let gitdir = text
                    .lines()
                    .map(|l| l.strip_prefix("gitdir: ").unwrap_or(l))
                    .collect::<Vec<_>>()
                    .join("\n");
                let main_repo = PathBuf::from(&gitdir)
                    .parent()
                    .and_then(Path::parent)
                    .and_then(Path::parent)
                    .map(Path::to_path_buf);
                if let Some(main_repo) = main_repo {
                    if main_repo.join(".loom").is_dir() {
                        return Some(main_repo);
                    }
                }
            }
        }
        dir = dir.parent()?;
    }
    None
}

fn logical_cwd() -> Option<PathBuf> {
    let physical = std::env::current_dir().ok()?;
    if let Some(pwd) = std::env::var_os("PWD") {
        let logical = PathBuf::from(pwd);
        if logical.is_absolute() && same_dir(&logical, &physical) {
            return Some(logical);
        }
    }
    Some(physical)
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(ma), Ok(mb)) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                ma.dev() == mb.dev() && ma.ino() == mb.ino()
            }
            #[cfg(not(unix))]
            {
                let _ = (ma, mb);
                a == b
            }
        }
        _ => false,
    }
}

fn executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).is_ok_and(|m| !m.is_dir() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

fn home() -> String {
    std::env::var("HOME").unwrap_or_default()
}

fn non_empty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.is_empty())
}

/// `_loom_daemon_repo_candidates <root>` — the ordered repo-local build paths.
///
/// The `cargo metadata` tier is the only one that can see a
/// `~/.cargo/config.toml` `build.target-dir` redirect, which is exactly the
/// arrangement on this fleet, so it is reproduced rather than dropped as an
/// optimisation — and, as in the shell, only when `$CARGO_TARGET_DIR` is unset
/// and a crate manifest exists to key it off.
#[must_use]
pub fn repo_candidates(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let cargo_target_dir = non_empty("CARGO_TARGET_DIR");
    if let Some(dir) = &cargo_target_dir {
        out.push(PathBuf::from(dir).join("release/loom-daemon"));
        out.push(PathBuf::from(dir).join("debug/loom-daemon"));
    }
    out.push(root.join("loom-daemon/target/release/loom-daemon"));
    out.push(root.join("loom-daemon/target/debug/loom-daemon"));
    out.push(root.join("target/release/loom-daemon"));
    out.push(root.join("target/debug/loom-daemon"));

    let manifest = root.join("loom-daemon/Cargo.toml");
    if cargo_target_dir.is_none() && manifest.is_file() {
        if let Some(meta_dir) = cargo_metadata_target_dir(&manifest) {
            out.push(PathBuf::from(&meta_dir).join("release/loom-daemon"));
            out.push(PathBuf::from(&meta_dir).join("debug/loom-daemon"));
        }
    }
    out
}

fn cargo_metadata_target_dir(manifest: &Path) -> Option<String> {
    let output = std::process::Command::new("cargo")
        .args([
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--manifest-path",
        ])
        .arg(manifest)
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    // `grep -o '"target_directory":"[^"]*"' | head -n1 | sed …` — a textual
    // scan, not a JSON parse, and deliberately kept textual: a JSON parse would
    // succeed on shapes the shell's grep missed and vice versa.
    let text = String::from_utf8_lossy(&output.stdout);
    let key = "\"target_directory\":\"";
    let start = text.find(key)? + key.len();
    let rest = &text[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// `loom_locate_daemon_bin <root>` — the precedence 21 scripts share.
///
/// Logs its choice to stderr (unless `LOOM_LOCATE_DAEMON_BIN_QUIET=1`), which
/// is deliberate: "which binary ran" is the diagnostic that turns "this
/// assertion failed" into "this assertion failed against a binary from 02:10".
#[must_use]
pub fn locate_daemon_bin(root: &Path) -> Option<PathBuf> {
    let mut resolved: Option<(PathBuf, String)> = None;

    if let Some(explicit) = non_empty("LOOM_DAEMON_BIN") {
        let p = PathBuf::from(&explicit);
        if executable(&p) {
            resolved = Some((p, "$LOOM_DAEMON_BIN".to_string()));
        }
    }

    if resolved.is_none() && std::env::var("LOOM_PREFER_REPO_BUILD").as_deref() == Ok("1") {
        if let Some(c) = repo_candidates(root).into_iter().find(|c| executable(c)) {
            resolved = Some((c, "repo-local build ($LOOM_PREFER_REPO_BUILD=1)".to_string()));
        }
    }

    if resolved.is_none() {
        if let Some(p) = on_path("loom-daemon") {
            resolved = Some((p, "$PATH".to_string()));
        }
    }

    if resolved.is_none() {
        let dir =
            non_empty("LOOM_DAEMON_BIN_DIR").unwrap_or_else(|| format!("{}/.local/bin", home()));
        let machine_bin = PathBuf::from(dir).join("loom-daemon");
        if executable(&machine_bin) {
            resolved = Some((
                machine_bin,
                "machine-level install (${LOOM_DAEMON_BIN_DIR:-$HOME/.local/bin})".to_string(),
            ));
        }
    }

    if resolved.is_none() {
        if let Some(c) = repo_candidates(root).into_iter().find(|c| executable(c)) {
            resolved = Some((c, "repo-local build".to_string()));
        }
    }

    if let Some((path, via)) = &resolved {
        if std::env::var("LOOM_LOCATE_DAEMON_BIN_QUIET").as_deref() != Ok("1") {
            out::say_err(&format!(
                "loom_locate_daemon_bin: resolved {} via {via} (mtime: {})",
                path.display(),
                mtime_human(path)
            ));
        }
    }
    resolved.map(|(p, _)| p)
}

fn on_path(name: &str) -> Option<PathBuf> {
    // `command -v` resolves against $PATH and requires an executable file.
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(name))
            .find(|c| executable(c))
    })
}

/// `_loom_daemon_bin_mtime_human` — best-effort local-time mtime, `unknown`
/// when it cannot be read.
fn mtime_human(path: &Path) -> String {
    let Ok(meta) = std::fs::metadata(path) else {
        return "unknown".to_string();
    };
    let Ok(modified) = meta.modified() else {
        return "unknown".to_string();
    };
    let dt: chrono::DateTime<chrono::Local> = modified.into();
    dt.format("%Y-%m-%d %H:%M:%S").to_string()
}

/// `loom_daemon_bin_search_paths <root>` — the candidate list printed when
/// nothing resolved. Mirrors [`locate_daemon_bin`]'s precedence.
#[must_use]
pub fn bin_search_paths(root: &Path) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(explicit) = non_empty("LOOM_DAEMON_BIN") {
        lines.push(format!("$LOOM_DAEMON_BIN={explicit}"));
    }
    if std::env::var("LOOM_PREFER_REPO_BUILD").as_deref() == Ok("1") {
        for c in repo_candidates(root) {
            lines.push(format!("{} ($LOOM_PREFER_REPO_BUILD=1)", c.display()));
        }
    }
    lines.push("loom-daemon on $PATH".to_string());
    let dir = non_empty("LOOM_DAEMON_BIN_DIR").unwrap_or_else(|| format!("{}/.local/bin", home()));
    lines.push(format!("{dir}/loom-daemon"));
    for c in repo_candidates(root) {
        lines.push(c.display().to_string());
    }
    lines
}

/// `canonical_daemon_path()` (`lib/canonical-daemon-path.sh`).
///
/// [`crate::fleet::path_bootstrap::CANONICAL_PATH_DIRS`] is the same list and
/// is already pinned against the shell library by a unit test there, so this
/// renders from it rather than declaring a fourth copy of the set the #4831
/// incident was about.
#[must_use]
pub fn canonical_daemon_path() -> String {
    let h = home();
    crate::fleet::path_bootstrap::CANONICAL_PATH_DIRS
        .iter()
        .map(|d| d.replace("${HOME}", &h))
        .collect::<Vec<_>>()
        .join(":")
}

/// `resolve_plist_path()` (#4172) — the deterministic `PATH` baked into every
/// rendered plist and unit, plus its one stderr line.
///
/// The line goes to **stderr** so `--print-plist`'s stdout stays pipeable and
/// diffable; moving it to stdout would corrupt every `--print-plist > file`
/// caller, including this suite's own fixture installs.
#[must_use]
pub fn resolve_plist_path() -> String {
    let canonical = canonical_daemon_path();
    if let Some(full) = non_empty("LOOM_DAEMON_PATH") {
        out::say_err(&format!("Rendered plist PATH: full override via LOOM_DAEMON_PATH -> {full}"));
        return full;
    }
    if let Some(extra) = non_empty("LOOM_DAEMON_PATH_EXTRA") {
        let joined = format!("{extra}:{canonical}");
        out::say_err(&format!(
            "Rendered plist PATH: canonical minimal PATH + LOOM_DAEMON_PATH_EXTRA -> {joined}"
        ));
        return joined;
    }
    out::say_err(&format!(
        "Rendered plist PATH: canonical minimal PATH (deterministic default) -> {canonical}"
    ));
    canonical
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_candidates_order_matches_the_shell_generator() {
        let root = PathBuf::from("/repo");
        let names: Vec<String> = repo_candidates(&root)
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        let idx = |s: &str| names.iter().position(|n| n == s).expect(s);
        assert!(
            idx("/repo/loom-daemon/target/release/loom-daemon")
                < idx("/repo/loom-daemon/target/debug/loom-daemon")
        );
        assert!(
            idx("/repo/loom-daemon/target/debug/loom-daemon")
                < idx("/repo/target/release/loom-daemon")
        );
        assert!(idx("/repo/target/release/loom-daemon") < idx("/repo/target/debug/loom-daemon"));
    }

    #[test]
    fn a_non_executable_candidate_is_rejected() {
        // The shell guarded with `-x`, not `-e`.
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("loom-daemon");
        std::fs::write(&f, b"text").expect("write");
        assert!(!executable(&f));
    }
}
