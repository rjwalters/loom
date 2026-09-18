//! Regenerates `tests/fixtures/extract_refs_shell_oracle.jsonl` — the frozen
//! oracle `tests/differential_extract_refs.rs` replays `extract()` against
//! (epic #7810, filed from #8072, corpus gaps closed by #8097).
//!
//! # Run
//!
//! ```text
//! cargo run -p loom-daemon --example generate_extract_refs_oracle
//! ```
//!
//! Overwrites `loom-daemon/tests/fixtures/extract_refs_shell_oracle.jsonl` in
//! place. Requires `bash`, `jq`, and a `git` checkout deep enough to contain
//! the pinned reference commit (a shallow CI checkout will not) — this is a
//! manual/offline regeneration tool, never invoked by `cargo test` or CI.
//!
//! # Why a Rust example rather than a shell script
//!
//! Repo policy (`.loom/docs/shell-language-policy.md`) routes new executable
//! logic through `loom-daemon`, not a new top-level `.sh`. This binary needs
//! no CLI surface of its own — it is a `cargo run --example`, checked in
//! next to the fixture it produces, so "regenerate" is one documented
//! command instead of archaeology (#8097, gap 2).
//!
//! # How it works
//!
//! 1. Recover the pre-port shell from git history — the exact commit
//!    `_meta.reference_impl` in the fixture names, the last one before the
//!    port in #7969 — via `git show <rev>:<path>`. The shell is retired from
//!    the working tree on purpose (see `tests/differential_extract_refs.rs`
//!    module docs); nothing here vendors it back in, it is fetched fresh
//!    into a temp file on every run and discarded after.
//! 2. Generate a corpus of `(body, comments)` pairs from the grammar
//!    `extract-refs` parses: the four trigger phrases and a set of
//!    near-misses; the full seven-member separator alphabet the port's
//!    `[[:space:]]` class and the retired shell's `grep -oE
//!    '[[:space:]]'` both define (space, `\n`, `\t`, `\r`, `\x0b`, `\x0c`,
//!    and NBSP `\u{00A0}` — included specifically because it is the one
//!    separator the two implementations used to disagree on, see
//!    `extract.rs`'s `phrase_re` doc); reference-number boundary shapes
//!    (zero, leading zeros, the `u64` limit and one past it, deep overflow);
//!    and comment-bearing inputs covering bot-authored exclusion (five login
//!    spellings), a distinct non-bot `[bot]`-suffixed author, an absent
//!    login, `OWN_MARKERS`-carrying comments, multi-comment concatenation,
//!    and cross-source (body+comment) reference dedup.
//! 3. Run every case through the recovered shell's `extract-refs --stdin
//!    --bot-login <BOT_LOGIN> --json`, feeding it the same JSON shape
//!    `gh issue view --json body,comments` returns.
//! 4. Write the frozen `_meta` line plus one line per case, deterministically
//!    ordered so a re-run without a real behavioural change reproduces a
//!    byte-identical file (i.e. it diffs cleanly in review).
//!
//! Enumeration here is fully deterministic (nested loops over the grammar
//! dimensions above) — no PRNG, no "seed" to keep synchronized with anything
//! else. The previous fixture's `_meta` named a seed for a generator that
//! was never committed; recording provenance that cannot be checked is
//! exactly the failure #8097 was filed to close, so this generator does not
//! repeat it.

use serde_json::{json, Map, Value};
use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The commit immediately before the port (#7969) — the last revision at
/// which the shell implementation existed. Matches `_meta.reference_impl`.
const REFERENCE_REV: &str = "b1968d2a";
const REFERENCE_PATH: &str = "defaults/scripts/dep-recheck-fingerprint.sh";

/// Must match `differential_extract_refs.rs`'s `BOT_LOGIN` constant — the
/// test asserts `_meta.bot_login` equals it, and this is the value that
/// determined which of this run's comment-bearing cases the shell excluded.
const BOT_LOGIN: &str = "loom-bot";

/// One generated `(body, comments)` pair, before the shell has answered it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct GenCase {
    body: String,
    comments: Vec<(String, String)>, // (author login, comment body)
}

impl GenCase {
    fn body_only(body: impl Into<String>) -> Self {
        Self {
            body: body.into(),
            comments: Vec::new(),
        }
    }

    fn with_comments(body: impl Into<String>, comments: Vec<(&str, &str)>) -> Self {
        Self {
            body: body.into(),
            comments: comments
                .into_iter()
                .map(|(l, b)| (l.to_string(), b.to_string()))
                .collect(),
        }
    }
}

/// The four machine-readable trigger phrases `extract()`/the shell parse,
/// reused verbatim from `phrase_re()`'s pattern.
const TRIGGERS: [&str; 4] = ["Blocked by", "Depends on", "Requires", "**Epic**"];

/// The full separator alphabet: every character `[[:space:]]` /
/// `[[:space:]]` (post-#8097) matches, plus NBSP — the one character it
/// deliberately does NOT match, included so the corpus proves the two sides
/// agree (both find nothing) rather than leaving the class untested.
const SEPARATORS: [(char, &str); 7] = [
    (' ', "space"),
    ('\n', "newline"),
    ('\t', "tab"),
    ('\r', "cr"),
    ('\x0b', "vt"),
    ('\x0c', "ff"),
    ('\u{00A0}', "nbsp"),
];

/// Reference-number shapes spanning the boundaries `extract()`'s doc comment
/// calls out: zero, a leading-zero form, the `u64` limit, one past it, deep
/// overflow, and an ordinary multi-digit value as a control.
const TOKEN_SHAPES: [&str; 8] = [
    "0",
    "5",
    "007",
    "0000",
    "18446744073709551615",       // u64::MAX
    "18446744073709551616",       // u64::MAX + 1
    "99999999999999999999999999", // deep overflow
    "123456789",
];

fn phase1_separator_grid() -> Vec<GenCase> {
    let mut cases = Vec::new();
    for trigger in TRIGGERS {
        for (sep, _name) in SEPARATORS {
            for token in TOKEN_SHAPES {
                // Bare separator run.
                cases.push(GenCase::body_only(format!("{trigger}{sep}#{token}")));
                // A single literal-colon-plus-separator run.
                cases.push(GenCase::body_only(format!("{trigger}:{sep}#{token}")));
                // Markup-and-separator mix: the class also admits `*`, `_`, `:`.
                cases.push(GenCase::body_only(format!("{trigger}*_:{sep}{sep}#{token}")));
            }
        }
    }
    cases
}

fn phase1b_near_misses() -> Vec<GenCase> {
    let near_miss_phrases = [
        "Blocking",
        "Required",
        "epic",
        "**epic**",
        "Depends",
        "block by",
        "BLOCKED BY",
        "Depend on",
        "*Epic*",
        "Blockedby",
        "See dependencies below",
        "Epic",
        "Blocked",
        "blocked by",
        "requires",
        "DEPENDS ON",
        "**epic #",
        "Blocked-by",
        "Depends_on",
        "Requires:",
    ];
    near_miss_phrases
        .iter()
        .map(|p| GenCase::body_only(format!("{p} #42")))
        .collect()
}

fn phase1c_multi_reference_bodies() -> Vec<GenCase> {
    let mut cases = Vec::new();
    let templates: [fn(char) -> String; 8] = [
        |s| format!("Blocked by #10{s}Depends on #9{s}Requires #9"),
        |s| format!("Requires #0{s}and Depends on #007{s}and Blocked by #7"),
        |s| format!("**Epic**{s}#18446744073709551615{s}Blocked by{s}#18446744073709551616"),
        |s| format!("Depends on #1{s}Depends on #01{s}Depends on #001"),
        |s| format!("Blocked by #5{s}unrelated text{s}Requires #5{s}Requires #6"),
        |s| {
            format!("Requires #18446744073709551616{s}Requires #18446744073709551615{s}Requires #7")
        },
        |s| format!("Blocked by #0000{s}Depends on #0{s}Requires #00"),
        |s| format!("**Epic** #99999999999999999999999999{s}Blocked by #1{s}Depends on #1"),
    ];
    for template in templates {
        for (sep, _name) in [SEPARATORS[0], SEPARATORS[1], SEPARATORS[4], SEPARATORS[6]] {
            cases.push(GenCase::body_only(template(sep)));
        }
    }
    cases
}

/// Comment-bearing corpus (#8097, gap 3): bot-authored exclusion across every
/// login spelling the port and the shell both normalise, `OWN_MARKERS`
/// exclusion, multi-comment concatenation, and cross-source dedup.
fn phase2_comment_bearing() -> Vec<GenCase> {
    let mut cases = Vec::new();

    // Every login spelling `normalise_login` collapses to `loom-bot`, plus
    // two controls that must NOT collapse to it: a distinct bot identity and
    // an absent/empty login (`gh` omits the author field on some shapes).
    let authors: [(&str, &str); 8] = [
        ("loom-bot", "exact"),
        ("LOOM-BOT", "case"),
        ("app/loom-bot", "app-prefix"),
        ("loom-bot[bot]", "bot-suffix"),
        ("APP/LOOM-BOT[BOT]", "both, mixed case"),
        ("alice", "ordinary human, not the bot"),
        ("some-other-bot[bot]", "a DIFFERENT bot identity"),
        ("", "absent/empty login"),
    ];
    let markers = [
        None,
        Some("<!-- curator:dep-recheck:abc -->"),
        Some("<!-- curator:operator-premise-recheck:abc -->"),
    ];

    for (login, _why) in authors {
        for marker in markers {
            let comment_body = match marker {
                Some(m) => format!("{m}\nRequires #77"),
                None => "Requires #77".to_string(),
            };
            // Body carries nothing of its own: isolates what the comment alone
            // contributes.
            cases.push(GenCase::with_comments(
                "See the discussion below.",
                vec![(login, &comment_body)],
            ));
            // Body ALSO carries its own reference: proves comment filtering
            // never touches the body, and that surviving comment refs dedup
            // and merge with body refs rather than replacing them.
            cases.push(GenCase::with_comments("Blocked by #5", vec![(login, &comment_body)]));
        }
    }

    // Multi-comment concatenation: a mix of excluded and included comments in
    // one issue, exercising the join order and per-comment `\n` separator.
    cases.push(GenCase::with_comments(
        "Blocked by #3",
        vec![
            ("loom-bot", "Requires #999"), // excluded: bot author
            ("a-human", "Depends on #12"), // included
        ],
    ));
    cases.push(GenCase::with_comments(
        "**Epic** #1",
        vec![
            ("a-human", "<!-- curator:dep-recheck:x -->\nDepends on #55"), // excluded: marker
            ("a-human", "Requires #61"),                                   // included
            ("app/loom-bot", "Blocked by #62"),                            // excluded: bot author
        ],
    ));
    cases.push(GenCase::with_comments(
        "Requires #8",
        vec![("alice", "Blocked by #008")], // cross-source dedup: 8 == 008
    ));
    // The artificial `\n` the extractor inserts BETWEEN body and an included
    // comment can itself span a phrase/reference split — the same
    // newline-spanning divergence `extract.rs` documents, this time at the
    // body/comment boundary rather than inside a single field.
    cases.push(GenCase::with_comments("Nothing here yet. Requires", vec![("alice", "#33")]));
    cases.push(GenCase::with_comments(
        "Depends on",
        vec![("loom-bot", "#999"), ("a-human", "#14")],
    ));
    // Separator diversity inside a comment body, not just the issue body —
    // for both an included (`a-human`) and an excluded (`loom-bot`) author,
    // so the separator-in-comment matrix is exercised on both sides of the
    // exclusion decision.
    for (sep, _name) in SEPARATORS {
        cases.push(GenCase::with_comments(
            "unrelated",
            vec![("a-human", &format!("Requires{sep}#91"))],
        ));
        cases.push(GenCase::with_comments(
            "Depends on #2",
            vec![("loom-bot", &format!("Blocked by{sep}#91"))],
        ));
        cases.push(GenCase::with_comments(
            "unrelated",
            vec![
                ("a-human", &format!("Requires{sep}#91")),
                ("app/loom-bot", &format!("Depends on{sep}#92")),
            ],
        ));
    }

    cases
}

fn build_corpus() -> Vec<GenCase> {
    let mut all = Vec::new();
    all.extend(phase1_separator_grid());
    all.extend(phase1b_near_misses());
    all.extend(phase1c_multi_reference_bodies());
    all.extend(phase2_comment_bearing());

    // Dedup: some templates legitimately coincide (e.g. two near-miss phrases
    // rendering the same text). A differential corpus counts distinct inputs,
    // not distinct generation paths.
    let mut seen = BTreeSet::new();
    all.retain(|c| seen.insert(c.clone()));
    all
}

/// Recover the retired shell from git history into a temp file, returning its
/// path. The caller is responsible for cleanup (a `tempfile::NamedTempFile`
/// removes itself on drop).
fn recover_reference_shell(repo_root: &Path) -> tempfile::NamedTempFile {
    let rev_path = format!("{REFERENCE_REV}:{REFERENCE_PATH}");
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(["show", &rev_path])
        .output()
        .unwrap_or_else(|e| panic!("failed to run `git show {rev_path}`: {e}"));
    assert!(
        output.status.success(),
        "`git show {rev_path}` failed — this checkout is likely shallow and does not contain \
         the reference commit. Run `git fetch --unshallow` (or deepen enough to include \
         {REFERENCE_REV}) and retry.\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !output.stdout.is_empty(),
        "`git show {rev_path}` returned no content — has the reference path moved?"
    );

    let mut f = tempfile::NamedTempFile::new().expect("create temp file for recovered shell");
    f.write_all(&output.stdout).expect("write recovered shell");
    f.flush().expect("flush recovered shell");
    f
}

/// Run one case through the recovered shell's `extract-refs --stdin --json`
/// and return its `refs` string.
fn shell_refs_for(shell_path: &Path, case: &GenCase) -> String {
    let comments: Vec<Value> = case
        .comments
        .iter()
        .map(|(login, body)| json!({"author": {"login": login}, "body": body}))
        .collect();
    let input = json!({"body": case.body, "comments": comments});

    let mut child = Command::new("bash")
        .arg(shell_path)
        .args([
            "extract-refs",
            "--stdin",
            "--bot-login",
            BOT_LOGIN,
            "--json",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn recovered shell");

    child
        .stdin
        .take()
        .expect("child stdin")
        .write_all(input.to_string().as_bytes())
        .expect("write case JSON to shell stdin");

    let output = child.wait_with_output().expect("wait for shell");
    assert!(
        output.status.success(),
        "recovered shell failed on case {case:?}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let parsed: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "shell output was not valid JSON for case {case:?}: {e}\nstdout: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    parsed["refs"]
        .as_str()
        .unwrap_or_else(|| panic!("shell JSON had no 'refs' string: {parsed}"))
        .to_string()
}

fn case_to_row(case: &GenCase, shell_refs: &str) -> Value {
    let comments: Vec<Value> = case
        .comments
        .iter()
        .map(|(login, body)| json!({"author": {"login": login}, "body": body}))
        .collect();
    let mut row = Map::new();
    row.insert("body".to_string(), Value::String(case.body.clone()));
    row.insert("comments".to_string(), Value::Array(comments));
    row.insert("shell_refs".to_string(), Value::String(shell_refs.to_string()));
    Value::Object(row)
}

fn meta_row(case_count: usize) -> Value {
    let corpus_desc = format!(
        "{case_count} inputs deterministically enumerated (no RNG) from the grammar \
         extract-refs parses: the four trigger phrases plus near-misses and multi-reference \
         bodies; the FULL seven-member [[:space:]] separator alphabet -- space, \\n, \\t, \\r, \
         \\x0b (VT), \\x0c (FF), and U+00A0 NBSP (the one separator the port's `[[:space:]]` \
         class and the retired shell's `[[:space:]]` deliberately do NOT match -- both sides \
         agree by finding nothing, proving the #8097 fix rather than merely asserting it); \
         reference-number boundaries (0, a leading-zero form, the u64 limit, one past it, deep \
         overflow); and comment-bearing inputs covering bot-authored exclusion across five login \
         spellings (exact, case-insensitive, app/-prefixed, [bot]-suffixed, both combined), a \
         DISTINCT non-bot [bot]-suffixed author, an absent login, OWN_MARKERS-carrying comments, \
         multi-comment concatenation, cross-source (body+comment) dedup, and the newline the \
         extractor itself inserts at the body/comment boundary."
    );
    json!({
        "_meta": {
            "purpose": "Frozen answers from the PRE-PORT shell for dep-recheck-fingerprint extract-refs, used by loom-daemon/tests/differential_extract_refs.rs (epic #7810, filed from #8072; corpus gaps closed by #8097).",
            "reference_impl": format!("defaults/scripts/dep-recheck-fingerprint.sh at git rev {REFERENCE_REV} (821 lines, the last commit before the port in #7969)"),
            "subcommand": format!("extract-refs --stdin --bot-login {BOT_LOGIN} --json"),
            "bot_login": BOT_LOGIN,
            "corpus": corpus_desc,
            "regenerate": "cargo run -p loom-daemon --example generate_extract_refs_oracle (loom-daemon/examples/generate_extract_refs_oracle.rs, #8097). Requires bash, jq, and a git history deep enough to contain the reference rev above; recovers the shell from git history on every run rather than vendoring it. Do NOT hand-edit this file.",
            "frozen": "Never edit by hand — regenerate via the command in `regenerate` above. The shell is retired; these answers are the historical ground truth.",
        }
    })
}

fn main() {
    let repo_root: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("loom-daemon has a parent dir")
        .to_path_buf();
    let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/extract_refs_shell_oracle.jsonl");

    eprintln!("Recovering the retired shell ({REFERENCE_REV}:{REFERENCE_PATH})...");
    let shell = recover_reference_shell(&repo_root);

    let corpus = build_corpus();
    eprintln!("Generated {} distinct cases. Running each through the shell...", corpus.len());

    let mut out = std::fs::File::create(&fixture_path).expect("create fixture file");
    writeln!(out, "{}", meta_row(corpus.len())).expect("write meta row");

    let mut with_comments = 0usize;
    for (i, case) in corpus.iter().enumerate() {
        if !case.comments.is_empty() {
            with_comments += 1;
        }
        let shell_refs = shell_refs_for(shell.path(), case);
        let row = case_to_row(case, &shell_refs);
        writeln!(out, "{row}").expect("write case row");
        if (i + 1) % 100 == 0 {
            eprintln!("  {}/{}", i + 1, corpus.len());
        }
    }

    eprintln!(
        "Wrote {} cases ({} comment-bearing) to {}",
        corpus.len(),
        with_comments,
        fixture_path.display()
    );
}
