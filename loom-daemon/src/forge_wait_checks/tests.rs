//! Fake-forge tests for `forge wait-checks` (#10330).
//!
//! A stub `gh` (a shell script) serves canned `gh api --include` responses
//! from per-URL fixture files and honours `If-None-Match` exactly like the
//! forge: same ETag → `304` (and gh's exit 1). A fake [`Clock`] drives the
//! wait without sleeping and rewrites the fixtures as simulated time passes,
//! so the whole stack — [`crate::forge_etag_store::fetch_conditional`], the
//! ETag memo and disk store, the classifiers and the loop — runs for real.
//! Every invocation is logged as `<status> <inm|-> <url>`.

#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};

use super::reads::GhReads;
use super::*;

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
const SHA2: &str = "fedcba9876543210fedcba9876543210fedcba98";

struct Forge {
    dir: tempfile::TempDir,
    store: tempfile::TempDir,
    gh: PathBuf,
}

fn key(url: &str) -> String {
    url.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

impl Forge {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        let gh = dir.path().join("gh");
        let d = dir.path().display();
        let script = format!(
            r#"#!/bin/sh
url=""; inm=""; prev=""
for a in "$@"; do
  [ "$prev" = "-H" ] && inm="${{a#If-None-Match: }}"
  case "$a" in repos/*|graphql) [ -z "$url" ] && url="$a" ;; esac
  prev="$a"
done
k=$(printf '%s' "$url" | tr -c 'A-Za-z0-9' '_')
if [ -f '{d}'/"$k.raw" ]; then
  echo "RAW - $url" >> '{d}/calls.log'
  cat '{d}'/"$k.raw"
  cat '{d}'/"$k.err" 1>&2
  exit 1
fi
if [ -f '{d}'/"$k.out" ]; then
  echo "OUT - $url" >> '{d}/calls.log'
  cat '{d}'/"$k.out"
  exit 0
fi
if [ ! -f '{d}'/"$k.body" ]; then
  echo "404 - $url" >> '{d}/calls.log'
  printf 'HTTP/2.0 404 Not Found\r\n\r\n{{"message":"Not Found"}}'
  exit 1
fi
etag=$(cat '{d}'/"$k.etag")
if [ -n "$inm" ] && [ "$inm" = "$etag" ]; then
  echo "304 inm $url" >> '{d}/calls.log'
  printf 'HTTP/2.0 304 Not Modified\r\n\r\n'
  echo 'gh: Not Modified (HTTP 304)' 1>&2
  exit 1
fi
if [ -n "$inm" ]; then t=inm; else t=-; fi
echo "200 $t $url" >> '{d}/calls.log'
printf 'HTTP/2.0 200 OK\r\nEtag: %s\r\n\r\n' "$etag"
cat '{d}'/"$k.body"
"#
        );
        std::fs::write(&gh, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let f = Self { dir, store, gh };
        // Defaults: PR #42 at SHA on main, no legacy statuses, no required
        // contexts (both lookup sources answer empty).
        f.pull(SHA);
        f.serve(&status_url(SHA), &json!({"statuses": [], "total_count": 0}));
        f.required(&[]);
        f
    }

    /// Serve `body` (with a content-derived ETag) for `url`.
    fn serve(&self, url: &str, body: &Value) {
        let text = body.to_string();
        let etag = format!("W/\"{}\"", crate::short_hash::short_sha16(&text));
        std::fs::write(self.dir.path().join(format!("{}.body", key(url))), &text).unwrap();
        std::fs::write(self.dir.path().join(format!("{}.etag", key(url))), etag).unwrap();
    }

    /// Refuse `url` with `status` (a raw `--include` response and gh's
    /// stderr line), until [`Forge::serve`] is called for it again.
    fn refuse(&self, url: &str, status: u16, headers: &str, body: &str) {
        let raw = format!("HTTP/2.0 {status} Forbidden\r\n{headers}\r\n{body}");
        let msg = crate::forge_denial::body_message(body).unwrap_or_default();
        std::fs::write(self.dir.path().join(format!("{}.raw", key(url))), raw).unwrap();
        std::fs::write(
            self.dir.path().join(format!("{}.err", key(url))),
            format!("gh: {msg} (HTTP {status})\n"),
        )
        .unwrap();
    }

    fn unrefuse(&self, url: &str) {
        let _ = std::fs::remove_file(self.dir.path().join(format!("{}.raw", key(url))));
    }

    fn pull(&self, sha: &str) {
        self.serve(
            "repos/o/r/pulls/42",
            &json!({"number": 42, "head": {"sha": sha}, "base": {"ref": "main"}}),
        );
    }

    fn runs(&self, sha: &str, rows: &[Value]) {
        self.serve(&runs_url(sha), &json!({"total_count": rows.len(), "check_runs": rows}));
    }

    /// The required-context lookup's two `--jq` reads (rulesets, classic).
    fn required(&self, contexts: &[&str]) {
        let out = contexts
            .iter()
            .map(|c| format!("{c}\n"))
            .collect::<String>();
        let rules = self
            .dir
            .path()
            .join(format!("{}.out", key("repos/o/r/rules/branches/main")));
        std::fs::write(rules, out).unwrap();
        std::fs::write(self.dir.path().join("graphql.out"), "").unwrap();
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.path().join("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }

    fn reads(&self) -> GhReads {
        GhReads::new(self.gh.clone(), None, self.store.path().to_path_buf(), Some("o/r")).unwrap()
    }

    fn polls(&self) -> usize {
        self.calls()
            .iter()
            .filter(|c| c.contains("/check-runs"))
            .count()
    }
}

fn runs_url(sha: &str) -> String {
    format!("repos/o/r/commits/{sha}/check-runs?per_page=100")
}

fn status_url(sha: &str) -> String {
    format!("repos/o/r/commits/{sha}/status?per_page=100")
}

fn run(name: &str, status: &str, conclusion: Option<&str>) -> Value {
    json!({"name": name, "status": status, "conclusion": conclusion, "id": 1,
           "html_url": format!("https://github.com/o/r/runs/{name}"),
           "details_url": "https://github.com/o/r/actions/runs/555/job/1"})
}

/// A fake clock; `events` (seconds, action) fire once simulated time passes them.
struct FakeClock<'a> {
    now: Duration,
    events: Vec<(u64, Box<dyn Fn() + 'a>)>,
}

impl<'a> FakeClock<'a> {
    fn new() -> Self {
        Self {
            now: Duration::ZERO,
            events: Vec::new(),
        }
    }
    fn at(mut self, secs: u64, f: impl Fn() + 'a) -> Self {
        self.events.push((secs, Box::new(f)));
        self
    }
}

impl Clock for FakeClock<'_> {
    fn elapsed(&self) -> Duration {
        self.now
    }
    fn sleep(&mut self, d: Duration) {
        self.now += d;
        let now = self.now.as_secs();
        let due: Vec<usize> = (0..self.events.len())
            .filter(|i| self.events[*i].0 <= now)
            .collect();
        for i in due.into_iter().rev() {
            let (_, f) = self.events.remove(i);
            f();
        }
    }
}

fn opts(timeout: u64) -> Opts {
    Opts {
        timeout: Duration::from_secs(timeout),
        required_only: false,
        base: None,
        min_interval: DEFAULT_MIN_INTERVAL,
        max_interval: DEFAULT_MAX_INTERVAL,
        settle_polls: zero_checks::DEFAULT_SETTLE_POLLS,
        settle_interval: zero_checks::DEFAULT_SETTLE_INTERVAL,
    }
}

fn go(f: &Forge, sel: Selector, o: &Opts, clock: &mut FakeClock) -> Outcome {
    wait(&mut f.reads(), &sel, o, clock).0
}

#[test]
fn selector_parsing() {
    assert_eq!(parse_selector("42"), Ok(Selector::Pr(42)));
    assert_eq!(parse_selector("ABCDEF1"), Ok(Selector::Sha("abcdef1".into())));
    assert_eq!(parse_selector(SHA), Ok(Selector::Sha(SHA.into())));
    for bad in [
        "",
        "0",
        "abc",
        "feature/x",
        "12345678901234567890",
        &"a".repeat(41),
    ] {
        assert!(parse_selector(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn backoff_goes_30_to_120_geometrically() {
    let mut b = Backoff::new(30, 120);
    let got: Vec<u64> = (0..7).map(|_| b.take()).collect();
    assert_eq!(got, vec![30, 45, 67, 100, 120, 120, 120]);
    let mut tiny = Backoff::new(0, 0);
    assert_eq!((tiny.take(), tiny.take()), (1, 1));
}

#[test]
fn green_in_one_poll() {
    let f = Forge::new();
    f.runs(SHA, &[run("build", "completed", Some("success"))]);
    let o = go(&f, Selector::Pr(42), &opts(1800), &mut FakeClock::new());
    assert_eq!(o, Outcome::Green { sha: SHA.into() });
    assert_eq!(f.polls(), 1);
}

#[test]
fn red_includes_action_required_and_names_the_run_id() {
    let f = Forge::new();
    f.runs(
        SHA,
        &[
            run("build", "completed", Some("success")),
            run("deploy-gate", "completed", Some("action_required")),
            run("slow", "in_progress", None),
        ],
    );
    let o = go(&f, Selector::Pr(42), &opts(1800), &mut FakeClock::new());
    let Outcome::Red { sha, failed } = &o else {
        panic!("{o:?}")
    };
    assert_eq!(sha, SHA);
    assert_eq!(failed.len(), 1);
    assert_eq!(o.sentinel(), format!("LOOM-CHECKS-RED {SHA} deploy-gate"));
    assert_eq!(o.detail(), vec!["deploy-gate\thttps://github.com/o/r/runs/deploy-gate\t555"]);
}

#[test]
fn pending_then_green() {
    let f = Forge::new();
    f.runs(SHA, &[run("build", "in_progress", None)]);
    let mut clock = FakeClock::new().at(100, || {
        f.runs(SHA, &[run("build", "completed", Some("success"))]);
    });
    let o = go(&f, Selector::Pr(42), &opts(1800), &mut clock);
    assert_eq!(o, Outcome::Green { sha: SHA.into() });
    // 0, 30, 75, 142 → green seen on the 4th poll.
    assert_eq!(f.polls(), 4);
}

#[test]
fn pending_then_timeout_polls_once_more_at_the_deadline() {
    let f = Forge::new();
    f.runs(SHA, &[run("build", "queued", None)]);
    let mut clock = FakeClock::new();
    let o = go(&f, Selector::Pr(42), &opts(300), &mut clock);
    assert_eq!(
        o,
        Outcome::Timeout {
            sha: SHA.into(),
            pending: vec!["build".into()]
        }
    );
    assert_eq!(clock.now, Duration::from_secs(300));
    // 0, 30, 75, 142, 242, 300.
    assert_eq!(f.polls(), 6);
}

#[test]
fn timeout_zero_is_exactly_one_poll() {
    let f = Forge::new();
    f.runs(SHA, &[run("build", "queued", None)]);
    let o = go(&f, Selector::Sha(SHA.into()), &opts(0), &mut FakeClock::new());
    assert!(matches!(o, Outcome::Timeout { .. }), "{o:?}");
    assert_eq!(f.polls(), 1);
}

#[test]
fn zero_rows_with_no_required_contexts_settles_none() {
    let f = Forge::new();
    f.runs(SHA, &[]);
    let mut clock = FakeClock::new();
    let o = go(&f, Selector::Pr(42), &opts(1800), &mut clock);
    assert_eq!(o, Outcome::NoChecks { sha: SHA.into() });
    assert_eq!(f.polls(), 3, "bounded settle: three empty reads");
    assert_eq!(clock.now, Duration::from_secs(10), "spaced by the 5s settle interval");
}

#[test]
fn zero_rows_with_a_required_context_times_out_never_none() {
    let f = Forge::new();
    f.runs(SHA, &[]);
    f.required(&["Backend"]);
    let o = go(&f, Selector::Pr(42), &opts(600), &mut FakeClock::new());
    assert_eq!(
        o,
        Outcome::Timeout {
            sha: SHA.into(),
            pending: vec!["Backend".into()]
        }
    );
    let lookups = f.calls().iter().filter(|c| c.starts_with("OUT")).count();
    assert_eq!(lookups, 2, "the required lookup runs once per wait (two sources)");
}

#[test]
fn unreadable_payload_is_error() {
    let f = Forge::new();
    f.serve(&runs_url(SHA), &json!({"total_count": 1, "check_runs": "nope"}));
    let o = go(&f, Selector::Pr(42), &opts(1800), &mut FakeClock::new());
    assert!(matches!(&o, Outcome::Error(w) if w.starts_with("unreadable")), "{o:?}");
}

#[test]
fn short_read_is_error_truncated_never_green() {
    let f = Forge::new();
    f.serve(
        &runs_url(SHA),
        &json!({"total_count": 5, "check_runs": [run("build", "completed", Some("success"))]}),
    );
    let o = go(&f, Selector::Pr(42), &opts(1800), &mut FakeClock::new());
    assert!(matches!(&o, Outcome::Error(w) if w.starts_with("truncated")), "{o:?}");
}

#[test]
fn a_second_page_is_read_when_total_count_exceeds_one_page() {
    let f = Forge::new();
    let page1: Vec<Value> = (0..100)
        .map(|i| run(&format!("c{i}"), "completed", Some("success")))
        .collect();
    f.serve(&runs_url(SHA), &json!({"total_count": 101, "check_runs": page1}));
    f.serve(
        &format!("{}&page=2", runs_url(SHA)),
        &json!({"total_count": 101, "check_runs": [run("last", "completed", Some("failure"))]}),
    );
    let o = go(&f, Selector::Sha(SHA.into()), &opts(0), &mut FakeClock::new());
    assert_eq!(o.sentinel(), format!("LOOM-CHECKS-RED {SHA} last"));
}

#[test]
fn head_moving_mid_wait_is_head_moved() {
    let f = Forge::new();
    f.runs(SHA, &[run("build", "in_progress", None)]);
    f.runs(SHA2, &[run("build", "completed", Some("success"))]);
    let mut clock = FakeClock::new().at(60, || f.pull(SHA2));
    let o = go(&f, Selector::Pr(42), &opts(1800), &mut clock);
    assert_eq!(
        o,
        Outcome::HeadMoved {
            old: SHA.into(),
            new: SHA2.into()
        }
    );
}

#[test]
fn default_mode_waits_for_a_required_context_that_has_not_registered() {
    let f = Forge::new();
    f.required(&["Backend"]);
    f.runs(SHA, &[run("labeler", "completed", Some("success"))]);
    let mut clock = FakeClock::new().at(40, || {
        f.runs(
            SHA,
            &[
                run("labeler", "completed", Some("success")),
                run("Backend", "completed", Some("success")),
            ],
        );
    });
    let o = go(&f, Selector::Pr(42), &opts(1800), &mut clock);
    assert_eq!(o, Outcome::Green { sha: SHA.into() });
    assert_eq!(f.polls(), 3);
}

#[test]
fn required_only_settles_on_required_checks_and_ignores_informational_red() {
    let f = Forge::new();
    f.required(&["Backend"]);
    f.runs(
        SHA,
        &[
            run("lint", "completed", Some("failure")),
            run("Backend", "completed", Some("success")),
            run("docs", "in_progress", None),
        ],
    );
    let mut o = opts(1800);
    o.required_only = true;
    assert_eq!(
        go(&f, Selector::Pr(42), &o, &mut FakeClock::new()),
        Outcome::Green { sha: SHA.into() }
    );
    f.runs(SHA, &[run("Backend", "completed", Some("timed_out"))]);
    let red = go(&f, Selector::Pr(42), &o, &mut FakeClock::new());
    assert_eq!(red.sentinel(), format!("LOOM-CHECKS-RED {SHA} Backend"));
}

#[test]
fn a_failing_legacy_status_is_red() {
    let f = Forge::new();
    f.runs(SHA, &[run("build", "completed", Some("success"))]);
    f.serve(
        &status_url(SHA),
        &json!({"statuses": [{"context": "ci/jenkins", "state": "failure",
                              "target_url": "https://ci.example/7"}], "total_count": 1}),
    );
    let o = go(&f, Selector::Pr(42), &opts(1800), &mut FakeClock::new());
    assert_eq!(o.sentinel(), format!("LOOM-CHECKS-RED {SHA} ci/jenkins"));
    assert_eq!(o.detail(), vec!["ci/jenkins\thttps://ci.example/7\t-"]);
}

#[test]
fn a_missing_pr_is_error_not_timeout() {
    let f = Forge::new();
    let o = go(&f, Selector::Pr(7), &opts(1800), &mut FakeClock::new());
    assert!(matches!(&o, Outcome::Error(w) if w.contains("HTTP 404")), "{o:?}");
}

#[test]
fn sha_mode_resolves_the_default_branch_for_the_required_lookup() {
    let f = Forge::new();
    f.serve("repos/o/r", &json!({"default_branch": "main"}));
    f.runs(SHA, &[run("build", "completed", Some("success"))]);
    let o = go(&f, Selector::Sha(SHA.into()), &opts(0), &mut FakeClock::new());
    assert_eq!(o, Outcome::Green { sha: SHA.into() });
    assert!(f.calls().iter().any(|c| c.ends_with(" repos/o/r")));
    assert!(!f.calls().iter().any(|c| c.contains("pulls/")), "SHA mode never reads the PR");
}

/// The issue's request budget: a 15-minute CI run whose rollup changes 6
/// times costs at most 12 polls; every unchanged read goes out with
/// `If-None-Match` and comes back `304`; at most 15 responses are non-304
/// (the ones that spend primary rate-limit quota).
#[test]
fn request_budget_for_a_fifteen_minute_run() {
    let f = Forge::new();
    f.required(&["Backend"]);
    let names = ["Backend", "lint", "docs", "e2e"];
    let state = |done: usize, started: bool| -> Vec<Value> {
        names
            .iter()
            .enumerate()
            .map(|(i, n)| {
                if i < done {
                    run(n, "completed", Some("success"))
                } else if started {
                    run(n, "in_progress", None)
                } else {
                    run(n, "queued", None)
                }
            })
            .collect()
    };
    f.runs(SHA, &state(0, false));
    let mut clock = FakeClock::new()
        .at(60, || f.runs(SHA, &state(0, true)))
        .at(180, || f.runs(SHA, &state(1, true)))
        .at(360, || f.runs(SHA, &state(2, true)))
        .at(540, || f.runs(SHA, &state(3, true)))
        // e2e is re-run: back to queued.
        .at(720, || f.runs(SHA, &state(3, false)))
        .at(900, || f.runs(SHA, &state(4, true)));
    let o = go(&f, Selector::Pr(42), &opts(1800), &mut clock);
    assert_eq!(o, Outcome::Green { sha: SHA.into() });

    let calls = f.calls();
    let polls = f.polls();
    let not_modified = calls.iter().filter(|c| c.starts_with("304")).count();
    let quota = calls.len() - not_modified;
    eprintln!(
        "wait-checks budget: {polls} polls, {} gh calls, {not_modified} x 304, {quota} non-304, \
         settled at {}s",
        calls.len(),
        clock.now.as_secs()
    );
    assert!(polls <= 12, "{polls} polls");
    assert!(quota <= 15, "{quota} non-304 responses: {calls:#?}");
    // Every read after the first per URL was conditional, and every one whose
    // fixture had not changed was a 304.
    let mut seen = std::collections::HashSet::new();
    for c in &calls {
        let url = c.rsplit(' ').next().unwrap();
        if c.starts_with("OUT") {
            continue;
        }
        if !seen.insert(url.to_string()) {
            assert!(c.contains(" inm "), "an unconditional repeat read: {c}");
        }
    }
    let pulls_200 = calls
        .iter()
        .filter(|c| c.starts_with("200") && c.contains("pulls/"))
        .count();
    let status_200 = calls
        .iter()
        .filter(|c| c.starts_with("200") && c.contains("/status"))
        .count();
    assert_eq!((pulls_200, status_200), (1, 1), "unchanged reads are 304s");
}

/// A fresh process (a second `--timeout 0` snapshot) revalidates the stored
/// entry instead of re-reading, and both reads are recorded under the
/// `forge_wait_checks` caller.
#[test]
fn a_second_snapshot_process_revalidates_and_records_under_its_caller() {
    fn counts() -> (u64, u64) {
        crate::forge_call_stats::status_report(chrono::Utc::now(), None)
            .since_start
            .into_iter()
            .find(|r| r.caller == reads::CALLER)
            .map(|r| (r.ok, r.not_modified))
            .unwrap_or_default()
    }
    let f = Forge::new();
    f.runs(SHA, &[run("build", "completed", Some("success"))]);
    let before = counts();
    for _ in 0..2 {
        let o = go(&f, Selector::Pr(42), &opts(0), &mut FakeClock::new());
        assert_eq!(o, Outcome::Green { sha: SHA.into() });
    }
    let second: Vec<String> = f
        .calls()
        .into_iter()
        .skip(3)
        .filter(|c| !c.starts_with("OUT"))
        .collect();
    assert!(second.iter().all(|c| c.starts_with("304 inm")), "{second:#?}");
    let after = counts();
    assert!(after.0 > before.0 && after.1 > before.1, "{before:?} -> {after:?}");
}

// ---- #10351 review: no GREEN on an incomplete required set ----------------

impl Forge {
    /// Make the required-context lookup fail: the rulesets source 404s
    /// (not plan-gated), as a GraphQL-exhausted classic leg would.
    fn required_fails(&self) {
        let rules = key("repos/o/r/rules/branches/main");
        let _ = std::fs::remove_file(self.dir.path().join(format!("{rules}.out")));
    }

    fn lookups(&self) -> usize {
        self.calls()
            .iter()
            .filter(|c| c.contains("rules/branches/main"))
            .count()
    }
}

/// Finding 1: `--required-only` on a base branch that requires nothing used
/// to be a vacuous GREEN while a check was failing or pending.
#[test]
fn required_only_with_no_required_contexts_is_never_green_while_checks_fail_or_pend() {
    let f = Forge::new();
    let ok = || run("labeler", "completed", Some("success"));
    let mut o = opts(0);
    o.required_only = true;

    f.runs(SHA, &[ok(), run("ci", "completed", Some("failure"))]);
    let red = go(&f, Selector::Pr(42), &o, &mut FakeClock::new());
    assert_eq!(red.sentinel(), format!("LOOM-CHECKS-RED {SHA} ci"));

    f.runs(SHA, &[ok(), run("ci", "queued", None)]);
    let pending = go(&f, Selector::Pr(42), &o, &mut FakeClock::new());
    assert_eq!(
        pending,
        Outcome::Timeout {
            sha: SHA.into(),
            pending: vec!["ci".into()]
        }
    );

    f.runs(SHA, &[ok(), run("ci", "completed", Some("success"))]);
    let (green, notes) = wait(&mut f.reads(), &Selector::Pr(42), &o, &mut FakeClock::new());
    assert_eq!(green, Outcome::Green { sha: SHA.into() });
    assert!(
        notes
            .iter()
            .any(|n| n.contains("requires no status-check contexts")),
        "{notes:?}"
    );
}

/// Finding 2 + the not-yet-created case: a failed lookup is retried (not
/// cached), the poll it failed on does not settle, and once the set is
/// known a required check that has not been created yet is pending.
#[test]
fn a_failed_required_lookup_is_retried_and_an_uncreated_required_check_is_waited_for() {
    let f = Forge::new();
    f.required_fails();
    let ok = || run("labeler", "completed", Some("success"));
    f.runs(SHA, &[ok()]);
    let mut clock = FakeClock::new()
        .at(20, || f.required(&["Backend"]))
        .at(60, || f.runs(SHA, &[ok(), run("Backend", "completed", Some("success"))]));
    let (o, notes) = wait(&mut f.reads(), &Selector::Pr(42), &opts(1800), &mut clock);
    assert_eq!(o, Outcome::Green { sha: SHA.into() });
    // t=0 lookup fails → wait; t=30 lookup succeeds, `Backend` absent → wait;
    // t=75 `Backend` green.
    assert_eq!(f.polls(), 3);
    assert_eq!(f.lookups(), 2, "retried once, then cached: {:#?}", f.calls());
    assert!(notes.iter().any(|n| n.contains("lookup for main failed")), "{notes:?}");
}

/// Finding 2, bounded: a lookup that keeps failing ends in ERROR, never
/// GREEN; a snapshot taken while it fails is a TIMEOUT naming the unknown set.
#[test]
fn a_required_lookup_that_keeps_failing_is_error_never_green() {
    let f = Forge::new();
    f.required_fails();
    f.runs(SHA, &[run("labeler", "completed", Some("success"))]);
    let (o, notes) = wait(&mut f.reads(), &Selector::Pr(42), &opts(1800), &mut FakeClock::new());
    assert!(
        matches!(&o, Outcome::Error(w) if w.starts_with("required-lookup-failed: ")),
        "{o:?}"
    );
    assert_eq!(f.polls(), MAX_LOOKUP_FAILURES as usize);
    assert_eq!(f.lookups(), MAX_LOOKUP_FAILURES as usize);
    assert!(!notes.is_empty());

    let snap = go(&f, Selector::Pr(42), &opts(0), &mut FakeClock::new());
    assert_eq!(
        snap,
        Outcome::Timeout {
            sha: SHA.into(),
            pending: vec![verdict::REQUIRED_UNKNOWN.into()]
        }
    );
}

/// `--required-only` waits through a failed lookup (no longer an immediate
/// ERROR) and then for a required check that has not been created yet,
/// while an informational failure stays informational.
#[test]
fn required_only_waits_for_a_required_check_that_has_not_been_created() {
    let f = Forge::new();
    f.required_fails();
    let base = || {
        vec![
            run("lint", "completed", Some("failure")),
            run("labeler", "completed", Some("success")),
        ]
    };
    f.runs(SHA, &base());
    let mut clock = FakeClock::new()
        .at(20, || f.required(&["Gate"]))
        .at(60, || {
            let mut rows = base();
            rows.push(run("Gate", "completed", Some("success")));
            f.runs(SHA, &rows);
        });
    let mut o = opts(1800);
    o.required_only = true;
    assert_eq!(go(&f, Selector::Pr(42), &o, &mut clock), Outcome::Green { sha: SHA.into() });
    assert_eq!(f.polls(), 3);
}

// ---------------------------------------------------------------------------
// #10633: 403s are classified; a statuses permission refusal degrades
// ---------------------------------------------------------------------------

const NOT_ACCESSIBLE: &str =
    r#"{"message":"Resource not accessible by integration","status":"403"}"#;
const SECONDARY: &str = r#"{"message":"You have exceeded a secondary rate limit. Please wait a few minutes before you try again."}"#;

#[test]
fn a_statuses_permission_refusal_degrades_to_check_runs_only_with_a_note() {
    let f = Forge::new();
    f.runs(SHA, &[run("build", "completed", Some("success"))]);
    f.refuse(&status_url(SHA), 403, "", NOT_ACCESSIBLE);
    let (o, notes) = wait(&mut f.reads(), &Selector::Pr(42), &opts(1800), &mut FakeClock::new());
    assert_eq!(o.sentinel(), format!("LOOM-CHECKS-GREEN {SHA}"));
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains("permission (needs statuses:read)"), "{notes:?}");
    assert!(notes[0].contains("Resource not accessible by integration"), "{notes:?}");
    assert!(notes[0].contains("Commit statuses: Read"), "{notes:?}");
}

#[test]
fn a_degraded_wait_stops_asking_for_statuses_and_still_waits_for_a_required_status() {
    let f = Forge::new();
    f.runs(SHA, &[run("build", "completed", Some("success"))]);
    // `ci/jenkins` is required and reported only as a legacy status we cannot
    // read: the verdict must never be GREEN.
    f.required(&["ci/jenkins"]);
    f.refuse(&status_url(SHA), 403, "", NOT_ACCESSIBLE);
    let o = go(&f, Selector::Pr(42), &opts(600), &mut FakeClock::new());
    assert_eq!(o.sentinel(), format!("LOOM-CHECKS-TIMEOUT {SHA} ci/jenkins"));
    let status_reads = f.calls().iter().filter(|c| c.contains("/status?")).count();
    assert_eq!(status_reads, 1, "{:?}", f.calls());
}

#[test]
fn an_empty_rollup_with_unreadable_statuses_is_error_never_none() {
    let f = Forge::new();
    f.runs(SHA, &[]);
    f.refuse(&status_url(SHA), 403, "", NOT_ACCESSIBLE);
    let o = go(&f, Selector::Pr(42), &opts(1800), &mut FakeClock::new());
    match &o {
        Outcome::Error(w) => {
            assert!(w.starts_with("statuses-unreadable: HTTP 403"), "{w}");
            assert!(w.contains("permission (needs statuses:read)"), "{w}");
        }
        other => panic!("expected ERROR, got {other:?}"),
    }
}

#[test]
fn a_check_runs_permission_refusal_is_error_naming_the_permission() {
    let f = Forge::new();
    f.refuse(&runs_url(SHA), 403, "", NOT_ACCESSIBLE);
    let o = go(&f, Selector::Pr(42), &opts(1800), &mut FakeClock::new());
    match &o {
        Outcome::Error(w) => {
            assert!(w.starts_with("HTTP 403 for repos/o/r/commits/"), "{w}");
            assert!(w.contains("permission (needs checks:read)"), "{w}");
        }
        other => panic!("expected ERROR, got {other:?}"),
    }
    assert_eq!(f.polls(), 1, "a permission refusal is not retried");
}

#[test]
fn a_secondary_rate_limit_403_is_retried_not_reported_as_permission() {
    let f = Forge::new();
    f.runs(SHA, &[run("build", "completed", Some("success"))]);
    f.refuse(&status_url(SHA), 403, "Retry-After: 60\r\n", SECONDARY);
    let url = status_url(SHA);
    let mut clock = FakeClock::new().at(30, || f.unrefuse(&url));
    let (o, notes) = wait(&mut f.reads(), &Selector::Pr(42), &opts(1800), &mut clock);
    assert_eq!(o.sentinel(), format!("LOOM-CHECKS-GREEN {SHA}"));
    assert!(notes.is_empty(), "a rate limit never degrades: {notes:?}");
}

#[test]
fn a_persistent_secondary_rate_limit_ends_as_read_failed_naming_the_class() {
    let f = Forge::new();
    f.runs(SHA, &[run("build", "completed", Some("success"))]);
    f.refuse(&status_url(SHA), 403, "Retry-After: 60\r\n", SECONDARY);
    let o = go(&f, Selector::Pr(42), &opts(1800), &mut FakeClock::new());
    match &o {
        Outcome::Error(w) => {
            assert!(w.starts_with("read-failed: HTTP 403"), "{w}");
            assert!(w.contains("secondary-rate-limit"), "{w}");
            assert!(!w.contains("permission"), "{w}");
        }
        other => panic!("expected ERROR, got {other:?}"),
    }
}
