//! `loom-daemon guard-mcp-tools` — the `PreToolUse` decision for the
//! `mcp__loom__*` tool namespace (Issue #9108).
//!
//! # Output contract
//!
//! Reads the `PreToolUse` payload JSON on stdin. Prints a
//! `hookSpecificOutput.permissionDecision` deny document to stdout to refuse,
//! or nothing at all to allow, and **always exits 0** — the contract every
//! Loom guard hook carries, so a bug here can never wedge Claude Code in a
//! retry loop or turn a missed check into a broken session.
//!
//! The logic lives in [`loom_daemon::mcp_tool_guard`]; this module is stdin,
//! stdout, and the exit code.

use anyhow::Result;

use loom_daemon::mcp_tool_guard::{self, HookPayload};

#[derive(clap::Args)]
pub(crate) struct GuardMcpToolsArgs {
    /// Workspace root used for the `guards.mcpToolArgs` toggle and the decision
    /// log. Defaults to the payload's `cwd`, then the current directory.
    #[arg(long, value_name = "PATH")]
    repo_root: Option<std::path::PathBuf>,
}

impl GuardMcpToolsArgs {
    pub(crate) fn run(self) -> Result<()> {
        let mut raw = String::new();
        // Every failure below is an ALLOW: a guard that cannot read its own
        // payload must not be the reason an MCP call fails.
        if std::io::Read::read_to_string(&mut std::io::stdin(), &mut raw).is_err() {
            std::process::exit(0);
        }
        let payload: HookPayload = serde_json::from_str(&raw).unwrap_or_default();

        let root = self
            .repo_root
            .or_else(|| payload.cwd.as_deref().map(std::path::PathBuf::from))
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        // A hook's cwd can be a linked worktree; the host-local config tier
        // (`.loom-local/`) lives only in the main checkout, so resolve the
        // toggle against that, exactly as the Stop-hook guard does.
        let root = loom_daemon::worktree_state::main_checkout_root(&root).unwrap_or(root);

        let decision = mcp_tool_guard::evaluate(&payload, &root);
        mcp_tool_guard::log_decision(&root, &decision);
        if let Some(json) = decision.to_hook_json() {
            println!("{json}");
        }
        std::process::exit(0);
    }
}
