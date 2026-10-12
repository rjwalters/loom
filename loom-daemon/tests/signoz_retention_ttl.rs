//! Live proof for the SigNoz trial's retention artifact (Issue #8528 scope
//! item 6 / Issue #8826): `signoz/retention.sql`, executed verbatim against
//! the same pinned ClickHouse the trial's telemetry store runs — including its
//! `ON CLUSTER cluster` clause, which needs a real distributed-DDL queue and so
//! cannot run in `clickhouse local` at all (`QUERY_IS_PROHIBITED`: "Replicated
//! DDL queries are disabled").
//!
//! This was the last artifact in the trial with **no** guard of any kind —
//! static or executed. Every other committed file is tied to the emitted
//! schema by `signoz_trial_artifacts.rs` and, since #9705/#9775/#9833/#9857/
//! #9892/#9905, run against the pinned engine. `retention.sql` had neither.
//! What it *did* have was a live application: `evidence.md` records it re-run
//! against the trial on 2026-09-25 with "all 16 `ON CLUSTER` statements
//! reported status 0", and the resulting effective DDL captured per table.
//!
//! **Status 0 and an effective TTL are not a retention outcome.** They say the
//! DDL parsed and was stored; they do not say a row past the window is deleted,
//! that a row inside it survives, or that the 7/30 split separates anything.
//! The gap is the same class as section 5's chunk join (#9905): a saved
//! artifact whose output was observed, in a shape that cannot distinguish
//! working from broken. Here the stakes are higher than a query's, because this
//! artifact *mutates* an operator's deployment, irrecoverably.
//!
//! Executing it found five things the committed header did not say, each
//! corrected in the same change that added this test:
//!
//! 1. **`MODIFY TTL` materializes on existing parts immediately.** Applying
//!    this file to a trial that already holds over-age data deletes that data
//!    at once, not at some later merge. The header read as a policy change.
//! 2. **The `/ 1000` is not cosmetic, and dropping it deletes *everything*.**
//!    `toDateTime(1790871234567)` does not throw and does not produce a far
//!    future — it **saturates** at `2106-02-07 06:28:15`, the DateTime maximum,
//!    and `+ INTERVAL 7 DAY` then **overflows past it**. Every row in the table
//!    is instantly expired. A units slip in any of the 12 millisecond-keyed
//!    statements wipes the table and still reports status 0.
//! 3. **"RESTORE 30 days" restores the policy, not the data.** A trial that ran
//!    the earlier all-seven-day version of this file had its 8-to-30-day-old
//!    metrics deleted at that moment. Re-widening the TTL cannot bring them
//!    back, and the header's "RESTORE" invited the opposite reading.
//! 4. **`ttl_only_drop_parts` decides whether the window is honoured at all.**
//!    With it set, a part holding one over-age row and one in-window row keeps
//!    **both** — an over-age row survives indefinitely until an unrelated merge
//!    rewrites that part. The live DDL capture recorded TTL expressions but not
//!    this setting, so the trial's actual cleanup behaviour is not established
//!    by what is in `evidence.md`; the README's check query now reads it.
//! 5. **One failing statement aborts the rest of the file.** The 10 metric
//!    statements are last, so a table a later pin renamed leaves exactly the
//!    statements whose job is to *restore* 30 days unapplied — while the six
//!    that *shorten* logs/traces to 7 days have already run.
//!
//! Properties below are each a documented claim of the artifact rather than an
//! invented one, and every one has the mutation of the **committed SQL** that
//! breaks it run as a counterfactual rather than described.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

/// Same pin as the SigNoz trial's telemetry store (`signoz/casting.yaml`) and
/// as `signoz_usage_queries.rs` / `signoz_cycle_time.rs` /
/// `signoz_queue_quota_queries.rs` /
/// `signoz_ci_failed_run_logs.rs`, so no proof in this repo can drift from the
/// deployment on ClickHouse version.
const CLICKHOUSE_IMAGE: &str = "clickhouse/clickhouse-server:25.12.5@sha256:cacf32d6884291dc2ff5e0156a97f46fc53ff7c929a7906d114e268a929dfd3a";

/// The committed artifact under proof.
const RETENTION: &str = include_str!("../../defaults/observability/signoz/retention.sql");

/// The rendered ClickHouse configuration of the trial's own telemetry store.
/// The cluster name every statement in [`RETENTION`] targets is read out of
/// this file's `remote_servers` block rather than restated here — a re-render
/// that renames the cluster has to fail a test, not be discovered by an
/// operator pasting a file that aborts on its first statement.
const RENDERED_CLICKHOUSE_CONFIG: &str = include_str!(
    "../../defaults/observability/signoz/pours/deployment/telemetrystore/clickhouse/config-0-0.yaml"
);

/// Every other committed query artifact of this trial. They are the authority
/// for which tables the **application API** owns: each one reads the active
/// signal tables (`signoz_index_v3`, `logs_v2`, `samples_v4`,
/// `time_series_v4`), and `retention.sql`'s own header says the API owns
/// exactly those — this file is for the auxiliary/legacy tables the API leaves
/// behind. Naming an active table here would fight the API's own setting, so
/// [`the_artifact_is_sixteen_modify_ttl_statements_and_nothing_else`] asserts
/// the two sets stay disjoint.
const SIBLING_ARTIFACTS: &[(&str, &str)] = &[
    ("queries.sql", include_str!("../../defaults/observability/signoz/queries.sql")),
    (
        "fixture-queries.sql",
        include_str!("../../defaults/observability/signoz/fixture-queries.sql"),
    ),
    (
        "ci-queries.sql",
        include_str!("../../defaults/observability/signoz/ci-queries.sql"),
    ),
    (
        "usage-queries.sql",
        include_str!("../../defaults/observability/signoz/usage-queries.sql"),
    ),
    (
        "cycle-time-extract.sql",
        include_str!("../../defaults/observability/signoz/cycle-time-extract.sql"),
    ),
    (
        "queue-dwell.sql",
        include_str!("../../defaults/observability/signoz/queue-dwell.sql"),
    ),
    (
        "quota-utilization.sql",
        include_str!("../../defaults/observability/signoz/quota-utilization.sql"),
    ),
    (
        "alerts/queue-starvation.json",
        include_str!("../../defaults/observability/signoz/alerts/queue-starvation.json"),
    ),
];

/// Monotonic suffix so two tests in one process never collide on a container
/// name or a scratch directory.
static NEXT_ID: AtomicU32 = AtomicU32::new(0);

/// One result row: column name -> JSON value, as `JSONEachRow` renders it.
type Row = BTreeMap<String, serde_json::Value>;

// ---------------------------------------------------------------------------
// The committed file, parsed
// ---------------------------------------------------------------------------

/// How a table's time column is encoded.
///
/// **Derived from the column's name, not from the TTL expression.** That
/// distinction is load-bearing and was found by mutation-testing an earlier
/// draft of this file: a version that read the encoding out of the expression
/// silently *adapted* to the mutation that drops `/ 1000` — reclassifying the
/// column as a `DateTime64`, typing the fixture to match, and passing. The
/// pinned schema's names declare the unit (`unix_milli`, `timestamp_ms`,
/// `last_reported_unix_milli`), so they are an authority independent of the
/// expression under proof, and
/// [`the_artifact_is_sixteen_modify_ttl_statements_and_nothing_else`] checks
/// the expression against them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimeEncoding {
    /// A `*_milli` / `*_ms` column: integer milliseconds since the epoch, which
    /// the TTL expression must divide by 1000.
    Millis,
    /// `toDateTime(<col>)` — a `DateTime64` wall clock.
    DateTime64,
    /// `<col>` used directly — a `DateTime`.
    DateTime,
}

/// Does this column's **name** declare milliseconds? The pinned SigNoz schema
/// names every millisecond column `…_milli` or `…_ms`.
fn name_declares_milliseconds(column: &str) -> bool {
    column.contains("milli") || column.ends_with("_ms")
}

/// One `ALTER TABLE … ON CLUSTER … MODIFY TTL …` statement of the artifact.
#[derive(Debug, Clone)]
struct Retention {
    /// Verbatim committed text of this statement (no trailing `;`).
    text: String,
    database: String,
    table: String,
    cluster: String,
    time_column: String,
    encoding: TimeEncoding,
    days: u32,
}

impl Retention {
    fn qualified(&self) -> String {
        format!("{}.{}", self.database, self.table)
    }

    /// `CREATE TABLE` for a minimal stand-in carrying exactly the time column
    /// this statement keys its TTL on, typed as the statement's own expression
    /// requires, plus an upstream 15-day TTL in the shape the pinned schema
    /// writes (`toIntervalSecond`). `k` labels the row so a survivor set reads
    /// as names rather than counts.
    fn create(&self, settings: &str) -> String {
        let (column_type, upstream) = match self.encoding {
            TimeEncoding::Millis => ("Int64", format!("toDateTime({} / 1000)", self.time_column)),
            TimeEncoding::DateTime64 => {
                ("DateTime64(9)", format!("toDateTime({})", self.time_column))
            }
            TimeEncoding::DateTime => ("DateTime", self.time_column.clone()),
        };
        format!(
            "CREATE TABLE {} ({} {}, k String) ENGINE = MergeTree ORDER BY k \
             TTL {} + toIntervalSecond(1296000){};",
            self.qualified(),
            self.time_column,
            column_type,
            upstream,
            settings,
        )
    }

    /// The same stand-in with **no** TTL at all.
    ///
    /// Needed wherever a test inserts a row that is already older than the
    /// upstream window: ClickHouse evaluates TTL as it writes the part, so a
    /// 27-day-old row inserted into a table still carrying the upstream 15-day
    /// TTL is dropped by the *insert*, before the committed statement has run.
    /// Starting from no TTL keeps every deletion below attributable to the
    /// committed expression.
    fn create_without_ttl(&self) -> String {
        let column_type = match self.encoding {
            TimeEncoding::Millis => "Int64",
            TimeEncoding::DateTime64 => "DateTime64(9)",
            TimeEncoding::DateTime => "DateTime",
        };
        format!(
            "CREATE TABLE {} ({} {}, k String) ENGINE = MergeTree ORDER BY k;",
            self.qualified(),
            self.time_column,
            column_type,
        )
    }

    /// A row labelled `label` whose time column sits `age_days` in the past.
    fn row(&self, label: &str, age_days: u32) -> String {
        let value = match self.encoding {
            TimeEncoding::Millis => {
                format!("toUnixTimestamp(now() - INTERVAL {age_days} DAY) * 1000")
            }
            TimeEncoding::DateTime64 => format!("now64(9) - INTERVAL {age_days} DAY"),
            TimeEncoding::DateTime => format!("now() - INTERVAL {age_days} DAY"),
        };
        format!(
            "INSERT INTO {} ({}, k) SELECT {}, '{}';",
            self.qualified(),
            self.time_column,
            value,
            label
        )
    }

    /// The committed statement with its `ON CLUSTER <name>` clause removed.
    ///
    /// This is the file's **one** transformation for the `clickhouse local`
    /// tests, and it is a transformation of the *distribution* mechanism only:
    /// `ON CLUSTER` decides which hosts receive a DDL, never what the DDL
    /// means, and the trial's cluster is a single shard with a single replica
    /// (`remote_servers` in the rendered config). The strict verbatim proof —
    /// the whole file, bytes as committed, clause included — is
    /// [`committed_retention_applies_verbatim_under_the_rendered_cluster_name`],
    /// which runs against a real distributed-DDL queue.
    fn without_cluster(&self) -> String {
        self.text
            .replace(&format!(" ON CLUSTER {}", self.cluster), "")
    }

    /// The same statement retargeted at a differently-named table in the same
    /// database, for a test that needs several variants of one table side by
    /// side. The committed text is rewritten too, so `without_cluster()` still
    /// names the table this variant created.
    fn renamed(&self, table: &str) -> Self {
        let text = self
            .text
            .replace(&self.qualified(), &format!("{}.{table}", self.database));
        Self {
            table: table.to_owned(),
            text,
            ..self.clone()
        }
    }
}

/// The committed file's statements. Line comments are stripped **before** the
/// split on `;`, mirroring `signoz_usage_queries.rs`:
/// the file's own prose uses semicolons and a naive split would cut a statement
/// in half mid-sentence. No `;` or `--` occurs inside a string literal in this
/// artifact.
fn statements(sql: &str) -> Vec<String> {
    let stripped: String = sql
        .lines()
        .map(|line| match line.find("--") {
            Some(at) => &line[..at],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n");
    stripped
        .split(';')
        .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|s| !s.is_empty())
        .collect()
}

/// Parses the committed artifact. Panics on any statement that is not an
/// `ALTER TABLE <db>.<table> ON CLUSTER <cluster> MODIFY TTL <expr> + INTERVAL
/// <n> DAY` — an artifact an operator pastes into a live deployment must not
/// be able to grow a `DROP`, a `TRUNCATE` or a `DELETE` unnoticed, and
/// [`the_artifact_is_sixteen_modify_ttl_statements_and_nothing_else`] is the
/// assertion that says so in ordinary CI.
fn parse(sql: &str) -> Vec<Retention> {
    let shape = regex::Regex::new(
        r"^ALTER TABLE (?<db>\w+)\.(?<table>\w+) ON CLUSTER (?<cluster>\w+) MODIFY TTL (?<expr>.+ \+ INTERVAL (?<days>\d+) DAY)$",
    )
    .unwrap();
    // The three time-expression shapes the artifact uses. `column` captures the
    // identifier in each, so the NAME (not the shape) decides the encoding.
    let shapes = [
        regex::Regex::new(r"^toDateTime\((?<col>\w+) / 1000\)$").unwrap(),
        regex::Regex::new(r"^toDateTime\((?<col>\w+)\)$").unwrap(),
        regex::Regex::new(r"^(?<col>\w+)$").unwrap(),
    ];

    statements(sql)
        .into_iter()
        .map(|text| {
            let caps = shape
                .captures(&text)
                .unwrap_or_else(|| panic!("not a MODIFY TTL statement: {text}"));
            let expr = caps.name("expr").unwrap().as_str();
            let time = expr
                .rsplit_once(" + INTERVAL ")
                .unwrap_or_else(|| panic!("no interval in {expr}"))
                .0;
            let time_column = shapes
                .iter()
                .find_map(|re| re.captures(time).map(|c| c["col"].to_owned()))
                .unwrap_or_else(|| panic!("unrecognised time expression: {time}"));
            let encoding = if name_declares_milliseconds(&time_column) {
                TimeEncoding::Millis
            } else if time.starts_with("toDateTime(") {
                TimeEncoding::DateTime64
            } else {
                TimeEncoding::DateTime
            };
            Retention {
                database: caps["db"].to_owned(),
                table: caps["table"].to_owned(),
                cluster: caps["cluster"].to_owned(),
                days: caps["days"].parse().unwrap(),
                time_column,
                encoding,
                text,
            }
        })
        .collect()
}

/// Every `signoz_<db>.<table>` the other committed artifacts read, with the
/// `distributed_` prefix folded away so a `Distributed` read and its local
/// counterpart name the same table. Returns table -> the artifact that reads it.
fn api_owned_signal_tables() -> BTreeMap<String, &'static str> {
    let reference = regex::Regex::new(r"signoz_(?<db>logs|traces|metrics)\.(?<table>\w+)").unwrap();
    let mut found = BTreeMap::new();
    for (artifact, body) in SIBLING_ARTIFACTS {
        for c in reference.captures_iter(body) {
            let table = c["table"]
                .strip_prefix("distributed_")
                .unwrap_or(&c["table"]);
            found
                .entry(format!("signoz_{}.{}", &c["db"], table))
                .or_insert(*artifact);
        }
    }
    found
}

/// The cluster name the trial's own render defines, read from the single key
/// under `remote_servers:` in the rendered ClickHouse configuration.
fn rendered_cluster_name() -> String {
    let mut lines = RENDERED_CLICKHOUSE_CONFIG
        .lines()
        .skip_while(|l| l.trim_end() != "remote_servers:");
    lines.next().expect("rendered config has remote_servers");
    let first = lines.next().expect("remote_servers has a cluster");
    let name = first
        .trim()
        .strip_suffix(':')
        .unwrap_or_else(|| panic!("unexpected remote_servers entry: {first}"));
    assert!(
        !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_'),
        "unexpected cluster name {name:?}"
    );
    name.to_owned()
}

/// The `CREATE DATABASE` statements every database the artifact touches needs.
fn create_databases(plan: &[Retention]) -> String {
    plan.iter()
        .map(|r| r.database.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|db| format!("CREATE DATABASE IF NOT EXISTS {db};"))
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// Engines: `clickhouse local` for TTL semantics, a real server for ON CLUSTER
// ---------------------------------------------------------------------------

/// Runs `script` through `clickhouse local` in the pinned image and returns
/// stdout. Panics with the engine's own stderr on failure — a statement that
/// does not apply must fail this test, not be silently skipped.
fn local(script: &str, format: &str) -> String {
    let mut child = Command::new("docker")
        .args([
            "run",
            "--rm",
            "-i",
            "--network",
            "none",
            "--entrypoint",
            "clickhouse",
            CLICKHOUSE_IMAGE,
            "local",
            "--multiquery",
            &format!("--format={format}"),
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
        "clickhouse rejected the script:\n{}\n--- script ---\n{script}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A single-node pinned ClickHouse with an embedded Keeper, so `ON CLUSTER
/// <name>` DDL has a real distributed-DDL queue to go through. The cluster
/// name, the `distributed_ddl` path and the `macros` are taken from the trial's
/// own rendered configuration; the raft/keeper block is this harness's, since
/// the trial runs Keeper as a separate service it would be pointless to
/// reproduce here. `--network none` leaves only loopback, so nothing is
/// published and two of these can never collide.
struct Server {
    name: String,
    scratch: PathBuf,
}

impl Server {
    fn start(cluster: &str) -> Self {
        let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
        let name = format!("loom-signoz-retention-proof-{}-{id}", std::process::id());
        let scratch = std::env::temp_dir().join(&name);
        std::fs::create_dir_all(&scratch).unwrap();
        let ddl_path = rendered_scalar("distributed_ddl", "path");
        let config = format!(
            "keeper_server:\n  \
               tcp_port: 9181\n  \
               server_id: 1\n  \
               log_storage_path: /var/lib/clickhouse/coordination/log\n  \
               snapshot_storage_path: /var/lib/clickhouse/coordination/snapshots\n  \
               raft_configuration:\n    \
                 server:\n      \
                   id: 1\n      \
                   hostname: localhost\n      \
                   port: 9234\n\
             zookeeper:\n  \
               node:\n    \
                 host: localhost\n    \
                 port: 9181\n\
             distributed_ddl:\n  \
               path: {ddl_path}\n\
             macros:\n  \
               shard: \"00\"\n  \
               replica: \"00\"\n\
             mark_cache_size: 134217728\n\
             remote_servers:\n  \
               {cluster}:\n    \
                 shard:\n      \
                   replica:\n        \
                     host: localhost\n        \
                     port: 9000\n"
        );
        std::fs::write(scratch.join("trial.yaml"), config).unwrap();

        let run = Command::new("docker")
            .args([
                "run",
                "-d",
                "--name",
                &name,
                "--network",
                "none",
                "--memory=1g",
                "-v",
                &format!(
                    "{}:/etc/clickhouse-server/config.d/trial.yaml:ro",
                    scratch.join("trial.yaml").display()
                ),
                CLICKHOUSE_IMAGE,
            ])
            .output()
            .expect("docker is required for this test");
        assert!(
            run.status.success(),
            "could not start the pinned ClickHouse: {}",
            String::from_utf8_lossy(&run.stderr)
        );

        let server = Self { name, scratch };
        // Bounded readiness wait: the prototype came up in 4-7s; 90 one-second
        // attempts leaves room for a cold CI runner without ever hanging.
        for attempt in 0..90 {
            if server.try_query("SELECT 1").is_some() {
                return server;
            }
            assert!(attempt < 89, "pinned ClickHouse never became ready");
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        unreachable!("the loop above either returns or asserts")
    }

    fn try_query(&self, sql: &str) -> Option<String> {
        let out = Command::new("docker")
            .args([
                "exec",
                &self.name,
                "clickhouse-client",
                "--format=TSVRaw",
                "-q",
                sql,
            ])
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Runs `script` on the server. Returns `Ok(stdout)` or `Err(stderr)` — the
    /// abort counterfactuals need the engine's own refusal, so a failure here
    /// is data, not a panic.
    fn script(&self, script: &str, format: &str) -> Result<String, String> {
        let mut child = Command::new("docker")
            .args([
                "exec",
                "-i",
                &self.name,
                "clickhouse-client",
                "--multiquery",
                &format!("--format={format}"),
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
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).into_owned())
        }
    }

    fn must(&self, script: &str) -> String {
        self.script(script, "TSVRaw").unwrap_or_else(|e| {
            panic!("server rejected the script:\n{e}\n--- script ---\n{script}")
        })
    }

    /// Effective table-level TTL per `database.table`, read from
    /// `system.tables` exactly as the README's own check query does.
    fn effective_ttls(&self) -> BTreeMap<String, String> {
        let out = self.must(
            "SELECT concat(database, '.', name) AS t, create_table_query \
             FROM system.tables WHERE database LIKE 'signoz_%' \
             AND create_table_query LIKE '%TTL%' AND engine NOT LIKE 'Distributed%' \
             FORMAT JSONEachRow",
        );
        rows(&out)
            .into_iter()
            .map(|r| {
                let table = r["t"].as_str().unwrap().to_owned();
                (table, table_ttl(r["create_table_query"].as_str().unwrap()))
            })
            .collect()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "--force", "--volumes", &self.name])
            .output();
        let _ = std::fs::remove_dir_all(&self.scratch);
    }
}

/// A scalar out of the rendered ClickHouse configuration: the single `key:`
/// under `section:`.
fn rendered_scalar(section: &str, key: &str) -> String {
    let mut lines = RENDERED_CLICKHOUSE_CONFIG
        .lines()
        .skip_while(|l| l.trim_end() != format!("{section}:"));
    lines.next().unwrap_or_else(|| panic!("no {section}:"));
    let line = lines.next().unwrap_or_else(|| panic!("{section} is empty"));
    let (found, value) = line
        .trim()
        .split_once(": ")
        .unwrap_or_else(|| panic!("unexpected {section} entry: {line}"));
    assert_eq!(found, key, "unexpected first key under {section}");
    value.to_owned()
}

/// The table-level TTL clause of a `create_table_query`, normalised. Column
/// TTLs live inside the column list, before `ENGINE =`, and are deliberately
/// excluded — the distinction is the subject of
/// [`modify_ttl_silently_drops_a_qualified_delete_predicate`].
fn table_ttl(create: &str) -> String {
    let after_engine = match create.find(" ENGINE = ") {
        Some(at) => &create[at..],
        None => create,
    };
    let from_ttl = match after_engine.find("TTL ") {
        Some(at) => &after_engine[at..],
        None => return String::new(),
    };
    match from_ttl.find(" SETTINGS ") {
        Some(at) => from_ttl[..at].trim().to_owned(),
        None => from_ttl.trim().to_owned(),
    }
}

fn rows(out: &str) -> Vec<Row> {
    out.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// `groupArray(k)` of a table, as a sorted set of labels.
fn survivors(out: &str) -> BTreeSet<String> {
    out.split(['[', ']', ',', '\'', '"'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

// ---------------------------------------------------------------------------
// Ordinary CI: the artifact's shape
// ---------------------------------------------------------------------------

/// An artifact whose documented use is "paste it into the live trial's
/// `clickhouse-client`" must be nothing but idempotent `MODIFY TTL`. [`parse`]
/// rejects any other statement shape, so this test is what keeps a `DROP`, a
/// `TRUNCATE`, a `DELETE WHERE` or an un-clustered statement from arriving in
/// the file unnoticed — in ordinary CI, with no Docker.
#[test]
fn the_artifact_is_sixteen_modify_ttl_statements_and_nothing_else() {
    let plan = parse(RETENTION);
    assert_eq!(plan.len(), 16, "statement count changed: {plan:#?}");

    let cluster = rendered_cluster_name();
    for r in &plan {
        assert_eq!(
            r.cluster,
            cluster,
            "{} targets cluster {:?} but the render defines {:?} — the file \
             would abort on its first statement",
            r.qualified(),
            r.cluster,
            cluster
        );
        // The unit the column's NAME declares must be the unit the expression
        // converts. A `*_milli` / `*_ms` column read without `/ 1000` does not
        // widen the window — it saturates `toDateTime` at the DateTime maximum
        // and overflows past it, expiring every row in the table while still
        // reporting status 0 (`dropping_the_millisecond_divisor_…` runs it).
        let required = format!("MODIFY TTL {} + INTERVAL {} DAY", ttl_expression(r), r.days);
        assert!(
            r.text.contains(&required),
            "{} keys its TTL on {:?}, which {} milliseconds, so the statement \
             must read `{required}`; it reads:\n{}",
            r.qualified(),
            r.time_column,
            if r.encoding == TimeEncoding::Millis {
                "declares"
            } else {
                "does not declare"
            },
            r.text
        );
    }

    // The 7/30 split the README and `../../docs/ci-observability.md` state.
    let by_days: BTreeMap<u32, Vec<String>> = plan.iter().fold(BTreeMap::new(), |mut acc, r| {
        acc.entry(r.days).or_default().push(r.qualified());
        acc
    });
    assert_eq!(
        by_days.keys().copied().collect::<Vec<_>>(),
        vec![7, 30],
        "only a 7-day logs/traces and a 30-day metrics window are documented"
    );
    assert_eq!(by_days[&7].len(), 6, "7-day tables: {:?}", by_days[&7]);
    assert_eq!(by_days[&30].len(), 10, "30-day tables: {:?}", by_days[&30]);
    for t in &by_days[&7] {
        assert!(
            t.starts_with("signoz_logs.") || t.starts_with("signoz_traces."),
            "{t} is at 7 days but is not a log/trace table"
        );
    }
    for t in &by_days[&30] {
        assert!(t.starts_with("signoz_metrics."), "{t} is at 30 days but is not a metric table");
    }

    // The API owns the active signal tables; this file is for what it leaves
    // behind. A statement naming an active table would fight the retention
    // setting the README tells the operator to make first.
    let api_owned = api_owned_signal_tables();
    assert!(
        api_owned.len() >= 4,
        "the sibling artifacts no longer identify the active signal tables: {api_owned:#?}"
    );
    for r in &plan {
        assert!(
            !api_owned.contains_key(&r.qualified()),
            "{} is an active signal table (read by {}), which the SigNoz \
             retention API owns — see this file's header",
            r.qualified(),
            api_owned[&r.qualified()]
        );
        let databases: BTreeSet<&str> = api_owned
            .keys()
            .map(|t| t.split('.').next().unwrap())
            .collect();
        assert!(
            databases.contains(r.database.as_str()),
            "{} is not one of the signal databases the trial queries ({databases:?})",
            r.qualified()
        );
    }

    // The resource fingerprint tables keep the upstream 30-minute grace beyond
    // the window (README, "Retention and operation"). That holds only because
    // this file does not name them: a `MODIFY TTL` would replace their whole
    // table-level TTL, grace and all — see
    // `modify_ttl_silently_drops_a_qualified_delete_predicate`.
    for r in &plan {
        assert!(
            !r.table.ends_with("_resource"),
            "{} is a resource fingerprint table; a MODIFY TTL here silently \
             drops the 30-minute grace the README promises",
            r.qualified()
        );
    }
}

// ---------------------------------------------------------------------------
// The whole file, verbatim, through a real distributed-DDL queue
// ---------------------------------------------------------------------------

/// The strict verbatim proof: the committed bytes, `ON CLUSTER cluster`
/// included, applied to the pinned engine under the cluster name the trial's
/// own render defines. Every statement must report host status 0 — the exact
/// check `evidence.md` recorded against the live trial on 2026-09-25 — and the
/// resulting effective DDL must be the documented 6x7 / 10x30 split.
///
/// Running it a second time asserts the header's idempotency claim on the axis
/// it actually holds: identical effective DDL, all statuses 0 again.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn committed_retention_applies_verbatim_under_the_rendered_cluster_name() {
    let plan = parse(RETENTION);
    let cluster = rendered_cluster_name();
    let server = Server::start(&cluster);

    let mut setup = create_databases(&plan);
    for r in &plan {
        setup.push('\n');
        setup.push_str(&r.create(""));
    }
    server.must(&setup);

    // Upstream 15 days everywhere, before the artifact runs.
    let before = server.effective_ttls();
    assert_eq!(before.len(), 16, "fixture tables: {before:#?}");
    for (table, ttl) in &before {
        assert!(
            ttl.contains("toIntervalSecond(1296000)"),
            "{table} did not start at the upstream 15 days: {ttl}"
        );
    }

    for pass in 1..=2 {
        // The committed file, byte for byte, comments included.
        let out = server
            .script(RETENTION, "TSV")
            .unwrap_or_else(|e| panic!("pass {pass}: the committed file was rejected:\n{e}"));
        let statuses: Vec<&str> = out
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.split('\t').nth(2).unwrap_or("<no status column>"))
            .collect();
        assert_eq!(
            statuses.len(),
            16,
            "pass {pass}: expected one distributed-DDL host row per statement, got:\n{out}"
        );
        assert!(
            statuses.iter().all(|s| *s == "0"),
            "pass {pass}: not every statement reported status 0:\n{out}"
        );

        let after = server.effective_ttls();
        for r in &plan {
            let ttl = &after[&r.qualified()];
            assert_eq!(
                *ttl,
                format!("TTL {} + toIntervalDay({})", ttl_expression(r), r.days),
                "pass {pass}: unexpected effective TTL for {}",
                r.qualified()
            );
        }
    }
}

/// The time half of a statement's TTL, as ClickHouse echoes it back.
fn ttl_expression(r: &Retention) -> String {
    match r.encoding {
        TimeEncoding::Millis => format!("toDateTime({} / 1000)", r.time_column),
        TimeEncoding::DateTime64 => format!("toDateTime({})", r.time_column),
        TimeEncoding::DateTime => r.time_column.clone(),
    }
}

/// Mutation: rename the cluster, as a re-render could. The engine refuses the
/// first statement (`CLUSTER_DOESNT_EXIST`) and the client stops there, so
/// **nothing** in the file applies — which is the benign direction, and the
/// reason `the_artifact_is_sixteen_modify_ttl_statements_and_nothing_else` ties
/// the name to the render rather than to a constant in this test.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn a_renamed_cluster_aborts_the_file_before_anything_applies() {
    let plan = parse(RETENTION);
    let cluster = rendered_cluster_name();
    let server = Server::start(&cluster);

    let mut setup = create_databases(&plan);
    for r in &plan {
        setup.push('\n');
        setup.push_str(&r.create(""));
    }
    server.must(&setup);

    let renamed =
        RETENTION.replace(&format!("ON CLUSTER {cluster}"), "ON CLUSTER telemetrystore_cluster");
    let err = server
        .script(&renamed, "TSV")
        .expect_err("a renamed cluster must be refused");
    assert!(err.contains("CLUSTER_DOESNT_EXIST"), "unexpected refusal: {err}");

    for (table, ttl) in server.effective_ttls() {
        assert!(
            ttl.contains("toIntervalSecond(1296000)"),
            "{table} was changed despite the abort: {ttl}"
        );
    }
}

/// Mutation: a table a later pin renamed away. The statement reports host
/// status 60 (`UNKNOWN_TABLE`) **and** the client aborts, so every statement
/// after it is skipped. Because the 10 metric statements are last, the
/// surviving effect is exactly the asymmetric one: logs and traces have already
/// been shortened to 7 days, while the statements whose documented job is to
/// *restore* 30 days to metrics never ran — the file's own header promises that
/// restore to any trial that applied the earlier all-seven-day version.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn a_renamed_table_aborts_the_file_and_leaves_the_metric_statements_unapplied() {
    let plan = parse(RETENTION);
    let cluster = rendered_cluster_name();
    let server = Server::start(&cluster);

    let mut setup = create_databases(&plan);
    for r in &plan {
        setup.push('\n');
        setup.push_str(&r.create(""));
    }
    server.must(&setup);

    // The trial this models ran the earlier all-seven-day version, so its
    // metric tables sit at 7 days and are waiting to be restored to 30.
    let metrics: Vec<&Retention> = plan.iter().filter(|r| r.days == 30).collect();
    let mut shorten = String::new();
    for r in &metrics {
        shorten.push_str(&format!(
            "ALTER TABLE {} MODIFY TTL {} + INTERVAL 7 DAY;\n",
            r.qualified(),
            ttl_expression(r)
        ));
    }
    server.must(&shorten);

    // A later pin renamed one of the trace tables out of existence.
    let aborts_at = plan
        .iter()
        .position(|r| r.table == "durationSort")
        .expect("durationSort is one of the committed statements");
    assert!(
        aborts_at > 0 && aborts_at < plan.len() - 1,
        "the abort point must have statements on both sides of it"
    );
    server.must(&format!("DROP TABLE {};", plan[aborts_at].qualified()));

    let err = server
        .script(RETENTION, "TSV")
        .expect_err("a missing table must be refused");
    assert!(err.contains("UNKNOWN_TABLE"), "unexpected refusal: {err}");

    let after = server.effective_ttls();
    for (at, r) in plan.iter().enumerate() {
        if at == aborts_at {
            continue;
        }
        let ttl = &after[&r.qualified()];
        if at < aborts_at {
            assert_eq!(
                *ttl,
                format!("TTL {} + toIntervalDay({})", ttl_expression(r), r.days),
                "statement {at} precedes the abort and should have applied to {}",
                r.qualified()
            );
        } else if r.days == 30 {
            // The whole point: these are the statements that RESTORE 30 days,
            // and every one of them sits after the abort.
            assert_eq!(
                *ttl,
                format!("TTL {} + toIntervalDay(7)", ttl_expression(r)),
                "{} should still be at the shortened 7 days after the abort",
                r.qualified()
            );
        } else {
            assert!(
                ttl.contains("toIntervalSecond(1296000)"),
                "{} follows the abort and should still be at the upstream 15 days: {ttl}",
                r.qualified()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// What status 0 and an effective TTL cannot show: the retention outcome
// ---------------------------------------------------------------------------

/// Per statement: an over-age row is deleted, an in-window row survives, and
/// the **same** 10-day-old row is deleted by every 7-day statement and kept by
/// every 30-day one. That last pair is what makes the split a split rather
/// than two numbers in a file.
///
/// It also establishes the timing the header did not state: `MODIFY TTL`
/// materialises on existing parts, so the deletion happens when the operator
/// pastes the file — not at some later merge.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn the_committed_expressions_delete_over_age_rows_and_keep_in_window_rows() {
    let plan = parse(RETENTION);
    let mut script = create_databases(&plan);
    for r in &plan {
        script.push('\n');
        script.push_str(&r.create_without_ttl());
        script.push('\n');
        script.push_str(&r.row("over", r.days + 3));
        script.push('\n');
        script.push_str(&r.row("in", r.days - 3));
        script.push('\n');
        script.push_str(&r.row("ten", 10));
        script.push('\n');
        script.push_str(&r.without_cluster());
        script.push_str(";\n");
        script.push_str(&format!(
            "SELECT '{}' AS t, groupArray(k) AS survived FROM {} FORMAT JSONEachRow;\n",
            r.qualified(),
            r.qualified()
        ));
    }

    let observed: BTreeMap<String, BTreeSet<String>> = rows(&local(&script, "JSONEachRow"))
        .into_iter()
        .map(|row| {
            let table = row["t"].as_str().unwrap().to_owned();
            (table, survivors(&row["survived"].to_string()))
        })
        .collect();

    for r in &plan {
        let got = &observed[&r.qualified()];
        let expected: BTreeSet<String> = if r.days > 10 {
            ["in", "ten"].iter().map(|s| (*s).to_owned()).collect()
        } else {
            ["in"].iter().map(|s| (*s).to_owned()).collect()
        };
        assert_eq!(
            *got,
            expected,
            "{} at {} days: a {}-day-old row must go, a {}-day-old row must \
             stay, and the 10-day-old row must {}",
            r.qualified(),
            r.days,
            r.days + 3,
            r.days - 3,
            if r.days > 10 { "stay" } else { "go" }
        );
    }
}

/// Mutation: drop the `/ 1000` from a millisecond statement, the single most
/// plausible edit to this file. The result is not "nothing expires" — it is
/// **every row deleted**. `toDateTime` saturates an out-of-range argument at
/// the `DateTime` maximum rather than throwing, and `+ INTERVAL 7 DAY` then
/// overflows past it, so the TTL lands in the past for every row. The statement
/// still reports status 0.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn dropping_the_millisecond_divisor_deletes_every_row_instead_of_none() {
    let plan = parse(RETENTION);
    let r = plan
        .iter()
        .find(|r| r.encoding == TimeEncoding::Millis)
        .expect("the artifact has millisecond-encoded statements");

    let mutated = r.without_cluster().replace(
        &format!("toDateTime({} / 1000)", r.time_column),
        &format!("toDateTime({})", r.time_column),
    );
    assert_ne!(mutated, r.without_cluster(), "the mutation did not apply");

    let script = format!(
        "{}\n{}\n{}\n{}\n{mutated};\n\
         SELECT groupArray(k) AS survived FROM {} FORMAT JSONEachRow;\n\
         SELECT toString(toDateTime(toUnixTimestamp(now()) * 1000)) AS saturated \
         FORMAT JSONEachRow;\n",
        create_databases(&plan),
        r.create_without_ttl(),
        r.row("yesterday", 1),
        r.row("today", 0),
        r.qualified(),
    );
    let out = rows(&local(&script, "JSONEachRow"));

    assert_eq!(
        survivors(&out[0]["survived"].to_string()),
        BTreeSet::new(),
        "the units slip was expected to delete the whole table"
    );
    let saturated = out[1]["saturated"].as_str().unwrap();
    assert_eq!(
        saturated, "2106-02-07 06:28:15",
        "the saturation value this finding rests on changed: {saturated}"
    );

    // The committed statement keeps both rows, in the same engine, same script
    // shape — so the difference is the divisor and nothing else.
    let control = format!(
        "{}\n{}\n{}\n{}\n{};\n\
         SELECT groupArray(k) AS survived FROM {} FORMAT JSONEachRow;\n",
        create_databases(&plan),
        r.create_without_ttl(),
        r.row("yesterday", 1),
        r.row("today", 0),
        r.without_cluster(),
        r.qualified(),
    );
    assert_eq!(
        survivors(&rows(&local(&control, "JSONEachRow"))[0]["survived"].to_string()),
        ["today", "yesterday"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect::<BTreeSet<_>>(),
        "the committed statement must keep both fresh rows"
    );
}

/// The header's "RESTORE 30 days on a trial that ran the earlier
/// all-seven-day version" restores the **policy**. It cannot restore the data:
/// the seven-day version deleted every metric row between 8 and 30 days old at
/// the moment it ran, and re-widening the window afterwards returns nothing.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn restoring_thirty_days_restores_the_policy_not_the_data() {
    let plan = parse(RETENTION);
    let r = plan
        .iter()
        .find(|r| r.days == 30)
        .expect("the artifact has 30-day statements");
    let seven_day_version = r
        .without_cluster()
        .replace("INTERVAL 30 DAY", "INTERVAL 7 DAY");
    assert_ne!(
        seven_day_version,
        r.without_cluster(),
        "the earlier version's edit did not apply"
    );

    // The history this models, in order: a trial at the documented 30-day
    // metric policy, holding a 20-day-old sample; the earlier all-seven-day
    // version of this file is applied; then the current file is applied to
    // restore 30 days.
    let script = format!(
        "{}\n{}\n{};\n{}\n{}\n\
         SELECT 'fresh' AS at, groupArray(k) AS survived FROM {} FORMAT JSONEachRow;\n\
         {seven_day_version};\n\
         SELECT 'after the 7-day version' AS at, groupArray(k) AS survived FROM {} \
         FORMAT JSONEachRow;\n\
         {};\n\
         SELECT 'after restoring 30' AS at, groupArray(k) AS survived FROM {} \
         FORMAT JSONEachRow;\n\
         SELECT 'policy' AS at, extract(create_table_query, 'toIntervalDay\\\\(\\\\d+\\\\)') \
         AS survived FROM system.tables WHERE concat(database, '.', name) = '{}' \
         FORMAT JSONEachRow;\n",
        create_databases(&plan),
        r.create_without_ttl(),
        r.without_cluster(),
        r.row("twenty_days", 20),
        r.row("two_days", 2),
        r.qualified(),
        r.qualified(),
        r.without_cluster(),
        r.qualified(),
        r.qualified(),
    );
    let observed: BTreeMap<String, String> = rows(&local(&script, "JSONEachRow"))
        .into_iter()
        .map(|row| (row["at"].as_str().unwrap().to_owned(), row["survived"].to_string()))
        .collect();

    assert_eq!(
        survivors(&observed["fresh"]),
        ["twenty_days", "two_days"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect::<BTreeSet<_>>()
    );
    assert_eq!(
        survivors(&observed["after the 7-day version"]),
        ["two_days"].iter().map(|s| (*s).to_owned()).collect(),
        "the earlier all-seven-day version was expected to delete the 20-day row"
    );
    assert_eq!(
        survivors(&observed["after restoring 30"]),
        ["two_days"].iter().map(|s| (*s).to_owned()).collect(),
        "re-widening the window must not be read as recovering the data"
    );
    assert!(
        observed["policy"].contains("toIntervalDay(30)"),
        "the policy itself must be back at 30 days: {}",
        observed["policy"]
    );
}

/// `MODIFY TTL` **replaces** the table-level TTL, qualification included. A
/// table arriving with a `DELETE WHERE` predicate — or a `GROUP BY` rollup, or
/// the resource tables' 30-minute grace — loses it silently and reports status
/// 0. Column TTLs, by contrast, survive untouched.
///
/// This is why the README's "resource fingerprint tables retain the upstream
/// 30-minute grace" holds only as long as no resource table is named in this
/// file, and why
/// `the_artifact_is_sixteen_modify_ttl_statements_and_nothing_else` asserts
/// that none is.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn modify_ttl_silently_drops_a_qualified_delete_predicate() {
    let plan = parse(RETENTION);
    let r = plan
        .iter()
        .find(|r| r.encoding == TimeEncoding::Millis)
        .expect("the artifact has millisecond-encoded statements");
    let time = ttl_expression(r);

    let script = format!(
        "{}\nCREATE TABLE {} ({} Int64, k String, \
           payload String TTL {time} + INTERVAL 1 DAY) \
         ENGINE = MergeTree ORDER BY k \
         TTL {time} + INTERVAL 1800 SECOND DELETE WHERE k != 'pinned';\n\
         SELECT 'before' AS at, create_table_query AS ddl FROM system.tables \
         WHERE concat(database, '.', name) = '{}' FORMAT JSONEachRow;\n\
         {};\n\
         SELECT 'after' AS at, create_table_query AS ddl FROM system.tables \
         WHERE concat(database, '.', name) = '{}' FORMAT JSONEachRow;\n",
        create_databases(&plan),
        r.qualified(),
        r.time_column,
        r.qualified(),
        r.without_cluster(),
        r.qualified(),
    );
    let observed: BTreeMap<String, String> = rows(&local(&script, "JSONEachRow"))
        .into_iter()
        .map(|row| {
            (row["at"].as_str().unwrap().to_owned(), row["ddl"].as_str().unwrap().to_owned())
        })
        .collect();

    assert!(
        table_ttl(&observed["before"]).contains("WHERE k != 'pinned'"),
        "fixture did not start qualified: {}",
        table_ttl(&observed["before"])
    );
    assert!(
        !table_ttl(&observed["after"]).contains("WHERE"),
        "the qualification was expected to be dropped silently: {}",
        table_ttl(&observed["after"])
    );
    assert_eq!(table_ttl(&observed["after"]), format!("TTL {time} + toIntervalDay({})", r.days));
    // `create_table_query` backtick-quotes identifiers, so match on the column
    // TTL itself rather than on the whole declaration.
    assert!(
        observed["after"].contains(&format!("`payload` String TTL {time} + toIntervalDay(1)")),
        "a column TTL must survive a table-level MODIFY TTL: {}",
        observed["after"]
    );
}

/// The window is only honoured per **part**. With `ttl_only_drop_parts` set, a
/// part holding one over-age row and one in-window row keeps both, so a row
/// well past seven days stays queryable until an unrelated merge rewrites that
/// part. The same two rows in two separate parts expire normally, and the same
/// mixed part expires normally with the setting off — so the setting, not the
/// data and not the committed expression, is what decides.
///
/// The live DDL capture in `evidence.md` recorded TTL expressions only, so this
/// is the axis on which the trial's *cleanup behaviour* is still unestablished;
/// the README's check query now reads the setting alongside the TTL.
#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn ttl_only_drop_parts_keeps_an_over_age_row_in_a_mixed_part() {
    let plan = parse(RETENTION);
    let r = plan
        .iter()
        .find(|r| r.days == 7 && r.encoding == TimeEncoding::Millis)
        .expect("the artifact has 7-day millisecond statements");

    // `mixed` gets both rows in ONE insert (one part); `split` gets one insert
    // each (two parts); `off` is `mixed` without the setting.
    let case = |name: &str, settings: &str, single_part: bool| {
        let table = r.renamed(name);
        let insert = if single_part {
            // One statement, so one part holding both rows.
            format!(
                "INSERT INTO {} ({}, k) VALUES \
                   (toUnixTimestamp(now() - INTERVAL 10 DAY) * 1000, 'ten_days'), \
                   (toUnixTimestamp(now() - INTERVAL 2 DAY) * 1000, 'two_days');",
                table.qualified(),
                table.time_column,
            )
        } else {
            format!("{}\n{}", table.row("ten_days", 10), table.row("two_days", 2))
        };
        format!(
            "{}\n{insert}\n{};\n\
             SELECT '{name}' AS scenario, groupArray(k) AS survived FROM {} \
             FORMAT JSONEachRow;\n",
            table.create(settings),
            table.without_cluster(),
            table.qualified(),
        )
    };

    let script = format!(
        "{}\n{}{}{}",
        create_databases(&plan),
        case("mixed", " SETTINGS ttl_only_drop_parts = 1", true),
        case("split", " SETTINGS ttl_only_drop_parts = 1", false),
        case("off", "", true),
    );
    let observed: BTreeMap<String, BTreeSet<String>> = rows(&local(&script, "JSONEachRow"))
        .into_iter()
        .map(|row| {
            (
                row["scenario"].as_str().unwrap().to_owned(),
                survivors(&row["survived"].to_string()),
            )
        })
        .collect();

    assert_eq!(
        observed["mixed"],
        ["ten_days", "two_days"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect::<BTreeSet<_>>(),
        "ttl_only_drop_parts = 1 was expected to keep the over-age row in a \
         mixed part — this is the gap between the policy and the outcome"
    );
    assert_eq!(
        observed["split"],
        ["two_days"].iter().map(|s| (*s).to_owned()).collect(),
        "two separate parts must expire independently"
    );
    assert_eq!(
        observed["off"],
        ["two_days"].iter().map(|s| (*s).to_owned()).collect(),
        "without the setting the same mixed part must expire the over-age row"
    );
}
