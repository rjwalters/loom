//! Record what Loom's lifecycle instrumentation costs on a representative run.
//!
//! Offline and credential-free: both measured arms execute inside throwaway
//! workspaces, no provider or forge is contacted, and the only file read
//! outside them is this host's own sweep-outcome journal, used read-only as
//! the reference denominator.
use anyhow::{Context, Result};
use loom_daemon::observability::overhead::{self, RunShape};
use std::path::PathBuf;

#[derive(clap::Args)]
pub(crate) struct OverheadArgs {
    /// Repetitions of the representative run per arm; the report is a median.
    #[arg(long, default_value_t = 5)]
    repetitions: usize,
    /// Owned `loom.tool` spans per role attempt. A parameter of the shape
    /// being measured, not a measured fleet average.
    #[arg(long, default_value_t = 4)]
    tools_per_attempt: usize,
    /// Workspace whose recorded sweep outcomes supply the reference duration.
    /// Read-only; defaults to the current directory.
    #[arg(long)]
    reference_workspace: Option<PathBuf>,
    /// Write the report here in addition to stdout.
    #[arg(long)]
    output: Option<PathBuf>,
}

impl OverheadArgs {
    pub(crate) fn run(self) -> Result<()> {
        anyhow::ensure!(
            cfg!(feature = "otlp"),
            "this binary was built without the otlp feature, so nothing is \
             instrumented and there is no overhead to measure"
        );
        let reference = match self.reference_workspace {
            Some(path) => path,
            None => std::env::current_dir().context("resolving the reference workspace")?,
        };
        // Throwaway roots: the measured writes must never land in a real
        // workspace's trace-context directory or outcome journal.
        let instrumented = tempfile::tempdir().context("creating the instrumented workspace")?;
        let baseline = tempfile::tempdir().context("creating the baseline workspace")?;
        write_config(instrumented.path(), true)?;
        write_config(baseline.path(), false)?;
        let report = overhead::measure(
            instrumented.path(),
            baseline.path(),
            &reference,
            &RunShape {
                tools_per_attempt: self.tools_per_attempt,
                ..RunShape::default()
            },
            self.repetitions,
        )?;
        let rendered = serde_json::to_string_pretty(&report)?;
        if let Some(path) = self.output {
            std::fs::write(&path, format!("{rendered}\n"))
                .with_context(|| format!("writing {}", path.display()))?;
        }
        println!("{rendered}");
        Ok(())
    }
}

fn write_config(root: &std::path::Path, traced: bool) -> Result<()> {
    std::fs::create_dir_all(root.join(".loom"))?;
    let config = if traced {
        serde_json::json!({"observability":{"enabled":true,"exporter":"otlp","endpoint":"http://127.0.0.1:4318"}})
    } else {
        serde_json::json!({"observability":{"enabled":false}})
    };
    std::fs::write(root.join(".loom/config.json"), config.to_string())
        .context("writing the measured workspace config")?;
    Ok(())
}
