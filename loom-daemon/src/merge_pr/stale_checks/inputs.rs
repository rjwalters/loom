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
//!   can flip the verdict for *every* file. `ci.yml` is the one entry judged
//!   below whole-file granularity: [`super::workflow_scope`] attributes **each
//!   side's** hunks to the job/component blocks they edit — `D`'s against the
//!   base tip's workflow, `P`'s against the PR head's — so a change to a job no
//!   required context runs is not a global-input move on that side (#9065). Any
//!   unattributable edit keeps the whole-file meaning, per side.
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
//!   non-empty, unless the repo declares its inputs in
//!   `.loom/stale-check-inputs.json` (#9589, see [`super::repo_specs`]) —
//!   the path a consumer repo, whose contexts this table never names, uses. Adding a step to a required job without updating this table
//!   therefore blocks merges rather than silently passing them — and the pin
//!   test in `tests.rs` fails first, at PR time.
//! - Missing `B`, `D` or `P` (a non-Actions check, an unreadable log, a
//!   failed/truncated/diverged compare, an unreadable `P`) falls back to
//!   #8248's `started_at` rule with a `Warning:` on stderr. Nothing is ever
//!   worse than today.
//! - Every set is safe to **over**-populate: every clause is monotone in
//!   `G`, `S` and `C`, so a path listed too broadly can only make the guard
//!   refuse more often.
//!
//! # Gates implemented inside the daemon binary
//!
//! Four components (`Shell Budget Ratchet`, `.gitignore Convergence Check`,
//! `Secret Scan`, `MCP Guard Wiring Contract`) run a `loom-daemon` subcommand
//! rather than a script. Their `G` used to be `loom-daemon/**`, which nearly
//! every base move and nearly every PR touches, so they read as stale almost
//! always and the guard stopped distinguishing anything (PRs #9543/#9544,
//! 2026-09-29: refused on `cli/forge_action.rs` vs `init/post_init.rs`).
//!
//! Each now lists the **source files its verdict can depend on**: the
//! subcommand's handler, the library module(s) that handler reaches by
//! following `mod` / `use crate::…` / `loom_daemon::…` / `super::…` paths
//! transitively, the dispatch chain from `main()` to the handler, and the
//! build inputs (`Cargo.toml`s, `Cargo.lock`, `rust-toolchain.toml`,
//! `.cargo/config.toml`, `build.rs`). `daemon_surface_tests.rs` recomputes
//! that closure from the source on every test run and fails when a checker
//! starts reaching a file its spec does not list, so the globs grow with the
//! code instead of silently going stale.
//!
//! What these globs deliberately do **not** model is whether the merged tree
//! still *compiles*: a Rust-level semantic conflict between an unrelated base
//! move and this PR (a renamed function on one side, a new caller on the
//! other) fails the build that `Daemon Checks` depends on, and no gate's
//! input set is the right place to guard that. It is out of scope here; see
//! the merge-time re-verification follow-up (#9571).

use super::repo_specs::RepoSpecs;
use super::workflow_scope::{CiScope, CiScopes};
use std::collections::{BTreeMap, BTreeSet};

/// `.github/workflows/ci.yml` is a global input to every required context:
/// it is where the job's steps, its runner and its path filters live.
///
/// It is the **only** `G` entry that is narrowed below the whole file: one
/// path covers ~25 jobs of which three are required, so #9065 attributes each
/// side's `ci.yml` hunks to the job/component blocks they edit (see
/// [`super::workflow_scope`]) and lets clauses 1 and 2 fire only for the
/// components whose own definition moved on that side. Every unattributable
/// edit restores the whole-file meaning.
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
        self.first_match_scoped(patterns, false)
    }

    /// [`Self::first_match`], optionally treating [`CI_WORKFLOW`] as absent —
    /// the #9065 narrowing (see
    /// [`stale_reason_scoped`]), applied to whichever side the caller is
    /// matching.
    fn first_match_scoped(&self, patterns: &[&str], skip_ci: bool) -> Option<&str> {
        self.paths
            .iter()
            .filter(|p| !(skip_ci && p.as_str() == CI_WORKFLOW))
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
    /// Which components a `.github/workflows/ci.yml` entry in `files` is a
    /// global input *for* (#9065). [`CiScope::Unscoped`] — the default, and the
    /// answer to every unattributable `ci.yml` edit — is the pre-#9065
    /// whole-file meaning.
    pub ci_scope: CiScope,
}

/// The input-scoped evidence for one PR: `P`, plus `B`/`D` per required
/// context, plus why any context has none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScopedEvidence {
    /// `P` — the PR's own delta.
    pub pr_delta: FileSet,
    /// Which components a `.github/workflows/ci.yml` entry in [`Self::pr_delta`]
    /// is a global input *for* (#9065), attributed against the **PR head's**
    /// workflow. [`CiScope::Unscoped`] — the default, and the answer to every
    /// unattributable edit — is the pre-#9065 whole-file meaning.
    ///
    /// One value for the whole PR, not one per context: `P` is a single file
    /// set read once, unlike `D`, which is keyed on each context's tested base.
    pub pr_ci_scope: CiScope,
    /// Per required context, the base it tested and the move since.
    pub base_moves: BTreeMap<String, BaseMove>,
    /// Per required context with no `base_moves` entry, why — surfaced as the
    /// `Warning:` that accompanies the fallback to the time rule.
    pub fallbacks: BTreeMap<String, String>,
    /// The base tip's per-repo input declaration (#9589), consulted only for a
    /// context with no built-in spec. [`RepoSpecs::Absent`] — the default —
    /// keeps such a context stale on any base move.
    pub repo_specs: RepoSpecs,
}

/// One required context's input specification. See the module header for what
/// `global` / `scanned` / `coupled` mean and why over-population is safe.
///
/// Generic over the borrow so a per-repo declaration (#9589,
/// [`super::repo_specs`]) can be judged by the very same clauses; the built-in
/// table is `CheckSpec<'static>`.
#[derive(Debug, Clone, Copy)]
pub struct CheckSpec<'a> {
    /// The component gate's name: its former `ci.yml` job name, and the name its
    /// `# component:` marker uses (see [`RequiredCheck`]).
    pub context: &'a str,
    /// `G` — inputs whose change can flip the verdict for every file.
    pub global: &'a [&'a str],
    /// `S` — paths whose verdict depends only on their own content plus `G`.
    pub scanned: &'a [&'a str],
    /// `C` — cross-file aggregates and links.
    pub coupled: &'a [&'a str],
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
///
/// `ci.yml` keeps its whole-file `G` meaning; [`stale_reason_scoped`] is the
/// form that narrows it (#9065).
#[must_use]
pub fn stale_reason(spec: &CheckSpec<'_>, d: &FileSet, p: &FileSet) -> Option<StaleReason> {
    stale_reason_scoped(spec, d, p, &CiScopes::unscoped())
}

/// [`stale_reason`] with each side's `ci.yml` attribution (#9065).
///
/// Both sides are narrowed, each against the tree its own patch's new side
/// belongs to (see [`CiScopes`]), and each independently: a side with no
/// attribution to offer stays [`CiScope::Unscoped`], i.e. keeps `ci.yml`'s
/// whole-file `G` meaning, with no effect on the other.
///
/// The `P`-side narrowing reaches **clauses 1 and 2 only** — the two whose
/// patterns include `G`, and therefore the only two where `P`'s `ci.yml` entry
/// can match at all. Clauses 3-5 match `P` against `scanned` / `coupled` /
/// `removed` and are left un-narrowed on that side: over-refusing is the
/// direction this guard is allowed to err in.
#[must_use]
pub fn stale_reason_scoped(
    spec: &CheckSpec<'_>,
    d: &FileSet,
    p: &FileSet,
    ci: &CiScopes,
) -> Option<StaleReason> {
    if d.is_empty() {
        // The base has not moved (once validated restamps are discounted), so
        // the tree the check tested IS the tree it will merge onto.
        return None;
    }
    // When a side's ci.yml hunks land outside this component's own job block
    // (and outside everything that job needs), that path is not one of this
    // check's inputs and must not read as a global-input move.
    let skip_ci_d = !ci.base.affects(spec.context);
    let skip_ci_p = !ci.pr.affects(spec.context);
    let d_match = |pats: &[&str]| d.first_match_scoped(pats, skip_ci_d);
    let p_match = |pats: &[&str]| p.first_match_scoped(pats, skip_ci_p);
    let any: Vec<&str> = spec
        .global
        .iter()
        .chain(spec.scanned.iter())
        .chain(spec.coupled.iter())
        .copied()
        .collect();

    // Clause 1: main changed a global input, and the PR has something for that
    // input to re-judge.
    if let (Some(b), Some(pp)) = (d_match(spec.global), p_match(&any)) {
        return Some(StaleReason {
            clause: "the base move changed a global input of this check",
            base_path: Some(b.to_string()),
            pr_path: Some(pp.to_string()),
        });
    }
    // Clause 2: the PR changed a global input, and main has something for it
    // to re-judge.
    if let (Some(pp), Some(b)) = (p_match(spec.global), d_match(&any)) {
        return Some(StaleReason {
            clause: "this PR changes a global input of this check and the base moved under it",
            base_path: Some(b.to_string()),
            pr_path: Some(pp.to_string()),
        });
    }
    // Clause 3: both sides touched the same per-file scanned path.
    if let Some(shared) = d
        .paths
        .iter()
        .filter(|path| !(skip_ci_d && path.as_str() == CI_WORKFLOW))
        .find(|path| {
            p.paths.contains(*path) && spec.scanned.iter().any(|pat| glob_match(pat, path))
        })
    {
        return Some(StaleReason {
            clause: "the base move and this PR both touch the same scanned file",
            base_path: Some(shared.clone()),
            pr_path: Some(shared.clone()),
        });
    }
    // Clause 4: both sides touched the coupled (cross-file aggregate/link) set.
    if let (Some(b), Some(pp)) = (d_match(spec.coupled), p.first_match(spec.coupled)) {
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
        if let (Some(b), Some(pp)) = (
            d.removed
                .iter()
                .find(|path| !(skip_ci_d && path.as_str() == CI_WORKFLOW)),
            p.first_match(spec.coupled).map(str::to_string),
        ) {
            return Some(StaleReason {
                clause: "the base move deleted or renamed a path while this PR touches the \
                         linking files",
                base_path: Some(b.clone()),
                pr_path: Some(pp),
            });
        }
        if let (Some(pp), Some(b)) =
            (p.removed.iter().next(), d_match(spec.coupled).map(str::to_string))
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
        clause: "this required check has no entry in the input-scope table and no declaration \
                 in `.loom/stale-check-inputs.json`, so which files it reads is unknown",
        base_path: Some(b.clone()),
        pr_path: None,
    })
}

/// The spec for one component gate, or `None`.
#[must_use]
pub fn spec_for(component: &str) -> Option<&'static CheckSpec<'static>> {
    SPECS.iter().find(|s| s.context == component)
}

/// The component specs behind a required `context`: its [`REQUIRED_CHECKS`]
/// entry's components, or — for a name that is itself a component — just that
/// one spec. `None` when any component is unmapped, which
/// [`unknown_check_reason`] then turns into a refusal (fail closed).
#[must_use]
pub fn specs_for(context: &str) -> Option<Vec<&'static CheckSpec<'static>>> {
    match required_check(context) {
        Some(req) => {
            // An aggregate (#10444) is its own components plus every component
            // of each context it aggregates — the OR of the verdicts it folds.
            let mut specs: Vec<&'static CheckSpec<'static>> = req
                .components
                .iter()
                .map(|c| spec_for(c))
                .collect::<Option<_>>()?;
            for agg in req.aggregates {
                for c in required_check(agg)?.components {
                    specs.push(spec_for(c)?);
                }
            }
            Some(specs)
        }
        None => spec_for(context).map(|s| vec![s]),
    }
}

/// The [`REQUIRED_CHECKS`] entry for `context`, or `None`.
#[must_use]
pub fn required_check(context: &str) -> Option<&'static RequiredCheck> {
    REQUIRED_CHECKS.iter().find(|r| r.context == context)
}

/// [`stale_reason_scoped`] for a required context made of `components`: the
/// first stale component (in table order) and why, or `None` when none is
/// stale.
#[must_use]
pub fn composite_stale_reason(
    components: &[&'static CheckSpec<'static>],
    d: &FileSet,
    p: &FileSet,
    ci: &CiScopes,
) -> Option<(&'static str, StaleReason)> {
    components
        .iter()
        .find_map(|spec| stale_reason_scoped(spec, d, p, ci).map(|r| (spec.context, r)))
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

/// Everything `scripts/check-role-prompt-budget.sh` reads (#9748): the two
/// shared prefixes (`SHARED_PREFIX`), role discovery (`git ls-files
/// 'defaults/roles/*.json'`) and the command directory (`CMD_DIR`) whose entry
/// points and transitively linked bare siblings are summed. It never reads
/// `defaults/docs`, installed `.loom/docs`, or the installed mirrors. The
/// command-directory set is conservative (every markdown file there, not a
/// per-role graph). A test runs the real checker's `--files` and fails if its
/// read surface outgrows this set (ci-principles rule 9: refine what a result
/// covers from the checker's own source; the check still runs on every PR).
///
/// Role discovery is `defaults/roles/**/*.json`, not `*.json`: a git pathspec
/// without `:(glob)` magic matches `*` across `/`, so the checker also
/// discovers a role from a JSON in a subdirectory. A one-segment glob would
/// silently drop that read — narrowing on a surface the checker does not
/// actually have, which rule 9 forbids.
const ROLE_PROMPT_PREFIX_READS: &[&str] = &[
    "CLAUDE.md",
    "defaults/.loom/CLAUDE.md",
    "defaults/roles/**/*.json",
    "defaults/.claude/commands/loom/*.md",
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
    "CI Result",
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
    /// Other required contexts whose jobs this one aggregates through
    /// `needs:` (#10444's `CI Result`). Their components are judged as part of
    /// this context too ([`specs_for`]) — the aggregate's verdict is theirs
    /// OR'd with its own components' — and their jobs' `ci.yml` blocks stay
    /// attributed to those components rather than to this context's own
    /// ([`super::workflow_scope`]). Empty for an ordinary job.
    pub aggregates: &'static [&'static str],
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
        aggregates: &[],
    },
    RequiredCheck {
        context: "Shell Syntax (macos-latest)",
        components: &["Shell Syntax (macos-latest)"],
        aggregates: &[],
    },
    RequiredCheck {
        context: "Daemon Checks",
        components: &[
            "Shell Budget Ratchet",
            ".gitignore Convergence Check",
            "Secret Scan",
            "MCP Guard Wiring Contract",
        ],
        aggregates: &[],
    },
    // The always-run aggregate (#10444): it fails when any job it `needs:`
    // failed or was cancelled. Its own component covers the gate script and
    // every job no other required context runs; the three required contexts
    // it also aggregates contribute their components unchanged.
    RequiredCheck {
        context: "CI Result",
        components: &["CI Result"],
        aggregates: &[
            "Structural Checks",
            "Shell Syntax (macos-latest)",
            "Daemon Checks",
        ],
    },
];

/// The hand-maintained input table. **Adding a step to a required job means
/// updating the matching entry here** — `tests.rs`'s pin test fails otherwise.
pub const SPECS: &[CheckSpec<'static>] = &[
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
    //
    // Rust surface: the handler (`cli/shell_budget.rs`), the `shell_budget`
    // module tree it calls, and the binary's top-level subcommand registry —
    // the handler reads `crate::Cli`'s subcommand names to validate
    // `Shell-Budget-Callout:` trailers, so `main.rs` and every enum it
    // `#[command(flatten)]`s (`cli/telemetry.rs`, `cli/script_ports.rs`,
    // `cli/dep_classify.rs`) are inputs too. MUST grow if the checker starts
    // using another module — `daemon_surface_tests.rs` fails until it does.
    CheckSpec {
        context: "Shell Budget Ratchet",
        global: &[
            "loom-daemon/src/cli/shell_budget.rs",
            "loom-daemon/src/shell_budget.rs",
            "loom-daemon/src/shell_budget/**",
            "loom-daemon/src/main.rs",
            "loom-daemon/src/daemon_service.rs",
            "loom-daemon/src/cli/script_ports.rs",
            "loom-daemon/src/cli/telemetry.rs",
            "loom-daemon/src/cli/dep_classify.rs",
            "scripts/shell-allowlist.txt",
            "scripts/shell-budget-baseline.txt",
            "Cargo.toml",
            "Cargo.lock",
            "rust-toolchain.toml",
            ".cargo/config.toml",
            "loom-daemon/Cargo.toml",
            "loom-daemon/build.rs",
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
            // The CI Result gate's tests ride in this component's step group
            // (#10444); the test drives the gate script, so both are inputs.
            "scripts/test-ci-result-gate.sh",
            "scripts/ci-result-gate.sh",
            // The release-decision tests also ride in this step group
            // (#10826); they drive the decision script and parse release.yml.
            "scripts/test-release-decision.sh",
            "scripts/release-decision.sh",
            ".github/workflows/release.yml",
            // The version-bump cadence gate tests ride here too (#11174); they
            // drive the gate script and grep version-bump-on-merge.yml.
            "scripts/test-version-bump-gate.sh",
            "scripts/version-bump-gate.sh",
            ".github/workflows/version-bump-on-merge.yml",
            // The image-input tests ride here too (#10825); they drive the
            // detection script, which reads .dockerignore, and parse ci.yml.
            "scripts/test-ci-image-inputs.sh",
            "scripts/ci-image-inputs.sh",
            ".dockerignore",
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
    //
    // `ci.yml` runs `--self-test` first, which executes
    // `check-markdown-token-budget.sh --list` on the REAL tree and fails the
    // step when its estimator disagrees with this one — so that script is a
    // global input too.
    CheckSpec {
        context: "Role Prompt Prefix Ratchet",
        global: &[
            "scripts/check-role-prompt-budget.sh",
            "scripts/role-prompt-budget.txt",
            "scripts/check-markdown-token-budget.sh",
            CI_WORKFLOW,
        ],
        scanned: &[],
        coupled: ROLE_PROMPT_PREFIX_READS,
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
    //
    // Rust surface: `update-gitignore`'s handler in `cli/misc_cmds.rs` calls
    // `loom_daemon::init::update_gitignore` (`init/post_init.rs`, which owns
    // EPHEMERAL_PATTERNS). The whole `init` module tree is listed, plus the
    // modules its production code reaches (`agent_skills`, `install_compat`,
    // `proc_exec`, `release_provenance`, `self_update`), and the dispatch chain.
    // MUST grow if the checker starts using another module —
    // `daemon_surface_tests.rs` fails until it does. The script also runs
    // `scripts/cargo-target-dir.sh`.
    CheckSpec {
        context: ".gitignore Convergence Check",
        global: &[
            "scripts/check-gitignore-convergence.sh",
            "scripts/cargo-target-dir.sh",
            "loom-daemon/src/cli/misc_cmds.rs",
            "loom-daemon/src/init/**",
            "loom-daemon/src/agent_skills.rs",
            "loom-daemon/src/install_compat.rs",
            "loom-daemon/src/install_compat/**",
            "loom-daemon/src/proc_exec.rs",
            "loom-daemon/src/release_provenance.rs",
            "loom-daemon/src/self_update.rs",
            "loom-daemon/src/main.rs",
            "loom-daemon/src/daemon_service.rs",
            "Cargo.toml",
            "Cargo.lock",
            "rust-toolchain.toml",
            ".cargo/config.toml",
            "loom-daemon/Cargo.toml",
            "loom-daemon/build.rs",
            CI_WORKFLOW,
        ],
        scanned: &[],
        coupled: &[".gitignore"],
        removal_sensitive: false,
    },
    // Scans the commits in `base..head` by revision (#9133), so like the
    // version-bump check it depends on the PR's own commits alone; `main`
    // moving can only make it stale through the scanner or its allowlist.
    //
    // Rust surface: the handler (`cli/secret_scan_cmd.rs`), the self-contained
    // `secret_scan` module (std + external crates only), and the dispatch
    // chain through `cli/script_ports.rs`. MUST grow if the checker starts
    // using another module — `daemon_surface_tests.rs` fails until it does.
    CheckSpec {
        context: "Secret Scan",
        global: &[
            "loom-daemon/src/cli/secret_scan_cmd.rs",
            "loom-daemon/src/secret_scan.rs",
            "loom-daemon/src/secret_scan/**",
            "loom-daemon/src/main.rs",
            "loom-daemon/src/daemon_service.rs",
            "loom-daemon/src/cli/script_ports.rs",
            "Cargo.toml",
            "Cargo.lock",
            "rust-toolchain.toml",
            ".cargo/config.toml",
            "loom-daemon/Cargo.toml",
            "loom-daemon/build.rs",
            ".loom/secret-scan-allow",
            CI_WORKFLOW,
        ],
        scanned: &[],
        coupled: &[],
        removal_sensitive: false,
    },
    // `loom-daemon check-guard-wiring` (#9108) — asserts the `mcp__loom__.*`
    // PreToolUse matcher is wired in BOTH `.claude/settings.json` and the
    // installer's `_PHOOK_*` arrays, routes through `hook-wiring.sh`, and
    // carries the fail-closed broken-install floor. Its whole subject is the
    // relationship BETWEEN those three files plus the hook they name, so they
    // are `coupled`, not `scanned`: a settings edit on one side and an
    // installer edit on the other is exactly the combination that can open the
    // hole while each side looks fine alone. `removal_sensitive` because
    // deleting the hook file is one of the four violations.
    //
    // Rust surface: the handler (`cli/check_guard_wiring.rs`), the
    // self-contained `guard_wiring` module, and the dispatch chain through
    // `cli/script_ports.rs`. MUST grow if the checker starts using another
    // module — `daemon_surface_tests.rs` fails until it does.
    CheckSpec {
        context: "MCP Guard Wiring Contract",
        global: &[
            "loom-daemon/src/cli/check_guard_wiring.rs",
            "loom-daemon/src/guard_wiring.rs",
            "loom-daemon/src/guard_wiring/**",
            "loom-daemon/src/main.rs",
            "loom-daemon/src/daemon_service.rs",
            "loom-daemon/src/cli/script_ports.rs",
            "Cargo.toml",
            "Cargo.lock",
            "rust-toolchain.toml",
            ".cargo/config.toml",
            "loom-daemon/Cargo.toml",
            "loom-daemon/build.rs",
            CI_WORKFLOW,
        ],
        scanned: &[],
        coupled: &[
            ".claude/settings.json",
            "defaults/.claude/settings.json",
            "scripts/install/provision-hooks.sh",
            "defaults/hooks/guard-mcp-tools.sh",
        ],
        removal_sensitive: true,
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
    // `CI Result`'s own component (#10444): the gate script plus the inputs of
    // every job it `needs:` that no other required context runs — the Rust
    // build/lint/test jobs, `node-packages`, the installer/codex/dep suites,
    // `shell-suite-tests`, `install-surface-checks`, `repo-hygiene` and the
    // image smokes. Global: a Rust or shell suite's verdict can hinge on any
    // file it compiles or reads, so any interaction refuses.
    //
    // `WORK_PLAN.md` is coupled, not global: two CI-wired shell suites
    // (`test-guide-operator-attention-fold.sh` Test 3, `test-docs-worktree.sh`
    // Test 6) assert on the committed root file, and `shell-suite-tests` only
    // reaches a verdict through this aggregate. Coupled puts it in clauses
    // 1-2's `any` set: a `WORK_PLAN.md` move on `main` refuses a PR that
    // touches a global input (those suites, `guide.md`), and a PR touching
    // `WORK_PLAN.md` refuses under a global move on `main`. A Guide docs
    // refresh on `main` alone still does not refuse an unrelated PR.
    CheckSpec {
        context: "CI Result",
        global: CI_RESULT_GLOBAL,
        scanned: &[],
        coupled: &["WORK_PLAN.md"],
        removal_sensitive: false,
    },
];

/// `CI Result`'s own `G`: the union of `ci.yml`'s `changes` path filters
/// (`backend`, `mcp`, `docker`, `scripts` — the paths those jobs are declared
/// to read) widened to every tracked code/config tree, plus the root files a
/// suite reads (`CLAUDE.md` via the premise-false suite, `VERSION`).
///
/// Deliberately absent, because no aggregated job reads them: `README.md`,
/// `CONTRIBUTING.md`, `SECURITY.md`, `LICENSE`, `WORK_LOG.md`, `docs/**`,
/// `assets/**`, `.vscode/**` and the editor/bot dotfiles. (`WORK_PLAN.md` IS
/// read — by two `shell-suite-tests` suites — and is in the spec's `coupled`
/// set rather than here; see the `CI Result` `CheckSpec`.) The
/// markdown among them is still judged by the `Structural Checks` components
/// (`Dangling Link Check`, `Conflict Marker Check`, …) this aggregate composes.
/// So an edit to `README.md` on `main` does not, on its own, refuse every PR.
const CI_RESULT_GLOBAL: &[&str] = &[
    "scripts/ci-result-gate.sh",
    CI_WORKFLOW,
    ".github/**",
    // backend
    "loom-daemon/**",
    "loom-api/**",
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain.toml",
    "rustfmt.toml",
    "deny.toml",
    ".cargo/**",
    ".config/**",
    // mcp / node
    "mcp-loom/**",
    "package.json",
    "pnpm-lock.yaml",
    "**/package.json",
    // docker
    "docker/**",
    ".dockerignore",
    // scripts, installer, install surface, shell suites
    "scripts/**",
    "defaults/**",
    "tests/**",
    "install.sh",
    ".loom/**",
    ".claude/**",
    ".agents/**",
    ".githooks/**",
    ".repo/**",
    "quickstarts/**",
    "examples/**",
    "**/*.sh",
    "**/*.rs",
    "**/*.ts",
    "VERSION",
    "CLAUDE.md",
    "AGENTS.md",
    "CHANGELOG.md",
    ".gitignore",
    ".gitattributes",
    ".shellcheckrc",
    ".env.example",
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

#[cfg(test)]
mod daemon_surface_tests;

#[cfg(test)]
mod aggregate_tests;
