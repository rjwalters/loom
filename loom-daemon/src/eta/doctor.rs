//! `loom-daemon eta doctor` (#10391): the pure half. The CLI
//! (`cli/eta_doctor_cmd.rs`) gathers [`Facts`] from this host, read-only, and
//! [`evaluate`] turns them into one verdict per check, walking the pipeline
//! in order: `config` -> `data` -> `fit` -> `serving` -> `snapshot_feed` ->
//! `outcomes`. Every WARN and FAIL carries the exact remedy.
//!
//! No I/O here, so every verdict is table-testable: a wrong remedy or a wrong
//! classification passes the type checker and misleads an operator, which is
//! why the cases are enumerated in the tests.

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use std::collections::BTreeMap;

use crate::eta::fit::publish::{FetchKind, PubStatus};
use crate::eta::fit::run;
use crate::eta::health::RefreshCycleState;
use crate::eta::regime::DriftState;
use crate::telemetry::kinds::eta_fit::EtaFitRecord;

/// A snapshot older than this is a FAIL.
pub const SNAPSHOT_FAIL_HOURS: i64 = 26;
/// A coefficient file whose cutoff is older than this is a FAIL (the loom-ui
/// "no fit written" alert threshold).
pub const FIT_FAIL_HOURS: i64 = 36;
/// The fit loop checks hourly; a record older than this means it is not.
pub const FIT_CHECK_STALE_HOURS: i64 = 3;

/// One verdict's status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Status {
    /// Healthy.
    Ok,
    /// Degraded, or about to be.
    Warn,
    /// Broken.
    Fail,
    /// Not evaluable here.
    Skip,
}

impl Status {
    /// `OK`, `WARN`, `FAIL` or `SKIP`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "OK",
            Status::Warn => "WARN",
            Status::Fail => "FAIL",
            Status::Skip => "SKIP",
        }
    }
}

/// One check's verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Check {
    /// The chain link: `config`, `data`, `fit`, `serving`, `snapshot_feed`
    /// or `outcomes`.
    pub link: &'static str,
    /// The check within the link.
    pub check: String,
    /// The verdict.
    pub status: Status,
    /// What was found.
    pub detail: String,
    /// What to do; present on WARN and FAIL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remedy: Option<String>,
}

impl Check {
    fn new(link: &'static str, check: &str, status: Status, detail: String) -> Self {
        Check {
            link,
            check: check.to_string(),
            status,
            detail,
            remedy: None,
        }
    }

    fn ok(link: &'static str, check: &str, detail: impl Into<String>) -> Self {
        Self::new(link, check, Status::Ok, detail.into())
    }

    fn skip(link: &'static str, check: &str, detail: impl Into<String>) -> Self {
        Self::new(link, check, Status::Skip, detail.into())
    }

    fn bad(
        link: &'static str,
        check: &str,
        status: Status,
        detail: impl Into<String>,
        remedy: impl Into<String>,
    ) -> Self {
        let mut c = Self::new(link, check, status, detail.into());
        c.remedy = Some(remedy.into());
        c
    }

    /// `STATUS link.check: detail`, then an indented `remedy:` line.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out =
            format!("{} {}.{}: {}", self.status.as_str(), self.link, self.check, self.detail);
        if let Some(remedy) = &self.remedy {
            out.push_str("\n    remedy: ");
            out.push_str(remedy);
        }
        out
    }
}

/// The refresh gate as the doctor sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// This host is the declared fleet captain.
    Captain,
    /// No `fleet.captain` declared.
    NoCaptain,
    /// Another host is the captain.
    StandDown {
        /// The captain's host id.
        captain: String,
    },
}

/// `config` link inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigFacts {
    /// `autonomous.eta.enabled`.
    pub eta_enabled: bool,
    /// `autonomous.eta.fit.enabled`.
    pub fit_enabled: bool,
    /// `autonomous.eta.fleetRefresh.enabled`.
    pub fleet_refresh_enabled: bool,
    /// `fleetRefresh.intervalSecs`.
    pub interval_secs: u64,
    /// An OTLP exporter is configured (`eta.fit` / `eta.estimate` are
    /// OTLP-only).
    pub otlp_exporter: bool,
    /// The native HTTPS exporter is configured (`eta.snapshot` is native-only).
    pub native_exporter: bool,
    /// The fleet's ETA authority as this host resolves it (#10498).
    pub authority: AuthorityFacts,
}

/// The ETA authority resolution as the doctor sees it (#10498).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorityFacts {
    /// The authority host id, `None` when no host qualifies.
    pub host: Option<String>,
    /// Why (`explicit`, `fleet_refresh`, `lowest_id_fallback`, `no_candidate`).
    pub reason: String,
    /// This host is the authority.
    pub is_local: bool,
    /// Other qualifying hosts: a conflict when non-empty.
    pub others: Vec<String>,
    /// The resolver's one-line explanation.
    pub detail: String,
}

/// One repo in the `data` link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoFacts {
    /// `owner/repo`.
    pub repo: String,
    /// A reader App resolves for it.
    pub has_reader: bool,
    /// Its forge has no reader-App path (not github.com), so `has_reader`
    /// is false for a reason a reader App cannot fix.
    pub unsupported_forge: bool,
    /// Its published snapshot's `as_of`.
    pub snapshot_as_of: Option<DateTime<Utc>>,
    /// An in-progress backfill's start.
    pub backfill_since: Option<DateTime<Utc>>,
}

/// `data` link inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataFacts {
    /// The captain gate, resolved read-only.
    pub gate: Gate,
    /// Every repo this host would refresh, and every repo with a snapshot.
    pub repos: Vec<RepoFacts>,
    /// The last refresh tick (`refresh-cycle.json`).
    pub refresh_cycle: Option<RefreshCycleState>,
}

/// `fit` link inputs.
#[derive(Debug, Clone, PartialEq)]
pub struct FitFacts {
    /// The newest coefficient file: `(id, cutoff)`.
    pub latest: Option<(String, DateTime<Utc>)>,
    /// Today's file exists.
    pub today_exists: bool,
    /// The last `eta.fit` record (`fit-check.json`).
    pub last_check: Option<EtaFitRecord>,
    /// The captain-published fit state (`fit-pub/status.json`, #10395);
    /// default when the file is absent.
    pub published: PubStatus,
}

/// One heuristic's pending-estimate tally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeuristicTally {
    /// `start`, `finish` or `land`.
    pub kind: String,
    /// The heuristic id.
    pub heuristic: String,
    /// Whether it is the kind's `current` heuristic.
    pub current: bool,
    /// Pending items it answered.
    pub answered: u64,
    /// Pending items it refused, per `no_estimate_reason`.
    pub refused: BTreeMap<String, u64>,
}

/// `serving` link inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServingFacts {
    /// A coefficient file is loaded into the registry.
    pub fit_loaded: bool,
    /// Registered heuristics that are shadows, as `kind:id`.
    pub shadows: Vec<String>,
    /// Per `(kind, heuristic)`.
    pub tallies: Vec<HeuristicTally>,
}

/// One shadow comparison's paired count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairFacts {
    /// `kind|current|candidate`.
    pub key: String,
    /// Paired observations.
    pub pairs: u64,
}

/// `outcomes` link inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutcomeFacts {
    /// The newest calibration row's `as_of`.
    pub calibration_newest: Option<DateTime<Utc>>,
    /// Paired counts per comparison.
    pub pairs: Vec<PairFacts>,
    /// The oldest pending estimate's `as_of`.
    pub oldest_pending: Option<DateTime<Utc>>,
    /// Pending estimates.
    pub pending: u64,
    /// Per-stage drift verdicts over the last 6 h of scored outcomes
    /// (#10528), one per stage that has any recent outcome.
    pub drift: Vec<DriftFacts>,
}

/// One stage's drift verdict ([`super::regime::drift`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriftFacts {
    /// The stage's wire name.
    pub stage: String,
    /// The one heuristic whose scored outcomes were checked (the serving
    /// `land` heuristic when the calibration log records it, else the
    /// calibration base); heuristics are never pooled.
    pub heuristic: String,
    /// Scored outcomes in the last 6 h.
    pub n_recent: u64,
    /// The tri-state verdict: `Unknown` below the sample floor.
    pub state: DriftState,
}

/// Everything the doctor reads.
#[derive(Debug, Clone, PartialEq)]
pub struct Facts {
    /// Now.
    pub now: DateTime<Utc>,
    /// `config`.
    pub config: ConfigFacts,
    /// `data`.
    pub data: DataFacts,
    /// `fit`.
    pub fit: FitFacts,
    /// `serving`.
    pub serving: ServingFacts,
    /// `outcomes`.
    pub outcomes: OutcomeFacts,
}

/// Whether any check failed (the CLI's exit code 1).
#[must_use]
pub fn has_fail(checks: &[Check]) -> bool {
    checks.iter().any(|c| c.status == Status::Fail)
}

/// Every verdict, in chain order.
#[must_use]
pub fn evaluate(facts: &Facts) -> Vec<Check> {
    let mut out = config(facts);
    out.extend(data(facts));
    out.extend(fit(facts));
    out.extend(serving(facts));
    out.extend(snapshot_feed(facts));
    out.extend(outcomes(facts));
    out
}

fn age(now: DateTime<Utc>, then: DateTime<Utc>) -> String {
    let d = now - then;
    if d < Duration::hours(1) {
        format!("{}m", d.num_minutes().max(0))
    } else if d < Duration::hours(48) {
        format!("{}h", d.num_hours())
    } else {
        format!("{}d", d.num_days())
    }
}

fn config(f: &Facts) -> Vec<Check> {
    let c = &f.config;
    let mut out = Vec::new();
    let switch = |name: &str, on: bool, key: &str, env: &str, fail: bool| {
        if on {
            Check::ok("config", name, format!("{key} is on"))
        } else {
            Check::bad(
                "config",
                name,
                if fail { Status::Fail } else { Status::Warn },
                format!("{key} is off"),
                format!(
                    "set {key}=true in .loom/config.json, or {env}=1 in the daemon's environment"
                ),
            )
        }
    };
    out.push(switch("eta", c.eta_enabled, "autonomous.eta.enabled", "LOOM_ETA_ENABLED", true));
    out.push(switch(
        "fit",
        c.fit_enabled,
        "autonomous.eta.fit.enabled",
        "LOOM_ETA_FIT_ENABLED",
        c.eta_enabled,
    ));
    out.push(switch(
        "fleet_refresh",
        c.fleet_refresh_enabled,
        "autonomous.eta.fleetRefresh.enabled",
        "LOOM_ETA_FLEET_REFRESH_ENABLED",
        false,
    ));
    out.push(authority(&c.authority));
    out.push(if c.otlp_exporter {
        Check::ok("config", "otlp_exporter", "an OTLP exporter is configured")
    } else {
        Check::bad(
            "config",
            "otlp_exporter",
            Status::Warn,
            "no OTLP exporter: eta.estimate, eta.outcome, eta.fleet_refresh and eta.fit are OTLP-only and go nowhere",
            "add `otlp` to observability.exporters in .loom/config.json (or LOOM_OBSERVABILITY_EXPORTER=otlp)",
        )
    });
    out.push(if c.native_exporter {
        Check::ok("config", "native_exporter", "the native HTTPS exporter is configured")
    } else {
        Check::bad(
            "config",
            "native_exporter",
            Status::Warn,
            "no native HTTPS exporter: eta.snapshot (the dashboard feed) is native-only and is not sent",
            "add `https` to observability.exporters in .loom/config.json",
        )
    });
    out
}

/// #10498: which host is the fleet's one ETA authority, and why.
fn authority(a: &AuthorityFacts) -> Check {
    if a.host.is_none() || !a.others.is_empty() {
        return Check::bad(
            "config",
            "authority",
            Status::Warn,
            format!("ETA authority: {}", a.detail),
            "set fleet.etaAuthority (or LOOM_ETA_AUTHORITY) to the one host that should emit \
             eta.* records",
        );
    }
    Check::ok("config", "authority", format!("ETA authority: {}", a.detail))
}

fn data(f: &Facts) -> Vec<Check> {
    let (c, d) = (&f.config, &f.data);
    let mut out = Vec::new();
    let with_snapshots = d
        .repos
        .iter()
        .filter(|r| r.snapshot_as_of.is_some())
        .count();
    let standing_down = matches!(d.gate, Gate::StandDown { .. });
    let refreshes = c.fleet_refresh_enabled && !standing_down;
    out.push(match &d.gate {
        _ if !c.fleet_refresh_enabled => Check::skip(
            "data",
            "captain_gate",
            "fleet refresh is off: snapshots are only as fresh as the last manual `eta fleet backfill|refresh`",
        ),
        Gate::Captain => Check::ok("data", "captain_gate", "this host is the fleet captain: it refreshes for every host"),
        Gate::NoCaptain => Check::ok(
            "data",
            "captain_gate",
            "no fleet.captain declared: every host with a reader refreshes (on a multi-host fleet, declare one so only one host spends the reader budgets)",
        ),
        Gate::StandDown { captain } if with_snapshots == 0 => Check::bad(
            "data",
            "captain_gate",
            Status::Fail,
            format!("standing down for captain {captain}, and this host has 0 snapshots to fit on"),
            format!(
                "share {captain}'s snapshot directory here: point LOOM_ETA_FLEET_SNAPSHOT_DIR at it (the captain's .loom/state/eta/fleet)"
            ),
        ),
        Gate::StandDown { captain } => Check::ok(
            "data",
            "captain_gate",
            format!("standing down for captain {captain}; fitting on {with_snapshots} snapshot(s)"),
        ),
    });
    for r in &d.repos {
        let name = format!("repo {}", r.repo);
        if refreshes && !r.has_reader && r.unsupported_forge {
            out.push(Check::bad(
                "data",
                &name,
                Status::Warn,
                "unsupported_forge: this repo's forge is not github.com, so the fleet refresh does not read it",
                "nothing to install: the ETA fleet refresh reads github.com repos only; drop the repo from this host's set if its ETA matters",
            ));
            continue;
        }
        if refreshes && !r.has_reader {
            out.push(Check::bad(
                "data",
                &name,
                Status::Fail,
                "no_reader: no reader App resolves for this repo, so it is never refreshed here",
                "install a reader App for the repo (the fleet reader-App provisioning step), or declare `fleet.captain` on a host that has readers and share its snapshot dir via LOOM_ETA_FLEET_SNAPSHOT_DIR",
            ));
            continue;
        }
        out.push(match r.snapshot_as_of {
            None => Check::bad(
                "data",
                &name,
                if refreshes { Status::Fail } else { Status::Warn },
                "no snapshot",
                if refreshes {
                    "run `loom-daemon eta fleet backfill --repo OWNER/NAME`, or let the refresh loop backfill it"
                } else if standing_down {
                    "share the captain's snapshot dir via LOOM_ETA_FLEET_SNAPSHOT_DIR"
                } else {
                    "enable autonomous.eta.fleetRefresh.enabled, or run `loom-daemon eta fleet backfill --repo OWNER/NAME`"
                },
            ),
            Some(as_of) => {
                let age_text = age(f.now, as_of);
                let backfill = r
                    .backfill_since
                    .map(|b| format!("; backfill in progress since {}", b.to_rfc3339()))
                    .unwrap_or_default();
                let interval = i64::try_from(c.interval_secs).unwrap_or(i64::MAX / 4);
                if f.now - as_of > Duration::hours(SNAPSHOT_FAIL_HOURS) {
                    Check::bad(
                        "data",
                        &name,
                        Status::Fail,
                        format!("snapshot is {age_text} old{backfill}"),
                        "check the refresh loop below; on a stand-down host the captain's snapshot dir must be shared (LOOM_ETA_FLEET_SNAPSHOT_DIR)",
                    )
                } else if f.now - as_of > Duration::seconds(interval.saturating_mul(2)) {
                    Check::bad(
                        "data",
                        &name,
                        Status::Warn,
                        format!("snapshot is {age_text} old (over 2 x intervalSecs){backfill}"),
                        "the refresh loop is behind: see `data.refresh_loop`",
                    )
                } else {
                    Check::ok("data", &name, format!("snapshot as of {} ({age_text} old){backfill}", as_of.to_rfc3339()))
                }
            }
        });
    }
    if d.repos.is_empty() {
        out.push(Check::bad(
            "data",
            "repos",
            Status::Fail,
            "no repo to refresh and no snapshot",
            "register a workspace root with an `origin` remote, or run `loom-daemon eta fleet backfill --repo OWNER/NAME`",
        ));
    }
    out.push(refresh_loop(f));
    out
}

fn refresh_loop(f: &Facts) -> Check {
    let (c, d) = (&f.config, &f.data);
    if !c.fleet_refresh_enabled || !c.eta_enabled {
        return Check::skip("data", "refresh_loop", "fleet refresh is off");
    }
    let Some(cycle) = &d.refresh_cycle else {
        return Check::bad(
            "data",
            "refresh_loop",
            Status::Warn,
            "no refresh-cycle.json: the refresh loop has not ticked on this host",
            "start the daemon (the first tick runs 2 minutes after start); if it is running, the refresh loop is not",
        );
    };
    let interval = i64::try_from(c.interval_secs).unwrap_or(3600);
    let since = age(f.now, cycle.started_at);
    if f.now - cycle.started_at > Duration::seconds(interval.saturating_mul(3)) {
        return Check::bad(
            "data",
            "refresh_loop",
            Status::Fail,
            format!("refresh loop not running: last tick {since} ago (gate {})", cycle.gate),
            "check `loom-daemon` is running and its log for `eta fleet refresh: cycle panicked`; a daemon restart re-arms the loop",
        );
    }
    let reasons: Vec<String> = cycle
        .stop_reasons
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    Check::ok(
        "data",
        "refresh_loop",
        format!(
            "last tick {since} ago, gate {}{}{}",
            cycle.gate,
            cycle
                .captain
                .as_ref()
                .map(|c| format!(" (captain {c})"))
                .unwrap_or_default(),
            if reasons.is_empty() {
                String::new()
            } else {
                format!(", stop reasons {}", reasons.join(" "))
            },
        ),
    )
}

fn fit(f: &Facts) -> Vec<Check> {
    let x = &f.fit;
    let mut out = Vec::new();
    out.push(match &x.latest {
        None => Check::bad(
            "fit",
            "coefficient_file",
            Status::Fail,
            "no coefficient file: twin-otter refuses no_model",
            fit_remedy(x.last_check.as_ref()),
        ),
        Some((id, cutoff)) if f.now - *cutoff > Duration::hours(FIT_FAIL_HOURS) => Check::bad(
            "fit",
            "coefficient_file",
            Status::Fail,
            format!(
                "newest fit {id} has cutoff {} ({} old, over {FIT_FAIL_HOURS}h)",
                cutoff.to_rfc3339(),
                age(f.now, *cutoff)
            ),
            fit_remedy(x.last_check.as_ref()),
        ),
        Some((id, cutoff)) => Check::ok(
            "fit",
            "coefficient_file",
            format!(
                "newest fit {id}, cutoff {} ({} old)",
                cutoff.to_rfc3339(),
                age(f.now, *cutoff)
            ),
        ),
    });
    out.push(match &x.last_check {
        None => Check::bad(
            "fit",
            "last_check",
            Status::Warn,
            "no eta.fit record yet (fit-check.json absent)",
            "the first check runs 2-10 minutes after the daemon starts; if it has been longer, see `config.fit` and `data.refresh_loop`",
        ),
        Some(r) => last_check(f.now, r),
    });
    out.push(published_fit(f.now, &x.published));
    let snapshot_dates: Vec<DateTime<Utc>> = f
        .data
        .repos
        .iter()
        .filter_map(|r| r.snapshot_as_of)
        .collect();
    out.push(match run::due(f.now, x.today_exists, &snapshot_dates) {
        Some(t) => {
            Check::ok("fit", "due_now", format!("a fit is due now (cutoff {})", t.to_rfc3339()))
        }
        None if x.today_exists => Check::ok("fit", "due_now", "not due: today's file exists"),
        None if snapshot_dates.is_empty() => {
            Check::skip("fit", "due_now", "no snapshots, so nothing to fit")
        }
        None => Check::ok(
            "fit",
            "due_now",
            format!(
                "waiting for fresh snapshots until {:02}:00Z (then it fits on whatever is there)",
                run::STALE_GRACE_HOURS
            ),
        ),
    });
    out
}

/// The captain-published fit (#10395). Informational unless the last fetch or
/// publish failed: an absent or stale publication just means this host uses
/// its own fit, or refuses `no_model`, exactly as before publication existed.
fn published_fit(now: DateTime<Utc>, p: &PubStatus) -> Check {
    let publish_failed = p.publish_error.as_deref();
    let Some(kind) = p.kind else {
        return match publish_failed {
            Some(e) => Check::bad(
                "fit",
                "published_fit",
                Status::Warn,
                format!("captain publish failing: {e}"),
                "check `fleet.repo`, `fleet.etaFitRef` and the captain's write credential",
            ),
            None => Check::skip(
                "fit",
                "published_fit",
                "no published fit: this host uses its own fit, or refuses no_model",
            ),
        };
    };
    let what = match (&p.fit_id, &p.captain_host) {
        (Some(id), Some(c)) => format!("fit {id} from captain {c}"),
        (Some(id), None) => format!("fit {id}"),
        _ => "no fit".to_string(),
    };
    let when = p
        .published_at
        .map_or(String::new(), |t| format!(", published {} ago", age(now, t)));
    let reason = p
        .reason
        .as_deref()
        .map_or(String::new(), |r| format!(" ({r})"));
    let tail = publish_failed.map_or(String::new(), |e| format!("; publish error: {e}"));
    let detail = format!("last fetch {kind:?}{reason}: {what}{when}{tail}");
    match kind {
        FetchKind::Installed | FetchKind::Current | FetchKind::NotModified
            if publish_failed.is_none() =>
        {
            Check::ok("fit", "published_fit", detail)
        }
        FetchKind::Absent => Check::skip(
            "fit",
            "published_fit",
            format!("{detail}: nothing published, this host uses its own fit or refuses no_model"),
        ),
        FetchKind::Stale => Check::bad(
            "fit",
            "published_fit",
            Status::Warn,
            format!("{detail}: stale publication ignored, this host uses its own fit or refuses no_model"),
            "the captain has stopped publishing; see the captain's `eta doctor` (fit.last_check, fit.published_fit)",
        ),
        _ => Check::bad(
            "fit",
            "published_fit",
            Status::Warn,
            detail,
            "the previous fit stays in service; check `fleet.repo`/`fleet.etaFitRef` and the refusal code",
        ),
    }
}

fn fit_remedy(last: Option<&EtaFitRecord>) -> String {
    match last.map(|r| (r.outcome.as_str(), r.skip_reason.as_deref())) {
        Some(("skipped", Some("no_snapshots"))) => {
            "no snapshots to fit on: see the `data` link".into()
        }
        Some(("skipped", Some("disabled"))) => {
            "set autonomous.eta.fit.enabled=true (LOOM_ETA_FIT_ENABLED=1)".into()
        }
        Some(("skipped", Some("held"))) => {
            "a backfill is in progress; the hold lifts within 6 hours of its start".into()
        }
        Some(("skipped", Some("stale_before_grace"))) => {
            "snapshots are stale: see the `data` link; the fit runs anyway after 06:00Z".into()
        }
        Some(("error" | "panic", _)) => {
            "see `fit.last_check`, then run `loom-daemon eta fit --dry-run`".into()
        }
        _ => "run `loom-daemon eta fit --dry-run` to see why, and check the `data` link".into(),
    }
}

fn last_check(now: DateTime<Utc>, r: &EtaFitRecord) -> Check {
    let when = format!("{} ago", age(now, r.started_at));
    let stale = now - r.started_at > Duration::hours(FIT_CHECK_STALE_HOURS);
    let reason = r.skip_reason.as_deref().unwrap_or("");
    let mut check = match (r.outcome.as_str(), reason) {
        ("written", _) => Check::ok(
            "fit",
            "last_check",
            format!("wrote fit {} {when} ({} rows)", r.fit_id.as_deref().unwrap_or("?"), r.rows_total.unwrap_or(0)),
        ),
        ("skipped", "today_exists") => Check::ok("fit", "last_check", format!("today's fit exists ({when})")),
        ("skipped", "no_snapshots") => Check::bad(
            "fit",
            "last_check",
            Status::Fail,
            format!("skipped: no_snapshots ({when})"),
            "no snapshots to fit on: see the `data` link (no_reader, or a stand-down host without LOOM_ETA_FLEET_SNAPSHOT_DIR)",
        ),
        ("skipped", "stale_before_grace") => Check::bad(
            "fit",
            "last_check",
            Status::Warn,
            format!("skipped: stale_before_grace ({when}); waiting for fresh snapshots until 06:00Z"),
            "snapshots have not been refreshed since the cutoff: see the `data` link",
        ),
        ("skipped", "held") => Check::bad(
            "fit",
            "last_check",
            Status::Warn,
            format!("skipped: held ({when}); a backfill is in progress"),
            "wait: the hold lifts within 6 hours of the backfill's start",
        ),
        ("skipped", "disabled") => Check::bad(
            "fit",
            "last_check",
            Status::Fail,
            format!("skipped: disabled ({when})"),
            "set autonomous.eta.fit.enabled=true (LOOM_ETA_FIT_ENABLED=1)",
        ),
        ("error", _) => Check::bad(
            "fit",
            "last_check",
            Status::Fail,
            format!("error ({when}): {}", r.error.as_deref().unwrap_or("?")),
            "run `loom-daemon eta fit --dry-run` to reproduce it",
        ),
        ("panic", _) => Check::bad(
            "fit",
            "last_check",
            Status::Fail,
            format!("the fit check panicked ({when})"),
            "run `loom-daemon eta fit --dry-run` to reproduce it, and check the daemon log for `daily refit panicked`",
        ),
        (other, _) => Check::bad(
            "fit",
            "last_check",
            Status::Warn,
            format!("unrecognised outcome {other:?} ({when})"),
            "this doctor is older than the daemon that wrote the record: update it",
        ),
    };
    if stale && check.status == Status::Ok {
        check = Check::bad(
            "fit",
            "last_check",
            Status::Warn,
            format!("{} (the loop checks hourly)", check.detail),
            "the fit loop is not checking: see `data.refresh_loop`, and the daemon log",
        );
    }
    check
}

fn serving(f: &Facts) -> Vec<Check> {
    let s = &f.serving;
    let mut out = Vec::new();
    out.push(if s.fit_loaded {
        Check::ok(
            "serving",
            "twin_otter_model",
            "a coefficient file is loaded: twin-otter has a model",
        )
    } else {
        Check::bad(
            "serving",
            "twin_otter_model",
            Status::Fail,
            "no coefficient file loaded: land-2026-10-04-twin-otter refuses no_model",
            "see the `fit` link: a fit must be written before twin-otter can answer",
        )
    });
    out.push(Check::ok(
        "serving",
        "registered_shadows",
        if s.shadows.is_empty() {
            "none".to_string()
        } else {
            s.shadows.join(", ")
        },
    ));
    if s.tallies.is_empty() {
        out.push(Check::skip("serving", "answers", "no pending estimates to tally"));
    }
    for t in &s.tallies {
        let name = format!("{} {}", t.kind, t.heuristic);
        let refused: u64 = t.refused.values().sum();
        let total = t.answered + refused;
        let mix: Vec<String> = t.refused.iter().map(|(k, v)| format!("{k}={v}")).collect();
        let detail = format!(
            "answered {}/{total}; refused {}",
            t.answered,
            if mix.is_empty() {
                "none".into()
            } else {
                mix.join(" ")
            }
        );
        out.push(if t.answered == 0 && t.refused.get("no_model") == Some(&refused) {
            Check::bad(
                "serving",
                &name,
                Status::Fail,
                format!("{detail}: every item refused no_model"),
                "no coefficient file: see the `fit` link",
            )
        } else if t.answered == 0 {
            Check::bad(
                "serving",
                &name,
                Status::Warn,
                format!("{detail}: answered nothing"),
                "read the refusal reasons above; the `data` link feeds the history these need",
            )
        } else {
            Check::ok("serving", &name, detail)
        });
    }
    out
}

fn snapshot_feed(f: &Facts) -> Vec<Check> {
    let mut out = Vec::new();
    out.push(if f.config.native_exporter {
        Check::ok("snapshot_feed", "native_exporter", "eta.snapshot has a native exporter to ride")
    } else {
        Check::bad(
            "snapshot_feed",
            "native_exporter",
            Status::Fail,
            "no native HTTPS exporter: the dashboard feed (eta.snapshot) is not sent",
            "add `https` to observability.exporters in .loom/config.json",
        )
    });
    out.push(Check::skip(
        "snapshot_feed",
        "alternates",
        "needs #10390 (eta.snapshot alternates) before the doctor can predict which rows carry them",
    ));
    out
}

fn outcomes(f: &Facts) -> Vec<Check> {
    let o = &f.outcomes;
    let mut out = Vec::new();
    out.push(match o.calibration_newest {
        None => Check::bad(
            "outcomes",
            "calibration",
            Status::Warn,
            "no calibration rows: no estimate has resolved against an outcome yet",
            "outcomes arrive as items merge; if estimates are being made and nothing resolves, check `serving` and the daemon's ETA pass log",
        ),
        Some(at) if f.now - at > Duration::days(7) => Check::bad(
            "outcomes",
            "calibration",
            Status::Warn,
            format!("newest calibration row is {} old", age(f.now, at)),
            "no estimate has resolved in a week: check `serving` and that items are landing",
        ),
        Some(at) => Check::ok("outcomes", "calibration", format!("newest calibration row {} old", age(f.now, at))),
    });
    if o.pairs.is_empty() {
        out.push(Check::skip(
            "outcomes",
            "shadow_pairs",
            "no shadow comparison has a paired outcome yet",
        ));
    }
    for p in &o.pairs {
        out.push(Check::ok(
            "outcomes",
            &format!("pairs {}", p.key),
            format!("{} paired outcome(s); a promotion gate needs 50", p.pairs),
        ));
    }
    if o.drift.is_empty() {
        out.push(Check::skip(
            "outcomes",
            "drift",
            "no scored outcome in the last 6h: nothing to check for regime drift",
        ));
    }
    for d in &o.drift {
        let name = format!("drift {}", d.stage);
        out.push(match d.state {
            DriftState::Drifted => Check::bad(
                "outcomes",
                &name,
                Status::Warn,
                format!(
                    "{} scored {} outcome(s) in 6h disagree with the baseline (regime drift, cause unknown); served ETAs are NOT adjusted for it yet (serving-path application is deferred, #10528)",
                    d.n_recent, d.heuristic
                ),
                "expect this stage's served ETAs to be biased until the next refit on current-regime rows",
            ),
            DriftState::Unknown => Check::ok(
                "outcomes",
                &name,
                format!(
                    "{} scored {} outcome(s) in 6h: drift unknown (below the {}-outcome floor)",
                    d.n_recent,
                    d.heuristic,
                    super::MIN_SAMPLES
                ),
            ),
            DriftState::Stable => Check::ok(
                "outcomes",
                &name,
                format!("{} scored {} outcome(s) in 6h, no drift", d.n_recent, d.heuristic),
            ),
        });
    }
    out.push(match o.oldest_pending {
        None => Check::skip("outcomes", "oldest_pending", "no pending estimates"),
        Some(at) => Check::ok(
            "outcomes",
            "oldest_pending",
            format!("{} pending; oldest estimate is {} old", o.pending, age(f.now, at)),
        ),
    });
    out
}

#[cfg(test)]
#[path = "doctor_tests.rs"]
mod tests;
