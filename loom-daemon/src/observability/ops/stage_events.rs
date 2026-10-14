//! An item's latest `labeled` times, read from the **end** of its events
//! (#11924), for stage-dwell ([`super::stage_dwell`]) and the step engine's
//! stage-entry provider ([`crate::step_dispatch::stage_entry`]).
//!
//! GitHub lists `GET repos/{repo}/issues/{n}/events` oldest first, 100 to a
//! page. Reading only page 1 of an item with more than 100 events returns
//! the label's *earliest* applications, not its latest, which overstates
//! stage age most for PRs that cycle through review many times. So
//! [`walk`] reads page 1 (the only way to learn the `Link` header's
//! `rel="last"` page), then walks back from the last page until it has seen
//! a `labeled` event for the wanted label, at most [`MAX_PAGES`] pages in
//! all, and never more than the caller's budget allows. Each page is one
//! read; [`LabelTimes::pages`] says how many were spent.
//!
//! **Only times read from a contiguous run ending at the last page are
//! returned.** A label's latest event in such a run is its latest event,
//! full stop. Page 1's times are kept only when the walk reached page 2, so
//! no skipped middle page can hide a newer application. A label whose
//! latest event lies before the pages read has no time (not found), never
//! an earlier one.
//!
//! Each reader passes its own `caller` (the facade operation), so
//! `loom.forge.calls` tells stage-dwell's reads from the provider's.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::cmd_out::CmdOutcome;
use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};

use super::stage_dwell::parse_label_times;

/// Most events pages one item read may spend: page 1 (for the `Link`
/// header), the last page, and one page back from it.
pub const MAX_PAGES: usize = 3;

/// Timeout for one page read.
const GH_TIMEOUT: Duration = Duration::from_secs(30);

/// What one item's events read found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LabelTimes {
    /// The latest `labeled` time of every label whose latest `labeled`
    /// event was read (see the module docs); `None` when a page read failed
    /// or was shed.
    pub times: Option<BTreeMap<String, DateTime<Utc>>>,
    /// Pages requested, a failed one included: each counts against the
    /// caller's read budget.
    pub pages: usize,
}

/// One events page and, from its `Link` header, the number of the last
/// page (`None`: the header names no `rel="last"`, so this page is the last).
#[derive(Debug, Clone, PartialEq)]
pub struct EventsPage {
    pub events: serde_json::Value,
    pub last: Option<u32>,
}

/// Read `label`'s latest `labeled` event (and every other label time the
/// pages read can vouch for) through `read`, which returns page `n` or
/// `None` on failure, spending at most `max_pages` reads (at least one, at
/// most [`MAX_PAGES`]). A multi-page item read with a budget of one page is
/// unanswered (`times: None`).
pub fn walk(
    mut read: impl FnMut(u32) -> Option<EventsPage>,
    label: &str,
    max_pages: usize,
) -> LabelTimes {
    let max_pages = max_pages.clamp(1, MAX_PAGES);
    let mut pages = 1;
    let Some(first) = read(1) else {
        return LabelTimes { times: None, pages };
    };
    let last = first.last.unwrap_or(1);
    if last <= 1 {
        return LabelTimes {
            times: Some(parse_label_times(&first.events)),
            pages,
        };
    }
    // Newest page first: a label's first time seen in the walk is its latest.
    let mut tail: BTreeMap<String, DateTime<Utc>> = BTreeMap::new();
    let mut lowest = last + 1;
    for page in (2..=last).rev() {
        if pages >= max_pages || has(&tail, label) {
            break;
        }
        pages += 1;
        let Some(read_page) = read(page) else {
            return LabelTimes { times: None, pages };
        };
        for (name, at) in parse_label_times(&read_page.events) {
            tail.entry(name).or_insert(at);
        }
        lowest = page;
    }
    if lowest > last {
        // The budget allowed page 1 alone: on a multi-page item that answers
        // nothing (its times may all be superseded), so it is unanswered,
        // not "not found".
        return LabelTimes { times: None, pages };
    }
    if lowest == 2 {
        for (name, at) in parse_label_times(&first.events) {
            tail.entry(name).or_insert(at);
        }
    }
    LabelTimes {
        times: Some(tail),
        pages,
    }
}

fn has(times: &BTreeMap<String, DateTime<Utc>>, label: &str) -> bool {
    times.keys().any(|name| name.eq_ignore_ascii_case(label))
}

/// The live read: [`walk`] over `gh api --include` page reads in the repo's
/// own checkout (so a cross-owner repo uses its own owner's credential),
/// each counted under `caller`. Observability-class: a shed page is a
/// failed read.
pub fn read(
    caller: &'static str,
    root: &Path,
    slug: &str,
    number: u32,
    label: &str,
    max_pages: usize,
) -> LabelTimes {
    walk(|page| read_page(caller, root, slug, number, page), label, max_pages)
}

fn read_page(
    caller: &'static str,
    root: &Path,
    slug: &str,
    number: u32,
    page: u32,
) -> Option<EventsPage> {
    let path = format!("repos/{slug}/issues/{number}/events?per_page=100&page={page}");
    let CmdOutcome::Ran(output) =
        GhInvocation::new(Operation::new(caller), AccessIntent::Read, GhTarget::None, GH_TIMEOUT)
            .read_class(crate::gh_invocation::ReadClass::Observability)
            .args(["api", "--include", path.as_str()])
            .current_dir(root)
            .run()
    else {
        return None;
    };
    if !output.status.success() {
        log::debug!("observability: gh api {path} failed in {}", root.display());
        return None;
    }
    parse_page(&String::from_utf8_lossy(&output.stdout))
}

/// One `gh api --include` events response: a `2xx` with a JSON body.
#[must_use]
pub fn parse_page(raw: &str) -> Option<EventsPage> {
    let response = crate::forge_listing::parse_http_response(raw)?;
    if !(200..300).contains(&response.status) {
        return None;
    }
    let events = serde_json::from_str(&response.body).ok()?;
    Some(EventsPage {
        events,
        last: last_page(raw),
    })
}

/// The page number of the header block's `Link: <…&page=N>; rel="last"`.
#[must_use]
pub fn last_page(raw: &str) -> Option<u32> {
    let link = raw
        .lines()
        .skip(1)
        .map(|line| line.trim_end_matches('\r'))
        .take_while(|line| !line.is_empty())
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim().eq_ignore_ascii_case("link").then_some(value)
        })?;
    link.split(',').find_map(|part| {
        let (url, params) = part.split_once(';')?;
        if !params.split(';').any(|p| p.trim() == r#"rel="last""#) {
            return None;
        }
        let url = url.trim().trim_start_matches('<').trim_end_matches('>');
        let (_, query) = url.split_once('?')?;
        query
            .split('&')
            .find_map(|kv| kv.strip_prefix("page="))
            .and_then(|n| n.parse().ok())
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "stage_events_tests.rs"]
mod tests;
