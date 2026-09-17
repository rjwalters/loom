//! Tests for the host facts (epic #7810, PR 5).

use super::*;

#[test]
fn each_published_platform_maps_to_its_triple() {
    assert_eq!(target_triple_for("macos", "aarch64"), Some("aarch64-apple-darwin"));
    assert_eq!(target_triple_for("linux", "aarch64"), Some("aarch64-unknown-linux-gnu"));
    assert_eq!(target_triple_for("linux", "x86_64"), Some("x86_64-unknown-linux-gnu"));
}

#[test]
fn both_arch_spellings_uname_reports_are_accepted() {
    assert_eq!(target_triple_for("macos", "arm64"), target_triple_for("macos", "aarch64"));
    assert_eq!(target_triple_for("linux", "amd64"), target_triple_for("linux", "x86_64"));
}

#[test]
fn an_unpublished_platform_is_none_not_a_guess() {
    // x86_64 macOS is the live case: the release workflow publishes no such
    // artifact, so a triple here would resolve a download that does not exist.
    assert_eq!(target_triple_for("macos", "x86_64"), None);
    assert_eq!(target_triple_for("windows", "x86_64"), None);
    assert_eq!(target_triple_for("linux", "riscv64"), None);
}

#[test]
fn this_host_resolves_or_reports_nothing() {
    // Whatever CI runs on, the answer must be a mapped triple or None — never
    // a panic and never a fabricated string.
    if let Some(t) = target_triple() {
        assert!(t.contains('-'), "{t}");
    }
}

// ---------------------------------------------------------------------------
// Repo slug
// ---------------------------------------------------------------------------

#[test]
fn every_remote_form_the_shell_enumerated_parses() {
    for url in [
        "git@github.com:rjwalters/loom.git",
        "git@github.com:rjwalters/loom",
        "https://github.com/rjwalters/loom.git",
        "https://github.com/rjwalters/loom",
        "http://github.com/rjwalters/loom",
        "ssh://git@github.com/rjwalters/loom.git",
    ] {
        assert_eq!(slug_from_remote_url(url).as_deref(), Some("rjwalters/loom"), "{url}");
    }
}

#[test]
fn a_trailing_slash_does_not_produce_an_empty_third_segment() {
    assert_eq!(
        slug_from_remote_url("https://github.com/rjwalters/loom/").as_deref(),
        Some("rjwalters/loom")
    );
}

#[test]
fn a_non_github_remote_yields_none() {
    // Releases are resolved through `gh`. A slug it cannot address is worse
    // than no slug: it turns "no artifact here" into a confusing API error.
    assert_eq!(slug_from_remote_url("git@gitlab.com:owner/repo.git"), None);
    assert_eq!(slug_from_remote_url("https://gitea.example.com/o/r"), None);
    assert_eq!(slug_from_remote_url("/srv/git/local.git"), None);
}

#[test]
fn a_url_naming_only_one_half_yields_none() {
    assert_eq!(slug_from_remote_url("https://github.com/rjwalters"), None);
    assert_eq!(slug_from_remote_url("https://github.com/"), None);
}

#[test]
fn a_deeper_path_is_not_silently_truncated_to_two_segments() {
    // github.com/o/r/tree/main is not a clone URL; accepting it would produce
    // a slug that resolves to the wrong thing rather than to nothing.
    assert_eq!(slug_from_remote_url("https://github.com/rjwalters/loom/tree/main"), None);
}

// ---------------------------------------------------------------------------
// sha256
// ---------------------------------------------------------------------------

#[test]
fn a_files_digest_is_the_real_sha256() {
    let p = std::env::temp_dir().join(format!("loom-sha-{}", std::process::id()));
    std::fs::write(&p, b"abc").expect("write");
    let got = sha256_file(&p);
    let _ = std::fs::remove_file(&p);
    assert_eq!(
        got.as_deref(),
        Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
    );
}

#[test]
fn an_unreadable_path_yields_none_rather_than_an_empty_digest() {
    // An empty string here would compare unequal to every published sha and
    // read as "same version, different bytes" on every single tick.
    assert_eq!(sha256_file(Path::new("/nonexistent/loom-daemon")), None);
}
