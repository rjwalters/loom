//! `loom-daemon forge-inventory …` — the operation inventory's CLI
//! (Issue #9777, phase 1 of epic #9769).
//!
//! Four verbs over one embedded manifest. None of them makes a forge call, so
//! all four run in CI with no credential and on a laptop with no network:
//!
//! | Verb | Exit 0 | Exit 1 | Exit 2 |
//! |---|---|---|---|
//! | `validate` | manifest is publishable | a rule failed | could not run |
//! | `validate --qualification` | evidence could support a GO | something is still unresolved | could not run |
//! | `gate` | no unclassified/grown call site | a bypass appeared or grew | could not run |
//! | `report` | always (read-only view) | — | could not run |
//! | `probe-manifest` | always (read-only view) | — | could not run |
//!
//! `gate --update` rewrites the baseline instead of judging it, and
//! `--manifest-dir` points the whole family at a synthetic manifest so a test
//! can exercise a deliberately broken one.

use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::{Context, Result};

use loom_daemon::forge_inventory::{
    self, gate, model::Profile, probe, report, validate, Inventory,
};

#[derive(clap::Subcommand)]
pub(crate) enum ForgeInventoryCommand {
    /// Coverage validator: reject a manifest that cannot support a claim.
    Validate(ValidateArgs),
    /// Change gate: reject a new unclassified direct forge call.
    Gate(GateArgs),
    /// Four-axis coverage report (platform / adapter / caller / unknowns).
    Report(ReportArgs),
    /// Emit the hosted-probe work list for one or more profiles.
    ProbeManifest(ProbeArgs),
}

impl ForgeInventoryCommand {
    pub(crate) fn run(self) -> Result<()> {
        match self {
            ForgeInventoryCommand::Validate(a) => a.run(),
            ForgeInventoryCommand::Gate(a) => a.run(),
            ForgeInventoryCommand::Report(a) => a.run(),
            ForgeInventoryCommand::ProbeManifest(a) => a.run(),
        }
    }
}

/// Load the embedded manifest, or a synthetic one from `--manifest-dir`.
fn load(manifest_dir: Option<&PathBuf>) -> Result<Inventory> {
    match manifest_dir {
        Some(dir) => forge_inventory::load_from_dir(dir),
        None => forge_inventory::load_embedded(),
    }
}

fn parse_profile(s: &str) -> Result<Profile> {
    Ok(match s {
        "required-coordination" => Profile::RequiredCoordination,
        "required-ci-landing" => Profile::RequiredCiLanding,
        "fleet-bootstrap" => Profile::FleetBootstrap,
        "delivery" => Profile::Delivery,
        "optional" => Profile::Optional,
        other => anyhow::bail!(
            "unknown profile `{other}` (required-coordination | required-ci-landing | fleet-bootstrap | delivery | optional)"
        ),
    })
}

// ============================================================================
// validate
// ============================================================================

#[derive(clap::Args)]
pub(crate) struct ValidateArgs {
    /// Validate a manifest directory instead of the embedded one.
    #[arg(long, value_name = "DIR")]
    manifest_dir: Option<PathBuf>,
    /// Repo root, for the filesystem-backed `test_status = implemented` check.
    #[arg(long, value_name = "DIR", default_value = ".")]
    root: PathBuf,
    /// Skip the filesystem-backed test-evidence check (pure rules only).
    #[arg(long)]
    no_test_evidence: bool,
    /// Also apply the qualification gate's rules: refuse to count an
    /// acknowledged unknown, a reserved-but-unimplemented test or probe-only
    /// evidence as a pass. This is #9769's GO/NO-GO question (#9792), NOT a CI
    /// gate — on phase 1's tree it reports every unresolved row by design.
    #[arg(long)]
    qualification: bool,
    /// Machine-readable findings.
    #[arg(long)]
    json: bool,
}

impl ValidateArgs {
    pub(crate) fn run(self) -> Result<()> {
        let inv = load(self.manifest_dir.as_ref())?;
        let mut findings = validate::validate(&inv);
        if !self.no_test_evidence {
            findings.extend(validate::validate_test_evidence(&inv, &self.root));
        }
        if self.qualification {
            findings.extend(validate::validate_qualification(&inv));
        }
        let totals = validate::totals(&inv);

        if self.json {
            let doc = serde_json::json!({
                "schema_version": inv.header.schema_version,
                "rows": totals.rows,
                "active": totals.active,
                "required": totals.required,
                "unknown_support": totals.unknown_support,
                "declared_tests": totals.declared_tests,
                "findings": findings.iter().map(|f| serde_json::json!({
                    "rule": f.rule.as_str(),
                    "subject": f.subject,
                    "detail": f.detail,
                })).collect::<Vec<_>>(),
            });
            println!("{}", serde_json::to_string_pretty(&doc)?);
        } else {
            for f in &findings {
                eprintln!("forge-inventory: {f}");
            }
            if findings.is_empty() {
                println!(
                    "forge-inventory: OK — {} row(s), {} active, {} required, {} with unknown platform support, {} required row(s) whose test id is still only reserved.",
                    totals.rows,
                    totals.active,
                    totals.required,
                    totals.unknown_support,
                    totals.declared_tests
                );
            } else {
                eprintln!(
                    "forge-inventory: {} finding(s) — see defaults/forge/ and loom-daemon/src/forge_inventory/validate.rs",
                    findings.len()
                );
            }
        }
        if findings.is_empty() {
            Ok(())
        } else {
            std::process::exit(1);
        }
    }
}

// ============================================================================
// gate
// ============================================================================

#[derive(clap::Args)]
pub(crate) struct GateArgs {
    /// Repo root to scan.
    #[arg(long, value_name = "DIR", default_value = ".")]
    root: PathBuf,
    /// Validate a manifest directory instead of the embedded one.
    #[arg(long, value_name = "DIR")]
    manifest_dir: Option<PathBuf>,
    /// Baseline file to compare against (default: the embedded copy).
    #[arg(long, value_name = "FILE")]
    baseline: Option<PathBuf>,
    /// Rewrite the baseline from the current tree instead of judging it.
    #[arg(long)]
    update: bool,
    /// Issue that owns removal of newly recorded entries (with `--update`).
    #[arg(long, value_name = "N")]
    removal_issue: Option<u64>,
}

/// Tracked files to scan, from `git ls-files` (so untracked scratch never
/// fails a gate and a deleted file never lingers in the measurement).
fn tracked_files(root: &std::path::Path) -> Result<Vec<String>> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files"])
        .output()
        .context("running `git ls-files`")?;
    anyhow::ensure!(out.status.success(), "`git ls-files` failed in {}", root.display());
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect())
}

impl GateArgs {
    pub(crate) fn run(self) -> Result<()> {
        let inv = load(self.manifest_dir.as_ref())?;
        let declared: BTreeSet<String> = inv.declared_caller_paths();
        let baseline = match self.baseline.as_ref() {
            Some(p) => toml::from_str(
                &std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?,
            )
            .with_context(|| format!("parsing {}", p.display()))?,
            None => forge_inventory::load_embedded_baseline()?,
        };
        let files = tracked_files(&self.root)?;
        let results = gate::evaluate(&self.root, &files, &declared, &baseline);

        if self.update {
            return self.write_updated(&results, &baseline);
        }

        let mut failed = false;
        for problem in gate::validate_baseline(&baseline, &inv.header.owners) {
            eprintln!("forge-inventory gate: BASELINE  {problem}");
            failed = true;
        }
        for r in results.iter().filter(|r| r.is_failure()) {
            failed = true;
            match r.verdict {
                gate::Verdict::Unclassified => {
                    eprintln!(
                        "forge-inventory gate: UNCLASSIFIED  {} ({} direct forge call site(s))",
                        r.path, r.calls
                    );
                }
                gate::Verdict::BaselineGrew { recorded } => {
                    eprintln!(
                        "forge-inventory gate: GREW  {} ({recorded} -> {} direct forge call site(s))",
                        r.path, r.calls
                    );
                }
                _ => {}
            }
            for sample in &r.samples {
                eprintln!("    {sample}");
            }
        }

        let resolved = gate::resolved_entries(&results, &baseline);
        for path in &resolved {
            println!("forge-inventory gate: RESOLVED  {path} (no direct forge call left — drop it with --update)");
        }

        if failed {
            eprintln!(
                "\nforge-inventory gate: a direct forge call must be CLASSIFIED or BASELINED.\n\
                 Classify it by declaring the file as a caller of an operation in\n\
                 defaults/forge/operations/*.toml, or — if migration is out of scope for\n\
                 this change — record it in {} with an owner and a removal issue:\n\
                 \n    loom-daemon forge-inventory gate --update --removal-issue <N>\n",
                forge_inventory::BASELINE_PATH
            );
            std::process::exit(1);
        }

        let classified = results
            .iter()
            .filter(|r| r.verdict == gate::Verdict::Classified)
            .count();
        let baselined = results.len() - classified;
        println!(
            "forge-inventory gate: OK — {} file(s) with direct forge calls: {classified} classified, {baselined} baselined{}.",
            results.len(),
            if resolved.is_empty() {
                String::new()
            } else {
                format!(", {} resolved entr(ies) ready to drop", resolved.len())
            }
        );
        Ok(())
    }

    /// Regenerate the baseline: keep each still-offending file, carrying its
    /// existing owner/removal issue forward when it has one.
    fn write_updated(&self, results: &[gate::FileResult], old: &gate::Baseline) -> Result<()> {
        let removal_issue = self.removal_issue.or(old.removal_epic).context(
            "--update needs --removal-issue <N> (or a removal_epic in the old baseline)",
        )?;
        let bypasses = results
            .iter()
            .filter(|r| r.verdict != gate::Verdict::Classified)
            .map(|r| {
                let prev = old.get(&r.path);
                gate::Bypass {
                    path: r.path.clone(),
                    calls: r.calls,
                    owner: prev.map_or_else(
                        || gate::owner_for_path(&r.path).to_string(),
                        |p| p.owner.clone(),
                    ),
                    removal_issue: prev.map_or(removal_issue, |p| p.removal_issue),
                }
            })
            .collect();
        let updated = gate::Baseline {
            removal_epic: old.removal_epic.or(Some(removal_issue)),
            bypasses,
        };
        let target = self
            .baseline
            .clone()
            .unwrap_or_else(|| self.root.join(forge_inventory::BASELINE_PATH));
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&target, gate::render_baseline(&updated))
            .with_context(|| format!("writing {}", target.display()))?;
        println!(
            "forge-inventory gate: wrote {} ({} entr(ies))",
            target.display(),
            updated.bypasses.len()
        );
        Ok(())
    }
}

// ============================================================================
// report
// ============================================================================

#[derive(clap::Args)]
pub(crate) struct ReportArgs {
    #[arg(long, value_name = "DIR")]
    manifest_dir: Option<PathBuf>,
    #[arg(long)]
    json: bool,
    /// Restrict the rows to one profile.
    #[arg(long, value_name = "PROFILE")]
    profile: Option<String>,
}

impl ReportArgs {
    pub(crate) fn run(self) -> Result<()> {
        let inv = load(self.manifest_dir.as_ref())?;
        let mut rep = report::build(&inv);
        if let Some(name) = self.profile.as_deref() {
            let want = parse_profile(name)?.as_str();
            rep.rows.retain(|r| r.profile == want);
        }
        if self.json {
            println!("{}", serde_json::to_string_pretty(&rep)?);
        } else {
            print!("{}", report::render_text(&rep));
        }
        Ok(())
    }
}

// ============================================================================
// probe-manifest
// ============================================================================

#[derive(clap::Args)]
pub(crate) struct ProbeArgs {
    #[arg(long, value_name = "DIR")]
    manifest_dir: Option<PathBuf>,
    /// Repeatable. Default: every required profile.
    #[arg(long = "profile", value_name = "PROFILE")]
    profiles: Vec<String>,
    /// Emit JSON. Accepted for symmetry and already the default; `--no-json`
    /// prints a terse checklist instead. (NOT `default_value_t = true`: that
    /// made `--json` an unconditional no-op whose `--help` still advertised a
    /// default — PR #9832 review.)
    #[arg(long)]
    json: bool,
    #[arg(long, conflicts_with = "json")]
    no_json: bool,
}

impl ProbeArgs {
    pub(crate) fn run(self) -> Result<()> {
        let inv = load(self.manifest_dir.as_ref())?;
        let profiles: Vec<Profile> = self
            .profiles
            .iter()
            .map(|s| parse_profile(s))
            .collect::<Result<_>>()?;
        let manifest = probe::build(&inv, &profiles);
        // The two flags conflict, so at most one is set; JSON when neither is.
        let as_json = self.json || !self.no_json;
        if as_json {
            println!("{}", serde_json::to_string_pretty(&manifest)?);
        } else {
            for e in &manifest.entries {
                println!(
                    "{:<34} {:<9} {:<10} establishes={} test={}",
                    e.id,
                    e.risk,
                    e.class,
                    e.establishes.as_str(),
                    e.test_id.as_deref().unwrap_or("-")
                );
            }
        }
        Ok(())
    }
}
