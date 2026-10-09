//! `loom-daemon forge token | is-fleet | identities | trusted-comments |
//! verdict-stale-notice` (#9537, #9548, #9709): the scripts' entry point to the
//! forge identity broker ([`loom_daemon::forge_identity`]) and the
//! comment-trust predicate ([`loom_daemon::comment_trust`]).

use std::path::{Path, PathBuf};

use anyhow::Result;
use loom_daemon::credential_preflight::{GithubAppMinter, GithubAppOutcome};
use loom_daemon::forge_identity::{self, Identity, IdentityMinter, Roster};
use serde_json::{json, Value};

/// The workspace the command runs for: the MAIN checkout of the cwd's repo
/// (so a linked worktree still sees the workspace's local config tier and its
/// published reader tokens), else the cwd.
pub(super) fn workspace() -> PathBuf {
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
    let warnings = forge_identity::config_warnings_for(&ws);
    if as_json {
        let tokens: Vec<Value> = tokens
            .iter()
            .map(|(o, a, e)| json!({"owner": o, "appId": a, "expiresAt": e}))
            .collect();
        println!(
            "{}",
            json!({"roster": roster, "fleetLogins": fleet, "readerTokens": tokens, "warnings": warnings})
        );
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
    for w in &warnings {
        println!("WARNING: {w}");
    }
    for (owner, app, exp) in &tokens {
        println!("  reader token {app} for {owner}: expires {exp}");
    }
    Ok(())
}

/// `forge trusted-comments [--self-login L] [--fetch N [--repo R]
/// [--with-body]] [--gh-shape]` (#9548): the input is a comment listing
/// (stdin, or issue/PR N's REST listing with `--fetch`) in REST or `gh
/// --json` shape (one array, concatenated pages, or an object with
/// `comments`/`reviews`); stdout is the same JSON holding only the comments
/// whose author [`loom_daemon::comment_trust`] trusts. Exits 1 with nothing
/// on stdout when the input is not such a document (including empty stdin)
/// or cannot be fetched, so a caller can never mistake a failed filter for
/// "no trusted comments".
pub(crate) fn trusted_comments(
    self_login: Option<String>,
    fetch: Option<(u64, Option<String>, bool)>,
    gh_shape: bool,
) -> Result<()> {
    use std::io::Read;
    let root = workspace();
    let input = match fetch {
        Some((n, repo, with_body)) => fetch_listing(&root, n, repo.as_deref(), with_body),
        None => {
            let mut input = Vec::new();
            std::io::stdin().read_to_end(&mut input)?;
            Some(input)
        }
    };
    let policy =
        loom_daemon::comment_trust::TrustPolicy::for_root(&root).with_self_login(self_login);
    match input.and_then(|i| loom_daemon::comment_trust::filter_document(&policy, &i)) {
        Some(Value::Array(items)) if gh_shape => {
            println!("{}", Value::Array(items.iter().map(gh_comment).collect()));
            Ok(())
        }
        Some(out) => {
            println!("{out}");
            Ok(())
        }
        None => {
            eprintln!(
                "forge trusted-comments: no JSON comment listing to filter (stdin empty or not \
                 an array of comments / an object with comments/reviews, or --fetch failed)"
            );
            std::process::exit(1)
        }
    }
}

/// `forge verdict-stale-notice` (#9709): the stale-verdict notice for the
/// shell guard, from the template the daemon pass uses, attributed against the
/// raw comment listing on stdin under this workspace's trust policy.
pub(crate) fn verdict_stale_notice(
    label: &str,
    marker_sha: &str,
    head_sha: &str,
    source: &str,
) -> Result<()> {
    use loom_daemon::verdict_stale_notice as notice;
    use std::io::Read;
    let Some(kind) = notice::kind_for_label(label) else {
        eprintln!("forge verdict-stale-notice: {label:?} is not a terminal verdict label");
        std::process::exit(1)
    };
    let mut input = Vec::new();
    let untrusted = std::io::stdin()
        .read_to_end(&mut input)
        .ok()
        .and_then(|_| loom_daemon::comment_trust::parse_listing(&input))
        .and_then(|items| {
            let policy = loom_daemon::comment_trust::TrustPolicy::for_root(&workspace());
            notice::untrusted_newer_marker(&policy, &items, kind)
        });
    println!("{}", notice::body(label, marker_sha, head_sha, "", untrusted.as_ref(), source));
    Ok(())
}

/// Issue/PR `n`'s REST comment listing, with the issue/PR object itself first
/// when `with_body` (so the trust filter decides whether its body counts).
fn fetch_listing(root: &Path, n: u64, repo: Option<&str>, with_body: bool) -> Option<Vec<u8>> {
    let repo = repo.unwrap_or("{owner}/{repo}");
    let get = |path: String| {
        let out = loom_daemon::script_helpers::run_gh(&["api", &path, "--paginate"], root, false);
        out.ok_output().map(|o| o.stdout.clone())
    };
    let comments = get(format!("repos/{repo}/issues/{n}/comments"))?;
    if !with_body {
        return Some(comments);
    }
    let issue: Value = serde_json::from_slice(&get(format!("repos/{repo}/issues/{n}"))?).ok()?;
    let mut items = vec![issue];
    items.extend(loom_daemon::comment_trust::parse_listing(&comments)?);
    serde_json::to_vec(&items).ok()
}

/// `forge promotion-gate` (#10827): the author gate on automatic promotion,
/// as `KEY=VALUE` lines. Exit 0 for `ELIGIBLE`, 1 for `HOLD`, 3 for
/// `UNAVAILABLE`; callers read the `GATE=` line.
pub(crate) fn promotion_gate(issue: u64, repo: Option<&str>) -> Result<()> {
    use loom_daemon::comment_trust::promotion_gate as gate;
    let out = gate::evaluate(&workspace(), repo.unwrap_or("{owner}/{repo}"), issue);
    let one_line = |s: &str| s.replace(['\n', '\r'], " ");
    println!("GATE={}", out.gate.word());
    println!("REASON={}", one_line(out.gate.reason()));
    println!("NOTICE={}", out.notice.word());
    if out.notice == gate::Notice::Needed {
        println!("NOTICE_BODY={}", one_line(&gate::notice_body(&out.gate)));
    }
    std::process::exit(match out.gate {
        gate::Gate::Eligible(_) => 0,
        gate::Gate::Hold(_) => 1,
        gate::Gate::Unavailable(_) => 3,
    })
}

/// One REST comment in the `gh --json comments` field names.
fn gh_comment(c: &Value) -> Value {
    json!({
        "author": {"login": c.pointer("/user/login").or_else(|| c.pointer("/author/login"))},
        "authorAssociation": c.get("author_association").or_else(|| c.get("authorAssociation")),
        "body": c.get("body"),
        "createdAt": c.get("created_at").or_else(|| c.get("createdAt")),
    })
}

/// `forge may-write [--repo R]` (#9548): exit 0 printing the repository to
/// name on the write, or exit 1 with the refusal on stderr. Never returns.
pub(crate) fn may_write(repo: Option<String>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    match loom_daemon::write_scope::may_write_from(&cwd, repo.as_deref()) {
        loom_daemon::write_scope::Verdict::Allow(nwo) => {
            println!("{nwo}");
            std::process::exit(0)
        }
        loom_daemon::write_scope::Verdict::Deny(why) => {
            eprintln!("{why}");
            std::process::exit(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::gh_comment;
    use serde_json::json;

    /// `--gh-shape` renames the REST fields and keeps the App spelling.
    #[test]
    fn gh_comment_renames_rest_fields() {
        let rest = json!({
            "user": {"login": "loom-fleet-dispatch[bot]", "type": "Bot"},
            "author_association": "NONE",
            "body": "<!-- champion:merge-risk-hold -->",
            "created_at": "2026-09-29T00:00:00Z",
        });
        let out = gh_comment(&rest);
        assert_eq!(out["author"]["login"], "loom-fleet-dispatch[bot]");
        assert_eq!(out["authorAssociation"], "NONE");
        assert_eq!(out["body"], "<!-- champion:merge-risk-hold -->");
        assert_eq!(out["createdAt"], "2026-09-29T00:00:00Z");
        let gh = json!({"author": {"login": "x"}, "authorAssociation": "OWNER", "body": "b", "createdAt": "t"});
        assert_eq!(gh_comment(&gh), gh);
    }
}
