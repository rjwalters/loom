//! The Champion re-evaluation ladder, stored on the forge (#11900; design:
//! #11894 §5).
//!
//! A Champion re-evaluation that leaves an item as it was (`--pr` `held`, or
//! `--issue` `blocked`) would be drawn again on the next tick: the classifier
//! still calls for a hold re-evaluation or a re-evaluate. So each such outcome
//! advances a ladder, **1 h, 1 h, 1 h, 2 h, 4 h, 8 h, 24 h, 48 h, 96 h, then
//! 7 d** for every later rung ([`rung_delay`]; operator ruling, #12513), and
//! the item is not re-evaluated before the rung's due time.
//!
//! # Markers from an older, longer ladder
//!
//! Until #12513 the ladder ran 6 h doubled per rung (6 h → 7 d), so a marker
//! written then can hold its item for days past what its rung now allows. A
//! stored `next=` further from now than [`rung_delay`] of its rung (plus
//! [`SKEW_GRACE`] for clock skew between hosts) is read as due now
//! ([`Reeval::due_at`], applied by [`due`], which the cutover reads). The
//! Champion's next verdict on the item rewrites the marker on this ladder
//! (`next` = verdict time + the new rung's delay), which is within the delay,
//! so the rule fires once per stale marker. A `next=` within the delay is
//! honoured unchanged.
//!
//! # Where it lives
//!
//! In the item's body, beside its park record (`<!-- loom:park … -->`) or under
//! its Champion hold marker, as one line:
//!
//! ```text
//! <!-- loom:reeval step=champion_merge rung=2 next=2026-10-16T12:00:00Z -->
//! ```
//!
//! On the forge and nowhere else past a day: a rung is days long, longer than
//! anything the peer broadcast may carry (24 h), and it must survive a restart
//! and a host change. The registry mirrors the due time locally, capped at
//! 24 h, only so the next tick does not redraw the item before the listing
//! shows the new body. The park itself is untouched: after a rollback it
//! stays parked until a role or a human clears it (nothing releases
//! `loom:blocked` automatically since #11301), and the marker is inert
//! to every other reader (it is not a `loom:park` record).
//!
//! A Champion verdict that moves the item (`merged`, `rejected`, `promoted`,
//! …) removes the marker ([`clear`]), so a later hold starts again at 1 h.

use std::sync::OnceLock;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use regex::Regex;

use crate::step_role::StepRole;

/// The opening sentinel.
/// Spelled with `concat!` so no bare label-namespace literal sits in source
/// (`tests/label_literal_guard.rs`): this is a marker, not a label.
pub const MARKER_OPEN: &str = concat!("<!-- loom", ":reeval ");

/// The rungs' delays in hours, rung 0 first (operator ruling, #12513). Every
/// rung past the table is [`LAST_RUNG`].
const RUNG_HOURS: [u64; 9] = [1, 1, 1, 2, 4, 8, 24, 48, 96];

/// The first rung: 1 h.
pub const FIRST_RUNG: Duration = Duration::from_secs(RUNG_HOURS[0] * 3600);

/// The last rung: 7 d. Every rung past the table stays at 7 d.
pub const LAST_RUNG: Duration = Duration::from_secs(7 * 24 * 3600);

/// How far a stored `next=` may sit past its rung's delay before it is read as
/// written under an older ladder: room for clock skew between the host that
/// wrote it and the host reading it.
pub const SKEW_GRACE: Duration = Duration::from_secs(5 * 60);

/// The delay of rung `rung` (0-based): 1, 1, 1, 2, 4, 8, 24, 48, 96 h, then
/// 7 d.
#[must_use]
pub fn rung_delay(rung: u32) -> Duration {
    usize::try_from(rung)
        .ok()
        .and_then(|i| RUNG_HOURS.get(i))
        .map_or(LAST_RUNG, |h| Duration::from_secs(h * 3600))
}

/// One ladder record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reeval {
    pub step: StepRole,
    pub rung: u32,
    /// Not re-evaluated before this.
    pub next: DateTime<Utc>,
}

impl Reeval {
    /// When this record is due, read at `now`: its `next`, unless that lies
    /// further from `now` than its rung's delay plus [`SKEW_GRACE`]. Such a
    /// record was written under an older, longer ladder (#12513), and is due
    /// now.
    #[must_use]
    pub fn due_at(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        let allowed = chrono::Duration::from_std(rung_delay(self.rung).saturating_add(SKEW_GRACE))
            .unwrap_or_else(|_| chrono::Duration::days(7));
        if self.next - now > allowed {
            now
        } else {
            self.next
        }
    }
}

fn marker_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(r"<!--\s*loom", r":reeval\s+([^>]*?)\s*-->"))
            .expect("static reeval-marker pattern")
    })
}

fn parse_inner(inner: &str) -> Option<Reeval> {
    let mut step = None;
    let mut rung = None;
    let mut next = None;
    for kv in inner.split_whitespace() {
        match kv.split_once('=') {
            Some(("step", v)) => step = v.parse::<StepRole>().ok(),
            Some(("rung", v)) => rung = v.parse::<u32>().ok(),
            Some(("next", v)) => {
                next = DateTime::parse_from_rfc3339(v)
                    .ok()
                    .map(|t| t.with_timezone(&Utc));
            }
            _ => {}
        }
    }
    Some(Reeval {
        step: step?,
        rung: rung?,
        next: next?,
    })
}

/// Every well-formed ladder record in `body`, in document order.
#[must_use]
pub fn parse(body: &str) -> Vec<Reeval> {
    marker_re()
        .captures_iter(body)
        .filter_map(|c| parse_inner(c.get(1)?.as_str()))
        .collect()
}

/// When `step` on the item whose body is `body` is next due, read at `now`,
/// if a ladder record for it exists: each record's [`Reeval::due_at`], so a
/// marker from the pre-#12513 ladder is due `now`. The cutover feeds this
/// into the draw's [`super::outcome_ledger::StepCooldowns`].
#[must_use]
pub fn due(body: &str, step: StepRole, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    parse(body)
        .into_iter()
        .filter(|r| r.step == step)
        .map(|r| r.due_at(now))
        .max()
}

/// Render one record as its body line.
#[must_use]
pub fn render(r: &Reeval) -> String {
    format!(
        "{MARKER_OPEN}step={} rung={} next={} -->",
        r.step.as_str(),
        r.rung,
        r.next.to_rfc3339_opts(SecondsFormat::Secs, true)
    )
}

/// `body` with `step`'s ladder markers removed (with their line when they
/// stood alone), or `None` when it has none. Another step's marker, and a
/// malformed one, are left alone: the same scope [`due`] reads.
#[must_use]
pub fn strip(body: &str, step: StepRole) -> Option<String> {
    let is_step = |c: &regex::Captures<'_>| {
        c.get(1)
            .and_then(|m| parse_inner(m.as_str()))
            .is_some_and(|r| r.step == step)
    };
    if !marker_re().captures_iter(body).any(|c| is_step(&c)) {
        return None;
    }
    let drop_markers = |text: &str| -> String {
        marker_re()
            .replace_all(text, |c: &regex::Captures<'_>| {
                if is_step(c) {
                    String::new()
                } else {
                    c.get(0).map_or(String::new(), |m| m.as_str().to_string())
                }
            })
            .into_owned()
    };
    let kept: Vec<&str> = body
        .split('\n')
        .filter(|line| !(drop_markers(line) != *line && drop_markers(line).trim().is_empty()))
        .collect();
    let out = drop_markers(&kept.join("\n"));
    Some(out.trim_end_matches('\n').to_string())
}

/// Advance `step`'s ladder in `body` at `now`: the next rung after the one
/// recorded (rung 0 when none is), due `now + rung_delay(rung)`. Returns the
/// new body and the record written.
#[must_use]
pub fn advance(body: &str, step: StepRole, now: DateTime<Utc>) -> (String, Reeval) {
    let rung = parse(body)
        .into_iter()
        .filter(|r| r.step == step)
        .map(|r| r.rung.saturating_add(1))
        .max()
        .unwrap_or(0);
    let delay =
        chrono::Duration::from_std(rung_delay(rung)).unwrap_or_else(|_| chrono::Duration::days(7));
    let record = Reeval {
        step,
        rung,
        next: now + delay,
    };
    let base = strip(body, step).unwrap_or_else(|| body.to_string());
    let base = base.trim_end_matches('\n');
    let out = if base.is_empty() {
        render(&record)
    } else {
        format!("{base}\n\n{}", render(&record))
    };
    (out, record)
}

/// The two forge operations the ladder needs: read and rewrite an item's body
/// (an issue or a PR: the issues API serves both).
pub trait BodyForge {
    fn body(&mut self, number: u64) -> Result<String, String>;
    fn set_body(&mut self, number: u64, body: &str) -> Result<(), String>;
}

impl<F: crate::operator_decision::cli::Forge> BodyForge for F {
    fn body(&mut self, number: u64) -> Result<String, String> {
        self.view(number).map(|s| s.body)
    }
    fn set_body(&mut self, number: u64, body: &str) -> Result<(), String> {
        crate::operator_decision::cli::Forge::set_body(self, number, body)
    }
}

/// A failed [`record`]: the forge's error, and the record it tried to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordError {
    /// The next rung after the stored one, due `now + rung_delay(rung)`: what
    /// the marker would have said. When the read itself failed the stored
    /// rung is unknown, and this is rung 0.
    pub attempted: Reeval,
    pub error: String,
}

impl std::fmt::Display for RecordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.error)
    }
}

/// Advance the ladder on the forge: read `item`'s body fresh, write the next
/// rung. Returns the record written.
///
/// # Errors
///
/// A failed read or write, as the forge reported it, with the record that was
/// not written (#12516: the caller holds the item locally until its due time).
pub fn record<F: BodyForge + ?Sized>(
    forge: &mut F,
    item: u32,
    step: StepRole,
    now: DateTime<Utc>,
) -> Result<Reeval, RecordError> {
    let body = forge.body(u64::from(item)).map_err(|error| RecordError {
        attempted: advance("", step, now).1,
        error,
    })?;
    let (new_body, rec) = advance(&body, step, now);
    forge
        .set_body(u64::from(item), &new_body)
        .map_err(|error| RecordError {
            attempted: rec.clone(),
            error,
        })?;
    Ok(rec)
}

/// Remove `step`'s ladder from `item`'s body on the forge. Returns whether a marker
/// was there (no write when none was).
///
/// # Errors
///
/// A failed read or write, as the forge reported it.
pub fn clear<F: BodyForge + ?Sized>(
    forge: &mut F,
    item: u32,
    step: StepRole,
) -> Result<bool, String> {
    let body = forge.body(u64::from(item))?;
    match strip(&body, step) {
        Some(new_body) => forge.set_body(u64::from(item), &new_body).map(|()| true),
        None => Ok(false),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "reeval_ladder_tests.rs"]
mod tests;
