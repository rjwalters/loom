//! `loom-daemon forge verdict-gate | verdict-labels` (#10581): the I/O half of
//! [`crate::verdict_gate`], called by `post-verdict.sh` before and after
//! it posts a verdict comment. Also `forge doctor-handback` (#9388), the I/O
//! half of [`crate::doctor_handback`]. Reads and writes go through REST (`gh api`),
//! never GraphQL, so they survive an exhausted GraphQL pool.

use std::path::Path;

use crate::claim_reconciliation::VerdictKind;
use crate::verdict_gate::{self as gate, Decision, GateInput};
use anyhow::Result;
use serde_json::Value;

fn verdict_or_exit(cmd: &str, verdict: &str) -> VerdictKind {
    gate::parse_verdict(verdict).unwrap_or_else(|| {
        eprintln!("forge {cmd}: --verdict must be approved or changes-requested, got {verdict:?}");
        std::process::exit(2)
    })
}

/// The `ROLE_ACTIONS` word a verdict's label write journals as (#12362).
const fn journal_action(kind: VerdictKind) -> &'static str {
    match kind {
        VerdictKind::Approved => "approved",
        VerdictKind::ChangesRequested => "changes_requested",
    }
}

/// The PR's label names, live (uncached). `None` when the read failed.
fn read_labels(repo: &str, pr: u64, root: &Path) -> Option<Vec<String>> {
    let path = format!("repos/{repo}/issues/{pr}/labels?per_page=100");
    let out = crate::script_helpers::gh_read("verdict.labels", &["api", &path], root, false);
    let doc = serde_json::from_slice::<Value>(&out.ok_output()?.stdout).ok()?;
    gate::label_names(&doc)
}

/// `forge verdict-body-check` (#9258): classify the body on stdin. An
/// unreadable stdin is a rejection, never a pass.
pub(crate) fn verdict_body_check() -> Result<()> {
    use std::io::Read;
    let mut body = String::new();
    let result = match std::io::stdin().read_to_string(&mut body) {
        Ok(_) => crate::verdict_body::check(&body),
        Err(_) => Err(crate::verdict_body::Reject::Empty),
    };
    let (line, code) = crate::verdict_body::render(result);
    println!("{line}");
    std::process::exit(code)
}

/// The body-edit history of issue `n` (GraphQL `userContentEdits`; REST has no
/// edit time). `None` when the read failed: never a pass (#12714).
///
/// `n` may name a pull request (`Part of #N` often points at one), so the
/// field is read through the `Comment` interface both types implement; an
/// `... on Issue`-only fragment returns no `userContentEdits` for a PR, and
/// that `None` would refuse every approval linking it, for good.
fn read_body_edits(repo: &str, n: u64, root: &Path) -> Option<Vec<gate::BodyEdit>> {
    let (owner, name) = repo.split_once('/')?;
    let query = "query($o:String!,$n:String!,$i:Int!){repository(owner:$o,name:$n){\
                 issueOrPullRequest(number:$i){... on Comment{userContentEdits(first:50){nodes{\
                 editedAt editor{login __typename}}}}}}}";
    let args = [
        "api".to_string(),
        "graphql".into(),
        "-f".into(),
        format!("query={query}"),
        "-f".into(),
        format!("o={owner}"),
        "-f".into(),
        format!("n={name}"),
        "-F".into(),
        format!("i={n}"),
    ];
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let out =
        crate::script_helpers::gh_route::gh_read_for_repo("verdict.body_edits", &argv, root, repo);
    let doc = serde_json::from_slice::<Value>(&out.ok_output()?.stdout).ok()?;
    let nodes = doc
        .pointer("/data/repository/issueOrPullRequest/userContentEdits/nodes")?
        .as_array()?;
    nodes
        .iter()
        .map(|e| {
            let at = chrono::DateTime::parse_from_rfc3339(e.get("editedAt")?.as_str()?).ok()?;
            let kind = e.pointer("/editor/__typename").and_then(Value::as_str);
            Some(gate::BodyEdit {
                at: at.with_timezone(&chrono::Utc),
                editor: e
                    .pointer("/editor/login")
                    .and_then(Value::as_str)
                    .unwrap_or("an unknown editor")
                    .to_string(),
                bot: matches!(kind, Some("Bot" | "Mannequin")),
            })
        })
        .collect()
}

#[cfg(test)]
#[test]
fn body_edits_use_the_named_repositorys_reader() {
    let response = serde_json::json!({
        "data": {"repository": {"issueOrPullRequest": {"userContentEdits": {"nodes": []}}}}
    });
    crate::gh_invocation::test_routing::assert_pooled_query("target/widget", &response, |root| {
        read_body_edits("target/widget", 7, root).is_some_and(|edits| edits.is_empty())
    });
}

/// The issues an approval is checked against for a stale read (#12614): those
/// the PR body closes plus every `--issue-body-sha N=HASH`, each re-read now.
/// `None` when the PR itself could not be read.
fn read_linked_issues(
    repo: &str,
    pr: u64,
    hashes: &[(u64, String)],
    root: &Path,
) -> Option<Vec<gate::IssueSnapshot>> {
    let pr_obj =
        crate::comment_trust::records::fetch_issue_object(repo, &pr.to_string(), root, false)?;
    let body = pr_obj.get("body").and_then(Value::as_str).unwrap_or("");
    let mut numbers = gate::closing_issues(body);
    numbers.extend(hashes.iter().map(|(n, _)| *n));
    numbers.sort_unstable();
    numbers.dedup();
    Some(
        numbers
            .into_iter()
            .map(|n| {
                let ns = n.to_string();
                let obj = crate::comment_trust::records::fetch_issue_object(repo, &ns, root, false);
                gate::IssueSnapshot {
                    number: n,
                    expected_body_sha: hashes.iter().find(|(h, _)| *h == n).map(|(_, v)| v.clone()),
                    body: obj.map(|o| {
                        o.get("body")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string()
                    }),
                    comments: crate::comment_trust::records::fetch_trusted_comments(
                        repo, &ns, root, false,
                    ),
                    edits: read_body_edits(repo, n, root),
                }
            })
            .collect(),
    )
}

/// `forge verdict-gate`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn verdict_gate(
    pr: u64,
    repo: &str,
    verdict: &str,
    sha: &str,
    overrule: &str,
    window_secs: i64,
    review_started: Option<&str>,
    issue_body_sha: &[String],
    acknowledged_comment: &[u64],
) -> Result<()> {
    let kind = verdict_or_exit("verdict-gate", verdict);
    let root = super::forge_identity_cmd::workspace();
    let flag_started = review_started.map(|s| {
        chrono::DateTime::parse_from_rfc3339(s.trim())
            .map(|d| d.with_timezone(&chrono::Utc))
            .unwrap_or_else(|_| {
                eprintln!("forge verdict-gate: --review-started must be RFC 3339, got {s:?}");
                std::process::exit(2)
            })
    });
    let hashes: Vec<(u64, String)> = issue_body_sha
        .iter()
        .map(|a| {
            a.split_once('=')
                .and_then(|(n, h)| {
                    Some((n.trim().trim_start_matches('#').parse().ok()?, h.trim().to_string()))
                })
                .unwrap_or_else(|| {
                    eprintln!("forge verdict-gate: --issue-body-sha must be N=HASH, got {a:?}");
                    std::process::exit(2)
                })
        })
        .collect();
    let comments =
        crate::comment_trust::records::fetch_trusted_comments(repo, &pr.to_string(), &root, false);
    // The start comes from the dispatch, not the agent (#12714): the agent's
    // value may only move it earlier.
    let dispatch = std::env::var("LOOM_SWEEP_ID")
        .ok()
        .and_then(|id| gate::dispatch_epoch(&id))
        .or_else(|| comments.as_deref().and_then(gate::lease_start));
    let started = gate::resolve_start(flag_started, dispatch);
    let issues = match (started, kind) {
        (Some(_), VerdictKind::Approved) => read_linked_issues(repo, pr, &hashes, &root),
        _ => Some(Vec::new()),
    };
    let labels = read_labels(repo, pr, &root);
    let own_sweep = std::env::var("LOOM_SWEEP_ID").ok();
    let decision = gate::decide(&GateInput {
        verdict: kind,
        sha,
        comments: comments.as_deref(),
        labels: labels.as_deref(),
        overrule,
        now: chrono::Utc::now(),
        window_secs,
        review_started: started,
        issues: issues.as_deref(),
        own_sweep: own_sweep.as_deref(),
        acknowledged: acknowledged_comment,
    });
    if matches!(decision, Decision::Proceed(_)) && (comments.is_none() || labels.is_none()) {
        eprintln!(
            "forge verdict-gate: WARNING — PR #{pr}'s comments or labels could not be read; \
             a changes-requested verdict posts anyway (it cannot merge anything)"
        );
    }
    let (line, code) = decision.render();
    // What this caller's gate saw, for `verdict-reconcile` to compare against.
    let seen = comments
        .as_deref()
        .map_or(0, |c| gate::count_markers(c, sha, opposite_of(kind)));
    let seen_same = comments
        .as_deref()
        .map_or(0, |c| gate::max_marker_id(c, sha, kind));
    println!("{line} seen-opposite={seen} seen-same-max-id={seen_same}");
    std::process::exit(code)
}

fn opposite_of(kind: VerdictKind) -> VerdictKind {
    match kind {
        VerdictKind::Approved => VerdictKind::ChangesRequested,
        VerdictKind::ChangesRequested => VerdictKind::Approved,
    }
}

/// `forge verdict-reconcile` (read-only): after the post and label transition,
/// re-read the PR and say whether a rival verdict landed at the same head
/// after this caller's gate read (a cross-host race the host lock cannot see).
pub(crate) fn verdict_reconcile(
    pr: u64,
    repo: &str,
    verdict: &str,
    sha: &str,
    seen_opposite: usize,
    seen_same_max_id: u64,
    nonce: &str,
) -> Result<()> {
    let kind = verdict_or_exit("verdict-reconcile", verdict);
    let root = super::forge_identity_cmd::workspace();
    let comments =
        crate::comment_trust::records::fetch_trusted_comments(repo, &pr.to_string(), &root, false);
    let outcome =
        gate::reconcile(kind, sha, seen_opposite, seen_same_max_id, nonce, comments.as_deref());
    // The loser of an identical-verdict race withdraws its own comment, so
    // exactly one stands. A failed delete is an unconfirmed state, not a pass.
    if let gate::Reconciled::Duplicate(id) = &outcome {
        let path = format!("repos/{repo}/issues/comments/{id}");
        let deleted = crate::script_helpers::gh_write(
            "verdict.withdraw_comment",
            &["api", "-X", "DELETE", &path],
            &root,
            false,
        )
        .ok_output()
        .is_some();
        if !deleted {
            println!(
                "{} UNREAD duplicate comment {id} could not be withdrawn",
                gate::RECONCILE_SENTINEL
            );
            std::process::exit(1)
        }
    }
    let (line, code) = outcome.render();
    println!("{line}");
    std::process::exit(code)
}

/// `forge verdict-labels`.
pub(crate) fn verdict_labels(pr: u64, repo: &str, verdict: &str, sha: Option<&str>) -> Result<()> {
    let kind = verdict_or_exit("verdict-labels", verdict);
    let root = super::forge_identity_cmd::workspace();
    let (add, remove) = gate::transition(kind);
    let base = format!("repos/{repo}/issues/{pr}/labels");
    // Add first: a failure after this point leaves the verdict label on,
    // never a verdict with no label (#10605).
    let mut args: Vec<String> = vec!["api".into(), "-X".into(), "POST".into(), base.clone()];
    for l in add {
        args.push("-f".into());
        args.push(format!("labels[]={l}"));
    }
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    // Never delete the source/claim labels unless the add landed: a failed
    // POST followed by the DELETEs would drop the PR out of its queue with
    // no verdict label to replace it.
    if crate::script_helpers::gh_write("verdict.add_labels", &argv, &root, false)
        .ok_output()
        .is_none()
    {
        eprintln!(
            "forge verdict-labels: PR #{pr}'s {} label could not be added; the existing queue/claim \
             labels were left in place.\n  Repair: {}",
            kind.marker_token(),
            gate::repair_command(pr, repo, kind)
        );
        std::process::exit(1)
    }
    // A DELETE of a label the PR does not carry is a 404; the re-read below
    // is what decides success, so the individual results are not consulted.
    for l in remove {
        let path = format!("{base}/{}", l.replace(':', "%3A"));
        let _ = crate::script_helpers::gh_write(
            "verdict.remove_label",
            &["api", "-X", "DELETE", &path],
            &root,
            false,
        );
    }
    let problems = match read_labels(repo, pr, &root) {
        Some(labels) => gate::label_problems(kind, &labels),
        None => vec!["the labels could not be re-read to verify the transition".to_string()],
    };
    if problems.is_empty() {
        // Telemetry only (#12362): the verdict the run observed, from this
        // write; no forge call.
        crate::observability::pick_journal::record_verdict(pr, journal_action(kind), sha);
        println!("{} OK {}", gate::LABELS_SENTINEL, add.join(","));
        return Ok(());
    }
    eprintln!(
        "forge verdict-labels: PR #{pr}'s {} label transition did not hold: {}.\n  Repair: {}",
        kind.marker_token(),
        problems.join("; "),
        gate::repair_command(pr, repo, kind)
    );
    std::process::exit(1)
}

/// Whether a `gh` failure is an authentication refusal: the session
/// credential expired mid-step (#14165). Matched on the forge's own status
/// spelling, not a bare `401`, so a PR number in an unrelated message cannot
/// class itself as a credential failure.
fn is_github_auth(out: &crate::cmd_out::CmdOutcome) -> bool {
    let stderr = out.stderr_trimmed();
    stderr.contains("HTTP 401") || stderr.contains("status: 401") || stderr.contains("status 401")
}

/// [`crate::doctor_handback::Forge`] over REST (`gh api`, uncached).
struct GhHandback<'a> {
    repo: String,
    pr: u64,
    labels: String,
    pull: String,
    root: &'a Path,
    held_gh: std::path::PathBuf,
    /// The claim's `--watch-pid`, so the held route resolves the claim's lease
    /// identity even when the session PID is not exported.
    watch_pid: Option<u32>,
    /// #14165: which forge call failed and why (a 401 classed as
    /// `[github_auth]`), so a `FAILED` outcome names the call instead of a
    /// generic "could not be read".
    failure: Option<String>,
}

pub(crate) mod held_review;

impl GhHandback<'_> {
    /// Record a named forge-call failure (#14165).
    fn note(&mut self, call: &str, out: &crate::cmd_out::CmdOutcome) {
        let mut why = out.failure_reason(call);
        if is_github_auth(out) {
            why.push_str(" [github_auth]");
        }
        self.failure = Some(why);
    }
}

impl crate::doctor_handback::Forge for GhHandback<'_> {
    fn held_approval(&mut self, expected_head: &str) -> Option<crate::doctor_handback::Outcome> {
        held_review::doctor(self, expected_head)
    }

    fn read(&mut self) -> Option<crate::doctor_handback::Snapshot> {
        // One `pulls/{n}` read carries both labels and head, so they agree.
        let out = crate::script_helpers::gh_read(
            "verdict.handback_read",
            &["api", &self.pull],
            self.root,
            false,
        );
        if !out.succeeded() {
            self.note("pull read (labels+head)", &out);
            return None;
        }
        let doc = match serde_json::from_slice::<Value>(&out.ok_output()?.stdout) {
            Ok(doc) => doc,
            Err(_) => {
                self.failure =
                    Some("pull read (labels+head): the body could not be parsed as JSON".into());
                return None;
            }
        };
        match crate::doctor_handback::snapshot_from_pull(&doc) {
            Some(s) => Some(s),
            None => {
                self.failure = Some(
                    "pull read (labels+head): the head SHA or labels were absent from the body"
                        .into(),
                );
                None
            }
        }
    }
    fn add(&mut self, label: &str) -> bool {
        let field = format!("labels[]={label}");
        let argv = ["api", "-X", "POST", &self.labels, "-f", &field];
        let out = crate::script_helpers::gh_write("verdict.handback_add", &argv, self.root, false);
        if !out.succeeded() {
            self.note(&format!("label add {label}"), &out);
        }
        out.succeeded()
    }
    fn remove(&mut self, label: &str) -> bool {
        let path = format!("{}/{}", self.labels, label.replace(':', "%3A"));
        let out = crate::script_helpers::gh_write(
            "verdict.handback_remove",
            &["api", "-X", "DELETE", &path],
            self.root,
            false,
        );
        if !out.succeeded() {
            self.note(&format!("label remove {label}"), &out);
        }
        out.succeeded()
    }
    fn rejected_at(&mut self, head: &str) -> Option<bool> {
        let comments = match crate::comment_trust::records::fetch_trusted_comments(
            &self.repo,
            &self.pr.to_string(),
            self.root,
            false,
        ) {
            Some(c) => c,
            None => {
                self.failure = Some(
                    "the trusted-comment read for the rejection check failed (retried, \
                         per-call deadline 30s)"
                        .into(),
                );
                return None;
            }
        };
        Some(gate::count_markers(&comments, head, VerdictKind::ChangesRequested) > 0)
    }
    fn rejected_precedes_claim(&mut self, head: &str) -> Option<bool> {
        let comments = match crate::comment_trust::records::fetch_trusted_comments(
            &self.repo,
            &self.pr.to_string(),
            self.root,
            false,
        ) {
            Some(c) => c,
            None => {
                self.failure = Some(
                    "the trusted-comment read for the rejection claim-time check failed \
                         (retried, per-call deadline 30s)"
                        .into(),
                );
                return None;
            }
        };
        let gh_bin = crate::gh_invocation::gh_bin();
        let claimed_at = crate::claim_reconciliation::forge::fetch_claim_labeled_at(
            Path::new(&gh_bin),
            self.root,
            u32::try_from(self.pr).ok()?,
            crate::doctor_handback::CLAIM,
        )?;
        crate::doctor_handback::rejection_precedes_claim(&comments, head, claimed_at)
    }
}

/// #14165: the per-PR lock must not outlive a terminated hand-back. The
/// holder's path is published here — leaked, because the signal handler reads
/// it without any lock — the moment the lock is taken; a SIGTERM/SIGINT
/// removes the directory, restores the default disposition and re-raises, so
/// the caller still sees the signal's exit status rather than a clean one.
static HELD_LOCK: std::sync::atomic::AtomicPtr<std::ffi::c_char> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

/// Publish the held lock's path for the signal path (#14165).
fn publish_held_lock(path: &Path) {
    if let Ok(c) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) {
        HELD_LOCK.store(
            Box::into_raw(c.into_boxed_c_str()).cast(),
            std::sync::atomic::Ordering::Release,
        );
    }
}

/// Retract the published path. Called *before* the normal-path release, so a
/// signal landing in between can at worst skip one release (the stale reap
/// covers it), never delete a successor's freshly acquired lock.
fn unpublish_held_lock() {
    HELD_LOCK.store(std::ptr::null_mut(), std::sync::atomic::Ordering::Release);
}

/// Remove the published lock directory. The body the signal handler runs —
/// async-signal-safe (one atomic load and `rmdir`), so no locks are taken.
fn release_held_lock() {
    let p = HELD_LOCK.load(std::sync::atomic::Ordering::Acquire);
    if !p.is_null() {
        // SAFETY: the pointer is a leaked CString published by
        // `publish_held_lock`; `rmdir` only reads it.
        unsafe { libc::rmdir(p) };
    }
}

#[cfg(unix)]
extern "C" fn on_termination(sig: libc::c_int) {
    release_held_lock();
    // SAFETY: restore the default disposition, then re-raise, so the exit
    // status reflects the signal the caller sent.
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

/// Catch SIGTERM/SIGINT for the hand-back's lifetime (#14165).
fn install_termination_release() {
    #[cfg(unix)]
    for sig in [libc::SIGTERM, libc::SIGINT] {
        // SAFETY: the handler only touches the atomic-published path and
        // async-signal-safe syscalls.
        if unsafe { libc::signal(sig, on_termination as *const () as libc::sighandler_t) }
            == libc::SIG_ERR
        {
            return;
        }
    }
}

/// #14165: `true` once the answer line is about to print, so the deadline
/// watchdog never races a completing hand-back into a second line.
static ANSWERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Fail the verb inside its own budget (#14165): a watchdog thread prints the
/// named deadline line and exits when the hand-back has not answered within
/// the budget, instead of leaving a caller's shorter wrapper to kill it
/// silently mid-write. Default 300 s.
fn arm_deadline_watchdog(deadline_secs: Option<u64>) {
    use crate::doctor_handback as hb;
    let secs = deadline_secs.unwrap_or(300);
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(secs));
        if ANSWERED.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        release_held_lock();
        println!("{}", hb::deadline_exceeded_line(secs));
        std::process::exit(hb::EXIT_FAILED);
    });
}

/// `forge doctor-handback` (#9388). Held under the per-PR verdict lock so a
/// same-host `post-verdict.sh` cannot interleave; the re-read covers other hosts.
/// Self-bounded (#14165): every forge call has a per-call deadline, the lock
/// wait is bounded, and the whole run answers within `--deadline-secs`.
pub(crate) fn doctor_handback(
    pr: u64,
    repo: Option<&str>,
    expected_head: &str,
    deadline_secs: Option<u64>,
    recover_held: bool,
    watch_pid: Option<u32>,
) -> Result<()> {
    use crate::doctor_handback as hb;
    if expected_head.trim().is_empty() {
        eprintln!("forge doctor-handback: --expected-head-sha must be the SHA you pushed");
        std::process::exit(2)
    }
    let cwd = std::env::current_dir()?;
    let repo = match crate::write_scope::may_write_from(&cwd, repo) {
        crate::write_scope::Verdict::Allow(nwo) => nwo,
        crate::write_scope::Verdict::Deny(why) => {
            println!("{} FAILED may not write: {why}", hb::SENTINEL);
            std::process::exit(hb::EXIT_FAILED)
        }
    };
    let root = super::forge_identity_cmd::workspace();
    install_termination_release();
    arm_deadline_watchdog(deadline_secs);
    let lock = lock_path(pr, &repo);
    let env_wait = std::env::var("LOOM_VERDICT_LOCK_WAIT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(90);
    // Bound the lock wait by the overall budget (#14165): waiting past it can
    // only end in the watchdog's exit, which names less than this refusal does.
    let wait = std::time::Duration::from_secs(deadline_secs.unwrap_or(300))
        .min(std::time::Duration::from_secs(env_wait));
    let outcome = match acquire_lock(&lock, wait) {
        Ok(()) => {
            publish_held_lock(&lock);
            let mut forge = GhHandback::new(repo, pr, &root);
            forge.watch_pid = watch_pid;
            let outcome = if recover_held {
                held_review::recover_cli(&mut forge, expected_head)
            } else {
                hb::run(&mut forge, expected_head)
            };
            unpublish_held_lock();
            let _ = std::fs::remove_dir(&lock);
            match (outcome, forge.failure) {
                (hb::Outcome::Failed(why), Some(named)) => {
                    hb::Outcome::Failed(format!("{why}; {named}"))
                }
                (outcome, _) => outcome,
            }
        }
        Err(holder_age) => hb::lock_held_outcome(pr, holder_age, wait),
    };
    ANSWERED.store(true, std::sync::atomic::Ordering::SeqCst);
    let (line, code) = outcome.render();
    println!("{line}");
    std::process::exit(code)
}

/// The lock directory for one PR (`owner/name` flattened, so it is one path
/// component).
fn lock_path(pr: u64, repo: &str) -> std::path::PathBuf {
    let base = std::env::var_os("LOOM_VERDICT_LOCK_DIR").map_or_else(
        || {
            Path::new(&std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into()))
                .join(".loom/locks/post-verdict")
        },
        Into::into,
    );
    base.join(format!("{}-{pr}", repo.replace('/', "_")))
}

/// Take the lock: `mkdir` is the atomic step. A holder that never released
/// (crashed) is reaped once its directory is older than [`LOCK_STALE`].
/// `Err(holder_age)` — the held directory's age when it could be read —
/// names the refusal for the caller's message (#14165).
fn acquire_lock(dir: &Path, wait: std::time::Duration) -> Result<(), Option<std::time::Duration>> {
    const LOCK_STALE: std::time::Duration = std::time::Duration::from_secs(300);
    if let Some(parent) = dir.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let deadline = std::time::Instant::now() + wait;
    loop {
        if std::fs::create_dir(dir).is_ok() {
            return Ok(());
        }
        let age = std::fs::metadata(dir)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok());
        if age.is_some_and(|age| age > LOCK_STALE) {
            let _ = std::fs::remove_dir(dir);
            continue;
        }
        if std::time::Instant::now() >= deadline {
            return Err(age);
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

/// `forge verdict-lock`.
pub(crate) fn verdict_lock(action: &str, pr: u64, repo: &str) -> Result<()> {
    let dir = lock_path(pr, repo);
    match action {
        "acquire" => {
            let wait = std::env::var("LOOM_VERDICT_LOCK_WAIT_SECS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(90);
            if let Err(holder_age) = acquire_lock(&dir, std::time::Duration::from_secs(wait)) {
                let age = holder_age.map_or_else(
                    || "an unknown age".to_string(),
                    |a| format!("{:.0}s", a.as_secs_f32()),
                );
                eprintln!(
                    "forge verdict-lock: lock held (holder age {age}); could not take {} within \
                     {wait}s; another verdict on PR #{pr} is in flight",
                    dir.display()
                );
                std::process::exit(9)
            }
            Ok(())
        }
        "release" => {
            let _ = std::fs::remove_dir(&dir);
            Ok(())
        }
        other => {
            eprintln!("forge verdict-lock: action must be acquire or release, got {other:?}");
            std::process::exit(2)
        }
    }
}

#[cfg(test)]
mod lock_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_held_lock_is_refused_until_released() {
        let dir = std::env::temp_dir().join(format!("loom-vlock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let lock = dir.join("owner_repo-1");
        assert!(acquire_lock(&lock, Duration::from_millis(0)).is_ok());
        assert!(
            acquire_lock(&lock, Duration::from_millis(300)).is_err(),
            "held: second caller times out"
        );
        std::fs::remove_dir(&lock).unwrap();
        assert!(acquire_lock(&lock, Duration::from_millis(0)).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_refused_lock_names_the_holders_age() {
        let lock = std::env::temp_dir().join(format!("loom-vlock-age-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&lock);
        std::fs::create_dir(&lock).unwrap();
        match acquire_lock(&lock, Duration::from_millis(0)) {
            Err(Some(age)) => assert!(age.as_nanos() > 0, "the holder's age must be reported"),
            other => panic!("a held lock must refuse with its age, got {other:?}"),
        }
        std::fs::remove_dir(&lock).unwrap();
    }

    #[test]
    fn a_termination_signal_releases_the_held_lock() {
        let lock = std::env::temp_dir().join(format!("loom-vlock-sig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&lock);
        std::fs::create_dir(&lock).unwrap();
        publish_held_lock(&lock);
        // The handler's body, exactly as a SIGTERM would run it (#14165).
        release_held_lock();
        assert!(!lock.exists(), "the held lock must be gone after the signal path");
        // Unpublished, a later signal must remove nothing — not even a
        // successor's lock at the same path.
        std::fs::create_dir(&lock).unwrap();
        unpublish_held_lock();
        release_held_lock();
        assert!(lock.exists(), "an unpublished path must not be removed");
        std::fs::remove_dir(&lock).unwrap();
    }

    /// A `gh` that died with a 401, the shape an expired session credential
    /// produces mid-step (#14165).
    fn gh_401() -> crate::cmd_out::CmdOutcome {
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.args(["-c", "echo 'gh: HTTP 401: Bad credentials' >&2; exit 1"])
            .stdin(std::process::Stdio::null());
        crate::cmd_out::run_command(cmd, Duration::from_secs(10))
    }

    #[test]
    fn a_401_is_classified_as_github_auth_and_names_the_call() {
        let tmp = std::env::temp_dir();
        let out = gh_401();
        assert!(is_github_auth(&out), "an HTTP 401 must class as github_auth");
        let mut forge = GhHandback::new("owner/name".into(), 1, &tmp);
        forge.note("pull read (labels+head)", &out);
        let named = forge.failure.expect("the failure must be recorded");
        assert!(named.contains("pull read"), "the failing call must be named: {named}");
        assert!(named.contains("[github_auth]"), "a 401 must be classed: {named}");
        // A bare 401 inside an unrelated message must NOT class (#4013 say).
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.args(["-c", "echo 'PR #4013 not found' >&2; exit 1"])
            .stdin(std::process::Stdio::null());
        let other = crate::cmd_out::run_command(cmd, Duration::from_secs(10));
        assert!(!is_github_auth(&other), "a PR number is not a credential refusal");
    }

    #[test]
    fn the_lock_wait_and_deadline_reasons_render() {
        let (line, code) = crate::doctor_handback::lock_held_outcome(
            7,
            Some(Duration::from_secs(41)),
            Duration::from_secs(90),
        )
        .render();
        assert_eq!(code, crate::doctor_handback::EXIT_FAILED);
        assert!(line.contains("lock held"), "{line}");
        assert!(line.contains("41s"), "the holder's age must be named: {line}");
        assert!(line.contains("90s"), "{line}");
        let line = crate::doctor_handback::deadline_exceeded_line(300);
        assert!(line.starts_with(crate::doctor_handback::SENTINEL), "{line}");
        assert!(line.contains("deadline of 300s"), "{line}");
    }
}
