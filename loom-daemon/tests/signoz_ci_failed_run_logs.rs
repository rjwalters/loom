//! Live proof for section 5 of the SigNoz trial's standing build/CI retro
//! queries, `signoz/ci-queries.sql` ("Failed run -> logs", #8826) — Issue
//! #8528 scope item 4 ("reproducible saved queries ... span-related logs") and
//! scope item 5 ("missing stays distinguishable from zero usage and successful
//! completion").
//!
//! **Why this section and not the whole file.** `ci-queries.sql` already ran
//! end to end against the live trial deployment on a real capture — 592 runs /
//! 2,131 jobs, every section non-empty, reconciling exactly to the records.
//! One line of that result was not a pass. `evidence.md` recorded it as:
//! *"Section 5's chunk join is unobserved on real `ci.job.log` data (none
//! reached the trial)."* All 60 rows of the live section-5 result read
//! `0 of 0`, so the half of the query that joins a failed job to its captured
//! log chunks has never produced a non-trivial row anywhere — not on the
//! trial, not in CI, not here. Everything downstream of that join is therefore
//! unexecuted code in a saved view people are meant to act on.
//!
//! That is the gap this closes, by the same technique every other query
//! artifact in this trial already uses — `usage-queries.sql` via
//! `signoz_usage_queries.rs` (#9705), `cycle-time-extract.sql` via
//! `signoz_cycle_time.rs` (#9775), `queue-dwell.sql` / `quota-utilization.sql`
//! via `signoz_queue_quota_queries.rs` (#9833), `alerts/queue-starvation.json`
//! via `signoz_queue_starvation_alert.rs` (#9857): the COMMITTED file, run
//! verbatim on `clickhouse local` in the pinned
//! `clickhouse/clickhouse-server:25.12.5` image the trial's telemetry store
//! runs. No multi-container SigNoz deployment, no persistent volume, no
//! network, no credential.
//!
//! **What executing it found.** A failed run does not always have a failed
//! job: a `startup_failure`, a cancelled matrix parent or a required check
//! that never produced a job all leave `failed_jobs` with nothing to offer.
//! The JOB side of the `LEFT JOIN` then misses, and ClickHouse fills every
//! job-sourced column with its type's zero rather than NULL — so before
//! #8528's fix such a run came back as `job_id` 0, `timed_out` false,
//! `0 of 0` chunks and a `logs_explorer_filter` of `loom.ci.job_id = 0`.
//! Byte for byte the shape of "a failed job whose log never arrived", plus a
//! filter that silently returns nothing when pasted into Logs Explorer. The
//! committed query now NULLs every job- and log-sourced column on that row
//! and empties the filter; `the_two_absences_are_distinguishable` is what
//! keeps it that way, and the live capture's own 60 all-`0 of 0` rows can no
//! longer be assumed to have all meant the same thing.
//!
//! **Derived, not restated.** Section 5 is located by its `failed_runs` CTE
//! and executed as the committed bytes between two `;`. Each mutation test
//! edits exactly one committed token and asserts the engine's answer changes,
//! so a semantic edit to the artifact fails here by name instead of silently
//! re-tuning a saved view.
//!
//! **What this does NOT establish.** No record went through SigNoz's own
//! ingester or its `logs_v2` as SigNoz actually creates it; this is
//! `clickhouse local` over a hand-written read surface, the same caveat the
//! four sibling proofs carry. Attribute-container placement is not guessed —
//! `signoz_trial_artifacts.rs` independently pins, in ordinary CI, which
//! container the daemon sends each key the artifact reads. The real-canary gap
//! is #8525.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "common/hub_image.rs"]
mod hub_image;

use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Command, Stdio};

/// Same pin as the SigNoz trial's telemetry store (`signoz/casting.yaml`) and
/// as every other engine-level proof in this trial, so no proof in this repo
/// can drift from the deployment on ClickHouse version.
const CLICKHOUSE_IMAGE: &str = "clickhouse/clickhouse-server:25.12.5@sha256:cacf32d6884291dc2ff5e0156a97f46fc53ff7c929a7906d114e268a929dfd3a";

/// The committed retro queries, byte for byte.
const CI_QUERIES: &str = include_str!("../../defaults/observability/signoz/ci-queries.sql");

/// The committed sweep-side extraction section 7 joins against. Applied before
/// the whole-file run so `loom_analytics.raw_ship_outcome` resolves; its
/// `CREATE DATABASE IF NOT EXISTS` / `CREATE OR REPLACE VIEW` are idempotent.
const CYCLE_TIME_EXTRACT: &str =
    include_str!("../../defaults/observability/signoz/cycle-time-extract.sql");

/// The synthetic log read surface section 5 runs over.
const FIXTURE: &str = include_str!("fixtures/signoz_ci_failed_run_logs/fixture.sql");

/// `since`, as the fixture's comment header states it. Bound as a query
/// parameter exactly the way the artifact's documented invocation does.
const SINCE: &str = "2026-09-20 00:00:00";

/// The fixture's in-scope repository.
const REPO: &str = "synthetic/ci-alpha";

/// One result row: column name -> JSON value, as `JSONEachRow` renders it.
/// `JSONEachRow` renders a ClickHouse NULL as JSON `null`, which is the whole
/// point of several assertions below.
type Row = BTreeMap<String, serde_json::Value>;

/// Runs `script` through `clickhouse local` in the pinned image, binding the
/// five parameters `ci-queries.sql`'s documented invocation binds, and returns
/// stdout. Panics with the engine's own stderr on failure — a query that does
/// not parse must fail this test, not be silently skipped.
fn clickhouse(script: &str, repo: &str) -> String {
    let mut child = Command::new("docker")
        .args([
            "run",
            "--rm",
            "-i",
            "--entrypoint",
            "clickhouse",
            &hub_image::resolve(CLICKHOUSE_IMAGE),
            "local",
            "--multiquery",
            "--format=JSONEachRow",
            &format!("--param_since={SINCE}"),
            &format!("--param_repo={repo}"),
            "--param_top=50",
            "--param_bucket_hours=24",
            "--param_window_hours=24",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("docker is required for this test");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "clickhouse rejected the script:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The committed file's statements. Line comments are stripped **before** the
/// split on `;`, mirroring `signoz_usage_queries.rs` / `signoz_cycle_time.rs`
/// / `signoz_queue_quota_queries.rs`: no `;` or `--` occurs inside a string
/// literal in this artifact, so what remains is each statement's exact
/// committed text. The strict verbatim proof is
/// `the_whole_committed_file_executes_on_the_pinned_engine`, which feeds the
/// engine the bytes as committed, comments included.
fn statements(sql: &str) -> Vec<String> {
    let code = sql
        .lines()
        .map(|line| line.find("--").map_or(line, |at| &line[..at]))
        .collect::<Vec<_>>()
        .join("\n");
    code.split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

/// Section 5, located by the `failed_runs` CTE no other section declares.
/// Positional indexing would silently select a neighbour the first time a
/// section is inserted above it.
fn section_five() -> String {
    let found: Vec<String> = statements(CI_QUERIES)
        .into_iter()
        .filter(|s| s.contains("failed_runs AS"))
        .collect();
    assert_eq!(
        found.len(),
        1,
        "exactly one statement in ci-queries.sql declares the failed_runs CTE; \
         found {}. If section 5 was renamed or split, update this locator \
         rather than loosening it.",
        found.len()
    );
    found.into_iter().next().unwrap()
}

/// Runs one query over the fixture and returns its rows.
fn run(query: &str, repo: &str) -> Vec<Row> {
    clickhouse(&format!("{FIXTURE}\n{query};"), repo)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("JSONEachRow line"))
        .collect()
}

/// Section 5 as committed, over the fixture, scoped to `synthetic/ci-alpha`.
fn section_five_rows() -> Vec<Row> {
    run(&section_five(), REPO)
}

/// Section 5 with exactly one committed token replaced. Panics if the token is
/// absent, so a mutation cannot quietly become a no-op that "passes".
fn mutated(from: &str, to: &str) -> String {
    let sql = section_five();
    assert!(
        sql.contains(from),
        "mutation target {from:?} is no longer in section 5; the committed \
         query changed and this mutation no longer tests what it claims"
    );
    sql.replacen(from, to, 1)
}

fn num(row: &Row, key: &str) -> Option<i64> {
    row.get(key)
        .unwrap_or_else(|| panic!("column {key} missing from {row:?}"))
        .as_i64()
}

fn text<'a>(row: &'a Row, key: &str) -> Option<&'a str> {
    row.get(key)
        .unwrap_or_else(|| panic!("column {key} missing from {row:?}"))
        .as_str()
}

fn flag(row: &Row, key: &str) -> Option<bool> {
    let value = row
        .get(key)
        .unwrap_or_else(|| panic!("column {key} missing from {row:?}"));
    if value.is_null() {
        None
    } else {
        Some(
            value
                .as_bool()
                .unwrap_or_else(|| panic!("column {key} is not a bool in {row:?}")),
        )
    }
}

/// The one row for `job_id`, which must be unique in the committed result.
fn job_row(rows: &[Row], job_id: i64) -> &Row {
    let matches: Vec<&Row> = rows
        .iter()
        .filter(|r| num(r, "job_id") == Some(job_id))
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "expected exactly one row for job {job_id}, got {}: {matches:?}",
        matches.len()
    );
    matches[0]
}

// ---------------------------------------------------------------------------
// Does the committed file still run at all?
// ---------------------------------------------------------------------------

/// The strict verbatim proof: every statement in `ci-queries.sql`, as the
/// bytes on disk, parses and executes on the pinned engine.
///
/// This is deliberately weaker than the section-5 tests and says so. Fourteen
/// of this file's sections read tables the fixture leaves empty, so they prove
/// only that they still *parse and execute* against the real read-surface
/// column names and types — the engine rejects an unknown identifier whether
/// or not rows exist, which is exactly the regression a `WHERE` on a renamed
/// SigNoz column would be. Their *behaviour* was established on the live trial
/// capture (592 runs / 2,131 jobs, `evidence.md`), not here.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn the_whole_committed_file_executes_on_the_pinned_engine() {
    let script = format!("{FIXTURE}\n{CYCLE_TIME_EXTRACT}\n{CI_QUERIES}");
    let out = clickhouse(&script, REPO);
    assert!(
        !out.trim().is_empty(),
        "the committed file produced no output at all over a seeded fixture; \
         section 5 alone must return rows"
    );
    // Exercised above as one script, so a failure anywhere aborts the run.
    // Re-assert the statement count the section locator depends on.
    assert!(statements(CI_QUERIES).len() > 15, "ci-queries.sql lost statements unexpectedly");
}

// ---------------------------------------------------------------------------
// The chunk join itself — the part no deployment has ever exercised.
// ---------------------------------------------------------------------------

/// A failed job's captured log is reported chunk by chunk, and a partial
/// capture is visible as a partial capture.
///
/// Job 70001's log arrived complete: chunks 0, 1 and 2 of 3. Job 70002's
/// arrived truncated AND incomplete: chunks 0 and 2 of 3, chunk 1 lost in
/// flight. `chunks_present` and `chunk_count` exist as two columns precisely
/// so `2 of 3` can be told from `3 of 3`; until this test, nothing had ever
/// produced either.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn a_captured_log_is_reported_chunk_by_chunk() {
    let rows = section_five_rows();

    let complete = job_row(&rows, 70001);
    assert_eq!(num(complete, "chunks_present"), Some(3));
    assert_eq!(num(complete, "chunk_count"), Some(3));
    assert_eq!(flag(complete, "truncated"), Some(false));
    assert_eq!(flag(complete, "timed_out"), Some(false));
    assert_eq!(
        text(complete, "logs_explorer_filter"),
        Some("loom.ci.job_id = 70001"),
        "the filter must name the job whose log it reconstructs"
    );

    let partial = job_row(&rows, 70002);
    assert_eq!(
        num(partial, "chunks_present"),
        Some(2),
        "chunk 1 of job 70002's log never arrived"
    );
    assert_eq!(num(partial, "chunk_count"), Some(3));
    assert_eq!(
        flag(partial, "truncated"),
        Some(true),
        "`truncated` is true on EVERY chunk of a capped log, so max() reads it \
         from either surviving chunk"
    );
    assert_eq!(flag(partial, "timed_out"), Some(true));
}

/// A chunk delivered twice does not inflate the captured count.
///
/// Loom's telemetry journal is at-least-once: `envelope_identity` keys a
/// `ci.job.log` on `repo|job_id|chunk_index`, so a committed-but-unconfirmed
/// unit replays as a byte-identical chunk. The committed query counts
/// `uniqExact(chunk_index)`; `count()` would report **4 of 3** for job 70001 —
/// more of the log captured than the log has, which reads as a corrupt record
/// rather than as the ordinary redelivery it is.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn a_replayed_chunk_does_not_inflate_the_captured_count() {
    assert_eq!(num(job_row(&section_five_rows(), 70001), "chunks_present"), Some(3));

    let naive = run(
        &mutated("uniqExact(toUInt32(attributes_number['loom.ci.chunk_index']))", "count()"),
        REPO,
    );
    assert_eq!(
        num(job_row(&naive, 70001), "chunks_present"),
        Some(4),
        "counting rows instead of distinct chunk indices must over-report the \
         replayed chunk; if it does not, this fixture stopped containing a \
         duplicate and the committed uniqExact() is no longer being tested"
    );
}

/// A replayed `ci.run` record does not fan out every job of that run.
///
/// `ci.run` is journal-keyed on `repo|run_id|run_attempt`, so it replays too.
/// `LIMIT 1 BY repo, run_id, run_attempt` in `failed_runs` is the only thing
/// standing between one redelivery and a doubled failure report.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn a_replayed_run_record_does_not_fan_out_its_jobs() {
    assert_eq!(section_five_rows().len(), 5);

    let fanned = run(&mutated("LIMIT 1 BY repo, run_id, run_attempt", ""), REPO);
    assert_eq!(
        fanned.len(),
        9,
        "without the run-level dedupe, run 9001's duplicate record must double \
         each of its four job rows (4 + 4 + 1 for run 9002)"
    );
}

// ---------------------------------------------------------------------------
// Scope item 5: missing must not read as zero.
// ---------------------------------------------------------------------------

/// The two absences this section can report are distinguishable from each
/// other, and neither reads as a successful completion.
///
/// * Job 70003 failed and its log never arrived: `job_id` set, `0 of 0`. The
///   committed comment's documented reading — log capture disabled, the repo
///   log-excluded, or the download still pending — never "the log was empty".
/// * Run 9002 failed with no non-successful job at all (`startup_failure`):
///   every job- and log-sourced column NULL and an empty
///   `logs_explorer_filter`.
///
/// Before #8528's fix the second case was reported as `job_id` 0, `0 of 0` and
/// `loom.ci.job_id = 0` — identical in shape to the first, and a filter that
/// returns nothing when pasted into Logs Explorer. `truncated` is the sharpest
/// case: a bool column has no zero that means "unknown", so on a join miss it
/// would read `false`, asserting that a log nobody has was not truncated.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn the_two_absences_are_distinguishable() {
    let rows = section_five_rows();

    let log_missing = job_row(&rows, 70003);
    assert_eq!(num(log_missing, "chunks_present"), Some(0));
    assert_eq!(num(log_missing, "chunk_count"), Some(0));
    assert_eq!(
        text(log_missing, "logs_explorer_filter"),
        Some("loom.ci.job_id = 70003"),
        "the job exists, so its filter must still be usable even though no \
         chunk arrived — that is how an operator checks whether capture is on"
    );

    let no_failing_job: Vec<&Row> = rows
        .iter()
        .filter(|r| num(r, "run_id") == Some(9002))
        .collect();
    assert_eq!(no_failing_job.len(), 1);
    let row = no_failing_job[0];
    assert_eq!(text(row, "run_conclusion"), Some("startup_failure"));
    for column in [
        "job",
        "job_id",
        "job_conclusion",
        "timed_out",
        "chunks_present",
        "chunk_count",
        "truncated",
    ] {
        assert!(
            row[column].is_null(),
            "a run with no non-successful job must report {column} as NULL, not \
             as its type's zero; got {:?}",
            row[column]
        );
    }
    assert_eq!(
        text(row, "logs_explorer_filter"),
        Some(""),
        "there is no job log to open, so the filter must be empty rather than \
         `loom.ci.job_id = 0`, which silently matches nothing"
    );

    // And the run is still reported. An INNER JOIN would read as "no failed
    // runs had anything wrong with them", the quietest possible failure.
    let inner = run(&mutated("LEFT JOIN failed_jobs", "INNER JOIN failed_jobs"), REPO);
    assert!(
        !inner.iter().any(|r| num(r, "run_id") == Some(9002)),
        "an INNER JOIN must drop the failed run that has no failing job — if \
         it does not, this fixture no longer contains such a run"
    );
    assert_eq!(inner.len(), 4);
}

/// A `ci.job` record is not mistaken for a log chunk.
///
/// The `job_logs` CTE selects on `mapContains(attributes_number,
/// 'loom.ci.chunk_index')` rather than `body = 'ci.job.log'`. That is sound —
/// `CiJobLogRecord` is the only record kind that sets `chunk_index` — but it
/// is a predicate on a shape, not on an identity, so it is worth knowing what
/// it costs if the shape stops being unique: job 70003, which has no log at
/// all, starts reporting `1 of 0`. One chunk present out of a log zero chunks
/// long is not a reading an operator can act on.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn only_log_chunk_records_feed_the_chunk_counts() {
    let loosened = run(
        &mutated(
            "WHERE mapContains(attributes_number, 'loom.ci.chunk_index')\n      AND timestamp",
            "WHERE timestamp",
        ),
        REPO,
    );
    let row = job_row(&loosened, 70003);
    assert_eq!(num(row, "chunks_present"), Some(1));
    assert_eq!(
        num(row, "chunk_count"),
        Some(0),
        "the job's own ci.job record carries no chunk_count, so dropping the \
         predicate produces chunks_present > chunk_count — impossible for a \
         real log"
    );
}

// ---------------------------------------------------------------------------
// The join keys, which are asymmetric on purpose.
// ---------------------------------------------------------------------------

/// The run/job attempt join reads the two different keys the daemon actually
/// emits, and a retry's jobs stay off the failed attempt.
///
/// `ci.run` carries `loom.ci.run_attempt` (`records.rs:723`); `ci.job` carries
/// `loom.ci.attempts` (`records.rs:820`). "Tidying" `failed_jobs` to read
/// `run_attempt` — the obvious-looking symmetry fix — makes the join match
/// nothing, and because it is a LEFT JOIN the result does not shrink or error:
/// every failed run still appears, now claiming it had no failing job.
/// Post-fix that at least reads as NULL rather than as `0 of 0`; it is still
/// wrong, and nothing but execution catches it.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn the_attempt_join_uses_the_key_each_record_kind_actually_emits() {
    let rows = section_five_rows();
    assert!(
        !rows.iter().any(|r| num(r, "job_id") == Some(70020)),
        "job 70020 belongs to run 9001 attempt 2, which SUCCEEDED; a retry's \
         jobs must never be attributed to the failed attempt"
    );

    let symmetric = run(
        &mutated(
            "toUInt32(attributes_number['loom.ci.attempts'])",
            "toUInt32(attributes_number['loom.ci.run_attempt'])",
        ),
        REPO,
    );
    assert_eq!(
        symmetric.len(),
        2,
        "with the wrong attempt key every job drops out, leaving one row per \
         failed run"
    );
    assert!(
        symmetric.iter().all(|r| r["job_id"].is_null()),
        "...and every one of them claims the run had no failing job: {symmetric:?}"
    );
}

/// Another repository's log chunks are not counted toward this one's job.
///
/// The `job_logs` CTE applies no repository predicate of its own — only
/// `l.repo = j.repo` on the join keeps a chunk with a colliding `job_id` out.
/// GitHub job ids are globally unique, so the collision the fixture stages is
/// synthetic; the join condition it exercises is real, and an unexecuted
/// defensive condition is exactly the kind that gets "simplified" away. With
/// it removed, `synthetic/ci-beta`'s private 5-chunk log is attributed to
/// `synthetic/ci-alpha`'s job — a cross-repository leak that reports MORE log
/// captured than exists, on a row scoped to a repo the chunks never belonged
/// to.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn another_repositorys_chunks_do_not_count_toward_this_job() {
    assert_eq!(num(job_row(&section_five_rows(), 70001), "chunk_count"), Some(3));

    let leaky = run(
        &mutated("ON l.repo = j.repo AND l.job_id = j.job_id", "ON l.job_id = j.job_id"),
        REPO,
    );
    let beta_leak: Vec<&Row> = leaky
        .iter()
        .filter(|r| num(r, "job_id") == Some(70001) && num(r, "chunk_count") == Some(5))
        .collect();
    assert_eq!(
        beta_leak.len(),
        1,
        "without the repo condition, ci-beta's 5-chunk log must appear on \
         ci-alpha's job 70001: {leaky:?}"
    );
    assert_eq!(num(beta_leak[0], "chunks_present"), Some(2));
}

// ---------------------------------------------------------------------------
// The bindings the saved view is parameterised on.
// ---------------------------------------------------------------------------

/// `since` and `repo` bind the result, and an unbound `repo` is cross-repo
/// rather than empty.
///
/// The out-of-window run 8000 failed, had a failed job and had a complete
/// one-chunk log — everything section 5 looks for — but completed before
/// `since`. `repo = ''` is the artifact's documented "all repositories"
/// binding, not a filter that matches the empty string.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn since_and_repo_bind_the_result() {
    let scoped = section_five_rows();
    assert!(
        !scoped.iter().any(|r| num(r, "run_id") == Some(8000)),
        "run 8000 completed before `since` and must not appear"
    );
    assert!(
        scoped.iter().all(|r| text(r, "repo") == Some(REPO)),
        "`repo` must scope the result: {scoped:?}"
    );

    let all = run(&section_five(), "");
    let beta: Vec<&Row> = all
        .iter()
        .filter(|r| text(r, "repo") == Some("synthetic/ci-beta"))
        .collect();
    assert_eq!(
        beta.len(),
        1,
        "an empty `repo` means every repository, so ci-beta's timed-out run \
         must appear: {all:?}"
    );
    assert_eq!(num(beta[0], "chunks_present"), Some(2));
    assert_eq!(num(beta[0], "chunk_count"), Some(5));
    assert!(
        !all.iter().any(|r| num(r, "run_id") == Some(8000)),
        "the window still applies when the repo is unbound"
    );
}

/// Only a run's non-successful jobs are listed, and "non-successful" is wider
/// than "failed".
///
/// The committed predicate is `NOT IN ('success', 'skipped', 'neutral')`, so a
/// `cancelled` job is reported (job 70006) while a `skipped` one is not (job
/// 70005) — a cancelled job is a thing that went wrong, a skipped one is a
/// thing that correctly did not run. Narrowing this to `= 'failure'` is a
/// one-token edit that hides a whole failure class.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn only_non_successful_jobs_are_listed() {
    let rows = section_five_rows();
    let listed: Vec<i64> = rows.iter().filter_map(|r| num(r, "job_id")).collect();
    assert_eq!(
        listed,
        vec![70001, 70002, 70003, 70006],
        "70004 succeeded and 70005 was skipped; 70006 was cancelled and counts"
    );
    assert_eq!(text(job_row(&rows, 70006), "job_conclusion"), Some("cancelled"));

    let narrowed = run(
        &mutated(
            "attributes_string['loom.ci.conclusion'] NOT IN ('success', 'skipped', 'neutral')",
            "attributes_string['loom.ci.conclusion'] = 'failure'",
        ),
        REPO,
    );
    assert!(
        !narrowed.iter().any(|r| num(r, "job_id") == Some(70006)),
        "narrowing the predicate to 'failure' must drop the cancelled job"
    );
}

// ---------------------------------------------------------------------------
// Static guard — runs in ordinary CI, no Docker.
// ---------------------------------------------------------------------------

/// Every job- and log-sourced column of section 5's projection is NULL-guarded
/// against the job-level join miss.
///
/// The engine-level proof above only runs in the Docker-gated job. This is the
/// same requirement in ordinary CI, so a future edit that drops a guard and
/// reintroduces the `0 of 0` / `loom.ci.job_id = 0` ambiguity fails on every
/// pull request rather than waiting for the gated suite.
#[test]
fn section_five_null_guards_every_job_sourced_column() {
    // The OUTER projection only: `AS job_id` also occurs inside the
    // `failed_jobs` CTE, where no guard belongs.
    let sql = section_five();
    let join = sql
        .find("FROM failed_runs AS r")
        .expect("section 5 must still select FROM failed_runs");
    let head = &sql[..join];
    let select = head
        .rfind("SELECT ")
        .expect("section 5 must have an outer SELECT");
    let projection = &head[select..];

    // (source expression, output column) for every column the LEFT JOIN can
    // leave unmatched.
    for (source, column) in [
        ("j.job", "job"),
        ("j.job_id", "job_id"),
        ("j.job_conclusion", "job_conclusion"),
        ("j.timed_out", "timed_out"),
        ("l.chunks_present", "chunks_present"),
        ("l.chunk_count", "chunk_count"),
        ("l.truncated", "truncated"),
    ] {
        let guarded = format!("if(j.job_id = 0, NULL, {source}) AS {column}");
        assert!(
            projection.contains(&guarded),
            "section 5's `{column}` is sourced from the failed_jobs/job_logs \
             side of a LEFT JOIN, so a run with no non-successful job would \
             report it as its type's zero. It must stay projected as \
             `{guarded}` (see `the_two_absences_are_distinguishable`). \
             Outer projection:\n{projection}"
        );
    }
    assert!(
        projection.contains("if(j.job_id = 0, '', concat('loom.ci.job_id = '"),
        "section 5's logs_explorer_filter must be empty when the job join \
         missed; `loom.ci.job_id = 0` is a Logs Explorer query that silently \
         returns nothing"
    );
}

// ---------------------------------------------------------------------------
// Section 18 (#10670): main's cancellations, split by whether the run started.
// ---------------------------------------------------------------------------

/// A repository the shared fixture never seeds, so section 18's counts here
/// come only from [`MAIN_CANCEL_ROWS`].
const MAIN_REPO: &str = "synthetic/ci-main";

/// One `ci.run` row (and, per `jobs`, its `ci.job` rows) for [`MAIN_REPO`].
fn main_run(
    run_id: u64,
    attempt: u32,
    event: &str,
    git_ref: &str,
    conclusion: &str,
    jobs: &[u64],
) -> String {
    let mut out = format!(
        "(1789992000000000000, 'ci.run', \
         {{'loom.repo': '{MAIN_REPO}', 'loom.ci.workflow': 'CI', 'loom.ci.conclusion': '{conclusion}', \
           'loom.ci.event': '{event}', 'loom.ci.ref': '{git_ref}'}}, \
         {{'loom.ci.run_id': {run_id}, 'loom.ci.run_attempt': {attempt}}}, {{}}, {{}})"
    );
    for job_id in jobs {
        out.push_str(&format!(
            ",\n(1789992000000000000, 'ci.job', \
             {{'loom.repo': '{MAIN_REPO}', 'loom.ci.workflow': 'CI', 'loom.ci.job': 'Build', \
               'loom.ci.conclusion': 'cancelled'}}, \
             {{'loom.ci.run_id': {run_id}, 'loom.ci.attempts': {attempt}, 'loom.ci.job_id': {job_id}}}, \
             {{'loom.ci.timed_out': false}}, {{}})"
        ));
    }
    out
}

/// The scenario #10670 describes, plus the rows section 18 must exclude.
fn main_cancel_rows() -> String {
    let rows = [
        // The oldest run: it ran and finished.
        main_run(5001, 1, "push", "main", "success", &[80001]),
        // Pending runs superseded by the concurrency bound: no job at all.
        // 5003 is delivered twice (at-least-once) and must count once.
        main_run(5002, 1, "push", "main", "cancelled", &[]),
        main_run(5003, 1, "push", "main", "cancelled", &[]),
        main_run(5003, 1, "push", "main", "cancelled", &[]),
        // A STARTED main run that was cancelled: the rule-2 violation.
        main_run(5004, 1, "push", "main", "cancelled", &[80004, 80005]),
        // A red main run, `refs/heads/main` spelling.
        main_run(5007, 1, "push", "refs/heads/main", "failure", &[80007]),
        // Attempt 2 of a run whose attempt 1 started: the attempts are
        // separate rows, and attempt 2 (no job) is superseded, not started.
        main_run(5008, 1, "push", "main", "failure", &[80008]),
        main_run(5008, 2, "push", "main", "cancelled", &[]),
        // Excluded: a superseded PR run, and a push to a non-default branch.
        main_run(5005, 1, "pull_request", "feature/issue-1", "cancelled", &[80050]),
        main_run(5006, 1, "push", "feature/issue-2", "cancelled", &[80060]),
    ];
    format!(
        "INSERT INTO signoz_logs.logs_v2 \
         (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string) VALUES\n{};",
        rows.join(",\n")
    )
}

/// Section 18, located by the `main_runs` CTE no other section declares.
fn section_eighteen() -> String {
    let found: Vec<String> = statements(CI_QUERIES)
        .into_iter()
        .filter(|s| s.contains("main_runs AS"))
        .collect();
    assert_eq!(
        found.len(),
        1,
        "exactly one statement in ci-queries.sql declares the main_runs CTE; found {}",
        found.len()
    );
    found.into_iter().next().unwrap()
}

fn section_eighteen_rows(sql: &str) -> Vec<Row> {
    clickhouse(&format!("{FIXTURE}\n{}\n{sql};", main_cancel_rows()), MAIN_REPO)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("JSONEachRow line"))
        .collect()
}

/// A pending run the concurrency bound superseded (no job) and a started run
/// that was cancelled (has a job) land in different columns, so a regression
/// to #7779's "cancel every in-progress main run" shows as a non-zero
/// `cancelled_after_start` instead of hiding inside a high cancel rate.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn section_eighteen_separates_superseded_from_started_cancellations() {
    let rows = section_eighteen_rows(&section_eighteen());
    assert_eq!(rows.len(), 1, "one row for {MAIN_REPO}/CI: {rows:?}");
    let r = &rows[0];
    assert_eq!(text(r, "repo"), Some(MAIN_REPO));
    assert_eq!(text(r, "workflow"), Some("CI"));
    // 5001, 5002, 5003 (once), 5004, 5007, 5008#1, 5008#2. Not 5005 (PR) or
    // 5006 (other branch).
    assert_eq!(num(r, "runs"), Some(7), "{r:?}");
    assert_eq!(num(r, "success"), Some(1), "{r:?}");
    assert_eq!(num(r, "failed"), Some(2), "{r:?}");
    assert_eq!(num(r, "cancelled"), Some(4), "{r:?}");
    assert_eq!(num(r, "superseded_before_start"), Some(3), "{r:?}");
    assert_eq!(num(r, "cancelled_after_start"), Some(1), "{r:?}");
    assert_eq!(num(r, "unreported"), Some(0), "{r:?}");
    let ratio = |k: &str| {
        r[k].as_f64()
            .unwrap_or_else(|| panic!("{k} not a number: {r:?}"))
    };
    assert!((ratio("cancelled_ratio") - 0.571).abs() < 1e-9, "{r:?}");
    assert!((ratio("verified_ratio") - 0.429).abs() < 1e-9, "{r:?}");
}

/// A run whose `ci.run` record lands just after `since` but whose job record
/// is stamped just before it still reads as STARTED: `run_jobs` looks back a
/// day before `since`, so the job join is not cut at the window edge.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn section_eighteen_keeps_jobs_stamped_just_before_since() {
    // SINCE (2026-09-20 00:00:00 UTC) is 1789862400; the run lands 1 min
    // after it, its job 1 h before it.
    let edge = format!(
        "INSERT INTO signoz_logs.logs_v2 \
         (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string) VALUES\n\
         (1789862460000000000, 'ci.run', \
          {{'loom.repo': '{MAIN_REPO}', 'loom.ci.workflow': 'CI', 'loom.ci.conclusion': 'cancelled', \
            'loom.ci.event': 'push', 'loom.ci.ref': 'main'}}, \
          {{'loom.ci.run_id': 5009, 'loom.ci.run_attempt': 1}}, {{}}, {{}}),\n\
         (1789858800000000000, 'ci.job', \
          {{'loom.repo': '{MAIN_REPO}', 'loom.ci.workflow': 'CI', 'loom.ci.job': 'Build', \
            'loom.ci.conclusion': 'cancelled'}}, \
          {{'loom.ci.run_id': 5009, 'loom.ci.attempts': 1, 'loom.ci.job_id': 80009}}, \
          {{'loom.ci.timed_out': false}}, {{}});"
    );
    let rows: Vec<Row> = clickhouse(
        &format!("{FIXTURE}\n{}\n{edge}\n{};", main_cancel_rows(), section_eighteen()),
        MAIN_REPO,
    )
    .lines()
    .filter(|l| !l.trim().is_empty())
    .map(|l| serde_json::from_str(l).expect("JSONEachRow line"))
    .collect();
    assert_eq!(num(&rows[0], "cancelled_after_start"), Some(2), "{rows:?}");
    assert_eq!(num(&rows[0], "superseded_before_start"), Some(3), "{rows:?}");
}

/// The run-attempt join key is load-bearing: joining on run_id alone would
/// let attempt 1's job mark attempt 2 (never started) as started.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn section_eighteen_joins_on_the_run_attempt() {
    let sql = section_eighteen();
    let from = " AND j.run_attempt = r.run_attempt";
    assert!(sql.contains(from), "section 18 no longer joins on the run attempt");
    let rows = section_eighteen_rows(&sql.replacen(from, "", 1));
    assert_eq!(num(&rows[0], "cancelled_after_start"), Some(2), "{rows:?}");
}
