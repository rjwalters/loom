//! Run-scoped cleanup and injected transport faults (#9789 slice 2).
//!
//! **Cleanup is scoped to one run namespace.** It closes open issues and
//! deletes labels whose name/title begins with exactly `<run-ns>: ` — the
//! prefix every disposable resource of the run carries, delimiter included,
//! so namespace `a` can never touch `ab`'s resources. Issues are closed, not
//! deleted (deleting needs admin and destroys evidence). Every close is read
//! back. Nothing outside the namespace is written.
//!
//! **Injected faults are labelled as such.** `FaultInjector` wraps the live
//! transport and fails one page of every list walk, so the page-two-failure
//! criterion can be exercised against the real service's real page one. The
//! rows it affects carry `injected_fault: true` — never confusable with an
//! observation of the service.

use serde_json::{json, Value};

use super::coordination::Case;
use super::{scrub, ProbeEntryView, ProbeHttp, RunnerConfig};

/// The prefix of an injected transport error. The runner keys
/// `CaseResult::injected_fault` on it; no server text is ever echoed into
/// an error, so a service cannot forge it.
pub const INJECTED_PREFIX: &str = "INJECTED FAULT (--inject-page-fault):";

/// Wraps a transport and fails every list read of page `page`.
pub struct FaultInjector<'a> {
    pub inner: &'a dyn ProbeHttp,
    pub page: u32,
}

impl ProbeHttp for FaultInjector<'_> {
    fn request(
        &self,
        method: &str,
        path: &str,
        token: &str,
        body: Option<&str>,
    ) -> Result<(u16, String), String> {
        let hit = method == "GET"
            && path
                .split(['?', '&'])
                .any(|kv| kv == format!("page={}", self.page));
        if hit {
            return Err(format!("{INJECTED_PREFIX} page {} of a list read", self.page));
        }
        self.inner.request(method, path, token, body)
    }
}

/// What a scoped cleanup did. Numbers and ids only — no titles or bodies.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CleanupReport {
    pub run_ns: String,
    pub issues_closed: Vec<u64>,
    pub labels_deleted: Vec<u64>,
    /// Failures (scrubbed); any entry makes the cleanup incomplete.
    pub failures: Vec<String>,
}

impl CleanupReport {
    pub fn is_clean(&self) -> bool {
        self.failures.is_empty()
    }
}

/// The exact prefix this run's disposable resources carry. `run_ns` is the
/// full namespace (#9945): no extra `loomp-` is added, so this matches
/// `issue_title` exactly.
pub fn namespace_prefix(cfg: &RunnerConfig) -> String {
    format!("{}: ", cfg.run_ns)
}

/// Close this run's open fixture issues and delete its labels. Refuses
/// without the live-write opt-in. A list walk that cannot reach its end is
/// a failure, not "nothing to clean".
pub fn cleanup(cfg: &RunnerConfig, http: &dyn ProbeHttp) -> anyhow::Result<CleanupReport> {
    if !cfg.live_write {
        anyhow::bail!("cleanup writes: refused without --live-write");
    }
    super::validate_config(cfg)?;
    let view = ProbeEntryView {
        id: "probe.cleanup".into(),
        test_id: None,
        risk: "normal".into(),
        disposition: "optional".into(),
        class: "write".into(),
    };
    let c = Case {
        test_id: "forge-probe::cleanup",
        view: &view,
        cfg,
        http,
        server_version: "",
    };
    let prefix = namespace_prefix(cfg);
    let ours = |v: &Value, key: &str| {
        v.get(key)
            .and_then(Value::as_str)
            .is_some_and(|t| t.starts_with(&prefix))
    };
    let mut report = CleanupReport {
        run_ns: cfg.run_ns.clone(),
        ..Default::default()
    };
    let tok = &cfg.writer_token;

    match c.walk(&format!("repos/{}/issues?state=open&type=issues", cfg.repo), 50, 200) {
        Ok(w) => {
            for n in w
                .rows
                .iter()
                .filter(|i| ours(i, "title"))
                .filter_map(|i| i.get("number").and_then(Value::as_u64))
            {
                let path = format!("repos/{}/issues/{n}", cfg.repo);
                let closed = http
                    .request("PATCH", &path, tok, Some(&json!({"state": "closed"}).to_string()))
                    .ok()
                    .filter(|(code, _)| (200..=299).contains(code))
                    .and_then(|_| http.request("GET", &path, tok, None).ok())
                    .and_then(|(_, b)| serde_json::from_str::<Value>(b.trim()).ok())
                    .is_some_and(|v| v.get("state").and_then(Value::as_str) == Some("closed"));
                if closed {
                    report.issues_closed.push(n);
                } else {
                    report
                        .failures
                        .push(format!("issue #{n}: close not confirmed on read-back"));
                }
            }
        }
        Err(e) => report
            .failures
            .push(format!("issue walk: {}", scrub(cfg, &e.to_string()))),
    }

    match c.walk(&format!("repos/{}/labels", cfg.repo), 50, 40) {
        Ok(w) => {
            for id in w
                .rows
                .iter()
                .filter(|l| ours(l, "name"))
                .filter_map(|l| l.get("id").and_then(Value::as_u64))
            {
                match http.request("DELETE", &format!("repos/{}/labels/{id}", cfg.repo), tok, None)
                {
                    Ok((200..=299, _)) => report.labels_deleted.push(id),
                    Ok((code, _)) => report
                        .failures
                        .push(format!("label {id}: DELETE answered {code}")),
                    Err(e) => report
                        .failures
                        .push(format!("label {id}: {}", scrub(cfg, &e))),
                }
            }
        }
        Err(e) => report
            .failures
            .push(format!("label walk: {}", scrub(cfg, &e.to_string()))),
    }
    Ok(report)
}
