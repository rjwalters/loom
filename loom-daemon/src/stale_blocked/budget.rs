//! The budget floor and the run's forge cost for `check-stale-blocked`
//! (issue #10480).
//!
//! The batched gatherer ([`super::batch`]) is cheap, but it is still run on
//! every sweep against every repo, and the GraphQL bucket it draws on is the
//! one epic #10332 is keeping clear. So the run checks before it spends:
//!
//! - **Before gathering**: after the candidate listing (a free `304` when
//!   nothing moved), the free `/rate_limit` + GraphQL `rateLimit` probe
//!   ([`crate::rate_limit_breaker::forge::probe_budget_ctx`]) is compared with
//!   the run's projected cost ([`project`]). If the run would take either
//!   bucket below its floor, nothing is gathered: every artifact is reported
//!   *not evaluated*, and the advisory still exits 0 ([`refusal`]).
//! - **Mid-run**: each GraphQL batch's own `rateLimit.remaining` and each REST
//!   response's `x-ratelimit-remaining` feed a [`Meter`]; before every further
//!   read a [`Guard`] checks the latest reading against the floor, and stops.
//!   What it did not read is reported not evaluated, never clear.
//!
//! A probe that does not answer is not a refusal: refusing on a failed *free*
//! read would make the advisory silently vanish. The run proceeds and reports
//! `budget_before: null`.
//!
//! A floor of `0` disables that bucket's check.

use serde::Serialize;

use super::batch::CLOSING_BATCH;
use super::Artifact;
use crate::forge_listing::RestIssue;

/// Default `--min-graphql-remaining`.
pub const DEFAULT_MIN_GRAPHQL_REMAINING: u64 = 1_000;

/// Default `--min-core-remaining`.
pub const DEFAULT_MIN_CORE_REMAINING: u64 = 1_000;

/// Comments per REST page, as [`super::batch`] reads them.
const COMMENT_PAGE: u64 = 100;

/// The remaining-points floor for each bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Floor {
    pub graphql: u64,
    pub core: u64,
}

impl Default for Floor {
    fn default() -> Self {
        Self {
            graphql: DEFAULT_MIN_GRAPHQL_REMAINING,
            core: DEFAULT_MIN_CORE_REMAINING,
        }
    }
}

/// The live budget, as the free probe read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Budget {
    pub core_remaining: u64,
    pub graphql_remaining: u64,
}

/// What this run has spent so far, and the latest remaining figures the
/// forge reported with its answers.
///
/// The candidate listing is not counted here: it goes through
/// [`crate::forge_listing`]'s own walk (and is recorded in `forge_call_stats`
/// like every other read).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Meter {
    /// Closing-reference GraphQL queries issued.
    pub graphql_queries: u64,
    /// Their summed `rateLimit.cost` (a query whose cost was not reported
    /// counts 1, GraphQL's minimum).
    pub graphql_points: u64,
    /// `rateLimit.remaining` from the latest query that reported it.
    pub graphql_remaining: Option<u64>,
    /// REST reads answered (comments pages, blocker states, PR merge states).
    pub rest_requests: u64,
    /// How many of those were `304 Not Modified` — free on the core bucket.
    pub rest_not_modified: u64,
    /// `x-ratelimit-remaining` from the latest core-pool REST answer.
    pub core_remaining: Option<u64>,
}

impl Meter {
    /// Record one GraphQL query.
    pub fn graphql(&mut self, cost: Option<u64>, remaining: Option<u64>) {
        self.graphql_queries += 1;
        self.graphql_points += cost.unwrap_or(1);
        if remaining.is_some() {
            self.graphql_remaining = remaining;
        }
    }

    /// Record one answered REST read.
    pub fn rest(&mut self, not_modified: bool, core_remaining: Option<u64>) {
        self.rest_requests += 1;
        if not_modified {
            self.rest_not_modified += 1;
        }
        if core_remaining.is_some() {
            self.core_remaining = core_remaining;
        }
    }
}

/// The run's projected cost, an upper bound.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Projection {
    /// ⌈issues / [`CLOSING_BATCH`]⌉ closing-reference queries.
    pub graphql: u64,
    /// Comment pages, plus `2·N` for blocker states and PR merge states (a
    /// conservative bound on distinct references).
    pub core: u64,
}

/// Project the cost of gathering `selected`.
#[must_use]
pub fn project(selected: &[(Artifact, RestIssue)]) -> Projection {
    let issues = selected
        .iter()
        .filter(|(k, _)| *k == Artifact::Issue)
        .count() as u64;
    let comment_pages: u64 = selected
        .iter()
        .map(|(_, r)| u64::from(r.comments).div_ceil(COMMENT_PAGE))
        .sum();
    Projection {
        graphql: issues.div_ceil(CLOSING_BATCH as u64),
        core: comment_pages + 2 * selected.len() as u64,
    }
}

/// Why the run must not start, or `None` when it fits above both floors. A
/// bucket the run does not draw on (projected `0`) is never a reason.
#[must_use]
pub fn refusal(budget: &Budget, projected: Projection, floor: Floor) -> Option<String> {
    let check = |name: &str, remaining: u64, cost: u64, min: u64| {
        (cost > 0 && remaining.saturating_sub(cost) < min).then(|| {
            format!(
                "budget floor: {name} remaining {remaining}, projected {cost}, floor {min} — \
                 not evaluated this run (advisory; exit 0)"
            )
        })
    };
    check("graphql", budget.graphql_remaining, projected.graphql, floor.graphql)
        .or_else(|| check("core", budget.core_remaining, projected.core, floor.core))
}

/// Mid-run floor checks over the [`Meter`]'s latest readings. The first stop
/// is kept for the report; every later check keeps refusing, because the
/// meter cannot rise again without a read.
#[derive(Debug)]
pub struct Guard {
    floor: Floor,
    stopped: Option<String>,
}

impl Guard {
    #[must_use]
    pub fn new(floor: Floor) -> Self {
        Self {
            floor,
            stopped: None,
        }
    }

    /// May another core (REST) read be made?
    ///
    /// # Errors
    /// The latest core reading is below the floor; the reason to report.
    pub fn core(&mut self, meter: &Meter) -> Result<(), String> {
        self.check("core", meter.core_remaining, self.floor.core)
    }

    /// May another GraphQL query be made?
    ///
    /// # Errors
    /// The latest GraphQL reading is below the floor; the reason to report.
    pub fn graphql(&mut self, meter: &Meter) -> Result<(), String> {
        self.check("graphql", meter.graphql_remaining, self.floor.graphql)
    }

    fn check(&mut self, name: &str, remaining: Option<u64>, min: u64) -> Result<(), String> {
        match remaining {
            Some(r) if r < min => {
                let why = format!(
                    "budget floor reached mid-run: {name} remaining {r} < floor {min} — not \
                     evaluated (advisory; exit 0)"
                );
                self.stopped.get_or_insert_with(|| why.clone());
                Err(why)
            }
            _ => Ok(()),
        }
    }

    /// The first mid-run stop, if any.
    #[must_use]
    pub fn stopped(self) -> Option<String> {
        self.stopped
    }
}

/// The `--json` `forge_cost` object (and the stderr cost line).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ForgeCost {
    #[serde(flatten)]
    pub meter: Meter,
    /// The probe's reading before gathering; `None` when it did not answer
    /// (or nothing needed gathering).
    pub budget_before: Option<Budget>,
    pub projected: Projection,
    pub floor: Floor,
    /// Why nothing was gathered (pre-run floor, or the breaker).
    pub budget_refused: Option<String>,
    /// Why gathering stopped part-way.
    pub budget_stopped: Option<String>,
}

impl ForgeCost {
    /// One human line with the same numbers as the JSON.
    #[must_use]
    pub fn summary(&self) -> String {
        let opt = |v: Option<u64>| v.map_or_else(|| "?".to_string(), |n| n.to_string());
        format!(
            "[stale-blocked] forge cost: graphql {} quer{} / {} point(s) (remaining {}); REST {} \
             request(s), {} not modified (remaining {})",
            self.meter.graphql_queries,
            if self.meter.graphql_queries == 1 {
                "y"
            } else {
                "ies"
            },
            self.meter.graphql_points,
            opt(self.meter.graphql_remaining),
            self.meter.rest_requests,
            self.meter.rest_not_modified,
            opt(self.meter.core_remaining),
        )
    }
}
