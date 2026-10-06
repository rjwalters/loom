//! `gh pr checks <N>` served from ETag'd REST reads (#10516).
//!
//! `gh` (2.100.0, `pkg/cmd/pr/checks`) reads the PR's `statusCheckRollup`
//! over GraphQL. The front reproduces the same output from the REST reads
//! `forge wait-checks` already makes ([`GhReads`]: the PR, its head commit's
//! check-runs and combined status, each a conditional request whose `304` is
//! free), or declines and passes the call through. The served shapes:
//!
//! - non-TTY text: one `name\tbucket\telapsed\tlink\tdescription` line per
//!   check (`cancel` printed as `fail`, elapsed as Go's `Duration.String()`
//!   or `0`), exit `1` if any check failed, else `8` if any is pending,
//!   else `0`;
//! - `--json` over [`JSON_FIELDS`]: a compact array with sorted keys, exit `0`;
//! - no checks at all, either mode: `no checks reported on the '<head>'
//!   branch` on stderr, exit `1` — the empty-read signature
//!   `champion-pr-merge` keys on (#6211).
//!
//! # Why some calls still pass through
//!
//! `gh`'s row order is not a function of the rows. It sorts the GraphQL
//! contexts — whose order REST does not expose — by `startedAt` with the
//! unstable `sort.Slice`, then (text mode) sorts again with a comparator that
//! is not a strict weak order. So the front reproduces Go's algorithm exactly
//! ([`super::go_sort`]) and, for every order of the rows that share a
//! `startedAt` (the only freedom the unknown GraphQL order leaves), checks
//! the output is the same. Text: every such order is tried, up to
//! [`MAX_ORDERS`]; JSON: rows sharing a `startedAt` must project
//! identically. Any ambiguity, any shape outside the contract (a duplicated
//! check name — `gh` dedups by workflow and event, which REST does not carry;
//! a non-canonical timestamp; a JSON string Go would escape differently) or
//! any read failure declines. Golden fixtures captured from the real `gh`
//! pin all of it (`pr_checks_tests`).

use std::collections::HashSet;
use std::path::Path;

use serde_json::Value;

use super::go_sort::sort_slice;
use super::Served;
use crate::forge_wait_checks::reads::GhReads;

/// `--json` fields served. `event` and `workflow` need a workflow-run read
/// per check suite and are passed through.
pub const JSON_FIELDS: &[&str] = &[
    "bucket",
    "completedAt",
    "description",
    "link",
    "name",
    "startedAt",
    "state",
];

/// Most row orders tried before declining (8!).
pub const MAX_ORDERS: u64 = 40_320;

/// Go's zero `time.Time`, as `encoding/json` prints it.
const ZERO_TIME: &str = "0001-01-01T00:00:00Z";

/// One parsed `gh pr checks` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub number: u32,
    pub repo: Option<String>,
    /// `--json` fields, sorted and de-duplicated (Go prints map keys sorted).
    pub json: Option<Vec<String>>,
}

/// Parse the argv after `pr checks`. `None` for any shape not served:
/// `--watch`, `--fail-fast`, `--interval`, `--required`, `--jq`,
/// `--template`, a branch / URL / no selector, an unknown flag or field.
#[must_use]
pub fn parse(rest: &[String]) -> Option<Query> {
    let (mut number, mut repo, mut json) = (None, None, None);
    let mut i = 0;
    while i < rest.len() {
        let a = rest[i].as_str();
        let mut value = |flag: &str| -> Option<Option<String>> {
            if a == flag {
                i += 1;
                return Some(rest.get(i).cloned());
            }
            a.strip_prefix(flag)
                .and_then(|v| v.strip_prefix('='))
                .map(|v| Some(v.to_string()))
        };
        if let Some(v) = value("--repo").or_else(|| value("-R")) {
            if repo.replace(v?).is_some() {
                return None;
            }
        } else if let Some(v) = value("--json") {
            let fields = parse_fields(&v?)?;
            if json.replace(fields).is_some() {
                return None;
            }
        } else if a.starts_with('-') || number.is_some() {
            return None;
        } else {
            number = Some(parse_number(a)?);
        }
        i += 1;
    }
    Some(Query {
        number: number?,
        repo,
        json,
    })
}

fn parse_number(a: &str) -> Option<u32> {
    let ok = !a.is_empty() && !a.starts_with('0') && a.bytes().all(|b| b.is_ascii_digit());
    ok.then(|| a.parse().ok()).flatten()
}

fn parse_fields(v: &str) -> Option<Vec<String>> {
    let mut fields = v
        .split(',')
        .map(|f| JSON_FIELDS.contains(&f).then(|| f.to_string()))
        .collect::<Option<Vec<_>>>()?;
    fields.sort();
    fields.dedup();
    Some(fields)
}

/// Serve `rest` (argv after `pr checks`, with an explicit `--repo`) through
/// conditional REST reads made with `next`. `None` passes the call through.
#[must_use]
pub fn serve(rest: &[String], next: &Path, cwd: Option<&Path>) -> Option<Served> {
    let q = parse(rest)?;
    let mut reads = GhReads::new(
        next.to_path_buf(),
        cwd.map(Path::to_path_buf),
        crate::forge_etag_store::disk_cache_dir(),
        Some(q.repo.as_deref()?),
    )
    .ok()?
    .with_caller(super::STATS_CALLER);
    let head = reads.pull(q.number).ok()?;
    let runs = reads.check_runs(&head.sha).ok()?;
    let status = reads.statuses(&head.sha).ok()?;
    render(q.json.as_deref(), head.head_ref.as_deref()?, &runs, &status)
}

/// One `gh pr checks` row, built from REST the way `gh` builds it from
/// GraphQL (`aggregateChecks`).
#[derive(Debug, Clone)]
struct Row {
    name: String,
    state: String,
    bucket: &'static str,
    link: String,
    description: String,
    started: Option<(i64, String)>,
    completed: Option<(i64, String)>,
}

impl Row {
    fn start_key(&self) -> i64 {
        self.started.as_ref().map_or(i64::MIN, |t| t.0)
    }

    fn text_line(&self) -> String {
        let elapsed = match (&self.started, &self.completed) {
            (Some((s, _)), Some((c, _))) if c > s => go_duration(c - s),
            _ => "0".to_string(),
        };
        let bucket = if self.bucket == "cancel" { "fail" } else { self.bucket };
        format!("{}\t{bucket}\t{elapsed}\t{}\t{}\n", self.name, self.link, self.description)
    }

    fn json_object(&self, fields: &[String]) -> Option<String> {
        let time = |t: &Option<(i64, String)>| {
            t.as_ref().map_or(ZERO_TIME.to_string(), |t| t.1.clone())
        };
        let mut out = String::from("{");
        for (n, f) in fields.iter().enumerate() {
            let v = match f.as_str() {
                "bucket" => self.bucket.to_string(),
                "completedAt" => time(&self.completed),
                "description" => self.description.clone(),
                "link" => self.link.clone(),
                "name" => self.name.clone(),
                "startedAt" => time(&self.started),
                "state" => self.state.clone(),
                _ => return None,
            };
            if n > 0 {
                out.push(',');
            }
            out.push_str(&format!("\"{f}\":{}", json_string(&v)?));
        }
        out.push('}');
        Some(out)
    }
}

/// Render `gh pr checks` from REST payloads. `None` declines.
#[must_use]
pub fn render(json: Option<&[String]>, head_ref: &str, runs: &Value, status: &Value) -> Option<Served> {
    let rows = rows(runs, status)?;
    if rows.is_empty() {
        return Some(Served {
            stdout: String::new(),
            stderr: format!("no checks reported on the '{head_ref}' branch\n"),
            code: 1,
        });
    }
    let groups = tie_groups(&rows);
    match json {
        Some(fields) => render_json(&rows, &groups, fields),
        None => {
            let mut outputs = text_outputs(&rows, &groups, 2)?;
            (outputs.len() == 1).then(|| Served {
                stdout: outputs.remove(0),
                stderr: String::new(),
                code: exit_code(&rows),
            })
        }
    }
}

/// `gh`'s exit status for a text render: any `fail` → 1, else any
/// `pending` → 8 (`cancel` and `skipping` are neither).
fn exit_code(rows: &[Row]) -> i32 {
    if rows.iter().any(|r| r.bucket == "fail") {
        1
    } else if rows.iter().any(|r| r.bucket == "pending") {
        8
    } else {
        0
    }
}

fn render_json(rows: &[Row], groups: &[(usize, usize)], fields: &[String]) -> Option<Served> {
    let objects = rows
        .iter()
        .map(|r| r.json_object(fields))
        .collect::<Option<Vec<_>>>()?;
    // gh's order within a tie is the unknown GraphQL order: only identical
    // projections make it irrelevant.
    if groups
        .iter()
        .any(|&(lo, hi)| objects[lo..hi].iter().any(|o| *o != objects[lo]))
    {
        return None;
    }
    Some(Served {
        stdout: format!("[{}]\n", objects.join(",")),
        stderr: String::new(),
        code: 0,
    })
}

/// REST check-runs + combined status → `gh`'s rows, sorted by `startedAt`
/// descending (stable, so each tie keeps an arbitrary but fixed order).
fn rows(runs: &Value, status: &Value) -> Option<Vec<Row>> {
    let mut rows = Vec::new();
    let mut seen = HashSet::new();
    for r in runs.get("check_runs")?.as_array()? {
        let name = r.get("name")?.as_str()?.to_string();
        let st = r.get("status")?.as_str()?;
        let state = if st == "completed" {
            r.get("conclusion")?.as_str()?.to_ascii_uppercase()
        } else {
            st.to_ascii_uppercase()
        };
        // gh dedups by name/workflow/event; REST carries neither of the latter.
        if !seen.insert(("run", name.clone())) {
            return None;
        }
        rows.push(Row {
            bucket: bucket(&state),
            state,
            link: opt_str(r.get("details_url"))?,
            description: String::new(),
            started: timestamp(r.get("started_at"))?,
            completed: timestamp(r.get("completed_at"))?,
            name,
        });
    }
    let statuses = match status.get("statuses") {
        None | Some(Value::Null) => Vec::new(),
        Some(v) => v.as_array()?.clone(),
    };
    for s in &statuses {
        let name = s.get("context")?.as_str()?.to_string();
        let state = match s.get("state")?.as_str()? {
            st @ ("success" | "failure" | "error" | "pending") => st.to_ascii_uppercase(),
            _ => return None,
        };
        if !seen.insert(("status", name.clone())) {
            return None;
        }
        rows.push(Row {
            bucket: bucket(&state),
            state,
            link: opt_str(s.get("target_url"))?,
            description: opt_str(s.get("description"))?,
            started: None,
            completed: None,
            name,
        });
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.start_key()));
    Some(rows)
}

/// `gh`'s bucket table (`aggregateChecks`).
fn bucket(state: &str) -> &'static str {
    match state {
        "SUCCESS" => "pass",
        "SKIPPED" | "NEUTRAL" => "skipping",
        "ERROR" | "FAILURE" | "TIMED_OUT" | "ACTION_REQUIRED" => "fail",
        "CANCELLED" => "cancel",
        _ => "pending",
    }
}

/// A string-or-null field as Go reads it (`null` → `""`); `None` for any
/// other type.
fn opt_str(v: Option<&Value>) -> Option<String> {
    match v {
        None | Some(Value::Null) => Some(String::new()),
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => None,
    }
}

/// `null` → `Some(None)`; a canonical `YYYY-MM-DDTHH:MM:SSZ` →
/// `Some(Some((epoch, text)))`; anything else (fractions, offsets) → `None`,
/// since GraphQL's rendering of it is not proven.
#[allow(clippy::option_option)]
fn timestamp(v: Option<&Value>) -> Option<Option<(i64, String)>> {
    let s = match v {
        None | Some(Value::Null) => return Some(None),
        Some(v) => v.as_str()?,
    };
    let t = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%SZ").ok()?;
    (t.format("%Y-%m-%dT%H:%M:%SZ").to_string() == s)
        .then(|| Some((t.and_utc().timestamp(), s.to_string())))
}

/// Go's `time.Duration.String()` for a positive whole number of seconds.
#[must_use]
pub fn go_duration(secs: i64) -> String {
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    if h > 0 {
        format!("{h}h{m}m{s}s")
    } else if m > 0 {
        format!("{m}m{s}s")
    } else {
        format!("{s}s")
    }
}

/// A JSON string as Go's `encoding/json` writes it with `SetEscapeHTML(false)`
/// — or `None` where its escaping is not reproduced here (control
/// characters, U+2028/U+2029).
fn json_string(s: &str) -> Option<String> {
    if s.chars().any(|c| c < ' ' || c == '\u{2028}' || c == '\u{2029}') {
        return None;
    }
    Some(format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")))
}

/// `[lo, hi)` runs of rows sharing a `startedAt`.
fn tie_groups(rows: &[Row]) -> Vec<(usize, usize)> {
    let mut groups = Vec::new();
    let mut lo = 0;
    for hi in 1..=rows.len() {
        if hi == rows.len() || rows[hi].start_key() != rows[lo].start_key() {
            groups.push((lo, hi));
            lo = hi;
        }
    }
    groups
}

/// `gh`'s `printTable` comparator — deliberately verbatim, including its
/// `"success"` (never a bucket) and its neither-less across non-`fail`
/// buckets.
fn table_less(a: &Row, b: &Row) -> bool {
    if a.bucket == b.bucket {
        if a.name == b.name {
            return a.link < b.link;
        }
        return a.name < b.name;
    }
    a.bucket == "fail" || (a.bucket == "pending" && b.bucket == "success")
}

/// The distinct text outputs over every tie order, stopping once `limit`
/// are found. `None` when there are more than [`MAX_ORDERS`] orders to try.
fn text_outputs(rows: &[Row], groups: &[(usize, usize)], limit: usize) -> Option<Vec<String>> {
    let lines: Vec<String> = rows.iter().map(Row::text_line).collect();
    let render = |order: &[usize]| -> String {
        let mut idx = order.to_vec();
        sort_slice(&mut idx, |&a, &b| table_less(&rows[a], &rows[b]));
        idx.iter().map(|&i| lines[i].as_str()).collect()
    };
    if is_total_order(rows) {
        // Any correct sort gives the one sorted order: no tie order matters.
        return Some(vec![render(&(0..rows.len()).collect::<Vec<_>>())]);
    }
    let mut seen: Vec<String> = Vec::new();
    for_each_order(rows.len(), groups, &mut |order| {
        let out = render(order);
        if !seen.contains(&out) {
            seen.push(out);
        }
        seen.len() < limit
    })?;
    Some(seen)
}

/// [`table_less`] is a strict total order on `rows`: at most one non-`fail`
/// bucket present, and no two rows share `(bucket, name, link)`.
fn is_total_order(rows: &[Row]) -> bool {
    let buckets: HashSet<&str> = rows
        .iter()
        .map(|r| r.bucket)
        .filter(|b| *b != "fail")
        .collect();
    let keys: HashSet<(&str, &str, &str)> = rows
        .iter()
        .map(|r| (r.bucket, r.name.as_str(), r.link.as_str()))
        .collect();
    buckets.len() <= 1 && keys.len() == rows.len()
}

/// Call `f` with every row order that keeps each tie group in place but
/// permutes its members, until `f` returns `false`. `None` (nothing called)
/// when there are more than [`MAX_ORDERS`].
fn for_each_order(
    n: usize,
    groups: &[(usize, usize)],
    f: &mut dyn FnMut(&[usize]) -> bool,
) -> Option<()> {
    let mut total: u64 = 1;
    for &(lo, hi) in groups {
        for k in 2..=(hi - lo) as u64 {
            total = total.checked_mul(k).filter(|t| *t <= MAX_ORDERS)?;
        }
    }
    let perms: Vec<Vec<Vec<usize>>> = groups
        .iter()
        .map(|&(lo, hi)| permutations(&(lo..hi).collect::<Vec<_>>()))
        .collect();
    let mut odometer = vec![0usize; groups.len()];
    let mut order = Vec::with_capacity(n);
    loop {
        order.clear();
        for (g, &k) in odometer.iter().enumerate() {
            order.extend_from_slice(&perms[g][k]);
        }
        if !f(&order) {
            return Some(());
        }
        let mut g = 0;
        loop {
            if g == odometer.len() {
                return Some(());
            }
            odometer[g] += 1;
            if odometer[g] < perms[g].len() {
                break;
            }
            odometer[g] = 0;
            g += 1;
        }
    }
}

/// Every permutation of `items` (lexicographic over positions).
fn permutations(items: &[usize]) -> Vec<Vec<usize>> {
    let mut out = vec![items.to_vec()];
    let mut cur = items.to_vec();
    // Next permutation over the original positions' ranks.
    let mut rank: Vec<usize> = (0..items.len()).collect();
    loop {
        let Some(i) = (1..rank.len()).rev().find(|&i| rank[i - 1] < rank[i]) else {
            return out;
        };
        let j = (i..rank.len()).rev().find(|&j| rank[j] > rank[i - 1]).unwrap_or(i);
        rank.swap(i - 1, j);
        rank[i..].reverse();
        cur.clear();
        cur.extend(rank.iter().map(|&r| items[r]));
        out.push(cur.clone());
    }
}

#[cfg(test)]
#[path = "pr_checks_tests.rs"]
mod tests;
