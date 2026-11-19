//! The `loom:cargo-shared-target` half of the guard (issue #10646): deny a
//! cargo build in a Loom-managed worktree falls through to a shared
//! `build.target-dir`.
//!
//! The deny half ([`super::evaluate`]) judges only an *explicit* target dir and
//! allows "unset", because unset normally means `<worktree>/target`. On a host
//! whose `~/.cargo/config.toml` sets `build.target-dir`, it does not: a plain
//! `cargo test` in a worktree builds into the host's shared dir, where its
//! path-keyed workspace-crate artifacts outlive the worktree and nothing
//! reclaims them (427 GB of `loom_daemon` on one host). The spawn path exports a
//! per-worktree `CARGO_TARGET_DIR`; an in-session Task subagent never passes
//! through it, so this is where that case is caught.
//!
//! The verdict is a **deny** (#11625), not an ask: a headless run cannot answer
//! an ask, and Claude Code shows an ask's reason to the human, not to the
//! model, so only a deny lets the agent read the `CARGO_TARGET_DIR` prefix and
//! rerun. An operator who shares a target dir on purpose (with sccache, say)
//! turns the check off with its own toggle, `guards.cargoSharedTarget` /
//! `LOOM_GUARD_CARGO_SHARED_TARGET`.

use std::path::{Path, PathBuf};

use super::{forms, normalize, under, Context};

/// The category toggle (`guards.cargoSharedTarget`, default on).
pub const TOGGLE_KEY: &str = "guards.cargoSharedTarget";
/// The toggle's env override (Claude Code settings `env`).
pub const TOGGLE_ENV: &str = "LOOM_GUARD_CARGO_SHARED_TARGET";
/// Every denial starts with this; the hook matches on it.
pub const BLOCKED_PREFIX: &str = "BLOCKED [cargo-shared-target]";
/// The sentinel `worktree.sh` writes at every Loom-managed worktree root.
pub const MANAGED_SENTINEL: &str = ".loom-managed";

/// Where cargo's configured `build.target-dir` came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// The `CARGO_BUILD_TARGET_DIR` environment variable.
    Env,
    /// `build.target-dir` in this config file.
    File(PathBuf),
}

/// cargo's configured `build.target-dir` for a cwd, resolved to a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Configured {
    pub dir: PathBuf,
    pub origin: Origin,
}

/// One ask.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedAsk {
    pub target: PathBuf,
    pub origin: Origin,
    pub worktree: PathBuf,
    /// The `CARGO_TARGET_DIR` value to use instead.
    pub use_instead: PathBuf,
}

/// `build.target-dir`'s quoted value in a cargo config body: `target-dir` in
/// the `[build]` table, or a top-level dotted `build.target-dir`. Not a TOML
/// parser; a form it misses costs a missed ask, never a wrong one.
#[must_use]
pub fn build_target_dir_value(body: &str) -> Option<String> {
    let mut table: Option<String> = None;
    for line in body.lines() {
        let line = line.trim();
        if let Some(header) = line.strip_prefix('[') {
            table = Some(header.split(']').next().unwrap_or("").trim().to_string());
            continue;
        }
        let Some((key, rhs)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().replace(['"', ' '], "");
        let hit = match table.as_deref() {
            None => key == "build.target-dir",
            Some("build") => key == "target-dir",
            Some(_) => false,
        };
        if !hit {
            continue;
        }
        let rhs = rhs.trim();
        let quote = rhs.chars().next().filter(|c| matches!(c, '"' | '\''))?;
        let rest = &rhs[1..];
        let value = &rest[..rest.find(quote)?];
        return (!value.is_empty()).then(|| value.to_string());
    }
    None
}

/// cargo's own precedence for `build.target-dir`, minus `CARGO_TARGET_DIR`
/// and `--target-dir` (the caller has already found neither):
/// `CARGO_BUILD_TARGET_DIR`, then the closest `.cargo/config.toml` (or legacy
/// `.cargo/config`) from `cwd` upward, then `$CARGO_HOME`'s. A relative config
/// value is anchored at the parent of the `.cargo` dir holding it, as cargo
/// does; a relative env value at `cwd`.
#[must_use]
pub fn configured_target(
    cwd: &Path,
    build_env: Option<&str>,
    cargo_home: Option<&Path>,
    read: &dyn Fn(&Path) -> Option<String>,
) -> Option<Configured> {
    if let Some(v) = build_env.filter(|v| !v.is_empty()) {
        return Some(Configured {
            dir: normalize(&cwd.join(v)),
            origin: Origin::Env,
        });
    }
    let from_dir = |cargo_dir: &Path| {
        ["config.toml", "config"].iter().find_map(|name| {
            let file = cargo_dir.join(name);
            let value = build_target_dir_value(&read(&file)?)?;
            let anchor = cargo_dir.parent().unwrap_or(cargo_dir);
            Some(Configured {
                dir: normalize(&anchor.join(value)),
                origin: Origin::File(file),
            })
        })
    };
    cwd.ancestors()
        .find_map(|d| from_dir(&d.join(".cargo")))
        .or_else(|| cargo_home.and_then(from_dir))
}

/// The process's `configured_target` for `cwd`, computed only when `worktree`
/// is a Loom-managed worktree (no other cwd can ask).
#[must_use]
pub fn for_process(cwd: &Path, worktree: Option<&Path>, home: Option<&Path>) -> Option<Configured> {
    worktree.filter(|wt| wt.join(MANAGED_SENTINEL).is_file())?;
    let cargo_home = std::env::var_os("CARGO_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.map(|h| h.join(".cargo")));
    let build_env = std::env::var("CARGO_BUILD_TARGET_DIR").ok();
    configured_target(cwd, build_env.as_deref(), cargo_home.as_deref(), &|p| {
        std::fs::read_to_string(p).ok()
    })
}

/// The ask for a cargo build with no explicit target dir, in `ctx`: `Some`
/// when cargo's configured target dir lies outside the managed worktree, its
/// per-worktree dir and `<repo>/.loom/targets/`.
#[must_use]
pub fn judge(ctx: &Context) -> Option<SharedAsk> {
    let configured = ctx.configured.as_ref()?;
    let worktree = ctx.worktree.as_ref()?;
    let target = forms(&configured.dir);
    let owned = ctx
        .main_root
        .iter()
        .map(|m| m.join(".loom/targets"))
        .chain(std::iter::once(worktree.clone()))
        .chain(ctx.owned.iter().cloned());
    for root in owned {
        if under(&target, &root) {
            return None;
        }
    }
    let use_instead = ctx
        .owned
        .first()
        .cloned()
        .unwrap_or_else(|| worktree.join("target"));
    Some(SharedAsk {
        target: configured.dir.clone(),
        origin: configured.origin.clone(),
        worktree: worktree.clone(),
        use_instead,
    })
}

/// The denial text the hook passes through verbatim.
#[must_use]
pub fn ask_reason(a: &SharedAsk) -> String {
    let from = match &a.origin {
        Origin::Env => "CARGO_BUILD_TARGET_DIR".to_string(),
        Origin::File(f) => format!("build.target-dir in {}", f.display()),
    };
    format!(
        "{BLOCKED_PREFIX}: this cargo command sets no CARGO_TARGET_DIR, so it will build into '{}' \
         (from {from}), outside this Loom worktree ({}). Nothing reclaims a worktree's \
         artifacts there once the worktree is gone, and concurrent worktree builds can clobber \
         each other's binaries (#10646). Prefix the command instead: CARGO_TARGET_DIR=\"{}\" \
         cargo …. If sharing that dir is deliberate, turn this check off: \
         {TOGGLE_ENV}=0 in Claude Code's settings env, or guards.cargoSharedTarget=false.",
        a.target.display(),
        a.worktree.display(),
        a.use_instead.display(),
    )
}

#[cfg(test)]
#[path = "shared_tests.rs"]
mod tests;
