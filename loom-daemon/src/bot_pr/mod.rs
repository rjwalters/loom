//! Champion's trusted-bot dependency-PR class (#4765).
//!
//! Dependabot PRs enter the Loom pipeline but are structurally unmergeable by
//! it: Champion's criterion #1 (size), #3 (critical-file exclusion) and #5
//! (recency) reject the entire class by construction. A lockfile diff is
//! machine-generated and huge; `Cargo.toml` / `package.json` are on the
//! critical-file blocklist, and *modifying a dependency manifest is the
//! definition* of a dependency PR; and the bumps queue up faster than the
//! pipeline drains them, so they age past 24h and get routed to Doctor, which
//! has no idea that `@dependabot rebase` is the correct refresh mechanism.
//!
//! The steady state is unpatched security advisories accumulating forever,
//! which is the opposite of what an autonomous-maintenance system should do.
//!
//! # What this module decides, and what it deliberately does not
//!
//! It answers exactly one question, deterministically: **is this PR a
//! trusted-bot, dependency-only change whose merge-blocking criteria may be
//! waived?** That is a pure function of (author, changed files, their diffs,
//! title/body version pairs, config) — no network, no clock, no judgement.
//!
//! It does **not** decide whether to merge. CI-green (criterion #6),
//! mergeable-state (#4) and Judge approval (`loom:pr`, #1) are unchanged hard
//! requirements, evaluated by `champion-pr-merge.md` exactly as before. This
//! module can only ever *waive the size ceiling and the critical-file
//! exclusion* for a PR it has proved is nothing but dependency-manifest
//! churn — the same two criteria that are meaningless as risk signals on a
//! generated lockfile diff.
//!
//! # Why it is Rust and not more prompt
//!
//! The predicate is the security boundary. "Every changed file is a
//! dependency manifest" is exactly the kind of claim an LLM asserts from
//! memory instead of from the file list — the confirmed mechanism behind the
//! #4613 false negative on criterion #3, where a Champion pass wrote "no
//! critical-file changes" about a PR that removed a workflow file. A
//! deterministic classifier that the prompt *invokes* and quotes cannot make
//! that mistake, and it is testable. See `defaults/docs/champion-bot-prs.md`
//! for the operator-facing contract.

pub mod classify;
pub mod config;
pub mod diff;
pub mod manifest;
pub mod render;
pub mod semver_guard;

pub use classify::{classify, ClassifyInput, Disqualified, Qualified};
pub use config::{BotPrConfig, MaxSemver};
