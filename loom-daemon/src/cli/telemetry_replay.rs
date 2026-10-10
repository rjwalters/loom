//! `loom-daemon telemetry-replay --as-of <t>` — fleet state at instant `t`,
//! as a daemon running at `t` could have known it (#10196 R6, #11127).
//!
//! Runs R3's committed replay SQL (queries 1 and 3 of
//! `defaults/observability/signoz/replay-queries.sql`) against the telemetry
//! store's ClickHouse, or reads an export of their output (`--from-file`),
//! and prints every emitting host's coverage — `covered`, or `unknown` with
//! the reason — and every reconstructed item. The logic lives in
//! [`loom_daemon::telemetry_replay`]; this is the thin CLI.
//!
//! `--check` (#11128) runs queries 6 and 7 instead: each covered host's view
//! against the webhook-derived label state over sample instants. Exit 0: every
//! covered host agrees within `--threshold`; 1: one disagreed for longer; 2:
//! the store or export could not be read.
//!
//! Endpoint resolution: flags, then `telemetry.signoz.*`, then (deprecated,
//! one release) `autonomous.eta.fleetRefresh.signoz.*`. The password is read
//! from an owner-only `--credential-file` at call time and never printed.

use std::path::PathBuf;

use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, Utc};

use loom_daemon::signoz_read::{
    ClickhouseHttp, EndpointConfig, ENDPOINT_CONFIG_KEY, LEGACY_ENDPOINT_CONFIG_KEY,
};
use loom_daemon::telemetry_replay::check::{
    self, CheckParams, DEFAULT_SPAN_SEC, DEFAULT_THRESHOLD_SEC, EXIT_UNREADABLE,
};
use loom_daemon::telemetry_replay::{
    self, ReplayParams, DEFAULT_STEP_SEC, DEFAULT_WINDOW_SEC, REPLAY_QUERIES,
};

/// `--check` without `--as-of` ends its instants this long before now, so
/// the webhook export and the hosts' last pass have landed.
const CHECK_LAG_SEC: i64 = 600;

#[derive(clap::Args)]
pub(crate) struct ReplayArgs {
    /// The replay instant (RFC 3339). Only records knowable before it count.
    /// With `--check`, the newest sample instant (default: 10 minutes ago).
    #[arg(long, value_name = "RFC3339", required_unless_present_any = ["print_sql", "check"])]
    pub as_of: Option<String>,

    /// Compare each covered host's view with the webhook-derived label state
    /// at the instants `--as-of`, minus every `--step-sec` back to
    /// `--span-sec`. Exit 1 when a covered host disagrees with the forge for
    /// longer than `--threshold`; 2 when the store cannot be read.
    #[arg(long)]
    pub check: bool,

    /// With `--check`: seconds a covered host may disagree with the forge.
    #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_THRESHOLD_SEC, requires = "check")]
    pub threshold: u32,

    /// With `--check`: seconds before `--as-of` to sample back to (86400 for
    /// the daily report).
    #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_SPAN_SEC, requires = "check")]
    pub span_sec: u32,

    /// With `--check`: seconds between sample instants.
    #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_STEP_SEC, requires = "check",
          value_parser = clap::value_parser!(u32).range(1..))]
    pub step_sec: u32,

    /// Lookback for each host's base anchor and its chain, in seconds.
    #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_WINDOW_SEC)]
    pub window_sec: u32,

    /// Scope to one repository (`owner/name`). Default: every repository.
    #[arg(long, value_name = "OWNER/NAME", default_value = "")]
    pub repo: String,

    /// Read a `JSONEachRow` export of the two queries' output (see
    /// `--print-sql`) instead of a live endpoint.
    #[arg(long, value_name = "PATH", conflicts_with = "endpoint")]
    pub from_file: Option<PathBuf>,

    /// ClickHouse HTTP endpoint of the telemetry store. Defaults to
    /// `telemetry.signoz.endpoint`.
    #[arg(long, value_name = "URL")]
    pub endpoint: Option<String>,

    /// ClickHouse user. Defaults to `telemetry.signoz.user`.
    #[arg(long, value_name = "USER")]
    pub user: Option<String>,

    /// Owner-only file holding the user's password. Defaults to
    /// `telemetry.signoz.credentialFile`.
    #[arg(long, value_name = "PATH")]
    pub credential_file: Option<PathBuf>,

    /// Directory whose Loom configuration to read. Defaults to the current
    /// directory.
    #[arg(long, value_name = "PATH")]
    pub repo_root: Option<PathBuf>,

    /// Print the replay as JSON.
    #[arg(long)]
    pub json: bool,

    /// Print the state and coverage queries (for `clickhouse-client
    /// --param_t=… --param_window=… --param_repo=… --format JSONEachRow`) and
    /// exit.
    #[arg(long)]
    pub print_sql: bool,
}

impl ReplayArgs {
    pub(crate) fn run(self) -> Result<()> {
        if self.check {
            return self.run_check();
        }
        if self.print_sql {
            let (state, coverage) = telemetry_replay::state_and_coverage_queries(REPLAY_QUERIES)
                .map_err(|e| anyhow!(e))?;
            println!("-- state at t (replay-queries.sql query 1)\n{state};\n");
            println!("-- coverage at t (replay-queries.sql query 3)\n{coverage};");
            return Ok(());
        }
        let raw = self.as_of.as_deref().unwrap_or_default();
        let as_of = DateTime::parse_from_rfc3339(raw)
            .map(|dt| dt.with_timezone(&Utc))
            .map_err(|e| anyhow!("invalid --as-of {raw:?}: {e}"))?;
        let params = ReplayParams {
            as_of,
            window_sec: self.window_sec,
            repo: self.repo.trim().to_string(),
        };
        let replay = match &self.from_file {
            Some(path) => {
                let export = std::fs::read_to_string(path)
                    .map_err(|e| anyhow!("could not read {}: {e}", path.display()))?;
                telemetry_replay::assemble_export(&params, &export).map_err(|e| anyhow!(e))?
            }
            None => telemetry_replay::fetch(&self.http()?, &params).map_err(|e| anyhow!(e))?,
        };
        if self.json {
            println!("{}", serde_json::to_string_pretty(&replay)?);
        } else {
            print!("{}", telemetry_replay::render(&replay));
        }
        Ok(())
    }

    fn run_check(self) -> Result<()> {
        if self.print_sql {
            let (runs, report) =
                check::check_and_report_queries(REPLAY_QUERIES).map_err(|e| anyhow!(e))?;
            println!("-- disagreements (replay-queries.sql query 6)\n{runs};\n");
            println!("-- agreement report per host (replay-queries.sql query 7)\n{report};");
            return Ok(());
        }
        let as_of = match self.as_of.as_deref() {
            Some(raw) => DateTime::parse_from_rfc3339(raw)
                .map(|dt| dt.with_timezone(&Utc))
                .map_err(|e| anyhow!("invalid --as-of {raw:?}: {e}"))?,
            None => Utc::now() - chrono::Duration::seconds(CHECK_LAG_SEC),
        };
        let params = CheckParams {
            replay: ReplayParams {
                as_of,
                window_sec: self.window_sec,
                repo: self.repo.trim().to_string(),
            },
            span_sec: self.span_sec,
            step_sec: self.step_sec,
            threshold_sec: self.threshold,
        };
        let result = match &self.from_file {
            Some(path) => std::fs::read_to_string(path)
                .map_err(|e| format!("could not read {}: {e}", path.display()))
                .and_then(|export| check::assemble_export(&params, &export)),
            None => self
                .http()
                .map_err(|e| e.to_string())
                .and_then(|http| check::fetch(&http, &params)),
        };
        let checked = match result {
            Ok(checked) => checked,
            Err(why) => {
                eprintln!("[telemetry-replay] --check could not read the store: {why}");
                std::process::exit(EXIT_UNREADABLE);
            }
        };
        if self.json {
            println!("{}", serde_json::to_string_pretty(&checked)?);
        } else {
            print!("{}", check::render(&checked));
        }
        std::process::exit(checked.exit_code());
    }

    fn http(&self) -> Result<ClickhouseHttp> {
        let root = self
            .repo_root
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let configured = EndpointConfig::read(&root);
        let endpoint = self.endpoint.clone().or(configured.endpoint);
        let user = self.user.clone().or(configured.user);
        let credential_file = self.credential_file.clone().or(configured.credential_file);
        let Some(endpoint) = endpoint else {
            bail!("no SigNoz source: pass --from-file or --endpoint, or configure {ENDPOINT_CONFIG_KEY}.endpoint");
        };
        // One warning, naming only the legacy fields a flag did not override.
        let used: Vec<&str> = configured
            .legacy
            .iter()
            .copied()
            .filter(|field| match *field {
                "endpoint" => self.endpoint.is_none(),
                "user" => self.user.is_none(),
                _ => self.credential_file.is_none(),
            })
            .collect();
        if !used.is_empty() {
            eprintln!(
                "[telemetry-replay] deprecated: {} read from {LEGACY_ENDPOINT_CONFIG_KEY}; \
                 move it to {ENDPOINT_CONFIG_KEY} (the old key is a deprecated fallback and will be removed, #11098)",
                used.join(", ")
            );
        }
        Ok(ClickhouseHttp {
            endpoint,
            user,
            credential_file,
            timeout: std::time::Duration::from_secs(120),
        })
    }
}
