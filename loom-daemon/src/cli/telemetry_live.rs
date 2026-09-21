//! Explicit paid smoke runs. Acceptance is independent of the worker's claims.
mod helpers;
mod workspace;

use anyhow::{ensure, Context, Result};
use std::{path::PathBuf, time::Duration};

#[derive(clap::Args)]
pub(crate) struct LiveArgs {
    /// Explicitly allow exactly two paid harness attempts. Omit for a no-I/O plan.
    #[arg(long)]
    execute: bool,
    /// New private directory for isolated repositories and sanitized evidence.
    #[arg(long)]
    output: PathBuf,
    /// OTLP/HTTP Collector base URL, used only by the final explicit export.
    #[arg(long)]
    endpoint: String,
    /// Collector ingress key file, separate from the provider credential.
    #[arg(long)]
    key_file: PathBuf,
    /// Existing installed Loom guard hooks (normally defaults/hooks).
    #[arg(long)]
    guard_dir: PathBuf,
    /// Literal ZAI_API_KEY assignment source; never evaluated as shell.
    #[arg(long)]
    zshrc: PathBuf,
    /// Per-harness wall deadline, including preflight; not a token or cost cap.
    #[arg(long, default_value_t = 180, value_parser = clap::value_parser!(u64).range(1..=180))]
    timeout_seconds: u64,
}

impl LiveArgs {
    pub(crate) fn run(self) -> Result<()> {
        if !self.execute {
            println!(
                "{}",
                serde_json::json!({"executed":false,"attempts":2,"runtimes":["pi","opencode"],"profile":"zai-flash","effort":"low","wall_seconds_per_attempt":self.timeout_seconds,"token_cap":null,"billing":"not-measured","task":"isolated read-only curator smoke; not an issue lifecycle"})
            );
            return Ok(());
        }
        ensure!(cfg!(feature = "otlp"), "live canary requires an OTLP-enabled binary");
        ensure!(
            loom_daemon::observability::endpoint_policy::valid_otlp_endpoint(&self.endpoint),
            "invalid Collector endpoint"
        );
        ensure!(
            loom_daemon::observability::endpoint_policy::reserved_placeholder_host(&self.endpoint)
                .is_none(),
            "placeholder Collector endpoint refused"
        );
        ensure!(
            !std::env::vars_os()
                .any(|(k, _)| k.to_string_lossy().starts_with("LOOM_OBSERVABILITY_")),
            "remove ambient LOOM_OBSERVABILITY overrides before this isolated canary"
        );
        workspace::validate_guards(&self.guard_dir)?;
        let guards = self.guard_dir.canonicalize()?;
        let key = helpers::key_from_zshrc(&self.zshrc)?;
        ensure!(
            !std::fs::read_to_string(&self.key_file)
                .context("cannot read Collector key file")?
                .trim()
                .is_empty(),
            "Collector key file is empty"
        );
        let key_file = self.key_file.canonicalize()?;
        let binary = std::env::current_exe()?;
        // Atomic no-clobber reservation, private before anything can write inside.
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&self.output)
            .context("canary output must be a new directory")?;
        let output = self.output.canonicalize()?;
        let mut reports = Vec::new();
        let mut envelopes = Vec::new();
        for runtime in ["pi", "opencode"] {
            let workspace = workspace::Workspace::create(&output, runtime, &self.endpoint)?;
            let (report, records) = self.attempt(&workspace, &binary, &guards, runtime, &key)?;
            reports.push(report);
            envelopes.extend(records);
        }
        let path = output.join("envelopes.jsonl");
        let mut file = std::fs::File::create(&path)?;
        use std::io::Write;
        for envelope in &envelopes {
            serde_json::to_writer(&mut file, envelope)?;
            file.write_all(b"\n")?;
        }
        file.sync_all()?;
        let mut export = std::process::Command::new(&binary);
        export
            .args(["telemetry-export", "--input"])
            .arg(&path)
            .arg("--endpoint")
            .arg(&self.endpoint)
            .arg("--key-file")
            .arg(key_file);
        let exported =
            loom_daemon::proc_exec::run_bounded(export, Duration::from_secs(60))?.succeeded();
        let passed = reports.iter().all(|r| r["verified"] == true);
        let report = serde_json::json!({"schema":1,"synthetic":false,"task":"isolated read-only curator smoke","full_issue_lifecycle":false,"forge_contact":false,"checkpoint_issue_is_local_fixture":true,"profile":"zai-flash","model":"glm-5.3-flash","effort":"low","attempts":reports,"token_cap":null,"billing":"not-measured","envelopes":envelopes.len(),"collector_acknowledged":exported,"backend_verification":"not_performed","unproven":["backend indexing and UI","full issue acceptance","cost efficiency","provider internal HTTP spans"]});
        std::fs::write(output.join("report.json"), serde_json::to_vec_pretty(&report)?)?;
        println!("{report}");
        ensure!(
            passed && exported,
            "canary failed verification or Collector delivery; sanitized evidence retained"
        );
        Ok(())
    }

    fn attempt(
        &self,
        workspace: &workspace::Workspace,
        binary: &std::path::Path,
        guards: &std::path::Path,
        runtime: &str,
        key: &str,
    ) -> Result<(serde_json::Value, Vec<loom_daemon::telemetry::TelemetryEnvelope>)> {
        use loom_daemon::{
            observability::{lifecycle, queue::DurableQueue},
            proc_exec::{run_bounded, run_bounded_observed, Completion},
            telemetry::trace::SpanName,
        };
        let root = &workspace.root;
        let execution = format!("canary-{}", uuid::Uuid::new_v4());
        let span = lifecycle::begin(
            root,
            &execution,
            SpanName::Sweep,
            lifecycle::attributes(&[
                ("loom.sweep_id", &execution),
                ("loom.runtime", runtime),
                ("loom.timing_source", "owned_boundary"),
            ]),
        )
        .context("canary trace initialization failed before model launch")?;
        let trace_id = span.context().trace_id.as_str().to_owned();
        let mut command = workspace.command(binary, guards, runtime, key)?;
        command.args(["spawn-worker", "--", "--profile", "zai-flash", "--effort", "low", "-p", "/loom:curator Read canary-input.txt using loom_read and follow the exact response contract."]);
        span.command(&mut command);
        let started = std::time::Instant::now();
        let completion =
            run_bounded_observed(command, Duration::from_secs(self.timeout_seconds), |pid| {
                lifecycle::child_spawned(root, &execution, pid)
            });
        let (child_result, verified) = match completion {
            Ok(Completion::Exited(output)) => {
                let result = if output.status.success() {
                    "success"
                } else if output.status.code().is_none() {
                    "signal"
                } else {
                    "failure"
                };
                let verified = output.status.success()
                    && workspace.input_unchanged()
                    && helpers::verify_stream(
                        runtime,
                        &output.stdout,
                        &output.stderr,
                        &workspace.expected,
                    )
                    .is_ok();
                (result, verified)
            }
            Ok(Completion::TimedOut { .. }) => ("timeout", false),
            Err(_) => ("spawn_failed", false),
        };
        if child_result != "spawn_failed" {
            lifecycle::child_exited(root, &execution, child_result);
        }
        let checkpoint = if verified {
            let mut command = workspace.command(binary, guards, runtime, key)?;
            command.args([
                "sweep-checkpoint",
                "write",
                "1",
                "curator-done",
                "--task-id",
                &execution,
                "--attempt",
                "1",
                "--model",
                "glm-5.3-flash",
            ]);
            span.command(&mut command);
            run_bounded(command, Duration::from_secs(10)).is_ok_and(|c| c.succeeded())
        } else {
            false
        };
        let verified = verified && checkpoint;
        lifecycle::finish_execution(
            root,
            &execution,
            if verified { "success" } else { "failure" },
            lifecycle::attributes(&[(
                "loom.result",
                if verified {
                    "success"
                } else {
                    "canary_verification_failed"
                },
            )]),
        );
        let queue = DurableQueue::open(root.join(".loom/logs/canary-queue.jsonl"), 1000);
        lifecycle::backfill(root, &queue);
        ensure!(
            queue.dropped_total() == 0 && !queue.is_empty(),
            "canary trace collection failed; journal retained"
        );
        let records = queue.peek_batch(1000);
        let ids: Vec<_> = records.iter().filter_map(|e| match &e.record {
            loom_daemon::telemetry::TelemetryRecord::Span(s) => Some(serde_json::json!({"trace_id":s.context.trace_id.as_str(),"span_id":s.context.span_id.as_str(),"parent_span_id":s.parent_span_id.as_ref().map(|v|v.as_str()),"name":s.name.as_str(),"status":s.status})), _ => None
        }).collect();
        Ok((
            serde_json::json!({"runtime":runtime,"trace_id":trace_id,"execution_id":execution,"child_result":child_result,"verified":verified,"checkpoint_written":checkpoint,"wall_milliseconds":started.elapsed().as_millis(),"expected_spans":ids}),
            records,
        ))
    }
}
