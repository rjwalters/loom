//! Step 4 of `fleet add-worker`, `forge-auth`: log `gh` in with the PAT (over
//! stdin) and wire git through `gh auth setup-git`.

use super::path_bootstrap;
use crate::fleet::{Plan, Step, StepStdin};

/// Push the `forge-auth` step, or a skip when no `--pat-file` was supplied.
pub(super) fn push_forge_auth(plan: &mut Plan, pat: Option<&String>) {
    match pat {
        Some(pat) => plan.push_step(
            Step::new(
                "forge-auth",
                "authenticate gh with the fine-grained PAT (via stdin) and set up git credential helper",
                Some("gh auth status >/dev/null 2>&1".to_string()),
                render_forge_auth(),
            )
            .with_stdin(StepStdin {
                content: pat.clone(),
                secret: true,
            }),
        ),
        None => plan.push_skip(
            "forge-auth",
            "authenticate gh with the fine-grained PAT",
            "no --pat-file supplied",
        ),
    }
}

fn render_forge_auth() -> String {
    // The PAT arrives on stdin; pipe it straight into `gh auth login` so it
    // never lands on a command line. `gh` stores it 0600 under ~/.config/gh.
    let export_line = path_bootstrap::canonical_path_export_line();
    format!(
        r#"set -e
{export_line}gh auth login --with-token
gh auth setup-git
"#
    )
}
