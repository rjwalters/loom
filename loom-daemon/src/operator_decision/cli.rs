//! `loom-daemon operator-decision validate|apply` — the entry points.
//!
//! Stdout is `KEY=VALUE` lines (plus the composed body under `--dry-run`);
//! refusal reasons go to stderr, one `REASON=` line each. The exit code is the
//! contract callers branch on — see [`exit`].
//!
//! Forge access goes through the [`Forge`] trait so the ordering rules (no
//! forge call at all on a refusal; body before label; nothing mutated under
//! `--dry-run`) are tested against a recording fake, not a live repo. The real
//! implementation, [`GhForge`], uses REST (`gh api`) for every read and write
//! — the GraphQL pool exhausts first under fleet load (#5047) — and files new
//! issues through `.loom/scripts/create-issue.sh`, which carries the REST
//! fallback, the duplicate backstop and the filing lock.

use super::render::{compose_body, render_section};
use super::validate::{parse, validate};
use super::{Decision, DECISION_LABEL, MALFORMED_LABEL};
use crate::cmd_out::{run_command, CmdOutcome};
use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Exit codes.
pub mod exit {
    /// Valid / applied (or would apply, under `--dry-run`).
    pub const OK: i32 = 0;
    /// The input fails the contract; nothing was touched. Reasons on stderr.
    pub const REFUSED: i32 = 1;
    /// Bad usage or unreadable input file.
    pub const USAGE: i32 = 2;
    /// A forge read or write failed. Under `--new`, `create-issue.sh`'s own
    /// 3 (duplicate backstop) and 75 (filing lock deferred) pass through
    /// unchanged instead, because callers already branch on them.
    pub const FORGE: i32 = 4;
}

/// What an existing issue looks like right before the write.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IssueState {
    pub body: String,
    pub labels: Vec<String>,
}

/// The forge operations `apply` needs. Errors carry a message for stderr.
pub trait Forge {
    fn view(&mut self, issue: u64) -> Result<IssueState, String>;
    fn set_body(&mut self, issue: u64, body: &str) -> Result<(), String>;
    fn add_labels(&mut self, issue: u64, labels: &[String]) -> Result<(), String>;
    fn remove_label(&mut self, issue: u64, label: &str) -> Result<(), String>;
    /// File a new issue; `Ok(url)`, or `Err((exit_code, message))`.
    fn create(
        &mut self,
        title: &str,
        body: &str,
        labels: &[String],
    ) -> Result<String, (i32, String)>;
}

/// Where `apply` writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Relabel mode: rewrite this issue's body and label it.
    Existing(u64),
    /// Filing mode: open a new issue with this title.
    New { title: String },
}

/// Everything `apply` needs besides the input and the forge.
#[derive(Debug, Clone)]
pub struct ApplyRequest {
    pub target: Target,
    pub also_labels: Vec<String>,
    pub remove_labels: Vec<String>,
    pub dry_run: bool,
}

/// Parse + validate `input`, reporting every reason on `err`. `Ok` only for a
/// contract-valid decision.
pub fn check(input: &str, err: &mut dyn Write) -> Result<Decision, i32> {
    let reasons = match parse(input) {
        Ok(d) => {
            let r = validate(&d);
            if r.is_empty() {
                return Ok(d);
            }
            r
        }
        Err(r) => vec![r],
    };
    let _ = writeln!(
        err,
        "operator-decision: REFUSED - the input fails the ranked-options contract \
         (see .loom/docs/operator-decision.md); nothing was changed"
    );
    for r in &reasons {
        let _ = writeln!(err, "REASON={r}");
    }
    Err(exit::REFUSED)
}

/// `validate`: exit 0 and a summary when valid, 1 with every reason otherwise.
pub fn validate_cmd(input: &str, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match check(input, err) {
        Ok(d) => {
            let _ = writeln!(out, "VALID=true");
            let _ = writeln!(out, "OPTIONS={}", d.options.len());
            let _ = writeln!(out, "RECOMMENDED={}", d.recommended.unwrap_or_default());
            exit::OK
        }
        Err(code) => {
            let _ = writeln!(out, "VALID=false");
            code
        }
    }
}

fn push_unique(v: &mut Vec<String>, s: &str) {
    let s = s.trim();
    if !s.is_empty() && !v.iter().any(|x| x == s) {
        v.push(s.to_string());
    }
}

/// `label` with a retired name read as its replacement (#11795): an
/// `--also-label loom:operator-only` from a not-yet-updated caller adds
/// `loom:operator`, never a retired name. A retired label with no replacement
/// maps to `""`, which `push_unique` drops.
fn current_name(label: &str) -> &str {
    match crate::label_registry::Registry::embedded().get_retired(label) {
        Some(r) => r.replaced_by.as_deref().unwrap_or(""),
        None => label,
    }
}

/// `apply`. Refuses before ANY forge call when the input is invalid; writes
/// the body before applying any label, so a failed body edit never leaves a
/// `loom:operator` decision label on a free-prose body.
pub fn apply(
    forge: &mut dyn Forge,
    input: &str,
    req: &ApplyRequest,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let d = match check(input, err) {
        Ok(d) => d,
        Err(code) => return code,
    };
    let mut wanted = vec![DECISION_LABEL.to_string()];
    for l in &req.also_labels {
        push_unique(&mut wanted, current_name(l.trim()));
    }

    match &req.target {
        Target::New { title } => {
            let body = render_section(&d);
            if req.dry_run {
                let _ = writeln!(out, "DRY_RUN=true");
                let _ = writeln!(out, "ACTION=create");
                let _ = writeln!(out, "LABELS_ADD={}", wanted.join(","));
                let _ = writeln!(out, "BODY<<EOF\n{body}EOF");
                return exit::OK;
            }
            match forge.create(title, &body, &wanted) {
                Ok(url) => {
                    let _ = writeln!(out, "CREATED={url}");
                    let _ = writeln!(out, "LABELS_ADD={}", wanted.join(","));
                    exit::OK
                }
                Err((code, msg)) => {
                    let _ = writeln!(err, "operator-decision: could not file the issue: {msg}");
                    code
                }
            }
        }
        Target::Existing(issue) => {
            let issue = *issue;
            // Fresh read immediately before the write: concurrent curators
            // clobber each other's body edits.
            let state = match forge.view(issue) {
                Ok(s) => s,
                Err(e) => {
                    let _ = writeln!(err, "operator-decision: could not read issue #{issue}: {e}");
                    return exit::FORGE;
                }
            };
            let body = compose_body(&state.body, &d);
            let has = |l: &str| state.labels.iter().any(|x| x == l);
            let add: Vec<String> = wanted.iter().filter(|l| !has(l)).cloned().collect();
            let mut remove = Vec::new();
            for l in &req.remove_labels {
                push_unique(&mut remove, l);
            }
            // A valid body is the repair for a loom-ui bounce.
            push_unique(&mut remove, MALFORMED_LABEL);
            remove.retain(|l| has(l) && !wanted.contains(l));
            let body_changed = body != state.body;

            if req.dry_run {
                let _ = writeln!(out, "DRY_RUN=true");
                let _ = writeln!(out, "ACTION=relabel");
                let _ = writeln!(out, "ISSUE={issue}");
                let _ = writeln!(out, "BODY_CHANGED={body_changed}");
                let _ = writeln!(out, "LABELS_ADD={}", add.join(","));
                let _ = writeln!(out, "LABELS_REMOVE={}", remove.join(","));
                let _ = writeln!(out, "BODY<<EOF\n{body}EOF");
                return exit::OK;
            }
            if body_changed {
                if let Err(e) = forge.set_body(issue, &body) {
                    let _ = writeln!(
                        err,
                        "operator-decision: body write to #{issue} failed, no label applied: {e}"
                    );
                    return exit::FORGE;
                }
            }
            if !add.is_empty() {
                if let Err(e) = forge.add_labels(issue, &add) {
                    let _ = writeln!(err, "operator-decision: labeling #{issue} failed: {e}");
                    return exit::FORGE;
                }
            }
            for l in &remove {
                if let Err(e) = forge.remove_label(issue, l) {
                    let _ =
                        writeln!(err, "operator-decision: removing {l} from #{issue} failed: {e}");
                    return exit::FORGE;
                }
            }
            let _ = writeln!(out, "APPLIED=true");
            let _ = writeln!(out, "ISSUE={issue}");
            let _ = writeln!(out, "BODY_CHANGED={body_changed}");
            let _ = writeln!(out, "LABELS_ADD={}", add.join(","));
            let _ = writeln!(out, "LABELS_REMOVE={}", remove.join(","));
            exit::OK
        }
    }
}

/// Journal a decision [`apply`] filed on an existing issue (exit `0`, not a
/// dry run) for the role run's verdict record (#12027). Best-effort and a
/// no-op outside a journaled run: it never changes output or exit.
pub fn journal_applied(req: &ApplyRequest, code: i32) {
    if let (exit::OK, false, Target::Existing(issue)) = (code, req.dry_run, &req.target) {
        crate::observability::pick_journal::record_applied(
            *issue,
            false,
            "escalated",
            "operator_decision",
        );
    }
}

/// Read `--input`: a path, or `-` for stdin.
pub fn read_input(path: &Path) -> Result<String, String> {
    if path.as_os_str() == "-" {
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .map_err(|e| format!("--input -: {e}"))?;
        return Ok(s);
    }
    std::fs::read_to_string(path).map_err(|e| format!("--input {}: {e}", path.display()))
}

/// The real forge: `gh api` (REST) for reads and writes, `create-issue.sh`
/// for filing.
///
/// Every REST write (body, label add, label remove) is first vetted by
/// [`crate::write_scope::may_write_from`] (#9548): `--repo OWNER/REPO`
/// accepts any repository, so a write must target a managed repo the
/// credential can write. The verdict is computed once per `GhForge`. Filing
/// (`create`) is vetted by `create-issue.sh` itself (`loom_write_repo`).
pub struct GhForge {
    repo_root: PathBuf,
    /// `owner/repo`; `None` lets `gh` resolve `{owner}/{repo}` from the
    /// checkout's remote.
    repo: Option<String>,
    /// Memoized write-scope verdict: `Err(reason)` refuses every write.
    write_ok: Option<Result<(), String>>,
    /// `Some(gh)`: vet writes under the workspace's own per-owner credential
    /// with this `gh` (#11437), as the tick's gate does. `None` (the default,
    /// CLI callers) vets under the credential the write carries.
    root_credential_gh: Option<PathBuf>,
    /// The call site a refusal's `write_scope.refused` record names.
    call_site: &'static str,
}

const GH_TIMEOUT: Duration = Duration::from_secs(60);
/// `create-issue.sh` may wait on the machine-wide filing lock and run the
/// duplicate scan, so it gets a longer deadline.
const CREATE_TIMEOUT: Duration = Duration::from_secs(300);

/// Percent-encode one URL path segment (label names may carry spaces).
fn encode_segment(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b':') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The write-scope verdict for `repo` (or the root's origin): under the
/// root's own credential when `gh` is given (#11437), else under the
/// credential the write itself will carry (#12606: the per-owner writer the
/// `gh` facade resolves for `root` and `repo`, else the process's). A refusal
/// is exported as `write_scope.refused` with call site `what`.
fn scope_verdict(
    root: &std::path::Path,
    repo: Option<&str>,
    gh: Option<&std::path::Path>,
    what: &str,
) -> crate::write_scope::Verdict {
    match (gh, repo.map(str::trim).filter(|r| !r.is_empty())) {
        (Some(gh), Some(r)) => crate::write_scope::repo_writable_with_at(root, r, gh, what),
        (Some(gh), None) => crate::write_scope::root_writable_with_at(root, gh, what),
        (None, _) => crate::write_scope::may_write_from_at(root, repo, what),
    }
}

impl GhForge {
    #[must_use]
    pub fn new(repo_root: PathBuf, repo: Option<String>) -> Self {
        Self {
            repo_root,
            repo,
            write_ok: None,
            root_credential_gh: None,
            call_site: "operator_decision.forge",
        }
    }

    /// Name the call site a refused write's `write_scope.refused` record
    /// carries (#12606).
    #[must_use]
    pub fn with_call_site(mut self, what: &'static str) -> Self {
        self.call_site = what;
        self
    }

    /// Like [`Self::new`], but writes are vetted under the root's own
    /// per-owner credential probed with `gh_bin` (#11437) — the credential
    /// the tick's gate admitted the root under, and the one the write uses.
    #[must_use]
    pub fn for_root_credential(repo_root: PathBuf, repo: Option<String>, gh_bin: PathBuf) -> Self {
        Self {
            root_credential_gh: Some(gh_bin),
            ..Self::new(repo_root, repo)
        }
    }

    /// Refuse a forge write the write scope denies (#9548).
    fn check_write(&mut self) -> Result<(), String> {
        let (root, repo) = (&self.repo_root, self.repo.as_deref());
        let gh = self.root_credential_gh.as_deref();
        let what = self.call_site;
        self.write_ok
            .get_or_insert_with(|| match scope_verdict(root, repo, gh, what) {
                crate::write_scope::Verdict::Allow(_) => Ok(()),
                crate::write_scope::Verdict::Deny(why) => {
                    Err(format!("refusing the write (#9548): {why}"))
                }
            })
            .clone()
    }

    fn issue_path(&self, issue: u64) -> String {
        let slug = self.repo.as_deref().unwrap_or("{owner}/{repo}");
        format!("repos/{slug}/issues/{issue}")
    }

    /// `open` / `closed` for issue (or PR) `n`, over REST — `park-record
    /// apply`'s closed-blocker refusal (#10152).
    pub fn issue_state(&self, n: u64) -> Result<String, String> {
        self.read_state(self.repo.as_deref(), &self.issue_path(n))
    }

    /// Like [`Self::issue_state`], for an issue/PR in another repository
    /// (`OWNER/REPO`) — a cross-repo park blocker is read in its own repo
    /// (#10443).
    pub fn issue_state_in(&self, repo: &str, n: u64) -> Result<String, String> {
        self.read_state(Some(repo), &format!("repos/{repo}/issues/{n}"))
    }

    fn read_state(&self, repo: Option<&str>, path: &str) -> Result<String, String> {
        #[derive(serde::Deserialize)]
        struct Raw {
            state: String,
        }
        let r = self.gh_for(repo, AccessIntent::Read, &["api", path]);
        let Some(o) = r.ok_output() else {
            return Err(r.failure_reason(&format!("gh api {path}")));
        };
        let raw: Raw = serde_json::from_slice(&o.stdout).map_err(|e| e.to_string())?;
        Ok(raw.state)
    }

    fn gh(&self, intent: AccessIntent, args: &[&str]) -> CmdOutcome {
        self.gh_for(self.repo.as_deref(), intent, args)
    }

    fn gh_for(&self, repo: Option<&str>, intent: AccessIntent, args: &[&str]) -> CmdOutcome {
        let target = repo
            .and_then(|r| GhTarget::repo(r).ok())
            .unwrap_or(GhTarget::None);
        GhInvocation::new(Operation::new("api.rest"), intent, target, GH_TIMEOUT)
            .args(args)
            .current_dir(&self.repo_root)
            .run()
    }

    /// `gh api -X <method> <path> --input <json file>`.
    fn send_json(
        &self,
        method: &str,
        path: &str,
        payload: &serde_json::Value,
    ) -> Result<(), String> {
        let mut f = tempfile::NamedTempFile::new().map_err(|e| e.to_string())?;
        f.write_all(payload.to_string().as_bytes())
            .map_err(|e| e.to_string())?;
        let file = f.path().to_string_lossy().to_string();
        let r = self.gh(AccessIntent::Write, &["api", "-X", method, path, "--input", &file]);
        if r.succeeded() {
            Ok(())
        } else {
            Err(r.failure_reason(&format!("gh api -X {method} {path}")))
        }
    }
}

impl Forge for GhForge {
    fn view(&mut self, issue: u64) -> Result<IssueState, String> {
        #[derive(serde::Deserialize)]
        struct Raw {
            #[serde(default)]
            body: Option<String>,
            #[serde(default)]
            labels: Vec<RawLabel>,
        }
        #[derive(serde::Deserialize)]
        struct RawLabel {
            name: String,
        }
        let path = self.issue_path(issue);
        let r = self.gh(AccessIntent::Read, &["api", &path]);
        let Some(o) = r.ok_output() else {
            return Err(r.failure_reason(&format!("gh api {path}")));
        };
        let raw: Raw = serde_json::from_slice(&o.stdout).map_err(|e| e.to_string())?;
        Ok(IssueState {
            body: raw.body.unwrap_or_default(),
            labels: raw.labels.into_iter().map(|l| l.name).collect(),
        })
    }

    fn set_body(&mut self, issue: u64, body: &str) -> Result<(), String> {
        self.check_write()?;
        let path = self.issue_path(issue);
        self.send_json("PATCH", &path, &serde_json::json!({ "body": body }))
    }

    fn add_labels(&mut self, issue: u64, labels: &[String]) -> Result<(), String> {
        self.check_write()?;
        let path = format!("{}/labels", self.issue_path(issue));
        self.send_json("POST", &path, &serde_json::json!({ "labels": labels }))
    }

    fn remove_label(&mut self, issue: u64, label: &str) -> Result<(), String> {
        self.check_write()?;
        let path = format!("{}/labels/{}", self.issue_path(issue), encode_segment(label));
        let r = self.gh(AccessIntent::Write, &["api", "-X", "DELETE", &path]);
        if r.succeeded() {
            Ok(())
        } else {
            Err(r.failure_reason(&format!("gh api -X DELETE {path}")))
        }
    }

    fn create(
        &mut self,
        title: &str,
        body: &str,
        labels: &[String],
    ) -> Result<String, (i32, String)> {
        let script = self
            .repo_root
            .join(".loom")
            .join("scripts")
            .join("create-issue.sh");
        let mut f = tempfile::NamedTempFile::new().map_err(|e| (exit::FORGE, e.to_string()))?;
        f.write_all(body.as_bytes())
            .map_err(|e| (exit::FORGE, e.to_string()))?;
        let mut cmd = Command::new(&script);
        cmd.arg("--title")
            .arg(title)
            .arg("--body-file")
            .arg(f.path());
        for l in labels {
            cmd.arg("--label").arg(l);
        }
        if let Some(repo) = &self.repo {
            cmd.arg("--repo").arg(repo);
        }
        cmd.current_dir(&self.repo_root)
            .stdin(std::process::Stdio::null());
        match run_command(cmd, CREATE_TIMEOUT) {
            CmdOutcome::Ran(o) if o.status.success() => {
                Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
            }
            CmdOutcome::Ran(o) => {
                let code = match o.status.code() {
                    Some(c @ (3 | 75)) => c,
                    _ => exit::FORGE,
                };
                Err((code, String::from_utf8_lossy(&o.stderr).trim().to_string()))
            }
            other => Err((exit::FORGE, other.failure_reason("create-issue.sh"))),
        }
    }
}

/// Resolve the checkout root `create-issue.sh` and `gh` run in.
#[must_use]
pub fn default_repo_root() -> PathBuf {
    crate::repo_root::find_repo_root_from_cwd().unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod journal_tests {
    use super::*;
    use crate::observability::pick_journal::{parse, set_test_journal, JournalEntry};

    fn journaled(req: &ApplyRequest, code: i32) -> Vec<JournalEntry> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("j.jsonl");
        set_test_journal(Some(path.clone()));
        journal_applied(req, code);
        set_test_journal(None);
        parse(&std::fs::read_to_string(&path).unwrap_or_default())
    }

    #[test]
    fn a_landed_decision_on_an_issue_journals_one_escalated_write() {
        let req = ApplyRequest {
            target: Target::Existing(42),
            also_labels: Vec::new(),
            remove_labels: Vec::new(),
            dry_run: false,
        };
        assert!(matches!(
            journaled(&req, exit::OK).as_slice(),
            [JournalEntry::Applied { number: 42, action, by, .. }]
                if action == "escalated" && by == "operator_decision"
        ));
        assert!(journaled(&req, exit::FORGE).is_empty(), "a failed apply");
        assert!(journaled(&req, exit::REFUSED).is_empty(), "a refused apply");
        let dry = ApplyRequest {
            dry_run: true,
            ..req.clone()
        };
        assert!(journaled(&dry, exit::OK).is_empty(), "a dry run wrote nothing");
        let new = ApplyRequest {
            target: Target::New {
                title: "t".to_string(),
            },
            ..req
        };
        assert!(journaled(&new, exit::OK).is_empty(), "a new filing is no verdict");
    }
}
