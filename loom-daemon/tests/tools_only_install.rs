//! The tools-only installer's stage / check / publish / rollback path against
//! the real `loom-tools` (#12940, design `docs/design/loom-tools-cli-boundary.md`
//! §11 (d)).
//!
//! `tests/install/test-provision-tools.sh` owns the scenarios (fresh install, a
//! failing staged check leaving the previous binary, a failing post-publish
//! check restoring `<dest>.previous`, fetch failing closed). It skips the
//! install half when no `loom-tools` is built, which is what a bare shell CI leg
//! has; this test hands it the binary cargo just built so the rollback is
//! exercised on every `cargo test`.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn tools_only_installer_installs_and_rolls_back_with_the_real_binary() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let out = Command::new("bash")
        .arg(root.join("tests/install/test-provision-tools.sh"))
        .env("LOOM_TOOLS_TEST_BIN", env!("CARGO_BIN_EXE_loom-tools"))
        .output()
        .expect("run test-provision-tools.sh");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}\n{}", String::from_utf8_lossy(&out.stderr));
    assert!(
        !stdout.contains("SKIP"),
        "the install half must run against the built binary:\n{stdout}"
    );
    assert!(stdout.contains("restores the previous binary"), "{stdout}");
}
