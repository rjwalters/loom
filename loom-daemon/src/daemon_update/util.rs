//! The small shell primitives the update script leaned on everywhere:
//! `command -v`, `uname`, its hand-rolled `realpath`, the two regex
//! truthiness tests, and the version/commit/semver extractors.
//!
//! Two of these deliberately shell out rather than answer from the process:
//!
//! * **`uname -s` / `uname -m`.** The script asked the *tool*, and the
//!   retained suite's launchd scenarios install a fake `uname` that answers
//!   `Darwin` so the Darwin-gated branches can be driven on a Linux runner.
//!   Answering from `cfg!(target_os = …)` would read as the obvious
//!   simplification and would silently delete those scenarios' coverage —
//!   they would pass by never entering the branch they exist to test.
//! * **`command -v`.** Resolved against `$PATH` at call time, not against
//!   anything baked in, because every scenario in the retained suite runs
//!   under a `MINIMAL_PATH` with stubbed `launchctl`/`systemctl`/`cargo`/
//!   `gh`/`crontab` in front of it.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// `[[ "$X" =~ ^(1|true|yes)$ ]]` — whole-value and case-sensitive, exactly as
/// bash's `=~` with those anchors. `Yes`, `TRUE` and `1 ` are all false.
#[must_use]
pub fn truthy(value: &str) -> bool {
    matches!(value, "1" | "true" | "yes")
}

/// `[[ "$X" =~ ^(0|false|no)$ ]]`.
#[must_use]
pub fn falsy(value: &str) -> bool {
    matches!(value, "0" | "false" | "no")
}

/// `${VAR:-}` run through [`truthy`].
#[must_use]
pub fn env_truthy(name: &str) -> bool {
    truthy(&std::env::var(name).unwrap_or_default())
}

/// `${VAR:-}` run through [`falsy`].
#[must_use]
pub fn env_falsy(name: &str) -> bool {
    falsy(&std::env::var(name).unwrap_or_default())
}

/// `${VAR:-}`, with an empty value treated as absent — the `${VAR:-default}`
/// shape, not `${VAR-default}`.
#[must_use]
pub fn env_non_empty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.is_empty())
}

/// `command -v <name> >/dev/null 2>&1`.
#[must_use]
pub fn have(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| {
            let candidate = dir.join(name);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::metadata(&candidate)
                    .is_ok_and(|m| !m.is_dir() && m.permissions().mode() & 0o111 != 0)
            }
            #[cfg(not(unix))]
            {
                candidate.is_file()
            }
        })
    })
}

/// `uname -s`, or `unknown` when the tool cannot be run — the script's own
/// `|| echo unknown` fallback.
#[must_use]
pub fn uname_s() -> String {
    uname_flag("-s")
}

/// `uname -m`, or `unknown`.
#[must_use]
pub fn uname_m() -> String {
    uname_flag("-m")
}

fn uname_flag(flag: &str) -> String {
    Command::new("uname")
        .arg(flag)
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// `_lde_realpath <target>` — the script's hand-rolled, macOS-safe realpath.
///
/// Empty string for a path that does not exist (the shell's `[[ -e ]]` guard),
/// so a supervisor config still pointing at a deleted binary resolves to `""`
/// and callers fall back to the raw spelling rather than comparing `"" == ""`.
///
/// The symlink walk is bounded at 32 hops exactly as the shell bounded it, and
/// only the *directory* is canonicalised (`cd "$dir" && pwd -P`) — the final
/// component is appended verbatim. That matters: `std::fs::canonicalize`
/// resolves the leaf too, so a path whose leaf is itself a symlink would
/// canonicalise one hop further than the shell did.
#[must_use]
pub fn realpath(target: &Path) -> String {
    if !target.exists() && std::fs::symlink_metadata(target).is_err() {
        return String::new();
    }
    let mut cur = target.to_path_buf();
    let mut depth = 0;
    while depth < 32 {
        let Ok(meta) = std::fs::symlink_metadata(&cur) else {
            break;
        };
        if !meta.file_type().is_symlink() {
            break;
        }
        let Ok(link) = std::fs::read_link(&cur) else {
            break;
        };
        cur = if link.is_absolute() {
            link
        } else {
            cur.parent().unwrap_or(Path::new(".")).join(link)
        };
        depth += 1;
    }
    let dir = cur.parent().unwrap_or(Path::new("."));
    let base = cur
        .file_name()
        .map(|b| b.to_string_lossy().to_string())
        .unwrap_or_default();
    match std::fs::canonicalize(dir) {
        Ok(canon) => format!("{}/{base}", canon.display()),
        Err(_) => cur.display().to_string(),
    }
}

/// [`realpath`] over a `&str`, with the empty string passing straight through.
#[must_use]
pub fn realpath_str(target: &str) -> String {
    if target.is_empty() {
        return String::new();
    }
    realpath(Path::new(target))
}

/// `extract_commit` — the first `commit <hex>` in `loom-daemon --version`
/// output, or `""`.
#[must_use]
pub fn extract_commit(version_output: &str) -> String {
    crate::release_resolve::semver::extract_commit(version_output).unwrap_or_default()
}

/// `extract_version` — the first `N.N.N` in `loom-daemon --version` output (or
/// in a release tag), or `""`.
#[must_use]
pub fn extract_version(version_output: &str) -> String {
    crate::release_resolve::semver::extract_version(version_output).unwrap_or_default()
}

/// `semver_compare <a> <b>` — `-1`, `0` or `1`.
///
/// Deliberately NOT delegated to [`crate::release_resolve::semver::compare`].
/// The two agree on every well-formed version and disagree on one malformed
/// shape: the shell stripped **every** non-digit from a component
/// (`${ai//[!0-9]/}`, so `1a2` is `12`), while `release_resolve` takes only the
/// leading digit run (`1a2` is `1`). This is the update script's own rule, and
/// widening `release_resolve`'s to match — or narrowing this one — would be a
/// behaviour change in a path neither issue asked for.
#[must_use]
pub fn semver_compare(a: &str, b: &str) -> std::cmp::Ordering {
    let parts = |v: &str| -> [u128; 3] {
        let mut out = [0u128; 3];
        for (i, seg) in v.split('.').take(3).enumerate() {
            let digits: String = seg.chars().filter(char::is_ascii_digit).collect();
            out[i] = digits.parse().unwrap_or(0);
        }
        out
    };
    parts(a).cmp(&parts(b))
}

/// `$("$bin" --version 2>/dev/null || true)` — the output with TRAILING
/// NEWLINES stripped, or `""`.
///
/// Command substitution strips trailing newlines and nothing else, so the
/// strip here is `trim_end_matches('\n')`, not `trim()`. That distinction is
/// load-bearing exactly once: `verify_destination_artifact` compares this
/// string for equality against the fetched artifact's own `--version` output,
/// and a `trim()` would silently paper over a trailing-space difference the
/// shell would have reported as a failed roll.
///
/// The `|| true` means a non-zero `--version` yields `""` rather than
/// aborting; every caller treats `""` as "could not determine".
#[must_use]
pub fn version_output(bin: &Path) -> String {
    Command::new(bin)
        .arg("--version")
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim_end_matches('\n')
                .to_string()
        })
        .unwrap_or_default()
}

/// `[[ -x "$path" ]]` — exists, is not a directory, and has an execute bit.
#[must_use]
pub fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).is_ok_and(|m| !m.is_dir() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// `kill -0 <pid> 2>/dev/null` — is this pid alive and signalable by us?
#[must_use]
pub fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Run a command, returning `(exit code, stdout, stderr)` with stderr captured.
///
/// `None` for the code when the binary could not be spawned at all, which the
/// shell saw as `127` from its own `$?` but distinguishes usefully here.
pub fn capture(program: &Path, args: &[String]) -> Option<(i32, String, String)> {
    let out = Command::new(program).args(args).output().ok()?;
    Some((
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    ))
}

/// `git -C <dir> <args>` — stdout trimmed, or `None` when git failed.
pub fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

/// `git -C <dir> <args>` run only for its exit status, output discarded.
pub fn git_ok(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// `$HOME`, or an empty path when it is unset (the shell's own `$HOME`
/// expansion under `set -u` would have aborted; every caller here treats an
/// empty home as "no such file", which is the same observable outcome).
#[must_use]
pub fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truthiness_is_whole_value_and_case_sensitive() {
        for v in ["1", "true", "yes"] {
            assert!(truthy(v), "{v}");
        }
        for v in ["TRUE", "Yes", "1 ", "on", ""] {
            assert!(!truthy(v), "{v}");
        }
        for v in ["0", "false", "no"] {
            assert!(falsy(v), "{v}");
        }
        for v in ["FALSE", "No", "0 ", "off", ""] {
            assert!(!falsy(v), "{v}");
        }
    }

    #[test]
    fn semver_compare_strips_every_non_digit_not_just_the_tail() {
        use std::cmp::Ordering;
        // The shell's `${ai//[!0-9]/}` is a GLOBAL delete, so an embedded
        // letter concatenates the digits around it: `1a2` becomes `12`, not
        // `1`. That is the whole point of this case — `release_resolve`'s
        // `take_while(is_ascii_digit)` stops at the letter and answers `1`,
        // which would sort these the OTHER way (Less). Greater is the retired
        // shell's answer, and matching it is the requirement.
        assert_eq!(semver_compare("0.1a2.0", "0.11.0"), Ordering::Greater);
        assert_eq!(semver_compare("0.19.5", "0.19.10"), Ordering::Less);
        assert_eq!(semver_compare("1.0.0", "1.0"), Ordering::Equal);
        assert_eq!(semver_compare("", "0.0.0"), Ordering::Equal);
        assert_eq!(semver_compare("2.0.0", "1.9.9"), Ordering::Greater);
    }

    #[test]
    fn extract_commit_and_version_read_the_scripts_own_banner_shape() {
        let line = "loom-daemon 0.15.0 (commit ab12cd3, built 2026-07-26T12:00:00Z)";
        assert_eq!(extract_commit(line), "ab12cd3");
        assert_eq!(extract_version(line), "0.15.0");
        assert_eq!(extract_commit("no commit here"), "");
        assert_eq!(extract_version("no version here"), "");
    }

    #[test]
    fn realpath_of_a_nonexistent_path_is_empty_not_the_input() {
        assert_eq!(realpath(Path::new("/definitely/not/here/at/all")), "");
        assert_eq!(realpath_str(""), "");
    }
}
