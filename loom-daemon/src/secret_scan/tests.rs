//! Tests for the credential content scan (#9133).
//!
//! Every credential-shaped value here is BUILT AT RUNTIME from pieces, so this
//! file never carries the shape it tests for and never trips the gate itself.
//! None of them was ever a credential.

use super::*;
use std::process::Command;

/// `sk-ant-<kind>-` + 93 varied characters + `AA`: the real length and alphabet.
fn fake_anthropic(kind: &str) -> String {
    let body: String = "k7Qx2Vb9Zr4Tm1Wc8Hn3Pd6Lf0Ys5Ju_-"
        .chars()
        .cycle()
        .take(93)
        .collect();
    format!("{}{}-{}AA", ["sk", "ant"].join("-") + "-", kind, body)
}

fn oauth() -> String {
    fake_anthropic(&format!("{}{}", "oat", "01"))
}

fn scanner() -> Scanner {
    Scanner::new(HashSet::new())
}

fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(args)
            .status()
            .unwrap();
        assert!(ok.success(), "git {args:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.email", "test@example.invalid"]);
    git(&["config", "user.name", "test"]);
    git(&["config", "commit.gpgsign", "false"]);
    git(&["config", "core.hooksPath", "/dev/null"]);
    std::fs::write(dir.path().join("README"), "seed\n").unwrap();
    git(&["add", "README"]);
    git(&["commit", "-q", "-m", "seed"]);
    dir
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .unwrap();
    assert!(ok.success(), "git {args:?}");
}

#[test]
fn fingerprint_is_sha256_prefix_of_the_trimmed_value() {
    // sha256("abc") = ba7816bf…
    assert_eq!(fingerprint("abc\n"), "ba7816bf");
}

#[test]
fn every_class_regex_compiles() {
    let s = scanner();
    assert_eq!(s.classes.len(), SECRET_CLASSES.len());
}

#[test]
fn real_shaped_token_is_found_and_its_value_never_rendered() {
    let token = oauth();
    let mut out = Vec::new();
    scanner().scan_line("x.token", 1, None, &format!("TOKEN={token}"), &mut out);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].class, "anthropic-oauth");
    assert_eq!(out[0].fingerprint, fingerprint(&token));
    let rendered = out[0].to_string();
    assert!(!rendered.contains(&token), "a finding must never carry the value");
    assert_eq!(rendered, format!("x.token:1: anthropic-oauth (fp {})", fingerprint(&token)));
}

#[test]
fn api_key_class_is_found() {
    let mut out = Vec::new();
    let key = fake_anthropic(&format!("{}{}", "api", "03"));
    scanner().scan_line("a", 1, None, &key, &mut out);
    assert_eq!(out.iter().map(|f| f.class).collect::<Vec<_>>(), ["anthropic-api-key"]);
}

#[test]
fn short_synthetic_fixtures_are_not_findings() {
    let mut out = Vec::new();
    let s = scanner();
    s.scan_line("a", 1, None, &format!("{}-{}", "sk-ant-oat01", "fake-real-token"), &mut out);
    s.scan_line("a", 2, None, &format!("{}{}", "tskey-auth-", "0123456789abcdef"), &mut out);
    assert!(out.is_empty(), "{out:?}");
}

#[test]
fn a_pem_header_alone_is_code_but_header_plus_body_is_a_key() {
    let header = format!("-----BEGIN {} KEY-----", "PRIVATE");
    let body: String = "MIIEvQIBADANBgkqhkiG9w0BAQEFAASC"
        .chars()
        .cycle()
        .take(64)
        .collect();
    let s = scanner();

    // A PEM parser's format check, and a test stub with a fake body.
    let mut out = Vec::new();
    s.scan_text("auth.ts", &format!("if (!pem.includes(\"{header}\")) {{}}\n"), &mut out);
    s.scan_text("t.py", &format!("write_text('{header}\\nsecret\\n')\n"), &mut out);
    assert!(out.is_empty(), "{out:?}");

    // Header on its own line, body on the next: reported at the header.
    s.scan_text("id_rsa", &format!("{header}\n{body}\n-----END\n"), &mut out);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!((out[0].class, out[0].line), ("private-key", 1));

    // The escaped one-line form (JSON, env files).
    let mut out = Vec::new();
    s.scan_text("k.json", &format!("{{\"key\": \"{header}\\n{body}\\n\"}}\n"), &mut out);
    assert_eq!(out.iter().map(|f| f.class).collect::<Vec<_>>(), ["private-key"]);
}

#[test]
fn allowlist_suppresses_exactly_the_listed_fingerprint() {
    let token = oauth();
    let other = fake_anthropic(&format!("{}{}", "api", "03"));
    let s = Scanner::new(parse_allowlist(&format!(
        "# comment\n{}  fixture\nnot-a-fp x\n",
        fingerprint(&token)
    )));
    let mut out = Vec::new();
    s.scan_line("a", 1, None, &token, &mut out);
    assert!(out.is_empty());
    s.scan_line("a", 2, None, &other, &mut out);
    assert_eq!(out.len(), 1);
}

#[test]
fn diff_scan_reports_added_lines_with_new_side_line_numbers() {
    let token = oauth();
    let diff = format!(
        "commit 0123456789abcdef0123\n\
         diff --git a/f b/f\n\
         --- a/f\n\
         +++ b/f\n\
         @@ -3,2 +3,3 @@\n\
         -{token}\n\
         -old\n\
         +keep\n\
         ++++ {token}\n\
         +also\n"
    );
    let mut out = Vec::new();
    scanner().scan_diff(&diff, &mut out);
    // The removed line is not a finding; the added line whose content starts
    // with `+++ ` is, and is not mistaken for a file header.
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!((out[0].path.as_str(), out[0].line), ("f", 4));
    assert_eq!(out[0].commit.as_deref(), Some("0123456789ab"));
}

#[test]
fn modes_are_read_from_the_git_subcommand_not_from_any_word() {
    assert_eq!(modes_for_command("git add -A && git commit -m x"), [Mode::Pending]);
    assert_eq!(modes_for_command("git -C ../r push origin HEAD"), [Mode::Unpushed]);
    assert_eq!(
        modes_for_command("git commit -q -m x; git push"),
        [Mode::Pending, Mode::Unpushed]
    );
    assert!(modes_for_command("git log --grep commit").is_empty());
    assert!(modes_for_command("echo push && ls commit").is_empty());
}

#[test]
fn pending_catches_the_incident_shape_an_untracked_pool_copy() {
    let dir = repo();
    let copy = dir.path().join(".loom/tokens.bak-20990101T000000Z");
    std::fs::create_dir_all(&copy).unwrap();
    std::fs::write(copy.join("a.token"), oauth()).unwrap();
    let found = scan(&scanner(), dir.path(), &Mode::Pending).unwrap();
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].path.ends_with("a.token"));
    // Staged alone does not see it: nothing is in the index yet, which is
    // exactly why the guard uses Pending.
    assert!(scan(&scanner(), dir.path(), &Mode::Staged)
        .unwrap()
        .is_empty());
}

#[test]
fn staged_catches_a_staged_value_at_any_path() {
    let dir = repo();
    std::fs::write(dir.path().join("notes.md"), format!("key {}\n", oauth())).unwrap();
    git(dir.path(), &["add", "notes.md"]);
    assert_eq!(scan(&scanner(), dir.path(), &Mode::Staged).unwrap().len(), 1);
}

#[test]
fn range_catches_a_value_added_then_deleted() {
    let dir = repo();
    let base = String::from_utf8(git_out(dir.path(), &["rev-parse", "HEAD"])).unwrap();
    std::fs::write(dir.path().join("leak.token"), oauth()).unwrap();
    git(dir.path(), &["add", "leak.token"]);
    git(dir.path(), &["commit", "-q", "-m", "add"]);
    git(dir.path(), &["rm", "-q", "leak.token"]);
    git(dir.path(), &["commit", "-q", "-m", "remove"]);
    let range = format!("{}..HEAD", base.trim());
    let found = scan(&scanner(), dir.path(), &Mode::Range(vec![range])).unwrap();
    assert_eq!(found.len(), 1, "history, not the net diff: {found:?}");
    assert!(found[0].commit.is_some());
    // A new branch in pre-push (remote sha all zeros) scans everything no
    // remote has — here, all of it.
    let head = String::from_utf8(git_out(dir.path(), &["rev-parse", "HEAD"])).unwrap();
    let stdin = format!("refs/heads/main {} refs/heads/main {ZERO_SHA}\n", head.trim());
    assert_eq!(
        scan(&scanner(), dir.path(), &Mode::PrePush(stdin))
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn clean_repo_has_no_findings() {
    let dir = repo();
    assert!(scan(&scanner(), dir.path(), &Mode::Pending)
        .unwrap()
        .is_empty());
}

fn git_out(dir: &Path, args: &[&str]) -> Vec<u8> {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap()
        .stdout
}
