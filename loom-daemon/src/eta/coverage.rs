//! ETA authority coverage against the fleet roster (#10897, Slice 1).
//!
//! The ETA pass lists PRs through a per-repo local workspace, so the
//! authority (#10498) can only emit ETAs for the repos *it* manages. When the
//! authority resolves to a host that manages 2 of 30 roster repos, every other
//! host correctly stops emitting and coverage collapses silently. This module
//! is the pure half of the guard:
//!
//! * [`coverage`] compares the roster with the slugs a pass covered.
//! * [`fallback_scope`] decides which roster repos a **non-authority** host
//!   keeps emitting for, because the authority is not known to cover them.
//!
//! The roster is the cached fleet-store `repos.yml`
//! ([`crate::eta::roster_history`]); nothing here reads the network. An
//! unknown (absent or empty) roster is [`State::Unknown`]: no signal, no
//! change to gating.
//!
//! A non-authority host cannot learn the authority's managed repos (see
//! [`crate::eta::authority`]), so the committed key `fleet.etaAuthorityCovers`
//! declares them: `"all"` (the authority manages every roster repo: the
//! healthy case, non-authority hosts stay silent) or a list of `owner/repo`
//! slugs. Undeclared means unverifiable, and the conservative rule applies:
//! duplicates beat ETAs missing for most of the fleet.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::eta::repo_priority::FleetMember;

/// How many missing slugs a log line / doctor line names.
pub const NAMED_MISSING: usize = 5;

/// Whether a pass covered the roster.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum State {
    /// No roster to compare against (none configured, cache cold, empty).
    #[default]
    Unknown,
    /// Every roster repo is covered.
    Full,
    /// At least one roster repo is not covered.
    Short,
}

/// A pass's coverage of the fleet roster.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Coverage {
    /// [`State::Unknown`] / [`State::Full`] / [`State::Short`].
    pub state: State,
    /// Distinct roster repos with a slug.
    pub roster: usize,
    /// How many of them were covered (repos outside the roster do not count).
    pub covered: usize,
    /// Roster slugs (lowercase, sorted) that were not covered.
    pub missing: Vec<String>,
}

fn roster_slugs(members: &[FleetMember]) -> BTreeSet<String> {
    members
        .iter()
        .filter_map(|m| m.repo.as_deref())
        .map(str::to_ascii_lowercase)
        .collect()
}

/// `members` against the slugs a pass `covered`. Slug match is
/// case-insensitive, a member with no slug is ignored, an empty roster is
/// [`State::Unknown`], and covered repos outside the roster are not counted.
#[must_use]
pub fn coverage<S: AsRef<str>>(members: &[FleetMember], covered: &[S]) -> Coverage {
    let roster = roster_slugs(members);
    if roster.is_empty() {
        return Coverage {
            state: State::Unknown,
            roster: 0,
            covered: 0,
            missing: Vec::new(),
        };
    }
    let have: BTreeSet<String> = covered
        .iter()
        .map(|s| s.as_ref().trim().to_ascii_lowercase())
        .collect();
    let missing: Vec<String> = roster.difference(&have).cloned().collect();
    Coverage {
        state: if missing.is_empty() {
            State::Full
        } else {
            State::Short
        },
        roster: roster.len(),
        covered: roster.len() - missing.len(),
        missing,
    }
}

impl Coverage {
    /// `k of n roster repos; missing: a, b (+m more)`.
    #[must_use]
    pub fn describe(&self) -> String {
        match self.state {
            State::Unknown => "roster unknown (no fleet.repo or cold cache)".to_string(),
            State::Full => format!("{} of {} roster repos", self.covered, self.roster),
            State::Short => {
                let named: Vec<&str> = self
                    .missing
                    .iter()
                    .take(NAMED_MISSING)
                    .map(String::as_str)
                    .collect();
                let more = self.missing.len().saturating_sub(NAMED_MISSING);
                let tail = if more > 0 {
                    format!(" (+{more} more)")
                } else {
                    String::new()
                };
                format!(
                    "{} of {} roster repos; missing: {}{tail}",
                    self.covered,
                    self.roster,
                    named.join(", ")
                )
            }
        }
    }
}

/// The repos the last authority pass covered, persisted so `eta doctor`
/// (a separate process) can report them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastPass {
    /// The host that ran the pass.
    pub host: String,
    /// Lowercase slugs the pass listed.
    pub covered: Vec<String>,
}

/// Where [`LastPass`] lives.
#[must_use]
pub fn last_pass_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".loom/state/eta/authority-coverage.json")
}

/// Persist `pass`, best effort (a failure only costs the doctor its view).
pub fn write_last_pass(workspace_root: &Path, pass: &LastPass) {
    let Ok(text) = serde_json::to_string(pass) else {
        return;
    };
    let path = last_pass_path(workspace_root);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(error) = crate::eta::health::write_atomic(&path, &text) {
        log::debug!("eta: writing {} failed: {error}", path.display());
    }
}

/// The persisted [`LastPass`]; `None` when absent or unreadable.
#[must_use]
pub fn read_last_pass(workspace_root: &Path) -> Option<LastPass> {
    let text = std::fs::read_to_string(last_pass_path(workspace_root)).ok()?;
    serde_json::from_str(&text).ok()
}

/// What `fleet.etaAuthorityCovers` declares about the authority's repos.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Declared {
    /// Nothing declared: the authority's coverage is unverifiable.
    #[default]
    Undeclared,
    /// `"all"`: the authority covers the whole roster.
    All,
    /// The listed slugs (lowercase).
    Repos(BTreeSet<String>),
}

/// Parse the `fleet.etaAuthorityCovers` value: `"all"`, or an array of
/// `owner/repo` strings. Anything else (null, blank, wrong type, an empty
/// array) is [`Declared::Undeclared`].
#[must_use]
pub fn parse_declared(value: Option<&Value>) -> Declared {
    match value {
        Some(Value::String(s)) if s.trim().eq_ignore_ascii_case("all") => Declared::All,
        Some(Value::Array(items)) => {
            let repos: BTreeSet<String> = items
                .iter()
                .filter_map(Value::as_str)
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect();
            if repos.is_empty() {
                Declared::Undeclared
            } else {
                Declared::Repos(repos)
            }
        }
        _ => Declared::Undeclared,
    }
}

/// The roster repos a **non-authority** host keeps emitting for. `None`:
/// stay silent (#10498 behaviour): the roster is unknown, or the authority
/// is declared to cover everything. `Some(scope)`: the host acts as an
/// emitter for exactly `scope`, the roster repos not declared covered, which
/// it then intersects with the repos it can list.
#[must_use]
pub fn fallback_scope(
    roster: Option<&[FleetMember]>,
    declared: &Declared,
) -> Option<BTreeSet<String>> {
    let roster = roster_slugs(roster?);
    let scope: BTreeSet<String> = match declared {
        Declared::All => return None,
        Declared::Undeclared => roster,
        Declared::Repos(covered) => roster.difference(covered).cloned().collect(),
    };
    (!scope.is_empty()).then_some(scope)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn member(repo: Option<&str>) -> FleetMember {
        FleetMember {
            repo: repo.map(str::to_string),
            priority: 50,
        }
    }

    fn roster(slugs: &[&str]) -> Vec<FleetMember> {
        slugs.iter().map(|s| member(Some(s))).collect()
    }

    #[test]
    fn two_of_five_is_short_and_names_the_three_missing() {
        let r = roster(&["a/one", "a/two", "a/three", "a/four", "a/five"]);
        let c = coverage(&r, &["a/one", "a/two"]);
        assert_eq!(c.state, State::Short);
        assert_eq!((c.roster, c.covered), (5, 2));
        assert_eq!(c.missing, vec!["a/five", "a/four", "a/three"]);
        assert_eq!(c.describe(), "2 of 5 roster repos; missing: a/five, a/four, a/three");
    }

    #[test]
    fn full_coverage_is_full() {
        let r = roster(&["a/one", "a/two"]);
        let c = coverage(&r, &["a/two", "a/one"]);
        assert_eq!(c.state, State::Full);
        assert!(c.missing.is_empty());
    }

    #[test]
    fn slug_match_is_case_insensitive() {
        let r = roster(&["org/repo"]);
        assert_eq!(coverage(&r, &["Org/Repo"]).state, State::Full);
    }

    #[test]
    fn a_member_without_a_slug_is_ignored() {
        let r = vec![member(None), member(Some("a/one"))];
        let c = coverage(&r, &["a/one"]);
        assert_eq!((c.state, c.roster), (State::Full, 1));
    }

    #[test]
    fn an_empty_or_slugless_roster_is_unknown() {
        assert_eq!(coverage::<&str>(&[], &["a/one"]).state, State::Unknown);
        assert_eq!(coverage(&[member(None)], &["a/one"]).state, State::Unknown);
    }

    #[test]
    fn extra_covered_repos_outside_the_roster_do_not_count() {
        let r = roster(&["a/one", "a/two"]);
        let c = coverage(&r, &["a/one", "x/extra", "x/other"]);
        assert_eq!((c.state, c.covered, c.roster), (State::Short, 1, 2));
        assert_eq!(c.missing, vec!["a/two"]);
    }

    #[test]
    fn describe_caps_the_named_slugs() {
        let r = roster(&["a/1", "a/2", "a/3", "a/4", "a/5", "a/6", "a/7"]);
        let line = coverage::<&str>(&r, &[]).describe();
        assert!(line.contains("(+2 more)"), "{line}");
    }

    #[test]
    fn the_last_pass_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_last_pass(dir.path()), None);
        let pass = LastPass {
            host: "h".into(),
            covered: vec!["a/one".into()],
        };
        write_last_pass(dir.path(), &pass);
        assert_eq!(read_last_pass(dir.path()), Some(pass));
    }

    #[test]
    fn declared_parses_all_lists_and_junk() {
        assert_eq!(parse_declared(Some(&json!("ALL"))), Declared::All);
        assert_eq!(
            parse_declared(Some(&json!(["A/One", " "]))),
            Declared::Repos(["a/one".to_string()].into())
        );
        for junk in [json!(null), json!(""), json!([]), json!(3), json!("some")] {
            assert_eq!(parse_declared(Some(&junk)), Declared::Undeclared);
        }
        assert_eq!(parse_declared(None), Declared::Undeclared);
    }

    #[test]
    fn a_non_authority_host_is_silent_when_the_roster_is_unknown() {
        assert_eq!(fallback_scope(None, &Declared::Undeclared), None);
        assert_eq!(fallback_scope(Some(&[]), &Declared::Undeclared), None);
    }

    #[test]
    fn a_non_authority_host_is_silent_when_the_authority_covers_all() {
        let r = roster(&["a/one", "a/two"]);
        assert_eq!(fallback_scope(Some(&r), &Declared::All), None);
    }

    #[test]
    fn a_non_authority_host_emits_what_the_authority_does_not_cover() {
        let r = roster(&["a/one", "a/two", "a/three"]);
        let declared = Declared::Repos(["a/one".to_string(), "a/two".to_string()].into());
        let scope = fallback_scope(Some(&r), &declared).unwrap();
        assert_eq!(scope, ["a/three".to_string()].into());
        // Everything declared: nothing left, silent.
        let all = Declared::Repos(["a/one", "a/two", "a/three"].map(String::from).into());
        assert_eq!(fallback_scope(Some(&r), &all), None);
    }

    #[test]
    fn an_unverifiable_authority_leaves_every_roster_repo_in_scope() {
        let r = roster(&["a/one", "A/Two"]);
        let scope = fallback_scope(Some(&r), &Declared::Undeclared).unwrap();
        assert_eq!(scope.len(), 2);
    }
}
