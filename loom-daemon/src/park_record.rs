//! The **park record** — the machine-readable `loom:blocked` marker (#8925).
//!
//! # The defect this closes
//!
//! `loom:blocked` says "waiting on a dependency". Nothing said *which*
//! dependency in a form a machine can read. Every role that applies the label
//! writes its reasoning as prose, in a **comment**, and prose in a comment is
//! not a declaration:
//!
//! - PR #8314 was parked on #8322 by a Doctor stand-down whose only record was
//!   *"filed #8322 to track it, standing down"*. Correct reasoning, 155h parked,
//!   invisible to every automated lane.
//! - Issue #8852 was parked on #8860. #8860 closed; #8852 stayed parked. Every
//!   other gate would have passed — the blocker was only ever stated in a
//!   Curator comment as *"**Phase 2** (follow-up, after #8860 lands)"*.
//!
//! A park record is the fix: one line, in the **artifact body**, naming one
//! blocker.
//!
//! # The grammar
//!
//! ```text
//! <!-- loom:park Blocked by: #8322 by=doctor at=2026-09-19T12:09:00Z reason="needs an architecture ruling" -->
//! ```
//!
//! - `<!-- loom:park ` … ` -->` — an HTML comment, so it renders invisibly, and
//!   a **sentinel** that distinguishes a declaration from prose that merely
//!   *mentions* a blocker. Same pattern, and same reason, as the lease record
//!   (`<!-- loom:lease host=… sweep=… -->`, `defaults/docs/lease-record.md`) and
//!   the verdict-SHA / `loom:ac-verified` markers.
//! - `Blocked by: #N` — **deliberately the existing vocabulary**, not a new one.
//!   [`crate::dep_recheck::extract::DEPENDENCY_PHRASES`] already matches
//!   `Blocked by`, as do `guide.md`'s `parse_dependencies`,
//!   `warn-operator-gated.sh` and `detect-dependency-cycle.sh`. So a park record
//!   is readable by **every parser already in the fleet** the moment it is
//!   written, and this module adds no second dependency vocabulary. The test
//!   suite asserts that round trip rather than trusting it.
//! - `by=<role>` / `at=<RFC3339>` — provenance, so a park can be attributed and
//!   aged without reading the comment trail.
//! - `reason="…"` — optional free text, quoted so it can contain spaces.
//!
//! # Why one record per blocker, and not a comma-separated list
//!
//! Not cosmetic — it is forced by the parsers this format promises to be
//! compatible with, which do **not** agree on multi-reference lines:
//!
//! | Parser | `Blocked by: #1, #2` yields |
//! |---|---|
//! | `guide.md`'s two-stage `parse_dependencies` (#4508) | `1`, `2` |
//! | [`crate::dep_recheck::extract`] (one regex, one capture group) | `1` only |
//!
//! The second is the parser `check-stale-blocked` and `curator.md`'s premise
//! re-check actually run, so a comma-separated record would silently declare
//! only its **first** blocker — a park that clears while a real blocker is still
//! open. Emitting one record per blocker is readable identically by both, needs
//! no change to either, and gives each blocker its own provenance. [`parse`]
//! stays tolerant of a hand-written multi-reference marker by expanding it into
//! one record per reference.
//!
//! # Why the reason is stripped before blockers are extracted
//!
//! A reason is prose written by an agent, and prose mentions issue numbers
//! ("compounded by the #8940 dispatch bug"). Extracting `#N` from the whole
//! marker would silently promote such a mention to a *declared blocker* — a park
//! that never clears because one of its "blockers" was never a blocker.
//! [`parse`] removes the quoted reason span first, then extracts.
//!
//! # What this module does not do
//!
//! It never reads the forge and never writes a label. Rendering is for the role
//! applying a park; parsing is for whoever later asks whether the park still
//! holds ([`crate::stale_blocked`]). The full convention, including what each
//! role must write, is `defaults/docs/park-record.md`.

pub mod apply;
mod code;

use regex::Regex;
use std::collections::BTreeSet;
use std::ops::Range;
use std::sync::OnceLock;

/// The opening sentinel. A body containing this substring carries a park record
/// (or an attempt at one — [`parse`] decides).
pub const MARKER_OPEN: &str = "<!-- loom:park ";

/// The closing sentinel.
pub const MARKER_CLOSE: &str = "-->";

/// The dependency phrase a rendered record always uses, chosen because every
/// existing parser in the fleet already matches it. Changing it is a fleet-wide
/// compatibility break, not a formatting preference.
pub const RENDERED_PHRASE: &str = "Blocked by:";

/// What a record with no stated blocker renders in place of a reference.
///
/// A record is still worth writing without one: it declares "parked, blocker
/// unstated", which is attributable and dated, unlike silence. No parser
/// extracts a reference from it, which is correct — there is none.
pub const UNSTATED: &str = "(unstated)";

/// A declared blocker: an issue/PR number, optionally qualified with the
/// repository it lives in (#10443).
///
/// `repo: None` means the repo the record was written in. A *qualified*
/// reference is never resolved against the local repo: `example-org/tool-repo#202` and
/// local `#202` are different artifacts. Accepted shapes: `N`, `#N`,
/// `OWNER/REPO#N`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockerRef {
    /// `OWNER/REPO`, or `None` for the local repo.
    pub repo: Option<String>,
    pub number: u64,
}

impl BlockerRef {
    /// A local-repo reference.
    #[must_use]
    pub fn local(number: u64) -> Self {
        Self { repo: None, number }
    }

    /// Whether this names `number` in the local repo, given the local repo's
    /// slug if known. A qualified ref matches only when its repo equals
    /// `current_repo` (case-insensitively).
    #[must_use]
    pub fn is_local(&self, current_repo: Option<&str>) -> bool {
        match (&self.repo, current_repo) {
            (None, _) => true,
            (Some(r), Some(cur)) => r.eq_ignore_ascii_case(cur),
            (Some(_), None) => false,
        }
    }
}

impl std::fmt::Display for BlockerRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.repo {
            Some(r) => write!(f, "{r}#{}", self.number),
            None => write!(f, "#{}", self.number),
        }
    }
}

impl std::str::FromStr for BlockerRef {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let t = s.trim();
        let bad = || format!("invalid blocker `{s}`: expected N, #N, or OWNER/REPO#N");
        let (repo, num) = match t.split_once('#') {
            Some((r, n)) => (r, n),
            None => ("", t),
        };
        let number: u64 = num.parse().map_err(|_| bad())?;
        if repo.is_empty() {
            return Ok(Self::local(number));
        }
        let well_formed = repo.split_once('/').is_some_and(|(o, r)| {
            let ok = |x: &str| {
                !x.is_empty()
                    && x.chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
            };
            ok(o) && ok(r)
        });
        if !well_formed {
            return Err(bad());
        }
        Ok(Self {
            repo: Some(repo.to_string()),
            number,
        })
    }
}

/// One park record: one declared blocker, with the provenance of the park.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParkRecord {
    /// The declared blocker. `None` is an explicit "blocker unstated".
    pub blocker: Option<BlockerRef>,
    /// `by=` — the role or identity that applied the park.
    pub by: Option<String>,
    /// `at=` — when, as written. Never re-derived: a timestamp inside text is
    /// evidence of intent, not a liveness signal (see `lease-record.md`).
    pub at: Option<String>,
    /// `reason="…"` — free text, already unescaped.
    pub reason: Option<String>,
}

/// `<!-- loom:park …( -->)` spans, captured lazily.
///
/// Non-greedy up to the first `-->`, which is exact rather than approximate: an
/// HTML comment cannot contain `--`, so the first `-->` after the sentinel is
/// always this comment's own terminator. [`sanitize_reason`] is what keeps a
/// rendered record inside that rule.
fn marker_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?s)<!--\s*loom:park\s+(.*?)-->").expect("static park-marker pattern")
    })
}

/// `reason="…"`, non-greedy so a later `"` on the same line cannot swallow the
/// rest of the marker.
fn reason_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"reason="([^"]*)""#).expect("static park-reason pattern"))
}

/// `key=value` for the unquoted provenance attributes.
fn attr_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\b(by|at)=([^\s]+)").expect("static park-attr pattern"))
}

/// Every `#N`, optionally qualified as `OWNER/REPO#N` (#10443). A bare `#N`
/// captures no repo, so records written before qualification parse unchanged.
fn ref_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?:([A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+))?#([0-9]+)")
            .expect("static park-ref pattern")
    })
}

/// Whether `text` carries at least one park record.
///
/// Deliberately keyed on [`parse`] rather than on the sentinel substring: a
/// truncated `<!-- loom:park` with no terminator is not a record, and treating
/// it as one would report a park as documented when nothing can read it.
#[must_use]
pub fn has_record(text: &str) -> bool {
    !parse(text).is_empty()
}

/// Every park record in `text`, in document order, one per declared blocker.
///
/// More than one is legitimate and expected: a multi-blocker park is written as
/// several records, and a second park applied later (by a different role, for a
/// different blocker) appends its own rather than editing someone else's.
///
/// A marker quoted inside a fenced block or an inline code span is not a
/// record: it renders as visible text, not a comment (#10837, [`code`]).
#[must_use]
pub fn parse(text: &str) -> Vec<ParkRecord> {
    markers(text)
        .into_iter()
        .flat_map(|(_, inner)| parse_inner(&text[inner]))
        .collect()
}

/// The `(whole, interior)` byte ranges of every park marker outside code,
/// in document order. Offsets index `text` itself ([`code::blank`] keeps them).
fn markers(text: &str) -> Vec<(Range<usize>, Range<usize>)> {
    let masked = code::blank(text);
    marker_re()
        .captures_iter(&masked)
        .filter_map(|c| Some((c.get(0)?.range(), c.get(1)?.range())))
        .collect()
}

/// Parse one marker's interior (everything between the sentinels) into one
/// record per reference it names — or exactly one `blocker: None` record when it
/// names none.
fn parse_inner(inner: &str) -> Vec<ParkRecord> {
    let reason = reason_re()
        .captures(inner)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string());

    // Strip the quoted reason span BEFORE extracting references — see the module
    // docs: a `#N` inside prose is a mention, never a declaration.
    let without_reason = reason_re().replace_all(inner, "");

    let mut by = None;
    let mut at = None;
    for c in attr_re().captures_iter(&without_reason) {
        let (Some(k), Some(v)) = (c.get(1), c.get(2)) else {
            continue;
        };
        match k.as_str() {
            "by" => by = Some(v.as_str().to_string()),
            "at" => at = Some(v.as_str().to_string()),
            _ => {}
        }
    }

    let refs: Vec<BlockerRef> = ref_re()
        .captures_iter(&without_reason)
        .filter_map(|c| {
            Some(BlockerRef {
                repo: c.get(1).map(|m| m.as_str().to_string()),
                number: c.get(2)?.as_str().parse().ok()?,
            })
        })
        .collect();

    let template = ParkRecord {
        blocker: None,
        by,
        at,
        reason,
    };

    if refs.is_empty() {
        return vec![template];
    }

    // Deduplicate within one marker while keeping first-seen order, so a
    // hand-written `#9, #9` does not become two records.
    let mut seen = BTreeSet::new();
    refs.into_iter()
        .filter(|r| seen.insert(r.clone()))
        .map(|r| ParkRecord {
            blocker: Some(r),
            ..template.clone()
        })
        .collect()
}

/// `text` with every qualified `OWNER/REPO#N` inside a park marker's blocker
/// span reduced to nothing, so prose extractors that only understand `#N`
/// cannot resolve a cross-repo blocker against the local repo (#10443).
/// Text outside park markers is untouched.
#[must_use]
pub fn mask_qualified(text: &str) -> String {
    marker_re()
        .replace_all(text, |c: &regex::Captures<'_>| {
            let whole = c.get(0).map_or("", |m| m.as_str());
            let masked = ref_re().replace_all(whole, |r: &regex::Captures<'_>| {
                if r.get(1).is_some() {
                    String::new()
                } else {
                    r.get(0).map_or(String::new(), |m| m.as_str().to_string())
                }
            });
            masked.into_owned()
        })
        .into_owned()
}

/// The union of every record's declared blockers, sorted and deduplicated.
#[must_use]
pub fn blockers(text: &str) -> Vec<BlockerRef> {
    let set: BTreeSet<BlockerRef> = parse(text).into_iter().filter_map(|r| r.blocker).collect();
    set.into_iter().collect()
}

/// Whether any park record in `text` names a **qualified** (`OWNER/REPO#N`)
/// blocker (#10556).
///
/// Read through [`parse`], so it agrees with [`blockers`] exactly: a mention
/// inside `reason="…"` or outside any marker does not count. A caller that
/// acts only on local blockers (the #10556 release pass) must skip a park
/// carrying one rather than let it drop out of a local-only check.
#[must_use]
pub fn has_qualified_ref(text: &str) -> bool {
    parse(text)
        .iter()
        .any(|r| r.blocker.as_ref().is_some_and(|b| b.repo.is_some()))
}

/// `body` with every park record naming a **local** blocker in `resolved`
/// removed, and every other record kept (#10556's re-park).
///
/// `resolved` holds local issue/PR numbers; a qualified `OWNER/REPO#N` record
/// is never matched by them, so it is always kept.
///
/// A marker naming only resolved blockers is dropped — with its whole line
/// when it stood alone on one. A hand-written multi-reference marker naming a
/// mix is re-rendered as one record per still-open blocker, keeping its
/// provenance. Everything else in the body is left byte-for-byte.
#[must_use]
pub fn drop_blockers(body: &str, resolved: &[u64]) -> String {
    let mut out = String::with_capacity(body.len());
    let mut last = 0;
    // A quoted marker in code is never a record, so never edited (#10837).
    for (whole, inner) in markers(body) {
        let records = parse_inner(&body[inner]);
        let keep: Vec<ParkRecord> = records
            .iter()
            .filter(|r| {
                r.blocker
                    .as_ref()
                    .is_none_or(|b| b.repo.is_some() || !resolved.contains(&b.number))
            })
            .cloned()
            .collect();
        if keep.len() == records.len() {
            continue;
        }
        let (mut start, mut end) = (whole.start, whole.end);
        let replacement = keep.iter().map(render).collect::<Vec<_>>().join("\n");
        if replacement.is_empty() {
            // Take the whole line when the marker was alone on it.
            let line_start = body[..start].rfind('\n').map_or(0, |i| i + 1);
            let line_end = body[end..].find('\n').map_or(body.len(), |i| end + i + 1);
            let alone =
                body[line_start..start].trim().is_empty() && body[end..line_end].trim().is_empty();
            if alone && line_start >= last {
                (start, end) = (line_start, line_end);
            }
        }
        out.push_str(&body[last..start]);
        out.push_str(&replacement);
        last = end;
    }
    out.push_str(&body[last..]);
    out
}

/// Render one park record as the one line a role writes into the artifact body.
#[must_use]
pub fn render(record: &ParkRecord) -> String {
    let reference = match &record.blocker {
        Some(b) => b.to_string(),
        None => UNSTATED.to_string(),
    };

    let mut out = format!("{MARKER_OPEN}{RENDERED_PHRASE} {reference}");
    if let Some(by) = &record.by {
        out.push_str(&format!(" by={}", sanitize_attr(by)));
    }
    if let Some(at) = &record.at {
        out.push_str(&format!(" at={}", sanitize_attr(at)));
    }
    if let Some(reason) = &record.reason {
        let clean = sanitize_reason(reason);
        if !clean.is_empty() {
            out.push_str(&format!(" reason=\"{clean}\""));
        }
    }
    out.push_str(&format!(" {MARKER_CLOSE}"));
    out
}

/// Render a whole park — one line per blocker, newline-joined, no trailing
/// newline. An empty `blockers` list renders the single `(unstated)` record.
#[must_use]
pub fn render_park(
    blockers: &[BlockerRef],
    by: Option<&str>,
    at: Option<&str>,
    reason: Option<&str>,
) -> String {
    let template = ParkRecord {
        blocker: None,
        by: by.map(str::to_string),
        at: at.map(str::to_string),
        reason: reason.map(str::to_string),
    };
    if blockers.is_empty() {
        return render(&template);
    }
    let mut seen = BTreeSet::new();
    blockers
        .iter()
        .filter(|b| seen.insert((*b).clone()))
        .map(|b| {
            render(&ParkRecord {
                blocker: Some(b.clone()),
                ..template.clone()
            })
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Collapse an unquoted attribute value to a single whitespace-free token.
///
/// `by=` / `at=` are parsed as "up to the next space", so a value containing one
/// would silently truncate. Whitespace becomes `_`; `"` and any `-` run that
/// could terminate the comment are removed by [`strip_comment_hazards`].
fn sanitize_attr(value: &str) -> String {
    let joined = value.split_whitespace().collect::<Vec<_>>().join("_");
    strip_comment_hazards(&joined).replace('"', "")
}

/// Make a free-text reason safe to embed: one line, no `"`, no `--`.
///
/// An HTML comment cannot contain `--`, so a reason quoting a CLI flag
/// (`--force`) would terminate the marker early and leave the rest of the reason
/// as visible body text — corrupting both the record and the artifact. Rather
/// than escape it (there is no escape inside an HTML comment), the hazard is
/// removed.
fn sanitize_reason(reason: &str) -> String {
    let one_line = reason.split_whitespace().collect::<Vec<_>>().join(" ");
    strip_comment_hazards(&one_line).replace('"', "'")
}

/// Collapse every run of two or more `-` to a single `-`.
fn strip_comment_hazards(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_dash = false;
    for ch in s.chars() {
        if ch == '-' {
            if prev_dash {
                continue;
            }
            prev_dash = true;
        } else {
            prev_dash = false;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests;
