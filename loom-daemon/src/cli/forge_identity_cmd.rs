//! `loom-daemon forge token | is-fleet | identities` (#9537): the scripts'
//! entry point to the forge identity broker ([`loom_daemon::forge_identity`]).

use std::path::{Path, PathBuf};

use anyhow::Result;
use loom_daemon::credential_preflight::{GithubAppMinter, GithubAppOutcome};
use loom_daemon::forge_identity::{self, Identity, IdentityMinter, Roster};
use serde_json::{json, Value};

/// The workspace the command runs for: the MAIN checkout of the cwd's repo
/// (so a linked worktree still sees the workspace's local config tier and its
/// published reader tokens), else the cwd.
fn workspace() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let common = std::process::Command::new("git")
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .current_dir(&cwd)
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()));
    if let Some(parent) = common
        .as_deref()
        .filter(|c| c.ends_with(".git"))
        .and_then(Path::parent)
    {
        return parent.to_path_buf();
    }
    std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(&cwd)
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()))
        .unwrap_or(cwd)
}

fn mint_as(ws: &Path, identity: &Identity, repo: &str, force: bool) -> GithubAppOutcome {
    let Some(script_path) = loom_daemon::credential_preflight::resolve_github_app_script(ws) else {
        return GithubAppOutcome::Error("github-app-token.sh not found in this workspace".into());
    };
    let minter = IdentityMinter {
        script_path,
        cwd: ws.to_path_buf(),
        identity: identity.clone(),
    };
    if force {
        minter.mint_forced(repo)
    } else {
        minter.mint(repo)
    }
}

fn minted_json(
    outcome: GithubAppOutcome,
    identity: &Identity,
    access: &str,
) -> Result<Value, String> {
    match outcome {
        GithubAppOutcome::Minted {
            token,
            installation_id,
            app_id,
            expires_at,
        } => Ok(json!({
            "status": "ok",
            "token": token,
            "installation_id": installation_id,
            "app_id": app_id,
            "slug": identity.slug,
            "access": access,
            "expires_at": expires_at,
        })),
        GithubAppOutcome::Error(reason) => Err(reason),
        GithubAppOutcome::NotConfigured => Err("not configured".into()),
    }
}

/// `forge token`.
pub(crate) fn token(repo: &str, access: &str, force: bool) -> Result<()> {
    let access = match access {
        "read" | "write" => access,
        other => {
            println!(
                "{}",
                json!({"status": "error", "message": format!("--access must be read or write, not {other}")})
            );
            return Ok(());
        }
    };
    let ws = workspace();
    let roster = forge_identity::resolve(&ws);
    let mut fallback_reason = None;
    if access == "read" {
        match forge_identity::reader_for(&roster, repo) {
            Some(reader) => match minted_json(mint_as(&ws, reader, repo, force), reader, "read") {
                Ok(v) => {
                    println!("{v}");
                    return Ok(());
                }
                Err(reason) => {
                    fallback_reason =
                        Some(format!("reader {} could not mint: {reason}", reader.app_id))
                }
            },
            None => fallback_reason = Some("no reader configured".to_string()),
        }
    }
    let Some(writer) = roster.writer.as_ref() else {
        println!("{}", json!({"status": "not_configured", "access": access}));
        return Ok(());
    };
    match minted_json(mint_as(&ws, writer, repo, force), writer, "write") {
        Ok(mut v) => {
            if let Some(r) = fallback_reason {
                v["fallback_reason"] = json!(r);
            }
            println!("{v}");
        }
        Err(reason) => {
            println!("{}", json!({"status": "error", "access": access, "message": reason}))
        }
    }
    Ok(())
}

/// `forge is-fleet`.
pub(crate) fn is_fleet(login: &str) -> Result<()> {
    let roster = forge_identity::resolve(&workspace());
    match forge_identity::role_of(&roster, login) {
        Some(role) => {
            println!("{role}");
            std::process::exit(0)
        }
        None => std::process::exit(1),
    }
}

/// Every reader token published under `ws`, as `(owner, app id, expires_at)`.
fn published(ws: &Path, roster: &Roster) -> Vec<(String, String, String)> {
    let base = ws.join(".loom").join("gh-config-by-owner");
    let mut out = Vec::new();
    let Ok(owners) = std::fs::read_dir(&base) else {
        return out;
    };
    for owner in owners.flatten() {
        let owner_name = owner.file_name().to_string_lossy().to_string();
        for reader in &roster.readers {
            let dir = owner.path().join(&reader.app_id);
            if let Some(side) = forge_identity::read_sidecar(&dir) {
                out.push((owner_name.clone(), reader.app_id.clone(), side.expires_at));
            }
        }
    }
    out.sort();
    out
}

/// `forge identities`.
pub(crate) fn identities(as_json: bool) -> Result<()> {
    let ws = workspace();
    let roster = forge_identity::resolve(&ws);
    let tokens = published(&ws, &roster);
    let fleet = forge_identity::FleetLogins::of(&roster).names();
    if as_json {
        let tokens: Vec<Value> = tokens
            .iter()
            .map(|(o, a, e)| json!({"owner": o, "appId": a, "expiresAt": e}))
            .collect();
        println!("{}", json!({"roster": roster, "fleetLogins": fleet, "readerTokens": tokens}));
        return Ok(());
    }
    let show =
        |i: &Identity| format!("{} ({})", i.slug.as_deref().unwrap_or("<no slug>"), i.app_id);
    println!(
        "writer:  {}",
        roster
            .writer
            .as_ref()
            .map_or_else(|| "<none: ambient gh auth>".into(), show)
    );
    if roster.readers.is_empty() {
        println!("readers: <none: reads use the writer>");
    }
    for r in &roster.readers {
        println!("reader:  {}", show(r));
    }
    println!("fleet logins: {}", fleet.join(", "));
    for (owner, app, exp) in &tokens {
        println!("  reader token {app} for {owner}: expires {exp}");
    }
    Ok(())
}
