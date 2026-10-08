//! Per-test spans from a `nextest-partition` leg's uploaded JUnit XML
//! (Issue #9456 — the deliberately-deferred remainder of #9089 proposal
//! item 3).
//!
//! # Why
//!
//! A `Rust Unit Tests (1/3)` leg has a `loom.ci.job` span and `loom.ci.step`
//! children, so "~110s of this leg's 250s was the test step" is answerable —
//! but *which tests* spent it is not. The shell-suite legs got that answer
//! from [`super::suites`]' `loom.ci.suite` spans; the nextest legs had nothing
//! below step granularity, which is why rebalancing `--partition count:k/N`
//! stayed guesswork (the same Unit partition's test step has ranged 50s–139s
//! between runs, #9089). This is also the per-test outcome/duration data
//! #7789's flake tracking wants for the Rust suite.
//!
//! # The contract, both halves in one place
//!
//! `.config/nextest.toml`'s `[profile.ci.junit]` makes nextest write
//! `target/nextest/ci/junit.xml`, and `ci.yml` uploads it as an artifact named
//! [`ARTIFACT_NAME_PREFIX`]`-<family>-<k>-<N>`. **The name is the whole
//! pairing key**: JUnit XML carries no run id, no shard and no job name, so
//! unlike a suite-timings record there is nothing inside the file to pair on.
//! The poller reads only artifacts matching that shape, and only for a run
//! that already has a not-yet-emitted `nextest-partition` job — so a repo with
//! no nextest legs pays nothing, not even the artifacts listing.
//!
//! `<family>` is [`job_family_slug`] of the leg's display name with its
//! `(k/N)` suffix removed (`Rust Unit Tests (1/3)` → `rust-unit-tests`). It is
//! load-bearing rather than cosmetic: the family keeps the key unambiguous
//! (`ci.yml` has one nextest-partition family, `Rust Unit Tests`, since
//! #10823; a second family sharding `1..3` would otherwise make `(kind, k, N)`
//! match two jobs, [`RejectReason::NoUniqueJob`] for every record). Renaming a
//! leg without
//! renaming its artifact is the same rejection — counted and logged, never
//! guessed.
//!
//! # Span volume is the one risk the suite work did not face
//!
//! A nextest leg runs **4,242 tests** against a shell-suite leg's ~118 suites
//! (measured: `cargo nextest list --workspace --profile ci --partition
//! count:1/3` on `main`, 2026-10-02 — 12,659 workspace tests over three
//! partitions). So "one span per test" is not an option: it would be ~25,400
//! spans per CI run across the nextest legs, two orders of magnitude more
//! trace volume than every other CI span combined. These spans exist to answer
//! "what should move between partitions" and "which test regressed", and both
//! questions live entirely in the slow tail — a 3 ms test is not a rebalancing
//! candidate at any partition size. So the emission is deliberately a **tail
//! sample**: tests at or above [`MIN_TEST_DURATION_MS`], slowest first, capped
//! at [`MAX_TEST_SPANS_PER_JOB`] per leg. Both numbers are sized against that
//! same measured run; see each constant and `ci-observability.md` §"Per-test
//! spans (#9456)".
//!
//! # Every value here is untrusted input
//!
//! A `pull_request` from a fork runs the **fork's** `ci.yml` and
//! `.config/nextest.toml`, so the artifact name, the XML and every test name,
//! timestamp and duration in it are attacker-controlled text. Hence: a byte
//! cap before parsing ([`MAX_JUNIT_BYTES`]), an element cap while parsing
//! ([`MAX_TESTCASES_PARSED`]), a span cap per job, control-stripped and
//! 200-char-truncated names (`bounded_attributes` DROPS a value with a control
//! character or over 256 chars, which would silently lose the one attribute
//! naming the test), an outcome vocabulary that is closed *by construction*
//! (it is a Rust enum the parser maps into, never forwarded text), and windows
//! clamped into the job's own GitHub-reported span. A record that fails any of
//! it is skipped and counted — never repaired into something plausible.
//!
//! # Why a hand parser and not an XML dependency
//!
//! The daemon has no XML dependency and a deliberately small dependency
//! surface. What this module consumes is four attributes of one element type
//! (`<testcase name= classname= timestamp= time=>`) plus *the presence* of one
//! of five child element names — a few hundred bytes of grammar. A general XML
//! parser would be a new dependency, with its own CVE surface and transitive
//! tree, for a document whose only interesting half is its start tags.
//!
//! Deciding that is also what makes the **redaction** rule enforceable: a
//! `<failure>`/`<rerunFailure>` element's text and `message=` attribute carry
//! the test's panic output **verbatim** (captured stdout and stderr). That is
//! log text, and nothing may be derived from log text here — see
//! `ci-observability.md` §"Why there is no `step` attribute", since an
//! attribute derived from it would ride straight past the gateway's body
//! scrub. This parser reads element *names* and the four attributes above and
//! structurally cannot read a failure message; a DOM-based reader would make
//! that a reviewer's discipline instead of a property of the code.

use std::collections::HashSet;

use chrono::{DateTime, Utc};

use super::records::{ShardInfo, ShardKind};
use crate::telemetry::trace::{SpanName, SpanRecord, SpanStatus, TraceContext};
use crate::telemetry::{RepoVisibility, TelemetryEnvelope, TelemetryRecord};

/// Artifacts whose name starts with this are the only ones the poller
/// downloads for test spans. `ci.yml` uploads
/// `ci-test-timings-<family>-<k>-<N>`.
pub const ARTIFACT_NAME_PREFIX: &str = "ci-test-timings";

/// Upper bound on the JUnit text parsed for one leg.
///
/// Measured, not guessed: a passing `<testcase …/>` is ~199 bytes, so the
/// 4,242-test leg's all-green document is ~845 KB. The real recorded leg
/// (4,242 tests, 71 failing) was 894 KB, because a failing test additionally
/// embeds its full captured stdout+stderr once per attempt plus
/// `<system-out>`/`<system-err>` copies — which nextest does not bound. 8 MiB
/// is ~9x the all-green size, enough headroom for a leg that is red
/// everywhere, and still bounds what a hostile fork can make the poller read
/// and scan.
pub const MAX_JUNIT_BYTES: u64 = 8 * 1024 * 1024;

/// Upper bound on `<testcase>` elements parsed out of one file. The largest
/// real leg holds 4,242; 50,000 is ~12x that and bounds the allocation a
/// hostile 8 MiB file of bare `<testcase/>` elements (~400,000 of them) could
/// otherwise force.
pub const MAX_TESTCASES_PARSED: usize = 50_000;

/// Upper bound on test artifacts downloaded for one run — `ci.yml` has three
/// nextest legs (one family x three partitions since #10823), with headroom.
pub const MAX_ARTIFACTS_PER_RUN: usize = 8;

/// Upper bound on `loom.ci.test` spans emitted for one job — a **backstop**,
/// not the mechanism that keeps the steady state cheap. That is
/// [`MIN_TEST_DURATION_MS`]: it already excludes 93% of a leg's tests.
///
/// Sized against the measured above-floor population rather than rounded for
/// comfort: of the recorded leg's 4,242 tests, **306** are at or above the
/// floor, so 512 is ~1.7x that — real slack, so an ordinary slow-test wave
/// does not silently truncate the ranking, while a pathological or forged
/// document still cannot emit more. The resulting ingest cost, stated: ~1,840
/// spans per CI run across the six nextest legs in steady state and at most
/// 3,072 in the worst case, against the ~300 CI spans a run emits today. The
/// same number [`super::suites::MAX_SUITE_SPANS_PER_JOB`] uses, deliberately:
/// two different per-leg span families with one reviewable bound.
pub const MAX_TEST_SPANS_PER_JOB: usize = 512;

/// Duration floor: a test faster than this emits no span at all. **This, not
/// the cap, is what makes the feature affordable.**
///
/// Measured on one real `--partition count:1/3` leg (4,242 tests, 873s of
/// summed test time): 306 tests (7.2%) are at or above 250 ms, and those 306
/// carry **89% of the leg's total test time**. So the floor discards 93% of the
/// records for 11% of the signal — and 250 ms is where that trade is available:
/// it is ~0.1% of a leg's ~250s wall time, below which a test cannot change a
/// partition's balance, cannot be a quarantine candidate, and cannot be told
/// apart from the process-spawn overhead nextest pays per test. Emitting one
/// span per test instead would be ~25,400 spans per run — two orders of
/// magnitude more trace volume than every other CI span combined — to answer a
/// question that lives entirely in the tail.
pub const MIN_TEST_DURATION_MS: i64 = 250;

/// Longest test / binary name carried on a span attribute; see the module
/// note on `bounded_attributes`.
const MAX_TEST_NAME_CHARS: usize = 200;

/// Longest `<family>` slug accepted from an artifact name.
const MAX_FAMILY_CHARS: usize = 64;

/// How one `<testcase>` concluded. A Rust enum rather than the file's own
/// text, so the attribute's vocabulary is closed by construction and a fork
/// cannot introduce unbounded cardinality into an attribute that is grouped
/// on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestOutcome {
    /// No `<failure>`/`<error>`/`<skipped>` child, and no rerun.
    Pass,
    /// A `<failure>` child — the test failed on its final attempt.
    Fail,
    /// An `<error>` child — nextest could not execute the test (abort, signal).
    Error,
    /// A `<flakyFailure>`/`<rerunFailure>` child but no `<failure>`: it failed
    /// at least once and ultimately passed. The #7789 signal.
    Flaky,
    /// A `<skipped>` child. Never emitted as a span — see [`window`].
    Skip,
}

impl TestOutcome {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            TestOutcome::Pass => "pass",
            TestOutcome::Fail => "fail",
            TestOutcome::Error => "error",
            TestOutcome::Flaky => "flaky",
            TestOutcome::Skip => "skip",
        }
    }

    #[must_use]
    fn status(self) -> SpanStatus {
        match self {
            // A flaky test ultimately passed; its `outcome` attribute, not its
            // span status, is what a flake query groups on.
            TestOutcome::Pass | TestOutcome::Flaky => SpanStatus::Ok,
            TestOutcome::Fail | TestOutcome::Error => SpanStatus::Error,
            TestOutcome::Skip => SpanStatus::Unset,
        }
    }
}

/// One `<testcase>` element, reduced to what a span needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestCase {
    /// `classname` — nextest's binary id: `loom-daemon` for a package's lib
    /// tests, `loom-daemon::<target>` for an integration test binary.
    pub binary: String,
    /// `name` — the test path inside that binary.
    pub name: String,
    /// `timestamp`, the instant the test started. `None` when the attribute is
    /// absent or unparseable, which costs the span: a duration with no anchor
    /// cannot be placed in the trace, and stacking it from the job's start
    /// would invent an ordering the data does not establish.
    pub started_at: Option<DateTime<Utc>>,
    /// `time`, in whole milliseconds. Never negative.
    pub duration_ms: i64,
    pub outcome: TestOutcome,
}

/// What an artifact's **name** says about which leg it belongs to. The whole
/// pairing key — see the module note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactIdentity {
    /// [`job_family_slug`] of the leg's display name.
    pub family: String,
    pub shard_index: u32,
    pub shard_total: u32,
}

/// Why a JUnit artifact was not turned into spans. Every variant is a named
/// reason the cycle logs — silently dropping one would be worse than no
/// artifact at all, because the absence of test spans would look like "this
/// leg ran no slow tests".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectReason {
    /// The artifact name is not `ci-test-timings-<family>-<k>-<N>` with
    /// `1 <= k <= N`.
    NoArtifactIdentity(String),
    /// The file has no `<testsuites` root — it is not JUnit XML at all.
    NotJunit,
    /// It parsed, but held no `<testcase>` element the poller could read.
    NoTestCases,
    /// No `nextest-partition` job of this run has that family and `(k, N)`, or
    /// several do. Never guessed — the same rule [`super::suites`] applies to
    /// an ambiguous shard, and story stitching to an ambiguous issue.
    NoUniqueJob {
        family: String,
        shard: String,
        matches: usize,
    },
}

impl std::fmt::Display for RejectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RejectReason::NoArtifactIdentity(name) => {
                write!(f, "artifact name {name:?} is not {ARTIFACT_NAME_PREFIX}-<family>-<k>-<N>")
            }
            RejectReason::NotJunit => write!(f, "artifact holds no <testsuites> JUnit document"),
            RejectReason::NoTestCases => write!(f, "JUnit document holds no <testcase> element"),
            RejectReason::NoUniqueJob {
                family,
                shard,
                matches,
            } => write!(
                f,
                "family {family:?} shard {shard:?} matches {matches} nextest-partition job(s) of \
                 this run, not exactly 1"
            ),
        }
    }
}

/// Whether this artifact is a JUnit test-timings record worth downloading. An
/// **expired** artifact is not: GitHub answers 410 for it, which would be
/// recorded as a failure every cycle for nothing.
#[must_use]
pub fn is_test_artifact(artifact: &super::suites::ArtifactJson) -> bool {
    artifact.name.starts_with(ARTIFACT_NAME_PREFIX) && !artifact.expired
}

/// `ci-test-timings-<family>-<k>-<N>` → its identity. Parsed from the right so
/// a family slug containing `-` (every real one does) stays intact.
#[must_use]
pub fn parse_artifact_name(name: &str) -> Option<ArtifactIdentity> {
    let rest = name
        .strip_prefix(ARTIFACT_NAME_PREFIX)
        .and_then(|rest| rest.strip_prefix('-'))?;
    let (head, total) = rest.rsplit_once('-')?;
    let (family, index) = head.rsplit_once('-')?;
    let shard_index: u32 = index.parse().ok()?;
    let shard_total: u32 = total.parse().ok()?;
    if family.is_empty()
        || family.chars().count() > MAX_FAMILY_CHARS
        || shard_index < 1
        || shard_total < 1
        || shard_index > shard_total
    {
        return None;
    }
    Some(ArtifactIdentity {
        family: family.to_string(),
        shard_index,
        shard_total,
    })
}

/// A matrix leg's **family** slug: its display name with the trailing `(…k/N)`
/// shard group [`super::records::parse_shard`] reads removed, lowercased, and
/// every run of non-alphanumeric characters collapsed to a single `-`.
///
/// `Rust Unit Tests (1/3)` → `rust-unit-tests`. This is what `ci.yml` writes
/// into the artifact name, and what distinguishes families that both shard
/// `1..3`.
#[must_use]
pub fn job_family_slug(job_name: &str) -> String {
    slugify(strip_shard_suffix(job_name))
}

/// `"Rust Unit Tests (1/3)"` → `"Rust Unit Tests"`. A name whose trailing
/// parenthesised group is not `(…k/N)` is returned unchanged, mirroring
/// `parse_shard`'s regex (which forbids nested parens, so the LAST `(` is the
/// group's opener).
fn strip_shard_suffix(job_name: &str) -> &str {
    let trimmed = job_name.trim_end();
    let Some(inner) = trimmed.strip_suffix(')') else {
        return trimmed;
    };
    let Some(open) = inner.rfind('(') else {
        return trimmed;
    };
    let group = &inner[open + 1..];
    let fraction = group.rsplit(',').next().unwrap_or(group).trim();
    let Some((k, n)) = fraction.split_once('/') else {
        return trimmed;
    };
    if k.trim().parse::<u32>().is_err() || n.trim().parse::<u32>().is_err() {
        return trimmed;
    }
    inner[..open].trim_end()
}

fn slugify(raw: &str) -> String {
    let mut out = String::new();
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

/// Which job of this run an artifact belongs to: the unique
/// `nextest-partition` leg with the same family and `(index, total)`. `jobs`
/// is `(job_id, display name, ShardInfo)` for every job of the run.
pub fn match_job(
    identity: &ArtifactIdentity,
    jobs: &[(u64, &str, ShardInfo)],
) -> Result<u64, RejectReason> {
    let matches: Vec<u64> = jobs
        .iter()
        .filter(|(_, name, shard)| {
            shard.kind == ShardKind::NextestPartition
                && shard.index == Some(identity.shard_index)
                && shard.total == Some(identity.shard_total)
                && job_family_slug(name) == identity.family
        })
        .map(|(job_id, _, _)| *job_id)
        .collect();
    match matches.as_slice() {
        [only] => Ok(*only),
        other => Err(RejectReason::NoUniqueJob {
            family: identity.family.clone(),
            shard: format!("{}/{}", identity.shard_index, identity.shard_total),
            matches: other.len(),
        }),
    }
}

// ---------------------------------------------------------------------------
// The parser
// ---------------------------------------------------------------------------

/// Parse the `<testcase>` elements of a JUnit document.
///
/// Reads only element names and the four `<testcase>` attributes a span needs
/// — never a `<failure>`'s message or text, which is the test's raw captured
/// output (see the module note on redaction).
///
/// A document that runs out mid-element stops the scan and keeps the prefix
/// already parsed: a truncated upload is still real data about the tests that
/// did complete, and the alternative (rejecting the file) loses them for no
/// gain. A file over [`MAX_JUNIT_BYTES`] never reaches here at all — it is
/// skipped by the reader, not truncated into this path.
pub fn parse(text: &str) -> Result<Vec<TestCase>, RejectReason> {
    if !text.contains("<testsuites") {
        return Err(RejectReason::NotJunit);
    }
    let bytes = text.as_bytes();
    let mut cases: Vec<TestCase> = Vec::new();
    let mut cursor = 0usize;
    while cases.len() < MAX_TESTCASES_PARSED {
        let Some(rel) = text[cursor..].find("<testcase") else {
            break;
        };
        let open = cursor + rel;
        let after_name = open + "<testcase".len();
        // `<testcases…` (no such element, but a hostile file may contain one)
        // is not `<testcase`: the name has to end right here.
        if !matches!(bytes.get(after_name), Some(b' ' | b'\t' | b'\r' | b'\n' | b'>' | b'/')) {
            cursor = after_name;
            continue;
        }
        let Some((gt, self_closing)) = start_tag_end(text, after_name) else {
            break;
        };
        let attrs_end = if self_closing { gt - 1 } else { gt };
        let attrs = &text[after_name..attrs_end];
        cursor = gt + 1;
        let (outcome, next) = if self_closing {
            (TestOutcome::Pass, cursor)
        } else {
            let Some(rel_close) = text[cursor..].find("</testcase") else {
                break;
            };
            let close = cursor + rel_close;
            (child_outcome(&text[cursor..close]), close)
        };
        cursor = next;
        let Some(name) = tag_attribute(attrs, "name") else {
            continue;
        };
        let binary = tag_attribute(attrs, "classname").unwrap_or_default();
        let started_at = tag_attribute(attrs, "timestamp")
            .and_then(|raw| DateTime::parse_from_rfc3339(raw.trim()).ok())
            .map(|at| at.with_timezone(&Utc));
        let duration_ms = tag_attribute(attrs, "time")
            .and_then(|raw| raw.trim().parse::<f64>().ok())
            .filter(|secs| secs.is_finite() && *secs >= 0.0)
            .map_or(0, |secs| (secs * 1000.0).round() as i64);
        cases.push(TestCase {
            binary,
            name,
            started_at,
            duration_ms,
            outcome,
        });
    }
    if cases.is_empty() {
        return Err(RejectReason::NoTestCases);
    }
    Ok(cases)
}

/// The index of a start tag's closing `>` and whether it was `/>`. Quoted
/// attribute values are skipped, so a raw `>` inside one cannot end the tag
/// early.
fn start_tag_end(text: &str, from: usize) -> Option<(usize, bool)> {
    let mut quote: Option<char> = None;
    let mut previous = ' ';
    for (offset, c) in text[from..].char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == '>' => return Some((from + offset, previous == '/')),
            None => {}
        }
        previous = c;
    }
    None
}

/// One attribute's value out of a start tag's attribute region, with the five
/// predefined XML entities decoded. A numeric character reference is left
/// verbatim on purpose: decoding one is the only way this parser could
/// reintroduce a control character, and the sanitizer would only have to strip
/// it again.
fn tag_attribute(region: &str, want: &str) -> Option<String> {
    let mut rest = region;
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            return None;
        }
        let eq = rest.find('=')?;
        let key = rest[..eq].trim();
        let after = rest[eq + 1..].trim_start();
        let quote = after.chars().next()?;
        if quote != '"' && quote != '\'' {
            return None;
        }
        let body = &after[quote.len_utf8()..];
        let close = body.find(quote)?;
        if key == want {
            return Some(unescape(&body[..close]));
        }
        rest = &body[close + quote.len_utf8()..];
    }
}

fn unescape(raw: &str) -> String {
    if !raw.contains('&') {
        return raw.to_string();
    }
    raw.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        // Last, so a literal `&amp;lt;` does not become `<`.
        .replace("&amp;", "&")
}

/// A `<testcase>`'s outcome, from the **names** of its children only.
///
/// Precedence is deliberate: nextest writes `<rerunFailure>` children
/// alongside `<failure>` for a test that failed on every attempt, and
/// `<flakyFailure>` with no `<failure>` for one that ultimately passed — so
/// checking `<failure>` first is what separates "failed" from "flaky".
fn child_outcome(body: &str) -> TestOutcome {
    if body.contains("<skipped") {
        TestOutcome::Skip
    } else if body.contains("<failure") {
        TestOutcome::Fail
    } else if body.contains("<error") {
        TestOutcome::Error
    } else if body.contains("<flakyFailure") || body.contains("<rerunFailure") {
        TestOutcome::Flaky
    } else {
        TestOutcome::Pass
    }
}

// ---------------------------------------------------------------------------
// Selection and spans
// ---------------------------------------------------------------------------

/// A test / binary name reduced to what a span attribute may carry — the same
/// treatment `records::step_name` and `suites::suite_name` apply, for the same
/// reason.
#[must_use]
fn bounded_name(raw: &str) -> String {
    let cleaned: String = raw
        .trim()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if cleaned.chars().count() <= MAX_TEST_NAME_CHARS {
        return cleaned;
    }
    let mut out: String = cleaned.chars().take(MAX_TEST_NAME_CHARS).collect();
    out.push('…');
    out
}

/// The tail sample one leg emits: tests at or above [`MIN_TEST_DURATION_MS`],
/// slowest first, de-duplicated on `(binary, name)` and capped at
/// [`MAX_TEST_SPANS_PER_JOB`].
///
/// A skipped test and a test with no usable window are dropped here rather
/// than later, so the cap is spent on spans that will actually be emitted.
/// Ordering breaks ties on `(binary, name)` so the selection is deterministic
/// across replays and hosts — two runs of the same input select the same tests.
#[must_use]
pub fn select(cases: &[TestCase]) -> Vec<TestCase> {
    let mut eligible: Vec<&TestCase> = cases
        .iter()
        .filter(|case| {
            case.outcome != TestOutcome::Skip
                && case.started_at.is_some()
                && case.duration_ms >= MIN_TEST_DURATION_MS
                && !case.name.trim().is_empty()
        })
        .collect();
    eligible.sort_by(|a, b| {
        b.duration_ms
            .cmp(&a.duration_ms)
            .then_with(|| a.binary.cmp(&b.binary))
            .then_with(|| a.name.cmp(&b.name))
    });
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut selected = Vec::new();
    for case in eligible {
        if selected.len() >= MAX_TEST_SPANS_PER_JOB {
            break;
        }
        let key = (bounded_name(&case.binary), bounded_name(&case.name));
        if key.1.is_empty() || !seen.insert(key) {
            continue;
        }
        selected.push(case.clone());
    }
    selected
}

/// One test's window, clamped inside `[job_started, job_ended]`.
///
/// `None` when the case carries no start instant — absent, never a zero-length
/// span at the job's start, which would read as "ran instantly" rather than
/// "was not placed" (ci-principles rule 6). A test that did not run is already
/// absent before this: nextest omits a filtered-out test from the document
/// entirely, and a `<skipped>` case is dropped by [`select`].
///
/// The clamp bounds the damage a wrong runner clock (or a forged record) can
/// do to the trace: a child span outside its parent's window renders as a
/// detached bar, and the job's own GitHub-reported window is the authority
/// here, not the runner's clock.
#[must_use]
pub fn window(
    case: &TestCase,
    job_started: DateTime<Utc>,
    job_ended: DateTime<Utc>,
) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let start = case.started_at?;
    let end = start + chrono::Duration::milliseconds(case.duration_ms.max(0));
    let start = start.clamp(job_started, job_ended);
    let end = end.clamp(start, job_ended);
    Some((start, end))
}

/// Everything a test span needs about its job, resolved once by the caller.
#[derive(Debug, Clone)]
pub struct TestSpanTarget<'a> {
    pub repo: &'a str,
    pub visibility: RepoVisibility,
    pub run_id: u64,
    pub attempt: u32,
    pub job_id: u64,
    pub job: &'a str,
    pub workflow: &'a str,
    pub shard: ShardInfo,
    pub job_context: TraceContext,
    pub job_started: DateTime<Utc>,
    pub job_ended: DateTime<Utc>,
}

/// The `loom.ci.test` spans of one leg, each a child of that leg's job span.
///
/// Span-only, for the same reasons step and suite spans are
/// (`ci-observability.md`): the metric label allowlist admits no test
/// dimension, and a per-test histogram would multiply the 30-day series count
/// by every leg's test count.
#[must_use]
pub fn test_envelopes(
    target: &TestSpanTarget<'_>,
    cases: &[TestCase],
    host_id: &str,
) -> Vec<TelemetryEnvelope> {
    let mut envelopes = Vec::new();
    for case in select(cases) {
        let name = bounded_name(&case.name);
        let binary = bounded_name(&case.binary);
        let Some((started_at, ended_at)) = window(&case, target.job_started, target.job_ended)
        else {
            continue;
        };
        let ctx = super::records::test_context(
            target.repo,
            target.run_id,
            target.attempt,
            target.job_id,
            &binary,
            &name,
        );
        let span = SpanRecord {
            context: ctx.clone(),
            parent_span_id: Some(target.job_context.span_id.clone()),
            name: SpanName::CiTest,
            started_at,
            ended_at,
            status: case.outcome.status(),
            attributes: super::records::span_attributes(vec![
                ("loom.repo", Some(target.repo.to_string())),
                (
                    "loom.repo.visibility",
                    Some(super::records::visibility_str(target.visibility).to_string()),
                ),
                ("loom.ci.run_id", Some(target.run_id.to_string())),
                ("loom.ci.job_id", Some(target.job_id.to_string())),
                ("loom.ci.workflow", Some(target.workflow.to_string())),
                ("loom.ci.job", Some(target.job.to_string())),
                ("loom.ci.test", Some(name)),
                ("loom.ci.test.binary", Some(binary)),
                ("loom.ci.test.outcome", Some(case.outcome.as_str().to_string())),
                ("loom.ci.shard.index", target.shard.index.map(|i| i.to_string())),
                ("loom.ci.shard.total", target.shard.total.map(|t| t.to_string())),
                ("loom.ci.shard.kind", Some(target.shard.kind.as_str().to_string())),
            ]),
            events: Vec::new(),
            links: Vec::new(),
        };
        let mut envelope = TelemetryEnvelope::new(host_id, TelemetryRecord::Span(span));
        envelope.trace_context = Some(ctx);
        envelopes.push(envelope);
    }
    envelopes
}
