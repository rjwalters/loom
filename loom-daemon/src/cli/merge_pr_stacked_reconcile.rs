//! `loom-daemon merge-pr stacked-reconcile` (#8191, slice 3 of
//! `docs/migration/merge-pr-orchestrator-port.md`): the whole post-merge
//! stacked-child reconcile pass in one invocation.
//!
//! stdin is the pre-merge `[{number, headRefName}]` snapshot (may be empty, in
//! which case the verb queries the forge itself when the parent is stacked).
//! stdout is `LEVEL<TAB>message` records ended by `LOOM-STACKED-RECONCILE DONE`;
//! exit 0 without the terminator means the verb did not run. Best-effort: the
//! merge has already happened. See [`loom_daemon::merge_pr::stacked_reconcile`].

use anyhow::Result;

use crate::merge_pr::stacked_reconcile::{print_records, run, Inputs, SystemHost};

#[derive(clap::Args)]
pub(crate) struct StackedReconcileArgs {
    /// `owner/repo`.
    #[arg(long, value_name = "NWO")]
    repo: String,

    /// The merged parent PR's head branch.
    #[arg(long, value_name = "BRANCH")]
    parent_branch: String,

    /// Directory holding `reconcile-stack.sh`.
    #[arg(long, value_name = "DIR")]
    script_dir: String,

    /// Path to `lib/forge-helpers.sh`.
    #[arg(long, value_name = "PATH")]
    forge_helpers: String,

    /// Remediation text appended to skip warnings.
    #[arg(long, value_name = "TEXT", default_value = "")]
    roll_hint: String,
}

impl StackedReconcileArgs {
    pub(crate) fn run(self) -> Result<()> {
        let mut snapshot = String::new();
        if std::io::Read::read_to_string(&mut std::io::stdin(), &mut snapshot).is_err() {
            eprintln!("merge-pr stacked-reconcile: could not read the snapshot from stdin");
            std::process::exit(2);
        }
        let inputs = Inputs {
            repo: self.repo,
            parent_branch: self.parent_branch,
            snapshot: Some(snapshot).filter(|s| !s.trim().is_empty()),
            script_dir: self.script_dir,
            roll_hint: self.roll_hint,
        };
        let host = SystemHost {
            forge_helpers: self.forge_helpers,
        };
        print_records(&run(&inputs, &host));
        Ok(())
    }
}
