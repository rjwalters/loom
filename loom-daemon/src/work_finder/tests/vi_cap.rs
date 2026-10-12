//! The `loom:very-important` flood cap (#11509) through the real ticks:
//! demotion changes the dispatch order and the draw pool, and is recorded on
//! the report that feeds `pick.decision`.

use super::*;
use crate::priority_pick::VERY_IMPORTANT_LABEL;
use crate::work_finder::vi_cap::ViDemotion;

fn issue(n: u32, created_at: &str, extra: &[&str]) -> WorkItem {
    let mut labels = vec!["loom:issue".to_string()];
    labels.extend(extra.iter().map(|l| (*l).to_string()));
    WorkItem::with_created_at(n, labels, Some(created_at.to_string()))
}

/// A source that also lists the repo's open very-important issues.
struct ViSource {
    ready: FakeSource,
    open_vi: Option<Vec<WorkItem>>,
    vi_reads: usize,
}

impl WorkSource for ViSource {
    fn list_ready_issues(&mut self) -> Result<Vec<WorkItem>> {
        self.ready.list_ready_issues()
    }

    fn list_open_very_important(&mut self) -> Option<Vec<WorkItem>> {
        self.vi_reads += 1;
        self.open_vi.clone()
    }
}

/// Single-workspace tick: of three very-important issues only the oldest
/// keeps the level, so plain-but-older work dispatches before the two
/// demoted ones only by age, never behind them by level.
#[test]
fn the_single_workspace_tick_dispatches_the_cap_order() {
    let vi = VERY_IMPORTANT_LABEL;
    let mut src = FakeSource::once(vec![
        issue(30, "2026-03-01T00:00:00Z", &[vi]),
        issue(10, "2026-01-01T00:00:00Z", &[]),
        issue(20, "2026-02-01T00:00:00Z", &[vi]),
        issue(40, "2026-04-01T00:00:00Z", &[vi]),
        issue(50, "2026-05-01T00:00:00Z", &["loom:important"]),
    ]);
    let mut dispatcher = RecordingDispatcher::default();
    let report = tick(&mut src, &mut dispatcher, 10, false).unwrap();
    // 20 (VI holder), then important by age: 30, 40, 50, then plain 10.
    assert_eq!(dispatcher.dispatched, vec![20, 30, 40, 50, 10]);
    let d = |number| ViDemotion {
        workspace_idx: 0,
        number,
        holder: 20,
    };
    assert_eq!(report.vi_demotions, vec![d(30), d(40)]);
}

/// Multi-workspace tick: a workspace whose only very-important ready issue
/// is demoted (an older one is being built) no longer wins the draw pool.
#[test]
fn a_demoted_workspace_does_not_win_the_very_important_draw() {
    let building = WorkItem::with_created_at(
        1,
        vec!["loom:building".into(), VERY_IMPORTANT_LABEL.into()],
        Some("2025-01-01T00:00:00Z".into()),
    );
    let mut multi = vec![
        (
            ViSource {
                ready: FakeSource::once(vec![issue(
                    5,
                    "2026-01-01T00:00:00Z",
                    &[VERY_IMPORTANT_LABEL],
                )]),
                open_vi: Some(vec![building]),
                vi_reads: 0,
            },
            RecordingDispatcher::default(),
        ),
        (
            ViSource {
                ready: FakeSource::once(vec![issue(7, "2026-01-01T00:00:00Z", &[])]),
                open_vi: Some(Vec::new()),
                vi_reads: 0,
            },
            RecordingDispatcher::default(),
        ),
    ];
    let report = tick_multi(&mut multi, &[100, 100], 1, &[false, false]);
    let log = report.workspace_draw.as_ref().unwrap();
    assert_eq!(log.steps[0].draw.pool, "all", "no very-important work is left after the cap");
    assert_eq!(
        report.vi_demotions,
        vec![ViDemotion {
            workspace_idx: 0,
            number: 5,
            holder: 1,
        }]
    );
    // The open-VI listing is read only where the ready listing has VI work.
    assert_eq!((multi[0].0.vi_reads, multi[1].0.vi_reads), (1, 0));
}

/// The cap is per workspace: one very-important issue in each of two
/// workspaces keeps both, and nothing is demoted.
#[test]
fn the_cap_is_per_workspace() {
    let vi = |n| issue(n, "2026-01-01T00:00:00Z", &[VERY_IMPORTANT_LABEL]);
    let mut multi = vec![
        (FakeSource::once(vec![vi(1)]), RecordingDispatcher::default()),
        (FakeSource::once(vec![vi(2)]), RecordingDispatcher::default()),
    ];
    let report = tick_multi(&mut multi, &[100, 100], 4, &[false, false]);
    assert!(report.vi_demotions.is_empty());
    assert_eq!(report.dispatched, 2);
}
