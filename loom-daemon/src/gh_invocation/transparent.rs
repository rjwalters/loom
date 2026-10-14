//! Transparent `gh` passthrough for the agent `gh` front (#10331).
//!
//! [`super::GhInvocation`] is the choke point for the daemon's *own* `gh`
//! calls: it chooses a credential (`GH_CONFIG_DIR` per root/owner), sets
//! `GH_REPO` from `LOOM_REPO`, and exports a trace context. The agent front
//! ([`crate::agent_gh`]) is a different thing: it stands in for the agent's
//! own `gh`, so a call it does not serve from the ETag cache must reach the
//! next `gh` **exactly** as the agent issued it — same argv, stdin, stdout,
//! stderr, exit status and credential. Applying the facade's env plan there
//! would silently change which identity an agent's mutation runs under.
//!
//! So this is the one sanctioned exec of that shape, kept inside the exempt
//! `gh_invocation/` directory so the choke-point scan in
//! `tests/gh_spawn_choke_point.rs` needs no allowlist entry. The only
//! environment it adds is the caller's (the front's recursion sentinel, and
//! for a classified read the gateway profile, 2am#4367 P4).

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::Path;
use std::process::Command;

/// Replace this process with `program args…` (unix `exec`), so the call keeps
/// its TTY, signals, streams and exit status byte-for-byte. Only returns on
/// failure.
#[cfg(unix)]
pub fn exec<A: AsRef<OsStr>>(program: &Path, args: &[A], env: &[(&str, OsString)]) -> io::Error {
    exec_with(program, args, env, &[])
}

/// [`exec`] that also removes `remove` from the child's environment — for
/// the one credential swap the front makes (2am#4367 P4: a classified read
/// on a gateway profile, with every env token removed so it cannot outrank
/// the profile).
#[cfg(unix)]
pub fn exec_with<A: AsRef<OsStr>>(
    program: &Path,
    args: &[A],
    env: &[(&str, OsString)],
    remove: &[&str],
) -> io::Error {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(program);
    cmd.args(args);
    for k in remove {
        cmd.env_remove(k);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.exec()
}

/// Non-unix: spawn with inherited stdio, wait, and exit with the child's code.
#[cfg(not(unix))]
pub fn exec<A: AsRef<OsStr>>(program: &Path, args: &[A], env: &[(&str, OsString)]) -> io::Error {
    exec_with(program, args, env, &[])
}

/// Non-unix [`exec_with`].
#[cfg(not(unix))]
pub fn exec_with<A: AsRef<OsStr>>(
    program: &Path,
    args: &[A],
    env: &[(&str, OsString)],
    remove: &[&str],
) -> io::Error {
    let mut cmd = Command::new(program);
    cmd.args(args);
    for k in remove {
        cmd.env_remove(k);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    match cmd.status() {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(e) => e,
    }
}
