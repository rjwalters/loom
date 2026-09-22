//! **The binary replacing itself.**
//!
//! The shell wrapper was a different process from the daemon it rolled. This
//! subcommand is not: `daemon-update` runs inside a `loom-daemon` binary, and
//! the file it provisions over is, on a normal fleet host, the very file this
//! process was exec'd from (`~/.local/bin/loom-daemon`). That is solvable on
//! Unix, but it is the part to DESIGN rather than discover, so the three rules
//! live here with the reasoning attached rather than being spread across the
//! call sites that happen to obey them.
//!
//! # Rule 1 — replace by UNLINK, never by truncate
//!
//! A running executable's text pages are mapped from its inode. Truncating
//! that inode and writing new bytes into it corrupts the *running* process:
//! the next page fault reads whatever is now at that offset. Unlinking it and
//! creating a NEW file at the same path does not — the old inode survives,
//! unnamed, until the last mapping goes away, and every future `exec` of the
//! path gets the new file.
//!
//! `install(1)` does exactly that (GNU unlinks the destination before opening;
//! BSD writes a temp file and `rename`s it), which is why
//! [`super::provision::install_to`] tries it FIRST and why neither it nor
//! anything else here ever reaches for `File::create` on the destination. On
//! Linux, `open(O_WRONLY)` on a running executable fails with `ETXTBSY`
//! anyway; on macOS it does not, so "the kernel would have stopped us" is not
//! a defence. [`replacement_allocates_a_new_inode`] pins the property.
//!
//! # Rule 2 — after the roll, read the PATH, never this process
//!
//! Every post-roll identity check must come from exec'ing the DESTINATION
//! PATH. Two tempting alternatives are both wrong here:
//!
//! * `std::env::current_exe()` — after rule 1 that is the OLD, now-unlinked
//!   image. It answers with the version we just replaced and reports the roll
//!   as a no-op.
//! * [`crate::daemon_bin_resolve::resolve_daemon_bin`] — its own module doc
//!   scopes it to callers with "no dependency on daemon-process/subprocess
//!   version parity", and this is the opposite of that. It strips the
//!   deleted-inode marker and hands back whatever file now sits at the path,
//!   which in the window between provisioning and a *deferred* restart is the
//!   NEW binary while the OLD process is still serving. It would report a roll
//!   complete that has not happened.
//!
//! [`version_of_destination`] is the one read, and
//! [`super::verify`] is its only caller — which is why the post-provision
//! assertions are self-replacement-safe by construction rather than by
//! remembering.
//!
//! # Rule 3 — "is the daemon on the new binary?" is answered by a NEW PID
//!
//! Not by a version string. A supervised roll is only complete once the
//! supervisor has relaunched onto a pid that is BOTH different from the
//! pre-restart one AND alive — see `restart::wait_for_new_launchd_pid` /
//! `wait_for_new_systemd_pid`. A version read alone cannot distinguish "the
//! new binary is on disk" from "the new binary is running", and the whole
//! #4232/#4950 class of outage is precisely the gap between those two.
//!
//! [`running_binary`] is reused from [`crate::auto_update::native_probe`]
//! rather than re-derived: its contract — `None` on the kernel's
//! ` (deleted)` marker — exists for exactly this hazard (#8017).

use std::path::{Path, PathBuf};

use super::out;
use super::util;

/// The binary this process was exec'd from, or `None` when that cannot be
/// answered honestly (including after its inode has been unlinked).
#[must_use]
pub fn running_binary() -> Option<PathBuf> {
    crate::auto_update::native_probe::running_binary()
}

/// Is `dest` the file this process is running from?
///
/// Compared by `(dev, ino)` rather than by path text: `~/.local/bin/loom-daemon`
/// and `/usr/local/bin/loom-daemon` are routinely the same file through a
/// symlink, and a path-only comparison would miss the self case on exactly the
/// hosts that have one.
#[must_use]
pub fn is_self_replacement(dest: &Path) -> bool {
    let Some(running) = running_binary() else {
        return false;
    };
    same_file(&running, dest)
}

fn same_file(a: &Path, b: &Path) -> bool {
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

/// Announce an impending self-replacement, once, on stderr.
///
/// Purely advisory and deliberately so: the write itself is already safe by
/// rule 1, and refusing here would make a `loom-daemon` that is the machine's
/// own installed binary unable to update itself — which is the entire job.
/// What it buys is that the one genuinely surprising line in a support log
/// ("the binary I was reading a version from is not the binary I am now
/// running") is never a mystery.
pub fn announce_if_self(dest: &Path) {
    if !is_self_replacement(dest) {
        return;
    }
    out::warn(&format!(
        "Self-replacement: {} is the binary running this update. It is replaced by unlink-and-create (never an in-place truncate), so this process keeps running from its old, now-unnamed inode; every identity check below re-execs the PATH, not this process (#8017/#8088).",
        dest.display()
    ));
}

/// `"$dest" --version` — the ONLY post-roll identity read.
///
/// See rule 2 above for why this takes a path and execs it, rather than
/// consulting `current_exe()` or the shared binary resolver.
#[must_use]
pub fn version_of_destination(dest: &Path) -> String {
    util::version_output(dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("loom-selfrepl-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[cfg(unix)]
    fn ino(p: &Path) -> u64 {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(p).unwrap().ino()
    }

    /// Rule 1, pinned: provisioning must allocate a NEW inode at the
    /// destination.
    ///
    /// This is the property that makes replacing a running binary safe. If
    /// [`super::super::provision::install_to`] ever regressed to an in-place
    /// truncate (a `File::create`, a `cp` without the unlink, a `>` redirect),
    /// the inode would be UNCHANGED here — and on a real host the process
    /// executing that inode would start faulting in the new file's bytes at
    /// the old file's offsets.
    #[cfg(unix)]
    #[test]
    fn replacement_allocates_a_new_inode() {
        let dir = tmpdir("inode");
        let dest = dir.join("loom-daemon");
        let fresh = dir.join("fresh");
        std::fs::write(&dest, "#!/bin/sh\necho old\n").unwrap();
        std::fs::write(&fresh, "#!/bin/sh\necho new\n").unwrap();
        let before = ino(&dest);

        assert!(super::super::provision::install_to(&fresh, &dest));

        assert_ne!(
            before,
            ino(&dest),
            "the destination was rewritten in place; a process running from it would be corrupted"
        );
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "#!/bin/sh\necho new\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Rule 2, pinned: the identity read follows the PATH across a
    /// replacement.
    ///
    /// The destination is replaced while an OPEN HANDLE to the original inode
    /// is still held — the filesystem-level stand-in for "a process is still
    /// running from it". A reader that followed the old inode (which is what
    /// `current_exe()` does for a self-replacing process) would still see
    /// `0.0.1`; this one must see `0.0.2`.
    #[cfg(unix)]
    #[test]
    fn the_version_read_follows_the_path_not_the_replaced_inode() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let dir = tmpdir("version");
        let dest = dir.join("loom-daemon");
        let write_stub = |path: &Path, version: &str| {
            let mut f = std::fs::File::create(path).unwrap();
            writeln!(f, "#!/bin/sh").unwrap();
            writeln!(
                f,
                "echo \"loom-daemon {version} (commit abc1234, built 2026-01-01T00:00:00Z)\""
            )
            .unwrap();
            drop(f);
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        write_stub(&dest, "0.0.1");
        // Hold the ORIGINAL inode open across the replacement.
        let held = std::fs::File::open(&dest).unwrap();
        assert!(version_of_destination(&dest).contains("0.0.1"));

        let fresh = dir.join("fresh");
        write_stub(&fresh, "0.0.2");
        assert!(super::super::provision::install_to(&fresh, &dest));

        assert!(
            version_of_destination(&dest).contains("0.0.2"),
            "the post-roll read must exec the PATH, not the inode the old process holds"
        );
        drop(held);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_path_that_is_not_this_process_is_not_a_self_replacement() {
        let dir = tmpdir("notself");
        let other = dir.join("loom-daemon");
        std::fs::write(&other, "x").unwrap();
        assert!(!is_self_replacement(&other));
        assert!(!is_self_replacement(Path::new("/definitely/not/here")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The self case is detected through a SYMLINK, not just by path equality.
    #[cfg(unix)]
    #[test]
    fn the_self_check_sees_through_a_symlink() {
        let Some(running) = running_binary() else {
            return; // no current_exe on this platform — nothing to assert
        };
        let dir = tmpdir("symlink");
        let link = dir.join("loom-daemon");
        std::os::unix::fs::symlink(&running, &link).unwrap();
        assert!(
            is_self_replacement(&link),
            "a symlink to the running binary IS the running binary"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
