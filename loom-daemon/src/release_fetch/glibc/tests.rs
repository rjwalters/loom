//! Tests for the GLIBC compatibility gate (#8837).
//!
//! Exercises the real [`check`] against FAKE `objdump`/`ldd`/`getconf`
//! scripts placed ahead of the real ones on `PATH` -- the same technique
//! `signature/tests.rs` uses for `codesign`/`cosign`, and for the same
//! reason: a CI runner's own glibc must never leak into what these tests
//! assert. `PATH` is process-global, so every PATH-dependent test here is
//! `#[serial]`.

use super::*;
use serial_test::serial;
use std::io::Write as _;

fn write_script(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
    let p = dir.join(name);
    let mut f = std::fs::File::create(&p).unwrap();
    writeln!(f, "#!/usr/bin/env bash\n{body}").unwrap();
    drop(f);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&p).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&p, perms).unwrap();
    }
    p
}

fn tempdir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "loom-daemon-glibc-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Prepends `bindir` to PATH for the duration of `f`, then restores it
/// exactly. Callers must be `#[serial]` -- PATH is process-global.
fn with_fake_bin<F: FnOnce()>(bindir: &Path, f: F) {
    let old = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{}:{old}", bindir.display()));
    f();
    std::env::set_var("PATH", old);
}

/// Puts `bindir` first on a minimal `PATH` (`bindir:/usr/bin:/bin`).
///
/// NOTE: this is NOT a sandbox. `/usr/bin:/bin` must stay on `PATH` because
/// every fake script is `#!/usr/bin/env bash`, which resolves `bash` via
/// `PATH` -- so the host's REAL `ldd`/`getconf` remain reachable. A test that
/// needs a tool to be "unavailable" must shadow it in `bindir` with a fake
/// that fails (see [`fake_unavailable`]), never rely on omission; omission
/// makes the outcome depend on the host's own glibc (#8843 CI failure).
fn with_only(bindir: &Path, f: impl FnOnce()) {
    let old = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{}:/usr/bin:/bin", bindir.display()));
    f();
    std::env::set_var("PATH", old);
}

/// Shadows `name` with a fake that exits non-zero without output, so the
/// probe for it genuinely fails regardless of what the host has installed.
fn fake_unavailable(dir: &Path, name: &str) {
    write_script(dir, name, "exit 1\n");
}

fn fake_target_bin(dir: &Path) -> std::path::PathBuf {
    let p = dir.join("loom-daemon-x86_64-unknown-linux-gnu");
    std::fs::write(&p, b"binary bytes").unwrap();
    p
}

// ---------------------------------------------------------------------------
// Not applicable
// ---------------------------------------------------------------------------

#[test]
fn non_gnu_target_is_a_silent_noop() {
    let dir = tempdir();
    let bin = dir.join("loom-daemon-aarch64-apple-darwin");
    std::fs::write(&bin, b"binary bytes").unwrap();
    let result = check(&bin, "aarch64-apple-darwin");
    assert_eq!(result.outcome, Outcome::Ok);
    assert!(result.message.is_empty());
}

// ---------------------------------------------------------------------------
// Missing tooling -- loud skip, never a block
// ---------------------------------------------------------------------------

#[test]
#[serial]
fn missing_objdump_is_a_loud_skip_not_a_block() {
    let dir = tempdir();
    let bin = fake_target_bin(&dir);
    let empty_bin_dir = tempdir();
    // Shadow the host's real objdump (`with_only` keeps /usr/bin on PATH).
    fake_unavailable(&empty_bin_dir, "objdump");
    let mut result = None;
    with_only(&empty_bin_dir, || {
        result = Some(check(&bin, "x86_64-unknown-linux-gnu"));
    });
    let result = result.unwrap();
    assert_eq!(result.outcome, Outcome::Ok);
    assert!(result.message.contains("objdump"), "{}", result.message);
    assert!(result.message.contains("SKIPPING"), "{}", result.message);
}

#[test]
#[serial]
fn objdump_output_with_no_glibc_symbol_is_a_loud_skip() {
    let dir = tempdir();
    let bin = fake_target_bin(&dir);
    let fakebin = tempdir();
    write_script(&fakebin, "objdump", "echo 'no versioned symbols here'\nexit 0\n");
    let empty_bin_dir = tempdir();
    let mut result = None;
    with_fake_bin(&fakebin, || {
        with_only(&empty_bin_dir, || {
            // objdump resolves via fakebin (prepended by the outer
            // with_fake_bin), everything else is absent.
            let old = std::env::var("PATH").unwrap_or_default();
            std::env::set_var("PATH", format!("{}:{old}", fakebin.display()));
            result = Some(check(&bin, "x86_64-unknown-linux-gnu"));
            std::env::set_var("PATH", old);
        });
    });
    let result = result.unwrap();
    assert_eq!(result.outcome, Outcome::Ok);
    assert!(result.message.contains("no GLIBC_x.y symbol"), "{}", result.message);
}

#[test]
#[serial]
fn missing_ldd_and_getconf_is_a_loud_skip_not_a_block() {
    let dir = tempdir();
    let bin = fake_target_bin(&dir);
    let fakebin = tempdir();
    write_script(
        &fakebin,
        "objdump",
        "echo '0000000000000000  DF *UND*  0000000000000000  GLIBC_2.34  __libc_start_main'\nexit 0\n",
    );
    // `with_only` still resolves the REAL system `ldd`/`getconf` via the
    // `/usr/bin:/bin` fallback it appends -- so unavailability has to be
    // forced explicitly here, by shadowing both with fakes that fail, rather
    // than by omission (there is no PATH on a real Linux host that lacks
    // them both).
    fake_unavailable(&fakebin, "ldd");
    fake_unavailable(&fakebin, "getconf");
    let mut result = None;
    with_only(&fakebin, || {
        result = Some(check(&bin, "x86_64-unknown-linux-gnu"));
    });
    let result = result.unwrap();
    assert_eq!(result.outcome, Outcome::Ok);
    assert!(result.message.contains("glibc version"), "{}", result.message);
    assert!(result.message.contains("SKIPPING"), "{}", result.message);
}

// ---------------------------------------------------------------------------
// The compatibility verdict itself
// ---------------------------------------------------------------------------

fn fake_objdump(dir: &Path, glibc_symbols: &[&str]) {
    let lines: String = glibc_symbols
        .iter()
        .map(|v| format!("echo '0000000000000000  DF *UND*  0000000000000000  {v}  some_symbol'\n"))
        .collect();
    write_script(dir, "objdump", &format!("{lines}exit 0\n"));
}

fn fake_ldd(dir: &Path, version: &str) {
    write_script(
        dir,
        "ldd",
        &format!("echo 'ldd (Ubuntu GLIBC {version}-0ubuntu3.8) {version}'\nexit 0\n"),
    );
}

/// The exact scenario from the 2026-09-24 incident: an artifact built
/// against a newer GLIBC than the host (2.35) provides must be refused.
#[test]
#[serial]
fn artifact_requiring_newer_glibc_than_host_is_incompatible() {
    let dir = tempdir();
    let bin = fake_target_bin(&dir);
    let fakebin = tempdir();
    fake_objdump(&fakebin, &["GLIBC_2.17", "GLIBC_2.34", "GLIBC_2.38", "GLIBC_2.39"]);
    fake_ldd(&fakebin, "2.35");

    let mut result = None;
    with_only(&fakebin, || {
        result = Some(check(&bin, "x86_64-unknown-linux-gnu"));
    });
    let result = result.unwrap();
    assert_eq!(result.outcome, Outcome::Incompatible);
    assert!(result.message.contains("GLIBC_2.39"), "{}", result.message);
    assert!(result.message.contains("GLIBC_2.35"), "{}", result.message);
}

/// A binary that only needs an OLD glibc symbol set against a newer host is
/// accepted -- the common case (every non-incident fetch).
#[test]
#[serial]
fn artifact_requiring_older_glibc_than_host_is_compatible() {
    let dir = tempdir();
    let bin = fake_target_bin(&dir);
    let fakebin = tempdir();
    fake_objdump(&fakebin, &["GLIBC_2.2.5", "GLIBC_2.17"]);
    fake_ldd(&fakebin, "2.35");

    let mut result = None;
    with_only(&fakebin, || {
        result = Some(check(&bin, "x86_64-unknown-linux-gnu"));
    });
    let result = result.unwrap();
    assert_eq!(result.outcome, Outcome::Ok);
    assert!(result.message.contains("GLIBC_2.17"), "{}", result.message);
    assert!(result.message.contains("GLIBC_2.35"), "{}", result.message);
}

/// Exactly matching the host's own glibc is compatible, not a boundary
/// failure.
#[test]
#[serial]
fn artifact_requiring_exactly_the_hosts_glibc_is_compatible() {
    let dir = tempdir();
    let bin = fake_target_bin(&dir);
    let fakebin = tempdir();
    fake_objdump(&fakebin, &["GLIBC_2.35"]);
    fake_ldd(&fakebin, "2.35");

    let mut result = None;
    with_only(&fakebin, || {
        result = Some(check(&bin, "x86_64-unknown-linux-gnu"));
    });
    let result = result.unwrap();
    assert_eq!(result.outcome, Outcome::Ok);
}

/// `getconf GNU_LIBC_VERSION` is the fallback when `ldd` is unavailable.
#[test]
#[serial]
fn falls_back_to_getconf_when_ldd_is_unavailable() {
    let dir = tempdir();
    let bin = fake_target_bin(&dir);
    let fakebin = tempdir();
    fake_objdump(&fakebin, &["GLIBC_2.39"]);
    // `ldd` must be made unavailable EXPLICITLY: `with_only` keeps
    // `/usr/bin:/bin` on PATH, so omitting it would resolve the host's real
    // `ldd` and the getconf fallback would never run -- the verdict would
    // then depend on the CI runner's glibc (2.39 on ubuntu-24.04 => `Ok`).
    fake_unavailable(&fakebin, "ldd");
    write_script(&fakebin, "getconf", "echo 'glibc 2.35'\nexit 0\n");

    let mut result = None;
    with_only(&fakebin, || {
        result = Some(check(&bin, "x86_64-unknown-linux-gnu"));
    });
    let result = result.unwrap();
    assert_eq!(result.outcome, Outcome::Incompatible);
    assert!(result.message.contains("GLIBC_2.35"), "{}", result.message);
}

// ---------------------------------------------------------------------------
// Version parsing
// ---------------------------------------------------------------------------

#[test]
fn parse_version_ignores_trailing_packaging_noise() {
    assert_eq!(parse_version("2.35-0ubuntu3.8"), Some((2, 35)));
    assert_eq!(parse_version("2.39)"), Some((2, 39)));
    assert_eq!(parse_version("2.17"), Some((2, 17)));
    assert_eq!(parse_version("not-a-version"), None);
}

#[test]
fn max_glibc_version_picks_the_highest_symbol() {
    let text = "GLIBC_2.17  GLIBC_2.2.5  GLIBC_2.34  GLIBC_2.4";
    assert_eq!(max_glibc_version(text), Some((2, 34)));
}
