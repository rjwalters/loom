//! One issue's or PR's label history from its `issues/{n}/events` pages
//! (#11367): every `labeled` / `unlabeled`, `closed` and `reopened` instant, so one read
//! dates both the stage a row left and the stage it entered.

use chrono::{DateTime, Utc};

/// The page size of an issue-events read; a shorter page is the last one.
pub const EVENTS_PAGE_SIZE: usize = 100;

/// One event of the history that a stage boundary can be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryEvent {
    Labeled(String),
    Unlabeled(String),
    Closed,
    Reopened,
}

/// Who caused an event, as the events API reports it (#11956). Captured only:
/// no human-versus-Loom-bot classification is made here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventActor {
    pub login: String,
    /// `actor.type == "Bot"`.
    pub is_bot: bool,
}

/// The events of one issue or PR, oldest first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LabelHistory {
    pub events: Vec<(DateTime<Utc>, HistoryEvent)>,
    /// The actor of `events[i]`, aligned by index (#11956). `None` when the
    /// page carried no `actor`; may be shorter than `events` for a history
    /// built without one, so read it through [`LabelHistory::actor`].
    pub actors: Vec<Option<EventActor>>,
}

impl LabelHistory {
    /// Add one page of the events API.
    fn extend_from_page(&mut self, page: &serde_json::Value) {
        for event in page.as_array().into_iter().flatten() {
            let Some(at) = event["created_at"]
                .as_str()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|t| t.with_timezone(&Utc))
            else {
                continue;
            };
            let label = || event["label"]["name"].as_str().map(str::to_string);
            let parsed = match event["event"].as_str() {
                Some("labeled") => label().map(HistoryEvent::Labeled),
                Some("unlabeled") => label().map(HistoryEvent::Unlabeled),
                Some("closed") => Some(HistoryEvent::Closed),
                Some("reopened") => Some(HistoryEvent::Reopened),
                _ => None,
            };
            if let Some(parsed) = parsed {
                self.events.push((at, parsed));
                self.actors
                    .push(event["actor"]["login"].as_str().map(|login| EventActor {
                        login: login.to_string(),
                        is_bot: event["actor"]["type"].as_str() == Some("Bot"),
                    }));
            }
        }
    }

    /// The history from pages `read_page(1)`, `read_page(2)`, … until a
    /// short page. `None` when any page fails or is not an array.
    pub fn read(mut read_page: impl FnMut(usize) -> Option<serde_json::Value>) -> Option<Self> {
        let mut history = LabelHistory::default();
        for page in 1.. {
            let events = read_page(page)?;
            let len = events.as_array()?.len();
            history.extend_from_page(&events);
            if len < EVENTS_PAGE_SIZE {
                break;
            }
        }
        let mut paired: Vec<_> = history.events.into_iter().zip(history.actors).collect();
        paired.sort_by_key(|((at, _), _)| *at);
        let (events, actors) = paired.into_iter().unzip();
        Some(LabelHistory { events, actors })
    }

    /// The actor of `events[index]`, when the page named one (#11956).
    pub fn actor(&self, index: usize) -> Option<&EventActor> {
        self.actors.get(index)?.as_ref()
    }

    /// The latest instant at or before `bound` of an event `pick` accepts.
    pub fn latest(
        &self,
        bound: DateTime<Utc>,
        pick: impl Fn(&HistoryEvent) -> bool,
    ) -> Option<DateTime<Utc>> {
        self.events
            .iter()
            .filter(|(at, e)| *at <= bound && pick(e))
            .map(|(at, _)| *at)
            .max()
    }

    /// The first event strictly after `after` that `pick` accepts.
    pub fn first_after(
        &self,
        after: DateTime<Utc>,
        pick: impl Fn(&HistoryEvent) -> bool,
    ) -> Option<(DateTime<Utc>, &HistoryEvent)> {
        self.events
            .iter()
            .find(|(at, e)| *at > after && pick(e))
            .map(|(at, e)| (*at, e))
    }

    /// Whether an event `pick` accepts falls in `[from, to]`.
    pub fn any_within(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        pick: impl Fn(&HistoryEvent) -> bool,
    ) -> bool {
        self.events
            .iter()
            .any(|(at, e)| *at >= from && *at <= to && pick(e))
    }
}

/// Whether `event` is the `labeled` event of one of `labels`.
pub fn labeled(event: &HistoryEvent, labels: &[&str]) -> bool {
    matches!(event, HistoryEvent::Labeled(l) if labels.contains(&l.as_str()))
}

/// Whether `event` is the `unlabeled` event of one of `labels`.
pub fn unlabeled(event: &HistoryEvent, labels: &[&str]) -> bool {
    matches!(event, HistoryEvent::Unlabeled(l) if labels.contains(&l.as_str()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn actor_login_and_bot_flag_are_captured_and_absence_keeps_the_event() {
        let page = serde_json::json!([
            {"event": "labeled", "label": {"name": "loom:issue"},
             "created_at": "2026-10-14T12:00:00Z",
             "actor": {"login": "loom-bot", "type": "Bot"}},
            {"event": "unlabeled", "label": {"name": "loom:operator"},
             "created_at": "2026-10-14T11:00:00Z",
             "actor": {"login": "turian", "type": "User"}},
            {"event": "closed", "created_at": "2026-10-14T13:00:00Z"},
        ]);
        let history = LabelHistory::read(|_| Some(page.clone())).unwrap();
        assert_eq!(history.events.len(), 3);
        // Oldest first: the actors travel with their events through the sort.
        assert_eq!(
            history.actor(0),
            Some(&EventActor {
                login: "turian".to_string(),
                is_bot: false
            })
        );
        assert_eq!(
            history.actor(1),
            Some(&EventActor {
                login: "loom-bot".to_string(),
                is_bot: true
            })
        );
        assert_eq!(history.actor(2), None, "no actor on the page: None");
        assert_eq!(history.actor(9), None, "out of range: None");
    }
}
