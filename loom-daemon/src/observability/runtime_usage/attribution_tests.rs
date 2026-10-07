//! `llm.billing` is copied onto usage only when the launch that billed it is
//! established (Issue #10749): an unmatched attempt never borrows another
//! attempt's launch, and execution totals over differently billed launches
//! are `unknown`, never one launch's class.

use chrono::{DateTime, Duration, Utc};

use super::*;
use crate::observability::llm_billing::LlmBilling;
use crate::telemetry::trace::{SpanStatus, TraceContext};

fn row(model: &str) -> ModelUsageTotals {
    ModelUsageTotals {
        model: model.into(),
        input: 10,
        output: 20,
        ..ModelUsageTotals::default()
    }
}

struct Trace {
    journal: Journal,
    root: TraceContext,
    t0: DateTime<Utc>,
}

impl Trace {
    fn new(root: &Path, execution: &str) -> Self {
        let store = TraceStore::new(root);
        let saved = store.load_or_create(root, execution).unwrap();
        Self {
            journal: Journal::for_context(&store.path(root, execution)),
            root: saved.context,
            t0: Utc::now() - Duration::hours(2),
        }
    }

    /// A completed span under `parent` over minutes `[from, to]` of the trace.
    fn span(
        &self,
        parent: &TraceContext,
        name: SpanName,
        key: &str,
        (from, to): (i64, i64),
        attributes: TraceAttributes,
    ) -> TraceContext {
        let active = self
            .journal
            .start(
                parent.derived_child(&[key]),
                Some(parent),
                name,
                self.t0 + Duration::minutes(from),
                attributes,
            )
            .unwrap();
        let context = active.record.context.clone();
        self.journal
            .finish(
                &active,
                self.t0 + Duration::minutes(to),
                SpanStatus::Ok,
                TraceAttributes::new(),
            )
            .unwrap();
        context
    }

    fn attempt(&self, role: &str, attempt: u32, window: (i64, i64)) -> TraceContext {
        let mut attributes = TraceAttributes::new();
        attributes.insert("loom.role".into(), role.into());
        attributes.insert("loom.attempt".into(), attempt.to_string());
        let key = format!("attempt-{role}-{attempt}");
        self.span(&self.root, SpanName::RoleAttempt, &key, window, attributes)
    }

    fn run(&self, parent: &TraceContext, key: &str, window: (i64, i64), billing: &LlmBilling) {
        let mut attributes = TraceAttributes::new();
        billing.stamp(&mut attributes);
        self.span(parent, SpanName::RuntimeRun, key, window, attributes);
    }
}

fn subscription() -> LlmBilling {
    LlmBilling::for_runtime("claude", None, false)
}

fn metered() -> LlmBilling {
    LlmBilling::native(Some("quick-cerebras"), None, true, "pool")
}

fn billing_of(spans: &[SpanRecord], role: &str) -> (String, Option<String>, Option<String>) {
    let span = spans
        .iter()
        .find(|s| s.attributes.get("loom.role").is_some_and(|r| r == role))
        .unwrap_or_else(|| panic!("no usage span for {role}: {spans:?}"));
    (
        span.attributes["llm.billing"].clone(),
        span.attributes.get("llm.credential.kind").cloned(),
        span.attributes.get("llm.provider.profile").cloned(),
    )
}

fn phase_usage<'a>(role: &'a str, attempt: u32, rows: &'a [ModelUsageTotals]) -> PhaseUsage<'a> {
    let at = Utc::now();
    PhaseUsage {
        role,
        attempt,
        window: (at, at),
        rows,
    }
}

fn unknown() -> (String, Option<String>, Option<String>) {
    ("unknown".into(), None, None)
}

#[test]
fn an_unmatched_attempt_never_borrows_another_attempts_launch() {
    let tmp = tempfile::tempdir().unwrap();
    let trace = Trace::new(tmp.path(), "sweep-mixed");
    let a = trace.attempt("curator", 1, (0, 10));
    let b = trace.attempt("builder", 1, (10, 20));
    // Attempt A ran on the subscription, B on a metered API key — and B's run
    // outlives B's window far enough to cover C's.
    trace.run(&a, "run-a", (1, 9), &subscription());
    trace.run(&b, "run-b", (11, 40), &metered());
    // C has no launch of its own: it must not inherit B's (latest) class.
    trace.attempt("judge", 1, (25, 28));
    let rows = [row("claude-sonnet-5")];
    let phases = [
        phase_usage("curator", 1, &rows),
        phase_usage("builder", 1, &rows),
        phase_usage("judge", 1, &rows),
    ];
    let spans = journal_phase_usage(tmp.path(), "sweep-mixed", &phases, None).unwrap();
    assert_eq!(
        billing_of(&spans, "curator"),
        ("subscription".into(), Some("oauth-pool".into()), None)
    );
    assert_eq!(
        billing_of(&spans, "builder"),
        ("api".into(), Some("api-key".into()), Some("quick-cerebras".into()))
    );
    assert_eq!(billing_of(&spans, "judge"), unknown());
}

#[test]
fn an_attempt_inside_one_unowned_launch_takes_that_launchs_class() {
    let tmp = tempfile::tempdir().unwrap();
    let trace = Trace::new(tmp.path(), "sweep-one");
    // One `claude -p` launch for the whole sweep; attempts are its siblings.
    trace.run(&trace.root, "run", (0, 60), &metered());
    trace.attempt("builder", 1, (5, 20));
    // An attempt outside every launch's interval stays unknown.
    trace.attempt("judge", 1, (70, 80));
    let rows = [row("glm-5.3-flash")];
    let phases = [
        phase_usage("builder", 1, &rows),
        phase_usage("judge", 1, &rows),
    ];
    let spans = journal_phase_usage(tmp.path(), "sweep-one", &phases, None).unwrap();
    assert_eq!(
        billing_of(&spans, "builder"),
        ("api".into(), Some("api-key".into()), Some("quick-cerebras".into()))
    );
    assert_eq!(billing_of(&spans, "judge"), unknown());
}

#[test]
fn an_attempt_inside_differently_billed_launches_is_unknown() {
    let tmp = tempfile::tempdir().unwrap();
    let trace = Trace::new(tmp.path(), "sweep-overlap");
    trace.run(&trace.root, "run-sub", (0, 60), &subscription());
    trace.run(&trace.root, "run-api", (0, 60), &metered());
    trace.attempt("builder", 1, (5, 20));
    let rows = [row("claude-sonnet-5")];
    let spans =
        journal_phase_usage(tmp.path(), "sweep-overlap", &[phase_usage("builder", 1, &rows)], None)
            .unwrap();
    assert_eq!(billing_of(&spans, "builder"), unknown());
}

#[test]
fn execution_totals_over_mixed_launches_are_unknown_for_every_model() {
    let tmp = tempfile::tempdir().unwrap();
    let trace = Trace::new(tmp.path(), "sweep-exec");
    let a = trace.attempt("curator", 1, (0, 10));
    let b = trace.attempt("builder", 1, (10, 20));
    trace.run(&a, "run-a", (1, 9), &subscription());
    trace.run(&b, "run-b", (11, 19), &metered());
    // The same model served both launches: its one row spans both classes.
    let rows = [row("claude-sonnet-5"), row("glm-5.3-flash")];
    let window = (trace.t0, Utc::now());
    let spans = journal_usage(tmp.path(), "sweep-exec", window, Some(&rows), None).unwrap();
    assert_eq!(spans.len(), 2, "{spans:?}");
    for span in &spans {
        assert_eq!(span.attributes["llm.billing"], "unknown", "{:?}", span.attributes);
        assert!(!span.attributes.contains_key("llm.credential.kind"));
        assert!(!span.attributes.contains_key("llm.provider.profile"));
    }
}

#[test]
fn execution_totals_keep_only_what_every_launch_shares() {
    let tmp = tempfile::tempdir().unwrap();
    let trace = Trace::new(tmp.path(), "sweep-api");
    trace.run(&trace.root, "run-1", (0, 10), &metered());
    let other = LlmBilling::native(Some("gemini-flash"), None, true, "pool");
    trace.run(&trace.root, "run-2", (10, 20), &other);
    let window = (trace.t0, Utc::now());
    let spans = journal_usage(tmp.path(), "sweep-api", window, Some(&[row("glm-5.3-flash")]), None)
        .unwrap();
    let [span] = spans.as_slice() else {
        panic!("one span: {spans:?}");
    };
    assert_eq!(span.attributes["llm.billing"], "api");
    assert_eq!(span.attributes["llm.credential.kind"], "api-key");
    assert!(!span.attributes.contains_key("llm.provider.profile"), "profiles differ");
}
