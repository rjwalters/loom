//! Opt-in consumer canary for the uncommitted-work `Stop` guard (Issue #8372).
//!
//! # Why a separate entry-point gate
//!
//! `worktree-state stop-hook` (#8267) is wired in Loom's own project-level
//! `.claude/settings.json` only. Consumer repos get their hooks from the
//! user-scope wiring in `scripts/install/provision-hooks.sh`, which now carries a
//! thin `defaults/hooks/guard-uncommitted-work.sh` stub that runs
//! `worktree-state stop-hook --consumer-canary`. That wiring reaches every Loom
//! workspace on the host, so it must do nothing unless the workspace explicitly
//! asked for it.
//!
//! The existing `guards.uncommittedWork` switch cannot be that gate: it is
//! default-ON (an absent key means enabled), so relying on it would enable a new
//! blocking turn-end guard fleet-wide by default. This module adds a second,
//! default-OFF key, [`CANARY_CONFIG_KEY`], that only the consumer entry point
//! reads. Both must agree for a consumer block: the canary key opts the
//! workspace in, and `guards.uncommittedWork: false` still disables the guard
//! inside an opted-in workspace. The dogfood invocation (no flag) never reads
//! the canary key and is unchanged.
//!
//! # Measurement
//!
//! "No correction reports" does not establish a false-positive rate, so every
//! opted-in invocation appends one structured record to a bounded JSONL log
//! outside every checkout (see [`outcome_log_path`]). A record names the
//! decision, the event, the session, the owned worktree, and the measured
//! worktree state that drove the decision — never transcript contents and never
//! file contents. Wrapper failures (missing binary, a binary too old for the
//! flag) are recorded by the shell stub with `"source":"wrapper"`, so they stay
//! visible without entering the detection denominator.
//!
//! # Failure mode: always allow
//!
//! Exactly as for the dogfood guard. A log write that fails never changes the
//! decision; it is surfaced as a `systemMessage` in the session instead, so an
//! unrecorded decision is visible as a coverage gap and is never counted as
//! measured.

use std::io::Write;
use std::path::{Path, PathBuf};

use super::stop_hook::{self, Decision, HookPayload};

/// The default-OFF opt-in key. Only an explicit boolean `true` enables it.
pub const CANARY_CONFIG_KEY: &str = "guards.uncommittedWorkConsumerCanary";

/// Overrides the outcome log location (an empty value means "use the default").
pub const OUTCOME_LOG_ENV: &str = "LOOM_UNCOMMITTED_WORK_CANARY_LOG";

/// Default log location relative to `$HOME`, outside every repository.
pub const OUTCOME_LOG_REL: &str = ".loom/logs/uncommitted-work-canary.jsonl";

/// Rotate the live log once it reaches this size …
pub const OUTCOME_LOG_MAX_BYTES: u64 = 1024 * 1024;

/// … keeping at most this many rotated generations (`.1` … `.N`). Total disk
/// use is therefore bounded at roughly `(N + 1) * OUTCOME_LOG_MAX_BYTES`.
pub const OUTCOME_LOG_KEEP: usize = 4;

/// Record schema version. Bump on any field rename or meaning change.
pub const SCHEMA_VERSION: u32 = 1;

/// Env var the stub uses to hand the daemon a correlation id, so a wrapper
/// record and a daemon record for the same hook invocation can be joined.
pub const INVOCATION_ID_ENV: &str = "LOOM_HOOK_INVOCATION_ID";

/// The workspace whose config decides the canary: the main checkout owning
/// `start` (so the gitignored `.loom-local/local.json` tier is honoured from a
/// linked worktree, the #4273 trap), else `start` itself.
#[must_use]
pub fn workspace_root(start: &Path) -> PathBuf {
    super::main_checkout_root(start).unwrap_or_else(|| start.to_path_buf())
}

/// Whether `guards.uncommittedWorkConsumerCanary` is explicitly `true` in the
/// effective config of `root`. Absent, `false`, or any non-boolean is OFF. No
/// env override on purpose: the opt-in is a per-workspace decision recorded in
/// config, where a reviewer of the canary sample can see it.
#[must_use]
pub fn canary_enabled(root: &Path) -> bool {
    let config = crate::config_resolver::resolve_effective_config(root);
    matches!(
        crate::config_resolver::get_path(&config, CANARY_CONFIG_KEY),
        Some(serde_json::Value::Bool(true))
    )
}

/// Where outcome records go: `$LOOM_UNCOMMITTED_WORK_CANARY_LOG` (rejected by
/// [`run`] when it lies inside a Git checkout or worktree), else
/// `~/.loom/logs/uncommitted-work-canary.jsonl`. `None` only when no home
/// directory can be determined (then nothing can be recorded — a coverage gap,
/// reported as such).
#[must_use]
pub fn outcome_log_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var(OUTCOME_LOG_ENV) {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    dirs::home_dir().map(|h| h.join(OUTCOME_LOG_REL))
}

/// The `outcome` field for a decision.
#[must_use]
pub fn outcome_name(decision: &Decision) -> &'static str {
    match decision {
        Decision::Silent => "allow",
        Decision::Advise(_) => "advisory",
        Decision::Block(_) => "block",
    }
}

/// Everything a record needs that is not in the payload.
#[derive(Debug, Clone)]
pub struct RecordContext<'a> {
    pub invocation_id: &'a str,
    pub workspace: &'a Path,
}

/// Build the outcome record for one opted-in invocation.
///
/// Carries references (session id, transcript *path*, worktree path) and the
/// measured counts/paths that drove the decision — enough to classify a block
/// against the worktree state at decision time — and nothing else. The block
/// reason text is deliberately omitted: it is derived from these fields.
#[must_use]
pub fn build_record(
    ctx: &RecordContext<'_>,
    payload: &HookPayload,
    eval: Option<&stop_hook::Evaluation>,
    outcome: &str,
    error: Option<&str>,
) -> serde_json::Value {
    let mut rec = serde_json::json!({
        "schema": SCHEMA_VERSION,
        "ts": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        "source": "daemon",
        "version": env!("CARGO_PKG_VERSION"),
        "sha": crate::self_update::BUILT_COMMIT_FULL,
        "event": payload.hook_event_name,
        "invocation_id": ctx.invocation_id,
        "session_id": payload.session_id,
        "transcript_path": payload.transcript_path,
        "cwd": payload.cwd,
        "workspace": ctx.workspace.display().to_string(),
        "stop_hook_active": payload.stop_hook_active,
        "outcome": outcome,
    });
    let obj = rec.as_object_mut().expect("json! object literal");
    if let Some(err) = error {
        obj.insert("error".into(), serde_json::Value::from(err));
    }
    if let Some(eval) = eval {
        obj.insert(
            "worktree".into(),
            serde_json::Value::from(eval.worktree.as_ref().map(|p| p.display().to_string())),
        );
        obj.insert("guard_enabled".into(), serde_json::Value::from(eval.guard_enabled));
        if let Some(state) = &eval.state {
            obj.insert("state".into(), state.to_json());
        }
    }
    rec
}

/// Append one record to `log`, rotating first so the file stays bounded.
///
/// # Errors
///
/// A human-readable reason when the record could not be written. The caller
/// must surface it (the record is then a coverage gap, never "measured").
pub fn append_record(log: &Path, record: &serde_json::Value) -> Result<(), String> {
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    // A rotation failure must not degrade into unbounded growth: refuse to
    // append rather than write past the bound.
    crate::rotate_log_file(log, OUTCOME_LOG_MAX_BYTES, OUTCOME_LOG_KEEP)
        .map_err(|e| format!("rotate {}: {e}", log.display()))?;
    let mut line = record.to_string();
    line.push('\n');
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .map_err(|e| format!("open {}: {e}", log.display()))?;
    // One write call for the whole line keeps concurrent appenders (several
    // sessions ending at once) from interleaving inside a record.
    f.write_all(line.as_bytes())
        .map_err(|e| format!("write {}: {e}", log.display()))
}

/// `log` made absolute with its deepest existing ancestor canonicalised, so a
/// symlink or `..` cannot smuggle a path into a checkout.
fn resolve_log_path(log: &Path) -> PathBuf {
    let abs = std::path::absolute(log).unwrap_or_else(|_| log.to_path_buf());
    let mut tail = Vec::new();
    let mut cur = abs.clone();
    loop {
        if let Ok(mut base) = cur.canonicalize() {
            base.extend(tail.iter().rev());
            return base;
        }
        match (cur.file_name().map(std::ffi::OsStr::to_os_string), cur.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name);
                cur = parent.to_path_buf();
            }
            _ => return abs,
        }
    }
}

/// The Git checkout or worktree root that contains `log`, if any. The outcome
/// log carries session/transcript/worktree attribution and must stay outside
/// every repository. The walk stops at `$HOME` (a dotfiles repo there must not
/// disqualify the default `~/.loom/logs/…` location).
fn containing_checkout(log: &Path) -> Option<PathBuf> {
    let home = dirs::home_dir().map(|h| h.canonicalize().unwrap_or(h));
    resolve_log_path(log)
        .ancestors()
        .skip(1)
        .take_while(|d| Some(*d) != home.as_deref())
        .find(|d| d.join(".git").exists())
        .map(Path::to_path_buf)
}

/// The hook output when a record could not be written: a NON-blocking coverage
/// gap notice. The decision is dropped (fail open) — a canary that cannot
/// record must never be the reason a session cannot stop — but any
/// `systemMessage` the decision carried is kept, so an advisory still shows.
fn gap_notice(out: Option<serde_json::Value>, why: &str) -> serde_json::Value {
    let notice = format!(
        "uncommitted-work consumer canary (#8372): this decision was NOT recorded ({why}); \
         the canary sample for this window is incomplete."
    );
    let msg = match out
        .as_ref()
        .and_then(|o| o.get("systemMessage"))
        .and_then(|m| m.as_str())
    {
        Some(existing) => format!("{existing}\n{notice}"),
        None => notice,
    };
    serde_json::json!({ "systemMessage": msg })
}

/// End-to-end for `stop-hook --consumer-canary`: raw stdin in, hook JSON out.
///
/// * `fallback_cwd` — where to resolve the workspace when the payload carries
///   no usable `cwd` (a malformed payload): the hook process's own cwd.
/// * `log` — the outcome log (`None` = cannot record; reported as a gap).
/// * `invocation_id` — correlation id (the stub's, or a fresh one).
///
/// Returns `None` for silence. Never panics on bad input; every failure is an
/// allow.
#[must_use]
pub fn run(
    raw: &str,
    base_ref: &str,
    fallback_cwd: &Path,
    log: Option<&Path>,
    invocation_id: &str,
) -> Option<serde_json::Value> {
    let parsed = serde_json::from_str::<HookPayload>(raw);
    let malformed = parsed.is_err();
    let payload = parsed.unwrap_or_default();

    let start = payload
        .cwd
        .as_deref()
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| fallback_cwd.to_path_buf());
    let workspace = workspace_root(&start);

    // THE GATE. Not opted in -> silent, and nothing is recorded: a workspace
    // that never asked for the canary leaves no trace of it.
    if !canary_enabled(&workspace) {
        return None;
    }

    let ctx = RecordContext {
        invocation_id,
        workspace: &workspace,
    };

    let (out, record) = if malformed {
        // Fail open; recorded as an error so it is excluded from the
        // detection denominator but still counted as an invocation.
        (None, build_record(&ctx, &payload, None, "error", Some("malformed_payload")))
    } else {
        let eval = stop_hook::evaluate_detailed(&payload, base_ref);
        let out = eval.decision.to_hook_json();
        let rec = build_record(&ctx, &payload, Some(&eval), outcome_name(&eval.decision), None);
        (out, rec)
    };

    let written = match log {
        Some(path) => match containing_checkout(path) {
            Some(root) => Err(format!(
                "outcome log {} is inside the Git checkout {}; it must stay outside every repository",
                path.display(),
                root.display()
            )),
            None => append_record(path, &record),
        },
        None => Err("no home directory for the outcome log".to_string()),
    };
    match written {
        Ok(()) => out,
        Err(why) => {
            eprintln!("worktree-state stop-hook --consumer-canary: outcome not recorded: {why}");
            Some(gap_notice(out, &why))
        }
    }
}

#[cfg(test)]
#[path = "consumer_canary_tests.rs"]
mod tests;
