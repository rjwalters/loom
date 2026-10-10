use std::fs;
use std::path::Path;
use std::process::Command;

use super::*;

fn src(version: &str, entries: &[(&str, &str)]) -> String {
    let body: String = entries
        .iter()
        .map(|(k, v)| format!("    (\"{k}\", \"{v}\"),\n"))
        .collect();
    format!(
        "/// doc\npub const CONTROL_VERSION: u32 = {version};\n\
         pub const POLICY: [(&str, &str); {}] = [\n{body}];\n",
        entries.len()
    )
}

const BASE: &[(&str, &str)] = &[("LOOM_GUARD_SQL", "1"), ("LOOM_RM_SCOPE", "repo")];

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn write(dir: &Path, text: &str) {
    let p = dir.join(DEFAULT_SOURCE_PATH);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, text).unwrap();
}

fn repo(initial: &str) -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    git(d.path(), &["init", "-q", "-b", "main"]);
    write(d.path(), initial);
    git(d.path(), &["add", "-A"]);
    git(d.path(), &["commit", "-q", "-m", "base"]);
    d
}

/// Commit `text` on a branch off main named `topic`, then return to main.
fn topic(d: &Path, text: &str) {
    git(d, &["checkout", "-q", "-b", "topic"]);
    write(d, text);
    git(d, &["commit", "-q", "-am", "topic"]);
    git(d, &["checkout", "-q", "main"]);
}

fn run(d: &Path) -> Result<Verdict> {
    check(d, "main", "topic", DEFAULT_SOURCE_PATH)
}

fn is_violation(v: Result<Verdict>) -> String {
    match v.unwrap() {
        Verdict::Violation(m) => m,
        other => panic!("expected violation, got {other:?}"),
    }
}

#[test]
fn value_change_without_bump_fails() {
    let d = repo(&src("2", BASE));
    topic(d.path(), &src("2", &[("LOOM_GUARD_SQL", "0"), ("LOOM_RM_SCOPE", "repo")]));
    let m = is_violation(run(d.path()));
    assert!(m.contains("LOOM_GUARD_SQL") && m.contains("changed"), "{m}");
}

#[test]
fn value_change_with_bump_passes() {
    let d = repo(&src("2", BASE));
    topic(d.path(), &src("3", &[("LOOM_GUARD_SQL", "0"), ("LOOM_RM_SCOPE", "repo")]));
    assert!(matches!(run(d.path()).unwrap(), Verdict::OkBumped(_)));
}

#[test]
fn key_addition_and_removal_need_bump() {
    let d = repo(&src("2", BASE));
    topic(
        d.path(),
        &src(
            "2",
            &[
                ("LOOM_GUARD_SQL", "1"),
                ("LOOM_RM_SCOPE", "repo"),
                ("LOOM_NEW", "1"),
            ],
        ),
    );
    assert!(is_violation(run(d.path())).contains("LOOM_NEW"));
    git(d.path(), &["branch", "-q", "-D", "topic"]);
    topic(d.path(), &src("2", &[("LOOM_GUARD_SQL", "1")]));
    assert!(is_violation(run(d.path())).contains("removed"));
    git(d.path(), &["branch", "-q", "-D", "topic"]);
    topic(d.path(), &src("5", &[("LOOM_GUARD_SQL", "1")]));
    assert!(matches!(run(d.path()).unwrap(), Verdict::OkBumped(_)));
}

#[test]
fn decreased_version_fails_even_when_policy_unchanged() {
    let d = repo(&src("2", BASE));
    topic(d.path(), &src("1", BASE));
    assert!(is_violation(run(d.path())).contains("decreased"));
}

#[test]
fn unchanged_policy_passes_with_or_without_version_bump() {
    let d = repo(&src("2", BASE));
    topic(d.path(), &src("2", BASE).replace("doc", "other doc"));
    assert!(matches!(run(d.path()).unwrap(), Verdict::Ok(_)));
    git(d.path(), &["branch", "-q", "-D", "topic"]);
    topic(d.path(), &src("3", BASE));
    assert!(matches!(run(d.path()).unwrap(), Verdict::Ok(_)));
}

#[test]
fn formatting_comments_and_order_do_not_need_bump() {
    let d = repo(&src("2", BASE));
    let reformatted = "pub const CONTROL_VERSION: u32 = 2; // same\n\
        pub const POLICY: [(&str, &str); 2] = [\n\
        // reordered, comment, one line\n\
        (\"LOOM_RM_SCOPE\", \"repo\"), /* x */ (\"LOOM_GUARD_SQL\", \"1\")\n];\n";
    topic(d.path(), reformatted);
    assert!(matches!(run(d.path()).unwrap(), Verdict::Ok(_)));
}

#[test]
fn declaration_whitespace_and_comments_between_tokens_parse() {
    let canonical = parse_declarations(&src("2", BASE)).unwrap();
    let variants = [
        "pub const\nCONTROL_VERSION: u32 = 2;\npub const\nPOLICY: [(&str, &str); 2] = \
         [(\"LOOM_GUARD_SQL\", \"1\"), (\"LOOM_RM_SCOPE\", \"repo\")];\n",
        "pub const   CONTROL_VERSION: u32 = 2;\npub const  \tPOLICY: [(&str, &str); 2] = \
         [(\"LOOM_GUARD_SQL\", \"1\"), (\"LOOM_RM_SCOPE\", \"repo\")];\n",
        "pub const /* c */ CONTROL_VERSION: u32 = 2;\npub const/* c */POLICY: [(&str, &str); 2] = \
         [(\"LOOM_GUARD_SQL\", \"1\"), (\"LOOM_RM_SCOPE\", \"repo\")];\n",
        "pub const // c\nCONTROL_VERSION: u32 = 2;\npub const // c\nPOLICY: [(&str, &str); 2] = \
         [(\"LOOM_GUARD_SQL\", \"1\"), (\"LOOM_RM_SCOPE\", \"repo\")];\n",
    ];
    for v in variants {
        let got = parse_declarations(v).unwrap_or_else(|e| panic!("{v:?}: {e}"));
        assert_eq!(format!("{got:?}"), format!("{canonical:?}"), "{v:?}");
    }
}

#[test]
fn lookalike_identifiers_are_not_declarations() {
    assert!(find_decls("const POLICY_X: u8 = 1;", "POLICY").is_empty());
    assert!(find_decls("constPOLICY: u8 = 1;", "POLICY").is_empty());
    assert!(find_decls("myconst POLICY: u8 = 1;", "POLICY").is_empty());
    assert_eq!(find_decls("pub const\n POLICY: u8 = 1;", "POLICY").len(), 1);
}

#[test]
fn diverged_branch_uses_merge_base_not_base_tip() {
    let d = repo(&src("2", BASE));
    topic(d.path(), &src("2", BASE).replace("doc", "topic only"));
    // main moves on: bumps version AND changes policy after the fork point.
    write(d.path(), &src("9", &[("LOOM_GUARD_SQL", "0")]));
    git(d.path(), &["commit", "-q", "-am", "main moved"]);
    // Against main's tip the topic would look like a decrease; against the
    // merge base it is unchanged.
    assert!(matches!(run(d.path()).unwrap(), Verdict::Ok(_)));
    // And a real policy change on the topic is judged against the fork point.
    git(d.path(), &["checkout", "-q", "topic"]);
    write(d.path(), &src("2", &[("LOOM_GUARD_SQL", "0"), ("LOOM_RM_SCOPE", "repo")]));
    git(d.path(), &["commit", "-q", "-am", "weaken"]);
    git(d.path(), &["checkout", "-q", "main"]);
    assert!(is_violation(run(d.path())).contains("still 2"));
}

#[test]
fn missing_refs_fail_with_diagnostics() {
    let d = repo(&src("2", BASE));
    topic(d.path(), &src("3", BASE));
    let e = check(d.path(), "nope", "topic", DEFAULT_SOURCE_PATH).unwrap_err();
    assert!(format!("{e:#}").contains("base ref `nope`"));
    let e = check(d.path(), "main", "nope", DEFAULT_SOURCE_PATH).unwrap_err();
    assert!(format!("{e:#}").contains("head ref `nope`"));
    let e = check(d.path(), "main", "topic", "no/such/file.rs").unwrap_err();
    assert!(format!("{e:#}").contains("no/such/file.rs"));
}

#[test]
fn unrelated_histories_fail() {
    let d = repo(&src("2", BASE));
    git(d.path(), &["checkout", "-q", "--orphan", "topic"]);
    git(d.path(), &["commit", "-q", "-m", "orphan", "--allow-empty"]);
    git(d.path(), &["checkout", "-q", "main"]);
    let e = run(d.path()).unwrap_err();
    assert!(format!("{e:#}").contains("no merge base"));
}

#[test]
fn malformed_and_ambiguous_declarations_fail() {
    let cases: &[(&str, &str)] = &[
        ("pub const POLICY: [(&str, &str); 1] = [(\"A\", \"1\")];\n", "CONTROL_VERSION"),
        ("pub const CONTROL_VERSION: u32 = 2;\n", "const POLICY"),
        (
            "pub const CONTROL_VERSION: u32 = two;\npub const POLICY: [(&str, &str); 0] = [];\n",
            "not a plain integer",
        ),
        (
            "pub const CONTROL_VERSION: u32 = 2;\npub const CONTROL_VERSION: u32 = 3;\npub const POLICY: [(&str, &str); 0] = [];\n",
            "2 `const CONTROL_VERSION`",
        ),
        (
            "pub const CONTROL_VERSION: u32 = 2;\nconst POLICY: [(&str, &str); 0] = [];\nconst POLICY: [(&str, &str); 0] = [];\n",
            "2 `const POLICY`",
        ),
        (
            "pub const CONTROL_VERSION: u32 = 2;\npub const POLICY: [(&str, &str); 2] = [(\"A\", \"1\"), (\"A\", \"2\")];\n",
            "duplicate POLICY key `A`",
        ),
        (
            "pub const CONTROL_VERSION: u32 = 2;\npub const POLICY: [(&str, &str); 1] = [(\"A\", FOO)];\n",
            "policy value",
        ),
        (
            "pub const CONTROL_VERSION: u32 = 2;\npub const POLICY: [(&str, &str); 3] = [(\"A\", \"1\")];\n",
            "annotated with length 3",
        ),
        (
            "pub const CONTROL_VERSION: u32 = 2;\npub const POLICY: [(&str, &str); 1] = build();\n",
            "expected `[`",
        ),
    ];
    for (text, needle) in cases {
        let err = format!("{:#}", parse_declarations(text).unwrap_err());
        assert!(err.contains(needle), "wanted {needle:?} in {err:?} for {text}");
    }
}

#[test]
fn commented_out_declarations_are_ignored() {
    let text = "// pub const CONTROL_VERSION: u32 = 99;\n/* const POLICY */\n".to_string()
        + &src("4", BASE);
    let d = parse_declarations(&text).unwrap();
    assert_eq!(d.control_version, 4);
    assert_eq!(d.policy.len(), 2);
}

#[test]
fn real_bundle_source_parses() {
    let real = include_str!("../tokens_pool/private_workspace/bundle.rs");
    let d = parse_declarations(real).unwrap();
    assert_eq!(
        d.control_version,
        u64::from(crate::tokens_pool::private_workspace::bundle::CONTROL_VERSION)
    );
    assert_eq!(d.policy.len(), crate::tokens_pool::private_workspace::bundle::POLICY.len());
}
