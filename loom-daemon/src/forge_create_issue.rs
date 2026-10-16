//! `loom-daemon forge create-issue` (#12504): the Rust port of the create step
//! of `create-issue.sh` (`forge_gh_create_issue_rl_safe`, #5047/#8289/#9714).
//!
//! Transport, in order: `gh issue create` (GraphQL); on a rate-limit rejection
//! (and only then) one REST `POST repos/{nwo}/issues` carrying title, body and
//! labels together; on a 5xx server error the mutation may have committed, so
//! the recent-issues listing is read first and an exact-title match is adopted.
//! An unreadable listing, or a full page with no match, means no POST.
//!
//! Every outcome is a [`Created`] or a [`CreateError`] with a stable `kind`, so
//! a caller checks one exit code or one JSON field and never scrapes text. Exit
//! 0 with an empty URL is a failure (`may_exist`), never a success (#8289).
//!
//! `gh` is reached through the [`Gh`] trait so the transport logic is tested
//! against a stub; the daemon has no HTTP client by house style (`forge_cmd`).

use std::io::Write;

use serde_json::json;

use crate::intake_reconcile::{is_loom_label, TRIAGE_LABEL};

/// Issues enter at `loom:triage` (#11820); these skip Curator admission.
const REFUSED_LABELS: [&str; 2] = ["loom:issue", "loom:curated"];
/// Page size of the reconcile listing; a full page cannot prove absence.
const RECONCILE_PAGE: usize = 20;

/// What a `gh` invocation produced.
#[derive(Debug, Clone, Default)]
pub struct GhOut {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl GhOut {
    fn ok(&self) -> bool {
        self.code == Some(0)
    }
}

/// The one seam to the forge: run `gh <args>` with optional stdin.
pub trait Gh {
    fn run(&self, args: &[String], stdin: Option<&str>) -> GhOut;
}

/// The real `gh` binary.
pub struct RealGh;

impl Gh for RealGh {
    fn run(&self, args: &[String], stdin: Option<&str>) -> GhOut {
        use crate::cmd_out::{CmdOutcome, Unavailable};
        use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};

        // The facade carries no free-form stdin, so a body for `--input -` is
        // staged in a private temp file and the `-` swapped for its path.
        let staged = match stdin.map(stage_input).transpose() {
            Ok(f) => f,
            Err(e) => {
                return GhOut {
                    code: None,
                    stdout: String::new(),
                    stderr: format!("cannot stage gh input: {e}"),
                }
            }
        };
        let mut argv: Vec<String> = args.to_vec();
        if let Some(f) = &staged {
            if let Some(slot) = argv.iter().position(|a| a == "-") {
                argv[slot] = f.path().to_string_lossy().into_owned();
            }
        }
        let write =
            args.first().map(String::as_str) != Some("api") || args.iter().any(|a| a == "POST");
        let (op, intent) = if write {
            ("forge.create_issue.write", AccessIntent::Write)
        } else {
            ("forge.create_issue.reconcile", AccessIntent::Read)
        };
        let outcome = GhInvocation::new(
            Operation::new(op),
            intent,
            GhTarget::None,
            crate::forge_cmd::FORGE_CMD_TIMEOUT,
        )
        .args(argv)
        .run();
        match outcome {
            CmdOutcome::Ran(o) => GhOut {
                code: o.status.code(),
                stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
            },
            CmdOutcome::Unavailable(Unavailable::Spawn(e)) => GhOut {
                code: None,
                stdout: String::new(),
                stderr: format!("cannot run gh: {e}"),
            },
            CmdOutcome::Unavailable(other) => GhOut {
                code: None,
                stdout: String::new(),
                stderr: format!("gh did not answer: {other:?}"),
            },
        }
    }
}

fn stage_input(text: &str) -> std::io::Result<tempfile::NamedTempFile> {
    let mut f = tempfile::NamedTempFile::new()?;
    f.write_all(text.as_bytes())?;
    f.flush()?;
    Ok(f)
}

/// A filing request. `labels` are raw `--label` values (comma lists allowed).
#[derive(Debug, Clone)]
pub struct Request {
    pub repo: String,
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
}

/// A created issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Created {
    pub number: u64,
    pub url: String,
}

/// Why nothing (certain) was created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateError {
    /// `loom:issue` / `loom:curated` requested. No forge call was made.
    RefusedLabel(String),
    /// Rate limited, and the REST fallback was too.
    RateLimited(String),
    /// Credential rejected.
    Auth(String),
    /// `gh issue create` / the REST POST failed definitively.
    RestFailed(String),
    /// Outcome unknown: the issue MAY exist; look before retrying.
    MayExist(String),
}

impl CreateError {
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::RefusedLabel(_) => "refused_label",
            Self::RateLimited(_) => "rate_limited",
            Self::Auth(_) => "auth",
            Self::RestFailed(_) => "rest_failed",
            Self::MayExist(_) => "may_exist",
        }
    }

    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            Self::RefusedLabel(m)
            | Self::RateLimited(m)
            | Self::Auth(m)
            | Self::RestFailed(m)
            | Self::MayExist(m) => m,
        }
    }

    /// Process exit code: 2 = invalid args / refused label, 1 = creation failed.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        if matches!(self, Self::RefusedLabel(_)) {
            2
        } else {
            1
        }
    }

    #[must_use]
    pub fn to_json(&self) -> String {
        json!({
            "error": self.kind(),
            "message": self.message(),
            "may_exist": matches!(self, Self::MayExist(_)),
        })
        .to_string()
    }
}

/// The #5047 five-signature table (mirrors `is_rate_limit_error` in
/// `forge-helpers.sh` and `rate_limit_breaker.rs`).
#[must_use]
pub fn is_rate_limit_error(text: &str) -> bool {
    let t = text.to_lowercase();
    [
        "api rate limit exceeded",
        "api rate limit already exceeded",
        "secondary rate limit",
        "abuse detection mechanism",
        "was submitted too quickly",
    ]
    .iter()
    .any(|s| t.contains(s))
}

/// The #9714 server-error signature: the mutation may have committed.
#[must_use]
pub fn is_server_error(text: &str) -> bool {
    let t = text.to_lowercase();
    t.contains("something went wrong while executing your query")
        || ["http 502", "http 503", "http 504"]
            .iter()
            .any(|s| t.contains(s))
}

fn is_auth_error(text: &str) -> bool {
    let t = text.to_lowercase();
    [
        "bad credentials",
        "http 401",
        "resource not accessible",
        "authentication required",
        "gh auth login",
    ]
    .iter()
    .any(|s| t.contains(s))
}

/// Split comma lists, trim, refuse the intake-skipping labels, and default to
/// `loom:triage` when the caller named no `loom:` label (#10041).
///
/// # Errors
/// [`CreateError::RefusedLabel`] for `loom:issue` / `loom:curated`.
pub fn normalize_labels(raw: &[String]) -> Result<Vec<String>, CreateError> {
    let mut out = Vec::new();
    for item in raw {
        for l in item.split(',').map(str::trim).filter(|l| !l.is_empty()) {
            if REFUSED_LABELS.contains(&l) {
                return Err(CreateError::RefusedLabel(format!(
                    "refusing --label {l}: issues enter at loom:triage and reach {l} through Curator admission. Use --label loom:triage (or no --label: the default)."
                )));
            }
            out.push(l.to_string());
        }
    }
    if !out.iter().any(|l| is_loom_label(l)) {
        out.push(TRIAGE_LABEL.to_string());
    }
    Ok(out)
}

fn issue_number(url: &str) -> Option<u64> {
    url.trim_end_matches('/').rsplit('/').next()?.parse().ok()
}

fn created(url: &str) -> Result<Created, CreateError> {
    let url = url.trim().to_string();
    match issue_number(&url) {
        Some(number) => Ok(Created { number, url }),
        None => Err(CreateError::MayExist(format!(
            "the forge reported success but no issue URL could be read from {url:?}; the issue MAY exist; check the forge before re-filing rather than blind-retrying."
        ))),
    }
}

fn s(v: &str) -> String {
    v.to_string()
}

/// Create the issue. `since` is the RFC 3339 lower bound for the reconcile
/// listing (taken before the attempt, minus clock skew).
///
/// # Errors
/// A [`CreateError`] whose `kind()` says why nothing certain was created.
pub fn create(gh: &dyn Gh, req: &Request, since: &str) -> Result<Created, CreateError> {
    let labels = normalize_labels(&req.labels)?;
    let mut args = vec![
        s("issue"),
        s("create"),
        s("--title"),
        req.title.clone(),
        s("--body"),
        req.body.clone(),
        s("--repo"),
        req.repo.clone(),
    ];
    for l in &labels {
        args.push(s("--label"));
        args.push(l.clone());
    }
    let first = gh.run(&args, None);
    // Exit 0 with empty stdout is NOT a success (#8289).
    if first.ok() && !first.stdout.trim().is_empty() {
        return created(&first.stdout);
    }

    let rate_limited = is_rate_limit_error(&first.stderr);
    let server_err = is_server_error(&first.stderr);
    if !rate_limited && !server_err {
        return Err(unexplained(&first));
    }

    let rest_path = format!("repos/{}/issues", req.repo);
    if !rate_limited {
        // A rate-limit rejection happens before execution; a server error may
        // have committed. Reconcile before any POST (#9714).
        let list = gh.run(
            &[
                s("api"),
                format!("{rest_path}?state=all&sort=created&direction=desc&per_page={RECONCILE_PAGE}&since={since}"),
            ],
            None,
        );
        let rows: Option<Vec<serde_json::Value>> = list
            .ok()
            .then(|| serde_json::from_str(&list.stdout).ok())
            .flatten();
        let Some(rows) = rows else {
            return Err(CreateError::MayExist(format!(
                "gh issue create hit a server error, and the duplicate probe failed or could not prove absence, so no REST retry was made. The issue MAY exist; check the forge before re-filing rather than blind-retrying. ({})",
                first.stderr.trim()
            )));
        };
        let want = req.title.trim();
        let hit = rows.iter().find(|r| {
            r.get("pull_request").is_none_or(serde_json::Value::is_null)
                && r.get("title")
                    .and_then(|t| t.as_str())
                    .is_some_and(|t| t.trim() == want)
        });
        if let Some(url) = hit.and_then(|r| r.get("html_url")).and_then(|u| u.as_str()) {
            return created(url);
        }
        if rows.len() >= RECONCILE_PAGE {
            return Err(CreateError::MayExist(format!(
                "gh issue create hit a server error, and the duplicate probe returned a full page with no match, so no REST retry was made. The issue MAY exist; check the forge before re-filing rather than blind-retrying. ({})",
                first.stderr.trim()
            )));
        }
    }

    // One POST carries title + body + labels together.
    let payload = json!({"title": req.title, "body": req.body, "labels": labels}).to_string();
    let post = gh.run(
        &[
            s("api"),
            s("--method"),
            s("POST"),
            rest_path,
            s("--input"),
            s("-"),
            s("--jq"),
            s(".html_url"),
        ],
        Some(&payload),
    );
    if post.ok() {
        return created(&post.stdout);
    }
    let detail = format!(
        "gh issue create failed ({}), and the REST fallback also failed ({}).",
        first.stderr.trim(),
        post.stderr.trim()
    );
    Err(if is_rate_limit_error(&post.stderr) {
        CreateError::RateLimited(detail)
    } else if is_auth_error(&post.stderr) {
        CreateError::Auth(detail)
    } else {
        CreateError::RestFailed(detail)
    })
}

/// Failure that is neither a rate limit nor a server error. Never silent: the
/// forge's own words when it gave any, else a synthesized line (#8289).
fn unexplained(out: &GhOut) -> CreateError {
    let err = out.stderr.trim();
    if out.ok() || (err.is_empty() && out.stdout.trim().is_empty()) {
        return CreateError::MayExist(format!(
            "gh issue create produced no error text (exit {}) and no usable issue URL (stdout: {}); killed, or a wrapper that failed silently? The issue MAY exist; check the forge before re-filing rather than blind-retrying.",
            out.code.map_or_else(|| "signal".to_string(), |c| c.to_string()),
            if out.stdout.trim().is_empty() { "<empty>" } else { out.stdout.trim() },
        ));
    }
    let text = if err.is_empty() {
        out.stdout.trim()
    } else {
        err
    };
    if is_auth_error(text) {
        CreateError::Auth(text.to_string())
    } else {
        CreateError::RestFailed(text.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Stub {
        replies: RefCell<Vec<GhOut>>,
        calls: RefCell<Vec<(Vec<String>, Option<String>)>>,
    }

    impl Stub {
        fn new(mut replies: Vec<GhOut>) -> Self {
            replies.reverse();
            Self {
                replies: RefCell::new(replies),
                calls: RefCell::new(vec![]),
            }
        }
        fn posts(&self) -> usize {
            self.calls
                .borrow()
                .iter()
                .filter(|(a, _)| a.iter().any(|x| x == "POST"))
                .count()
        }
    }

    impl Gh for Stub {
        fn run(&self, args: &[String], stdin: Option<&str>) -> GhOut {
            self.calls
                .borrow_mut()
                .push((args.to_vec(), stdin.map(str::to_string)));
            self.replies
                .borrow_mut()
                .pop()
                .expect("unexpected extra gh call")
        }
    }

    fn out(code: i32, stdout: &str, stderr: &str) -> GhOut {
        GhOut {
            code: Some(code),
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    fn req(labels: &[&str]) -> Request {
        Request {
            repo: "o/r".into(),
            title: "T".into(),
            body: "B".into(),
            labels: labels.iter().map(|l| (*l).to_string()).collect(),
        }
    }

    const URL: &str = "https://github.com/o/r/issues/7\n";
    const RL: &str = "GraphQL: API rate limit already exceeded for user ID 1";

    #[test]
    fn label_guard_refuses_and_defaults() {
        assert_eq!(normalize_labels(&[]).unwrap(), vec!["loom:triage"]);
        assert_eq!(normalize_labels(&["bug".into()]).unwrap(), vec!["bug", "loom:triage"]);
        assert_eq!(
            normalize_labels(&["bug, loom:architect".into()]).unwrap(),
            vec!["bug", "loom:architect"]
        );
        for bad in ["loom:issue", "bug,loom:curated", " loom:issue "] {
            let e = normalize_labels(&[bad.into()]).unwrap_err();
            assert_eq!(e.kind(), "refused_label");
            assert_eq!(e.exit_code(), 2);
        }
    }

    #[test]
    fn refused_label_makes_no_forge_call() {
        let gh = Stub::new(vec![]);
        assert_eq!(create(&gh, &req(&["loom:issue"]), "t").unwrap_err().kind(), "refused_label");
        assert!(gh.calls.borrow().is_empty());
    }

    #[test]
    fn plain_success() {
        let gh = Stub::new(vec![out(0, URL, "")]);
        let c = create(&gh, &req(&[]), "t").unwrap();
        assert_eq!(
            c,
            Created {
                number: 7,
                url: URL.trim().into()
            }
        );
        assert!(gh.calls.borrow()[0].0.contains(&"loom:triage".to_string()));
    }

    #[test]
    fn rate_limit_falls_back_to_one_rest_post_with_labels() {
        let gh = Stub::new(vec![out(1, "", RL), out(0, URL, "")]);
        assert_eq!(create(&gh, &req(&["bug"]), "t").unwrap().number, 7);
        assert_eq!(gh.posts(), 1);
        let payload = gh.calls.borrow()[1].1.clone().unwrap();
        let v: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(v["labels"], json!(["bug", "loom:triage"]));
        assert_eq!(v["title"], "T");
    }

    #[test]
    fn rate_limit_then_rest_failure_is_a_structured_error() {
        let gh = Stub::new(vec![
            out(1, "", RL),
            out(1, "", "HTTP 403: API rate limit exceeded"),
        ]);
        let e = create(&gh, &req(&[]), "t").unwrap_err();
        assert_eq!(e.kind(), "rate_limited");
        assert_eq!(e.exit_code(), 1);
        let gh = Stub::new(vec![out(1, "", RL), out(1, "", "HTTP 401: Bad credentials")]);
        assert_eq!(create(&gh, &req(&[]), "t").unwrap_err().kind(), "auth");
        let gh = Stub::new(vec![out(1, "", RL), out(1, "", "HTTP 422")]);
        assert_eq!(create(&gh, &req(&[]), "t").unwrap_err().kind(), "rest_failed");
    }

    #[test]
    fn non_rate_limit_failure_never_posts() {
        let gh = Stub::new(vec![out(1, "", "HTTP 422: Validation Failed")]);
        assert_eq!(create(&gh, &req(&[]), "t").unwrap_err().kind(), "rest_failed");
        assert_eq!(gh.posts(), 0);
    }

    #[test]
    fn exit_zero_with_empty_stdout_is_may_exist() {
        let gh = Stub::new(vec![out(0, "  \n", "")]);
        let e = create(&gh, &req(&[]), "t").unwrap_err();
        assert_eq!(e.kind(), "may_exist");
        assert!(e.to_json().contains("\"may_exist\":true"));
        assert_eq!(gh.posts(), 0);
        let gh = Stub::new(vec![out(1, "", "")]);
        assert_eq!(create(&gh, &req(&[]), "t").unwrap_err().kind(), "may_exist");
    }

    #[test]
    fn server_error_adopts_exact_title_match_without_post() {
        let listing = r#"[{"title":"other","html_url":"u1"},{"title":" T ","html_url":"https://github.com/o/r/issues/9"},{"title":"T","html_url":"pr","pull_request":{}}]"#;
        let gh = Stub::new(vec![out(1, "", "HTTP 502: Bad Gateway"), out(0, listing, "")]);
        assert_eq!(create(&gh, &req(&[]), "t").unwrap().number, 9);
        assert_eq!(gh.posts(), 0);
    }

    #[test]
    fn server_error_with_empty_listing_posts() {
        let gh = Stub::new(vec![out(1, "", "HTTP 503"), out(0, "[]", ""), out(0, URL, "")]);
        assert_eq!(create(&gh, &req(&[]), "t").unwrap().number, 7);
        assert_eq!(gh.posts(), 1);
    }

    #[test]
    fn server_error_unreadable_or_full_listing_never_posts() {
        for listing in [out(1, "", "boom"), out(0, "not json", "")] {
            let gh = Stub::new(vec![out(1, "", "HTTP 502"), listing]);
            assert_eq!(create(&gh, &req(&[]), "t").unwrap_err().kind(), "may_exist");
            assert_eq!(gh.posts(), 0);
        }
        let full = serde_json::to_string(
            &(0..20)
                .map(|i| json!({"title": format!("x{i}")}))
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let gh = Stub::new(vec![
            out(1, "", "Something went wrong while executing your query"),
            out(0, &full, ""),
        ]);
        assert_eq!(create(&gh, &req(&[]), "t").unwrap_err().kind(), "may_exist");
        assert_eq!(gh.posts(), 0);
    }
}
