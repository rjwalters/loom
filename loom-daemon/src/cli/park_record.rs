//! `loom-daemon park-record` — render and read the `loom:blocked` park record
//! (#8925).
//!
//! # Why a command and not a documented string
//!
//! The park record is written by a **role prompt**, i.e. by an LLM copying a
//! format out of prose. That is exactly the situation in which a format drifts:
//! a comma instead of a second record, a `blocked-by=` attribute instead of the
//! `Blocked by:` phrase every existing parser matches, a reason containing `--`
//! that terminates the comment early. Each of those produces a record that looks
//! right and reads as zero blockers.
//!
//! `render` removes the opportunity: the role supplies numbers and a reason, and
//! gets back the exact line to paste. `parse` is its inverse, for a shell caller
//! (`guide.md`'s unblock sweep) that needs the declared blockers without
//! re-implementing the grammar in `grep`.
//!
//! Both are pure — no forge read, no label write. `apply` (#10152) is the one
//! that parks: it writes the record into the body, then adds `loom:blocked` —
//! the shared label-apply path, so a park cannot be applied without one.

use std::io::Read;
use std::path::PathBuf;

use anyhow::Result;

use loom_daemon::park_record::{self, BlockerRef, ParkRecord};

#[derive(clap::Subcommand)]
pub(crate) enum ParkRecordCommand {
    /// Render the park record line(s) to paste into an artifact body. One line
    /// per blocker (see `park-record.md` for why a comma-separated list is
    /// unsafe). Exit 0 always.
    Render(RenderArgs),

    /// Read the park records out of a body. Prints one declared blocker number
    /// per line, or a JSON object with `--json`. Exit 0 when at least one record
    /// was found, 1 when none was — so a shell caller can branch on "is this
    /// park declared?" without parsing output.
    Parse(ParseArgs),

    /// Park an issue or PR: write the park record into its BODY, then add
    /// `loom:blocked` (#10152). Refuses (exit 1, nothing changed) with no
    /// `--blocked-by` and no explicit `--reason`, or with a closed blocker.
    /// Idempotent. Exit 4 on a forge failure; a failed body write adds no label.
    Apply(ApplyArgs),
}

#[derive(clap::Args)]
pub(crate) struct ApplyArgs {
    /// The issue to park.
    #[arg(
        long,
        value_name = "N",
        required_unless_present = "pr",
        conflicts_with = "pr"
    )]
    pub issue: Option<u64>,

    /// The PR to park.
    #[arg(long, value_name = "N")]
    pub pr: Option<u64>,

    /// The open blocker(s): `N`, `#N` (this repo) or `OWNER/REPO#N` (another
    /// repo). Repeatable; comma-separated accepted.
    #[arg(long = "blocked-by", value_name = "REF", num_args = 1.., value_delimiter = ',')]
    pub blocked_by: Vec<BlockerRef>,

    /// Why. Required when no `--blocked-by` is given (e.g. `operator`), so a
    /// park with no named blocker is a deliberate choice.
    #[arg(long, value_name = "TEXT")]
    pub reason: Option<String>,

    /// Who is parking it — a role name or `human`.
    #[arg(long = "by", value_name = "ROLE")]
    pub by: Option<String>,

    /// Label to remove once parked (repeatable), e.g. loom:building.
    #[arg(long = "remove-label", value_name = "LABEL")]
    pub remove_label: Vec<String>,

    /// Target repository; defaults to the checkout's remote.
    #[arg(long, value_name = "OWNER/REPO")]
    pub repo: Option<String>,

    /// Print the planned body and label changes; mutate nothing.
    #[arg(long = "dry-run")]
    pub dry_run: bool,
}

#[derive(clap::Args)]
pub(crate) struct RenderArgs {
    /// The blocker(s): `N`, `#N` or `OWNER/REPO#N`. Repeatable, and comma-separated
    /// values are accepted. Omit to render an explicit "blocker unstated"
    /// record, which is still attributable and dated — unlike silence.
    #[arg(long = "blocked-by", value_name = "REF", num_args = 1.., value_delimiter = ',')]
    pub blocked_by: Vec<BlockerRef>,

    /// Who is applying the park — a role name (`doctor`, `champion`, `curator`)
    /// or `human`.
    #[arg(long = "by", value_name = "ROLE")]
    pub by: Option<String>,

    /// When, as RFC 3339. Defaults to now.
    #[arg(long = "at", value_name = "TIMESTAMP")]
    pub at: Option<String>,

    /// One-line free-text reason. Sanitised: flattened to a single line, `"`
    /// downgraded to `'`, and `--` runs collapsed so the reason cannot terminate
    /// the HTML comment early.
    #[arg(long, value_name = "TEXT")]
    pub reason: Option<String>,
}

#[derive(clap::Args)]
pub(crate) struct ParseArgs {
    /// Read the body from this file. Use `-` (or omit) for stdin.
    #[arg(long, value_name = "PATH")]
    pub file: Option<PathBuf>,

    /// Emit one JSON object instead of one blocker number per line.
    #[arg(long)]
    pub json: bool,
}

impl ParkRecordCommand {
    pub(crate) fn run(self) -> Result<()> {
        match self {
            ParkRecordCommand::Render(args) => args.run(),
            ParkRecordCommand::Parse(args) => args.run(),
            ParkRecordCommand::Apply(args) => args.run(),
        }
    }
}

impl RenderArgs {
    fn run(self) -> Result<()> {
        let at = self.at.unwrap_or_else(|| {
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        });
        println!(
            "{}",
            park_record::render_park(
                &self.blocked_by,
                self.by.as_deref(),
                Some(&at),
                self.reason.as_deref(),
            )
        );
        Ok(())
    }
}

impl ApplyArgs {
    /// Never returns: exits with the command's own code.
    fn run(self) -> Result<()> {
        use loom_daemon::operator_decision::cli::{default_repo_root, GhForge};
        use loom_daemon::park_record::apply::{apply, ApplyRequest};
        let req = ApplyRequest {
            number: self.issue.or(self.pr).unwrap_or_default(),
            repo: self.repo.clone(),
            blocked_by: self.blocked_by,
            reason: self.reason,
            by: self.by,
            at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            remove_labels: self.remove_label,
            dry_run: self.dry_run,
        };
        let mut forge = GhForge::new(default_repo_root(), self.repo);
        let code = apply(&mut forge, &req, &mut std::io::stdout(), &mut std::io::stderr());
        std::process::exit(code)
    }
}

impl ParseArgs {
    fn run(self) -> Result<()> {
        let body = match self.file.as_deref() {
            Some(p) if p != std::path::Path::new("-") => std::fs::read_to_string(p)?,
            _ => {
                let mut buf = String::new();
                std::io::stdin().read_to_string(&mut buf)?;
                buf
            }
        };

        let records = park_record::parse(&body);
        let blockers = park_record::blockers(&body);

        if self.json {
            let rows: Vec<_> = records.iter().map(record_json).collect();
            println!(
                "{}",
                serde_json::json!({
                    "records": rows,
                    // Local blockers stay bare numbers (unchanged); qualified ones are
                    // `OWNER/REPO#N` strings.
                    "blockers": blockers.iter().map(|b| match &b.repo {
                        None => serde_json::json!(b.number),
                        Some(_) => serde_json::json!(b.to_string()),
                    }).collect::<Vec<_>>(),
                    "declared": !records.is_empty(),
                })
            );
        } else {
            for b in &blockers {
                // Bare number for a local blocker (unchanged), qualified otherwise.
                match &b.repo {
                    None => println!("{}", b.number),
                    Some(_) => println!("{b}"),
                }
            }
        }

        // Exit 1 for "no record", mirroring `forge check-open-pr`'s convention
        // that an answer of "none" is an exit code, not an error.
        if records.is_empty() {
            std::process::exit(1);
        }
        Ok(())
    }
}

fn record_json(r: &ParkRecord) -> serde_json::Value {
    serde_json::json!({
        "blocker": r.blocker.as_ref().map(|b| b.number),
        "blocker_repo": r.blocker.as_ref().and_then(|b| b.repo.clone()),
        "by": r.by,
        "at": r.at,
        "reason": r.reason,
    })
}
