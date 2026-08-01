//! Shared canonical PATH bootstrap for fleet remote-ops (#4831).
//!
//! Before this module existed there were THREE independently hand-maintained,
//! disagreeing partial PATH definitions across the fleet subsystem:
//!
//! - [`super::drain::GhClaimResetter`]'s local `Command::new("gh")` calls had
//!   NO PATH handling at all — they silently inherited whatever PATH the
//!   `loom-daemon` process itself was launched with (a launchd/systemd
//!   non-interactive daemon may not have `gh`/Homebrew on that inherited
//!   PATH).
//! - [`super::add_worker`]'s provisioning steps hand-rolled ~12 duplicated
//!   `export PATH="$HOME/.local/bin:$PATH"` lines — missing both
//!   `${HOME}/.cargo/bin` and Homebrew's `/opt/homebrew/bin`.
//! - [`super::add_worker`]'s rendered systemd unit hardcoded a THIRD, even
//!   narrower `Environment=PATH=%h/.local/bin:/usr/local/bin:/usr/bin:/bin`
//!   that agreed with neither of the above.
//!
//! [`CANONICAL_PATH_DIRS`] is the one Rust-side definition of the full
//! canonical superset (mirroring `resolve_plist_path()` in
//! `defaults/scripts/cli/loom-daemon-start.sh`, the fullest of the three
//! pre-#4831 definitions) that every fleet call site should render from
//! instead of hand-rolling its own subset.
//!
//! Rust cannot `source` the shell library
//! (`defaults/scripts/lib/canonical-daemon-path.sh`) that shell call sites
//! use, so this module keeps an independently-declared but byte-for-byte
//! equal copy — [`tests::canonical_path_matches_shell_lib`] parses that
//! file's `canonical_daemon_path()` body at test time and fails the build the
//! moment the two drift.

/// The canonical PATH directory list, highest-precedence first, using shell
/// variable syntax (`${HOME}`, matching `resolve_plist_path()`'s own
/// brace-quoted style byte-for-byte — see
/// [`tests::canonical_path_matches_shell_lib`]) for directories under the
/// user's home. Order mirrors `resolve_plist_path()`'s canonical set in
/// `defaults/scripts/cli/loom-daemon-start.sh` exactly: loom's own installed
/// binaries first, then rustup/cargo, then Homebrew, then the standard system
/// dirs.
pub const CANONICAL_PATH_DIRS: &[&str] = &[
    "${HOME}/.local/bin",
    "${HOME}/.cargo/bin",
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "/usr/bin",
    "/bin",
    "/usr/sbin",
    "/sbin",
];

/// [`CANONICAL_PATH_DIRS`] colon-joined, `${HOME}`-relative (for embedding
/// into a rendered *remote* shell script, where `${HOME}` is expanded by the
/// shell that runs it, not by this process).
#[must_use]
pub fn canonical_path_shell() -> String {
    CANONICAL_PATH_DIRS.join(":")
}

/// A full `export PATH="..."` line (with a trailing newline) suitable for
/// splicing verbatim at the top of a rendered fleet-provisioning shell step.
/// Prepends the canonical set onto the step's inherited `$PATH` (rather than
/// replacing it outright) so a provisioning session's ambient PATH is never
/// narrowed, only widened.
#[must_use]
pub fn canonical_path_export_line() -> String {
    format!("export PATH=\"{}:$PATH\"\n", canonical_path_shell())
}

/// [`CANONICAL_PATH_DIRS`] rendered for a systemd unit's static
/// `Environment=PATH=` line, where `%h` (not `${HOME}`) is systemd's
/// home-directory specifier and no `$PATH` suffix is meaningful (systemd
/// does not inherit a shell `$PATH` to append to).
#[must_use]
pub fn canonical_path_systemd() -> String {
    CANONICAL_PATH_DIRS
        .iter()
        .map(|dir| dir.replace("${HOME}", "%h"))
        .collect::<Vec<_>>()
        .join(":")
}

/// [`CANONICAL_PATH_DIRS`] with `${HOME}` expanded to a concrete path — used
/// by [`local_gh_path_env`] to build a real `PATH` env-var value for a
/// `std::process::Command` run locally (never over SSH), where there is no
/// remote shell to expand `${HOME}` for us.
#[must_use]
fn canonical_path_expanded(home: &str) -> String {
    CANONICAL_PATH_DIRS
        .iter()
        .map(|dir| dir.replace("${HOME}", home))
        .collect::<Vec<_>>()
        .join(":")
}

/// The `PATH` value [`super::drain::GhClaimResetter`]'s local `gh`
/// invocations should run with: the canonical set (so `gh`/Homebrew resolve
/// even when the daemon process itself was launched non-interactively with a
/// minimal inherited PATH) prepended onto whatever PATH this process actually
/// inherited (so an operator's deliberately customized PATH is only widened,
/// never narrowed out).
#[must_use]
pub fn local_gh_path_env() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let canonical = canonical_path_expanded(&home);
    match std::env::var("PATH") {
        Ok(inherited) if !inherited.is_empty() => format!("{canonical}:{inherited}"),
        _ => canonical,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shell library's `canonical_daemon_path()` function body (#4831) —
    /// kept in sync with [`CANONICAL_PATH_DIRS`] by
    /// [`canonical_path_matches_shell_lib`] below, since Rust cannot
    /// `source` bash at compile time.
    const SHELL_LIB: &str = include_str!("../../../defaults/scripts/lib/canonical-daemon-path.sh");

    #[test]
    fn canonical_path_matches_shell_lib() {
        // Extract the single `printf '%s' '<value>'` line's <value> out of
        // canonical_daemon_path() and assert it equals CANONICAL_PATH_DIRS
        // joined the same way -- a drift between the Rust and shell
        // definitions (e.g. someone widening one but not the other) fails
        // this test rather than silently shipping a narrower PATH on one
        // side of the language boundary.
        let printf_line = SHELL_LIB
            .lines()
            .find(|line| line.trim_start().starts_with("printf '%s'"))
            .expect("canonical-daemon-path.sh must contain a `printf '%s' ...` line");
        let start = printf_line
            .find('"')
            .expect("printf line must double-quote its PATH value");
        let value = &printf_line[start + 1..];
        let end = value
            .rfind('"')
            .expect("printf line must be closed with a matching quote");
        let shell_value = &value[..end];

        assert_eq!(
            shell_value,
            canonical_path_shell(),
            "loom-daemon/src/fleet/path_bootstrap.rs::CANONICAL_PATH_DIRS has drifted from \
             defaults/scripts/lib/canonical-daemon-path.sh::canonical_daemon_path() -- keep \
             both definitions byte-for-byte equal (#4831)"
        );
    }

    #[test]
    fn export_line_prepends_onto_inherited_path() {
        let line = canonical_path_export_line();
        assert!(line.starts_with("export PATH=\""));
        assert!(line.ends_with("$PATH\"\n"));
        assert!(line.contains("${HOME}/.local/bin"));
        assert!(line.contains("${HOME}/.cargo/bin"));
        assert!(line.contains("/opt/homebrew/bin"));
        assert!(line.contains("/usr/local/bin"));
    }

    #[test]
    fn systemd_path_uses_percent_h_not_dollar_home() {
        let path = canonical_path_systemd();
        assert!(
            !path.contains("${HOME}"),
            "systemd Environment=PATH= must not contain ${{HOME}}: {path}"
        );
        assert!(path.starts_with("%h/.local/bin:%h/.cargo/bin:"));
        assert!(path.contains("/opt/homebrew/bin"));
        assert!(path.contains("/usr/local/bin"));
        assert!(path.contains("/usr/bin"));
        assert!(path.contains("/bin"));
    }

    #[test]
    #[serial_test::serial(env_home_path)]
    fn local_gh_path_env_includes_canonical_and_inherited() {
        // std::env::set_var mutates process-global state -- serialized via
        // #[serial] (mirrors disk_headroom.rs's PATH-mutating tests) and
        // restored afterward so this doesn't race sibling tests that read
        // HOME/PATH.
        let old_home = std::env::var("HOME").ok();
        let old_path = std::env::var("PATH").ok();
        std::env::set_var("HOME", "/tmp/fake-home-4831");
        std::env::set_var("PATH", "/inherited/only/bin");

        let value = local_gh_path_env();

        match old_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        match old_path {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }

        assert!(value.contains("/tmp/fake-home-4831/.local/bin"));
        assert!(value.contains("/tmp/fake-home-4831/.cargo/bin"));
        assert!(value.contains("/opt/homebrew/bin"));
        assert!(value.contains("/inherited/only/bin"));
        // Canonical set must come FIRST so it cannot be shadowed by a
        // narrower inherited PATH.
        assert!(value.find("/tmp/fake-home-4831/.local/bin") < value.find("/inherited/only/bin"));
    }
}
