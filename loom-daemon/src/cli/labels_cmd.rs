//! `loom-daemon labels …` (#10013): query and generate from the label
//! registry (`defaults/labels.json`). Args live here because `main.rs` is
//! frozen by the file-size ratchet.
//!
//! Exit codes: 0 ok; 1 unknown label (`get`) or drift (`check`); 2 unknown
//! property/kind/field or unreadable registry.

use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Subcommand;
use loom_daemon::label_registry::{generate, Registry, BOOL_PROPERTIES, KINDS};

/// Query the label registry and keep `labels.yml` generated from it (#10013).
#[derive(clap::Args)]
pub(crate) struct LabelsArgs {
    #[command(subcommand)]
    action: LabelsCommand,
}

#[derive(Subcommand)]
pub(crate) enum LabelsCommand {
    /// Print label names (one per line) with a property or of a kind.
    List {
        /// A boolean property: park, skip, hold, operator_gate,
        /// blocked_colabel, hard_exclusion, champion_path, human_gated,
        /// merge_hold, operator_hold, contradicts_approval (report order).
        #[arg(long, conflicts_with = "kind")]
        property: Option<String>,
        /// A label kind (workflow, claim, pr-lane, proposal, hold, ...).
        #[arg(long)]
        kind: Option<String>,
    },
    /// Print one label as JSON, or a single field with `--field`.
    Get {
        name: String,
        #[arg(long)]
        field: Option<String>,
    },
    /// Print the generated Loom block, or with `--write` rewrite both full
    /// `labels.yml` copies from `defaults/labels.json`.
    Generate {
        #[arg(long)]
        root: Option<PathBuf>,
        #[arg(long)]
        write: bool,
    },
    /// Exit 1 when either full `labels.yml` copy differs from the generated block.
    Check {
        #[arg(long)]
        root: Option<PathBuf>,
    },
}

const COPIES: &[&str] = &[".github/labels.yml", "defaults/.github/labels.yml"];

fn root_of(root: Option<PathBuf>) -> PathBuf {
    // The git toplevel of the CWD, NOT the canonical repo root: from a worktree
    // `generate --write` must edit that worktree's files, never the primary
    // checkout's.
    root.or_else(|| {
        let out = std::process::Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
    })
    .or_else(|| std::env::current_dir().ok())
    .unwrap_or_else(|| PathBuf::from("."))
}

/// On-disk registry when present (so `generate` sees edits without a
/// rebuild), else the embedded one.
fn load(root: &Path) -> Result<Registry> {
    let p = root.join("defaults/labels.json");
    if p.is_file() {
        Registry::parse(&std::fs::read_to_string(&p)?)
    } else {
        Ok(Registry::embedded().clone())
    }
}

pub(crate) fn dispatch(args: LabelsArgs) -> Result<()> {
    let code = run(args.action)?;
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

fn run(cmd: LabelsCommand) -> Result<i32> {
    match cmd {
        LabelsCommand::List { property, kind } => {
            let reg = Registry::embedded();
            let names: Vec<&str> = match (property, kind) {
                (Some(p), None) => match reg.with_property(&p) {
                    Some(n) => n,
                    None => {
                        eprintln!("unknown property '{p}'; known: {}", BOOL_PROPERTIES.join(", "));
                        return Ok(2);
                    }
                },
                (None, Some(k)) => {
                    if !KINDS.contains(&k.as_str()) {
                        eprintln!("unknown kind '{k}'; known: {}", KINDS.join(", "));
                        return Ok(2);
                    }
                    reg.with_kind(&k)
                }
                _ => reg.labels.iter().map(|l| l.name.as_str()).collect(),
            };
            for n in names {
                println!("{n}");
            }
            Ok(0)
        }
        LabelsCommand::Get { name, field } => {
            let Some(label) = Registry::embedded().get(&name) else {
                eprintln!("unknown label '{name}'");
                return Ok(1);
            };
            let mut v = serde_json::to_value(label_json(label))?;
            if let Some(f) = field {
                match v.get_mut(&f).map(serde_json::Value::take) {
                    Some(serde_json::Value::String(s)) => println!("{s}"),
                    Some(serde_json::Value::Null) => {}
                    Some(other) => println!("{other}"),
                    None => {
                        eprintln!("unknown field '{f}'");
                        return Ok(2);
                    }
                }
            } else {
                println!("{}", serde_json::to_string_pretty(&v)?);
            }
            Ok(0)
        }
        LabelsCommand::Generate { root, write } => {
            let root = root_of(root);
            let block = generate::loom_block(&load(&root)?);
            if write {
                for rel in COPIES {
                    std::fs::write(root.join(rel), &block)?;
                    eprintln!("wrote {rel}");
                }
            } else {
                print!("{block}");
            }
            Ok(0)
        }
        LabelsCommand::Check { root } => {
            let root = root_of(root);
            let block = generate::loom_block(&load(&root)?);
            let mut rc = 0;
            for rel in COPIES {
                match std::fs::read_to_string(root.join(rel)) {
                    Ok(c) if c == block => println!("labels check: {rel} matches the registry"),
                    Ok(_) => {
                        eprintln!(
                            "labels check: {rel} differs from defaults/labels.json. Edit the \
                             registry and run `loom-daemon labels generate --write`."
                        );
                        rc = 1;
                    }
                    Err(e) => {
                        eprintln!("labels check: cannot read {rel}: {e}");
                        rc = 2;
                    }
                }
            }
            Ok(rc)
        }
    }
}

fn label_json(l: &loom_daemon::label_registry::Label) -> serde_json::Value {
    let mut v = serde_json::json!({
        "name": l.name, "description": l.description, "color": l.color,
        "kind": l.kind, "applied_by": l.applied_by, "removed_by": l.removed_by,
        "park": l.park, "skip": l.skip, "hold": l.hold,
        "operator_gate": l.operator_gate, "blocked_colabel": l.blocked_colabel,
        "hard_exclusion": l.hard_exclusion, "champion_path": l.champion_path,
        "human_gated": l.human_gated,
        "merge_hold": l.merge_hold, "operator_hold": l.operator_hold, "contradicts_approval": l.contradicts_approval,
        "stale_after_minutes": l.stale_after_minutes, "requires_base": l.requires_base,
        "remove_with": l.remove_with, "lifecycle": l.lifecycle,
    });
    if let Some(p) = &l.propagate {
        v["propagate"] = p.clone();
    }
    v
}
