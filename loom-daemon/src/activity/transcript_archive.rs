//! Roll raw Claude Code transcripts into a verified, incremental `.tar.zst`
//! archive before Claude Code's `cleanupPeriodDays` fuse deletes them
//! (issue #8494, split from #8477's item 5 — deliberately left unbuilt
//! there).
//!
//! # Why this exists
//!
//! #8477 defaulted transcript *token* ingestion on, so the derived
//! `resource_usage`/`cost_by_*` data survives Claude Code's 30-day transcript
//! retention. It does not address the *raw* transcripts, which are still
//! deleted at `cleanupPeriodDays` (default 30). Raw transcripts are needed
//! for forensics, `claude --resume` on an older session, and re-ingestion
//! under a corrected method (pricing tables and dedupe logic do get fixed
//! after the fact — see `transcript_ingest`'s own module doc). The only
//! lever before this was raising `cleanupPeriodDays`, which costs disk 1:1
//! with the extra window (~33GB/30 days, measured on a fleet host). zstd
//! compression on that same set measured 10.9:1 (25.4GB -> 2.33GB), so a
//! rolling compressed archive is roughly an order of magnitude cheaper than
//! raising retention for the same forensic value.
//!
//! # What a run does
//!
//! Each run (1) lists every transcript [`super::transcript_ingest::collect_transcripts`]
//! finds, (2) drops any not yet old enough (`min_age_hours` — a still-growing,
//! actively-written transcript is left for a later run rather than
//! snapshotted mid-write) or already recorded in the `transcript_archive`
//! ledger with a matching size/mtime, (3) tars and zstd-compresses the rest
//! into one dated `.tar.zst` under `archive_dir`, hashing each file with
//! SHA-256 as it is added, (4) writes a sibling `.manifest.json` (path, size,
//! mtime, sha256 per entry), (5) **reads the archive back** and re-hashes
//! every entry against the manifest — an archive that is not read back is
//! not a backup — and (6) only on a clean verification upserts the ledger.
//! A verification failure `bail!`s before the ledger is touched, so a
//! corrupt/partial archive is never recorded as done and the same files are
//! retried on the next run.
//!
//! # `memory/` is never touched
//!
//! `~/.claude/projects/<project>/memory/` holds persistent agent memory, not
//! session transcripts (`defaults/docs/transcript-token-ingest.md`,
//! `.loom/docs/troubleshooting.md`) — no transcript tooling may ever read or
//! archive it. This module never lists it in the first place:
//! [`super::transcript_ingest::collect_transcripts`] only considers `.jsonl`
//! files directly inside a project directory (a session transcript) plus a
//! session's own `<uuid>/subagents/*.jsonl`, so a sibling `memory/` directory
//! is structurally never visited. Because that is a property of *another*
//! module, [`is_memory_path`] re-asserts it locally as a second layer — any
//! candidate whose projects-relative path has a `memory` component is dropped
//! before it is even counted. `memory_directory_is_never_archived` and
//! `an_explicit_memory_path_is_refused_even_if_it_reaches_the_filter` below
//! assert both layers with tests, not merely a comment (the issue's own AC2).
//!
//! # Opt-in, operator-driven
//!
//! Unlike transcript token ingestion (#8477, on by default — an unset host
//! was silently losing data), this never starts on its own: it consumes real
//! disk, and the derived data it backstops (`resource_usage`) is already
//! preserved by #8477. An operator runs `loom-daemon archive-transcripts` by
//! hand or on their own cron/systemd timer.
//!
//! # Restoring
//!
//! See "Restoring from an archive" in `defaults/docs/transcript-token-ingest.md`.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::db::ActivityDb;
use super::transcript_ingest::{collect_transcripts, ledger_key};
use crate::transcript_tokens::claude_projects_dir;

/// Mirrors [`super::transcript_ingest::MAX_TRANSCRIPT_BYTES`]: a pathological
/// transcript must not be read into memory wholesale by a maintenance pass.
pub const MAX_TRANSCRIPT_BYTES: u64 = crate::transcript_tokens::MAX_TRANSCRIPT_BYTES;

/// Default zstd compression level. 19 (of a max 22) is high but not "ultra" —
/// chosen to land close to the 10.9:1 ratio this module's doc measured, on a
/// job that runs occasionally (operator-driven), not on a hot path.
pub const DEFAULT_ZSTD_LEVEL: i32 = 19;

/// `~/.loom/transcript-archives`, or `./transcript-archives` when no home
/// directory resolves.
#[must_use]
pub fn default_archive_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".loom")
        .join("transcript-archives")
}

/// What to archive.
#[derive(Debug, Clone)]
pub struct ArchiveOptions {
    /// `${CLAUDE_CONFIG_DIR:-$HOME/.claude}/projects`.
    pub projects_dir: PathBuf,
    /// Restrict to one workspace's project directory (all of them when `None`).
    pub workspace: Option<PathBuf>,
    /// Where the `.tar.zst` + `.manifest.json` pair is written.
    pub archive_dir: PathBuf,
    /// Skip transcripts modified more recently than this many hours ago.
    pub min_age_hours: i64,
    /// Re-archive transcripts the ledger says are unchanged.
    pub force: bool,
    /// Report what would be archived, write nothing.
    pub dry_run: bool,
    pub max_transcript_bytes: u64,
    pub zstd_level: i32,
}

impl Default for ArchiveOptions {
    fn default() -> Self {
        Self {
            projects_dir: claude_projects_dir().unwrap_or_else(|| PathBuf::from("projects")),
            workspace: None,
            archive_dir: default_archive_dir(),
            min_age_hours: 24,
            force: false,
            dry_run: false,
            max_transcript_bytes: MAX_TRANSCRIPT_BYTES,
            zstd_level: DEFAULT_ZSTD_LEVEL,
        }
    }
}

/// What one archive pass did.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct ArchiveStats {
    pub transcripts_seen: usize,
    pub skipped_too_recent: usize,
    pub skipped_already_archived: usize,
    pub skipped_oversize: usize,
    pub archived: usize,
    pub bytes_raw: u64,
    pub bytes_compressed: u64,
    pub archive_path: Option<String>,
    pub manifest_path: Option<String>,
}

/// One archived file's manifest record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ManifestEntry {
    /// Path relative to `projects_dir` — the same key form the ledger and
    /// `transcript_ingest` both use.
    pub path: String,
    pub size: u64,
    pub mtime: i64,
    pub sha256: String,
}

/// The manifest written alongside an archive's `.tar.zst`, so the archive can
/// be audited or selectively restored from without unpacking it whole.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    pub created_at: String,
    pub archive_file: String,
    pub projects_dir: String,
    pub entries: Vec<ManifestEntry>,
}

struct Candidate {
    path: PathBuf,
    key: String,
    size: u64,
    mtime: i64,
}

/// True for any projects-relative path with a `memory` component —
/// `~/.claude/projects/<project>/memory/`
/// holds persistent agent memory, not session transcripts, and no transcript
/// tooling may read or archive it.
///
/// [`collect_transcripts`] already cannot reach such a path (it lists only the
/// `.jsonl` files directly inside a project directory plus a session's own
/// `<uuid>/subagents/*.jsonl`), so this is a **second, local** guarantee rather
/// than the only one: it keeps the exclusion true in this module even if that
/// upstream walk is ever widened, instead of depending on a property proven
/// somewhere else. `memory_directory_is_never_archived` and
/// `an_explicit_memory_path_is_refused_even_if_it_reaches_the_filter` below
/// assert both layers.
fn is_memory_path(path: &Path) -> bool {
    path.components()
        .any(|c| c.as_os_str().eq_ignore_ascii_case("memory"))
}

/// Files eligible for this run: old enough, not oversize, never under
/// `memory/`, and (unless `force`) not already in the ledger with a matching
/// size/mtime.
fn collect_candidates(
    db: &ActivityDb,
    opts: &ArchiveOptions,
    stats: &mut ArchiveStats,
) -> Result<Vec<Candidate>> {
    let now = Utc::now();
    let cutoff = opts.min_age_hours.checked_mul(3600).unwrap_or(i64::MAX);

    let mut out = Vec::new();
    for path in collect_transcripts(&opts.projects_dir, opts.workspace.as_deref()) {
        let key = ledger_key(&opts.projects_dir, &path);

        // Defence in depth: `collect_transcripts` structurally cannot yield a
        // `memory/` path, so this should never fire -- but the exclusion is a
        // hard constraint of this tool, not an inherited side effect, so it is
        // also enforced here, before the file is even counted as a transcript.
        if is_memory_path(Path::new(&key)) {
            log::warn!(
                "transcript-archive: refusing to archive a memory/ path: {}",
                path.display()
            );
            continue;
        }

        stats.transcripts_seen += 1;

        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        let size = meta.len();
        let modified: DateTime<Utc> = meta.modified().map_or_else(|_| now, Into::into);
        let mtime = modified.timestamp();

        if (now - modified).num_seconds() < cutoff {
            stats.skipped_too_recent += 1;
            continue;
        }

        if size > opts.max_transcript_bytes {
            log::warn!(
                "transcript-archive: skipping oversized transcript {} ({size} bytes)",
                path.display()
            );
            stats.skipped_oversize += 1;
            continue;
        }

        if !opts.force {
            if let Some(entry) = lookup_ledger(db, &key)? {
                let size_i64 = i64::try_from(size).unwrap_or(i64::MAX);
                if entry.file_size == size_i64 && entry.file_mtime == mtime {
                    stats.skipped_already_archived += 1;
                    continue;
                }
            }
        }

        out.push(Candidate {
            path,
            key,
            size,
            mtime,
        });
    }
    // Deterministic ordering: reproducible archives, easier to diff/audit.
    out.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(out)
}

/// SHA-256 of a file's full contents, streamed rather than read wholesale.
fn sha256_file(path: &Path) -> Result<String> {
    let mut file =
        BufReader::new(File::open(path).with_context(|| format!("opening {}", path.display()))?);
    let mut hasher = Sha256::new();
    let mut buf = [0_u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// SHA-256 of an in-memory buffer.
fn sha256_bytes(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

/// Write `candidates` into a new `.tar.zst` + `.manifest.json` pair under
/// `opts.archive_dir`, returning the manifest and the archive's compressed
/// size in bytes.
fn write_archive(
    opts: &ArchiveOptions,
    candidates: &[Candidate],
) -> Result<(Manifest, PathBuf, PathBuf, u64)> {
    std::fs::create_dir_all(&opts.archive_dir)
        .with_context(|| format!("creating archive directory {}", opts.archive_dir.display()))?;

    let stamp = Utc::now().format("%Y%m%dT%H%M%SZ");
    let archive_path = opts
        .archive_dir
        .join(format!("transcripts-{stamp}.tar.zst"));
    let manifest_path = opts
        .archive_dir
        .join(format!("transcripts-{stamp}.manifest.json"));

    let file = File::create(&archive_path)
        .with_context(|| format!("creating archive file {}", archive_path.display()))?;
    let encoder = zstd::stream::write::Encoder::new(file, opts.zstd_level)
        .context("initializing zstd encoder")?;
    let mut tar_builder = tar::Builder::new(encoder);

    let mut entries = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let sha256 = sha256_file(&candidate.path)?;
        tar_builder
            .append_path_with_name(&candidate.path, &candidate.key)
            .with_context(|| format!("adding {} to archive", candidate.path.display()))?;
        entries.push(ManifestEntry {
            path: candidate.key.clone(),
            size: candidate.size,
            mtime: candidate.mtime,
            sha256,
        });
    }

    let encoder = tar_builder.into_inner().context("finalizing tar stream")?;
    encoder.finish().context("finalizing zstd stream")?;

    let compressed_bytes = std::fs::metadata(&archive_path)
        .map(|m| m.len())
        .unwrap_or(0);

    let manifest = Manifest {
        created_at: Utc::now().to_rfc3339(),
        archive_file: archive_path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
        projects_dir: opts.projects_dir.to_string_lossy().into_owned(),
        entries,
    };
    let manifest_json = serde_json::to_string_pretty(&manifest).context("serializing manifest")?;
    std::fs::write(&manifest_path, manifest_json)
        .with_context(|| format!("writing manifest {}", manifest_path.display()))?;

    Ok((manifest, archive_path, manifest_path, compressed_bytes))
}

/// Read `archive_path` back and confirm every manifest entry's bytes hash to
/// the checksum recorded for it, and that no entry is missing. An archive
/// that is not read back is not a backup.
///
/// # Errors
///
/// Returns an error (never panics) on any mismatch: entry count, a missing
/// path, or a checksum that does not match what was written.
fn verify_archive(archive_path: &Path, manifest: &Manifest) -> Result<()> {
    let file = File::open(archive_path)
        .with_context(|| format!("re-opening {} to verify", archive_path.display()))?;
    let decoder = zstd::stream::read::Decoder::new(file).context("initializing zstd decoder")?;
    let mut archive = tar::Archive::new(decoder);

    let mut seen: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for entry in archive.entries().context("reading archive entries")? {
        let mut entry = entry.context("reading one archive entry")?;
        let path = entry
            .path()
            .context("reading entry path")?
            .to_string_lossy()
            .into_owned();
        let mut buf = Vec::new();
        entry
            .read_to_end(&mut buf)
            .context("reading entry contents")?;
        seen.insert(path, sha256_bytes(&buf));
    }

    if seen.len() != manifest.entries.len() {
        bail!(
            "archive verification failed: manifest lists {} entries, archive contains {}",
            manifest.entries.len(),
            seen.len()
        );
    }
    for expected in &manifest.entries {
        match seen.get(&expected.path) {
            None => bail!("archive verification failed: {} missing from archive", expected.path),
            Some(actual) if *actual != expected.sha256 => bail!(
                "archive verification failed: {} checksum mismatch (expected {}, got {actual})",
                expected.path,
                expected.sha256
            ),
            Some(_) => {}
        }
    }
    Ok(())
}

/// Archive every eligible transcript in one pass, returning what it did.
///
/// # Errors
///
/// Propagates SQLite, filesystem, and archive-verification failures. A
/// verification failure leaves the ledger untouched, so the same files are
/// retried on the next run rather than being silently marked done.
pub fn archive(db: &ActivityDb, opts: &ArchiveOptions) -> Result<ArchiveStats> {
    let _ = db.conn.busy_timeout(Duration::from_secs(10));

    let mut stats = ArchiveStats::default();
    let candidates = collect_candidates(db, opts, &mut stats)?;

    if candidates.is_empty() {
        // No-op run, not an error: a quiet host or one that just ran has
        // nothing new to archive.
        return Ok(stats);
    }

    stats.bytes_raw = candidates.iter().map(|c| c.size).sum();
    stats.archived = candidates.len();

    if opts.dry_run {
        return Ok(stats);
    }

    let (manifest, archive_path, manifest_path, compressed_bytes) =
        write_archive(opts, &candidates)?;
    verify_archive(&archive_path, &manifest)?;

    let archive_file = manifest.archive_file.clone();
    let tx = db.conn.unchecked_transaction()?;
    for candidate in &candidates {
        let entry = manifest
            .entries
            .iter()
            .find(|e| e.path == candidate.key)
            .expect("every candidate has a manifest entry");
        upsert_ledger(
            &tx,
            &candidate.key,
            i64::try_from(candidate.size).unwrap_or(i64::MAX),
            candidate.mtime,
            &entry.sha256,
            &archive_file,
        )?;
    }
    tx.commit()?;

    stats.bytes_compressed = compressed_bytes;
    stats.archive_path = Some(archive_path.to_string_lossy().into_owned());
    stats.manifest_path = Some(manifest_path.to_string_lossy().into_owned());
    Ok(stats)
}

#[derive(Debug, Clone, Copy)]
struct LedgerEntry {
    file_size: i64,
    file_mtime: i64,
}

fn lookup_ledger(db: &ActivityDb, key: &str) -> Result<Option<LedgerEntry>> {
    db.conn
        .query_row(
            "SELECT file_size, file_mtime FROM transcript_archive WHERE transcript_path = ?1",
            params![key],
            |row| {
                Ok(LedgerEntry {
                    file_size: row.get(0)?,
                    file_mtime: row.get(1)?,
                })
            },
        )
        .optional()
        .context("reading transcript_archive ledger")
}

fn upsert_ledger(
    conn: &rusqlite::Connection,
    key: &str,
    file_size: i64,
    file_mtime: i64,
    sha256: &str,
    archive_file: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO transcript_archive (\
            transcript_path, file_size, file_mtime, sha256, archive_file, archived_at\
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
         ON CONFLICT(transcript_path) DO UPDATE SET \
            file_size = excluded.file_size, \
            file_mtime = excluded.file_mtime, \
            sha256 = excluded.sha256, \
            archive_file = excluded.archive_file, \
            archived_at = excluded.archived_at",
        params![
            key,
            file_size,
            file_mtime,
            sha256,
            archive_file,
            Utc::now().to_rfc3339()
        ],
    )?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::transcript_tokens::project_slug;

    const WORKSPACE: &str = "/home/ubuntu/GitHub/loom";

    fn assistant_line(id: &str, ts: &str) -> String {
        serde_json::json!({
            "type": "assistant",
            "timestamp": ts,
            "sessionId": "uuid-a",
            "cwd": WORKSPACE,
            "gitBranch": "main",
            "message": {
                "model": "claude-sonnet-5",
                "id": id,
                "usage": {"input_tokens": 10, "output_tokens": 20},
            },
        })
        .to_string()
    }

    fn open_db(dir: &Path) -> ActivityDb {
        ActivityDb::new(dir.join("activity.db")).unwrap()
    }

    fn opts(home: &Path) -> ArchiveOptions {
        ArchiveOptions {
            projects_dir: home.join("projects"),
            archive_dir: home.join("archives"),
            min_age_hours: 0,
            // Fast in tests: correctness of the pipeline does not depend on
            // the compression level, only production disk economics does.
            zstd_level: 1,
            ..ArchiveOptions::default()
        }
    }

    fn seed_transcript(projects: &Path, uuid: &str, lines: &[String]) -> PathBuf {
        let dir = projects.join(project_slug(Path::new(WORKSPACE)));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{uuid}.jsonl"));
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        path
    }

    fn count(db: &ActivityDb, sql: &str) -> i64 {
        db.conn.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    #[test]
    fn archiving_produces_a_verified_tar_zst_and_manifest() {
        let home = tempfile::tempdir().unwrap();
        seed_transcript(
            &home.path().join("projects"),
            "uuid-a",
            &[assistant_line("msg_1", "2026-09-18T04:00:00Z")],
        );

        let db = open_db(home.path());
        let stats = archive(&db, &opts(home.path())).unwrap();

        assert_eq!(stats.transcripts_seen, 1);
        assert_eq!(stats.archived, 1);
        assert!(stats.bytes_raw > 0);
        assert!(stats.bytes_compressed > 0);
        let archive_path = PathBuf::from(stats.archive_path.clone().unwrap());
        assert!(archive_path.exists(), "the .tar.zst was written");
        let manifest_path = PathBuf::from(stats.manifest_path.unwrap());
        let manifest: Manifest =
            serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
        assert_eq!(manifest.entries.len(), 1);
        assert!(manifest.entries[0].path.ends_with("uuid-a.jsonl"));
        assert_eq!(manifest.entries[0].sha256.len(), 64, "a real sha256 hex digest");

        // The archive really does contain the file, and it really does
        // decompress+extract back to the same bytes the manifest recorded.
        verify_archive(&archive_path, &manifest)
            .expect("re-verifying a freshly written archive succeeds");

        assert_eq!(count(&db, "SELECT COUNT(*) FROM transcript_archive"), 1);
    }

    #[test]
    fn memory_directory_is_never_archived() {
        let home = tempfile::tempdir().unwrap();
        let projects = home.path().join("projects");
        let project_dir = projects.join(project_slug(Path::new(WORKSPACE)));
        seed_transcript(&projects, "uuid-a", &[assistant_line("msg_1", "2026-09-18T04:00:00Z")]);

        // A memory/ directory sitting right next to the real transcript,
        // holding a file that would look exactly like a transcript if this
        // module ever recursed into it.
        let memory_dir = project_dir.join("memory");
        std::fs::create_dir_all(&memory_dir).unwrap();
        std::fs::write(memory_dir.join("MEMORY.md"), "persistent agent memory, not a transcript")
            .unwrap();
        std::fs::write(
            memory_dir.join("feedback_should_never_be_archived.jsonl"),
            assistant_line("msg_memory", "2026-09-18T04:00:00Z") + "\n",
        )
        .unwrap();

        let db = open_db(home.path());
        let stats = archive(&db, &opts(home.path())).unwrap();

        // Only the real transcript was seen and archived -- the memory/
        // directory's files (even the one carrying a .jsonl extension) never
        // entered the candidate set at all.
        assert_eq!(stats.transcripts_seen, 1, "memory/ must not be counted as a transcript");
        assert_eq!(stats.archived, 1);

        let manifest_path = PathBuf::from(stats.manifest_path.unwrap());
        let manifest: Manifest =
            serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
        assert!(
            manifest.entries.iter().all(|e| !e.path.contains("memory")),
            "manifest must not reference memory/: {:?}",
            manifest.entries
        );

        assert_eq!(
            db.conn
                .query_row(
                    "SELECT COUNT(*) FROM transcript_archive WHERE transcript_path LIKE '%memory%'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0,
            "the ledger must not record any memory/ path either"
        );
    }

    #[test]
    fn an_explicit_memory_path_is_refused_even_if_it_reaches_the_filter() {
        // The second layer, tested directly: were `collect_transcripts` ever
        // widened to walk a project directory recursively, this is what still
        // keeps `memory/` out of the archive.
        assert!(is_memory_path(Path::new("-home-ubuntu-GitHub-loom/memory/MEMORY.md")));
        assert!(is_memory_path(Path::new("-home-ubuntu-GitHub-loom/memory/feedback_x.jsonl")));
        assert!(is_memory_path(Path::new("memory/uuid-a.jsonl")));
        // ...without over-matching a transcript that merely mentions the word.
        assert!(!is_memory_path(Path::new("-home-ubuntu-GitHub-loom/uuid-a.jsonl")));
        assert!(!is_memory_path(Path::new("-home-ubuntu-memory-bank/uuid-a.jsonl")));
        assert!(!is_memory_path(Path::new(
            "-home-ubuntu-GitHub-loom/uuid-a/subagents/memory-agent.jsonl"
        )));
    }

    #[test]
    fn rerunning_does_not_rearchive_already_archived_transcripts() {
        let home = tempfile::tempdir().unwrap();
        seed_transcript(
            &home.path().join("projects"),
            "uuid-a",
            &[assistant_line("msg_1", "2026-09-18T04:00:00Z")],
        );

        let db = open_db(home.path());
        let first = archive(&db, &opts(home.path())).unwrap();
        assert_eq!(first.archived, 1);

        let second = archive(&db, &opts(home.path())).unwrap();
        assert_eq!(second.archived, 0, "nothing new to archive");
        assert_eq!(second.skipped_already_archived, 1);
        assert!(second.archive_path.is_none(), "a no-op run writes no archive file");
        assert_eq!(count(&db, "SELECT COUNT(*) FROM transcript_archive"), 1, "not duplicated");

        // --force re-archives it anyway, still without erroring or duplicating
        // the ledger row (upsert, not insert).
        let forced = archive(
            &db,
            &ArchiveOptions {
                force: true,
                ..opts(home.path())
            },
        )
        .unwrap();
        assert_eq!(forced.archived, 1);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM transcript_archive"), 1);
    }

    #[test]
    fn a_dry_run_reports_without_writing_anything() {
        let home = tempfile::tempdir().unwrap();
        seed_transcript(
            &home.path().join("projects"),
            "uuid-a",
            &[assistant_line("msg_1", "2026-09-18T04:00:00Z")],
        );

        let db = open_db(home.path());
        let stats = archive(
            &db,
            &ArchiveOptions {
                dry_run: true,
                ..opts(home.path())
            },
        )
        .unwrap();

        assert_eq!(stats.archived, 1, "it reports what it would archive");
        assert!(stats.archive_path.is_none());
        assert_eq!(count(&db, "SELECT COUNT(*) FROM transcript_archive"), 0);
        assert!(
            std::fs::read_dir(home.path().join("archives")).is_err(),
            "dry run creates no archive directory"
        );
    }

    #[test]
    fn a_transcript_modified_too_recently_is_left_for_a_later_run() {
        let home = tempfile::tempdir().unwrap();
        seed_transcript(
            &home.path().join("projects"),
            "uuid-a",
            &[assistant_line("msg_1", "2026-09-18T04:00:00Z")],
        );

        let db = open_db(home.path());
        let stats = archive(
            &db,
            &ArchiveOptions {
                min_age_hours: 24 * 365,
                ..opts(home.path())
            },
        )
        .unwrap();

        assert_eq!(stats.skipped_too_recent, 1);
        assert_eq!(stats.archived, 0);
        assert!(stats.archive_path.is_none(), "a no-op run writes no archive file");
    }

    #[test]
    fn an_empty_host_produces_a_noop_run_not_an_error() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("projects")).unwrap();

        let db = open_db(home.path());
        let stats = archive(&db, &opts(home.path())).unwrap();

        assert_eq!(stats.transcripts_seen, 0);
        assert_eq!(stats.archived, 0);
        assert!(stats.archive_path.is_none());
    }

    #[test]
    fn a_tampered_archive_fails_verification_without_touching_the_ledger() {
        let home = tempfile::tempdir().unwrap();
        seed_transcript(
            &home.path().join("projects"),
            "uuid-a",
            &[assistant_line("msg_1", "2026-09-18T04:00:00Z")],
        );

        let manifest = Manifest {
            created_at: Utc::now().to_rfc3339(),
            archive_file: "does-not-matter.tar.zst".to_string(),
            projects_dir: "irrelevant".to_string(),
            entries: vec![ManifestEntry {
                path: "uuid-a.jsonl".to_string(),
                size: 1,
                mtime: 0,
                sha256: "0".repeat(64),
            }],
        };

        // A corrupted archive file (not a real tar.zst at all) must fail
        // verification loudly rather than being silently accepted.
        let bogus = home.path().join("bogus.tar.zst");
        std::fs::write(&bogus, b"not actually a tar.zst archive").unwrap();
        assert!(verify_archive(&bogus, &manifest).is_err());
    }
}
