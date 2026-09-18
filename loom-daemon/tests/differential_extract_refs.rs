//! Differential test: the ported `extract-refs` against a frozen oracle of the
//! **pre-port shell's** answers (epic #7810, filed from #8072).
//!
//! # Why this exists
//!
//! The port method for epic #7810 is "keep the shell's test suite and run its
//! assertions unchanged against the Rust" — a retained black-box suite as the
//! equivalence proof. #8011 showed the limit of that method: the `dep_recheck`
//! port shipped **three** silent behavioural divergences while its retained
//! suite was 104/104 green. A retained suite proves only what its author
//! thought to write down, and nobody writes down the input they did not
//! imagine. All three escapees were inputs nobody imagined.
//!
//! Differential testing closes exactly that gap, because the corpus is
//! *generated* from the grammar the implementation parses rather than
//! hand-picked. This test replays 1050 such inputs and asserts the port differs
//! from the shell in only the ways we have deliberately accepted.
//!
//! # Why an oracle file rather than running the shell
//!
//! The pre-port shell is gone from the tree (that was the point). Running it
//! here would mean either a `git show` against a pinned rev — which breaks
//! under CI's default shallow checkout — or vendoring 821 lines of dead shell
//! plus a runtime `bash` **and** `jq` dependency into `cargo test`. Instead the
//! shell's answers are frozen into a fixture once, by the documented command in
//! its `_meta` records, and this test needs no shell at all.
//!
//! # What a failure here means
//!
//! `UNEXPLAINED` means the port now differs from the retired shell in a way
//! nobody has classified. That is the #8011 shape recurring: read the reported
//! case, decide whether the new behaviour is right, then either fix it or add
//! the class here **with** its reasoning. Do not silence it by widening a class.
//!
//! The per-class counts are a characterization, not a contract — they change
//! when `extract()` legitimately changes. The zero-`UNEXPLAINED` assertion is
//! the real invariant.
//!
//! # Each class is recognised by its mechanism
//!
//! Review caught the first version of this test recognising the newline class as
//! "the port found an extra reference and the body contains a newline". That is
//! a property of the INPUT, and 88% of these bodies contain a newline, so the
//! class absorbed an extra reference from any cause — measured against the
//! case-insensitivity mutation below, it caught 9 of the 116 cases the mutation
//! actually changed. [`newline_only_refs`] now computes what the class can
//! genuinely produce, and the extras must be a subset of exactly that: same
//! green baseline, 116 of 116 caught.
//!
//! A class whose test is a property of the input is a hole shaped like a class.
//!
//! # A field the corpus never populates is a half the suite never tests
//!
//! The first version of this corpus (700 cases) carried a `body` and nothing
//! else, so every case ran with `comments: []`. `extract()`'s comment half —
//! `comment_counts()` and `normalise_login()`, i.e. the self-comment loop
//! suppression this subcommand exists for — was therefore never reached.
//! Mutation-measured (#8094): gutting `comment_counts()` to `true`, or making
//! `normalise_login()` the identity, left the suite **green**. The body half
//! discriminated well and the comment half did not discriminate at all, and a
//! green run cannot tell those two apart.
//!
//! The corpus now carries 350 comment-bearing cases generated from the comment
//! grammar too (bot- and human-authored, `app/`-prefixed and `[bot]`-suffixed
//! spellings on both the author *and* the supplied `--bot-login`, own-marker
//! comments and their near-misses, and references reachable only through a
//! comment). Both mutations now turn it red, and the test asserts that
//! discriminating power directly — as a measured property of the corpus — so it
//! cannot quietly decay back to zero.

use loom_daemon::dep_recheck::extract::{extract, Author, Comment, Input};

/// Frozen answers from the pre-port shell. See the `_meta` records.
const ORACLE: &str = include_str!("fixtures/extract_refs_shell_oracle.jsonl");

/// The `--bot-login` the oracle was generated with for every case that does not
/// name its own. Must match `_meta.bot_login`.
const BOT_LOGIN: &str = "loom-bot";

/// The automation's own re-check markers, spelled as the shell's `jq` `test()`
/// regex spelled them. A local copy on purpose — see [`model_counts_for_port`].
const OWN_MARKERS: [&str; 2] = [
    "<!-- curator:dep-recheck:",
    "<!-- curator:operator-premise-recheck:",
];

/// The four accepted ways the port differs from the shell it replaced.
#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
enum Divergence {
    /// The shell matched with `grep -oE`, which is line-oriented and cannot
    /// span a newline; the Rust regex runs over the whole text with `\s`
    /// inside `[*_:\s]*` matching `\n`. So `"Blocked by\n#42"` matches here and
    /// did not there. Documented and kept in `extract.rs` (#8011): missing a
    /// genuine declared reference is the worse failure for a check whose whole
    /// job is finding one.
    NewlineSpan,
    /// `#007`. The shell's `sort -un` sorts numerically but prints the ORIGINAL
    /// token, so it emitted `007`; the Rust parses to `u64` and prints `7`.
    /// Both denote issue 7, and GitHub resolves `#007` to issue 7, so the Rust
    /// spelling is the more correct one — but it does change `CONCLUSION_HASH`
    /// for any text using a zero-padded reference.
    LeadingZero,
    /// A `#N` above `u64::MAX` (boundary confirmed exactly at
    /// 18446744073709551615 vs ...616). `extract()` drops it via
    /// `.parse().ok()`; the shell kept the literal token. No real issue number
    /// is 20 digits, so this is reachable only from adversarial forge text.
    /// Downstream a dropped ref is usually fail-SAFE (zero refs yields
    /// `verdict: open`, i.e. still blocked), but in a MIXED set — one merged
    /// ref plus one dropped — the verdict becomes `stale-premise` where the
    /// shell would instead have hard-errored on the unfetchable token.
    OverflowDropped,
    /// The **fourth** divergence recorded in #8011 — and the one this harness
    /// structurally could not observe until the corpus grew comments (#8094).
    /// The shell lower-cased the supplied `--bot-login` and nothing more, while
    /// normalising the `app/` / `[bot]` shape on the comment *author* alone. So
    /// a shell caller passing `--bot-login app/loom-bot` never matched a plain
    /// `loom-bot` author, and ingested the automation's own comments — the
    /// asymmetry defeated the normalisation it existed for. `extract.rs` runs
    /// both sides through `normalise_login`, so such a comment is now excluded
    /// as intended. Kept (it is a fix), and reachable only from a caller that
    /// passes `--bot-login` in a non-normalised spelling; no live caller does,
    /// which is why nothing in production changes with it.
    BotLoginNormalisation,
}

/// One frozen case.
struct Case {
    body: String,
    /// `(author login, comment body)`, in forge order.
    comments: Vec<(String, String)>,
    /// The `--bot-login` this case's oracle answer was generated with.
    bot_login: String,
    shell_refs: String,
}

fn load_oracle() -> Vec<Case> {
    let mut cases = Vec::new();
    // Every `--bot-login` spelling some `_meta` record declares. A case may not
    // use one that no provenance record admits — otherwise "the oracle was
    // generated with this flag" asserts nothing about the cases that follow.
    let mut declared_bot_logins: Vec<String> = Vec::new();
    for (i, line) in ORACLE.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let v: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("oracle line {}: {e}", i + 1));
        if v.get("_meta").is_some() {
            // Provenance record; assert it still describes this test's setup.
            let meta = &v["_meta"];
            assert_eq!(
                meta["bot_login"].as_str(),
                Some(BOT_LOGIN),
                "oracle was generated with a different default --bot-login than this test uses"
            );
            declared_bot_logins.push(BOT_LOGIN.to_string());
            if let Some(variants) = meta["bot_login_variants"].as_array() {
                for variant in variants {
                    declared_bot_logins.push(
                        variant
                            .as_str()
                            .expect("bot_login_variants entry")
                            .to_string(),
                    );
                }
            }
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
        cases.push(Case {
            body: v["body"].as_str().expect("body").to_string(),
            comments,
            bot_login: v["bot_login"].as_str().unwrap_or(BOT_LOGIN).to_string(),
            shell_refs: v["shell_refs"].as_str().expect("shell_refs").to_string(),
        });
    }
    for c in &cases {
        assert!(
            declared_bot_logins.contains(&c.bot_login),
            "case uses --bot-login {:?}, which no _meta record declares — its oracle answer \
             cannot be attributed to a documented replay command",
            c.bot_login
        );
    }
    cases
}

/// The port's login normalisation, **re-implemented here on purpose**.
///
/// Calling `extract::normalise_login` would make this test agree with the
/// implementation by construction: the mutation that replaces that function
/// with the identity would replace this expectation with it too, and the suite
/// would stay green while the rule it encodes was gone. This copy is the
/// independent statement of the rule, and the port is compared against *it*.
fn model_normalise_login(login: &str) -> String {
    let lower = login.to_ascii_lowercase();
    let no_prefix = lower.strip_prefix("app/").unwrap_or(&lower);
    no_prefix
        .strip_suffix("[bot]")
        .unwrap_or(no_prefix)
        .to_string()
}

fn carries_own_marker(body: &str) -> bool {
    OWN_MARKERS.iter().any(|m| body.contains(m))
}

/// Whether the **port** should ingest this comment: both logins normalised.
fn model_counts_for_port(author: &str, body: &str, bot_login: &str) -> bool {
    model_normalise_login(author) != model_normalise_login(bot_login) && !carries_own_marker(body)
}

/// Whether the **shell** ingested this comment: the author normalised, but the
/// supplied `--bot-login` merely lower-cased (`tr '[:upper:]' '[:lower:]'`).
fn model_counts_for_shell(author: &str, body: &str, bot_login: &str) -> bool {
    model_normalise_login(author) != bot_login.to_ascii_lowercase() && !carries_own_marker(body)
}

/// The text `extract()` should build: the body, then each ingested comment
/// preceded by a newline.
fn port_text(case: &Case) -> String {
    let mut text = case.body.clone();
    for (author, body) in &case.comments {
        if model_counts_for_port(author, body, &case.bot_login) {
            text.push('\n');
            text.push_str(body);
        }
    }
    text
}

/// The text the shell parsed: `printf '%s\n%s' "$body" "$comments_text"`, where
/// both halves came out of a command substitution (so each lost its trailing
/// newlines) and `comments_text` was `jq -r`'s newline-joined ingested bodies.
fn shell_text(case: &Case) -> String {
    let ingested: Vec<&str> = case
        .comments
        .iter()
        .filter(|(a, b)| model_counts_for_shell(a, b, &case.bot_login))
        .map(|(_, b)| b.as_str())
        .collect();
    format!(
        "{}\n{}",
        case.body.trim_end_matches('\n'),
        ingested.join("\n").trim_end_matches('\n')
    )
}

/// Run the port's *parser* over an arbitrary text with no comment filtering:
/// `extract()` on a comment-free input is exactly the parsing half of it.
fn parse_only(text: &str) -> String {
    extract(
        &Input {
            body: text.to_string(),
            comments: Vec::new(),
        },
        BOT_LOGIN,
    )
}

/// References the phrase pattern finds over the WHOLE text but not within any
/// single line — exactly what the accepted newline-spanning divergence can
/// account for, and nothing else.
///
/// The shell matched with `grep -oE`, which is line-oriented and cannot span a
/// newline; the port's regex runs over the concatenated text with `\s` (inside
/// `[*_:\s]*`) matching `\n`. Normalised through `u64` the way the port does,
/// so a zero-padded token compares equal to its canonical form.
fn newline_only_refs(text: &str) -> std::collections::BTreeSet<String> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"(Blocked by|Depends on|Requires|\*\*Epic\*\*)[*_:\s]*#([0-9]+)")
            .expect("static dependency-phrase pattern")
    });
    let nums = |text: &str| -> std::collections::BTreeSet<String> {
        re.captures_iter(text)
            .filter_map(|c| c.get(2)?.as_str().parse::<u64>().ok())
            .map(|n| n.to_string())
            .collect()
    };
    let whole = nums(text);
    let per_line: std::collections::BTreeSet<String> = text.lines().flat_map(nums).collect();
    whole.difference(&per_line).cloned().collect()
}

/// Account for the two implementations ingesting a different SET of comments.
///
/// Recognised by its mechanism, not by "this case has comments": the two
/// predicates differ in exactly one term — the shell compared against a merely
/// lower-cased `--bot-login`, the port against a fully normalised one — so a
/// selection difference is explainable only when the supplied flag was not
/// already in normalised form, and only for a comment whose author matches one
/// of those two spellings and not the other. Anything else (a comment dropped
/// for an unrelated reason, an own-marker comment suddenly ingested) is a
/// finding, not this class.
fn classify_selection(case: &Case) -> Result<Option<Divergence>, String> {
    let disagreeing: Vec<&(String, String)> = case
        .comments
        .iter()
        .filter(|(a, b)| {
            model_counts_for_port(a, b, &case.bot_login)
                != model_counts_for_shell(a, b, &case.bot_login)
        })
        .collect();
    if disagreeing.is_empty() {
        return Ok(None);
    }
    let normalised_flag = model_normalise_login(&case.bot_login);
    let lowered_flag = case.bot_login.to_ascii_lowercase();
    if normalised_flag == lowered_flag {
        return Err(format!(
            "comment selection differs although --bot-login {:?} normalises to itself, so the \
             two rules are the same predicate for this case — no accepted class explains it",
            case.bot_login
        ));
    }
    for (author, _) in &disagreeing {
        let normalised_author = model_normalise_login(author);
        if normalised_author != normalised_flag && normalised_author != lowered_flag {
            return Err(format!(
                "a comment by {author:?} is ingested by one implementation and not the other, \
                 but its login matches neither the normalised ({normalised_flag:?}) nor the \
                 lower-cased ({lowered_flag:?}) spelling of --bot-login — the bot-login \
                 normalisation divergence cannot produce that"
            ));
        }
    }
    Ok(Some(Divergence::BotLoginNormalisation))
}

/// Explain every token-level difference between the shell's answer and the
/// port's, or return `Err` describing the part that no known class covers.
///
/// `text` is the text the SHELL parsed and `rust` is the port's parser run over
/// that same text, so any comment-selection difference has already been peeled
/// off by [`classify_selection`] and what remains here is purely about parsing.
fn classify(text: &str, shell: &str, rust: &str) -> Result<Vec<Divergence>, String> {
    let mut classes = Vec::new();

    // Re-derive what the shell's tokens become under the port's own rules:
    // parse as u64, dropping anything that overflows.
    let mut normalised: Vec<String> = Vec::new();
    for tok in shell.split_whitespace() {
        match tok.parse::<u64>() {
            Ok(v) => {
                if tok != v.to_string() {
                    classes.push(Divergence::LeadingZero);
                }
                normalised.push(v.to_string());
            }
            Err(_) => classes.push(Divergence::OverflowDropped),
        }
    }
    normalised.sort_unstable_by_key(|s| s.parse::<u64>().unwrap_or(u64::MAX));
    normalised.dedup();

    let got: Vec<&str> = rust.split_whitespace().collect();

    let extra: Vec<String> = got
        .iter()
        .filter(|t| !normalised.contains(&t.to_string()))
        .map(|t| (*t).to_string())
        .collect();
    if !extra.is_empty() {
        // Recognise the class by its MECHANISM, not by a property of the input
        // that merely correlates with it. Keying this on `text.contains('\n')`
        // absorbed an extra ref from ANY cause, because 88% of these bodies
        // contain a newline — so a later change that stopped dropping overflow
        // refs, say, would have slipped through silently. Compute instead what
        // the newline divergence can actually produce, and require the extras
        // to be a subset of exactly that.
        let explainable = newline_only_refs(text);
        let unexplained: Vec<&String> =
            extra.iter().filter(|t| !explainable.contains(*t)).collect();
        if unexplained.is_empty() {
            classes.push(Divergence::NewlineSpan);
        } else {
            return Err(format!(
                "port reported {unexplained:?} which the shell did not, and the \
                 newline-spanning divergence cannot produce them: the pattern does not match \
                 them across a line break in this input either"
            ));
        }
    }

    let missing: Vec<&String> = normalised
        .iter()
        .filter(|t| !got.contains(&t.as_str()))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "port DROPPED {missing:?}, which the shell found and which parse cleanly as u64 — \
             no accepted class explains losing a declared reference"
        ));
    }

    // Same reference SET, different string. None of the accepted classes is
    // about rendering, so ordering/separator/formatting drift must not be
    // absorbed by a LeadingZero or OverflowDropped class that merely happened
    // to apply to this input.
    if extra.is_empty() && missing.is_empty() {
        let want = normalised.join(" ");
        if rust != want {
            return Err(format!(
                "port and shell agree on WHICH references were found but render them \
                 differently: expected {want:?}, got {rust:?} — no accepted class is about \
                 ordering or formatting"
            ));
        }
    }

    classes.sort_unstable_by_key(|c| format!("{c:?}"));
    classes.dedup();
    Ok(classes)
}

fn describe(case: &Case, rust: &str, why: &str) -> String {
    format!(
        "body={:?}\n    comments={:?}  bot_login={:?}\n    shell=[{}]  port=[{}]\n    {why}",
        case.body, case.comments, case.bot_login, case.shell_refs, rust
    )
}

#[test]
fn ported_extract_refs_diverges_from_the_retired_shell_only_in_known_ways() {
    let cases = load_oracle();
    assert!(
        cases.len() >= 1000,
        "oracle shrank to {} cases — a differential test that runs almost nothing passes for \
         the wrong reason",
        cases.len()
    );

    let mut agreed = 0usize;
    let mut per_class = std::collections::BTreeMap::<String, usize>::new();
    let mut unexplained = Vec::new();
    // The corpus's discriminating power over the comment half (#8094), measured
    // the way the gap itself was found: in how many cases would the two
    // mutations that used to pass silently change the port's answer?
    let mut comment_bearing = 0usize;
    let mut filter_load_bearing = 0usize;
    let mut normalisation_load_bearing = 0usize;

    for case in &cases {
        let input = Input {
            body: case.body.clone(),
            comments: case
                .comments
                .iter()
                .map(|(login, body)| Comment {
                    author: Author {
                        login: login.clone(),
                    },
                    body: body.clone(),
                })
                .collect(),
        };
        let rust = extract(&input, &case.bot_login);

        if !case.comments.is_empty() {
            comment_bearing += 1;
            // `comment_counts()` gutted to `true`: every comment ingested.
            let unfiltered = {
                let mut t = case.body.clone();
                for (_, b) in &case.comments {
                    t.push('\n');
                    t.push_str(b);
                }
                parse_only(&t)
            };
            if unfiltered != rust {
                filter_load_bearing += 1;
            }
            // `normalise_login()` made the identity: raw login comparison.
            let unnormalised = {
                let mut t = case.body.clone();
                for (a, b) in &case.comments {
                    if a != &case.bot_login && !carries_own_marker(b) {
                        t.push('\n');
                        t.push_str(b);
                    }
                }
                parse_only(&t)
            };
            if unnormalised != rust {
                normalisation_load_bearing += 1;
            }
        }

        // 1. The port must ingest the comments the documented rule ingests.
        //    Checked against this test's own copy of that rule — not against
        //    `comment_counts`/`normalise_login` themselves — so a change to
        //    either shows up here instead of moving the expectation with it.
        let modelled = parse_only(&port_text(case));
        if rust != modelled {
            unexplained.push(describe(
                case,
                &rust,
                &format!(
                    "port ingested a different set of comments than the documented rule \
                     (author vs normalised --bot-login, plus own-marker exclusion) selects: \
                     that rule yields [{modelled}]"
                ),
            ));
            continue;
        }

        if rust == case.shell_refs {
            agreed += 1;
            continue;
        }

        // 2. Peel off the comment-SELECTION divergence before looking at
        //    parsing, so that neither can be mistaken for the other.
        let mut classes = Vec::new();
        let shell_side_text = shell_text(case);
        let shell_side = parse_only(&shell_side_text);
        if shell_side != rust {
            match classify_selection(case) {
                Ok(Some(c)) => classes.push(c),
                Ok(None) => {
                    unexplained.push(describe(
                        case,
                        &rust,
                        "both implementations ingest the same comments, yet the port's answer \
                         over the shell's own text differs from its answer over its own — no \
                         accepted class explains that",
                    ));
                    continue;
                }
                Err(why) => {
                    unexplained.push(describe(case, &rust, &why));
                    continue;
                }
            }
        }

        match classify(&shell_side_text, &case.shell_refs, &shell_side) {
            Ok(parse_classes) if parse_classes.is_empty() && classes.is_empty() => {
                unexplained.push(describe(case, &rust, "outputs differ but no class applies"));
            }
            Ok(parse_classes) => {
                classes.extend(parse_classes);
                for c in classes {
                    *per_class.entry(format!("{c:?}")).or_default() += 1;
                }
            }
            Err(why) => unexplained.push(describe(case, &rust, &why)),
        }
    }

    assert!(
        unexplained.is_empty(),
        "{} of {} generated inputs diverge from the retired shell in UNCLASSIFIED ways.\n\
         This is the #8011 shape: a divergence that a retained black-box suite would not \
         have noticed.\n\n{}",
        unexplained.len(),
        cases.len(),
        unexplained
            .iter()
            .take(10)
            .map(|s| format!("  - {s}"))
            .collect::<Vec<_>>()
            .join("\n")
    );

    // The corpus must keep exercising the comment half. These three are not
    // style preferences: before #8094 all three were 0, the suite was green,
    // and both comment-half mutations passed undetected. A corpus that stops
    // discriminating now fails here instead of reverting silently to that.
    assert!(
        comment_bearing >= 300,
        "only {comment_bearing} cases carry comments — nothing else in this suite reaches the \
         comment-filter half of extract()"
    );
    assert!(
        filter_load_bearing >= 150,
        "the comment filter changes the answer in only {filter_load_bearing} cases — gutting \
         comment_counts() would go undetected again (#8094)"
    );
    assert!(
        normalisation_load_bearing >= 50,
        "login normalisation changes the answer in only {normalisation_load_bearing} cases — \
         making normalise_login() the identity would go undetected again (#8094)"
    );

    // Characterization. Update deliberately when `extract()` changes; a shift
    // here is a real behaviour change and should be explained in the commit.
    assert_eq!(agreed, 625, "cases where the port and the shell agree exactly");
    assert_eq!(per_class.get("NewlineSpan").copied().unwrap_or(0), 195);
    assert_eq!(per_class.get("LeadingZero").copied().unwrap_or(0), 137);
    assert_eq!(per_class.get("OverflowDropped").copied().unwrap_or(0), 140);
    assert_eq!(per_class.get("BotLoginNormalisation").copied().unwrap_or(0), 30);
}
