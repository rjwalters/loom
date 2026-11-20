//! Library-test fixture: write a file and mark it executable.
//!
//! Tests stand in fake `gh`, `git`, hook and spawn scripts all the time, and
//! every one needs the same two steps: write the body, then `chmod 0755`. One
//! helper here replaces the private per-file copies that had accumulated
//! under four different names.

use std::fs;
use std::path::Path;

/// Write `body` to `path` and make it executable (mode 0755).
///
/// Missing parent directories are created, so a fixture can name a script at
/// `root/.loom/scripts/x.sh` without laying the tree out first. Panics on
/// failure: a fixture that cannot be built must fail the test loudly.
pub(crate) fn write_executable(path: &Path, body: impl AsRef<[u8]>) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}
