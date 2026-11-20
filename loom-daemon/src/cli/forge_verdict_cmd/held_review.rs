use super::{acquire_lock, lock_path, GhHandback};
use crate::claim_reconciliation::gh_call;
use crate::cli::pr_lease::{self, Role};
use crate::doctor_handback as hb;
use crate::doctor_handback::held_review::{
    self as held, Forge, Intent, Mode, Observation, Outcome,
};
use crate::forge_merge_queue::github::GhQueueApi;
use chrono::Utc;
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

impl<'a> GhHandback<'a> {
    pub(super) fn new(repo: String, pr: u64, root: &'a Path) -> Self {
        Self {
            pr,
            labels: format!("repos/{repo}/issues/{pr}/labels"),
            pull: format!("repos/{repo}/pulls/{pr}"),
            repo,
            root,
            held_gh: crate::gh_invocation::gh_bin().into(),
            failure: None,
        }
    }

    fn pull_document(&self) -> Option<Value> {
        let out = self.held_read("verdict.handback_read", &["api", &self.pull])?;
        let doc: Value = serde_json::from_slice(&out).ok()?;
        (doc.get("state")?.as_str()? == "open").then_some(doc)
    }

    fn held_read(&self, op: &'static str, args: &[&str]) -> Option<Vec<u8>> {
        gh_call::ok_stdout(
            gh_call::read(op, &self.held_gh, self.root)
                .reader_repo(&self.repo)
                .args(args),
        )
    }

    fn held_write(&self, op: &'static str, args: &[&str]) -> bool {
        gh_call::write(op, &self.held_gh, self.root)
            .args(args)
            .run()
            .succeeded()
    }
}

fn can_act(comments: &[Value], mode: Mode) -> Option<bool> {
    let mut listing = String::new();
    let mut count = 0;
    for row in comments {
        if row.get("body")?.as_str()?.starts_with("<!-- loom:pr-lease") {
            listing.push_str(&row.to_string());
            listing.push('\n');
            count += 1;
        }
    }
    let rows = pr_lease::parse_rows(&listing);
    if rows.len() != count
        || rows
            .iter()
            .any(|row| pr_lease::parse_record(&row.body).is_none())
    {
        return None;
    }
    let now = Utc::now();
    let ttl_ms = (crate::claim_reconciliation::resolve_lease_ttl_minutes() * 60_000.0) as i64;
    let ttl = chrono::Duration::milliseconds(ttl_ms);
    let doctor = pr_lease::live_leases(&rows, Role::Doctor, now, ttl);
    let peer = [Role::Judge, Role::Champion]
        .into_iter()
        .any(|role| !pr_lease::live_leases(&rows, role, now, ttl).is_empty());
    if peer || mode == Mode::Recover {
        return Some(!peer && doctor.is_empty());
    }
    let watch_pid = ["LOOM_AGENT_SESSION_PID", "CLAUDE_PID"]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok())
        .find_map(|pid| pid.parse().ok());
    let sweep = pr_lease::resolve_sweep(None, watch_pid, |key| std::env::var(key).ok());
    let host = pr_lease::published_host();
    Some(
        sweep != "pr-lease-unknown"
            && !doctor.is_empty()
            && doctor
                .iter()
                .all(|owner| owner.holder.sweep == sweep && owner.holder.host == host),
    )
}

impl Forge for GhHandback<'_> {
    fn observe(&mut self, mode: Mode) -> Option<Observation> {
        let doc = self.pull_document()?;
        let snapshot = hb::snapshot_from_pull(&doc)?;
        if !held::candidate(&snapshot.labels) {
            return None;
        }
        let path = format!("repos/{}/issues/{}/comments?per_page=100", self.repo, self.pr);
        let listing = self.held_read("verdict.pr_comments", &["api", &path, "--paginate"])?;
        let trusted =
            crate::comment_trust::TrustPolicy::for_root(self.root).trusted_listing(&listing)?;
        let can_act = can_act(&trusted, mode)?;
        let park_body = match doc.get("body")? {
            Value::Null => String::new(),
            Value::String(body) => body.clone(),
            _ => return None,
        };
        Some(Observation {
            snapshot,
            park_body,
            trusted,
            can_act,
        })
    }

    fn content_changed(&mut self, intent: &Intent, expected: &str) -> Option<bool> {
        use crate::verdict_equivalence::Equivalence;
        let pr = self.pr.try_into().ok()?;
        let result = crate::verdict_equivalence::assess(
            &self.held_gh,
            Some(self.root),
            pr,
            intent.from(),
            expected,
        );
        match result.equivalence {
            Equivalence::Changed => Some(true),
            Equivalence::Equivalent(_) => Some(false),
            Equivalence::Indeterminate => None,
        }
    }

    fn record(&mut self, intent: &Intent) -> bool {
        let path = format!("repos/{}/issues/{}/comments", self.repo, self.pr);
        let body = format!("body={}", intent.body());
        self.held_write("verdict.stale_comment", &["api", "-X", "POST", &path, "-f", &body])
    }

    fn ensure_idle(&mut self) -> bool {
        // The installed guard retains preventive disarm and revocation. The
        // review path proceeds only on a positive idle read of this head.
        let Some(doc) = self.pull_document() else {
            return false;
        };
        if !doc.get("auto_merge").is_some_and(Value::is_null) {
            return false;
        }
        let (Ok(pr), Some(head)) =
            (self.pr.try_into(), doc.pointer("/head/sha").and_then(Value::as_str))
        else {
            return false;
        };
        let Ok(queue) = GhQueueApi::new(&self.held_gh.to_string_lossy(), &self.repo) else {
            return false;
        };
        queue.known_unqueued(pr, head)
    }

    fn add(&mut self, label: &str) -> bool {
        let field = format!("labels[]={label}");
        self.held_write("verdict.handback_add", &["api", "-X", "POST", &self.labels, "-f", &field])
    }
    fn remove(&mut self, label: &str) -> bool {
        let path = format!("{}/{}", self.labels, label.replace(':', "%3A"));
        self.held_write("verdict.handback_remove", &["api", "-X", "DELETE", &path])
    }
    fn snapshot(&mut self) -> Option<hb::Snapshot> {
        hb::snapshot_from_pull(&self.pull_document()?)
    }
}

fn report(outcome: Outcome) -> Option<hb::Outcome> {
    match outcome {
        Outcome::Requeued => Some(hb::Outcome::HeldReview),
        Outcome::Interrupted => Some(hb::Outcome::Failed(
            "held repair remains pending; verify the head, hold, and merge admission state before retrying".into(),
        )),
        Outcome::Ineligible => None,
    }
}

pub(super) fn doctor(forge: &mut GhHandback<'_>, expected: &str) -> Option<hb::Outcome> {
    report(held::route(forge, Mode::Doctor, expected))
}

pub(super) fn recover_cli(forge: &mut GhHandback<'_>, expected: &str) -> hb::Outcome {
    report(held::route(forge, Mode::Recover, expected)).unwrap_or(hb::Outcome::NoHeldReview)
}

/// Periodic recovery shares the verdict lock and does not wait on a peer.
pub(crate) fn recover(gh_bin: &Path, root: &Path, pr: u32, expected: &str) -> Outcome {
    let crate::write_scope::Verdict::Allow(repo) =
        crate::write_scope::root_writable_with_at(root, gh_bin, "held_review")
    else {
        return Outcome::Ineligible;
    };
    let lock = lock_path(u64::from(pr), &repo);
    if acquire_lock(&lock, Duration::ZERO).is_err() {
        return Outcome::Ineligible;
    }
    let mut forge = GhHandback::new(repo, u64::from(pr), root);
    forge.held_gh = gh_bin.into();
    let outcome = held::route(&mut forge, Mode::Recover, expected);
    let _ = std::fs::remove_dir(lock);
    outcome
}
