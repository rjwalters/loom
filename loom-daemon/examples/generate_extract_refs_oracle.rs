//! Extends `tests/fixtures/extract_refs_shell_oracle.jsonl` — the frozen
//! oracle `tests/differential_extract_refs.rs` replays `extract()` against
//! (epic #7810, filed from #8072; corpus gaps closed by #8097, reconciled
//! with #8094's comment corpus in #8136).
//!
//! # Run
//!
//! ```text
//! cargo run -p loom-daemon --example generate_extract_refs_oracle
//! ```
//!
//! Rewrites `loom-daemon/tests/fixtures/extract_refs_shell_oracle.jsonl` in
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
//! # Extending a frozen oracle is ADDITIVE, not a regeneration
//!
//! `verification-recipes.md` §6 states the rule this tool implements: replay
//! the existing cases against the same pinned rev to confirm they still
//! reproduce, then **append** the new cases and their own provenance record
//! below them. The two blocks already in the fixture (#8072's 700 body-only
//! cases and #8094's 350 comment-bearing ones) are historical ground truth
//! captured from an implementation that no longer exists; re-deriving them
//! from a generator that was never committed is impossible, and *replacing*
//! them with a freshly enumerated corpus would silently drop whatever they
//! covered that the new enumeration does not. So this tool:
//!
//! 1. Copies every record above its own `_meta` marker through byte for byte.
//! 2. Replays each of those preserved cases through the recovered shell —
//!    with that case's own `--bot-login` — and aborts on the first mismatch.
//!    A preserved block that no longer reproduces is a finding, not something
//!    to quietly rewrite.
//! 3. Enumerates and appends its own block, with its own `_meta` record.
//!
//! # What this tool's own block adds (#8097 / #8136)
//!
//! 1. **The full separator alphabet.** The two preserved blocks between them
//!    contain space, `\n` and `\t` only — three of the seven characters
//!    `[[:space:]]` defines, even though `_meta.corpus` claimed "every
//!    separator". This block enumerates all six ASCII ones (space, `\n`,
//!    `\t`, `\r`, `\x0b` VT, `\x0c` FF) **plus NBSP `\u{00A0}`**, which is
//!    the one character the two implementations used to disagree on: GNU
//!    grep's `[[:space:]]` excludes it, the `regex` crate's Unicode-aware
//!    `\s` included it. `phrase_re()` now spells the class as the ASCII
//!    POSIX `[[:space:]]` and the NBSP cases here prove both sides agree by
//!    finding nothing — see `extract.rs`'s `phrase_re` doc.
//! 2. **Per-case `--bot-login`.** Like #8094's block (and unlike this
//!    generator's first version, which hard-coded one already-normalised
//!    login and so could never reach the `BotLoginNormalisation`
//!    divergence), every comment-bearing case here names the `--bot-login`
//!    it was replayed with, enumerated across all seven spellings
//!    `normalise_login` collapses. Combined with axis 1 that makes this
//!    block a genuine superset of both parents' axes rather than a union
//!    that drops one.
//! 3. **Comment-bearing inputs** covering bot-authored exclusion across
//!    every login spelling, a distinct non-bot `[bot]`-suffixed author, an
//!    absent login, `OWN_MARKERS`-carrying comments, multi-comment
//!    concatenation, cross-source (body+comment) dedup, and the newline the
//!    extractor itself inserts at the body/comment seam.
//!
//! Enumeration here is fully deterministic (nested loops over the grammar
//! dimensions above) — no PRNG, no "seed" to keep synchronized with anything
//! else. The first fixture block's `_meta` named a seed for a generator that
//! was never committed; recording provenance that cannot be checked is
//! exactly the failure #8097 was filed to close, so this generator does not
//! repeat it. Re-running it on an unchanged tree reproduces the fixture byte
//! for byte.

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
/// test asserts every `_meta.bot_login` equals it, and it is the default for
/// any case that does not name a `--bot-login` of its own.
const BOT_LOGIN: &str = "loom-bot";

/// Marks this generator's own `_meta` record. Everything ABOVE the line
/// carrying it is preserved verbatim (see the module docs); everything from
/// it down is this tool's output. A literal sentinel rather than "the last
/// `_meta` line" so that a future third extension appending its own block
/// below cannot silently make this tool eat it.
const BLOCK_ID: &str = "8097-separator-alphabet-and-bot-login-variants";

/// Floor on the preserved prefix, as a sanity check against a truncated or
/// hand-mangled fixture: the two blocks this tool appends below are 700 +
/// 350 cases plus their two `_meta` records.
const MIN_PRESERVED_LINES: usize = 1052;

/// One generated case, before the shell has answered it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct GenCase {
    body: String,
    /// `(author login, comment body)`, in forge order.
    comments: Vec<(String, String)>,
    /// The `--bot-login` this case is replayed with. Almost always
    /// [`BOT_LOGIN`]; a non-normalised spelling is what makes the
    /// `BotLoginNormalisation` divergence reachable.
    bot_login: String,
}

impl GenCase {
    fn body_only(body: impl Into<String>) -> Self {
        Self {
            body: body.into(),
            comments: Vec::new(),
            bot_login: BOT_LOGIN.to_string(),
        }
    }

    fn with_comments(body: impl Into<String>, comments: Vec<(&str, &str)>) -> Self {
        Self {
            body: body.into(),
            comments: comments
                .into_iter()
                .map(|(l, b)| (l.to_string(), b.to_string()))
                .collect(),
            bot_login: BOT_LOGIN.to_string(),
        }
    }

    fn replayed_with(mut self, bot_login: &str) -> Self {
        self.bot_login = bot_login.to_string();
        self
    }
}

/// The four machine-readable trigger phrases `extract()`/the shell parse,
/// reused verbatim from `phrase_re()`'s pattern.
const TRIGGERS: [&str; 4] = ["Blocked by", "Depends on", "Requires", "**Epic**"];

/// The full separator alphabet: every character the port's `[[:space:]]`
/// class and the retired shell's `grep -oE '[[:space:]]'` both match, plus
/// NBSP — the one character neither matches (post-#8097), included so the
/// corpus proves the two sides agree rather than leaving the class untested.
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

/// Every `--bot-login` spelling this block replays comment-bearing cases
/// with. The first is already normalised (so port and shell agree by
/// construction, the control); the other six are the spellings for which the
/// shell's lower-case-only treatment of the flag diverges from the port's
/// full `normalise_login`. Kept identical to #8094's
/// `_meta.bot_login_variants` so the two blocks exercise the same flag
/// alphabet.
const BOT_LOGIN_VARIANTS: [&str; 7] = [
    "loom-bot",
    "app/loom-bot",
    "loom-bot[bot]",
    "LOOM-BOT",
    "app/LOOM-BOT",
    "Loom-Bot[bot]",
    "app/loom-bot[bot]",
];

/// Comment author logins: the five spellings `normalise_login` collapses to
/// `loom-bot`, plus three controls that must NOT collapse to it.
const COMMENT_AUTHORS: [(&str, &str); 8] = [
    ("loom-bot", "exact"),
    ("LOOM-BOT", "case"),
    ("app/loom-bot", "app-prefix"),
    ("loom-bot[bot]", "bot-suffix"),
    ("APP/LOOM-BOT[BOT]", "both, mixed case"),
    ("alice", "ordinary human, not the bot"),
    ("some-other-bot[bot]", "a DIFFERENT bot identity"),
    ("", "absent/empty login"),
];

/// `OWN_MARKERS` and their absence — the belt-and-suspenders exclusion that
/// applies whatever login the forge reports.
const OWN_MARKERS: [Option<&str>; 3] = [
    None,
    Some("<!-- curator:dep-recheck:abc -->"),
    Some("<!-- curator:operator-premise-recheck:abc -->"),
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

/// Comment-bearing corpus (#8097 gap 3), crossed with the `--bot-login`
/// alphabet (#8136).
///
/// The `bot_login` axis is what makes the fourth divergence reachable at all:
/// the shell compared a comment's normalised author against a merely
/// LOWER-CASED `--bot-login`, the port normalises both. For an
/// already-normalised flag (`loom-bot`) the two rules are the same predicate
/// and no case can distinguish them — which is exactly why this generator's
/// first version, with one fixed constant, left `BotLoginNormalisation`
/// structurally unreachable no matter how many comments it generated.
fn phase2_comment_bearing() -> Vec<GenCase> {
    let mut cases = Vec::new();

    for bot_login in BOT_LOGIN_VARIANTS {
        for (login, _why) in COMMENT_AUTHORS {
            for marker in OWN_MARKERS {
                let comment_body = match marker {
                    Some(m) => format!("{m}\nRequires #77"),
                    None => "Requires #77".to_string(),
                };
                // Body carries nothing of its own: isolates what the comment
                // alone contributes, so an inclusion/exclusion difference is
                // visible as a whole reference appearing or vanishing.
                cases.push(
                    GenCase::with_comments(
                        "See the discussion below.",
                        vec![(login, &comment_body)],
                    )
                    .replayed_with(bot_login),
                );
                // Body ALSO carries its own reference: proves comment
                // filtering never touches the body, and that surviving
                // comment refs dedup and merge with body refs rather than
                // replacing them.
                cases.push(
                    GenCase::with_comments("Blocked by #5", vec![(login, &comment_body)])
                        .replayed_with(bot_login),
                );
            }
        }
    }

    // Multi-comment concatenation: a mix of excluded and included comments in
    // one issue, exercising the join order and per-comment `\n` separator.
    // Replayed under both a normalised and a non-normalised flag, so the same
    // structural shape is seen on both sides of the selection divergence.
    for bot_login in ["loom-bot", "app/loom-bot", "Loom-Bot[bot]"] {
        cases.push(
            GenCase::with_comments(
                "Blocked by #3",
                vec![
                    ("loom-bot", "Requires #999"), // bot author
                    ("a-human", "Depends on #12"), // included
                ],
            )
            .replayed_with(bot_login),
        );
        cases.push(
            GenCase::with_comments(
                "**Epic** #1",
                vec![
                    ("a-human", "<!-- curator:dep-recheck:x -->\nDepends on #55"), // marker
                    ("a-human", "Requires #61"),                                   // included
                    ("app/loom-bot", "Blocked by #62"),                            // bot author
                ],
            )
            .replayed_with(bot_login),
        );
        cases.push(
            GenCase::with_comments("Requires #8", vec![("alice", "Blocked by #008")])
                .replayed_with(bot_login), // cross-source dedup: 8 == 008
        );
        // The artificial `\n` the extractor inserts BETWEEN body and an
        // included comment can itself span a phrase/reference split — the same
        // newline-spanning divergence `extract.rs` documents, this time at the
        // body/comment boundary rather than inside a single field.
        cases.push(
            GenCase::with_comments("Nothing here yet. Requires", vec![("alice", "#33")])
                .replayed_with(bot_login),
        );
        cases.push(
            GenCase::with_comments("Depends on", vec![("loom-bot", "#999"), ("a-human", "#14")])
                .replayed_with(bot_login),
        );
    }

    // Separator diversity inside a comment body, not just the issue body —
    // for both an included (`a-human`) and an excluded (`loom-bot`) author,
    // so the separator-in-comment matrix is exercised on both sides of the
    // exclusion decision, and under a flag spelling that moves the exclusion
    // boundary as well as one that does not.
    for bot_login in ["loom-bot", "app/loom-bot"] {
        for (sep, _name) in SEPARATORS {
            cases.push(
                GenCase::with_comments(
                    "unrelated",
                    vec![("a-human", &format!("Requires{sep}#91"))],
                )
                .replayed_with(bot_login),
            );
            cases.push(
                GenCase::with_comments(
                    "Depends on #2",
                    vec![("loom-bot", &format!("Blocked by{sep}#91"))],
                )
                .replayed_with(bot_login),
            );
            cases.push(
                GenCase::with_comments(
                    "unrelated",
                    vec![
                        ("a-human", &format!("Requires{sep}#91")),
                        ("app/loom-bot", &format!("Depends on{sep}#92")),
                    ],
                )
                .replayed_with(bot_login),
            );
        }
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

/// Run one `(body, comments, bot_login)` triple through the recovered shell's
/// `extract-refs --stdin --json` and return its `refs` string.
fn shell_refs_for(
    shell_path: &Path,
    body: &str,
    comments: &[(String, String)],
    bot_login: &str,
) -> String {
    let comments_json: Vec<Value> = comments
        .iter()
        .map(|(login, body)| json!({"author": {"login": login}, "body": body}))
        .collect();
    let input = json!({"body": body, "comments": comments_json});

    let mut child = Command::new("bash")
        .arg(shell_path)
        .args([
            "extract-refs",
            "--stdin",
            "--bot-login",
            bot_login,
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
        "recovered shell failed on body={body:?} comments={comments:?} bot_login={bot_login:?}\
         \nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let parsed: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "shell output was not valid JSON for body={body:?}: {e}\nstdout: {}",
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
    if !comments.is_empty() {
        row.insert("comments".to_string(), Value::Array(comments));
        // Only a comment-bearing case can observe the flag; recording it on a
        // body-only row would assert provenance the row cannot demonstrate.
        row.insert("bot_login".to_string(), Value::String(case.bot_login.clone()));
    }
    row.insert("shell_refs".to_string(), Value::String(shell_refs.to_string()));
    Value::Object(row)
}

fn meta_row(case_count: usize, comment_bearing: usize) -> Value {
    let corpus_desc = format!(
        "{case_count} inputs ({comment_bearing} comment-bearing) deterministically enumerated \
         (no RNG, no seed) from the grammar extract-refs parses, extending the two blocks above \
         along the two axes neither of them covers. (1) The FULL seven-member separator \
         alphabet -- space, \\n, \\t, \\r, \\x0b (VT), \\x0c (FF), and U+00A0 NBSP -- crossed \
         with the four trigger phrases and reference-number boundaries (0, a leading-zero form, \
         the u64 limit, one past it, deep overflow); the 1050 records above contain only space, \
         \\n and \\t. NBSP is the one separator the port's `[[:space:]]` class and the retired \
         shell's `[[:space:]]` deliberately do NOT match -- both sides agree by finding nothing, \
         proving the #8097 fix rather than merely asserting it. (2) Comment-bearing cases \
         crossed with all seven --bot-login spellings (see bot_login_variants), eight comment \
         author logins (the five that normalise to the bot identity, a human, a DISTINCT \
         [bot]-suffixed identity, and an absent login), and OWN_MARKERS presence -- plus \
         near-miss phrases, multi-reference bodies, multi-comment concatenation, cross-source \
         (body+comment) dedup, and the newline the extractor itself inserts at the body/comment \
         seam. Each comment-bearing case names the --bot-login it was replayed with."
    );
    json!({
        "_meta": {
            "block_id": BLOCK_ID,
            "purpose": "Extension (#8097, reconciled with #8094's block in #8136): the full [[:space:]] separator alphabet including NBSP, and comment-bearing cases crossed with every --bot-login spelling. Unlike the two blocks above, this one has a COMMITTED generator -- regenerating it is one command, not archaeology.",
            "reference_impl": format!("unchanged -- defaults/scripts/dep-recheck-fingerprint.sh at git rev {REFERENCE_REV} (821 lines, the last commit before the port in #7969). The 1050 records above were replayed against this same rev while generating these, with 0 mismatches; the generator aborts rather than rewrite a preserved record that no longer reproduces."),
            "subcommand": "extract-refs --stdin --bot-login <the case's own bot_login, or the default below> --json",
            "bot_login": BOT_LOGIN,
            "bot_login_variants": BOT_LOGIN_VARIANTS,
            "corpus": corpus_desc,
            "regenerate": "cargo run -p loom-daemon --example generate_extract_refs_oracle (loom-daemon/examples/generate_extract_refs_oracle.rs, #8097). Requires bash, jq, and a git history deep enough to contain the reference rev above; recovers the shell from git history on every run rather than vendoring it. It preserves every record ABOVE this _meta line byte for byte (re-verifying each against the shell) and regenerates everything from this line down. Do NOT hand-edit this file.",
            "frozen": "Never edit by hand -- regenerate via the command in `regenerate` above. The shell is retired; these answers are the historical ground truth.",
        }
    })
}

/// Everything above this generator's own `_meta` line, verbatim, plus each
/// preserved case paired with the answer the file records for it, parsed out
/// for re-verification.
struct Preserved {
    text: String,
    cases: Vec<(GenCase, String)>,
}

fn read_preserved(fixture_path: &Path) -> Preserved {
    let existing = std::fs::read_to_string(fixture_path).unwrap_or_else(|e| {
        panic!(
            "cannot read {} to preserve its frozen blocks: {e} — this tool EXTENDS the oracle, \
             it does not recreate it (see verification-recipes.md §6)",
            fixture_path.display()
        )
    });

    let mut text = String::new();
    let mut cases = Vec::new();
    let mut metas = 0usize;
    for line in existing.lines() {
        if line.contains(BLOCK_ID) {
            break;
        }
        text.push_str(line);
        text.push('\n');
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("preserved fixture line is not JSON: {e}\n{line}"));
        if v.get("_meta").is_some() {
            metas += 1;
            continue;
        }
        let comments = v["comments"]
            .as_array()
            .map(|cs| {
                cs.iter()
                    .map(|c| {
                        (
                            c["author"]["login"]
                                .as_str()
                                .unwrap_or_default()
                                .to_string(),
                            c["body"].as_str().unwrap_or_default().to_string(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        cases.push((
            GenCase {
                body: v["body"].as_str().expect("preserved case body").to_string(),
                comments,
                bot_login: v["bot_login"].as_str().unwrap_or(BOT_LOGIN).to_string(),
            },
            v["shell_refs"]
                .as_str()
                .expect("preserved case shell_refs")
                .to_string(),
        ));
    }

    assert!(
        metas >= 2 && cases.len() + metas >= MIN_PRESERVED_LINES,
        "only {} preserved records ({metas} _meta) above the {BLOCK_ID} marker — the frozen \
         blocks look truncated, refusing to write a fixture that silently drops them",
        cases.len()
    );
    Preserved { text, cases }
}

fn main() {
    let repo_root: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("loom-daemon has a parent dir")
        .to_path_buf();
    let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/extract_refs_shell_oracle.jsonl");

    let preserved = read_preserved(&fixture_path);
    eprintln!("Recovering the retired shell ({REFERENCE_REV}:{REFERENCE_PATH})...");
    let shell = recover_reference_shell(&repo_root);

    eprintln!(
        "Re-verifying {} preserved cases against the reference rev...",
        preserved.cases.len()
    );
    let mut reverified = 0usize;
    for (i, (case, expected)) in preserved.cases.iter().enumerate() {
        let got = shell_refs_for(shell.path(), &case.body, &case.comments, &case.bot_login);
        assert_eq!(
            &got,
            expected,
            "preserved case {} no longer reproduces against {REFERENCE_REV}: body={:?} \
             comments={:?} bot_login={:?} — that is a finding about the reference recovery, not \
             something to rewrite silently",
            i + 1,
            case.body,
            case.comments,
            case.bot_login
        );
        reverified += 1;
        if reverified.is_multiple_of(200) {
            eprintln!("  re-verified {reverified}/{}", preserved.cases.len());
        }
    }
    eprintln!("  {reverified}/{reverified} preserved cases reproduce exactly.");

    let corpus = build_corpus();
    let comment_bearing = corpus.iter().filter(|c| !c.comments.is_empty()).count();
    eprintln!(
        "Enumerated {} new cases ({comment_bearing} comment-bearing). Running each through the \
         shell...",
        corpus.len()
    );

    let mut out = String::with_capacity(preserved.text.len() * 2);
    out.push_str(&preserved.text);
    out.push_str(&meta_row(corpus.len(), comment_bearing).to_string());
    out.push('\n');

    for (i, case) in corpus.iter().enumerate() {
        let shell_refs = shell_refs_for(shell.path(), &case.body, &case.comments, &case.bot_login);
        out.push_str(&case_to_row(case, &shell_refs).to_string());
        out.push('\n');
        if (i + 1).is_multiple_of(200) {
            eprintln!("  {}/{}", i + 1, corpus.len());
        }
    }

    std::fs::write(&fixture_path, &out).expect("write fixture file");
    eprintln!(
        "Wrote {} preserved + {} new cases to {}",
        preserved.cases.len(),
        corpus.len(),
        fixture_path.display()
    );
}
