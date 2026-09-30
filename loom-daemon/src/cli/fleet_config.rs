//! `loom-daemon fleet-config` — read the operator's fleet state store.
//!
//! Thin clap → [`loom_daemon::fleet_store`] wiring. It lives here rather than
//! in `main.rs` because `main.rs` is frozen by the file-size ratchet; the
//! variant there is one line.
//!
//! Exit codes, for every sub-verb: `0` success / in sync, `1` drift (or, for
//! `fetch`, a failed fetch; for `roster --apply`, a desired repo left unapplied
//! because it is not cloned), `2` error — nothing could be determined.

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use clap::{Args, Subcommand};

use loom_daemon::fleet_store::fetch::{self, Freshness, Loaded, Policy};
use loom_daemon::fleet_store::gh::GhTransport;
use loom_daemon::fleet_store::render::{self, Drift};
use loom_daemon::fleet_store::roster::{self, Change, Registered};
use loom_daemon::fleet_store::{self as store, state, StoreLocation};
use loom_daemon::workspace_registry::{self as registry, WorkspaceRegistry};

use super::workspace_fleet::handle_workspace_command;
use crate::WorkspaceAction;

/// `loom-daemon fleet-config` arguments.
#[derive(Args)]
pub(crate) struct FleetConfigArgs {
    /// The daemon workspace: its config names the store (`fleet.repo`) and
    /// supplies the forge credentials, and its `.loom-local/local.json` is the
    /// host-local tier `render` writes. Defaults to the current repo.
    #[arg(long, value_name = "PATH", default_value = ".", global = true)]
    workspace: String,

    #[command(subcommand)]
    command: FleetConfigCommand,
}

#[derive(Subcommand)]
enum FleetConfigCommand {
    /// Fetch the store into the local cache (`~/.loom/fleet-store/OWNER/REPO`)
    /// with a conditional request, and record the commit SHA. A failed fetch
    /// exits 1 and leaves the last good snapshot in place for `render`/`state`.
    Fetch {
        /// Print the cache manifest as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Render this host's machine tier (`deep_merge(fleet/defaults.json,
    /// fleet/hosts/<H>/defaults.json)`) and host-local tier
    /// (`fleet/hosts/<H>/local.json`) and write them where the daemon reads
    /// them. Falls back to the cached snapshot (with a warning) if the forge
    /// is unreachable.
    Render {
        /// Host id in the store (default: `LOOM_HOST_ID`, else the hostname).
        #[arg(long, value_name = "HOST")]
        host: Option<String>,
        /// Write nothing: print a diff and exit 1 on drift.
        #[arg(long)]
        check: bool,
        /// Use the cached snapshot without contacting the forge.
        #[arg(long)]
        offline: bool,
    },
    /// Diff the store's `repos.yml` against the workspace registry (adds,
    /// removes, priority changes). Fails closed: no cached fallback.
    Roster {
        /// Exit 1 when the registry does not match the store.
        #[arg(long, conflicts_with = "apply")]
        check: bool,
        /// Apply the changes through `workspace add/remove/set-priority`, for
        /// repos already cloned under `root` only (never clones).
        #[arg(long)]
        apply: bool,
        /// Print the plan as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print this host's desired run state from `fleet/state.yml`
    /// (report only; nothing enforces it yet).
    State {
        /// Host id in the store (default: `LOOM_HOST_ID`, else the hostname).
        #[arg(long, value_name = "HOST")]
        host: Option<String>,
        /// Emit JSON.
        #[arg(long)]
        json: bool,
        /// Use the cached snapshot without contacting the forge.
        #[arg(long)]
        offline: bool,
    },
}

/// Run `fleet-config`, exiting with the documented code.
pub(crate) fn dispatch(args: FleetConfigArgs) -> Result<()> {
    let code = run(args).unwrap_or_else(|e| {
        eprintln!("loom-daemon fleet-config: error: {e:#}");
        2
    });
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

struct Ctx {
    workspace: std::path::PathBuf,
    location: StoreLocation,
    cache: std::path::PathBuf,
    transport: GhTransport,
}

impl Ctx {
    fn load(&self, policy: Policy) -> Result<Loaded> {
        let loaded = fetch::load(&self.transport, &self.cache, &self.location, policy, Utc::now())?;
        if let Some(w) = loaded.staleness_warning(Utc::now()) {
            eprintln!("{w}");
        }
        Ok(loaded)
    }
}

fn run(args: FleetConfigArgs) -> Result<i32> {
    let workspace = loom_daemon::worktree_ops::repo::resolve_repo_root(&args.workspace)
        .with_context(|| format!("resolving workspace {}", args.workspace))?;
    let location = store::require_location(&workspace)?;
    let cache = store::default_cache_dir(&location)?;
    let transport = GhTransport::new(&workspace, &location.repo);
    let ctx = Ctx {
        workspace,
        location,
        cache,
        transport,
    };
    match args.command {
        FleetConfigCommand::Fetch { json } => cmd_fetch(&ctx, json),
        FleetConfigCommand::Render {
            host,
            check,
            offline,
        } => cmd_render(&ctx, host, check, offline),
        FleetConfigCommand::Roster { check, apply, json } => cmd_roster(&ctx, check, apply, json),
        FleetConfigCommand::State {
            host,
            json,
            offline,
        } => cmd_state(&ctx, host, json, offline),
    }
}

fn policy(offline: bool) -> Policy {
    if offline {
        Policy::Offline
    } else {
        Policy::AllowStale
    }
}

fn resolve_host(explicit: Option<String>) -> Result<String> {
    let own = loom_daemon::sweep_registry::host_identity();
    let host = explicit.unwrap_or_else(|| own.clone());
    store::validate_host(&host)?;
    if host != own {
        eprintln!("note: reading host `{host}` from the store; this machine identifies as `{own}`");
    }
    Ok(host)
}

fn source_line(ctx: &Ctx, loaded: &Loaded) -> String {
    let m = &loaded.snapshot.manifest;
    let how = match &loaded.freshness {
        Freshness::Live { changed: true } => "fetched",
        Freshness::Live { changed: false } => "current",
        Freshness::Cached { .. } => "CACHED",
    };
    format!(
        "fleet store {} @ {}: commit {} ({how})",
        ctx.location.repo,
        m.reference,
        loaded.snapshot.short_commit()
    )
}

fn cmd_fetch(ctx: &Ctx, json: bool) -> Result<i32> {
    match fetch::sync(&ctx.transport, &ctx.cache, &ctx.location, Utc::now()) {
        Ok((snapshot, changed)) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&snapshot.manifest)?);
                return Ok(0);
            }
            let cred = ctx
                .transport
                .last_credential()
                .map_or_else(|| "n/a".to_string(), |c| c.label().to_string());
            println!(
                "fleet store {} @ {}: commit {} ({}), {} file(s), via {cred}",
                ctx.location.repo,
                ctx.location.reference,
                snapshot.manifest.commit,
                if changed { "updated" } else { "unchanged" },
                snapshot.files.len(),
            );
            println!("cache: {}", ctx.cache.display());
            for p in snapshot.files.keys() {
                println!("  {p}");
            }
            Ok(0)
        }
        Err(e) => {
            eprintln!("loom-daemon fleet-config: fetch failed: {e:#}");
            if let Some(c) = ctx.transport.last_credential() {
                eprintln!("(last request ran under {})", c.label());
            }
            match fetch::read_cache(&ctx.cache, &ctx.location) {
                Ok(Some(snap)) => {
                    let age = Utc::now() - snap.manifest.confirmed_at;
                    eprintln!(
                        "the last good snapshot (commit {}, confirmed {}m ago) stays in {} for \
                         `render`/`state`; `roster` refuses it",
                        snap.short_commit(),
                        age.num_minutes(),
                        ctx.cache.display()
                    );
                }
                _ => eprintln!("there is no cached snapshot to fall back on"),
            }
            Ok(1)
        }
    }
}

fn cmd_render(ctx: &Ctx, host: Option<String>, check: bool, offline: bool) -> Result<i32> {
    let host = resolve_host(host)?;
    let loaded = ctx.load(policy(offline))?;
    let machine_path = loom_daemon::config_resolver::private_defaults_path().ok_or_else(|| {
        anyhow!(
            "the machine tier is disabled ({} is set to the empty string)",
            loom_daemon::config_resolver::PRIVATE_DEFAULTS_ENV
        )
    })?;
    let local_path = ctx
        .workspace
        .join(loom_daemon::config_resolver::LOCAL_CONFIG_REL);
    let targets = render::render(&loaded.snapshot, &host, &machine_path, &local_path)?;
    println!("{} — host {host}", source_line(ctx, &loaded));
    if targets.iter().all(|t| t.tier != render::Tier::Local) {
        println!(
            "host-local tier: the store has no {}; {} left as is",
            store::host_local_path(&host),
            local_path.display()
        );
    }
    if check {
        let drifts: Vec<Drift> = targets.iter().map(render::drift).collect();
        for (t, d) in targets.iter().zip(&drifts) {
            print_drift(t, d);
        }
        return Ok(render::check_exit_code(&drifts));
    }
    let stamp = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let mut wrote = false;
    for t in &targets {
        match render::write(t, &stamp)? {
            (false, _) => println!("{}: in sync — {}", t.tier.name(), t.path.display()),
            (true, backup) => {
                wrote = true;
                let b = backup.map_or_else(String::new, |b| format!(" (backup: {})", b.display()));
                println!("{}: wrote {}{b}", t.tier.name(), t.path.display());
            }
        }
    }
    if wrote {
        println!(
            "note: many daemon knobs are read once at startup — restart the daemon for the new \
             config to take effect (see fleet-config-lifecycle.md)"
        );
    }
    Ok(0)
}

fn print_drift(t: &render::Target, d: &Drift) {
    let where_ = t.path.display();
    match d {
        Drift::InSync => println!("{}: in sync — {where_}", t.tier.name()),
        Drift::Missing => println!("{}: DRIFT — {where_} does not exist", t.tier.name()),
        Drift::Unparseable(e) => {
            println!("{}: DRIFT — {where_} is not valid JSON ({e})", t.tier.name())
        }
        Drift::Differs(lines) => {
            println!("{}: DRIFT — {where_} (on disk -> store)", t.tier.name());
            for l in lines {
                println!("  {l}");
            }
        }
    }
}

fn cmd_roster(ctx: &Ctx, check: bool, apply: bool, json: bool) -> Result<i32> {
    // Fail closed: the roster carries the firewall inputs, so it is read only
    // from a snapshot the forge confirmed current in this invocation.
    let loaded = ctx.load(Policy::FailClosed)?;
    let text = loaded.snapshot.text(store::ROSTER_PATH)?.ok_or_else(|| {
        anyhow!(
            "the store has no {} (commit {})",
            store::ROSTER_PATH,
            loaded.snapshot.short_commit()
        )
    })?;
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home directory"))?;
    let parsed = roster::parse(&text, &home)?;
    let registry_path = registry::default_registry_path()?;
    let reg = WorkspaceRegistry::load(&registry_path)?;
    let registered: Vec<Registered> = reg
        .workspaces
        .iter()
        .map(|w| Registered {
            root: w.root.clone(),
            priority: w.priority,
        })
        .collect();
    let plan = roster::plan(&parsed, &registered, &registry::normalize_path, &|p: &Path| {
        p.join(".git").exists()
    });

    if json {
        println!("{}", serde_json::to_string_pretty(&plan)?);
    } else {
        println!(
            "{} — {} desired, {} in sync, registry {}",
            source_line(ctx, &loaded),
            parsed.desired().len(),
            plan.in_sync,
            registry_path.display()
        );
        for c in &plan.changes {
            println!("  {}", roster::describe(c));
        }
        for u in &plan.unmanaged {
            println!("  . unmanaged {} (not in the store; left alone)", u.display());
        }
        if plan.is_in_sync() {
            println!("registry matches the store");
        }
    }
    if !apply {
        return Ok(if check { plan.check_exit_code() } else { 0 });
    }
    let mut unapplied = 0;
    for c in &plan.changes {
        let action = match c {
            Change::Remove { path, .. } => WorkspaceAction::Remove {
                path: path.to_string_lossy().to_string(),
            },
            Change::Add { path, priority, .. } => WorkspaceAction::Add {
                path: path.to_string_lossy().to_string(),
                priority: *priority,
                config_overrides: None,
                no_init: false,
            },
            Change::SetPriority { path, to, .. } => WorkspaceAction::SetPriority {
                path: path.to_string_lossy().to_string(),
                priority: *to,
            },
            Change::MissingClone { .. } => {
                unapplied += 1;
                continue;
            }
        };
        handle_workspace_command(action)?;
    }
    if unapplied > 0 {
        eprintln!(
            "{unapplied} desired repo(s) not cloned under {} — not registered",
            parsed.root.display()
        );
        return Ok(1);
    }
    Ok(0)
}

fn cmd_state(ctx: &Ctx, host: Option<String>, json: bool, offline: bool) -> Result<i32> {
    let host = resolve_host(host)?;
    let loaded = ctx.load(policy(offline))?;
    let text = loaded
        .snapshot
        .text(store::STATE_PATH)?
        .ok_or_else(|| anyhow!("the store has no {}", store::STATE_PATH))?;
    let hs = state::resolve(&text, &host)?;
    if json {
        let mut v = serde_json::to_value(&hs)?;
        v["commit"] = loaded.snapshot.manifest.commit.clone().into();
        v["cached"] = matches!(loaded.freshness, Freshness::Cached { .. }).into();
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(0);
    }
    println!("{}", source_line(ctx, &loaded));
    let from = if hs.source == "host" {
        "host entry"
    } else {
        "fleet default"
    };
    println!("{host}: {} ({from})", hs.state.as_str());
    for (k, v) in [("since", &hs.since), ("by", &hs.by), ("reason", &hs.reason)] {
        if let Some(v) = v {
            println!("  {k}: {v}");
        }
    }
    Ok(0)
}
