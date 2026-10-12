//! Dispatch-candidate ordering keys and the comparator (#3946, re-keyed by
//! #11508 to the priority model of #11103).
//!
//! The ready queue ([`super::ready_queue`]) ranks its rows with
//! [`candidate_cmp`], and the in-workspace order of the workspace draw
//! ([`super::workspace_draw`]) uses the same four keys, so the queue a human
//! reads agrees with the order dispatch takes inside one workspace. The
//! leading key is the verified red-main fix (#11547): while a repo's `main`
//! is verified red this tick, its fix ranks first.

use std::cmp::Ordering;

use crate::priority_pick::Level;

/// A dispatch candidate tagged with its ordering keys and the workspace index
/// used to route the eventual `dispatch()` back to the owning workspace.
/// Built by [`super::ready_queue::key_of`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PriorityCandidate {
    /// The owning workspace's index in the `workspaces` slice (dispatch routing).
    pub workspace_idx: usize,
    /// The owning workspace's `fleet_priority` (lower = higher priority). It
    /// weights the workspace draw; it is not one of [`candidate_cmp`]'s keys.
    pub workspace_priority: u32,
    /// The issue's creation timestamp for age ordering (oldest-first).
    pub created_at: Option<String>,
    /// The issue number (dispatch target + final deterministic tiebreak).
    pub number: u32,
    /// The issue's priority level (#11103), from its `loom:important` /
    /// `loom:very-important` label only ([`Level::of`]).
    pub level: Level,
    /// Whether the issue carries the `<!-- loom:main-red-fix -->` marker
    /// **and** its repo's `main` is verified red this tick (#9244). It is
    /// [`candidate_cmp`]'s leading key (#11547): while `main` is red nothing
    /// in that repo can merge, so its fix goes first. It is not an input to
    /// [`Self::level`], and on a green `main` it is `false`, so the marker
    /// grants nothing. Its only gate effect is unchanged: it lets the fix
    /// past the three gates that `main` being red would otherwise deadlock
    /// (#11508): the build back-off, the file-overlap gate and the
    /// repo-sharding slice.
    pub main_red_fix: bool,
    /// The issue's `<!-- loom:complexity=<tier> -->` stratum (#4827), carried
    /// from its work item so pass 2's `dispatch()` can stratify the
    /// model-cost A/B arm assignment without re-fetching the body. Not part of
    /// the ordering keys; [`candidate_cmp`] ignores it.
    pub complexity: Option<String>,
}

/// Total ordering over dispatch candidates (#11103, #11508, #11547):
///
/// 0. verified red-main fix first (`main_red_fix`, true only while the
///    repo's `main` is verified red this tick);
/// 1. priority level, highest first (`loom:very-important`, then
///    `loom:important`, then none);
/// 2. `createdAt` oldest first (a dated issue sorts before an undated one);
/// 3. issue number ascending, so the order is fully deterministic.
///
/// Priority changes order only, never eligibility: no key here admits a
/// candidate past any gate.
///
/// The multi-workspace tick dispatches in workspace-draw order
/// ([`super::workspace_draw`]); this comparator ranks the published
/// ready-queue rows (`rank`), and a row's dispatch position is its plan
/// `position`. The keys live in [`candidate_keys`] (Issue #9288), the one
/// seam the published dispatch plan is projected from.
#[must_use]
pub fn candidate_cmp(a: &PriorityCandidate, b: &PriorityCandidate) -> Ordering {
    candidate_keys(a).cmp(&candidate_keys(b))
}

/// One comparator key's value, carrying its own direction so the derived
/// [`Ord`] on `[CandidateKey; N]` is exactly [`candidate_cmp`] (Issue #9288).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyValue<'a> {
    /// Ascending unsigned value (issue number).
    Asc(u64),
    /// Descending unsigned value: the highest sorts first (the level).
    Desc(u64),
    /// A flag that sorts `true` first (the verified red-main fix); on the
    /// wire a JSON bool.
    TrueFirst(bool),
    /// Oldest-first ISO-8601 timestamp: a dated issue (`Some`) sorts before
    /// an undated one (`None`); two dated issues compare lexically (⇒
    /// chronologically); two undated issues are equal.
    OldestFirst(Option<&'a str>),
}

impl Ord for KeyValue<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Asc(a), Self::Asc(b)) => a.cmp(b),
            (Self::Desc(a), Self::Desc(b)) => b.cmp(a),
            (Self::TrueFirst(a), Self::TrueFirst(b)) => b.cmp(a),
            (Self::OldestFirst(a), Self::OldestFirst(b)) => match (a, b) {
                (Some(x), Some(y)) => x.cmp(y),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            },
            // Keys are compared position by position, so the variants always
            // match; a mismatch still orders deterministically.
            _ => self.kind().cmp(&other.kind()),
        }
    }
}

impl PartialOrd for KeyValue<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl KeyValue<'_> {
    const fn kind(&self) -> u8 {
        match self {
            Self::Asc(_) => 0,
            Self::OldestFirst(_) => 1,
            Self::Desc(_) => 2,
            Self::TrueFirst(_) => 3,
        }
    }

    /// The value as it goes on the wire.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Self::Asc(n) | Self::Desc(n) => serde_json::Value::from(*n),
            Self::OldestFirst(t) => t.map_or(serde_json::Value::Null, serde_json::Value::from),
            Self::TrueFirst(b) => serde_json::Value::Bool(*b),
        }
    }
}

/// A named comparator key. Ordered by value only: the names are the same at
/// every position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CandidateKey<'a> {
    pub value: KeyValue<'a>,
    pub name: &'static str,
}

/// The wire value of `level` in the `level` key: 0 default, 1 important,
/// 2 very important.
#[must_use]
pub fn level_rank(level: Level) -> u64 {
    u64::from(level.rank())
}

/// The dispatch ordering keys of `c`, in comparator order (Issue #9288):
/// exactly `main_red_fix`, `level`, `created_at`, `number` (#11547).
///
/// **The single seam for ready-queue rank**: [`candidate_cmp`] is the
/// lexicographic compare of these keys, and every ready-queue row's plan
/// `keys` and the plan's `ordering` are projected from them.
#[must_use]
pub fn candidate_keys(c: &PriorityCandidate) -> [CandidateKey<'_>; 4] {
    [
        CandidateKey {
            name: "main_red_fix",
            value: KeyValue::TrueFirst(c.main_red_fix),
        },
        CandidateKey {
            name: "level",
            value: KeyValue::Desc(level_rank(c.level)),
        },
        CandidateKey {
            name: "created_at",
            value: KeyValue::OldestFirst(c.created_at.as_deref()),
        },
        CandidateKey {
            name: "number",
            value: KeyValue::Asc(u64::from(c.number)),
        },
    ]
}
