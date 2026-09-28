//! Agent-facing conditional-request (ETag/304) single-object reads (#9254).
//!
//! # Why this exists
//!
//! [`crate::forge_cached_list`] (#5056) serves `gh issue list` / `gh pr list`
//! through REST + ETag, where an unchanged answer is a free, never-stale
//! `304`. Single-object reads — `gh issue view N` / `gh pr view N`, which role
//! prompts issue constantly to re-read one item's labels/state before a claim,
//! label transition, or merge decision — only had `gh-cached`'s 30s TTL cache
//! and fell through to plain **GraphQL** `gh` on every miss.
//!
//! This module serves them as `loom-daemon forge issue view --cached N …` /
//! `forge pr view --cached N …`:
//!
//! - `issue view` → `GET repos/{o}/{r}/issues/{n}`
//! - `pr view` → `GET repos/{o}/{r}/pulls/{n}`
//!
//! each sent with `If-None-Match` against a disk-persistent ETag entry, so a
//! repeat read from a fresh short-lived agent process costs a free `304`.
//! Because every read is revalidated, it is never stale the way a TTL entry
//! is — which is why ADR-0021's amendment permits it for gating reads.
//!
//! # Output parity with `gh … view --json`
//!
//! Output is what `gh` prints when stdout is not a TTY: compact JSON, keys
//! sorted, trailing newline; `--jq` goes through the same `jq -c -r` step as
//! the listing path. Each served field reproduces `gh`'s name, JSON type and
//! value, including `null` for an unset timestamp and full label objects
//! (`{id,name,description,color}`, `id` being the GraphQL node id REST
//! exposes as `node_id`). `author` is **not** served: `gh` emits the user's
//! display `name`, which the REST issue/pull payload does not carry.
//!
//! PR `state` is derived — REST reports `open|closed` plus `merged_at`, `gh`
//! reports `OPEN|CLOSED|MERGED`: a non-null `merged_at` is `MERGED`,
//! otherwise the uppercased REST state.
//!
//! # Scope — what declines
//!
//! Anything this cannot reproduce exactly **declines** (exit [`DECLINED`], no
//! stdout) so `gh-cached` falls back to plain `gh`: no `--json`; a field
//! outside the supported set (e.g. `author`, `mergeable`, `mergeStateStatus`,
//! `reviews`, `statusCheckRollup`, `files`, `comments`); `--comments`, `--web`,
//! `--template` or any other flag; a non-numeric or missing selector (URL,
//! branch, current-branch `pr view`); an `issue view` of a number that is
//! actually a PR; Gitea; and any non-200/304 answer.
//!
//! # Invalidation
//!
//! `forge <issue|pr> view --cached --invalidate [N]` drops the view entries
//! for `N` (both kinds — issues and PRs share one number space, and a PR edit
//! changes its `issues/{n}` row too), or every view entry with no `N`.
//! `gh-cached` calls it after a successful mutation so the next read after our
//! own write is unconditional, guarding against a replica-lag `304`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use serde_json::{json, Map, Value};

use crate::forge_cmd::{detect_forge, ForgeType};
use crate::forge_listing::parse_http_response;

/// Exit code signalling "this shape is not cacheable; fall back to `gh`".
pub const DECLINED: i32 = crate::forge_cmd::EX_FORGE_DECLINED;

/// `--json` fields served for `issue view`.
const ISSUE_FIELDS: &[&str] = &[
    "body",
    "closedAt",
    "createdAt",
    "id",
    "labels",
    "number",
    "state",
    "title",
    "updatedAt",
    "url",
];

/// `--json` fields served for `pr view` (the issue set plus pull-only fields).
const PR_FIELDS: &[&str] = &[
    "baseRefName",
    "body",
    "closedAt",
    "createdAt",
    "headRefName",
    "id",
    "isDraft",
    "labels",
    "mergedAt",
    "number",
    "state",
    "title",
    "updatedAt",
    "url",
];

/// Filename prefix that distinguishes view entries from the listing entries
/// sharing [`cache_dir`].
const VIEW_PREFIX: &str = "view-";

/// View entries whose last `200` write is older than this are pruned on the
/// next write. Without it the directory grows by one file per distinct
/// `(repo, identity, number)` ever read; an evicted hot entry costs one `200`.
const VIEW_ENTRY_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// A parsed, cacheable `view` query. `None` from [`parse_query`] declines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewQuery {
    pub number: u32,
    /// Requested fields, sorted and de-duplicated (gh's output order).
    pub json_fields: Vec<String>,
    pub jq: Option<String>,
    pub repo: Option<String>,
}

/// Is this a `forge <issue|pr> view --cached …` request?
#[must_use]
pub fn is_cached_view(args: &[String]) -> bool {
    args.first().map(String::as_str) == Some("view") && args.iter().any(|a| a == "--cached")
}

/// Entry point from `forge_cmd::dispatch`. Serves to stdout and exits `0`, or
/// exits [`DECLINED`] with no stdout. `--invalidate` drops entries and exits
/// `0`. Never returns.
pub fn handle(entity: &str, args: &[String]) -> ! {
    if args.iter().any(|a| a == "--invalidate") {
        let number = args
            .iter()
            .skip(1)
            .find(|a| !a.starts_with('-'))
            .and_then(|a| a.parse::<u32>().ok());
        invalidate(&cache_dir(), number);
        std::process::exit(0);
    }
    if detect_forge(None) == ForgeType::Gitea {
        std::process::exit(DECLINED);
    }
    if let Some(output) = build_output(entity, args, &default_fetcher) {
        print!("{output}");
        std::process::exit(0);
    }
    eprintln!(
        "loom-daemon forge {entity} view --cached: not a cacheable shape (or lookup failed); \
         caller should fall back to gh"
    );
    std::process::exit(DECLINED)
}

/// Fetches the raw REST JSON body for `(entity, number, repo)`; injected by
/// tests. Production uses [`default_fetcher`].
type Fetcher<'a> = dyn Fn(&str, u32, Option<&str>) -> Option<String> + 'a;

fn default_fetcher(entity: &str, number: u32, repo: Option<&str>) -> Option<String> {
    let gh_bin = std::env::var("LOOM_GH_BIN").unwrap_or_else(|_| "gh".to_string());
    let cwd = std::env::current_dir().ok();
    fetch_conditional(Path::new(&gh_bin), cwd.as_deref(), &cache_dir(), entity, number, repo)
}

/// Side-effect-free (given `fetch`) pipeline: parse → fetch → project → jq.
pub fn build_output(entity: &str, args: &[String], fetch: &Fetcher) -> Option<String> {
    let q = parse_query(entity, args)?;
    let body = fetch(entity, q.number, q.repo.as_deref())?;
    let row: Value = serde_json::from_str(body.trim()).ok()?;
    let obj = project(entity, &row, q.number, &q.json_fields)?;
    match &q.jq {
        Some(expr) => crate::forge_cached_list::apply_jq(&obj, expr),
        None => serde_json::to_string(&obj).ok().map(|s| format!("{s}\n")),
    }
}

/// Parse the `view` argv (`args[0] == "view"`) into a [`ViewQuery`], or `None`
/// to decline. Accepts `--flag value` and `--flag=value`.
pub fn parse_query(entity: &str, args: &[String]) -> Option<ViewQuery> {
    let supported = match entity {
        "issue" => ISSUE_FIELDS,
        "pr" => PR_FIELDS,
        _ => return None,
    };
    let mut number: Option<u32> = None;
    let mut json_fields: Vec<String> = Vec::new();
    let mut jq = None;
    let mut repo = None;

    let mut i = 1;
    while i < args.len() {
        let arg = &args[i];
        let (flag, inline_val) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with('-') => (f, Some(v)),
            _ => (arg.as_str(), None),
        };
        let take_val = |i: &mut usize| -> Option<String> {
            if let Some(v) = inline_val {
                Some(v.to_string())
            } else {
                *i += 1;
                args.get(*i).cloned()
            }
        };
        match flag {
            "--cached" => {}
            "--repo" | "-R" => repo = Some(take_val(&mut i)?),
            "--jq" | "-q" => jq = Some(take_val(&mut i)?),
            "--json" => {
                for field in take_val(&mut i)?
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    if !supported.contains(&field) {
                        return None;
                    }
                    json_fields.push(field.to_string());
                }
            }
            // The selector: only a plain number (never a URL/branch), once.
            positional if !positional.starts_with('-') && number.is_none() => {
                if !positional.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                number = Some(positional.parse().ok()?);
            }
            // --comments, --web, --template, a second selector, …
            _ => return None,
        }
        i += 1;
    }

    if json_fields.is_empty() {
        return None;
    }
    json_fields.sort();
    json_fields.dedup();
    Some(ViewQuery {
        number: number?,
        json_fields,
        jq,
        repo,
    })
}

/// Project a REST issue/pull row into gh's `--json` object for `fields`
/// (already sorted). `None` declines: an `issue view` of a PR, a number
/// mismatch, or a row missing a value gh would always have.
fn project(entity: &str, row: &Value, number: u32, fields: &[String]) -> Option<Value> {
    if row.get("number")?.as_u64()? != u64::from(number) {
        return None;
    }
    // REST `issues/{n}` also answers for PRs; `gh issue view` of a PR number
    // behaves differently, so let plain gh keep that behaviour.
    if entity == "issue" && row.get("pull_request").is_some_and(|v| !v.is_null()) {
        return None;
    }
    let str_or_empty = |key: &str| json!(row.get(key).and_then(Value::as_str).unwrap_or(""));
    let str_or_null = |key: &str| match row.get(key) {
        Some(Value::String(s)) => json!(s),
        _ => Value::Null,
    };

    let mut obj = Map::new();
    for f in fields {
        let v = match f.as_str() {
            "number" => json!(number),
            "title" => str_or_empty("title"),
            "body" => str_or_empty("body"),
            "id" => json!(row.get("node_id")?.as_str()?),
            "url" => json!(row.get("html_url")?.as_str()?),
            "state" => json!(derive_state(entity, row)?),
            "labels" => project_labels(row.get("labels")?)?,
            "createdAt" => str_or_null("created_at"),
            "updatedAt" => str_or_null("updated_at"),
            "closedAt" => str_or_null("closed_at"),
            "mergedAt" => str_or_null("merged_at"),
            "isDraft" => json!(row.get("draft").and_then(Value::as_bool).unwrap_or(false)),
            "headRefName" => json!(row.get("head")?.get("ref")?.as_str()?),
            "baseRefName" => json!(row.get("base")?.get("ref")?.as_str()?),
            _ => return None,
        };
        obj.insert(f.clone(), v);
    }
    Some(Value::Object(obj))
}

/// gh's `state`: `OPEN`/`CLOSED`, plus `MERGED` for a PR with `merged_at`.
/// Getting this wrong would misroute every "is it merged?" decision.
fn derive_state(entity: &str, row: &Value) -> Option<String> {
    if entity == "pr" && row.get("merged_at").is_some_and(|v| !v.is_null()) {
        return Some("MERGED".to_string());
    }
    let state = row.get("state")?.as_str()?;
    match state {
        "open" | "closed" => Some(state.to_ascii_uppercase()),
        _ => None,
    }
}

/// REST label objects → gh's `{id,name,description,color}` (in that order).
fn project_labels(labels: &Value) -> Option<Value> {
    let mut out = Vec::new();
    for l in labels.as_array()? {
        let mut m = Map::new();
        m.insert("id".into(), json!(l.get("node_id")?.as_str()?));
        m.insert("name".into(), json!(l.get("name")?.as_str()?));
        m.insert(
            "description".into(),
            json!(l.get("description").and_then(Value::as_str).unwrap_or("")),
        );
        m.insert("color".into(), json!(l.get("color")?.as_str()?));
        out.push(Value::Object(m));
    }
    Some(Value::Array(out))
}

// ============================================================================
// Disk-persistent ETag store for view entries
// ============================================================================
//
// Self-contained on purpose: the shared disk-ETag primitives are still private
// to `forge_listing.rs` until "PR A" (#9251/#9252, PR #9261) makes them
// `pub(crate)`. Fold this store (and record 200/304 through PR A's stats sink)
// into them once it lands — tracked by #9273.

/// Shared with the listing store (`LOOM_LISTING_CACHE_DIR` overrides; else
/// `${TMPDIR:-/tmp}/loom-forge-listing-cache`); view files carry
/// [`VIEW_PREFIX`].
fn cache_dir() -> PathBuf {
    if let Ok(d) = std::env::var("LOOM_LISTING_CACHE_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    let base = std::env::var("TMPDIR")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/tmp".to_string());
    PathBuf::from(base).join("loom-forge-listing-cache")
}

#[must_use]
pub fn build_view_url(entity: &str, repo: Option<&str>, number: u32) -> String {
    let repo_path = repo.unwrap_or("{owner}/{repo}");
    let kind = if entity == "pr" { "pulls" } else { "issues" };
    format!("repos/{repo_path}/{kind}/{number}")
}

/// Resolved `owner/repo` (explicit, else `cwd`'s origin remote, else the raw
/// path) — the placeholder URL alone says nothing about which repo it hits.
fn repo_scope(cwd: Option<&Path>, repo: Option<&str>) -> String {
    if let Some(r) = repo {
        return r.to_string();
    }
    cwd.map(|dir| {
        crate::credential_preflight::nwo_from_git_remote(dir)
            .unwrap_or_else(|| dir.display().to_string())
    })
    .unwrap_or_default()
}

/// Credential identity: host, config dir, and a truncated SHA-256 fingerprint
/// of any env token (never the token itself), so two identities never share
/// an entry.
fn credential_identity(cwd: Option<&Path>) -> String {
    use sha2::{Digest, Sha256};
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    let token = [env("GH_TOKEN"), env("GITHUB_TOKEN")].concat();
    let fingerprint = if token.is_empty() {
        String::new()
    } else {
        Sha256::digest(token.as_bytes())
            .iter()
            .take(8)
            .map(|b| format!("{b:02x}"))
            .collect()
    };
    let scoped_config = cwd
        .and_then(crate::credential_preflight::gh_config_dir_for_root)
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    format!("{}|{}|{scoped_config}|{fingerprint}", env("GH_HOST"), env("GH_CONFIG_DIR"))
}

fn entry_path(dir: &Path, entity: &str, number: u32, cache_key: &str) -> PathBuf {
    // FNV-1a 64-bit: dependency-free, path-safe.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in cache_key.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    dir.join(format!("{VIEW_PREFIX}{entity}-{number}-{hash:016x}.json"))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct DiskEntry {
    etag: String,
    body: String,
}

fn read_entry(path: &Path) -> Option<DiskEntry> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Atomic (temp + rename) best-effort write.
fn write_entry(path: &Path, entry: &DiskEntry) {
    let Some(dir) = path.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let Ok(serialized) = serde_json::to_string(entry) else {
        return;
    };
    let tmp = dir.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        path.file_name().and_then(|n| n.to_str()).unwrap_or("entry")
    ));
    if std::fs::write(&tmp, serialized).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// View entry files in `dir`, as `(path, file name)`.
fn view_files(dir: &Path) -> Vec<(PathBuf, String)> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    rd.filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            (name.starts_with(VIEW_PREFIX) && name.ends_with(".json")).then(|| (e.path(), name))
        })
        .collect()
}

/// Drop view entries for `number` (both kinds), or every view entry.
fn invalidate(dir: &Path, number: Option<u32>) {
    let infixes = number.map(|n| [format!("issue-{n}-"), format!("pr-{n}-")]);
    for (path, name) in view_files(dir) {
        let hit = infixes.as_ref().is_none_or(|[a, b]| {
            let rest = &name[VIEW_PREFIX.len()..];
            rest.starts_with(a.as_str()) || rest.starts_with(b.as_str())
        });
        if hit {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Remove view entries last written more than [`VIEW_ENTRY_MAX_AGE`] ago.
fn prune_stale(dir: &Path) {
    let now = SystemTime::now();
    for (path, _) in view_files(dir) {
        let stale = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > VIEW_ENTRY_MAX_AGE);
        if stale {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// One conditional `gh api --include` GET. Returns the body to serve (fresh on
/// `200`, the stored body on `304`), or `None` to decline.
fn fetch_conditional(
    gh_bin: &Path,
    cwd: Option<&Path>,
    dir: &Path,
    entity: &str,
    number: u32,
    repo_override: Option<&str>,
) -> Option<String> {
    let env_repo = std::env::var("LOOM_REPO").ok().filter(|s| !s.is_empty());
    let repo = repo_override.or(env_repo.as_deref());
    let url = build_view_url(entity, repo, number);
    let key = format!("{}|{}|{url}", repo_scope(cwd, repo), credential_identity(cwd));
    let path = entry_path(dir, entity, number, &key);
    let prior = read_entry(&path);

    let mut cmd = Command::new(gh_bin);
    cmd.arg("api").arg("--include").arg(&url);
    if let Some(p) = &prior {
        cmd.arg("-H").arg(format!("If-None-Match: {}", p.etag));
    }
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    crate::credential_preflight::apply_gh_config_for_cwd(&mut cmd, cwd);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = cmd.output().ok()?;
    let response = parse_http_response(&String::from_utf8_lossy(&out.stdout))?;

    match response.status {
        304 => match prior {
            Some(p) => Some(p.body),
            None => {
                let _ = std::fs::remove_file(&path);
                None
            }
        },
        200 if out.status.success() => {
            if let Some(etag) = response.etag {
                write_entry(
                    &path,
                    &DiskEntry {
                        etag,
                        body: response.body.clone(),
                    },
                );
                prune_stale(dir);
            }
            Some(response.body)
        }
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|p| (*p).to_string()).collect()
    }

    fn label(name: &str) -> Value {
        json!({
            "id": 1, "node_id": format!("LA_{name}"), "url": "u", "name": name,
            "color": "10B981", "default": false, "description": null
        })
    }

    fn issue_row(number: u32) -> Value {
        json!({
            "number": number, "node_id": "I_x", "html_url": format!("https://github.com/o/r/issues/{number}"),
            "title": "t", "state": "open", "body": null, "labels": [label("loom:issue")],
            "created_at": "2026-09-27T00:00:00Z", "updated_at": "2026-09-27T01:00:00Z",
            "closed_at": null, "user": {"login": "octocat"}
        })
    }

    fn pr_row(number: u32, state: &str, merged_at: Option<&str>) -> Value {
        json!({
            "number": number, "node_id": "PR_x", "html_url": format!("https://github.com/o/r/pull/{number}"),
            "title": "p", "state": state, "body": "b", "labels": [label("loom:pr")],
            "created_at": "c", "updated_at": "u", "closed_at": merged_at, "merged_at": merged_at,
            "draft": true, "head": {"ref": "feature/issue-1"}, "base": {"ref": "main"}
        })
    }

    fn fixture(body: Value) -> impl Fn(&str, u32, Option<&str>) -> Option<String> {
        move |_: &str, _: u32, _: Option<&str>| Some(body.to_string())
    }

    // ===== is_cached_view / parse_query =====

    #[test]
    fn detects_cached_view_only_with_flag_and_view_verb() {
        assert!(is_cached_view(&argv(&["view", "--cached", "1"])));
        assert!(!is_cached_view(&argv(&["view", "1"])));
        assert!(!is_cached_view(&argv(&["list", "--cached"])));
    }

    #[test]
    fn declines_table_of_shapes() {
        for (entity, parts) in [
            ("issue", vec!["view", "--cached", "42"]), // no --json
            ("issue", vec!["view", "--cached", "42", "--json", "author"]),
            ("issue", vec!["view", "--cached", "42", "--json", "headRefName"]),
            ("pr", vec!["view", "--cached", "42", "--json", "mergeable"]),
            ("pr", vec!["view", "--cached", "42", "--json", "mergeStateStatus"]),
            ("pr", vec!["view", "--cached", "42", "--json", "reviews"]),
            ("pr", vec!["view", "--cached", "42", "--json", "statusCheckRollup"]),
            ("pr", vec!["view", "--cached", "42", "--json", "files,state"]),
            ("pr", vec!["view", "--cached", "--json", "state"]), // current branch
            ("pr", vec!["view", "--cached", "feature/x", "--json", "state"]),
            (
                "issue",
                vec![
                    "view",
                    "--cached",
                    "https://github.com/o/r/issues/1",
                    "--json",
                    "state",
                ],
            ),
            ("issue", vec!["view", "--cached", "42", "--comments", "--json", "state"]),
            ("issue", vec!["view", "--cached", "42", "--web"]),
            ("pr", vec!["view", "--cached", "42", "--json", "state", "-t", "{{.}}"]),
            ("pr", vec!["view", "--cached", "1", "2", "--json", "state"]),
            ("repo", vec!["view", "--cached", "1", "--json", "state"]),
        ] {
            assert!(
                parse_query(entity, &argv(&parts)).is_none(),
                "{entity} {parts:?} should decline"
            );
        }
    }

    #[test]
    fn parses_supported_shape_sorted_and_deduped() {
        let q = parse_query(
            "pr",
            &argv(&[
                "view",
                "--cached",
                "42",
                "--json=state,labels,state",
                "-R",
                "o/r",
                "--jq",
                ".state",
            ]),
        )
        .unwrap();
        assert_eq!(q.number, 42);
        assert_eq!(q.json_fields, vec!["labels", "state"]);
        assert_eq!(q.repo.as_deref(), Some("o/r"));
        assert_eq!(q.jq.as_deref(), Some(".state"));
    }

    // ===== projection =====

    #[test]
    fn issue_projection_matches_gh_shape_byte_for_byte() {
        let out = build_output(
            "issue",
            &argv(&[
                "view",
                "--cached",
                "7",
                "--json",
                "title,state,number,labels,closedAt,body",
            ]),
            &fixture(issue_row(7)),
        )
        .unwrap();
        assert_eq!(
            out,
            "{\"body\":\"\",\"closedAt\":null,\"labels\":[{\"id\":\"LA_loom:issue\",\"name\":\
             \"loom:issue\",\"description\":\"\",\"color\":\"10B981\"}],\"number\":7,\"state\":\
             \"OPEN\",\"title\":\"t\"}\n"
        );
    }

    #[test]
    fn issue_view_of_a_pr_row_declines() {
        let mut row = issue_row(8);
        row["pull_request"] = json!({"url": "x"});
        assert!(build_output(
            "issue",
            &argv(&["view", "--cached", "8", "--json", "state"]),
            &fixture(row)
        )
        .is_none());
    }

    #[test]
    fn number_mismatch_declines() {
        assert!(build_output(
            "issue",
            &argv(&["view", "--cached", "9", "--json", "state"]),
            &fixture(issue_row(7))
        )
        .is_none());
    }

    fn pr_state(state: &str, merged_at: Option<&str>) -> String {
        let out = build_output(
            "pr",
            &argv(&["view", "--cached", "5", "--json", "state"]),
            &fixture(pr_row(5, state, merged_at)),
        )
        .unwrap();
        serde_json::from_str::<Value>(&out).unwrap()["state"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn pr_state_derives_merged_from_merged_at() {
        assert_eq!(pr_state("open", None), "OPEN");
        assert_eq!(pr_state("closed", None), "CLOSED");
        assert_eq!(pr_state("closed", Some("2026-09-27T23:10:33Z")), "MERGED");
    }

    #[test]
    fn pr_projection_serves_pull_fields() {
        let out = build_output(
            "pr",
            &argv(&[
                "view",
                "--cached",
                "5",
                "--json",
                "headRefName,baseRefName,isDraft,mergedAt,url,id",
            ]),
            &fixture(pr_row(5, "open", None)),
        )
        .unwrap();
        assert_eq!(
            out,
            "{\"baseRefName\":\"main\",\"headRefName\":\"feature/issue-1\",\"id\":\"PR_x\",\
             \"isDraft\":true,\"mergedAt\":null,\"url\":\"https://github.com/o/r/pull/5\"}\n"
        );
    }

    #[test]
    fn jq_applied_when_jq_present() {
        if Command::new("jq").arg("--version").output().is_err() {
            return;
        }
        let out = build_output(
            "pr",
            &argv(&[
                "view",
                "--cached",
                "5",
                "--json",
                "labels,state",
                "--jq",
                ".labels[].name",
            ]),
            &fixture(pr_row(5, "open", None)),
        )
        .unwrap();
        assert_eq!(out, "loom:pr\n");
    }

    #[test]
    fn fetch_failure_declines() {
        let none = |_: &str, _: u32, _: Option<&str>| None;
        assert!(
            build_output("issue", &argv(&["view", "--cached", "1", "--json", "state"]), &none)
                .is_none()
        );
    }

    #[test]
    fn view_urls() {
        assert_eq!(build_view_url("issue", None, 3), "repos/{owner}/{repo}/issues/3");
        assert_eq!(build_view_url("pr", Some("o/r"), 3), "repos/o/r/pulls/3");
    }

    // ===== end-to-end with a fake gh =====

    /// 200 + ETag on an unconditional call; 304 + exit 1 (like real gh) when
    /// the caller presents the ETag. Logs each invocation's argv.
    fn write_fake_gh(dir: &Path) -> PathBuf {
        let path = dir.join("fake-gh.sh");
        let script = format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> '{log}'
case "$*" in
  *'If-None-Match: W/"v1"'*)
    printf 'HTTP/2.0 304 Not Modified\r\n\r\n'
    echo 'gh: Not Modified (HTTP 304)' 1>&2
    exit 1
    ;;
  *)
    printf 'HTTP/2.0 200 OK\r\nEtag: W/"v1"\r\n\r\n'
    printf '{{"number": 42, "node_id": "I_1", "state": "open", "labels": [{{"node_id": "LA_1", "name": "loom:issue", "description": null, "color": "fff"}}]}}\n'
    ;;
esac
"#,
            log = dir.join("calls.log").display()
        );
        std::fs::write(&path, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    #[test]
    fn etag_roundtrip_serves_304_from_disk_and_invalidate_forces_unconditional() {
        let dir = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let gh = write_fake_gh(dir.path());
        let fetch = |entity: &str, n: u32, repo: Option<&str>| {
            let repo = repo.or(Some("o/r"));
            fetch_conditional(&gh, Some(dir.path()), cache.path(), entity, n, repo)
        };
        let args = argv(&["view", "--cached", "42", "--json", "labels,state"]);
        let expected = "{\"labels\":[{\"id\":\"LA_1\",\"name\":\"loom:issue\",\"description\":\
                        \"\",\"color\":\"fff\"}],\"state\":\"OPEN\"}\n";

        // Round 1: unconditional 200, entry written to disk.
        assert_eq!(build_output("issue", &args, &fetch).unwrap(), expected);
        // Round 2: sends If-None-Match, gets 304, serves the stored body.
        assert_eq!(build_output("issue", &args, &fetch).unwrap(), expected);
        let log = std::fs::read_to_string(dir.path().join("calls.log")).unwrap();
        let calls: Vec<&str> = log.lines().collect();
        assert_eq!(calls.len(), 2);
        assert!(calls[0].contains("repos/o/r/issues/42") && !calls[0].contains("If-None-Match"));
        assert!(calls[1].contains("If-None-Match: W/\"v1\""));

        // A write-through invalidation of #42 makes the next read unconditional.
        invalidate(cache.path(), Some(42));
        assert!(view_files(cache.path()).is_empty());
        build_output("issue", &args, &fetch).unwrap();
        let log = std::fs::read_to_string(dir.path().join("calls.log")).unwrap();
        assert!(!log.lines().nth(2).unwrap().contains("If-None-Match"));
    }

    #[test]
    fn invalidate_scopes_to_number_and_kind_prefix() {
        let cache = tempfile::tempdir().unwrap();
        let entry = DiskEntry {
            etag: "e".into(),
            body: "{}".into(),
        };
        for (entity, n) in [("issue", 42), ("pr", 42), ("issue", 420), ("pr", 4)] {
            write_entry(&entry_path(cache.path(), entity, n, "k"), &entry);
        }
        std::fs::write(cache.path().join("listing-abc.json"), "{}").unwrap();
        invalidate(cache.path(), Some(42));
        let mut left: Vec<String> = view_files(cache.path())
            .into_iter()
            .map(|(_, n)| n)
            .collect();
        left.sort();
        assert_eq!(left.len(), 2);
        assert!(left[0].starts_with("view-issue-420-") && left[1].starts_with("view-pr-4-"));
        invalidate(cache.path(), None);
        assert!(view_files(cache.path()).is_empty());
        // Listing entries are never touched.
        assert!(cache.path().join("listing-abc.json").exists());
    }

    #[test]
    fn non_200_answer_declines() {
        let dir = tempfile::tempdir().unwrap();
        let gh = dir.path().join("gh404.sh");
        std::fs::write(&gh, "#!/bin/sh\nprintf 'HTTP/2.0 404 Not Found\\r\\n\\r\\n{}'\nexit 1\n")
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert!(
            fetch_conditional(&gh, Some(dir.path()), dir.path(), "pr", 1, Some("o/r")).is_none()
        );
    }
}
