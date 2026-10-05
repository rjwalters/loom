//! `loom-daemon forge-probe …` — the hosted-qualification probe runner
//! (Issue #9789, phase 1 of epic #9769).
//!
//! Executes the #9777 probe manifest against a live forge and prints the
//! sanitized receipt as JSON: per-case outcome, server version, actor
//! login, run timestamp, expected/observed. Unlike `forge-inventory`,
//! whose verbs make no forge call, this command's whole purpose IS forge
//! calls — every call bounded by a timeout, never retried, read-only
//! unless `--live-write` is passed.
//!
//! | Exit | Meaning |
//! |---|---|
//! | 0 | every required row executed and passed |
//! | 1 | the qualification verdict is not clean — a required row is
//!     unknown, failed, or unsupported. Slice 1 runs on the full matrix,
//!     so this is the expected exit until the slice-2 matrix lands. |
//! | 2 | could not run: missing config, missing credential, or a
//!     transport fault on the version probe |
//!
//! Credentials come from the hosted-trial runbook's environment
//! (`GITEA_QUAL_WRITER_TOKEN`, `GITEA_QUAL_READONLY_TOKEN`) — never argv,
//! never echoed (#5982, the credential-storage policy). Receipt rows
//! carry logins and version strings only.

use std::time::Duration;

use anyhow::Result;

use loom_daemon::forge_probe::{run, verdict, LiveHttp, RunnerConfig};

#[derive(clap::Args)]
pub(crate) struct ForgeProbeArgs {
    /// Base URL of the instance under qualification
    /// (e.g. `https://gitea.example.com`). Defaults to
    /// `$GITEA_QUAL_INSTANCE_URL`.
    #[arg(long, value_name = "URL")]
    origin: Option<String>,
    /// The disposable repo write cases create resources in, as
    /// `<org>/<repo>` — created by the hosted-trial runbook's setup,
    /// never by the runner. Defaults to `$GITEA_QUAL_ORG/loomp-test`.
    #[arg(long, value_name = "ORG/REPO")]
    repo: Option<String>,
    /// Run namespace stamped into every disposable title
    /// (`loomp-<ns>: …`). Defaults to `$GITEA_QUAL_RUN_NS`, else
    /// `loomp-<unix-seconds>` so repeated runs never collide.
    #[arg(long, value_name = "NS")]
    run_ns: Option<String>,
    /// Execute write cases. Without this, write rows refuse and stay
    /// `unknown` (#9789: read-only by default).
    #[arg(long)]
    live_write: bool,
    /// Only run rows whose test_id contains this substring (repeatable;
    /// empty = every manifest row).
    #[arg(long = "case", value_name = "SUBSTR")]
    case: Vec<String>,
    /// Per-call HTTP timeout in seconds. Calls are bounded and never
    /// retried — a probe records, it does not fight.
    /// Must be at least 1: curl treats `--max-time 0` as no limit.
    #[arg(
        long,
        value_name = "SECS",
        default_value_t = 30,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    timeout: u64,
}

impl ForgeProbeArgs {
    pub(crate) fn run(self) -> Result<()> {
        // "Could not run" is exit 2 per the forge-inventory family contract:
        // config/credential gaps print to stderr and exit directly, never
        // through main's error path (which is exit 1).
        let missing = |name: &str| -> ! {
            eprintln!(
                "forge-probe: {name} did not resolve — the runbook's step-8 names must resolve first"
            );
            std::process::exit(2);
        };
        let origin = match self.origin {
            Some(o) => o,
            None => match std::env::var("GITEA_QUAL_INSTANCE_URL") {
                Ok(v) => v,
                Err(_) => missing("no --origin and no $GITEA_QUAL_INSTANCE_URL"),
            },
        };
        let writer_token = match std::env::var("GITEA_QUAL_WRITER_TOKEN") {
            Ok(v) => v,
            Err(_) => missing("no $GITEA_QUAL_WRITER_TOKEN in the environment"),
        };
        let repo = match self.repo {
            Some(r) => r,
            None => match std::env::var("GITEA_QUAL_ORG") {
                Ok(org) => format!("{org}/loomp-test"),
                Err(_) => missing("no --repo and no $GITEA_QUAL_ORG"),
            },
        };
        let run_ns = match self.run_ns {
            Some(ns) => ns,
            None => std::env::var("GITEA_QUAL_RUN_NS").unwrap_or_else(|_| {
                let secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                format!("loomp-{secs}")
            }),
        };
        let cfg = RunnerConfig {
            origin,
            repo,
            writer_token,
            readonly_token: std::env::var("GITEA_QUAL_READONLY_TOKEN").ok(),
            run_ns,
            live_write: self.live_write,
            timeout: Duration::from_secs(self.timeout),
            only: self.case,
        };
        let http = LiveHttp {
            origin: cfg.origin.clone(),
            timeout: cfg.timeout,
        };
        let results = match run(&cfg, &http) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("forge-probe: could not run: {e:#}");
                std::process::exit(2);
            }
        };
        println!("{}", serde_json::to_string_pretty(&results)?);
        let (ok, unknown, failed, unsupported) = verdict(&results);
        eprintln!(
            "forge-probe: verdict {} — {} case(s) run, {unknown} unknown, {failed} failed, \
             {unsupported} unsupported (required rows only)",
            if ok { "GO" } else { "NO-GO" },
            results.len()
        );
        if ok {
            Ok(())
        } else {
            std::process::exit(1);
        }
    }
}
