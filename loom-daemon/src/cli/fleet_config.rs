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
use loom_daemon::fleet_store::propose;
use loom_daemon::fleet_store::reload;
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
    ///
    /// Every changed key is classified live-reloadable or restart-required
    /// (#9597, [`loom_daemon::fleet_store::reload`]). A live-reloadable
    /// change is confirmed against a running daemon over IPC (`DaemonStatus`)
    /// with no restart; a restart-required change is recorded in
    /// [`loom_daemon::fleet_store::pending_restart`] so `loom-daemon status`
    /// keeps reporting it until the daemon that was running at render time
    /// actually restarts.
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
        /// Permit a lossy reduction: writing a machine tier that DROPS
        /// top-level blocks the file on disk carries (the store never had
        /// them). 2am#1653's clobber class — the refusal is the default.
        #[arg(long)]
        allow_reduce: bool,
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
    /// Open a PR against the store instead of hand-editing it (#9599).
    /// Always a branch + PR — never a direct push. Edits `fleet.yml`, the
    /// store's source, and regenerates its renders with the store's own
    /// `scripts/render.py` when `python3` with PyYAML is available (#10905). Needs the writer app's
    /// `contents: write` and `pull_requests: write` on the store, an
    /// explicit operator grant; without it this fails with a clear error,
    /// not a crash.
    Propose {
        #[command(subcommand)]
        command: ProposeCommand,
    },
}

#[derive(Subcommand)]
enum ProposeCommand {
    /// Move a host (or, with no `--host`, the fleet default) to a new
    /// desired run state in `fleet.yml`'s `state:`.
    State {
        /// `running`, `paused` or `stopped`.
        state: String,
        /// Host id in the store (default: the fleet-wide default entry).
        #[arg(long, value_name = "HOST")]
        host: Option<String>,
        /// Why — recorded as `reason` on the entry.
        #[arg(long)]
        reason: String,
        /// Who — recorded as `by` (default: this host's identity).
        #[arg(long, value_name = "WHO")]
        by: Option<String>,
        /// Print the diff without opening a PR.
        #[arg(long)]
        dry_run: bool,
    },
    /// Change one `fleet.yml` `repos:` record's dispatch priority.
    Priority {
        /// The record's `name:`.
        repo: String,
        /// The new `fleet_priority`.
        priority: u32,
        /// Print the diff without opening a PR.
        #[arg(long)]
        dry_run: bool,
    },
    /// Turn this host's `render --check` drift into a PR that moves the
    /// on-disk values into `fleet.yml`'s `config.hosts.<host>`.
    Adopt {
        /// Host id in the store (default: `LOOM_HOST_ID`, else the hostname).
        #[arg(long, value_name = "HOST")]
        host: Option<String>,
        /// Print the diff without opening a PR.
        #[arg(long)]
        dry_run: bool,
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
            allow_reduce,
        } => cmd_render(&ctx, host, check, offline, allow_reduce),
        FleetConfigCommand::Roster { check, apply, json } => cmd_roster(&ctx, check, apply, json),
        FleetConfigCommand::State {
            host,
            json,
            offline,
        } => cmd_state(&ctx, host, json, offline),
        FleetConfigCommand::Propose { command } => match command {
            ProposeCommand::State {
                state,
                host,
                reason,
                by,
                dry_run,
            } => cmd_propose_state(&ctx, state, host, reason, by, dry_run),
            ProposeCommand::Priority {
                repo,
                priority,
                dry_run,
            } => cmd_propose_priority(&ctx, repo, priority, dry_run),
            ProposeCommand::Adopt { host, dry_run } => cmd_propose_adopt(&ctx, host, dry_run),
        },
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

fn cmd_render(
    ctx: &Ctx,
    host: Option<String>,
    check: bool,
    offline: bool,
    allow_reduce: bool,
) -> Result<i32> {
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
    // The lossy-reduction guard (2am#1653): a machine-tier target that would
    // DROP top-level blocks the file on disk carries is refused unless the
    // operator names it. The store is the tier's record of truth, so the
    // blocks belong IN the store first (`fleet-config propose adopt`
    // pushes them there) — a silent write that loses `runtimes`/`forge`/
    // `autonomous`/`safehouse` for half a day is the failure this refuses.
    for t in &targets {
        let lost = render::lost_top_level_keys(t);
        if lost.is_empty() {
            continue;
        }
        let msg = format!(
            "{} would DROP top-level block(s) the file on disk carries: {} — the store's \
             fleet/defaults.json never had them. Add them to the store first \
             (`fleet-config propose adopt` proposes exactly that), or pass \
             --allow-reduce to accept the loss knowingly.",
            t.tier.name(),
            lost.join(", ")
        );
        if check {
            // check reports drift already; make the reduction unmissable and
            // fail the check (exit 2 = the CLI's error class)
            eprintln!("LOSSY REDUCTION: {msg}");
            return Ok(2);
        }
        if !allow_reduce {
            eprintln!("REFUSED — lossy reduction: {msg}");
            return Ok(2);
        }
        println!("--allow-reduce: accepting the dropped block(s): {}", lost.join(", "));
    }
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
        print_reload_classification(&targets, &drifts);
        return Ok(render::check_exit_code(&drifts));
    }
    let drifts: Vec<Drift> = targets.iter().map(render::drift).collect();
    let (live, restart_required) = classify_drifted_paths(&targets, &drifts);
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
        super::fleet_config_reload::report(&ctx.workspace, &live, &restart_required);
    }
    Ok(0)
}

/// [`Reloadability`](reload::Reloadability)-classify every path each target's
/// `Drift::Differs` names, returning `(live, restart_required)` in target
/// order (Issue #9597). A `Missing`/`Unparseable` target replaces the whole
/// file, so there is no meaningful per-path diff to classify — it is
/// conservatively treated as restart-required as a whole, named by tier
/// rather than by key.
fn classify_drifted_paths(
    targets: &[render::Target],
    drifts: &[Drift],
) -> (Vec<String>, Vec<String>) {
    let mut live = Vec::new();
    let mut restart_required = Vec::new();
    for (t, d) in targets.iter().zip(drifts) {
        match d {
            Drift::Differs(_) => {
                let (l, r) = reload::partition(&render::drifted_paths(t));
                live.extend(l);
                restart_required.extend(r);
            }
            Drift::Missing | Drift::Unparseable(_) => {
                restart_required.push(format!("{} (whole file)", t.tier.name()));
            }
            Drift::InSync => {}
        }
    }
    (live, restart_required)
}

/// `render --check`'s read-only counterpart to the write path's live-reload
/// report: names which changed keys are live-reloadable vs. restart-required,
/// without signaling a daemon or writing the pending-restart marker (nothing
/// was written).
fn print_reload_classification(targets: &[render::Target], drifts: &[Drift]) {
    let (live, restart_required) = classify_drifted_paths(targets, drifts);
    if !live.is_empty() {
        println!("live-reloadable if applied (no restart needed): {}", live.join(", "));
    }
    if !restart_required.is_empty() {
        println!(
            "restart-required if applied (needs the daemon to restart): {}",
            restart_required.join(", ")
        );
    }
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
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home directory"))?;
    let parsed = roster::from_snapshot(&loaded.snapshot, &home)?
        .ok_or_else(|| anyhow!(roster::missing_message(&loaded.snapshot)))?;
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
    let hs = state::resolve_snapshot(&loaded.snapshot, &host)?
        .ok_or_else(|| anyhow!(state::missing_message(&loaded.snapshot)))?;
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

fn cmd_propose_state(
    ctx: &Ctx,
    state_arg: String,
    host: Option<String>,
    reason: String,
    by: Option<String>,
    dry_run: bool,
) -> Result<i32> {
    let run_state = state::RunState::parse(&state_arg)
        .ok_or_else(|| anyhow!("`{state_arg}` is not running, paused or stopped"))?;
    if let Some(h) = &host {
        store::validate_host(h)?;
    }
    // Fail closed: an edit must be based on content the forge confirmed
    // current in this invocation, or its blob `sha` (and so the PUT below)
    // could target a commit that has already moved on.
    let loaded = ctx.load(Policy::FailClosed)?;
    let source = propose::source::load(&ctx.transport, &ctx.location.repo, &loaded.snapshot)?;
    let by = by.unwrap_or_else(loom_daemon::sweep_registry::host_identity);
    let since = Utc::now().format("%Y-%m-%dT%H:%MZ").to_string();
    let after =
        propose::edit_state(&source.text, host.as_deref(), run_state, &reason, &by, &since)?;
    let who = host.as_deref().unwrap_or("the fleet default");
    let title = format!("fleet-config: set {who} state to {}", run_state.as_str());
    submit_or_print(ctx, &loaded, &source, "state", after, title, dry_run)
}

fn cmd_propose_priority(ctx: &Ctx, repo: String, priority: u32, dry_run: bool) -> Result<i32> {
    let loaded = ctx.load(Policy::FailClosed)?;
    let source = propose::source::load(&ctx.transport, &ctx.location.repo, &loaded.snapshot)?;
    let after = propose::edit_priority(&source.text, &repo, priority)?;
    let title = format!("fleet-config: set {repo} priority to {priority}");
    submit_or_print(ctx, &loaded, &source, "priority", after, title, dry_run)
}

fn cmd_propose_adopt(ctx: &Ctx, host: Option<String>, dry_run: bool) -> Result<i32> {
    let host = resolve_host(host)?;
    let loaded = ctx.load(Policy::FailClosed)?;
    let machine_path = loom_daemon::config_resolver::private_defaults_path().ok_or_else(|| {
        anyhow!(
            "the machine tier is disabled ({} is set to the empty string)",
            loom_daemon::config_resolver::PRIVATE_DEFAULTS_ENV
        )
    })?;
    let local_path = ctx
        .workspace
        .join(loom_daemon::config_resolver::LOCAL_CONFIG_REL);
    let source = propose::source::load(&ctx.transport, &ctx.location.repo, &loaded.snapshot)?;
    let Some(after) =
        propose::adopt::plan(&loaded.snapshot, &source.text, &host, &machine_path, &local_path)?
    else {
        println!("{}", source_line(ctx, &loaded));
        println!("{host}: no drift to adopt — the on-disk config already matches the store");
        return Ok(0);
    };
    let title = format!("fleet-config: adopt {host}'s local drift");
    submit_or_print(ctx, &loaded, &source, "adopt", after, title, dry_run)
}

/// Common tail of every `propose` sub-verb: print the branch/title/diff
/// report, then either stop there (`--dry-run`) or open the PR.
///
/// A sub-verb whose edit turned out to be a no-op (the store already says
/// what it was asked to say) stops here with nothing proposed: an empty PR
/// is noise for the operator who has to review it.
///
/// Otherwise the edited `fleet.yml` is rendered with the store's own
/// renderer, and the PR carries it with every render it changes, so it
/// passes the store's `validate` (#10905). When the renderer cannot run
/// here, the PR carries `fleet.yml` alone and its body says so.
fn submit_or_print(
    ctx: &Ctx,
    loaded: &Loaded,
    source: &propose::source::Source,
    kind: &str,
    after: String,
    title: String,
    dry_run: bool,
) -> Result<i32> {
    let mut files = propose::drop_unchanged(vec![source.change(after.clone())]);
    if files.is_empty() {
        println!("{}", source_line(ctx, loaded));
        println!("nothing to propose — the store already matches this change");
        return Ok(0);
    }
    let renders = match propose::source::render(&ctx.transport, source, &loaded.snapshot, &after)? {
        propose::source::Rendered::Files(rendered) => {
            files.extend(propose::drop_unchanged(rendered));
            format!(
                "`{}` and every file it renders to were regenerated with the store's own `{}`.",
                propose::source::SOURCE_PATH,
                propose::source::RENDERER
            )
        }
        propose::source::Rendered::Skipped(why) => {
            eprintln!(
                "warning: renders not regenerated ({why}); run `python3 {}` on the PR branch \
                 before it can pass the store's validate check",
                propose::source::RENDERER
            );
            format!(
                "**Renders not regenerated** ({why}): run `python3 {}` on this branch and push \
                 the result, or the store's `validate` check fails (`fleet-stale`).",
                propose::source::RENDERER
            )
        }
    };
    let stamp = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let branch = propose::branch_name(kind, &stamp);
    let marker = propose::provenance_marker(&ctx.workspace, &loaded.snapshot.manifest.commit);
    let body = format!(
        "Opened by `loom-daemon fleet-config propose {kind}` (see `daemon-reference.md` § \
         \"Fleet store\" for the store's file contract). Merges stay the operator's — this \
         command only ever proposes.\n\n{renders}\n\n{marker}\n"
    );
    let proposal = propose::Proposal {
        branch,
        title,
        body,
        files,
    };
    println!("{}", source_line(ctx, loaded));
    print!("{}", proposal.report());
    if dry_run {
        println!("(--dry-run: no PR opened)");
        return Ok(0);
    }
    let pr = propose::submit(
        &ctx.transport,
        &ctx.location.repo,
        &ctx.location.reference,
        &loaded.snapshot.manifest.commit,
        &proposal,
    )?;
    match (pr.number, pr.url) {
        (Some(n), Some(url)) => println!("opened PR #{n}: {url}"),
        (_, Some(url)) => println!("opened PR: {url}"),
        _ => println!("opened the PR (the forge did not echo its number/URL back)"),
    }
    Ok(0)
}
