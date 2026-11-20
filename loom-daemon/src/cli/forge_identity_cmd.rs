//! `loom-daemon forge token | is-fleet | identities | trusted-comments |
//! verdict-stale-notice` (#9537, #9548, #9709): the scripts' entry point to the
//! forge identity broker ([`crate::forge_identity`]) and the
//! comment-trust predicate ([`crate::comment_trust`]).

use std::path::{Path, PathBuf};

use crate::credential_preflight::{GithubAppMinter, GithubAppOutcome};
use crate::forge_identity::{self, Identity, IdentityMinter, Roster};
use anyhow::Result;
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
    let Some(script_path) = crate::credential_preflight::resolve_github_app_script(ws) else {
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
    let owner = repo.split_once('/').map_or(repo, |(o, _)| o);
    let Some(writer) = forge_identity::writer_for_owner(&roster, owner) else {
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
    for w in &roster.writers {
        println!(
            "writer:  {} for owners {}",
            show(w),
            w.owners
                .as_ref()
                .map_or_else(|| "<none>".into(), |o| o.join(", "))
        );
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
/// whose author [`crate::comment_trust`] trusts. Exits 1 with nothing
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
    let policy = crate::comment_trust::TrustPolicy::for_root(&root).with_self_login(self_login);
    match input.and_then(|i| crate::comment_trust::filter_document(&policy, &i)) {
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

/// `forge claim-liveness` (#10235): the daemon's extra liveness evidence for a
/// PR claim, as one RFC3339 timestamp (or nothing) — see
/// [`crate::claim_reconciliation::extra_liveness_at`]. Exits 1 only on an
/// unparseable `--claimed-at`; a failed forge read prints nothing (fail-safe).
pub(crate) fn claim_liveness(
    number: u32,
    label: &str,
    claimed_at: &str,
    repo: Option<&str>,
) -> Result<()> {
    let Ok(since) = chrono::DateTime::parse_from_rfc3339(claimed_at) else {
        eprintln!("forge claim-liveness: --claimed-at {claimed_at:?} is not RFC3339");
        std::process::exit(1)
    };
    if let Some(at) = crate::claim_reconciliation::extra_liveness_at(
        &workspace(),
        number,
        label,
        since.with_timezone(&chrono::Utc),
        repo,
    ) {
        println!("{}", at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    }
    Ok(())
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
    use crate::verdict_stale_notice as notice;
    use std::io::Read;
    let Some(kind) = notice::kind_for_label(label) else {
        eprintln!("forge verdict-stale-notice: {label:?} is not a terminal verdict label");
        std::process::exit(1)
    };
    let mut input = Vec::new();
    let untrusted = std::io::stdin()
        .read_to_end(&mut input)
        .ok()
        .and_then(|_| crate::comment_trust::parse_listing(&input))
        .and_then(|items| {
            let policy = crate::comment_trust::TrustPolicy::for_root(&workspace());
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
        let out = crate::script_helpers::gh_read(
            "forge_cmd.listing",
            &["api", &path, "--paginate"],
            root,
            false,
        );
        out.ok_output().map(|o| o.stdout.clone())
    };
    let comments = get(format!("repos/{repo}/issues/{n}/comments"))?;
    if !with_body {
        return Some(comments);
    }
    let issue: Value = serde_json::from_slice(&get(format!("repos/{repo}/issues/{n}"))?).ok()?;
    let mut items = vec![issue];
    items.extend(crate::comment_trust::parse_listing(&comments)?);
    serde_json::to_vec(&items).ok()
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
///
/// The probe runs with the per-owner writer route established first
/// ([`crate::write_scope::may_write_decision_cli`], #13929): when a
/// `forge.identities.writers[]` entry covers the target owner, the owner's
/// writer credential is minted, published and registered in-process, so the
/// probe vets the credential `forge token` could mint — not just the ambient
/// one.
pub(crate) fn may_write(repo: Option<String>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let ws = workspace();
    let verdict = crate::write_scope::may_write_decision_cli(
        &cwd,
        repo.as_deref(),
        &crate::write_scope::default_gh(),
        &ws,
        &|identity, nwo| mint_as(&ws, identity, nwo, false),
    );
    match verdict {
        crate::write_scope::Verdict::Allow(nwo) => {
            println!("{nwo}");
            std::process::exit(0)
        }
        crate::write_scope::Verdict::Deny(why) => {
            eprintln!("{why}");
            std::process::exit(1)
        }
    }
}

/// `forge promotion-author --number N [--repo R]` (#10827): the author gate on
/// automatic promotion — see [`crate::promotion_author`]. Prints the
/// verdict token, then the one-time notice body when the issue is held. Exits 1
/// when the issue could not be read, so a caller cannot take silence for trust.
pub(crate) fn promotion_author(number: u64, repo: Option<&str>) -> Result<()> {
    use crate::promotion_author::{classify, notice_body};
    let root = workspace();
    let repo = repo.unwrap_or("{owner}/{repo}");
    let issue =
        crate::comment_trust::records::fetch_issue_object(repo, &number.to_string(), &root, false);
    let policy = crate::comment_trust::TrustPolicy::for_root(&root);
    let mut gate = classify(&policy, issue.as_ref());
    if matches!(gate, crate::promotion_author::Gate::Untrusted(_)) {
        // #13578: a human maintainer's earlier `loom:issue` is a durable
        // adoption. Any read failure keeps the verdict above (fail closed). A
        // withdrawal of the label, or an edit of the body by a non-trusted
        // editor since the adoption, ends it. The reads below need the real
        // `owner/name`: GraphQL does not expand the `gh` placeholder.
        let adopted = issue
            .as_ref()
            .and_then(|i| real_repo(repo, i))
            .and_then(|real| adopter(&policy, &real, number, &root));
        if let Some(who) = adopted {
            eprintln!("promotion-author: adopted by {who} (maintainer `labeled loom:issue`)");
            gate = crate::promotion_author::Gate::Trusted;
        }
    }
    println!("{}", gate.token());
    let text = |key: &str| {
        issue
            .as_ref()
            .and_then(|i| i.get(key))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let stamp = issue.as_ref().map(|_| crate::promotion_author::Stamp {
        rev: crate::promotion_author::revision(&text("title"), &text("body")),
        trust: policy.digest(),
    });
    let body = notice_body(&gate, stamp.as_ref());
    if !body.is_empty() {
        print!("{body}");
    }
    if issue.is_none() {
        std::process::exit(1)
    }
    Ok(())
}

/// Whether `slug` is a real GitHub `owner/name`: the shared slug rule plus
/// GitHub's stricter owner (login) rule. Anything else builds no call, which
/// keeps the verdict `UNTRUSTED`.
fn github_slug(slug: &str) -> bool {
    crate::forge_slug::valid_slug(slug)
        && slug
            .split_once('/')
            .is_some_and(|(owner, _)| crate::forge_bucket_book::valid_owner(owner))
}

/// The real `owner/name` the issue was read from: `repo` itself when it is
/// one, else (the `gh` placeholder `{owner}/{repo}`, which only `gh api` REST
/// paths expand) the repository the fetched `issue` object names in its
/// `repository_url`. `None` when neither is a valid slug, so no later call is
/// built from a placeholder.
fn real_repo(repo: &str, issue: &Value) -> Option<String> {
    if github_slug(repo) {
        return Some(repo.to_string());
    }
    let url = issue.get("repository_url").and_then(Value::as_str)?;
    let (_, slug) = url.rsplit_once("/repos/")?;
    github_slug(slug).then(|| slug.to_string())
}

/// The maintainer whose `loom:issue` adoption of issue `number` still stands,
/// if the whole timeline and the body's edit history could be read (#13578).
/// Either read failing yields `None`. `repo` is a real `owner/name`.
fn adopter(
    policy: &crate::comment_trust::TrustPolicy,
    repo: &str,
    number: u64,
    root: &Path,
) -> Option<String> {
    let path = format!("repos/{repo}/issues/{number}/timeline");
    let out = crate::script_helpers::gh_read(
        "promotion_author.timeline",
        &["api", &path, "--paginate"],
        root,
        false,
    );
    let events = crate::comment_trust::parse_listing(&out.ok_output()?.stdout)?;
    let edits = body_edits(repo, number, root)?;
    // Timeline actors and editors have no association: map write access or
    // better to one. One lookup per login; a failed lookup is no access.
    let seen = std::cell::RefCell::new(std::collections::HashMap::<String, Option<String>>::new());
    crate::promotion_author::standing_adoption(policy, &events, &edits, |login| {
        if let Some(known) = seen.borrow().get(login) {
            return known.clone();
        }
        let out = promotion_permission_invocation(repo, login, root)?.run();
        let access = out.ok_output().and_then(|out| {
            matches!(String::from_utf8_lossy(&out.stdout).trim(), "admin" | "maintain" | "write")
                .then(|| "COLLABORATOR".to_string())
        });
        seen.borrow_mut().insert(login.to_string(), access.clone());
        access
    })
}

/// GitHub evaluates this endpoint under the asking credential. Keep only
/// this permission probe on the writer, following `write_scope::GhProbe`.
fn promotion_permission_invocation(
    repo: &str,
    login: &str,
    root: &Path,
) -> Option<crate::gh_invocation::GhInvocation> {
    use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};
    let path = format!("repos/{repo}/collaborators/{login}/permission");
    Some(
        GhInvocation::new(
            Operation::new("promotion_author.permission"),
            AccessIntent::Read,
            GhTarget::repo(repo).ok()?,
            crate::cmd_out::DEFAULT_TIMEOUT,
        )
        .current_dir(root)
        .writer_identity()
        .args(["api", &path, "--jq", ".permission"]),
    )
}

const BODY_EDITS_QUERY: &str = "query($o:String!,$n:String!,$i:Int!){repository(owner:$o,name:$n){\
    issue(number:$i){userContentEdits(first:100){pageInfo{hasNextPage} \
    nodes{createdAt editedAt editor{login __typename}}}}}}";

/// The `gh` arguments of the edit-history read. `None` unless `repo` is a real
/// `owner/name`: `-f` values reach GraphQL literally, so a placeholder would
/// name no repository.
fn body_edits_args(repo: &str, number: u64) -> Option<Vec<String>> {
    if !github_slug(repo) {
        return None;
    }
    let (owner, name) = repo.split_once('/')?;
    Some(vec![
        "api".to_string(),
        "graphql".to_string(),
        "-f".to_string(),
        format!("query={BODY_EDITS_QUERY}"),
        "-f".to_string(),
        format!("o={owner}"),
        "-f".to_string(),
        format!("n={name}"),
        "-F".to_string(),
        format!("i={number}"),
    ])
}

/// The `userContentEdits` nodes of issue `number`'s body (`createdAt`,
/// `editedAt`, `editor`): the timeline carries no body edits. `None` on any
/// failure, a GraphQL error, or more edits than one page (not fully read).
fn body_edits(repo: &str, number: u64, root: &Path) -> Option<Vec<serde_json::Value>> {
    let args = body_edits_args(repo, number)?;
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = crate::script_helpers::gh_route::gh_read_for_repo(
        "promotion_author.body_edits",
        &args,
        root,
        repo,
    );
    let json: serde_json::Value = serde_json::from_slice(&out.ok_output()?.stdout).ok()?;
    if json.get("errors").is_some() {
        return None;
    }
    let edits = json.pointer("/data/repository/issue/userContentEdits")?;
    if edits
        .pointer("/pageInfo/hasNextPage")
        .and_then(serde_json::Value::as_bool)
        != Some(false)
    {
        return None;
    }
    edits.get("nodes")?.as_array().cloned()
}

#[cfg(test)]
mod tests {
    use super::{body_edits_args, gh_comment, promotion_permission_invocation, real_repo};
    use serde_json::json;

    #[cfg(unix)]
    #[test]
    fn promotion_author_pins_the_exact_permission_read() {
        use crate::gh_invocation::AccessIntent;
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("temporary fake credential and program");
        let writer = temp.path().join("writer");
        fs::create_dir(&writer).expect("fake writer directory");
        let gh = temp.path().join("gh");
        fs::write(
            &gh,
            r#"#!/bin/sh
test "$*" = 'api repos/acme/widget/collaborators/maintainer/permission --jq .permission' || exit 3
test "${GH_CONFIG_DIR##*/}" = writer || exit 4
test -z "${GH_TOKEN:-}${GITHUB_TOKEN:-}" || exit 5
printf 'write\n'
"#,
        )
        .expect("write stub");
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).expect("executable stub");
        let inv = promotion_permission_invocation("acme/widget", "maintainer", temp.path())
            .expect("valid repository target");
        assert_eq!(inv.intent(), AccessIntent::Read);
        assert_eq!(inv.target().slug().as_deref(), Some("acme/widget"));
        assert!(!inv.is_reader_only(), "the asker's permission would be shed");
        let out = inv
            .program(&gh)
            .gh_config_dir(Some(&writer))
            .without_token_env()
            .run();
        assert_eq!(out.ok_stdout_trimmed().as_deref(), Some("write"));
    }

    /// Finding A (20261116-r15 q48): with no `--repo` the repository is the
    /// `gh` placeholder, and the GraphQL call must still name a real one.
    #[test]
    fn the_default_repo_path_builds_a_graphql_call_with_a_real_owner_and_name() {
        let issue = json!({
            "number": 13171,
            "repository_url": "https://api.github.com/repos/rjwalters/loom",
        });
        let repo = real_repo("{owner}/{repo}", &issue).expect("resolved from the issue");
        assert_eq!(repo, "rjwalters/loom");
        let args = body_edits_args(&repo, 13171).expect("a real slug builds the call");
        assert!(args.contains(&"o=rjwalters".to_string()), "{args:?}");
        assert!(args.contains(&"n=loom".to_string()), "{args:?}");
        assert!(args.contains(&"i=13171".to_string()), "{args:?}");
        assert!(!args
            .iter()
            .any(|a| a.starts_with("o={") || a.starts_with("n={")));
        // The default path and `--repo` build the same call.
        assert_eq!(real_repo("rjwalters/loom", &issue).as_deref(), Some("rjwalters/loom"));
        assert_eq!(Some(args), body_edits_args("rjwalters/loom", 13171));
    }

    /// A placeholder is never sent to GraphQL, and an issue object that names
    /// no repository resolves to nothing (the verdict stays `UNTRUSTED`).
    #[test]
    fn an_unresolved_placeholder_builds_no_graphql_call() {
        assert_eq!(body_edits_args("{owner}/{repo}", 7), None);
        assert_eq!(real_repo("{owner}/{repo}", &json!({"number": 7})), None);
        let odd = json!({"repository_url": "https://api.github.com/repos/{owner}/{repo}"});
        assert_eq!(real_repo("{owner}/{repo}", &odd), None);
        let ghes = json!({"repository_url": "https://ghe.example/api/v3/repos/Org-1/my.repo"});
        assert_eq!(real_repo("{owner}/{repo}", &ghes).as_deref(), Some("Org-1/my.repo"));
        // The owner half follows GitHub's login rule, stricter than the shared
        // slug rule: no `.` or `_`, at most 39 bytes.
        let long = format!("{}/r", "a".repeat(40));
        for bad in [
            "o.x/r",
            "o_x/r",
            "-o/r",
            long.as_str(),
            "o/r/x",
            "o/-r",
            "o/..",
            "/r",
            "o/",
        ] {
            assert_eq!(body_edits_args(bad, 7), None, "{bad}");
            assert_eq!(real_repo(bad, &json!({"number": 7})), None, "{bad}");
            let url = json!({"repository_url": format!("https://api.github.com/repos/{bad}")});
            assert_eq!(real_repo("{owner}/{repo}", &url), None, "{bad}");
        }
    }

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
