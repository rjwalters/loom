//! Input-scoped staleness for the required-check freshness guard (#8919).
//!
//! # Why the time rule alone is wrong in BOTH directions
//!
//! #8248's guard asks "did this green required check start before the base
//! branch's current tip?". On a busy `main` that is simultaneously too strict
//! and too lenient:
//!
//! - **Too strict.** `main` moved three times on 2026-09-25 (14:26, 14:47,
//!   14:58 UTC), each move landing inside a ~8 min CI run. PR #8641 lost the
//!   race three times on base moves that touched nothing any of its required
//!   checks read.
//! - **Too lenient.** A re-run does NOT re-test against the new base. GitHub
//!   re-runs a workflow run with the ORIGINAL `GITHUB_SHA`, and for a
//!   `pull_request` run that SHA is the test merge commit built when the event
//!   fired — built on the OLD base. Verified 2026-09-25 on run 36145858487
//!   (PR #8692): attempt 1 (14:12:11Z) and attempt 7 (16:26:47Z) both logged
//!   `HEAD is now at cb7c91f Merge 162b0f05… into 803f0c7d…`. Attempt 7 still
//!   tested base `803f0c7d` from 14:06Z, yet its `started_at` satisfied the
//!   time rule. So the 2026-09-18 incident (#8078 merging a green `File Size
//!   Ratchet` onto a baseline #8204 had tightened underneath it) can be
//!   reproduced through a re-run, with the time rule reporting FRESH.
//!
//! # The predicate
//!
//! Three inputs, all keyed on **the base the check actually tested** rather
//! than on when it ran:
//!
//! - **`B`** — the tested base, read from the run's own checkout log line
//!   (`Merge <head> into <B>`), with `<head>` required to match the PR head.
//!   `B` is invariant across re-runs, which is the whole point.
//! - **`D`** — the base-move diff, `compare/B...tip`, with **validated version
//!   restamps** removed (see [`super::evidence`]).
//! - **`P`** — the PR's own delta, `pulls/{n}/files`, including removed and
//!   renamed paths.
//!
//! Each component gate carries three path sets in [`SPECS`] (a required context
//! is one or more components — see [`RequiredCheck`]):
//!
//! - **`G`** — global inputs: the check's own scripts, its baseline / allowlist
//!   / budget files, and `.github/workflows/ci.yml`. A change to any of these
//!   can flip the verdict for *every* file.
//! - **`S`** — per-file scanned paths: the verdict for each such file depends
//!   only on that file's own content, plus `G`.
//! - **`C`** — coupled paths: cross-file aggregates (a SUM over a set) or links
//!   (one file naming another).
//!
//! A check is **stale** iff any of:
//!
//! | clause | meaning |
//! |---|---|
//! | `D∩G ≠ ∅` and `P∩(G∪S∪C) ≠ ∅` | main changed a global input and the PR has something for it to re-judge |
//! | `P∩G ≠ ∅` and `D∩(G∪S∪C) ≠ ∅` | the PR changed a global input and main has something for it to re-judge |
//! | `D∩P∩S ≠ ∅` | both sides touched the same scanned file |
//! | `D∩C ≠ ∅` and `P∩C ≠ ∅` | both sides touched the coupled set |
//! | the removal clause | a deletion/rename on one side while the other side touches the coupled set |
//!
//! **Why this is sound.** For a file outside `P`, the merged tree's content is
//! `main`'s; for a file outside `D`, it is the content the check tested. So a
//! check whose inputs do not interact between the two sides reaches, on the
//! merged tree, either the verdict it already reported or `main`'s own verdict
//! — and `main`'s own verdict is not this PR's business. That is the
//! **deliberate semantic shift**: the guard now asks "can merging *this* PR
//! turn the check red?" rather than "is `main` green?".
//!
//! # Failing closed
//!
//! - A required context with **no spec here** is stale whenever `D` is
//!   non-empty. Adding a step to a required job without updating this table
//!   therefore blocks merges rather than silently passing them — and the pin
//!   test in `tests.rs` fails first, at PR time.
//! - Missing `B`, `D` or `P` (a non-Actions check, an unreadable log, a
//!   failed/truncated/diverged compare, an unreadable `P`) falls back to
//!   #8248's `started_at` rule with a `Warning:` on stderr. Nothing is ever
//!   worse than today.
//! - Every set is safe to **over**-populate: every clause is monotone in
//!   `G`, `S` and `C`, so a path listed too broadly can only make the guard
//!   refuse more often.

use std::collections::{BTreeMap, BTreeSet};

/// `.github/workflows/ci.yml` is a global input to every required context:
/// it is where the job's steps, its runner and its path filters live.
pub const CI_WORKFLOW: &str = ".github/workflows/ci.yml";

/// One side's changed-path set — `D` (the base move) or `P` (the PR delta).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileSet {
    /// Every path the side touched. For a rename, BOTH the old and the new
    /// path are present: a link check cares about the name that disappeared
    /// just as much as the one that appeared.
    pub paths: BTreeSet<String>,
    /// The subset of `paths` that this side **removed** — a deletion, or the
    /// vacated old path of a rename. Drives the removal clause.
    pub removed: BTreeSet<String>,
}

impl FileSet {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    /// The first path (in sorted order, so the reason text is deterministic)
    /// matching any of `patterns`.
    fn first_match(&self, patterns: &[&str]) -> Option<&str> {
        self.paths
            .iter()
            .find(|p| patterns.iter().any(|pat| glob_match(pat, p)))
            .map(String::as_str)
    }
}

/// Build a [`FileSet`] from `(path, is_removal)` pairs — the shape both the
/// compare API and `pulls/{n}/files` project onto.
#[must_use]
pub fn file_set<'a, I: IntoIterator<Item = (&'a str, bool)>>(entries: I) -> FileSet {
    let mut set = FileSet::default();
    for (path, removed) in entries {
        set.paths.insert(path.to_string());
        if removed {
            set.removed.insert(path.to_string());
        }
    }
    set
}

/// The base a required check's run actually tested, plus the base-move diff
/// measured from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseMove {
    /// `B` — the base SHA the run's checkout log reported, as logged (GitHub
    /// abbreviates it in some renderings and the compare API accepts either).
    pub tested_base: String,
    /// `D` — `compare/B...tip`, with validated version restamps removed.
    pub files: FileSet,
}

/// The input-scoped evidence for one PR: `P`, plus `B`/`D` per required
/// context, plus why any context has none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScopedEvidence {
    /// `P` — the PR's own delta.
    pub pr_delta: FileSet,
    /// Per required context, the base it tested and the move since.
    pub base_moves: BTreeMap<String, BaseMove>,
    /// Per required context with no `base_moves` entry, why — surfaced as the
    /// `Warning:` that accompanies the fallback to the time rule.
    pub fallbacks: BTreeMap<String, String>,
}

/// One required context's input specification. See the module header for what
/// `global` / `scanned` / `coupled` mean and why over-population is safe.
#[derive(Debug, Clone, Copy)]
pub struct CheckSpec {
    /// The component gate's name: its former `ci.yml` job name, and the name its
    /// `# component:` marker uses (see [`RequiredCheck`]).
    pub context: &'static str,
    /// `G` — inputs whose change can flip the verdict for every file.
    pub global: &'static [&'static str],
    /// `S` — paths whose verdict depends only on their own content plus `G`.
    pub scanned: &'static [&'static str],
    /// `C` — cross-file aggregates and links.
    pub coupled: &'static [&'static str],
    /// Does a *deletion or rename* on one side, with the other side touching
    /// `C`, make this check stale? True for the link/parity checks, where the
    /// path that disappeared need not itself be in `C`.
    pub removal_sensitive: bool,
}

/// Why a check is stale under the input-scoped predicate — the operator-facing
/// half of the refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleReason {
    /// Which clause fired.
    pub clause: &'static str,
    /// A concrete path from `D` that participates, when the clause names one.
    pub base_path: Option<String>,
    /// A concrete path from `P` that participates, when the clause names one.
    pub pr_path: Option<String>,
}

impl std::fmt::Display for StaleReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.clause)?;
        if let Some(b) = &self.base_path {
            write!(f, " (base move touched `{b}`")?;
            match &self.pr_path {
                Some(p) => write!(f, "; this PR touches `{p}`)")?,
                None => write!(f, ")")?,
            }
        } else if let Some(p) = &self.pr_path {
            write!(f, " (this PR touches `{p}`)")?;
        }
        Ok(())
    }
}

/// Apply the input-scoped predicate: `None` = this check's tested verdict
/// still holds on the merged tree; `Some(reason)` = it does not.
#[must_use]
pub fn stale_reason(spec: &CheckSpec, d: &FileSet, p: &FileSet) -> Option<StaleReason> {
    if d.is_empty() {
        // The base has not moved (once validated restamps are discounted), so
        // the tree the check tested IS the tree it will merge onto.
        return None;
    }
    let any: Vec<&str> = spec
        .global
        .iter()
        .chain(spec.scanned.iter())
        .chain(spec.coupled.iter())
        .copied()
        .collect();

    // Clause 1: main changed a global input, and the PR has something for that
    // input to re-judge.
    if let (Some(b), Some(pp)) = (d.first_match(spec.global), p.first_match(&any)) {
        return Some(StaleReason {
            clause: "the base move changed a global input of this check",
            base_path: Some(b.to_string()),
            pr_path: Some(pp.to_string()),
        });
    }
    // Clause 2: the PR changed a global input, and main has something for it
    // to re-judge.
    if let (Some(pp), Some(b)) = (p.first_match(spec.global), d.first_match(&any)) {
        return Some(StaleReason {
            clause: "this PR changes a global input of this check and the base moved under it",
            base_path: Some(b.to_string()),
            pr_path: Some(pp.to_string()),
        });
    }
    // Clause 3: both sides touched the same per-file scanned path.
    if let Some(shared) = d.paths.iter().find(|path| {
        p.paths.contains(*path) && spec.scanned.iter().any(|pat| glob_match(pat, path))
    }) {
        return Some(StaleReason {
            clause: "the base move and this PR both touch the same scanned file",
            base_path: Some(shared.clone()),
            pr_path: Some(shared.clone()),
        });
    }
    // Clause 4: both sides touched the coupled (cross-file aggregate/link) set.
    if let (Some(b), Some(pp)) = (d.first_match(spec.coupled), p.first_match(spec.coupled)) {
        return Some(StaleReason {
            clause: "the base move and this PR both touch this check's coupled inputs",
            base_path: Some(b.to_string()),
            pr_path: Some(pp.to_string()),
        });
    }
    // Clause 5 (removal): a path vanished on one side while the other side
    // touches the linking files. The vanished path need not itself be linkable
    // — a deleted script breaks a doc link naming it.
    if spec.removal_sensitive {
        if let (Some(b), Some(pp)) =
            (d.removed.iter().next(), p.first_match(spec.coupled).map(str::to_string))
        {
            return Some(StaleReason {
                clause: "the base move deleted or renamed a path while this PR touches the \
                         linking files",
                base_path: Some(b.clone()),
                pr_path: Some(pp),
            });
        }
        if let (Some(pp), Some(b)) =
            (p.removed.iter().next(), d.first_match(spec.coupled).map(str::to_string))
        {
            return Some(StaleReason {
                clause: "this PR deletes or renames a path while the base move touches the \
                         linking files",
                base_path: Some(b),
                pr_path: Some(pp.clone()),
            });
        }
    }
    None
}

/// The refusal for a required context with no spec in [`SPECS`]: unknown
/// inputs plus a moved base is an unknown, and an unknown refuses.
#[must_use]
pub fn unknown_check_reason(d: &FileSet) -> Option<StaleReason> {
    d.paths.iter().next().map(|b| StaleReason {
        clause: "this required check has no entry in the input-scope table, so which files it \
                 reads is unknown",
        base_path: Some(b.clone()),
        pr_path: None,
    })
}

/// The spec for one component gate, or `None`.
#[must_use]
pub fn spec_for(component: &str) -> Option<&'static CheckSpec> {
    SPECS.iter().find(|s| s.context == component)
}

/// The component specs behind a required `context`: its [`REQUIRED_CHECKS`]
/// entry's components, or — for a name that is itself a component — just that
/// one spec. `None` when any component is unmapped, which
/// [`unknown_check_reason`] then turns into a refusal (fail closed).
#[must_use]
pub fn specs_for(context: &str) -> Option<Vec<&'static CheckSpec>> {
    match REQUIRED_CHECKS.iter().find(|r| r.context == context) {
        Some(req) => req.components.iter().map(|c| spec_for(c)).collect(),
        None => spec_for(context).map(|s| vec![s]),
    }
}

/// [`stale_reason`] for a required context made of `components`: the first
/// stale component (in table order) and why, or `None` when none is stale.
#[must_use]
pub fn composite_stale_reason(
    components: &[&'static CheckSpec],
    d: &FileSet,
    p: &FileSet,
) -> Option<(&'static str, StaleReason)> {
    components
        .iter()
        .find_map(|spec| stale_reason(spec, d, p).map(|r| (spec.context, r)))
}

// --- Shared pattern groups ---------------------------------------------------
//
// Named once so the table below reads as the policy rather than as a wall of
// globs, and so a set used by several checks cannot drift between them.

/// Every tracked shell script (the pool the shell gates measure and parse).
const SHELL: &[&str] = &["**/*.sh"];

/// Everything `check-file-size-budget.sh` measures (`git ls-files '*.rs' '*.sh'
/// '*.ts'`).
const SOURCE_FILES: &[&str] = &["**/*.rs", "**/*.sh", "**/*.ts"];

/// The agent-facing markdown + role JSON the prompt-size ratchets read.
const PROMPT_SURFACE: &[&str] = &[
    "CLAUDE.md",
    "AGENTS.md",
    "defaults/.loom/CLAUDE.md",
    "defaults/.loom/AGENTS.md",
    ".loom/CLAUDE.md",
    "defaults/roles/**",
    "defaults/.claude/commands/loom/**",
    "defaults/docs/**",
    ".loom/roles/**",
    ".loom/docs/**",
    ".claude/commands/loom/**",
];

/// The two mirrored trees the resync-parity gate pairs up.
const RESYNC_PAIRS: &[&str] = &[
    "defaults/hooks/**",
    "defaults/scripts/**",
    ".loom/hooks/**",
    ".loom/scripts/**",
];

/// Every tracked markdown file — the link graph's nodes.
const MARKDOWN: &[&str] = &["**/*.md"];

/// `main`'s required status-check contexts, as branch protection names them
/// (`.loom/config.json` `branchProtection.requiredStatusChecks`, applied by
/// `scripts/install/setup-branch-protection.sh`). The pin test asserts each has
/// a [`RequiredCheck`] and each names a real `ci.yml` job.
pub const REQUIRED_CONTEXTS: &[&str] = &[
    "Structural Checks",
    "Shell Syntax (macos-latest)",
    "Daemon Checks",
];

/// One required context and the component checks its job runs as steps.
///
/// #9065 folded 19 single-gate jobs into three, to stop ~20 five-second jobs
/// from competing for the account's concurrent-job cap. The inputs did not
/// change, so neither did the specs: each former job is now a *component*,
/// keyed by its old name, and its steps sit under a `# component: <name>`
/// marker in `ci.yml`.
///
/// A composite context is stale iff **any** component is stale — never the
/// union of their input sets. The union would add cross terms (`main` changes
/// component A's script, the PR touches only component B's files) that make
/// the whole context stale when no gate's verdict could have moved. Taking the
/// OR of per-component verdicts refuses exactly as often as 19 separately
/// required contexts did.
#[derive(Debug, Clone, Copy)]
pub struct RequiredCheck {
    /// The required status-check context (the `ci.yml` job `name:`).
    pub context: &'static str,
    /// The [`CheckSpec::context`] names of the gates this job runs.
    pub components: &'static [&'static str],
}

/// Required context → component gates. See [`RequiredCheck`].
pub const REQUIRED_CHECKS: &[RequiredCheck] = &[
    RequiredCheck {
        context: "Structural Checks",
        components: &[
            "Conflict Marker Check",
            "CLAUDE.md Line Budget",
            "File Size Ratchet",
            "Shell Allowlist",
            "Markdown Token Ratchet",
            "Role Prompt Prefix Ratchet",
            "AGENTS.md Sync Check",
            "Config Host-Path Guard",
            "Docs/Defaults Parity Check",
            "Guard Scan-String Tier Contracts",
            "Doc Table-of-Contents Freshness",
            "Hooks/Scripts Defaults Parity Check",
            "Vendored Private-Reference Scrub",
            "Dangling Link Check",
            "PRs Must Not Hand-Edit Version-Bearing Files",
            "Shell Syntax (ubuntu-latest)",
        ],
    },
    RequiredCheck {
        context: "Shell Syntax (macos-latest)",
        components: &["Shell Syntax (macos-latest)"],
    },
    RequiredCheck {
        context: "Daemon Checks",
        components: &["Shell Budget Ratchet", ".gitignore Convergence Check"],
    },
];

/// The hand-maintained input table. **Adding a step to a required job means
/// updating the matching entry here** — `tests.rs`'s pin test fails otherwise.
pub const SPECS: &[CheckSpec] = &[
    // Reads exactly one file's line count.
    CheckSpec {
        context: "CLAUDE.md Line Budget",
        global: &["scripts/check-claude-md-budget.sh", CI_WORKFLOW],
        scanned: &["CLAUDE.md"],
        coupled: &[],
        removal_sensitive: false,
    },
    // The #8248 incident's own check: a per-file code-line count against a
    // repo-global baseline ledger.
    CheckSpec {
        context: "File Size Ratchet",
        global: &[
            "scripts/check-file-size-budget.sh",
            "scripts/file-size-baseline.txt",
            CI_WORKFLOW,
        ],
        scanned: SOURCE_FILES,
        coupled: &[],
        removal_sensitive: false,
    },
    // `loom-daemon shell-budget --check` — the measuring logic is Rust, and
    // the allowlist supplies each script's category.
    CheckSpec {
        context: "Shell Budget Ratchet",
        global: &[
            "loom-daemon/**",
            "scripts/shell-allowlist.txt",
            "scripts/shell-budget-baseline.txt",
            "Cargo.toml",
            "Cargo.lock",
            "rust-toolchain.toml",
            CI_WORKFLOW,
        ],
        scanned: SHELL,
        coupled: &[],
        removal_sensitive: false,
    },
    // Every tracked `.sh` must carry an allowlist entry, so a deletion on one
    // side and an entry edit on the other interact.
    CheckSpec {
        context: "Shell Allowlist",
        global: &[
            "scripts/check-shell-allowlist.sh",
            "scripts/shell-allowlist.txt",
            CI_WORKFLOW,
        ],
        scanned: SHELL,
        coupled: &["scripts/shell-allowlist.txt", "**/*.sh"],
        removal_sensitive: true,
    },
    // Per-file byte-count proxy against a baseline ledger.
    CheckSpec {
        context: "Markdown Token Ratchet",
        global: &[
            "scripts/check-markdown-token-budget.sh",
            "scripts/markdown-token-baseline.txt",
            CI_WORKFLOW,
        ],
        scanned: PROMPT_SURFACE,
        coupled: &[],
        removal_sensitive: false,
    },
    // An AGGREGATE over each role's whole prompt file set — split a file in two
    // and every per-file number drops while this one rises. Purely coupled.
    CheckSpec {
        context: "Role Prompt Prefix Ratchet",
        global: &[
            "scripts/check-role-prompt-budget.sh",
            "scripts/role-prompt-budget.txt",
            CI_WORKFLOW,
        ],
        scanned: &[],
        coupled: PROMPT_SURFACE,
        removal_sensitive: true,
    },
    // A content scan of every tracked file, strictly per-file.
    CheckSpec {
        context: "Conflict Marker Check",
        global: &["defaults/scripts/check-conflict-markers.sh", CI_WORKFLOW],
        scanned: &["**"],
        coupled: &[],
        removal_sensitive: false,
    },
    // `bash -n` over every script, plus the pipefail and daemon-subcommand
    // ratchets. VERSION is a global input: `check-daemon-subcommand-versions.sh`
    // fails when a `requires-daemon >= X` has X > VERSION. A VALIDATED
    // increasing restamp is discounted from D before matching (that condition
    // can only clear, never appear, as VERSION rises); any other VERSION change
    // lands here.
    CheckSpec {
        context: "Shell Syntax (ubuntu-latest)",
        global: SHELL_SYNTAX_GLOBAL,
        scanned: SHELL,
        coupled: &[],
        removal_sensitive: false,
    },
    CheckSpec {
        context: "Shell Syntax (macos-latest)",
        global: SHELL_SYNTAX_GLOBAL,
        scanned: SHELL,
        coupled: &[],
        removal_sensitive: false,
    },
    // AGENTS.md is GENERATED from CLAUDE.md: a generated/source pair, coupled
    // by construction.
    CheckSpec {
        context: "AGENTS.md Sync Check",
        global: &[
            "scripts/check-agents-md-sync.sh",
            "defaults/scripts/generate-agents-md.sh",
            CI_WORKFLOW,
        ],
        scanned: &[],
        coupled: &[
            "AGENTS.md",
            "CLAUDE.md",
            "defaults/.loom/AGENTS.md",
            "defaults/.loom/CLAUDE.md",
        ],
        removal_sensitive: false,
    },
    // Scans the two tracked config files for home-directory absolute paths.
    CheckSpec {
        context: "Config Host-Path Guard",
        global: &[
            "defaults/scripts/check-config-no-host-paths.sh",
            CI_WORKFLOW,
        ],
        scanned: &[".loom/config.json", ".loom-project/project.json"],
        coupled: &[],
        removal_sensitive: false,
    },
    // Pairs `.loom/docs/*` against `defaults/docs/*` (orphans, should-be-symlinks).
    CheckSpec {
        context: "Docs/Defaults Parity Check",
        global: &["scripts/check-docs-defaults-parity.sh", CI_WORKFLOW],
        scanned: &[],
        coupled: &[".loom/docs/**", "defaults/docs/**", "docs/adr/**"],
        removal_sensitive: true,
    },
    // Pairs the guard's scan-string tiers against their doc + ADR + suites.
    CheckSpec {
        context: "Guard Scan-String Tier Contracts",
        global: &["scripts/check-guard-scan-contracts.sh", CI_WORKFLOW],
        scanned: &[],
        coupled: &[
            "defaults/hooks/guard-destructive-generic.sh",
            "defaults/docs/guard-scan-contracts.md",
            "docs/adr/0016-*.md",
            "tests/hooks/test-guard-destructive*.sh",
        ],
        removal_sensitive: false,
    },
    // A generated TOC is a function of its own file's headings — per-file.
    CheckSpec {
        context: "Doc Table-of-Contents Freshness",
        global: &["scripts/check-doc-tocs.sh", CI_WORKFLOW],
        scanned: &["defaults/docs/*.md", "defaults/.claude/commands/loom/*.md"],
        coupled: &[],
        removal_sensitive: false,
    },
    // Byte-identity between `defaults/` and its installed `.loom/` copies:
    // every file is half of a pair, and a deletion on one side is the classic
    // drift.
    CheckSpec {
        context: "Hooks/Scripts Defaults Parity Check",
        global: &[
            "scripts/check-hooks-defaults-parity.sh",
            ".loom/resync-ignore",
            CI_WORKFLOW,
        ],
        scanned: &[],
        coupled: RESYNC_PAIRS,
        removal_sensitive: true,
    },
    // A per-file content scan over the copy-installed surface.
    CheckSpec {
        context: "Vendored Private-Reference Scrub",
        global: &["scripts/check-vendored-private-refs.sh", CI_WORKFLOW],
        scanned: &["defaults/**", "loom-daemon/src/fleet/**"],
        coupled: &[],
        removal_sensitive: false,
    },
    // The link graph: markdown names other paths, so the set is coupled AND a
    // deletion anywhere can break a link from anywhere.
    CheckSpec {
        context: "Dangling Link Check",
        global: &[
            "scripts/check-dangling-links.sh",
            "scripts/check-doc-anchors.sh",
            CI_WORKFLOW,
        ],
        scanned: &[],
        coupled: MARKDOWN,
        removal_sensitive: true,
    },
    // `.gitignore` against EPHEMERAL_PATTERNS, which lives in Rust.
    CheckSpec {
        context: ".gitignore Convergence Check",
        global: &[
            "scripts/check-gitignore-convergence.sh",
            "loom-daemon/**",
            "Cargo.toml",
            "Cargo.lock",
            CI_WORKFLOW,
        ],
        scanned: &[],
        coupled: &[".gitignore"],
        removal_sensitive: false,
    },
    // Its ONLY input is its own script: it diffs `merge-base(base, head)..head`
    // by git revision (the full-history checkout has both), so it depends on the PR's own
    // commits alone. The version-bearing files `main` restamps are NOT inputs
    // to it — which is why a restamp on `main` cannot make it stale.
    CheckSpec {
        context: "PRs Must Not Hand-Edit Version-Bearing Files",
        global: &[
            "defaults/scripts/check-defaults-version-bump.sh",
            CI_WORKFLOW,
        ],
        scanned: &[],
        coupled: &[],
        removal_sensitive: false,
    },
];

/// Shared by both `Shell Syntax` matrix legs, which run identical steps.
const SHELL_SYNTAX_GLOBAL: &[&str] = &[
    "defaults/scripts/check-shell-syntax.sh",
    "scripts/check-shell-allowlist.sh",
    "scripts/shell-allowlist.txt",
    "scripts/check-pipefail-early-exit.sh",
    "scripts/pipefail-early-exit-baseline.txt",
    "scripts/check-daemon-subcommand-versions.sh",
    "scripts/daemon-subcommand-baseline.txt",
    "VERSION",
    CI_WORKFLOW,
];

// --- Glob matching -----------------------------------------------------------

/// Match `path` against a `/`-segmented glob: `*` matches within one segment,
/// `**` matches zero or more whole segments.
///
/// Deliberately tiny and dependency-free — the table's patterns are all of the
/// form `a/b/**`, `**/*.ext`, `dir/*.ext`, `pre*suf.ext`, or a literal path.
#[must_use]
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let pat: Vec<&str> = pattern.split('/').collect();
    let seg: Vec<&str> = path.split('/').collect();
    segments_match(&pat, &seg)
}

fn segments_match(pat: &[&str], seg: &[&str]) -> bool {
    match pat.split_first() {
        None => seg.is_empty(),
        Some((&"**", rest)) => (0..=seg.len()).any(|i| segments_match(rest, &seg[i..])),
        Some((head, rest)) => match seg.split_first() {
            None => false,
            Some((first, tail)) => wildcard_match(head, first) && segments_match(rest, tail),
        },
    }
}

/// `*`-only wildcard match inside a single path segment.
fn wildcard_match(pat: &str, text: &str) -> bool {
    if !pat.contains('*') {
        return pat == text;
    }
    let parts: Vec<&str> = pat.split('*').collect();
    let last = parts.len() - 1;
    let mut rest = text;
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            if !rest.starts_with(part) {
                return false;
            }
            rest = &rest[part.len()..];
        } else if i == last {
            return part.is_empty() || (rest.len() >= part.len() && rest.ends_with(part));
        } else if !part.is_empty() {
            match rest.find(part) {
                Some(idx) => rest = &rest[idx + part.len()..],
                None => return false,
            }
        }
    }
    true
}

#[cfg(test)]
mod tests;
