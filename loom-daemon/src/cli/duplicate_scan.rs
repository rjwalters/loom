//! `loom-daemon duplicate-scan` — the similarity scan behind
//! `defaults/scripts/check-duplicate.sh` (#8360, re-landing #8289's near-match
//! band).
//!
//! # What moved here, and why
//!
//! `check-duplicate.sh` is a `contract`-category file
//! (`scripts/shell-allowlist.txt`): its name is the invocation contract role
//! prompts and CI rely on, and its allowlist entry — like every `contract`
//! entry — says "new logic goes behind it into loom-daemon". PR #8353 added
//! the near-match band as +71 inline shell lines there (+54 more in
//! `create-issue.sh`) and the Shell Budget Ratchet correctly refused it:
//! portable (`contract`) growth has no override, because the epic the ratchet
//! enforces (#7810) exists to retire exactly that shell. This module is the
//! port the policy asks for: the keyword extraction, the true-Jaccard
//! scoring, the threshold banding, and the degenerate-result detector now
//! live in Rust, and the script keeps only the forge fetch (with its
//! rate-limit REST fallback, #4526) and the output aggregation.
//!
//! The scan is byte-for-byte the behaviour the shell had after #4409, plus
//! #8289's opt-in near-match band: candidates scoring in
//! `[--warn-threshold, --threshold)` are reported as `NEAR #<n>: ...` context
//! rows that never move the exit code and never count toward the degenerate
//! detector. The band exists because the block threshold alone was a cliff —
//! a hard `exit 3` at >= 18% similarity and total silence at 17% — and the
//! #4409 calibration says the silent zone contains a confirmed duplicate pair
//! (13%), so silence there hid real signal. Default band `[13, 18)`: 13 is
//! both the top of the observed unrelated-issue range and that second pair's
//! score, i.e. the zone where the two populations are indistinguishable.
//!
//! # Contract (the shell side depends on every line of this)
//!
//! The candidate pool arrives on **stdin** as the JSON array
//! `gh ... --json number,title,body` produces (or the REST-fallback reshaping
//! of it): it is untrusted forge text and can be megabytes, so it belongs on
//! a stream, not in argv. The query title/body arrive as arguments — GitHub
//! caps a title at 256 chars and a body at 64 KiB, so argv is safely bounded.
//!
//! stdout is exactly what the script's per-pool search functions used to
//! print, so the script's aggregation (which re-headers and splices these
//! lines across three pools) is unchanged:
//!
//! | pool | matches | no matches | degenerate (#4409) |
//! |---|---|---|---|
//! | `open-issues` | `DUPLICATE_FOUND` header + `#N: …` rows, **exit 1** | nothing, exit 0 | `NON_DISCRIMINATIVE (open issues): …`, exit 1 |
//! | `merged-prs` | [`RATE_LIMIT_FALLBACK` sentinel when `--rest-fallback`] + `PR #N: …` rows, exit 0 | nothing, exit 0 | `NON_DISCRIMINATIVE (merged PRs): …`, exit 0 |
//! | `closed-issues` | [`RATE_LIMIT_FALLBACK` sentinel when `--rest-fallback`] + `Closed #N: …` rows, exit 0 | nothing, exit 0 | `NON_DISCRIMINATIVE (closed issues): …`, exit 0 |
//!
//! With `--rest-fallback`, the open-issues `DUPLICATE_FOUND` header instead
//! carries the REST-labelling suffix the script's umbrella aggregation (and
//! its `--json` parser) match by prefix (#4526).
//!
//! Near-match rows are **not** written to stdout: that channel is a verdict
//! channel the script splices across pools, and a context-only block riding
//! it would end up inside a `DUPLICATE_FOUND` list. They go to the file named
//! by `--near-file`, as a JSON array of `{number, title, similarity}`, and
//! only when there are any (the script reads the file's existence as the
//! signal). Degenerate results suppress the band: when the scorer is not
//! separating anything, more low-confidence rows are the last thing a caller
//! needs.
//!
//! Exit codes: 0 and 1 are the answers above; 2 means "could not run"
//! (unreadable stdin, bad arguments) — never a verdict. The script maps an
//! unexpected 2 to "pool incomplete", and `create-issue.sh` fails open on it.

use std::io::Read;
use std::path::PathBuf;

use anyhow::Result;
use serde::Deserialize;

/// Mirror of `check-duplicate.sh`'s `readonly MIN_SCANNED_FOR_DEGENERATE`:
/// below 4 scanned candidates, ">50% matched" is not a meaningful signal (one
/// real match out of one candidate is trivially ">50%").
const MIN_SCANNED_FOR_DEGENERATE: usize = 4;

/// The stop-word list `check-duplicate.sh`'s `extract_keywords` filtered on,
/// carried over verbatim (including the duplicate `both` the shell list
/// carried — harmless in a membership test, kept so the lists diff cleanly).
const STOP_WORDS: &[&str] = &[
    "the", "a", "an", "is", "are", "was", "were", "be", "been", "being", "have", "has", "had",
    "do", "does", "did", "will", "would", "could", "should", "may", "might", "must", "shall",
    "can", "need", "dare", "ought", "used", "to", "of", "in", "for", "on", "with", "at", "by",
    "from", "up", "about", "into", "over", "after", "beneath", "under", "above", "and", "but",
    "or", "nor", "so", "yet", "both", "either", "neither", "not", "only", "own", "same", "than",
    "too", "very", "just", "also", "now", "here", "there", "when", "where", "why", "how", "all",
    "each", "every", "both", "few", "more", "most", "other", "some", "such", "no", "any", "this",
    "that", "these", "those", "what", "which", "who", "whom", "whose", "it", "its", "i", "me",
    "my", "we", "our", "you", "your", "he", "him", "his", "she", "her", "they", "them", "their",
    "add", "fix", "update", "remove", "change", "make", "get", "set", "new", "use", "work", "file",
    "code", "test", "error", "bug", "feature", "issue", "pr", "pull", "request",
];

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pool {
    /// The OPEN-issues pool: the only one whose matches are a verdict (exit 1)
    /// and the only one the near-match band applies to.
    OpenIssues,
    /// Recently merged PRs (`--include-merged-prs`): context, exit 0.
    MergedPrs,
    /// Recently closed issues (`--include-merged-prs`): context, exit 0.
    ClosedIssues,
}

impl Pool {
    /// Row prefix: `#`, `PR #`, or `Closed #` before the issue number.
    fn row_prefix(self) -> &'static str {
        match self {
            Pool::OpenIssues => "#",
            Pool::MergedPrs => "PR #",
            Pool::ClosedIssues => "Closed #",
        }
    }

    /// The NON_DISCRIMINATIVE line. The open-issues variant additionally
    /// carries the manual-review hint the shell version had.
    fn degenerate_line(self, matched: usize, scanned: usize, threshold: u32) -> String {
        let label = match self {
            Pool::OpenIssues => "open issues",
            Pool::MergedPrs => "merged PRs",
            Pool::ClosedIssues => "closed issues",
        };
        let suffix = match self {
            Pool::OpenIssues => " (e.g. gh issue list --search)",
            _ => "",
        };
        format!(
            "NON_DISCRIMINATIVE ({label}): {matched} of {scanned} candidates scored >= \
             {threshold}% similarity -- not discriminative, fall back to manual review{suffix}."
        )
    }
}

#[derive(clap::Args)]
pub(crate) struct DuplicateScanArgs {
    /// Which candidate pool stdin holds. Fixes the row prefix, the
    /// degenerate-message label, and whether a match is a verdict (exit 1)
    /// or context (exit 0).
    #[arg(long, value_enum)]
    pool: Pool,

    /// The new issue's title (the query).
    #[arg(long, value_name = "TEXT")]
    title: String,

    /// The new issue's body (the query's second half). Optional.
    #[arg(long, value_name = "TEXT", default_value = "")]
    body: String,

    /// Block threshold: similarity at/above this is a match, on the true
    /// Jaccard scale (default 18, check-duplicate.sh's #4409 calibration).
    #[arg(long, value_name = "N", default_value_t = 18)]
    threshold: u32,

    /// Near-match band floor (#8289): OPEN-issues candidates scoring in
    /// [N, --threshold) are reported to --near-file as context. Never a
    /// verdict: does not move the exit code and does not count toward the
    /// degenerate detector. 0, or any value >= --threshold, disables the
    /// band.
    #[arg(long, value_name = "N")]
    warn_threshold: Option<u32>,

    /// The issue being curated/probed, skipped when it appears in the pool
    /// (#4662): it always scores ~100% against itself.
    #[arg(long, value_name = "N")]
    self_issue: Option<u64>,

    /// Where to write the near-match rows (a JSON array of
    /// {number, title, similarity}). Written only when the band is active
    /// AND at least one candidate landed in it — the file's existence is the
    /// caller's signal.
    #[arg(long, value_name = "PATH")]
    near_file: Option<PathBuf>,

    /// The pool was fetched via the REST fallback (#4526): label the
    /// open-issues DUPLICATE_FOUND header / emit the merged+closed
    /// RATE_LIMIT_FALLBACK sentinel, so a reader knows the ranking basis
    /// changed.
    #[arg(long)]
    rest_fallback: bool,
}

/// One candidate from the pool. Extra fields are ignored; `number` must be
/// present and non-null (the shell skipped nulls silently, so we do too).
#[derive(Deserialize, Debug)]
struct Candidate {
    number: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: String,
}

/// A near-match row as written to `--near-file`.
#[derive(serde::Serialize, Debug, PartialEq, Eq)]
struct NearMatch {
    number: u64,
    title: String,
    similarity: u32,
}

/// The scan's answer: everything the caller prints or branches on.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ScanOutcome {
    /// Verdict text for stdout, exactly the lines the per-pool shell function
    /// printed (joined with `\n` when printed).
    pub(crate) stdout_lines: Vec<String>,
    /// The pool verdict: 1 only for open-issues matches/degenerate results,
    /// 0 otherwise.
    pub(crate) exit_code: i32,
    /// Near-band rows (open pool, band active, non-degenerate, non-empty).
    pub(crate) near_matches: Vec<NearMatch>,
}

/// `extract_keywords`, ported: lowercase, split on non-alphanumerics, drop
/// stop words and tokens under 3 characters, dedupe (sorted, like the
/// shell's `sort -u` — the sort is what makes [`jaccard_percent`]'s
/// two-pointer walk correct).
#[must_use]
pub(crate) fn extract_keywords(text: &str) -> Vec<String> {
    let mut out: Vec<String> = text
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| w.len() >= 3 && !STOP_WORDS.contains(w))
        .map(str::to_string)
        .collect();
    out.sort();
    out.dedup();
    out
}

/// True Jaccard similarity as a truncated integer percentage:
/// `matches * 100 / |union|` where `union = |a| + |b| - matches` — the exact
/// arithmetic `check-duplicate.sh`'s `calculate_similarity` performed in
/// bash's integer arithmetic (so scores match to the digit, not
/// approximately). Both inputs must be sorted deduplicated sets (see
/// [`extract_keywords`]).
#[must_use]
pub(crate) fn jaccard_percent(a: &[String], b: &[String]) -> u32 {
    let mut i = 0;
    let mut j = 0;
    let mut matches = 0usize;
    while i < a.len() && j < b.len() {
        use std::cmp::Ordering;
        match a[i].as_str().cmp(b[j].as_str()) {
            Ordering::Equal => {
                matches += 1;
                i += 1;
                j += 1;
            }
            Ordering::Less => i += 1,
            Ordering::Greater => j += 1,
        }
    }
    let union = a.len() + b.len() - matches;
    if union == 0 {
        0
    } else {
        ((matches * 100) / union) as u32
    }
}

/// The banding decision, pure: given query keywords and candidates (already
/// parsed), produce the outcome the caller prints. The probed issue itself
/// (when given) is skipped before scoring (#4662).
#[must_use]
#[allow(clippy::too_many_arguments)]
pub(crate) fn scan(
    pool: Pool,
    query_keywords: &[String],
    candidates: &[Candidate],
    threshold: u32,
    warn_threshold: Option<u32>,
    self_issue: Option<u64>,
    rest_fallback: bool,
) -> ScanOutcome {
    let mut outcome = ScanOutcome::default();

    // The band is open-issues only, and only when 0 < warn < threshold —
    // check-duplicate.sh already validates this, but the decision is
    // re-derived here so the subcommand cannot be talked into an inverted
    // band by a future caller.
    let band_active =
        pool == Pool::OpenIssues && warn_threshold.is_some_and(|w| w > 0 && w < threshold);

    let mut scanned = 0usize;
    let mut matched_rows: Vec<String> = Vec::new();
    let mut near: Vec<NearMatch> = Vec::new();

    if query_keywords.is_empty() {
        // The shell warned (open pool) and bailed before scoring; merged and
        // closed pools were silent. Either way: no scan, exit 0.
        if pool == Pool::OpenIssues {
            eprintln!("WARNING: No significant keywords extracted from title/body");
        }
        outcome.exit_code = 0;
        return outcome;
    }

    for c in candidates {
        if Some(c.number) == self_issue {
            continue;
        }
        scanned += 1;
        let existing = extract_keywords(&format!("{} {}", c.title, c.body));
        let similarity = jaccard_percent(query_keywords, &existing);
        if similarity >= threshold {
            matched_rows.push(format!(
                "{}{}: {} (similarity: {similarity}%)",
                pool.row_prefix(),
                c.number,
                c.title
            ));
        } else if band_active && similarity >= warn_threshold.unwrap_or(0) {
            near.push(NearMatch {
                number: c.number,
                title: c.title.clone(),
                similarity,
            });
        }
    }

    // Degenerate-result self-detection (#4409). Deliberately counted on
    // BLOCK matches only: near-band rows must not tip it, and it cannot fire
    // at matched == 0 (0*2 > scanned is never true), so evaluating it before
    // the matched==0 path is equivalent to the shell's original ordering.
    if !matched_rows.is_empty()
        && scanned >= MIN_SCANNED_FOR_DEGENERATE
        && matched_rows.len() * 2 > scanned
    {
        outcome
            .stdout_lines
            .push(pool.degenerate_line(matched_rows.len(), scanned, threshold));
        outcome.exit_code = if pool == Pool::OpenIssues { 1 } else { 0 };
        // Degenerate => no near rows: the scorer just said it separates
        // nothing, so low-confidence context would only pile on noise.
        return outcome;
    }

    if !matched_rows.is_empty() {
        match pool {
            Pool::OpenIssues => {
                if rest_fallback {
                    outcome.stdout_lines.push(
                        "DUPLICATE_FOUND (REST fallback -- similarity ranking basis differs from GraphQL)"
                            .to_string(),
                    );
                } else {
                    outcome.stdout_lines.push("DUPLICATE_FOUND".to_string());
                }
                outcome.exit_code = 1;
            }
            Pool::MergedPrs | Pool::ClosedIssues => {
                if rest_fallback {
                    outcome.stdout_lines.push("RATE_LIMIT_FALLBACK".to_string());
                }
                outcome.exit_code = 0;
            }
        }
        outcome.stdout_lines.extend(matched_rows);
    } else {
        outcome.exit_code = 0;
    }

    if !near.is_empty() {
        outcome.near_matches = near;
    }
    outcome
}

impl DuplicateScanArgs {
    pub(crate) fn run(self) -> Result<()> {
        let mut buf = String::new();
        if std::io::stdin().read_to_string(&mut buf).is_err() {
            eprintln!("duplicate-scan: could not read the candidate pool from stdin");
            std::process::exit(2);
        }

        // Parse element-by-element, skipping entries that do not shape up:
        // the shell's per-line `jq` loop silently skipped null-numbered
        // entries, and a pool that is not valid JSON top-to-bottom behaved
        // as empty (jq errored per line, the loop ran zero iterations) — an
        // answer of "no matches", not a crash.
        let mut candidates: Vec<Candidate> = Vec::new();
        let mut unparsed = false;
        if !buf.trim().is_empty() {
            match serde_json::from_str::<Vec<serde_json::Value>>(&buf) {
                Ok(values) => {
                    for v in values {
                        match serde_json::from_value::<Candidate>(v) {
                            Ok(c) => candidates.push(c),
                            Err(_) => unparsed = true,
                        }
                    }
                }
                Err(_) => unparsed = true,
            }
        }
        if unparsed {
            eprintln!(
                "duplicate-scan: warning: part of the candidate pool was not valid JSON and was skipped"
            );
        }

        let query_keywords = extract_keywords(&format!("{} {}", self.title, self.body));
        let outcome = scan(
            self.pool,
            &query_keywords,
            &candidates,
            self.threshold,
            self.warn_threshold,
            self.self_issue,
            self.rest_fallback,
        );

        if !outcome.stdout_lines.is_empty() {
            println!("{}", outcome.stdout_lines.join("\n"));
        }

        // The near file is written only when there are rows, and only on a
        // non-degenerate result (scan() already empties near_matches on the
        // degenerate path). Write-then-rename so the caller never reads a
        // half-written file.
        if let (Some(path), false) = (&self.near_file, outcome.near_matches.is_empty()) {
            let tmp = path.with_extension("json.tmp");
            match serde_json::to_string(&outcome.near_matches) {
                Ok(text) => {
                    if let Err(e) =
                        std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, path))
                    {
                        eprintln!("duplicate-scan: could not write {}: {e}", path.display());
                        std::process::exit(2);
                    }
                }
                Err(e) => {
                    eprintln!("duplicate-scan: could not serialize near matches: {e}");
                    std::process::exit(2);
                }
            }
        }

        if outcome.exit_code != 0 {
            std::process::exit(outcome.exit_code);
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn kws(words: &[&str]) -> Vec<String> {
        let mut v: Vec<String> = words.iter().map(|w| (*w).to_string()).collect();
        v.sort();
        v.dedup();
        v
    }

    fn cand(number: u64, title: &str, body: &str) -> Candidate {
        Candidate {
            number,
            title: title.to_string(),
            body: body.to_string(),
        }
    }

    /// Keyword extraction parity with the shell's `extract_keywords`:
    /// lowercase, alnum-split, stop-word filtered, >=3 chars, deduped.
    #[test]
    fn extract_keywords_matches_shell_rules() {
        let got = extract_keywords("Fix the Cache Invalidation bug — cache invalidation!");
        // "fix" and "bug" are stop words (the repo's list filters them);
        // "cache" and "invalidation" survive, deduped.
        assert_eq!(got, kws(&["cache", "invalidation"]));
    }

    #[test]
    fn extract_keywords_splits_on_non_alphanumerics() {
        let got = extract_keywords("sweep-lease-fence.sh:392 repo_args unbound");
        // "sh" is dropped by the 3-char floor, like the shell's `grep -E
        // '.{3,}'`; digits are alphanumeric tokens.
        assert_eq!(got, kws(&["392", "fence", "lease", "repo_args", "sweep", "unbound"]));
    }

    #[test]
    fn extract_keywords_empty_and_stop_only_input_is_empty() {
        assert!(extract_keywords("").is_empty());
        assert!(extract_keywords("the a an is of to it its").is_empty());
    }

    /// True Jaccard, truncating like bash's integer arithmetic — the exact
    /// numbers test-check-duplicate.sh's fixtures assert end-to-end.
    #[test]
    fn jaccard_matches_the_hand_checkable_fixtures() {
        // Query {alpha,bravo,charlie,delta} vs {alpha,bravo,echo,foxtrot}:
        // 2/(4+4-2) = 33%.
        let q = kws(&["alpha", "bravo", "charlie", "delta"]);
        let c = kws(&["alpha", "bravo", "echo", "foxtrot"]);
        assert_eq!(jaccard_percent(&q, &c), 33);
        // vs {alpha,bravo,echo,foxtrot,golf,hotel}: 2/(4+6-2) = 25%.
        let c6 = kws(&["alpha", "bravo", "echo", "foxtrot", "golf", "hotel"]);
        assert_eq!(jaccard_percent(&q, &c6), 25);
        // Disjoint: 0%. Identical: 100%.
        let d = kws(&["yankee", "zulu", "xray", "whiskey"]);
        assert_eq!(jaccard_percent(&q, &d), 0);
        assert_eq!(jaccard_percent(&q, &q), 100);
    }

    #[test]
    fn jaccard_truncates_like_bash_integer_division() {
        // 1 match / 6-union = 16.67 -> 16, the truncation bash's
        // $((matches * 100 / union)) performed.
        let a = kws(&["one"]);
        let b = kws(&["one", "two", "three", "four", "five", "six"]);
        assert_eq!(jaccard_percent(&a, &b), 16);
    }

    #[test]
    fn jaccard_empty_side_is_zero() {
        let a = kws(&["alpha"]);
        assert_eq!(jaccard_percent(&a, &[]), 0);
        assert_eq!(jaccard_percent(&[], &a), 0);
        assert_eq!(jaccard_percent(&[], &[]), 0);
    }

    /// The banding boundaries the issue's acceptance criteria name:
    /// `[13, 18)` — below 13 silent, in-band NEAR with exit 0, at/above 18
    /// BLOCK with exit 1. Fixtures are hand-checkable: query
    /// {alpha,bravo,charlie,delta}, sim = shared*100/(4 + n - shared).
    #[test]
    fn band_boundaries_are_half_open() {
        let q = kws(&["alpha", "bravo", "charlie", "delta"]);
        // 2 shared, 10 words: 2/12 = 16% -> strictly inside (13, 18).
        let in_band = [
            "alpha", "bravo", "echo", "foxtrot", "golf", "hotel", "india", "juliet", "kilo", "lima",
        ];
        // 3 shared, 7 words: 3/8 = 37% -> block.
        let over = [
            "alpha", "bravo", "charlie", "echo", "foxtrot", "golf", "hotel",
        ];
        // 0 shared -> 0% -> silent.
        let silent = [
            "echo", "foxtrot", "golf", "hotel", "india", "juliet", "kilo", "lima", "mike",
            "november",
        ];
        assert_eq!(jaccard_percent(&q, &kws(&in_band)), 16);
        assert_eq!(jaccard_percent(&q, &kws(&over)), 37);
        assert_eq!(jaccard_percent(&q, &kws(&silent)), 0);

        let near = scan(
            Pool::OpenIssues,
            &q,
            &[cand(1, &in_band.join(" "), "")],
            18,
            Some(13),
            None,
            false,
        );
        assert_eq!(near.exit_code, 0, "a near match alone is not a verdict");
        assert!(near.stdout_lines.is_empty());
        assert_eq!(near.near_matches.len(), 1);
        assert_eq!(near.near_matches[0].similarity, 16);

        let block =
            scan(Pool::OpenIssues, &q, &[cand(1, &over.join(" "), "")], 18, Some(13), None, false);
        assert_eq!(block.exit_code, 1);
        assert_eq!(block.stdout_lines[0], "DUPLICATE_FOUND");

        let quiet = scan(
            Pool::OpenIssues,
            &q,
            &[cand(1, &silent.join(" "), "")],
            18,
            Some(13),
            None,
            false,
        );
        assert_eq!(quiet.exit_code, 0);
        assert!(quiet.near_matches.is_empty());
    }

    /// 13 itself is in the band, 18 itself is a block: the half-open edges.
    /// With query {alpha,bravo,charlie,delta} and k=2 shared words,
    /// sim = 200/(4 + n - 2) = 200/(n+2): n=13 -> 13%, n=9 -> 18%.
    #[test]
    fn band_includes_floor_excludes_ceiling() {
        let q = kws(&["alpha", "bravo", "charlie", "delta"]);
        let at_floor = [
            "alpha", "bravo", "kilo", "lima", "mike", "november", "oscar", "papa", "quebec",
            "romeo", "sierra", "tango", "uniform",
        ]; // 13 words, 2 shared -> 200/15 = 13.33 -> 13%
        let at_ceiling = [
            "alpha", "bravo", "kilo", "lima", "mike", "november", "oscar", "papa", "quebec",
        ]; // 9 words, 2 shared -> 200/11 = 18.18 -> 18%
        assert_eq!(jaccard_percent(&q, &kws(&at_floor)), 13);
        assert_eq!(jaccard_percent(&q, &kws(&at_ceiling)), 18);

        let near = scan(
            Pool::OpenIssues,
            &q,
            &[cand(7, &at_floor.join(" "), "")],
            18,
            Some(13),
            None,
            false,
        );
        assert_eq!(near.exit_code, 0, "score exactly 13 is IN the band");
        assert_eq!(near.near_matches.len(), 1);
        assert_eq!(near.near_matches[0].similarity, 13);

        let block = scan(
            Pool::OpenIssues,
            &q,
            &[cand(7, &at_ceiling.join(" "), "")],
            18,
            Some(13),
            None,
            false,
        );
        assert_eq!(block.exit_code, 1, "score exactly 18 is a BLOCK, not a near match");
        assert!(block.near_matches.is_empty());
    }

    /// Degenerate (#4409): more than half the scanned candidates over
    /// threshold => NON_DISCRIMINATIVE, and the band is suppressed.
    #[test]
    fn degenerate_suppresses_near_rows() {
        let q = kws(&[
            "quantum",
            "flux",
            "capacitor",
            "reactor",
            "core",
            "module",
            "driver",
        ]);
        // 4 candidates, 3 of which are near-identical to the query (>= 50%
        // each) -> 3*2 > 4 => degenerate. Mirrors test nb7's fixture.
        let cands = vec![
            cand(601, "Quantum flux widget", ""),
            cand(602, "Capacitor reactor system", ""),
            cand(603, "Completely unrelated banana fruit basket", ""),
            cand(604, "Core module driver suite", ""),
        ];
        let out = scan(Pool::OpenIssues, &q, &cands, 18, Some(1), None, false);
        assert_eq!(out.exit_code, 1);
        assert_eq!(out.stdout_lines.len(), 1, "degenerate prints only its marker line");
        assert!(out.stdout_lines[0].starts_with("NON_DISCRIMINATIVE (open issues): "));
        assert!(
            out.stdout_lines[0].contains("3 of 4"),
            "reports matched of scanned: {}",
            out.stdout_lines[0]
        );
        assert!(out.near_matches.is_empty(), "no near rows on a degenerate result");
    }

    /// Near matches never tip the degenerate detector: matched counts BLOCK
    /// rows only, so 1 block + 3 near out of 4 scanned (2 > 4 false) is NOT
    /// degenerate (test nb8's fixture, mirrored).
    #[test]
    fn near_matches_do_not_tip_the_degenerate_detector() {
        let q = kws(&["alpha", "bravo", "charlie", "delta"]);
        let cands = vec![
            cand(701, "Alpha Bravo Echo Foxtrot", ""), // 2/6 = 33% >= 30
            cand(702, "Alpha Bravo Echo Foxtrot Golf Hotel", ""), // 25 NEAR
            cand(705, "Alpha Bravo Echo Foxtrot Golf India", ""), // 25 NEAR
            cand(706, "Alpha Bravo Echo Foxtrot Golf Juliet", ""), // 25 NEAR
        ];
        let out = scan(Pool::OpenIssues, &q, &cands, 30, Some(20), None, false);
        assert_eq!(out.exit_code, 1);
        assert_eq!(out.stdout_lines[0], "DUPLICATE_FOUND");
        assert_eq!(out.near_matches.len(), 3);
        assert!(out
            .stdout_lines
            .iter()
            .all(|l| !l.starts_with("NON_DISCRIMINATIVE")));
    }

    /// The self-issue skip (#4662): the probed issue never scores against
    /// itself.
    #[test]
    fn self_issue_is_skipped() {
        let q = kws(&["alpha", "bravo", "charlie"]);
        let cands = vec![cand(42, "alpha bravo charlie", "")];
        let out = scan(Pool::OpenIssues, &q, &cands, 18, None, Some(42), false);
        assert_eq!(out.exit_code, 0);
        assert!(out.stdout_lines.is_empty());
    }

    /// Pools render their own prefixes and verdict weights: a merged-PR
    /// match is context (exit 0) with a `PR #` prefix; closed issues use
    /// `Closed #`.
    #[test]
    fn pool_prefixes_and_exit_codes() {
        let q = kws(&["alpha", "bravo", "charlie", "delta"]);
        let cands = vec![cand(9, "Alpha Bravo Echo Foxtrot", "")];
        let merged = scan(Pool::MergedPrs, &q, &cands, 18, Some(13), None, false);
        assert_eq!(merged.exit_code, 0);
        assert_eq!(merged.stdout_lines, vec!["PR #9: Alpha Bravo Echo Foxtrot (similarity: 33%)"]);

        let closed = scan(Pool::ClosedIssues, &q, &cands, 18, Some(13), None, false);
        assert_eq!(closed.exit_code, 0);
        assert_eq!(
            closed.stdout_lines,
            vec!["Closed #9: Alpha Bravo Echo Foxtrot (similarity: 33%)"]
        );

        let open = scan(Pool::OpenIssues, &q, &cands, 18, Some(13), None, false);
        assert_eq!(open.exit_code, 1);
        assert_eq!(
            open.stdout_lines,
            vec![
                "DUPLICATE_FOUND",
                "#9: Alpha Bravo Echo Foxtrot (similarity: 33%)"
            ]
        );
    }

    /// REST-fallback labeling (#4526): open pool re-labels the header;
    /// merged/closed pools emit the sentinel line the script's aggregator
    /// strips and re-reports as REST_FALLBACK.
    #[test]
    fn rest_fallback_labels_output() {
        let q = kws(&["alpha", "bravo", "charlie", "delta"]);
        let cands = vec![cand(9, "Alpha Bravo Echo Foxtrot", "")];
        let open = scan(Pool::OpenIssues, &q, &cands, 18, None, None, true);
        assert_eq!(
            open.stdout_lines[0],
            "DUPLICATE_FOUND (REST fallback -- similarity ranking basis differs from GraphQL)"
        );
        let merged = scan(Pool::MergedPrs, &q, &cands, 18, None, None, true);
        assert_eq!(merged.stdout_lines[0], "RATE_LIMIT_FALLBACK");
        assert_eq!(merged.stdout_lines[1], "PR #9: Alpha Bravo Echo Foxtrot (similarity: 33%)");
    }

    /// Degenerate merged-PR results are exit 0 with the shorter
    /// manual-review line (no "(e.g. gh issue list --search)" suffix).
    #[test]
    fn merged_degenerate_is_exit_zero() {
        let q = kws(&[
            "quantum",
            "flux",
            "capacitor",
            "reactor",
            "core",
            "module",
            "driver",
        ]);
        let cands = vec![
            cand(601, "Quantum flux widget", ""),
            cand(602, "Capacitor reactor system", ""),
            cand(603, "Completely unrelated banana fruit basket", ""),
            cand(604, "Core module driver suite", ""),
        ];
        let out = scan(Pool::MergedPrs, &q, &cands, 18, None, None, false);
        assert_eq!(out.exit_code, 0);
        assert!(out.stdout_lines[0].starts_with("NON_DISCRIMINATIVE (merged PRs): "));
        assert!(out.stdout_lines[0].ends_with("fall back to manual review."));
    }

    /// The band is inert for non-open pools even when a warn threshold is
    /// passed, and inert on the open pool when warn >= threshold or 0.
    #[test]
    fn band_is_open_pool_only_and_never_inverted() {
        let q = kws(&["alpha", "bravo", "charlie", "delta"]);
        let cands = vec![cand(702, "Alpha Bravo Echo Foxtrot Golf Hotel", "")]; // 25%
        let merged = scan(Pool::MergedPrs, &q, &cands, 30, Some(20), None, false);
        assert!(merged.near_matches.is_empty(), "merged pool never reports near rows");

        let inverted = scan(Pool::OpenIssues, &q, &cands, 30, Some(30), None, false);
        assert!(inverted.near_matches.is_empty(), "warn == threshold disables the band");

        let zero = scan(Pool::OpenIssues, &q, &cands, 30, Some(0), None, false);
        assert!(zero.near_matches.is_empty(), "warn 0 disables the band");
    }

    /// An empty query (only stop words / short tokens) bails before scoring:
    /// warned (open pool) and exit 0, regardless of candidates.
    #[test]
    fn empty_query_keywords_bail_out() {
        let q = extract_keywords("the a an is it");
        assert!(q.is_empty());
        let out = scan(
            Pool::OpenIssues,
            &q,
            &[cand(1, "anything at all", "")],
            18,
            Some(13),
            None,
            false,
        );
        assert_eq!(out.exit_code, 0);
        assert!(out.stdout_lines.is_empty());
    }

    /// Candidates with a null number were skipped by the shell's per-line jq
    /// loop; element-level parse failures are skipped the same way.
    #[test]
    fn null_numbered_candidates_are_skipped_by_the_runner_parse() {
        let pool_json = r#"[
            {"number": null, "title": "broken", "body": ""},
            {"number": 5, "title": "Alpha Bravo Echo Foxtrot", "body": ""}
        ]"#;
        let values: Vec<serde_json::Value> = serde_json::from_str(pool_json).unwrap();
        let cands: Vec<Candidate> = values
            .iter()
            .filter_map(|v| serde_json::from_value(v.clone()).ok())
            .collect();
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].number, 5);
    }
}
