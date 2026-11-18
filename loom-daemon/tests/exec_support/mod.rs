//! Integration-test fixture: write a file and mark it executable.
//!
//! Each file under `tests/` is its own crate, so it cannot reach the library's
//! `exec_test_support`; this is its counterpart. Include it with
//! `mod exec_support;`. It lives in a directory (not a `tests/*.rs` file) so
//! cargo does not build it as a test target of its own.

use std::fs;
use std::path::Path;

/// Write `body` to `path` and make it executable (mode 0755), creating any
/// missing parent directories. Panics on failure.
pub fn write_executable(path: &Path, body: impl AsRef<[u8]>) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent dir");
    }
    fs::write(path, body).expect("write");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod");
    }
}
