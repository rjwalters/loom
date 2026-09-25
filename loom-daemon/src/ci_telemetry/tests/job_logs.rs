//! Phase-2 job-log capture tests (Issue #8825) — chunking, the per-job cap
//! and its truncation marker, the log-only exclusion key, and the
//! `logs_done` idempotency that is deliberately independent of the phase-1
//! `ci.job` record's.
//!
//! A sibling module of `super` (the phase-1 suite) rather than more lines in
//! it: `ci_telemetry/tests.rs` is at the file-size ratchet, and these tests
//! are a self-contained concern.

use super::*;

#[test]
fn chunking_splits_on_line_boundaries_and_never_exceeds_the_chunk_size() {
    let text: String = (0..500)
        .map(|n| format!("2026-09-20T09:00:00.0000000Z line number {n} of the build log\n"))
        .collect();
    let chunked = logs::chunk(&text, logs::DEFAULT_MAX_BYTES, 1024);
    assert!(!chunked.truncated);
    assert_eq!(chunked.total_bytes, text.len());
    assert!(chunked.chunks.len() > 10, "{} chunk(s)", chunked.chunks.len());
    for chunk in &chunked.chunks {
        assert!(chunk.len() <= 1024, "chunk of {} bytes exceeds the limit", chunk.len());
        assert!(chunk.ends_with('\n'), "a chunk ended mid-line: {chunk:?}");
        for line in chunk.lines() {
            assert!(line.starts_with("2026-09-20T"), "a line was split: {line:?}");
        }
    }
    // Concatenation reproduces the original exactly — ordering by
    // chunk_index is the whole reconstruction contract.
    assert_eq!(chunked.chunks.concat(), text);
}

#[test]
fn a_bom_is_stripped_and_a_single_oversized_line_is_split_at_char_boundaries() {
    let chunked = logs::chunk("\u{feff}hello\n", logs::DEFAULT_MAX_BYTES, 1024);
    assert_eq!(chunked.chunks, vec!["hello\n".to_string()]);

    // One line longer than a whole chunk: the ≤ chunk-size invariant is
    // absolute, so this is the documented exception to "never mid-line".
    // Multi-byte chars must still never be cut in half.
    let line = format!("{}\n", "é".repeat(100));
    let chunked = logs::chunk(&line, logs::DEFAULT_MAX_BYTES, 64);
    assert!(chunked.chunks.len() > 1);
    for chunk in &chunked.chunks {
        assert!(chunk.len() <= 64);
    }
    assert_eq!(chunked.chunks.concat(), line);
}

/// A cap that lands **inside a multi-byte character** must truncate, not
/// panic. This is not hypothetical: the cap is an arbitrary byte count and
/// build logs carry UTF-8 (rustc arrows, box-drawing, emoji), so byte-slicing
/// the text at the cap would abort the poll cycle — and then every cycle
/// after it, forever, because the same job stays pending.
#[test]
fn a_cap_landing_mid_character_truncates_instead_of_panicking() {
    // "é" is two bytes. With `count` of them per line and a cap chosen to
    // land on the second byte of one, a naive `&text[..cap]` panics.
    let text: String = (0..20)
        .map(|n| format!("{} é{n}\n", "é".repeat(5)))
        .collect();
    for cap in 1..text.len() {
        let chunked = logs::chunk(&text, cap, 16);
        assert!(
            chunked.chunks.concat().len() <= cap + chunked.note.as_ref().map_or(0, String::len)
        );
        // Whatever survived is whole lines (the marker chunk aside).
        let kept = chunked.chunks.len() - usize::from(chunked.truncated);
        for chunk in &chunked.chunks[..kept] {
            assert!(chunk.is_char_boundary(chunk.len()));
        }
        if chunked.truncated {
            assert!(chunked.note.is_some());
        }
    }
    // And the no-truncation case is byte-identical.
    assert_eq!(logs::chunk(&text, text.len(), 16).chunks.concat(), text);
}

/// A repo added to `logCaptureExcludedRepos` **after** its jobs were already
/// wanted stops being downloaded on the next cycle — an exclusion is usually
/// added *because* something in those logs must not be stored, so honouring it
/// only for future jobs would miss the point.
#[test]
fn an_exclusion_added_after_the_want_stops_the_download() {
    let dir = TempDir::new().unwrap();
    // Cycle 1: capture on for both repos, but every alpha download fails, so
    // alpha's wants outlive the cycle while beta's are captured.
    let api = FixtureApi::new();
    let alpha_jobs: Vec<u64> = (1..=3)
        .flat_map(|run| (1..=4).map(move |j| 10_000 + run * 10 + j))
        .collect();
    for job_id in &alpha_jobs {
        api.fail_log_times(&logs::logs_path("fixture-org/alpha", *job_id), 1);
    }
    let first = run_cycle(&ctx_with_logs(dir.path()), &api).unwrap();
    assert_eq!(first.summary.logs_captured, 12, "beta's 12 jobs only: {}", first.summary());
    assert!(reconstruct_logs(dir.path())
        .keys()
        .all(|job_id| *job_id >= 20000));

    // Cycle 2: alpha is now log-excluded. Its pending logs are not fetched —
    // without the download-time check they all would be, since the want is
    // already in the ledger. Its records stay captured either way.
    let excluded = CycleContext {
        log_excluded_repos: vec!["fixture-org/alpha".into()],
        ..ctx_with_logs(dir.path())
    };
    let second = run_cycle(&excluded, &FixtureApi::new()).unwrap();
    assert_eq!(second.summary.logs_captured, 0, "{}", second.summary());
    let by_job = reconstruct_logs(dir.path());
    assert!(
        by_job.keys().all(|job_id| *job_id >= 20000),
        "an excluded repo's pending logs were downloaded anyway: {:?}",
        by_job.keys().collect::<Vec<_>>()
    );

    // Lifting the exclusion resumes the capture: the wants were parked, not
    // discarded.
    let third = run_cycle(&ctx_with_logs(dir.path()), &FixtureApi::new()).unwrap();
    assert_eq!(third.summary.logs_captured, 11, "the 410 job stays failed: {}", third.summary());
    assert!(reconstruct_logs(dir.path()).contains_key(&10011));
}

#[test]
fn the_cap_truncates_on_a_line_boundary_and_the_marker_names_it() {
    let text: String = (0..2000)
        .map(|n| format!("2026-09-20T09:00:00.0000000Z line number {n}\n"))
        .collect();
    let cap = 4096;
    let chunked = logs::chunk(&text, cap, logs::CHUNK_BYTES);
    assert!(chunked.truncated);
    assert_eq!(chunked.total_bytes, text.len());

    let note = chunked.note.clone().unwrap();
    assert!(note.contains(&cap.to_string()), "the marker must name the cap: {note}");
    assert!(note.contains("logCaptureMaxBytes"), "{note}");
    assert_eq!(chunked.chunks.last(), Some(&note));

    // AC4: total emitted bytes <= cap + one marker record.
    let text_bytes: usize = chunked.chunks[..chunked.chunks.len() - 1]
        .iter()
        .map(String::len)
        .sum();
    assert!(text_bytes <= cap, "{text_bytes} bytes of text exceeds the {cap}-byte cap");
    assert!(chunked.chunks[..chunked.chunks.len() - 1]
        .iter()
        .all(|chunk| chunk.ends_with('\n')));
}

#[test]
fn every_chunk_of_a_truncated_log_reads_as_truncated() {
    let text: String = (0..2000)
        .map(|n| format!("2026-09-20T09:00:00.0000000Z line number {n}\n"))
        .collect();
    let chunked = logs::chunk(&text, 4096, logs::CHUNK_BYTES);
    let target = logs::LogTarget {
        repo: "fixture-org/alpha".into(),
        visibility: crate::telemetry::RepoVisibility::Private,
        run_id: 1001,
        job_id: 10011,
        attempt: 1,
        workflow: "CI".into(),
        job: "job-1".into(),
        completed_at: now(),
    };
    let envelopes = logs::log_envelopes(&target, &chunked, "fixture-host");
    let records: Vec<_> = envelopes
        .iter()
        .filter_map(|env| match &env.record {
            TelemetryRecord::CiJobLog(r) => Some(r.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(records.len(), chunked.chunks.len());
    for (index, record) in records.iter().enumerate() {
        assert!(record.truncated, "chunk {index} does not read as truncated");
        assert_eq!(record.chunk_index, u32::try_from(index).unwrap());
        assert_eq!(record.chunk_count, u32::try_from(records.len()).unwrap());
        assert_eq!(record.log_bytes_total, chunked.total_bytes as u64);
    }
    // Only the final marker carries the note.
    assert!(records[..records.len() - 1]
        .iter()
        .all(|r| r.truncation_note.is_none()));
    assert!(records.last().unwrap().truncation_note.is_some());
    // Chunks correlate to their job's span, like the ci.job record does.
    assert!(envelopes.iter().all(|env| env.trace_context.is_some()));
}

#[test]
fn a_cycle_with_capture_on_reconstructs_each_job_log_in_chunk_order() {
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    let report = run_cycle(&ctx_with_logs(dir.path()), &api).unwrap();
    // 24 jobs, minus the one whose log is permanently gone (410).
    assert_eq!(report.summary.logs_captured, 23, "{}", report.summary());
    assert_eq!(report.summary.log_failures, 1);
    assert_eq!(report.summary.logs_truncated, 1, "the 400-line fixture log must truncate");
    assert_no_duplicates(dir.path());

    let by_job = reconstruct_logs(dir.path());
    assert_eq!(by_job.len(), 23);
    assert!(!by_job.contains_key(&10013), "a failed download must emit nothing");

    // The realistic committed fixture reconstructs byte-for-byte, BOM
    // stripped — and the daemon did NOT scrub its credential-shaped line,
    // because the gateway is the redaction boundary.
    let chunks = &by_job[&10011];
    let text: String = chunks.iter().map(|c| c.text.as_str()).collect();
    assert!(text.starts_with("2026-09-20T09:00:10.1000000Z Current runner version"));
    assert!(text.contains("ghp_FIXTUREAAAAAAAAAAAAAAAAAAAAAAAAAAA"));
    assert!(text.ends_with("##[error]Process completed with exit code 1.\n"));
    assert!(chunks.iter().all(|c| !c.truncated));
    assert_eq!(chunks[0].job, "job-1");
    assert_eq!(chunks[0].workflow, "CI alpha");

    // The big one truncated, and says so on every chunk.
    let big = &by_job[&10012];
    assert!(big.iter().all(|c| c.truncated));
    assert!(big.last().unwrap().truncation_note.is_some());
    let captured: usize =
        big.iter().map(|c| c.text.len()).sum::<usize>() - big.last().unwrap().text.len();
    assert!(captured <= TEST_LOG_CAP);
}

/// AC3: re-poll and restart leave captured logs untouched, and a failed
/// download retries **without** re-emitting the `ci.job` record that already
/// landed — the two are independently idempotent.
#[test]
fn logs_done_is_idempotent_and_a_failed_download_retries_on_the_next_cycle() {
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    let log_path = logs::logs_path("fixture-org/alpha", 10011);
    api.fail_log_times(&log_path, 1);

    let first = run_cycle(&ctx_with_logs(dir.path()), &api).unwrap();
    assert_eq!(first.summary.jobs_emitted, 24);
    assert_eq!(first.summary.log_failures, 2, "the flaky job plus the 410 job");
    assert!(!reconstruct_logs(dir.path()).contains_key(&10011));

    // Second cycle: nothing new to poll, but the flaky job's log is retried
    // and succeeds. No run/job record is re-emitted.
    let second = run_cycle(&ctx_with_logs(dir.path()), &FixtureApi::new()).unwrap();
    assert_eq!((second.summary.runs_emitted, second.summary.jobs_emitted), (0, 0));
    assert_eq!(second.summary.logs_captured, 1);
    assert!(reconstruct_logs(dir.path()).contains_key(&10011));
    assert_no_duplicates(dir.path());

    // Third cycle: everything captured stays captured (a restart reads the
    // same ledger, so this is the restart case too).
    let before = journal(dir.path()).len();
    let third = run_cycle(&ctx_with_logs(dir.path()), &FixtureApi::new()).unwrap();
    assert_eq!(third.summary.logs_captured, 0);
    assert_eq!(third.summary.job_logs_emitted, 0);
    assert_eq!(journal(dir.path()).len(), before);
    assert_no_duplicates(dir.path());
}

#[test]
fn a_permanently_failing_log_stops_retrying_and_is_reported_as_failed() {
    let dir = TempDir::new().unwrap();
    for _ in 0..(logs::MAX_ATTEMPTS + 2) {
        run_cycle(&ctx_with_logs(dir.path()), &FixtureApi::new()).unwrap();
    }
    let ledger = Ledger::open_read_only(state_dir(dir.path()).join("seen.jsonl")).unwrap();
    let counts = ledger.log_counts();
    assert_eq!(counts.failed, 1, "the 410 job must land in the failed bucket");
    assert_eq!(counts.pending, 0, "a given-up job must not sit in pending forever");
    assert_eq!(counts.done, 23);
    assert!(ledger
        .pending_logs()
        .iter()
        .all(|target| target.job_id != 10013));
    let (repo, job_id, attempts, error) = ledger.last_log_failure().unwrap();
    assert_eq!((repo.as_str(), job_id), ("fixture-org/alpha", 10013));
    assert_eq!(attempts, logs::MAX_ATTEMPTS);
    assert!(error.contains("410"), "the failure must be named: {error}");

    let by_repo = ledger.log_counts_by_repo();
    assert_eq!(by_repo["fixture-org/alpha"].failed, 1);
    assert_eq!(by_repo["fixture-org/beta"].done, 12);
}

#[test]
fn capture_off_and_log_excluded_repos_capture_no_logs_but_still_capture_records() {
    let dir = TempDir::new().unwrap();
    let report = run_cycle(&ctx(dir.path()), &FixtureApi::new()).unwrap();
    assert_eq!(report.summary.jobs_emitted, 24);
    assert_eq!(report.summary.logs_captured, 0);
    assert!(reconstruct_logs(dir.path()).is_empty());

    // Log-only exclusion: records and metrics stay unconditional (the
    // ci-observability policy makes logs the ONLY excludable signal).
    let dir = TempDir::new().unwrap();
    let excluded = CycleContext {
        log_excluded_repos: vec!["alpha".into()],
        ..ctx_with_logs(dir.path())
    };
    let report = run_cycle(&excluded, &FixtureApi::new()).unwrap();
    assert_eq!(report.summary.jobs_emitted, 24, "records are never excludable");
    let by_job = reconstruct_logs(dir.path());
    assert!(by_job.keys().all(|job_id| *job_id >= 20000), "alpha logs must be skipped");
    assert_eq!(by_job.len(), 12);
}

#[test]
fn compaction_preserves_outstanding_log_capture_state() {
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    api.fail_log_times(&logs::logs_path("fixture-org/alpha", 10011), 1);
    run_cycle(&ctx_with_logs(dir.path()), &api).unwrap();

    let path = state_dir(dir.path()).join("seen.jsonl");
    let mut ledger = Ledger::open(path.clone()).unwrap();
    let before_pending: Vec<u64> = ledger.pending_logs().iter().map(|t| t.job_id).collect();
    let before_counts = ledger.log_counts();
    assert!(ledger.compact_if_large(0).unwrap(), "compaction must have run");

    let compacted = Ledger::open_read_only(path).unwrap();
    assert_eq!(
        compacted
            .pending_logs()
            .iter()
            .map(|t| t.job_id)
            .collect::<Vec<_>>(),
        before_pending,
        "compaction dropped a wanted-but-uncaptured job log"
    );
    assert_eq!(compacted.log_counts(), before_counts);
    // The give-up counter survives too, or a compaction would restart an
    // unbounded retry loop against a log GitHub has already expired.
    assert_eq!(compacted.last_log_failure().map(|f| f.2), Some(1));
}

#[test]
fn a_ledger_written_before_phase_2_still_loads() {
    // A pre-#8825 `unit` line has no `logs` field at all; it must load as a
    // record unit, not as a log unit, or every job's records would re-emit.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("seen.jsonl");
    let legacy =
        r#"{"type":"unit","seq":1,"repo":"o/r","run_id":7,"job_id":71,"attempt":1,"envelopes":[]}"#;
    std::fs::write(&path, format!("{legacy}\n")).unwrap();
    let ledger = Ledger::open_read_only(path).unwrap();
    assert!(ledger.is_seen(&UnitKey::job("o/r", 7, 71, 1)));
    assert!(!ledger.is_seen(&UnitKey::job_logs("o/r", 7, 71, 1)));
    assert_eq!(ledger.log_counts(), ledger::LogCounts::default());
}
