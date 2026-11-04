//! `loom-daemon forge additive-migrations <pr>` (#13102) — the fail-closed decision behind
//! Champion's opt-in `additiveMigrations` standing-authorization class key.
//!
//! # What the question is
//!
//! `*migrations/*` is in Champion's kept set, so every D1 migration used to be a critical-file
//! hold needing an operator release. The operator ruled (2026-10-23) that purely *additive*
//! schema migrations should not. This verb answers, for one PR: is **every** migration file
//! it touches a newly added `.sql` file whose statements are all additive?
//!
//! Allowed statements (matched on the leading tokens, case- and whitespace-insensitive, so a
//! statement may span lines): `CREATE TABLE IF NOT EXISTS`, `CREATE [UNIQUE] INDEX IF NOT
//! EXISTS`, `ALTER TABLE <t> ADD COLUMN`, and `INSERT OR IGNORE INTO <t> [(cols)] SELECT …`.
//! Comments (`--`, `/* */`) are ignored. Quoted text (`'…'`, `"…"`, `` `…` ``, `[…]`) is one
//! opaque token, so a `DROP` inside a string literal neither makes an allowed statement
//! forbidden nor makes a forbidden one allowed: the statement is judged by its leading tokens.
//!
//! # Fail closed
//!
//! Exit 0 (`ADDITIVE_MIGRATIONS=1` on stdout) requires every fetched page of the file list to
//! have parsed, at least one migration file, and every migration file (path containing
//! `migrations/`, or ending `_migration.py`) to be `status == "added"` with no
//! `previous_filename`, a `.sql` extension, readable content at the PR head, at least some
//! content, and only allowed statements. Anything else — a forbidden statement, an
//! edited/removed/renamed file, a non-SQL migration file, unterminated quote or comment, an
//! unreadable file, a forge error — exits 1 with the reason on stderr. A caller treats **any**
//! non-zero exit (clap's 2 on a daemon predating this verb, 126/127 for a missing binary) as a
//! hold.

use std::path::Path;

use anyhow::Result;
use serde::Deserialize;

/// `per_page` for the file-list read (GitHub's maximum).
const PAGE_SIZE: usize = 100;

/// GitHub's `pulls/<n>/files` stops at 3000 files, i.e. 30 full pages. A list that is still
/// full at the cap may be truncated, so it holds.
const MAX_PAGES: u32 = 30;

/// Why a PR is NOT cleared. Every variant exits 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// `--repo` was not a plain `OWNER/REPO`.
    InvalidRepo,
    /// A page fetch failed or did not parse as the expected file list.
    FetchFailed(String),
    /// The file list may be truncated (the forge caps it).
    ListTruncated,
    /// The PR touches no migration file.
    NoMigrationFiles,
    /// A migration path that is not a plain added file (edited, removed, renamed, ...).
    NotAdded { file: String, status: String },
    /// A migration-path file that is not `.sql`.
    NotSql(String),
    /// The file's content could not be read at the PR head.
    ReadFailed { file: String, why: String },
    /// The file has no content at all.
    Empty(String),
    /// The SQL could not be split (unterminated quote or comment).
    Unparseable { file: String, why: String },
    /// A statement outside the allow-list.
    Forbidden { file: String, statement: String },
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRepo => f.write_str("--repo is not a plain OWNER/REPO"),
            Self::FetchFailed(why) => write!(f, "could not read the PR's file list: {why}"),
            Self::ListTruncated => f.write_str("the PR's file list may be truncated"),
            Self::NoMigrationFiles => f.write_str("the PR touches no migration file"),
            Self::NotAdded { file, status } => {
                write!(f, "{file} is not a newly added file (status: {status})")
            }
            Self::NotSql(file) => write!(f, "{file} is a migration path but not a .sql file"),
            Self::ReadFailed { file, why } => write!(f, "could not read {file}: {why}"),
            Self::Empty(file) => write!(f, "{file} is empty"),
            Self::Unparseable { file, why } => write!(f, "{file} could not be parsed: {why}"),
            Self::Forbidden { file, statement } => {
                write!(f, "{file} has a non-additive statement: {statement}")
            }
        }
    }
}

/// One entry of a `pulls/<n>/files` page.
#[derive(Deserialize)]
struct FileEntry {
    filename: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    previous_filename: Option<String>,
}

/// Does `path` fall under Champion's `*migrations/*` / `*_migration.py` kept-set patterns?
fn is_migration_path(path: &str) -> bool {
    path.contains("migrations/") || path.ends_with("_migration.py")
}

// ---------------------------------------------------------------- SQL

/// Split `sql` into statements of tokens, dropping comments. Quoted text is one token (quotes
/// kept); `(`, `)` and `,` are their own tokens. Errors on an unterminated quote or comment.
fn statements(sql: &str) -> Result<Vec<Vec<String>>, String> {
    let chars: Vec<char> = sql.chars().collect();
    let (mut out, mut cur, mut word) = (Vec::new(), Vec::<String>::new(), String::new());
    let flush = |word: &mut String, cur: &mut Vec<String>| {
        if !word.is_empty() {
            cur.push(std::mem::take(word));
        }
    };
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '-' if chars.get(i + 1) == Some(&'-') => {
                flush(&mut word, &mut cur);
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                flush(&mut word, &mut cur);
                i += 2;
                loop {
                    match (chars.get(i), chars.get(i + 1)) {
                        (Some('*'), Some('/')) => break,
                        (Some(_), _) => i += 1,
                        (None, _) => return Err("unterminated /* comment".into()),
                    }
                }
                i += 2;
                continue;
            }
            '\'' | '"' | '`' | '[' => {
                flush(&mut word, &mut cur);
                let close = if c == '[' { ']' } else { c };
                let mut tok = String::from(c);
                i += 1;
                loop {
                    match chars.get(i) {
                        None => return Err(format!("unterminated {c} quote")),
                        Some(&q) if q == close => {
                            tok.push(q);
                            // A doubled quote is an escaped quote (not for `[...]`).
                            if c != '[' && chars.get(i + 1) == Some(&close) {
                                tok.push(close);
                                i += 2;
                                continue;
                            }
                            i += 1;
                            break;
                        }
                        Some(&q) => {
                            tok.push(q);
                            i += 1;
                        }
                    }
                }
                cur.push(tok);
                continue;
            }
            ';' => {
                flush(&mut word, &mut cur);
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            '(' | ')' | ',' => {
                flush(&mut word, &mut cur);
                cur.push(c.to_string());
            }
            c if c.is_whitespace() => flush(&mut word, &mut cur),
            c => word.push(c),
        }
        i += 1;
    }
    flush(&mut word, &mut cur);
    if !cur.is_empty() {
        out.push(cur);
    }
    Ok(out)
}

/// Do `tokens` start with `words` (case-insensitive, unquoted)?
fn starts_with_words(tokens: &[String], words: &[&str]) -> bool {
    tokens.len() >= words.len()
        && words
            .iter()
            .zip(tokens)
            .all(|(w, t)| t.eq_ignore_ascii_case(w))
}

/// Is this statement on the allow-list?
fn is_additive(t: &[String]) -> bool {
    if starts_with_words(t, &["CREATE", "TABLE", "IF", "NOT", "EXISTS"])
        || starts_with_words(t, &["CREATE", "INDEX", "IF", "NOT", "EXISTS"])
        || starts_with_words(t, &["CREATE", "UNIQUE", "INDEX", "IF", "NOT", "EXISTS"])
    {
        return true;
    }
    // ALTER TABLE <name> ADD COLUMN ...  (the name is one token, quoted or not)
    if starts_with_words(t, &["ALTER", "TABLE"]) {
        return t.len() >= 5
            && t[3].eq_ignore_ascii_case("ADD")
            && t[4].eq_ignore_ascii_case("COLUMN");
    }
    // INSERT OR IGNORE INTO <name> [( cols )] SELECT ...
    if starts_with_words(t, &["INSERT", "OR", "IGNORE", "INTO"]) {
        let mut i = 5; // past the table name
        if t.get(i).map(String::as_str) == Some("(") {
            match t[i..].iter().position(|x| x == ")") {
                Some(close) => i += close + 1,
                None => return false,
            }
        }
        return t.get(i).is_some_and(|x| x.eq_ignore_ascii_case("SELECT"));
    }
    false
}

/// Classify one migration file's SQL.
///
/// # Errors
///
/// [`Reason::Empty`], [`Reason::Unparseable`] or [`Reason::Forbidden`] for `file`.
pub fn classify_sql(file: &str, sql: &str) -> Result<(), Reason> {
    if sql.trim().is_empty() {
        return Err(Reason::Empty(file.to_string()));
    }
    let stmts = statements(sql).map_err(|why| Reason::Unparseable {
        file: file.to_string(),
        why,
    })?;
    for s in &stmts {
        if !is_additive(s) {
            let mut statement = s.join(" ");
            statement.truncate(
                statement
                    .char_indices()
                    .nth(120)
                    .map_or(statement.len(), |(i, _)| i),
            );
            return Err(Reason::Forbidden {
                file: file.to_string(),
                statement,
            });
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- PR

/// Decide a whole PR: `entries` is the full file list, `read` returns a file's text at the
/// PR head.
///
/// # Errors
///
/// The [`Reason`] the PR is not cleared.
fn decide_files<R>(entries: &[FileEntry], mut read: R) -> Result<(), Reason>
where
    R: FnMut(&str) -> Result<String, String>,
{
    let mut seen = false;
    for e in entries {
        let prev = e.previous_filename.as_deref();
        if !is_migration_path(&e.filename) && !prev.is_some_and(is_migration_path) {
            continue;
        }
        seen = true;
        if e.status != "added" || prev.is_some() {
            return Err(Reason::NotAdded {
                file: e.filename.clone(),
                status: if e.status.is_empty() {
                    "unknown".into()
                } else {
                    e.status.clone()
                },
            });
        }
        if !e.filename.ends_with(".sql") {
            return Err(Reason::NotSql(e.filename.clone()));
        }
        let sql = read(&e.filename).map_err(|why| Reason::ReadFailed {
            file: e.filename.clone(),
            why,
        })?;
        classify_sql(&e.filename, &sql)?;
    }
    if seen {
        Ok(())
    } else {
        Err(Reason::NoMigrationFiles)
    }
}

/// Walk the file list page by page through `fetch_page` (1-based page to raw JSON body).
fn list_files<F>(mut fetch_page: F) -> Result<Vec<FileEntry>, Reason>
where
    F: FnMut(u32) -> Result<Vec<u8>, String>,
{
    let mut all = Vec::new();
    for page in 1..=MAX_PAGES {
        let body = fetch_page(page).map_err(Reason::FetchFailed)?;
        let entries: Vec<FileEntry> = serde_json::from_slice(&body)
            .map_err(|e| Reason::FetchFailed(format!("page {page} did not parse: {e}")))?;
        let full = entries.len() >= PAGE_SIZE;
        all.extend(entries);
        if !full {
            return Ok(all);
        }
    }
    Err(Reason::ListTruncated)
}

/// The whole verdict with the forge reads injected.
///
/// # Errors
///
/// The [`Reason`] the PR is not cleared.
pub fn decide<F, R>(fetch_page: F, read: R) -> Result<(), Reason>
where
    F: FnMut(u32) -> Result<Vec<u8>, String>,
    R: FnMut(&str) -> Result<String, String>,
{
    decide_files(&list_files(fetch_page)?, read)
}

/// Reject file names that could address a different API endpoint.
fn safe_path(file: &str) -> bool {
    !file.is_empty()
        && !file.starts_with('/')
        && !file
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
        && file
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/'))
}

/// Decode a `repos/<r>/contents/<f>?ref=<sha>` JSON body to text.
fn decode_contents(body: &[u8]) -> Result<String, String> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine as _;
    let v: serde_json::Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    if v.get("encoding").and_then(|e| e.as_str()) != Some("base64") {
        return Err("contents API returned no base64 body".into());
    }
    let raw: String = v
        .get("content")
        .and_then(|c| c.as_str())
        .ok_or("contents API returned no content")?
        .split_whitespace()
        .collect();
    let bytes = STANDARD.decode(raw).map_err(|e| e.to_string())?;
    String::from_utf8(bytes).map_err(|e| e.to_string())
}

fn gh_read(gh_bin: &Path, path: &str) -> Result<Vec<u8>, String> {
    crate::verdict_equivalence::gh_api_checked("forge.additive_migrations", gh_bin, None, path)
}

/// The PR head SHA.
fn gh_head_sha(gh_bin: &Path, repo: &str, pr: u32) -> Result<String, String> {
    let body = gh_read(gh_bin, &format!("repos/{repo}/pulls/{pr}"))?;
    let v: serde_json::Value = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
    v.pointer("/head/sha")
        .and_then(|s| s.as_str())
        .filter(|s| s.len() >= 7 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(str::to_string)
        .ok_or_else(|| "PR head sha missing".into())
}

/// Parsed `forge additive-migrations` arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    /// Pull request number.
    pub pr: u32,
    /// `OWNER/REPO`; `None` resolves it from the cwd (never a `{owner}/{repo}` placeholder).
    pub repo: Option<String>,
}

/// Handle `loom-daemon forge additive-migrations <pr> [--repo OWNER/REPO]`.
/// Never returns (exits): 0 with `ADDITIVE_MIGRATIONS=1` when every migration file is
/// additive, otherwise 1 with the reason on stderr.
pub fn handle(args: &Args) -> Result<()> {
    use crate::forge_version_only_diff::{gh_fetch_page, resolve_repo, valid_repo};
    let pr = args.pr;
    let verdict = match args.repo.as_deref() {
        Some(r) if !valid_repo(r) => Err(Reason::InvalidRepo),
        r => {
            let gh = crate::forge_cmd::gh_bin();
            match resolve_repo(r, &gh) {
                Err(why) => Err(Reason::FetchFailed(why)),
                Ok(repo) => {
                    let repo = repo.as_str();
                    let gh = Path::new(&gh);
                    let mut head: Option<String> = None;
                    decide(
                        |page| gh_fetch_page(gh, repo, pr, page),
                        |file| {
                            if !safe_path(file) {
                                return Err("unsafe file path".into());
                            }
                            if head.is_none() {
                                head = Some(gh_head_sha(gh, repo, pr)?);
                            }
                            let sha = head.as_deref().unwrap_or_default();
                            decode_contents(&gh_read(
                                gh,
                                &format!("repos/{repo}/contents/{file}?ref={sha}"),
                            )?)
                        },
                    )
                }
            }
        }
    };
    match verdict {
        Ok(()) => {
            println!("ADDITIVE_MIGRATIONS=1");
            std::process::exit(0);
        }
        Err(reason) => {
            eprintln!("loom-daemon forge additive-migrations: PR #{pr} held: {reason}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn ok(sql: &str) {
        assert_eq!(classify_sql("m/1.sql", sql), Ok(()), "{sql}");
    }

    fn held(sql: &str) {
        assert!(classify_sql("m/1.sql", sql).is_err(), "should hold: {sql}");
    }

    #[test]
    fn allowed_forms_pass() {
        for sql in [
            "CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY, a TEXT);",
            "CREATE INDEX IF NOT EXISTS i ON t(a);",
            "CREATE UNIQUE INDEX IF NOT EXISTS i ON t(a);",
            "ALTER TABLE t ADD COLUMN b TEXT DEFAULT 'x';",
            "INSERT OR IGNORE INTO t SELECT * FROM u;",
            "INSERT OR IGNORE INTO t (a, b) SELECT a, b FROM u WHERE a > 1;",
            "-- only a comment\n/* and a block */",
            "create table if not exists t (a int)",
        ] {
            ok(sql);
        }
    }

    #[test]
    fn forbidden_forms_hold() {
        for sql in [
            "DROP TABLE t;",
            "DELETE FROM t;",
            "UPDATE t SET a = 1;",
            "ALTER TABLE t RENAME TO u;",
            "ALTER TABLE t RENAME COLUMN a TO b;",
            "ALTER TABLE t DROP COLUMN a;",
            "REPLACE INTO t VALUES (1);",
            "INSERT INTO t VALUES (1);",
            "INSERT OR REPLACE INTO t SELECT * FROM u;",
            "INSERT OR IGNORE INTO t VALUES (1);",
            "CREATE TRIGGER x AFTER INSERT ON t BEGIN SELECT 1; END;",
            "PRAGMA foreign_keys = OFF;",
            "CREATE TABLE t (a int);",
            "CREATE INDEX i ON t(a);",
            "CREATE TEMP TABLE IF NOT EXISTS t (a int);",
            "CREATE TABLE IF NOT EXISTS t (a int); DROP TABLE t;",
        ] {
            held(sql);
        }
    }

    #[test]
    fn multiline_statement_passes() {
        ok("CREATE TABLE\n  IF NOT EXISTS\n  t (\n    a TEXT,\n    b TEXT\n  );\n");
        ok("ALTER TABLE t\n  ADD\n  COLUMN c INTEGER;");
    }

    #[test]
    fn comments_are_ignored() {
        ok("-- DROP TABLE x;\nCREATE TABLE IF NOT EXISTS t (a int); -- DELETE FROM t\n");
        ok("/* DROP TABLE x; */ CREATE TABLE IF NOT EXISTS t (a int);");
        held("-- fine\nDROP TABLE t; -- CREATE TABLE IF NOT EXISTS t (a int);");
    }

    #[test]
    fn semicolon_and_drop_inside_string_literal() {
        // Judged by leading tokens: a literal never turns a statement forbidden...
        ok("INSERT OR IGNORE INTO t SELECT 'a; DROP TABLE x' FROM u;");
        // ...and never launders a forbidden one.
        held("DELETE FROM t WHERE a = 'CREATE TABLE IF NOT EXISTS x';");
    }

    #[test]
    fn unparsable_sql_holds() {
        held("CREATE TABLE IF NOT EXISTS t (a TEXT DEFAULT 'oops);");
        held("CREATE TABLE IF NOT EXISTS t (a int); /* never closed");
        held("");
        held("   \n\t ");
    }

    #[test]
    fn crlf_and_mixed_case() {
        ok("Create Table If Not Exists t (a int);\r\nAlter Table t Add Column b int;\r\n");
    }

    fn entry(name: &str, status: &str, prev: Option<&str>) -> FileEntry {
        FileEntry {
            filename: name.into(),
            status: status.into(),
            previous_filename: prev.map(str::to_string),
        }
    }

    fn good(_: &str) -> Result<String, String> {
        Ok("CREATE TABLE IF NOT EXISTS t (a int);".into())
    }

    #[test]
    fn added_sql_migration_passes() {
        let e = [
            entry("migrations/0042_x.sql", "added", None),
            entry("src/lib.rs", "modified", None),
        ];
        assert_eq!(decide_files(&e, good), Ok(()));
    }

    #[test]
    fn edited_removed_renamed_migration_holds() {
        for (status, prev) in [
            ("modified", None),
            ("removed", None),
            ("renamed", Some("migrations/old.sql")),
            ("added", Some("elsewhere/old.sql")),
            ("", None),
        ] {
            let e = [entry("migrations/0042_x.sql", status, prev)];
            assert!(matches!(decide_files(&e, good), Err(Reason::NotAdded { .. })));
        }
        // Renamed *out of* the migrations dir still counts.
        let e = [entry("docs/x.sql", "renamed", Some("migrations/0001.sql"))];
        assert!(matches!(decide_files(&e, good), Err(Reason::NotAdded { .. })));
    }

    #[test]
    fn non_sql_migration_path_holds() {
        let e = [entry("db/0003_add_migration.py", "added", None)];
        assert_eq!(decide_files(&e, good), Err(Reason::NotSql("db/0003_add_migration.py".into())));
        let e = [entry("migrations/README.md", "added", None)];
        assert!(matches!(decide_files(&e, good), Err(Reason::NotSql(_))));
    }

    #[test]
    fn no_migration_file_holds() {
        let e = [entry("src/lib.rs", "added", None)];
        assert_eq!(decide_files(&e, good), Err(Reason::NoMigrationFiles));
        assert_eq!(decide_files(&[], good), Err(Reason::NoMigrationFiles));
    }

    #[test]
    fn one_bad_file_among_good_holds() {
        let e = [
            entry("migrations/1.sql", "added", None),
            entry("migrations/2.sql", "added", None),
        ];
        let r = |f: &str| {
            Ok(if f.ends_with("2.sql") {
                "DROP TABLE t;".into()
            } else {
                "CREATE TABLE IF NOT EXISTS t (a int);".into()
            })
        };
        assert!(matches!(decide_files(&e, r), Err(Reason::Forbidden { .. })));
    }

    #[test]
    fn unreadable_file_holds() {
        let e = [entry("migrations/1.sql", "added", None)];
        let r = |_: &str| Err("boom".to_string());
        assert!(matches!(decide_files(&e, r), Err(Reason::ReadFailed { .. })));
    }

    #[test]
    fn list_fetch_errors_hold() {
        let r = decide(|_| Err("api down".into()), good);
        assert!(matches!(r, Err(Reason::FetchFailed(_))));
        let r = decide(|_| Ok(b"not json".to_vec()), good);
        assert!(matches!(r, Err(Reason::FetchFailed(_))));
    }

    #[test]
    fn pagination_collects_every_page() {
        let page1: Vec<String> = (0..PAGE_SIZE)
            .map(|i| format!(r#"{{"filename":"src/f{i}.rs","status":"modified"}}"#))
            .collect();
        let page1 = format!("[{}]", page1.join(","));
        let page2 = r#"[{"filename":"migrations/9.sql","status":"added"}]"#.to_string();
        let r =
            decide(|p| Ok(if p == 1 { page1.clone() } else { page2.clone() }.into_bytes()), good);
        assert_eq!(r, Ok(()));
    }

    #[test]
    fn full_list_at_cap_holds() {
        let page: Vec<String> = (0..PAGE_SIZE)
            .map(|i| format!(r#"{{"filename":"src/f{i}.rs","status":"modified"}}"#))
            .collect();
        let page = format!("[{}]", page.join(","));
        let r = decide(|_| Ok(page.clone().into_bytes()), good);
        assert_eq!(r, Err(Reason::ListTruncated));
    }

    #[test]
    fn safe_path_rejects_traversal() {
        assert!(safe_path("migrations/0001_x.sql"));
        assert!(!safe_path("migrations/../x.sql"));
        assert!(!safe_path("/migrations/x.sql"));
        assert!(!safe_path("migrations/x y.sql"));
    }
}
