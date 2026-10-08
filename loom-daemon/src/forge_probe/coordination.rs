//! `forge-probe` slice 2 (#9789): every `required-coordination` manifest row
//! executes, each case independently selectable and self-contained (it makes
//! its own namespaced fixture, so `--case X` never depends on another row).
//!
//! Rules every handler follows — the slice-1 review lessons, applied
//! uniformly rather than per case:
//! - **Semantic readback, never the acknowledgement.** A 2xx proves nothing;
//!   a case passes only on a read that shows the effect.
//! - **No echo.** Response bodies are described by `body_shape`, never
//!   copied into a receipt; transport errors pass through `scrub`.
//! - **Unknown is not pass, and not fail.** Transport faults, partial
//!   pagination and ambiguous answers are `Err(ForgeOutcome)`, which the
//!   runner records as `unknown`. A list read that did not provably reach
//!   its end never feeds a decision.
//! - **Native absence is `unsupported`**, distinct from a present-but-wrong
//!   behaviour (`fail`).
//! - **Fixture ownership is proven before any further write**: the fixture's
//!   namespaced title is read back first.

use std::collections::HashSet;

use serde_json::{json, Value};

use super::{
    body_shape, issue_title, now_secs, scrub, secrets_of, with_actor, CaseResult, ProbeEntryView,
    ProbeHttp, RunnerConfig, OUTCOME_FAIL, OUTCOME_PASS, OUTCOME_UNKNOWN, OUTCOME_UNSUPPORTED,
};
use crate::forge_contract::{CredentialRef, ForgeOutcome};

pub(super) type CaseOutcome = std::result::Result<CaseResult, ForgeOutcome>;

/// Spacing between `issue-search` polls (the manifest declares the row
/// `consistency = "eventual"`; gitea-1's body indexer lagged ~2s live).
#[cfg(not(test))]
const SEARCH_POLL: std::time::Duration = std::time::Duration::from_secs(2);
#[cfg(test)]
const SEARCH_POLL: std::time::Duration = std::time::Duration::ZERO;
/// Bounded eventual window: at most this many search reads.
const SEARCH_ATTEMPTS: usize = 5;

/// Route a coordination `test_id` to its handler; `None` = not this slice.
pub(super) fn dispatch(
    test_id: &str,
    view: &ProbeEntryView,
    cfg: &RunnerConfig,
    http: &dyn ProbeHttp,
    server_version: &str,
) -> Option<CaseOutcome> {
    let c = Case {
        test_id,
        view,
        cfg,
        http,
        server_version,
    };
    let name = test_id.strip_prefix("forge-probe::coordination::")?;
    let handler: fn(&Case) -> CaseOutcome = match name {
        "issue-view-state" => issue_view_state,
        "issue-list" => issue_list,
        "issue-search" => issue_search,
        "issue-edit-labels" => issue_edit_labels,
        "issue-edit-body" => issue_edit_body,
        "issue-close-with-reason" => issue_close_with_reason,
        "label-sync-catalogue" => label_sync_catalogue,
        "comment-list" => comment_list,
        "comment-edit-delete" => comment_edit_delete,
        "timeline-read" => timeline_read,
        "epic-sub-issues" => epic_sub_issues,
        _ => return None,
    };
    // Every slice-2 case creates a fixture, so even a manifest `read` row
    // writes: gate all of them on the explicit live-write opt-in.
    if !cfg.live_write {
        return Some(Ok(c.refused()));
    }
    Some(handler(&c))
}

/// One case's execution context.
pub(super) struct Case<'a> {
    pub test_id: &'a str,
    pub view: &'a ProbeEntryView,
    pub cfg: &'a RunnerConfig,
    pub http: &'a dyn ProbeHttp,
    pub server_version: &'a str,
}

/// A completed list walk.
#[derive(Debug)]
pub(super) struct Walk {
    pub rows: Vec<Value>,
    /// Pages that carried at least one row.
    pub pages: u32,
    /// The endpoint ignored `page` and page 1 was provably the whole list.
    pub page_ignored: bool,
}

impl Case<'_> {
    fn unknown(&self, why: impl Into<String>) -> ForgeOutcome {
        ForgeOutcome::Unknown {
            operation: self.view.id.clone(),
            why: why.into(),
        }
    }

    /// One bounded request; a transport fault is `Unknown` with the error
    /// scrubbed of run credentials.
    fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> Result<(u16, String), ForgeOutcome> {
        let body = body.map(|b| b.to_string());
        self.http
            .request(method, path, &self.cfg.writer_token, body.as_deref())
            .map_err(|e| self.unknown(format!("{method} transport fault: {}", scrub(self.cfg, &e))))
    }

    /// A request whose effect the case depends on: anything but 2xx is an
    /// error outcome, classified (denied / transient / ambiguous).
    fn call_ok(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        what: &str,
    ) -> Result<String, ForgeOutcome> {
        let (code, resp) = self.call(method, path, body)?;
        match code {
            200..=299 => Ok(resp),
            401 | 403 => Err(ForgeOutcome::InsufficientPermission {
                operation: self.view.id.clone(),
                principal: CredentialRef("env:GITEA_QUAL_WRITER_TOKEN".into()),
            }),
            429 | 500..=599 => Err(ForgeOutcome::Transient {
                operation: self.view.id.clone(),
                status: Some(code),
                retry_after: None,
            }),
            _ => Err(self.unknown(format!("{what} answered HTTP {code} ({})", body_shape(&resp)))),
        }
    }

    fn get_json(&self, path: &str, what: &str) -> Result<Value, ForgeOutcome> {
        let resp = self.call_ok("GET", path, None, what)?;
        serde_json::from_str(resp.trim())
            .map_err(|_| self.unknown(format!("{what}: unparseable ({})", body_shape(&resp))))
    }

    fn repo(&self) -> &str {
        &self.cfg.repo
    }

    /// The row for a case refused without `--live-write`.
    fn refused(&self) -> CaseResult {
        CaseResult {
            test_id: self.test_id.to_string(),
            operation: self.view.id.clone(),
            risk: self.view.risk.clone(),
            disposition: self.view.disposition.clone(),
            outcome: OUTCOME_UNKNOWN.into(),
            server_version: self.server_version.to_string(),
            actor: None,
            at: now_secs(),
            expected: "a live run with a disposable fixture".into(),
            observed: "refused: live-write not opted in (--live-write)".into(),
            notes: vec![
                "safety: read-only by default (#9789); this case needs a namespaced fixture".into(),
            ],
            injected_fault: false,
        }
    }

    /// An executed row, stamped with actor evidence (a pass without a
    /// resolvable actor fails closed — slice 1's `with_actor`).
    fn finish(
        &self,
        outcome: &str,
        expected: String,
        observed: String,
        notes: Vec<String>,
    ) -> CaseResult {
        let (outcome, observed, actor) =
            with_actor(self.http, &self.cfg.writer_token, &secrets_of(self.cfg), outcome, observed);
        CaseResult {
            test_id: self.test_id.to_string(),
            operation: self.view.id.clone(),
            risk: self.view.risk.clone(),
            disposition: self.view.disposition.clone(),
            outcome,
            server_version: self.server_version.to_string(),
            actor,
            at: now_secs(),
            expected,
            observed,
            notes,
            injected_fault: false,
        }
    }

    /// Create one namespaced fixture issue and prove ownership by reading
    /// its title back before the case writes anything else to it.
    fn fixture(&self, case: &str, body: &str) -> Result<u64, ForgeOutcome> {
        let title = issue_title(self.cfg, case);
        let resp = self.call_ok(
            "POST",
            &format!("repos/{}/issues", self.repo()),
            Some(json!({"title": title, "body": body})),
            "fixture issue create",
        )?;
        let n = serde_json::from_str::<Value>(resp.trim())
            .ok()
            .and_then(|v| v.get("number").and_then(Value::as_u64))
            .ok_or_else(|| {
                self.unknown(format!("fixture create carried no number ({})", body_shape(&resp)))
            })?;
        let read =
            self.get_json(&format!("repos/{}/issues/{n}", self.repo()), "fixture read-back")?;
        if read.get("number").and_then(Value::as_u64) != Some(n)
            || read.get("title").and_then(Value::as_str) != Some(title.as_str())
        {
            return Err(self.unknown(format!(
                "fixture #{n} ownership unproven: read-back number/title mismatch — no further write"
            )));
        }
        Ok(n)
    }

    /// Walk a paginated list to a provable end. Anything short of that —
    /// a page fault, a non-array page, a repeated row, an unbounded walk —
    /// is `Unknown` (partial pagination is unusable for decisions).
    pub(super) fn walk(
        &self,
        base: &str,
        limit: usize,
        max_pages: u32,
    ) -> Result<Walk, ForgeOutcome> {
        let sep = if base.contains('?') { '&' } else { '?' };
        let mut rows: Vec<Value> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut first_id: Option<String> = None;
        for page in 1..=max_pages {
            // Every earlier page was non-empty (an empty one ends the walk).
            let pages = page - 1;
            let path = format!("{base}{sep}page={page}&limit={limit}");
            let partial = |why: String| {
                self.unknown(format!(
                    "partial pagination: page {page} {why} after {} row(s) over {pages} page(s) — the list is unusable for decisions",
                    rows.len()
                ))
            };
            let (code, body) = match self
                .http
                .request("GET", &path, &self.cfg.writer_token, None)
            {
                Ok(r) => r,
                Err(e) => return Err(partial(format!("transport fault: {}", scrub(self.cfg, &e)))),
            };
            if !(200..=299).contains(&code) {
                return Err(partial(format!("answered HTTP {code} ({})", body_shape(&body))));
            }
            let arr = match serde_json::from_str::<Value>(body.trim()) {
                // gitea-1 answers JSON `null` on out-of-range pages (live
                // observation, 2026-10-01): an empty page, not a malformed one.
                Ok(Value::Null) => Vec::new(),
                Ok(Value::Array(a)) => a,
                _ => return Err(partial(format!("is not a JSON array ({})", body_shape(&body)))),
            };
            if arr.is_empty() {
                return Ok(Walk {
                    rows,
                    pages,
                    page_ignored: false,
                });
            }
            let id_of = |r: &Value| r.get("id").map(Value::to_string);
            let this_first = arr.first().and_then(id_of);
            if page > 1 && this_first.is_some() && this_first == first_id {
                // The endpoint ignored `page` (live: issue comments). Page 1
                // was the whole list only if it was not cut at `limit`.
                if page == 2 && rows.len() < limit {
                    return Ok(Walk {
                        rows,
                        pages,
                        page_ignored: true,
                    });
                }
                return Err(partial(format!(
                    "repeated page 1 (limit {limit}) — the endpoint may cut at the limit, completeness unprovable"
                )));
            }
            for r in &arr {
                if let Some(id) = id_of(r) {
                    if !seen.insert(id.clone()) {
                        return Err(partial(format!("repeated row id {id} — unstable pagination")));
                    }
                }
            }
            if page == 1 {
                first_id = this_first;
            }
            rows.extend(arr);
        }
        Err(self.unknown(format!(
            "partial pagination: no empty page within {max_pages} pages ({} row(s) read)",
            rows.len()
        )))
    }

    fn post_comment(&self, n: u64, body: &str) -> Result<u64, ForgeOutcome> {
        let resp = self.call_ok(
            "POST",
            &format!("repos/{}/issues/{n}/comments", self.repo()),
            Some(json!({ "body": body })),
            "comment create",
        )?;
        serde_json::from_str::<Value>(resp.trim())
            .ok()
            .and_then(|v| v.get("id").and_then(Value::as_u64))
            .ok_or_else(|| {
                self.unknown(format!("comment create carried no id ({})", body_shape(&resp)))
            })
    }

    fn create_label(&self, name: &str, desc: &str) -> Result<u64, ForgeOutcome> {
        let resp = self.call_ok(
            "POST",
            &format!("repos/{}/labels", self.repo()),
            Some(json!({"name": name, "color": "#00aabb", "description": desc})),
            "label create",
        )?;
        serde_json::from_str::<Value>(resp.trim())
            .ok()
            .and_then(|v| v.get("id").and_then(Value::as_u64))
            .ok_or_else(|| {
                self.unknown(format!("label create carried no id ({})", body_shape(&resp)))
            })
    }
}

fn str_of<'v>(v: &'v Value, k: &str) -> Option<&'v str> {
    v.get(k).and_then(Value::as_str)
}

fn ids(rows: &[Value]) -> Vec<u64> {
    rows.iter()
        .filter_map(|r| r.get("id").and_then(Value::as_u64))
        .collect()
}

fn issue_note(n: u64) -> String {
    format!("disposable issue number: {n}")
}

fn verdict_of(ok: bool) -> &'static str {
    if ok {
        OUTCOME_PASS
    } else {
        OUTCOME_FAIL
    }
}

/// `issue-view-state`: one issue's number, state and labels by number.
fn issue_view_state(c: &Case) -> CaseOutcome {
    let n = c.fixture("issue-view-state", "issue-view-state fixture")?;
    let v = c.get_json(&format!("repos/{}/issues/{n}", c.repo()), "issue view")?;
    let number_ok = v.get("number").and_then(Value::as_u64) == Some(n);
    let state = str_of(&v, "state").unwrap_or("(absent)").to_string();
    let labels_ok = v.get("labels").is_some_and(Value::is_array);
    let ok = number_ok && state == "open" && labels_ok;
    Ok(c.finish(
        verdict_of(ok),
        "number + state=open + a labels array, by number".into(),
        format!("number match: {number_ok}; state={state}; labels array: {labels_ok}"),
        vec![
            issue_note(n),
            "PR-number resolution through the issue read is not exercised (needs a PR fixture — later slice)".into(),
        ],
    ))
}

/// `issue-list`: complete multi-page pagination. A walk that never left
/// page 1 did not exercise pagination, so it stays `unknown`.
fn issue_list(c: &Case) -> CaseOutcome {
    let n1 = c.fixture("issue-list", "issue-list fixture one")?;
    let n2 = c.fixture("issue-list", "issue-list fixture two")?;
    let w = c.walk(&format!("repos/{}/issues?state=all&type=issues", c.repo()), 10, 200)?;
    let numbers: Vec<u64> = w
        .rows
        .iter()
        .filter_map(|i| i.get("number").and_then(Value::as_u64))
        .collect();
    let found = numbers.contains(&n1) && numbers.contains(&n2);
    let multi = w.pages >= 2 && !w.page_ignored;
    let outcome = match (found, multi) {
        (false, _) => OUTCOME_FAIL,
        (true, true) => OUTCOME_PASS,
        (true, false) => OUTCOME_UNKNOWN,
    };
    let mut notes = vec![format!("disposable issue numbers: {n1}, {n2}")];
    if !multi {
        notes.push("multi-page pagination not exercised (list fit one page or page was ignored) — not a pass".into());
    }
    Ok(c.finish(
        outcome,
        "a walked multi-page list, ended by an empty page, contains every created issue".into(),
        format!(
            "{} row(s) over {} page(s); fixtures #{n1},#{n2} {}",
            w.rows.len(),
            w.pages,
            if found { "present" } else { "MISSING" }
        ),
        notes,
    ))
}

/// `issue-search`: a body-marker fixture appears in the first page of the
/// search endpoint within a bounded eventual window.
fn issue_search(c: &Case) -> CaseOutcome {
    let marker = format!("loomp-search-{}", c.cfg.run_ns);
    let title = issue_title(c.cfg, "issue-search");
    let n = c.fixture("issue-search", &marker)?;
    let path = format!("repos/issues/search?q={marker}&type=issues&state=all");
    let mut hit = false;
    let mut attempts = 0usize;
    while attempts < SEARCH_ATTEMPTS {
        attempts += 1;
        let v = c.get_json(&path, "issue search")?;
        let arr = v.as_array().cloned().unwrap_or_default();
        // Search is cross-repository: a number alone can collide, so the
        // namespaced title must match too.
        hit = arr.iter().any(|i| {
            i.get("number").and_then(Value::as_u64) == Some(n)
                && str_of(i, "title") == Some(title.as_str())
        });
        if hit {
            break;
        }
        std::thread::sleep(SEARCH_POLL);
    }
    Ok(c.finish(
        verdict_of(hit),
        format!(
            "search finds fixture #{n} by body marker within {SEARCH_ATTEMPTS} reads (eventual)"
        ),
        format!("hit: {hit} after {attempts} read(s) at ~{}s spacing", SEARCH_POLL.as_secs()),
        vec![issue_note(n)],
    ))
}

/// `issue-edit-labels` (HIGH): a colon + Unicode label survives add and
/// removal, both read back on the issue.
fn issue_edit_labels(c: &Case) -> CaseOutcome {
    let n = c.fixture("issue-edit-labels", "labels fixture")?;
    let name = format!("loomp-{}: labels ✓", c.cfg.run_ns);
    let id = c.create_label(&name, "probe")?;
    let issue = format!("repos/{}/issues/{n}", c.repo());
    let labels_on = |c: &Case| -> Result<Vec<(u64, String)>, ForgeOutcome> {
        let v = c.get_json(&issue, "issue read-back")?;
        Ok(v.get("labels")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|l| Some((l.get("id")?.as_u64()?, str_of(l, "name")?.to_string())))
                    .collect()
            })
            .unwrap_or_default())
    };
    c.call_ok("POST", &format!("{issue}/labels"), Some(json!({"labels": [id]})), "label add")?;
    let added = labels_on(c)?.iter().any(|(i, nm)| *i == id && nm == &name);
    c.call_ok("DELETE", &format!("{issue}/labels/{id}"), None, "label remove")?;
    let removed = !labels_on(c)?.iter().any(|(i, _)| *i == id);
    let deleted = c
        .call("DELETE", &format!("repos/{}/labels/{id}", c.repo()), None)
        .map(|(code, _)| code.to_string())
        .unwrap_or_else(|_| "transport fault".into());
    Ok(c.finish(
        verdict_of(added && removed),
        "a colon+Unicode label reads back byte-equal after add, and is gone after removal".into(),
        format!("add visible byte-equal: {added}; removal visible: {removed}"),
        vec![
            issue_note(n),
            format!("label id {id}; catalogue DELETE answered {deleted} (run cleanup also sweeps the namespace)"),
            "competing-claims ordering (concurrent add/remove from two clients) is a two-client case — not this slice".into(),
        ],
    ))
}

/// `issue-edit-body`: machine markers, Unicode, quotes and tags survive a
/// body edit byte-equal.
fn issue_edit_body(c: &Case) -> CaseOutcome {
    let n = c.fixture("issue-edit-body", "body fixture")?;
    let text =
        "<!-- loom:park reason=\"probe\" -->\n\nunicode ✓ 'single' \"double\" <tags &> `code`";
    let issue = format!("repos/{}/issues/{n}", c.repo());
    c.call_ok("PATCH", &issue, Some(json!({ "body": text })), "body edit")?;
    let v = c.get_json(&issue, "issue read-back")?;
    let read = str_of(&v, "body").unwrap_or_default();
    let equal = read == text;
    let observed = if equal {
        "byte-equal".to_string()
    } else {
        let at = read
            .bytes()
            .zip(text.bytes())
            .take_while(|(a, b)| a == b)
            .count();
        format!(
            "REWRITTEN: {} bytes read vs {} sent; first difference at byte {at}",
            read.len(),
            text.len()
        )
    };
    Ok(c.finish(
        verdict_of(equal),
        "edited body reads back byte-equal (marker, Unicode, quotes, tags intact)".into(),
        observed,
        vec![issue_note(n)],
    ))
}

/// `issue-close-with-reason`: close as not-planned, read the reason back,
/// reopen and read that back. A reason field the server never returns is
/// native absence (`unsupported`); a wrong reason is a `fail`.
fn issue_close_with_reason(c: &Case) -> CaseOutcome {
    let n = c.fixture("issue-close-with-reason", "close fixture")?;
    let issue = format!("repos/{}/issues/{n}", c.repo());
    c.call_ok(
        "PATCH",
        &issue,
        Some(json!({"state": "closed", "state_reason": "not_planned"})),
        "close",
    )?;
    let v = c.get_json(&issue, "close read-back")?;
    let state = str_of(&v, "state").unwrap_or("(absent)").to_string();
    let reason_field = v.get("state_reason");
    let reason = reason_field.and_then(Value::as_str).map(String::from);
    c.call_ok("PATCH", &issue, Some(json!({"state": "open"})), "reopen")?;
    let reopened = str_of(&c.get_json(&issue, "reopen read-back")?, "state") == Some("open");
    let outcome = if state != "closed" || !reopened {
        OUTCOME_FAIL
    } else if reason_field.is_none() {
        OUTCOME_UNSUPPORTED
    } else if reason.as_deref() == Some("not_planned") {
        OUTCOME_PASS
    } else {
        OUTCOME_FAIL
    };
    Ok(c.finish(
        outcome,
        "close reads back state=closed with durable state_reason=not_planned; reopen reads back open".into(),
        format!(
            "state={state}; state_reason={}; reopened: {reopened}",
            match (reason_field, reason.as_deref()) {
                (None, _) => "(field absent)",
                (Some(_), None) => "(null/non-string)",
                (Some(_), Some(r)) => r,
            }
        ),
        vec![
            issue_note(n),
            "a 2xx close acknowledgement alone is not evidence of the reason (#9789)".into(),
        ],
    ))
}

/// `label-sync-catalogue`: create → edit → complete list → delete →
/// complete list. Both lists must reach a provable end: a failed walk after
/// the delete must never read as "deleted".
fn label_sync_catalogue(c: &Case) -> CaseOutcome {
    let name = format!("loomp-{}: catalogue", c.cfg.run_ns);
    let id = c.create_label(&name, "probe")?;
    let label = format!("repos/{}/labels/{id}", c.repo());
    let list = format!("repos/{}/labels", c.repo());
    c.call_ok(
        "PATCH",
        &label,
        Some(json!({"color": "#00ccdd", "description": "probe-edited"})),
        "label edit",
    )?;
    let before = c.walk(&list, 50, 40)?;
    // Gitea stores colours without the leading '#' (live observation).
    let norm = |s: Option<&str>| {
        s.unwrap_or_default()
            .trim_start_matches('#')
            .to_ascii_lowercase()
    };
    let listed = before.rows.iter().any(|l| {
        l.get("id").and_then(Value::as_u64) == Some(id)
            && str_of(l, "name") == Some(name.as_str())
            && norm(str_of(l, "color")) == "00ccdd"
            && str_of(l, "description") == Some("probe-edited")
    });
    c.call_ok("DELETE", &label, None, "label delete")?;
    let after = c.walk(&list, 50, 40)?;
    let gone = !ids(&after.rows).contains(&id);
    Ok(c.finish(
        verdict_of(listed && gone),
        "label create/edit/delete round-trips, each visible in a complete catalogue list".into(),
        format!(
            "edited state listed: {listed}; deleted and absent from a complete re-list: {gone}"
        ),
        vec![format!(
            "label id {id} (numeric) — Loom addresses labels by name; the adapter owns translation"
        )],
    ))
}

/// `comment-list` (HIGH, the lease arbiter): two comments read back in
/// creation order, each with a forge-assigned `updated_at`, over a list read
/// that provably reached its end.
fn comment_list(c: &Case) -> CaseOutcome {
    let n = c.fixture("comment-list", "comment-list fixture")?;
    let m1 = format!("probe-c1-{}", c.cfg.run_ns);
    let m2 = format!("probe-c2-{}", c.cfg.run_ns);
    let id1 = c.post_comment(n, &m1)?;
    let id2 = c.post_comment(n, &m2)?;
    let w = c.walk(&format!("repos/{}/issues/{n}/comments", c.repo()), 50, 20)?;
    let pos = |id: u64| {
        w.rows
            .iter()
            .position(|r| r.get("id").and_then(Value::as_u64) == Some(id))
    };
    let body_at = |i: Option<usize>| i.and_then(|i| str_of(&w.rows[i], "body"));
    let (p1, p2) = (pos(id1), pos(id2));
    let order_ok = matches!((p1, p2), (Some(a), Some(b)) if a < b)
        && body_at(p1) == Some(m1.as_str())
        && body_at(p2) == Some(m2.as_str());
    let with_updated = w
        .rows
        .iter()
        .filter(|r| str_of(r, "updated_at").is_some())
        .count();
    let ok = order_ok && with_updated == w.rows.len();
    Ok(c.finish(
        verdict_of(ok),
        "both comments, in creation order, updated_at on every row, complete list".into(),
        format!(
            "{} row(s); order {}; updated_at on {with_updated}/{}; pagination: {}",
            w.rows.len(),
            if order_ok { "preserved" } else { "BROKEN" },
            w.rows.len(),
            if w.page_ignored {
                "page param ignored — page 1 under the limit, complete by construction"
            } else {
                "walked to an empty page"
            }
        ),
        vec![issue_note(n), format!("comment ids: {id1}, {id2}")],
    ))
}

/// `comment-edit-delete`: edit in place (lease renewal re-PATCHes a comment
/// it located by id) and delete, both read back over complete lists.
fn comment_edit_delete(c: &Case) -> CaseOutcome {
    let n = c.fixture("comment-edit-delete", "comment edit fixture")?;
    let cid = c.post_comment(n, "original")?;
    let comment = format!("repos/{}/issues/comments/{cid}", c.repo());
    let list = format!("repos/{}/issues/{n}/comments", c.repo());
    c.call_ok("PATCH", &comment, Some(json!({"body": "edited"})), "comment edit")?;
    let edited = c.walk(&list, 50, 20)?.rows.iter().any(|r| {
        r.get("id").and_then(Value::as_u64) == Some(cid) && str_of(r, "body") == Some("edited")
    });
    c.call_ok("DELETE", &comment, None, "comment delete")?;
    let gone = !ids(&c.walk(&list, 50, 20)?.rows).contains(&cid);
    Ok(c.finish(
        verdict_of(edited && gone),
        "comment edits in place (same id) and is absent from a complete re-list after delete"
            .into(),
        format!("edit visible on the same id: {edited}; delete confirmed: {gone}"),
        vec![issue_note(n), format!("comment id {cid}")],
    ))
}

/// `timeline-read`: the timeline merges the comment and the close event.
fn timeline_read(c: &Case) -> CaseOutcome {
    let n = c.fixture("timeline-read", "timeline fixture")?;
    c.post_comment(n, "timeline marker")?;
    c.call_ok(
        "PATCH",
        &format!("repos/{}/issues/{n}", c.repo()),
        Some(json!({"state": "closed"})),
        "close",
    )?;
    let path = format!("repos/{}/issues/{n}/timeline", c.repo());
    // Native absence: the endpoint itself 404s for an issue we just read back.
    if let Ok((404, _)) = c.call("GET", &format!("{path}?page=1&limit=1"), None) {
        return Ok(c.finish(
            OUTCOME_UNSUPPORTED,
            "timeline carries the comment and close events".into(),
            "timeline endpoint absent (404) for an existing issue".into(),
            vec![issue_note(n)],
        ));
    }
    let w = c.walk(&path, 50, 20)?;
    let kinds: Vec<&str> = w.rows.iter().filter_map(|e| str_of(e, "type")).collect();
    let ok = kinds.contains(&"comment") && kinds.contains(&"close");
    let stamped = w.rows.iter().all(|e| str_of(e, "created_at").is_some());
    Ok(c.finish(
        verdict_of(ok && stamped),
        "timeline carries the comment and close events, every event timestamped".into(),
        format!("event types: [{}]; created_at on every event: {stamped}", kinds.join(", ")),
        vec![issue_note(n)],
    ))
}

/// `epic-sub-issues`: GitHub's sub-issue containment. The fixture proves
/// the issue exists, so a 404 is the endpoint's absence (`unsupported`). A
/// 2xx on a bare read proves no containment semantics — `unknown`, never
/// a pass. Gitea dependencies (block/blocked-by) are a different edge.
fn epic_sub_issues(c: &Case) -> CaseOutcome {
    let n = c.fixture("epic-sub-issues", "parent fixture")?;
    let (code, body) = c.call("GET", &format!("repos/{}/issues/{n}/sub_issues", c.repo()), None)?;
    let (outcome, observed) = match code {
        404 | 405 => (
            OUTCOME_UNSUPPORTED,
            format!("endpoint absent (HTTP {code}) for an existing issue — containment is not a native edge"),
        ),
        200..=299 => (
            OUTCOME_UNKNOWN,
            format!("endpoint answered {code} ({}); containment not exercised — a bare read is not evidence", body_shape(&body)),
        ),
        _ => (OUTCOME_UNKNOWN, format!("answered HTTP {code} ({})", body_shape(&body))),
    };
    Ok(c.finish(
        outcome,
        "a sub-issue containment edge readable through the API".into(),
        observed,
        vec![
            issue_note(n),
            "dependency edges (block/blocked-by) are a different relationship from sub-issue containment".into(),
        ],
    ))
}
