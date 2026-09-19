//! The bot-PR verdict: does this PR qualify for the criterion #1 / #3 waivers?
//! (#4765)
//!
//! Every disqualification is a *named* reason, not a bare `false`, because the
//! Champion prompt quotes it back into the PR comment it posts. A reason an
//! operator can read is what makes "this did not auto-merge" debuggable
//! instead of mysterious.

use std::collections::HashMap;

use super::config::BotPrConfig;
use super::manifest;
use super::semver_guard::{self, BumpLevel, SemverVerdict};

/// Everything the verdict is a function of. No field is fetched here — the
/// caller owns every forge read, so this stays pure and testable.
#[derive(Debug, Clone, Default)]
pub struct ClassifyInput {
    /// `author.login` exactly as the forge reported it.
    pub author: String,
    pub title: String,
    pub body: String,
    /// The **authoritative** changed-file list, from the paginated REST
    /// endpoint. Never `gh pr view --json files` (truncates at 100, #4613).
    pub files: Vec<String>,
    /// `path -> patch`, needed only for the workflow version-pin carve-out.
    pub patches: HashMap<String, String>,
}

/// Why a PR is not in the trusted-bot dependency class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Disqualified {
    /// `champion.autoMergeDependabot` is not enabled. The default, and the
    /// only reason that says nothing at all about the PR.
    Disabled,
    UntrustedAuthor {
        author: String,
    },
    /// A PR with no changed files cannot be shown to be dependency-only.
    NoFiles,
    NonManifestFile {
        path: String,
    },
    WorkflowNotVersionPinOnly {
        path: String,
    },
    /// A workflow file is in the list but the diff carried no patch for it, so
    /// the pin-only carve-out cannot be evaluated. Fails closed (#4613's
    /// lesson: never assert a file-list claim you did not actually read).
    WorkflowPatchUnavailable {
        path: String,
    },
    SemverExceeded {
        level: BumpLevel,
        max: &'static str,
    },
    SemverUnparseable {
        max: &'static str,
    },
}

impl Disqualified {
    /// Stable machine-readable token, emitted as `BOT_PR_REASON`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::UntrustedAuthor { .. } => "untrusted-author",
            Self::NoFiles => "no-files",
            Self::NonManifestFile { .. } => "non-manifest-file",
            Self::WorkflowNotVersionPinOnly { .. } => "workflow-not-version-pin-only",
            Self::WorkflowPatchUnavailable { .. } => "workflow-patch-unavailable",
            Self::SemverExceeded { .. } => "semver-exceeded",
            Self::SemverUnparseable { .. } => "semver-unparseable",
        }
    }

    /// One sentence for the PR comment.
    #[must_use]
    pub fn explain(&self) -> String {
        match self {
            Self::Disabled => {
                "champion.autoMergeDependabot is not enabled for this repository".to_string()
            }
            Self::UntrustedAuthor { author } => {
                format!("PR author {author:?} is not in champion.trustedBotAuthors")
            }
            Self::NoFiles => "the PR reports no changed files".to_string(),
            Self::NonManifestFile { path } => format!(
                "{path} is not a dependency manifest or lockfile, so the diff is not \
                 dependency-only"
            ),
            Self::WorkflowNotVersionPinOnly { path } => {
                format!("{path} changes more than a `uses:` version pin")
            }
            Self::WorkflowPatchUnavailable { path } => format!(
                "no diff was available for {path}, so its `uses:`-pin-only status could not \
                 be verified"
            ),
            Self::SemverExceeded { level, max } => {
                format!("a {} bump exceeds champion.dependabotMaxSemver = {max}", level.as_str())
            }
            Self::SemverUnparseable { max } => format!(
                "champion.dependabotMaxSemver = {max} is set but no `from X to Y` version \
                 pair could be parsed from the title or body"
            ),
        }
    }
}

/// A qualifying PR, and what qualifying buys it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Qualified {
    /// The worst bump found, when the semver guard evaluated one.
    pub bump_level: Option<BumpLevel>,
    /// Which criteria the Champion may waive. **Deliberately only these two.**
    pub waived: Vec<&'static str>,
}

/// What a qualifying PR waives, and nothing else. Criterion #1 (Judge approval
/// via `loom:pr`), #2 (merge-risk judgment), #4 (mergeable) and #6 (CI green)
/// stay hard requirements — CI is the actual safety gate for a version bump,
/// which is the whole premise this feature rests on.
///
/// Issue #4765 also asked for the **size** ceiling to be waived. There is
/// nothing left to waive: `champion.auto_merge_max_lines` was retired before
/// this landed (`champion-pr-merge.md` → criterion #2's migration note), and
/// that criterion already forbids justifying a hold by a line count. Naming a
/// waiver for a gate that does not exist would put a false claim in every
/// pre-merge comment, so it is deliberately absent.
pub const WAIVED_CRITERIA: &[&str] = &["critical-file", "recency-doctor-route"];

/// Classify a PR. `Ok` means the waivers apply; `Err` names why they do not.
///
/// # Errors
///
/// Returns [`Disqualified`] — a verdict, never an operational failure.
pub fn classify(cfg: &BotPrConfig, input: &ClassifyInput) -> Result<Qualified, Disqualified> {
    if !cfg.enabled {
        return Err(Disqualified::Disabled);
    }
    if !cfg.trusts(&input.author) {
        return Err(Disqualified::UntrustedAuthor {
            author: input.author.clone(),
        });
    }

    let files: Vec<&String> = input
        .files
        .iter()
        .filter(|f| !f.trim().is_empty())
        .collect();
    if files.is_empty() {
        return Err(Disqualified::NoFiles);
    }

    for path in &files {
        let path = path.trim();
        if manifest::is_dependency_manifest(path) {
            continue;
        }
        if manifest::is_workflow(path) {
            let Some(patch) = input.patches.get(path) else {
                return Err(Disqualified::WorkflowPatchUnavailable {
                    path: path.to_string(),
                });
            };
            if manifest::workflow_diff_is_version_pin_only(patch) {
                continue;
            }
            return Err(Disqualified::WorkflowNotVersionPinOnly {
                path: path.to_string(),
            });
        }
        return Err(Disqualified::NonManifestFile {
            path: path.to_string(),
        });
    }

    let bump_level = match semver_guard::evaluate(cfg.max_semver, &input.title, &input.body) {
        SemverVerdict::NotEvaluated => None,
        SemverVerdict::Within(level) => Some(level),
        SemverVerdict::Exceeded(level) => {
            return Err(Disqualified::SemverExceeded {
                level,
                max: cfg.max_semver.as_str(),
            })
        }
        SemverVerdict::Unparseable => {
            return Err(Disqualified::SemverUnparseable {
                max: cfg.max_semver.as_str(),
            })
        }
    };

    Ok(Qualified {
        bump_level,
        waived: WAIVED_CRITERIA.to_vec(),
    })
}

#[cfg(test)]
mod tests;
