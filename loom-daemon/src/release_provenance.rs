//! Is this binary an official release build? (#10718, operator ruling of
//! 2026-10-08: only official release builds may push Loom updates into fleet
//! repos.)
//!
//! A daemon resyncs a repo's installed Loom from the payload it embeds, so
//! whoever built the binary chose the files. "Built from a clean checkout" is
//! not enough: a clean build of any feature branch carries its base's version
//! and that branch's `defaults/`. Two things must both hold.
//!
//! # (a) The build stamp
//!
//! `build_stamp.rs` bakes `LOOM_RELEASE_BUILD_TAG` into the binary as
//! [`BUILT_RELEASE_TAG`]. Only `.github/workflows/release.yml` sets that
//! variable, and only on a run that publishes. A developer's build, a feature
//! branch's, CI's test and release-check jobs and `scripts/daemon-build.sh`
//! leave it unset, so they bake in the empty string. The stamp must equal
//! `v<crate version>`, the source commit must be known, and the tree clean.
//!
//! # (b) The tag check
//!
//! The stamp is an environment variable, so anyone can set it. Once per
//! process the daemon therefore asks the forge what commit tag `v<version>`
//! of [`RELEASE_REPO`] names (peeling an annotated tag) and compares it with
//! the commit this binary was built from. The answer is cached for the life
//! of the process.
//!
//! The lookup itself is the caller's ([`TagLookup`]; production passes
//! `release_fetch::source::resolve_tag_commit` over `gh api`). This module
//! reaches no forge code, so the installer's payload gate, which reads
//! [`is_verified`], does not either.
//!
//! * equal: [`Provenance::Verified`], final.
//! * different: [`Provenance::Mismatch`], final. Not a release build.
//! * no answer (network, `gh`, a tag that does not exist yet):
//!   [`Provenance::Unverified`]. Nothing is pushed, and the lookup is retried
//!   on a later tick, one sync interval apart at first, doubling, capped at
//!   [`RETRY_CAP`].
//!
//! Until the answer is `Verified`, [`is_verified`] is false, so
//! `init::payload::Stamp::this_binary` reports `release_build: false` and
//! both the #10878 resync gate and the workspace pass's host gate refuse.

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use chrono::{DateTime, Utc};

/// Resolve a tag of a repository (`OWNER/REPO`, tag name) to the full commit
/// it names. `Err` means "no answer", never "a different commit".
pub type TagLookup<'a> = dyn Fn(&str, &str) -> Result<String, String> + 'a;

/// The repository whose tags are releases.
pub const RELEASE_REPO: &str = "rjwalters/loom";

/// The marker the stamp is embedded in, so `release.yml` can check a built
/// artifact (including a cross-compiled one it cannot run) with `grep -a`.
const MARKER: &str = concat!("loom-release-build-tag=", env!("LOOM_DAEMON_RELEASE_TAG"), ";");

/// Longest wait between two tag lookups that got no answer.
pub const RETRY_CAP: Duration = Duration::from_secs(60 * 60);

/// The release tag this binary was stamped with at build time; empty for
/// every build `release.yml` did not make.
#[must_use]
pub fn built_release_tag() -> &'static str {
    MARKER
        .strip_prefix("loom-release-build-tag=")
        .and_then(|rest| rest.strip_suffix(';'))
        .unwrap_or("")
}

/// What is known about this binary's provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provenance {
    /// (a) failed: the binary does not carry a release stamp for its own
    /// version. Final; the text says which part is missing.
    NotStamped(String),
    /// (a) holds, (b) has no answer yet. Nothing is pushed meanwhile.
    Unverified {
        /// Why the last lookup got no answer; empty before the first one.
        why: String,
        /// When the lookup is tried again; `None` before the first one.
        retry_at: Option<DateTime<Utc>>,
    },
    /// (b) answered: the tag names a different commit. Final.
    Mismatch {
        /// The commit the release tag names.
        tag_commit: String,
    },
    /// Both hold. Final.
    Verified,
}

impl Provenance {
    /// A final "no": this process will never be a release build.
    #[must_use]
    pub fn refuted(&self) -> bool {
        matches!(self, Self::NotStamped(_) | Self::Mismatch { .. })
    }
}

impl std::fmt::Display for Provenance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotStamped(why) => write!(f, "not a release build: {why}"),
            Self::Unverified { why, .. } if why.is_empty() => {
                write!(f, "release tag not checked yet")
            }
            Self::Unverified { why, .. } => write!(f, "release tag not verified: {why}"),
            Self::Mismatch { tag_commit } => write!(
                f,
                "not a release build: the release tag names commit {tag_commit}, not the one \
                 this binary was built from"
            ),
            Self::Verified => write!(f, "official release build"),
        }
    }
}

/// Check (a). Pure. `Ok` carries the tag to look up.
///
/// # Errors
/// The part of the stamp that is missing or wrong.
pub fn stamp_check(tag: &str, version: &str, commit: &str, tree: &str) -> Result<String, String> {
    if tag.is_empty() {
        return Err("it was not built by the release workflow".to_string());
    }
    let expected = format!("v{version}");
    if tag != expected {
        return Err(format!("it is stamped {tag}, but its version is {version}"));
    }
    if !(commit.len() == 40 && commit.bytes().all(|b| b.is_ascii_hexdigit())) {
        return Err("the build did not record its source commit".to_string());
    }
    if tree != "clean" {
        return Err(format!("it was built from a {tree} tree"));
    }
    Ok(expected)
}

/// One binary's provenance, and when (b) may next be asked. The process-wide
/// one is behind [`ensure`]; tests build their own.
#[derive(Debug)]
pub struct Checker {
    /// The tag to look up, or why (a) failed.
    stamp: Result<String, String>,
    /// The commit this binary was built from.
    built: String,
    outcome: Option<Provenance>,
    failures: u32,
    why: String,
    retry_at: Option<DateTime<Utc>>,
}

impl Checker {
    /// A checker for a binary with these build facts.
    #[must_use]
    pub fn new(tag: &str, version: &str, commit: &str, tree: &str) -> Self {
        Self {
            stamp: stamp_check(tag, version, commit, tree),
            built: commit.to_ascii_lowercase(),
            outcome: None,
            failures: 0,
            why: String::new(),
            retry_at: None,
        }
    }

    /// What is known now. Never asks the forge.
    #[must_use]
    pub fn current(&self) -> Provenance {
        if let Some(done) = &self.outcome {
            return done.clone();
        }
        match &self.stamp {
            Err(why) => Provenance::NotStamped(why.clone()),
            Ok(_) => Provenance::Unverified {
                why: self.why.clone(),
                retry_at: self.retry_at,
            },
        }
    }

    /// [`Self::current`], asking the forge first when (b) is still open and a
    /// lookup is due. At most one lookup per call, and none once the answer
    /// is final.
    pub fn ensure(
        &mut self,
        now: DateTime<Utc>,
        interval: Duration,
        lookup: &TagLookup<'_>,
    ) -> Provenance {
        let Ok(tag) = &self.stamp else {
            return self.current();
        };
        if self.outcome.is_some() || self.retry_at.is_some_and(|at| now < at) {
            return self.current();
        }
        match lookup(RELEASE_REPO, tag) {
            Ok(commit) if commit.eq_ignore_ascii_case(&self.built) => {
                self.outcome = Some(Provenance::Verified);
            }
            Ok(commit) => self.outcome = Some(Provenance::Mismatch { tag_commit: commit }),
            Err(why) => {
                self.failures = self.failures.saturating_add(1);
                let doublings = self.failures.saturating_sub(1).min(20);
                let wait = interval.saturating_mul(1u32 << doublings).min(RETRY_CAP);
                self.why = why;
                self.retry_at =
                    Some(now + chrono::Duration::from_std(wait).unwrap_or(chrono::Duration::MAX));
            }
        }
        self.current()
    }
}

fn this_binary() -> &'static Mutex<Checker> {
    static CELL: OnceLock<Mutex<Checker>> = OnceLock::new();
    CELL.get_or_init(|| {
        Mutex::new(Checker::new(
            built_release_tag(),
            env!("CARGO_PKG_VERSION"),
            crate::self_update::BUILT_COMMIT_FULL,
            crate::self_update::BUILT_TREE_STATE,
        ))
    })
}

fn with<T>(f: impl FnOnce(&mut Checker) -> T) -> T {
    let mut guard = match this_binary().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    f(&mut guard)
}

/// This binary's provenance as known now. Never asks the forge.
#[must_use]
pub fn current() -> Provenance {
    with(|c| c.current())
}

/// Is this binary a verified official release build? Never asks the forge:
/// false until [`ensure`] has had its answer.
#[must_use]
pub fn is_verified() -> bool {
    current() == Provenance::Verified
}

/// This binary's provenance, asking the forge when the tag check is still
/// open and due. Blocking for as long as `lookup` is.
pub fn ensure(now: DateTime<Utc>, interval: Duration, lookup: &TagLookup<'_>) -> Provenance {
    with(|c| c.ensure(now, interval, lookup))
}

#[cfg(test)]
#[path = "release_provenance_tests.rs"]
mod tests;
