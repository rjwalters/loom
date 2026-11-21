//! `loom-daemon ratification-lane status <PR>` — the read-only report that
//! names, for a PR's current head, the **effective** ee- and market-key
//! review markers and flags the superseded duplicates the pre-#14800 retry
//! bug piled up (#14834). Only reads: no dispatch, no post, no edit, no
//! hold release. "Effective" identifies the review to read first; the human
//! retains the ratification decision.
//!
//! The sub-action enum lives here, not in `root.rs`, because that file is
//! frozen by the file-size ratchet — the same reason `cli::inflight` is
//! shaped this way.

use anyhow::{anyhow, Result};
use clap::Subcommand;
use std::path::PathBuf;

use crate::role_runner::ratification_lane::{resolve_status, GhPrSource, KeyStatus};

#[derive(Subcommand)]
pub(crate) enum Action {
    /// Print the effective ee- and market-key review markers for a PR's
    /// current head, flagging superseded duplicates (#14834). Read-only.
    ///
    /// Prints the reviewed head SHA, then one row per key linking the
    /// effective review and its superseded duplicates. A key a complete read
    /// finds no marker for is printed as MISSING. Exits non-zero when the
    /// PR head or the comment history cannot be read.
    Status {
        /// The PR number.
        #[arg(value_name = "PR")]
        pr: u64,

        /// The `owner/name` repository. Defaults to the current directory's
        /// git remote.
        #[arg(long, value_name = "NWO")]
        repo: Option<String>,

        /// Workspace root for config and trust resolution. Defaults to the
        /// current directory.
        #[arg(long, value_name = "PATH")]
        root: Option<PathBuf>,
    },
}

pub(crate) fn handle(action: Action) -> Result<()> {
    let Action::Status { pr, repo, root } = action;
    let root = match root {
        Some(r) => r,
        None => std::env::current_dir()
            .map_err(|e| anyhow!("could not resolve the working directory: {e}"))?,
    };
    let nwo = match repo {
        Some(n) => n,
        None => crate::credential_preflight::nwo_from_git_remote(&root).ok_or_else(|| {
            anyhow!(
                "could not resolve the repository from {}; pass --repo owner/name",
                root.display()
            )
        })?,
    };
    let policy = crate::comment_trust::TrustPolicy::for_root(&root);
    let source = GhPrSource::new(&root, &nwo, &policy);
    let head = source
        .pr_head(pr)
        .map_err(|why| anyhow!("{nwo}#{pr}: head could not be read: {why}"))?;
    let comments = source
        .marker_comments(pr)
        .map_err(|why| anyhow!("{nwo}#{pr}: {why}"))?;
    let status = resolve_status(&head, &nwo, pr, &comments);
    println!("{nwo}#{pr}");
    println!("Reviewed head: {}", status.head_sha);
    println!("{}", key_row(&status.ee));
    println!("{}", key_row(&status.market));
    Ok(())
}

/// One key's block: the effective link (or the explicit missing-key line),
/// then one indented line per superseded duplicate.
fn key_row(k: &KeyStatus) -> String {
    let mut out = match &k.effective {
        Some(m) => {
            format!("{}: effective {} {} ({})", k.key.wire(), stamp(m), m.url, m.format.label())
        }
        None => format!(
            "{}: MISSING — no trusted {} marker names this head",
            k.key.wire(),
            k.key.wire()
        ),
    };
    for s in &k.superseded {
        out.push_str(&format!("\n  superseded {} {} ({})", stamp(s), s.url, s.format.label()));
    }
    out
}

/// RFC 3339, second precision — the forge's own comment-creation stamp.
fn stamp(m: &crate::role_runner::ratification_lane::MarkerLink) -> String {
    m.created_at
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::role_runner::ratification_lane::{Key, MarkerFormat, MarkerLink};

    fn link(id: u64, at: &str) -> MarkerLink {
        MarkerLink {
            comment_id: id,
            created_at: chrono::NaiveDateTime::parse_from_str(at, "%Y-%m-%dT%H:%M:%S")
                .unwrap()
                .and_utc(),
            url: format!("https://example/pull/7#issuecomment-{id}"),
            format: MarkerFormat::Lane,
        }
    }

    #[test]
    fn key_row_names_the_effective_marker_then_superseded_links() {
        let k = KeyStatus {
            key: Key::Market,
            effective: Some(MarkerLink {
                format: MarkerFormat::Hand,
                ..link(30, "2026-11-20T19:14:25")
            }),
            superseded: vec![
                link(20, "2026-11-20T19:00:37"),
                link(25, "2026-11-20T19:05:00"),
            ],
        };
        let row = key_row(&k);
        assert!(row.starts_with("market: effective 2026-11-20T19:14:25Z "), "{row}");
        assert!(row.contains("#issuecomment-30 (RATIFY-KEY)"), "{row}");
        assert!(row.contains("\n  superseded 2026-11-20T19:00:37Z "), "{row}");
        assert!(row.contains("\n  superseded 2026-11-20T19:05:00Z "), "{row}");
    }

    #[test]
    fn key_row_says_missing_explicitly() {
        let k = KeyStatus {
            key: Key::Ee,
            effective: None,
            superseded: Vec::new(),
        };
        let row = key_row(&k);
        assert!(row.starts_with("ee: MISSING"), "{row}");
        assert!(row.contains("no trusted ee marker"), "{row}");
    }

    #[test]
    fn documented_invocation_parses_through_the_cli_entry_point() {
        let cli = crate::cli::whole_cli_parse::try_parse_cli(&[
            "loom-daemon",
            "ratification-lane",
            "status",
            "98",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(crate::cli::Commands::RatificationLane {
                action: Action::Status { pr: 98, .. }
            })
        ));
        let with_flags = crate::cli::whole_cli_parse::try_parse_cli(&[
            "loom-daemon",
            "ratification-lane",
            "status",
            "7",
            "--repo",
            "o/r",
            "--root",
            "/tmp/x",
        ])
        .unwrap();
        assert!(matches!(
            with_flags.command,
            Some(crate::cli::Commands::RatificationLane {
                action: Action::Status { pr: 7, .. }
            })
        ));
    }
}
