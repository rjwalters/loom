//! Tests for unconditional checksum verification (epic #7810, PR 6a).

use super::*;

fn write(dir: &std::path::Path, name: &str, contents: &[u8]) -> std::path::PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, contents).unwrap();
    p
}

/// #7977/#5020 AC2: a checksum mismatch must be treated as tamper evidence,
/// never a soft-fallback condition — this is the property `fetch::verify`
/// hard-aborts on rather than retrying or falling back to a source build.
#[test]
fn matching_digest_verifies() {
    let dir = tempfile_dir();
    let bin = write(&dir, "bin", b"hello world");
    let digest = sha256_file(&bin).unwrap();
    let sha = write(&dir, "bin.sha256", format!("{digest}  bin\n").as_bytes());
    assert!(verify(&bin, &sha));
}

#[test]
fn mismatched_digest_fails_closed() {
    let dir = tempfile_dir();
    let bin = write(&dir, "bin", b"hello world");
    let sha = write(
        &dir,
        "bin.sha256",
        b"0000000000000000000000000000000000000000000000000000000000000000  bin\n",
    );
    assert!(!verify(&bin, &sha));
}

#[test]
fn missing_sha_file_fails_closed() {
    let dir = tempfile_dir();
    let bin = write(&dir, "bin", b"hello world");
    assert!(!verify(&bin, &dir.join("does-not-exist.sha256")));
}

#[test]
fn unreadable_binary_fails_closed() {
    let dir = tempfile_dir();
    let sha = write(&dir, "bin.sha256", b"deadbeef  bin\n");
    assert!(!verify(&dir.join("does-not-exist"), &sha));
}

/// Only the FIRST whitespace-delimited field of the FIRST line counts — the
/// `shasum -a 256` / `sha256sum` line shape (`<hex>  <filename>`), matching
/// `awk 'NR==1{print $1}'` in the shell original.
#[test]
fn only_the_first_field_of_the_first_line_is_read() {
    let dir = tempfile_dir();
    let bin = write(&dir, "bin", b"hello world");
    let digest = sha256_file(&bin).unwrap();
    let sha = write(
        &dir,
        "bin.sha256",
        format!("{digest}  bin  extra-trailing-noise\nsecond line ignored\n").as_bytes(),
    );
    assert!(verify(&bin, &sha));
}

fn tempfile_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "loom-daemon-checksum-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
