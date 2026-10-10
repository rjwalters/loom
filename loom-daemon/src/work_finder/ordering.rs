//! Dispatch-candidate ordering keys and the comparator (#3946, re-keyed by
//! #9244 for `loom:operator-priority`).
//!
//! Moved out of `work_finder.rs` (size-frozen, `.loom/docs/file-size-policy.md`)
//! when #9244 added the operator-priority and red-main-fix keys. The ready
//! queue ([`super::ready_queue`]) and both tick paths rank with the same
//! comparator, so the queue a human reads is the order dispatch uses.

use std::cmp::Ordering;

/// A dispatch candidate tagged with the cross-repo ordering keys: its
/// workspace's priority tier, the operator-priority ("starred") keys, the
/// red-main-fix key, age, and the workspace index used to route the eventual
/// `dispatch()` back to the owning workspace. Built by
/// [`super::ready_queue::key_of`], then globally sorted by [`candidate_cmp`]
/// before the shared concurrency budget is filled.
///
/// `loom:urgent` is **not** a key any more (#9244). The label is still
/// tolerated on an issue; it just no longer changes where the issue sorts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PriorityCandidate {
    /// The owning workspace's index in the `workspaces` slice (dispatch routing).
    pub workspace_idx: usize,
    /// The owning workspace's priority tier (lower = higher priority).
    pub workspace_priority: u32,
    /// The issue's effective operator priority level (#10307): 0 unstarred,
    /// 1 the star, 2 `loom:operator-high-priority` (own or inherited), and so
    /// on up the level table. Higher levels sort first, fleet-wide.
    pub operator_level: u8,
    /// Whether the issue is starred at any level (#9244): the operator
    /// starred it, so it sorts ahead of everything unstarred, fleet-wide.
    pub operator_priority: bool,
    /// When the issue was starred (the `labeled` timeline event for
    /// `loom:operator-priority`), when known. Orders starred issues among
    /// themselves; `None` falls back to [`Self::created_at`].
    pub operator_priority_at: Option<String>,
    /// Whether the issue carries the `<!-- loom:main-red-fix -->` marker
    /// **and** its repo's `main` is verified red right now. Resolved before
    /// the key is built, so [`candidate_cmp`] stays a pure function.
    pub main_red_fix: bool,
    /// The issue's creation timestamp for age ordering (oldest-first).
    pub created_at: Option<String>,
    /// The issue number (dispatch target + final deterministic tiebreak).
    pub number: u32,
    /// The issue's priority level (#11103), which the multi-workspace tick's
    /// workspace draw and in-workspace order use
    /// ([`super::workspace_draw`]). Not one of [`candidate_cmp`]'s keys.
    pub level: crate::priority_pick::Level,
    /// The issue's `<!-- loom:complexity=<tier> -->` stratum (#4827), carried
    /// from its work item so pass 2's `dispatch()` can stratify the
    /// model-cost A/B arm assignment without re-fetching the body. Not part of
    /// the ordering keys; [`candidate_cmp`] ignores it.
    pub complexity: Option<String>,
}

impl PriorityCandidate {
    /// Whether the candidate is `loom:very-important` (#11103) for the
    /// overflow slot and build back-off. A red-main fix is bridged to that
    /// level for the draw only; it keeps its own rules (it never uses the
    /// overflow slot, #9244), so it is excluded here.
    #[must_use]
    pub fn is_very_important(&self) -> bool {
        self.level == crate::priority_pick::Level::VeryImportant && !self.main_red_fix
    }
}

/// Total ordering over dispatch candidates (#3946, #9244, #10307):
///
/// 0. effective operator priority level, highest first (level 2 before the
///    plain star, #10307);
/// 1. starred (`loom:operator-priority`, any level) first;
/// 2. among starred issues, starred-at ascending — the issue starred first
///    lands first — with a missing starred-at falling back to `createdAt`;
/// 3. red-main fixes first (set only while that repo's `main` is verified red);
/// 4. workspace priority ascending (a tool repo pinned to `0` drains before a
///    product repo at the default `100`);
/// 5. `createdAt` oldest first (a dated issue sorts before an undated one);
/// 6. issue number ascending, so the order is fully deterministic.
///
/// The multi-workspace tick no longer dispatches in this order (#11103): it
/// orders candidates by weighted workspace draws
/// ([`super::workspace_draw`]). This comparator still ranks the published
/// ready-queue rows (`rank`); a row's dispatch position is its plan
/// `position`.
///
/// The keys themselves live in [`candidate_keys`] (Issue #9288), the one seam
/// the published dispatch plan is projected from; this is their
/// lexicographic compare.
#[must_use]
pub fn candidate_cmp(a: &PriorityCandidate, b: &PriorityCandidate) -> Ordering {
    candidate_keys(a).cmp(&candidate_keys(b))
}

/// One comparator key's value, carrying its own direction so the derived
/// [`Ord`] on `[CandidateKey; N]` is exactly [`candidate_cmp`] (Issue #9288).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyValue<'a> {
    /// Ascending unsigned value (workspace priority, issue number).
    Asc(u64),
    /// Descending unsigned value: the highest sorts first (the operator
    /// priority level, #10307).
    Desc(u64),
    /// A flag that sorts `true` first (starred, red-main fix).
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
            Self::TrueFirst(_) => 1,
            Self::OldestFirst(_) => 2,
            Self::Desc(_) => 3,
        }
    }

    /// The value as it goes on the wire.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Self::Asc(n) | Self::Desc(n) => serde_json::Value::from(*n),
            Self::TrueFirst(b) => serde_json::Value::Bool(*b),
            Self::OldestFirst(t) => t.map_or(serde_json::Value::Null, serde_json::Value::from),
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

/// The dispatch ordering keys of `c`, in comparator order (Issue #9288).
///
/// **The single seam for ready-queue rank**: [`candidate_cmp`] is the
/// lexicographic compare of these keys, and every ready-queue
/// row's plan `keys` and the plan's `ordering` are projected from them. The
/// order the tick dispatches in is the workspace draw (#11103), published as
/// each row's plan `position` and the `pick.decision` draw log.
///
/// Key 2 (starred-at) only orders starred issues among themselves: for an
/// unstarred candidate it is `None`, which ties with every other unstarred
/// candidate, and a starred one falls back to `createdAt` when its starred-at
/// is unknown.
#[must_use]
pub fn candidate_keys(c: &PriorityCandidate) -> [CandidateKey<'_>; 7] {
    let starred_at = if !c.operator_priority {
        None
    } else if c.operator_priority_at.is_some() {
        c.operator_priority_at.as_deref()
    } else {
        c.created_at.as_deref()
    };
    [
        CandidateKey {
            name: "operator_priority_level",
            value: KeyValue::Desc(u64::from(c.operator_level)),
        },
        CandidateKey {
            name: "operator_priority",
            value: KeyValue::TrueFirst(c.operator_priority),
        },
        CandidateKey {
            name: "operator_priority_at",
            value: KeyValue::OldestFirst(starred_at),
        },
        CandidateKey {
            name: "main_red_fix",
            value: KeyValue::TrueFirst(c.main_red_fix),
        },
        CandidateKey {
            name: "workspace_priority",
            value: KeyValue::Asc(u64::from(c.workspace_priority)),
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

/// The [`candidate_keys`] the ETA queue position uses (#10333), by name:
/// priority level, star bucket, starred-at, age (the PR's stage entry stands
/// in for `created_at`), then number.
pub const ETA_POSITION_KEYS: [&str; 5] = [
    "operator_priority_level",
    "operator_priority",
    "operator_priority_at",
    "created_at",
    "number",
];

/// The [`candidate_keys`] the ETA queue position **ignores** (#10333):
/// `main_red_fix` (a per-tick, per-repo red-main verdict with no point-in-time
/// record) and `workspace_priority` (constant inside one repo, and ETA
/// counts same-repo items only).
pub const ETA_IGNORED_KEYS: [&str; 2] = ["main_red_fix", "workspace_priority"];

/// The [`candidate_keys`] the ETA's **fleet-wide** dispatch position uses
/// (`eta-fit/v2`, #10508): every key but `main_red_fix` (no point-in-time
/// record). Unlike [`ETA_POSITION_KEYS`] it keeps `workspace_priority`,
/// because it counts PRs across repos, as cross-repo dispatch does.
pub const ETA_FLEET_POSITION_KEYS: [&str; 6] = [
    "operator_priority_level",
    "operator_priority",
    "operator_priority_at",
    "workspace_priority",
    "created_at",
    "number",
];

/// Lexicographic compare of `a` and `b` over only the [`candidate_keys`]
/// named in `names`, in comparator order. [`candidate_cmp`] is this with every
/// key; the ETA's queue position is this with [`ETA_POSITION_KEYS`], so
/// there is one ordering and a subset view of it, never a second comparator.
#[must_use]
pub fn keyed_cmp(a: &PriorityCandidate, b: &PriorityCandidate, names: &[&str]) -> Ordering {
    let (ka, kb) = (candidate_keys(a), candidate_keys(b));
    ka.iter()
        .zip(kb.iter())
        .filter(|(x, _)| names.contains(&x.name))
        .map(|(x, y)| x.value.cmp(&y.value))
        .find(|o| *o != Ordering::Equal)
        .unwrap_or(Ordering::Equal)
}
