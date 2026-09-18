//! Measuring the shell epic #7810 is actually trying to retire.
//!
//! # Why a category-aware number rather than one total
//!
//! The first cut of this gate (#8084) ratcheted ONE number: total production
//! shell, 51,814 code lines. That number cannot go to zero and was never
//! supposed to, because a fixed part of it is shell that must stay shell:
//!
//! - **`bootstrap`** runs before a `loom-daemon` binary exists, or after it is
//!   removed. Requiring the binary to install itself is circular.
//! - **`vendored`** is owned upstream in rjwalters/repo; its structure is not
//!   ours to change.
//!
//! Together that floor is 13,230 lines — a quarter of the total — and it is
//! **correctly permanent**. Ratcheting the sum of "what we are retiring" and
//! "what we are keeping" produces a number with no target, so no run of it ever
//! says how far along the epic is or what finished would look like.
//!
//! What the epic is actually retiring is the **portable** pool:
//!
//! - **`contract`** — the file's NAME is consumed by role prompts, CI workflows,
//!   hooks or consumer repos, so the name survives as a stub while the logic
//!   moves into the daemon. The allowlist's own reason line says it: *"The name
//!   stays; logic ports behind it."*
//! - **`hook-entry`** — a `PreToolUse`/`SessionStart` hook needs a
//!   shell-invocable command. Same deal: the stub stays, the logic behind it is
//!   a port target.
//!
//! That pool is 38,439 lines and its target is ~145 — the `stub` category,
//! which is what a fully ported file becomes (under 40 code lines, last line
//! `exec`). **That** is the number worth putting on a ratchet, and the number
//! worth being able to look at.
//!
//! # What it shows today
//!
//! Immediately before epic #7810's first port commit the portable pool was
//! 38,374 lines. It is now 38,439. Four scripts have been ported and roughly
//! 2,200 lines deleted, and the pool is nonetheless **65 lines larger** than
//! when the epic started, because more portable shell arrived than left.
//!
//! Reporting that is the point. A gate that only prevents growth, without ever
//! showing the trend, let four merged ports read as progress while the thing
//! they were retiring quietly grew.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Categories whose logic is a port target — the pool the epic drives to zero.
pub const PORTABLE: &[&str] = &["contract", "hook-entry"];
/// Categories that must stay shell. This floor is not a failure; it is the
/// answer to "what does finished look like".
pub const FLOOR: &[&str] = &["bootstrap", "vendored"];
/// What a fully ported file becomes: under 40 code lines, ending in `exec`.
pub const STUB: &str = "stub";
/// The trivial-glue cap: STRICTLY under this many code lines, a file cannot
/// be carrying logic. The gate compares with `-lt`, so 40 is over the cap,
/// not at it. `scripts/check-shell-allowlist.sh` (`STUB_MAX_CODE_LINES`)
/// is the authority and enforces it on the `stub` category; the copy here is
/// for [`churn`], which applies the same cap to a different question — has
/// THIS file handed its logic off, whatever category it declares. Keep the
/// two in step; `churn::tests` pins them together.
pub const STUB_CAP: usize = 40;

/// One measurement of the tree.
#[derive(Debug, Clone, Default)]
pub struct Budget {
    /// Code lines per allowlist category.
    pub by_category: BTreeMap<String, u64>,
    /// Files per allowlist category.
    pub files_by_category: BTreeMap<String, u64>,
    /// Per-file `(category, code lines)`, keyed by repo-relative path.
    ///
    /// Needed because the per-CATEGORY totals cannot distinguish "added 20
    /// lines to a bootstrap script" from "retired 20 portable lines and added
    /// 20 bootstrap ones" — the two produce identical category figures, and
    /// review used exactly that to launder new portable shell past a
    /// declaration (#8154).
    pub by_file: BTreeMap<String, (String, u64)>,
    /// Production scripts carrying no allowlist entry at all. Non-zero means
    /// the allowlist and the tree have drifted, which makes every other number
    /// here an undercount — so it is surfaced rather than folded in silently.
    pub unlisted: Vec<PathBuf>,
}

impl Budget {
    fn sum(&self, cats: &[&str]) -> u64 {
        cats.iter().filter_map(|c| self.by_category.get(*c)).sum()
    }

    /// The pool epic #7810 is retiring. **This is the headline number.**
    #[must_use]
    pub fn portable(&self) -> u64 {
        self.sum(PORTABLE)
    }

    /// Shell that must stay shell — the epic's floor, not its debt.
    #[must_use]
    pub fn floor(&self) -> u64 {
        self.sum(FLOOR)
    }

    /// Already-ported files' remaining glue.
    #[must_use]
    pub fn stubbed(&self) -> u64 {
        self.by_category.get(STUB).copied().unwrap_or(0)
    }

    #[must_use]
    pub fn total(&self) -> u64 {
        self.by_category.values().sum()
    }

    #[must_use]
    pub fn file_count(&self) -> u64 {
        self.files_by_category.values().sum()
    }
}

/// Whether a tracked `.sh` path counts as PRODUCTION shell.
///
/// Tests are excluded, and the reason is load-bearing rather than tidy: the
/// retained black-box suite method every port in this epic runs on REQUIRES
/// test shell to grow as production shell shrinks — a port keeps the old suite
/// and runs its assertions against the Rust. Counting tests would make the
/// correct move look like a regression, and a gate that punishes the right
/// behaviour gets gamed or deleted.
///
/// `.loom/` is excluded as an installed mirror of `defaults/`; counting both
/// double-counts one source.
#[must_use]
pub fn is_production_shell(path: &str) -> bool {
    if path.starts_with(".loom/") {
        return false;
    }
    if path.split('/').any(|seg| seg == "tests" || seg == "test") {
        return false;
    }
    !path.rsplit('/').next().unwrap_or(path).starts_with("test-")
}

/// Code lines: leading whitespace stripped, blanks skipped, `#`-leading lines
/// skipped. Byte-for-byte the rule `check-file-size-budget.sh` applies to `.sh`
/// (its `measure_all` awk pass), so the two gates never disagree about a file.
#[must_use]
pub fn code_lines(text: &str) -> usize {
    text.lines()
        .filter(|l| {
            let t = l.trim_start();
            !t.is_empty() && !t.starts_with('#')
        })
        .count()
}

/// Parse `scripts/shell-allowlist.txt` into `path -> category`.
#[must_use]
pub fn parse_allowlist(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some((it.next()?.to_string(), it.next()?.to_string()))
        })
        .collect()
}

/// Measure the tree at `root`.
///
/// # Errors
/// When `git ls-files` cannot be run or fails.
pub fn measure(root: &Path) -> Result<Budget, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z", "--", "*.sh"])
        .output()
        .map_err(|e| format!("could not run `git ls-files` in {}: {e}", root.display()))?;
    if !out.status.success() {
        return Err(format!(
            "`git ls-files` failed in {}: {}",
            root.display(),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let tracked: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();

    let allowlist_text = std::fs::read_to_string(root.join("scripts/shell-allowlist.txt"))
        .map_err(|e| format!("could not read scripts/shell-allowlist.txt: {e}"))?;
    let categories = parse_allowlist(&allowlist_text);

    // A discovery failure must not read as "no shell left": a broken pathspec
    // or a non-git checkout reports a budget of zero, which every downstream
    // check would happily accept as a win.
    //
    // The floor is DERIVED from the allowlist rather than hard-coded. A fixed
    // number encodes an assumption about how much shell this repo contains,
    // and this epic exists to invalidate that assumption — a constant chosen
    // today eventually fails on a healthy tree while blaming "broken
    // discovery". The allowlist enumerates every in-scope script and is
    // CI-enforced to stay in sync, so it tracks the real figure by
    // construction (#8120).
    if tracked.len() < categories.len() {
        return Err(format!(
            "scripts/shell-allowlist.txt enumerates {} scripts but discovery found only {} — \
             that is a discovery failure, not a smaller repo, and a measurement of nothing must \
             not read as progress",
            categories.len(),
            tracked.len()
        ));
    }

    // The PRODUCTION count needs its own floor, from a signal INDEPENDENT of
    // the filter being checked. The allowlist types every script, and a
    // non-`test` category is that signal: it is maintained by a human argument
    // in the allowlist, not derived from `is_production_shell`.
    //
    // Without it a too-narrow filter is invisible. Review proved it: adding
    // `if path.starts_with("defaults/") { return false }` made every gate pass
    // while the report announced
    //
    //     net vs epic start   -34222 — retired since the epic began
    //
    // A fabricated 34,000-line win, green. The floor above cannot catch that,
    // because `tracked` is the PRE-filter list — only a bound on the
    // post-filter count can.
    let expected = production_entry_count(&categories);
    let counted = tracked.iter().filter(|p| is_production_shell(p)).count();
    if counted < expected {
        return Err(production_filter_error(expected, counted));
    }

    let mut budget = Budget::default();
    for rel in tracked.iter().filter(|p| is_production_shell(p)) {
        let text = std::fs::read_to_string(root.join(rel))
            .map_err(|e| format!("tracked shell file {rel} could not be read as text: {e}"))?;
        let n = code_lines(&text) as u64;
        match categories.get(rel) {
            Some(cat) => {
                *budget.by_category.entry(cat.clone()).or_default() += n;
                *budget.files_by_category.entry(cat.clone()).or_default() += 1;
                budget.by_file.insert(rel.clone(), (cat.clone(), n));
            }
            None => budget.unlisted.push(PathBuf::from(rel)),
        }
    }
    Ok(budget)
}

/// Render the progress report — the thing a human reads to answer "are we
/// actually getting anywhere".
#[must_use]
pub fn render_report(budget: &Budget, origin_portable: u64) -> String {
    let portable = budget.portable();
    let net = i128::from(portable) - i128::from(origin_portable);
    let direction = if net > 0 {
        format!("+{net} — the pool has GROWN since the epic began")
    } else if net < 0 {
        format!("{net} — retired since the epic began")
    } else {
        "0 — unchanged since the epic began".to_string()
    };

    // Progress is measured against what can actually be retired, not against a
    // total that includes a permanent floor.
    let target = budget.stubbed();
    let retirable = portable.saturating_sub(target);

    let mut s = String::new();
    s.push_str("Epic #7810 — retire portable shell\n\n");
    s.push_str(&format!(
        "  portable remaining   {portable:>7}   <- the number that must reach ~{target}\n"
    ));
    s.push_str(&format!(
        "  irreducible floor    {:>7}   bootstrap + vendored; correctly permanent\n",
        budget.floor()
    ));
    s.push_str(&format!(
        "  already stubbed      {:>7}   what a ported file leaves behind\n",
        budget.stubbed()
    ));
    s.push_str(&format!(
        "  ----------------------------\n  total production     {:>7}   across {} files\n\n",
        budget.total(),
        budget.file_count()
    ));
    s.push_str(&format!("  net vs epic start    {direction}\n"));
    s.push_str(&format!("  still to retire      {retirable:>7}\n"));
    s.push_str("\n  by category:\n");
    for (cat, lines) in &budget.by_category {
        let files = budget.files_by_category.get(cat).copied().unwrap_or(0);
        let role = if PORTABLE.contains(&cat.as_str()) {
            "port target"
        } else if FLOOR.contains(&cat.as_str()) {
            "stays shell"
        } else {
            "ported"
        };
        s.push_str(&format!("    {cat:<12} {lines:>7} lines  {files:>4} files   ({role})\n"));
    }
    if !budget.unlisted.is_empty() {
        s.push_str(&format!(
            "\n  WARNING: {} production script(s) carry no allowlist entry, so every\n  \
             number above is an undercount:\n",
            budget.unlisted.len()
        ));
        for p in budget.unlisted.iter().take(10) {
            s.push_str(&format!("    {}\n", p.display()));
        }
    }
    s
}

/// The revision immediately before epic #7810's first port commit, as recorded
/// in `scripts/shell-budget-baseline.txt`.
///
/// Returns `None` when the key is absent, which is not an error — the report
/// simply omits the cumulative figure rather than failing.
#[must_use]
pub fn read_origin_rev(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join("scripts/shell-budget-baseline.txt")).ok()?;
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .find_map(|l| {
            let mut it = l.split_whitespace();
            (it.next()? == "origin_rev").then(|| it.next().map(str::to_string))?
        })
}

/// The portable-shell figure at epic #7810's first port commit, read from the
/// baseline file. It is the denominator progress is measured against and is
/// never regenerated — an origin that moves measures nothing.
#[must_use]
pub fn read_origin_portable(root: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(root.join("scripts/shell-budget-baseline.txt")).ok()?;
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .find_map(|l| {
            let mut it = l.split_whitespace();
            (it.next()? == "origin_portable").then(|| it.next()?.parse().ok())?
        })
}

/// The invariants that hold of any single tree, independent of comparison.
///
/// An unlisted production script makes every figure an undercount whichever
/// revision you measure, and a scope filter that stops seeing whole directories
/// makes the gate pass for the wrong reason. Neither needs a `before` to judge.
///
/// # Errors
/// Returns the operator-facing explanation when the tree is malformed.
pub fn check_invariants(budget: &Budget) -> Result<(), String> {
    if !budget.unlisted.is_empty() {
        return Err(format!(
            "{} production script(s) carry no allowlist entry, so every figure is an \
             undercount — add them to scripts/shell-allowlist.txt:\n{}",
            budget.unlisted.len(),
            budget
                .unlisted
                .iter()
                .map(|p| format!("  {}", p.display()))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    // Deliberately NOT a fixed floor: a constant would fail on a healthy tree
    // once the epic has retired enough shell, and blame the scope filter for
    // the success.
    //
    // The real bound on the production count lives in `measure`, against the
    // allowlist's non-`test` entries. An earlier version of THIS comment
    // claimed that bound already existed when nobody had written it, which left
    // a too-narrow filter free to manufacture a 34,000-line win with every gate
    // green. The comment was the bug.
    if budget.file_count() == 0 {
        return Err(
            "no production shell files were counted at all — the scope filter matched nothing, \
             and a gate that measures nothing passes for the wrong reason"
                .to_string(),
        );
    }
    Ok(())
}

/// How many allowlist entries are typed as production, i.e. not `test`.
///
/// Deliberately reads the TYPED category rather than applying
/// [`is_production_shell`]: the floor exists to catch that predicate being
/// wrong, so it cannot be computed with it. The category is an independent
/// signal — a human argues it in `scripts/shell-allowlist.txt` and CI enforces
/// that every script has one.
///
/// Verified to agree with [`is_production_shell`] on all 514 current entries
/// (236 vs 236, zero disagreements), so the floor is tight rather than slack.
fn production_entry_count(categories: &BTreeMap<String, String>) -> usize {
    categories.values().filter(|c| *c != "test").count()
}

/// The shared explanation, so both floors say the same thing.
fn production_filter_error(expected: usize, counted: usize) -> String {
    format!(
        "the allowlist types {expected} scripts as production (non-`test`) but the scope filter \
         counted only {counted} — `is_production_shell` is too narrow, and a filter that \
         silently drops production shell manufactures progress that did not happen"
    )
}

/// Measure the tree at a git revision rather than the working copy.
///
/// # Why this exists
///
/// A ratchet that compares against a committed baseline number breaks every
/// time `main` moves: the number is a snapshot of one tree, and any other tree
/// disagrees with it. That is not a theoretical cost — this baseline went stale
/// twice in one day, the role-prompt ratchet red-lined `main` at its own merge
/// commit for the same reason (#8105), and the standing instruction it produces
/// is "regenerate the baseline on every merge", which is a chore that teaches
/// people to regenerate without looking.
///
/// Comparing against the **merge-base** instead asks the only question the gate
/// actually cares about: *does this change add portable shell?* Whatever `main`
/// did in the meantime is not this change's doing and not this gate's business.
/// It needs no committed number, so it cannot go stale.
///
/// # Errors
/// When git cannot be run, or the revision does not resolve.
pub fn measure_at_rev(root: &Path, rev: &str) -> Result<Budget, String> {
    let git = |args: &[&str]| -> Result<String, String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .map_err(|e| format!("could not run git {args:?}: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    };

    let allowlist_text = git(&["show", &format!("{rev}:scripts/shell-allowlist.txt")])?;
    let categories = parse_allowlist(&allowlist_text);

    let listing = git(&["ls-tree", "-r", "--name-only", "-z", rev])?;
    let tracked: Vec<&str> = listing.split('\0').filter(|s| !s.is_empty()).collect();
    let shell: Vec<&str> = tracked
        .iter()
        .copied()
        .filter(|p| p.ends_with(".sh") && is_production_shell(p))
        .collect();

    // Same reasoning as the working-tree path, against THAT revision's own
    // allowlist — which is the only figure that can be right for a tree from
    // an arbitrary point in the epic's history.
    // Compare like with like: `shell` is the PRODUCTION subset, so the floor is
    // the production subset of that revision's allowlist, not its whole count.
    // (The first version of this compared against the full allowlist, which
    // includes every test script, and refused a perfectly good revision.)
    // Typed category, NOT is_production_shell. An earlier version filtered both
    // sides through the same predicate, so a bug in it cancelled out and the
    // floor could not see the thing it exists to see.
    let expected = production_entry_count(&categories);
    if shell.len() < expected {
        return Err(format!(
            "{rev}'s allowlist types {expected} scripts as production but only {} were found \
             there — either the revision query is broken or the scope filter is too narrow, and \
             an empty comparison must not read as no growth",
            shell.len()
        ));
    }

    let mut budget = Budget::default();
    for rel in shell {
        let text = git(&["show", &format!("{rev}:{rel}")])?;
        let n = code_lines(&text) as u64;
        match categories.get(rel) {
            Some(cat) => {
                *budget.by_category.entry(cat.clone()).or_default() += n;
                *budget.files_by_category.entry(cat.clone()).or_default() += 1;
                budget.by_file.insert((*rel).to_string(), (cat.clone(), n));
            }
            None => budget.unlisted.push(PathBuf::from(rel)),
        }
    }
    Ok(budget)
}

/// What this change should be measured against.
#[derive(Debug, Clone)]
pub struct Comparison {
    /// The revision to measure `before` at.
    pub rev: String,
    /// How to describe it to a human.
    pub desc: String,
}

/// Resolve the revision to compare against.
///
/// Normally the merge-base of `HEAD` and `base_ref` — which is precisely "the
/// tree this change started from", so the comparison measures the change and
/// nothing else.
///
/// **On the base branch itself** (a push to `main`, where the merge-base of
/// `HEAD` and `origin/main` is `HEAD`) that would compare a tree against
/// itself and pass vacuously. There, the analogous question is what the most
/// recent commit did, so it falls back to `HEAD~1`. Without this the gate
/// would be a no-op on every direct push, which is exactly where nobody is
/// reviewing.
///
/// # Errors
/// When git cannot be run at all, or `HEAD` has no parent to compare against.
pub fn comparison(root: &Path, base_ref: &str) -> Result<Comparison, String> {
    let git = |args: &[&str]| -> Option<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    };

    let head = git(&["rev-parse", "HEAD"]).ok_or_else(|| "could not resolve HEAD".to_string())?;

    // No common ancestor — a shallow clone, or an unrelated base. Falling back
    // to `base_ref` itself looks harmless and is not: it measures a DIFFERENT
    // tree, so a branch that adds shell can report a NEGATIVE delta and pass.
    // Reproduced in review: a depth-1 clone of a branch adding +300 portable
    // lines exited 0 with `-100`. Refuse instead — a comparison that cannot be
    // made must not read as "no growth".
    let Some(base) = git(&["merge-base", "HEAD", base_ref]) else {
        return Err(format!(
            "no merge-base between HEAD and {base_ref} — this is usually a shallow clone \
             (CI needs `fetch-depth: 0`) or an unrelated base ref. Refusing to compare against \
             {base_ref} directly: that measures a different tree, so a change that ADDS shell \
             can report a negative delta and pass."
        ));
    };

    if base == head {
        // We are ON the base branch. Measure what the tip commit did.
        let parent = git(&["rev-parse", "HEAD~1"]).ok_or_else(|| {
            format!(
                "HEAD is {base_ref} and has no parent to compare against — refusing to compare \
                 a tree with itself, which would pass without measuring anything"
            )
        })?;
        return Ok(Comparison {
            rev: parent.clone(),
            desc: format!(
                "HEAD~1 ({}) — on {base_ref}, so this measures the tip commit",
                &parent[..parent.len().min(8)]
            ),
        });
    }

    Ok(Comparison {
        rev: base.clone(),
        desc: format!("{base_ref} ({})", &base[..base.len().min(8)]),
    })
}

pub mod churn;
mod declaration;

pub use declaration::{
    parse_growth_declarations, GrowthDeclaration, MalformedDeclaration, GROWTH_TRAILER,
};

/// Read the `Shell-Budget-Growth:` declarations from every commit in
/// `base_rev..HEAD`.
///
/// # Why not `git interpret-trailers`
///
/// git only parses the message's FINAL paragraph, and that is the wrong rule
/// here for two reproduced reasons:
///
///  1. The repo's own commit shape breaks it. A declaration paragraph followed
///     by `Closes #N` and `Co-Authored-By:` puts the declaration outside the
///     final paragraph, so git returns nothing — and the build then fails with
///     a message about growth while the real problem is placement. That is the
///     "a typo degrades to no override" failure this whole change exists to
///     prevent.
///  2. It disagrees with itself across a merge. GitHub's squash of a
///     multi-commit PR concatenates each commit as `* subject` + body, so every
///     body trailer ends up mid-message. The gate would accept a PR and then
///     red-line `main` on the very commit it just approved — the #8073/#8105
///     failure mode the merge-base design was built to avoid.
///
/// So the rule is positional in a different way: **column 0, not inside a
/// fenced block, and never the subject line**. That keeps the protections git
/// was adopted for — an indented example does not declare (the defect that
/// made this PR grant itself 59 lines), a fenced example does not declare, and
/// a trailer written as a subject does not declare — while surviving both
/// shapes above.
///
/// Anything that looks like a declaration but sits in a position this does not
/// read is reported as MALFORMED rather than ignored, so it can never fail
/// silently.
///
/// # Errors
/// Returns a message when `git log` cannot be run.
pub fn collect_growth_declarations(
    root: &Path,
    base_rev: &str,
) -> Result<(Vec<GrowthDeclaration>, Vec<MalformedDeclaration>), String> {
    let out = Command::new("git")
        .current_dir(root)
        .args(["log", "--format=%x00%B", &format!("{base_rev}..HEAD")])
        .output()
        .map_err(|e| format!("could not run git log: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git log {base_rev}..HEAD failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }

    let text = String::from_utf8_lossy(&out.stdout);
    let mut ok = Vec::new();
    let mut bad = Vec::new();
    for message in text.split('\0').filter(|m| !m.trim().is_empty()) {
        let (mut o, mut b) = parse_growth_declarations(message);
        ok.append(&mut o);
        bad.append(&mut b);
    }
    Ok((ok, bad))
}

/// Everything `check_against_rev` needs beyond the two measurements.
#[derive(Debug, Default, Clone, Copy)]
pub struct GrowthContext<'a> {
    /// `Shell-Budget-Growth:` trailers found in the compared commit range.
    pub declared: &'a [GrowthDeclaration],
}

impl<'a> GrowthContext<'a> {
    /// A context with no declarations and no recategorisation — the shape
    /// every pre-#8154 caller had.
    #[must_use]
    pub const fn none() -> Self {
        Self { declared: &[] }
    }
}

/// Compare this tree against a base revision. `Ok` when the change does not
/// grow the portable pool.
///
/// # Errors
/// Returns the operator-facing explanation when it does.
pub fn check_against_rev(
    now: &Budget,
    before: &Budget,
    base_desc: &str,
    ctx: &GrowthContext<'_>,
) -> Result<(), String> {
    let declared = ctx.declared;
    // Portable growth is NOT overridable, deliberately. The trailer buys a
    // larger permanent floor, which is a cost the epic can price; it does not
    // buy more of the thing the epic exists to retire. #8154 asked only for the
    // floor and this keeps it there.
    if now.portable() > before.portable() {
        return Err(portable_growth_message(now, before, base_desc));
    }

    // Total is checked too, as a delta. Dropping it entirely (as the first cut
    // of this redesign did) opened two holes review reproduced: a 500-line
    // `bootstrap` script passed, and recategorising a 639-line file
    // `contract` -> `bootstrap` while ADDING 200 portable lines passed while
    // reporting -439. Portable is what the epic retires, but growth in the
    // permanent floor is still growth and should be deliberate.
    if now.total() > before.total() {
        let growth = now.total() - before.total();
        // Saturating: two `u64::MAX` declarations overflow. Debug panics
        // (fail-closed but ugly); release wraps to a small number and would
        // then REFUSE growth it should allow, which is the wrong failure.
        let allowed: u64 = declared
            .iter()
            .fold(0u64, |acc, d| acc.saturating_add(d.lines));

        // A declaration must not launder PORTABLE growth into the floor.
        //
        // The first cut of this vetoed a change that RECATEGORISED a script.
        // Review defeated that three ways without ever recategorising: `git mv`
        // the portable file and list the new path as `bootstrap`; delete it and
        // add an equivalent; or leave the allowlist untouched and simply move
        // the lines from a `contract` file into a `bootstrap` one. All three
        // produce category totals byte-identical to the legitimate "add 20
        // lines to a bootstrap script" case, so no rule over the two
        // category-level `Budget`s can tell them apart.
        //
        // The rule that actually closes it is per-FILE: a declaration applies
        // only when no file that was PORTABLE at the base lost code lines. A
        // file that shrank, or that vanished, is portable shell being retired —
        // and retiring portable shell in the same change that grows the floor
        // is exactly what makes the totals ambiguous. Once no portable file may
        // shrink, any new portable line necessarily raises `now.portable()`,
        // and the portable leg above fires.
        //
        // The cost is that "retire portable shell AND grow the floor" must be
        // two PRs. That is the same "split the change" this already asks for,
        // and each half is then reviewable on its own terms.
        let mut lost: Vec<(&String, u64, u64)> = Vec::new();
        for (path, (cat, before_lines)) in &before.by_file {
            if !PORTABLE.contains(&cat.as_str()) {
                continue;
            }
            // The file's category NOW matters as much as its line count. A
            // `contract` file relisted as `bootstrap` with identical lines has
            // lost every portable line it had — review reproduced exactly that
            // to launder 20 new portable lines past a declaration after the
            // first per-file cut shipped. Reading only the count re-opened the
            // hole the deleted `recategorised_since` used to cover.
            let now_lines = now
                .by_file
                .get(path)
                .filter(|(c, _)| PORTABLE.contains(&c.as_str()))
                .map_or(0, |(_, n)| *n);
            if now_lines < *before_lines {
                lost.push((path, *before_lines, now_lines));
            }
        }
        if !lost.is_empty() {
            let detail = lost
                .iter()
                .map(|(p, was, is)| {
                    if *is == 0 {
                        format!("    {p}  {was} -> gone")
                    } else {
                        format!("    {p}  {was} -> {is}")
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            return Err(format!(
                "This change adds {growth} code lines of production shell (vs {base_desc}: {} -> \
                 {}) while PORTABLE shell shrank or disappeared:\n\n{detail}\n\n\
                 A `{GROWTH_TRAILER}` declaration does not apply to a change that does both. \
                 Retiring portable lines while adding floor lines makes the two \
                 indistinguishable from simply moving them, so the declaration could buy new \
                 portable shell without the portable leg ever seeing it.\n\n\
                 Split the change: retire portable shell in one PR, grow the floor in another. \
                 Each is then reviewable on its own terms.",
                before.total(),
                now.total()
            ));
        }

        if allowed >= growth {
            return Ok(());
        }

        let declared_note = if declared.is_empty() {
            String::new()
        } else {
            let each = declared
                .iter()
                .map(|d| format!("    {} lines — {}", d.lines, d.reason))
                .collect::<Vec<_>>()
                .join("\n");
            format!(
                "\n\nYou declared {allowed} line(s), which is {} short:\n\n{each}\n\n\
                 Raise the declared count to cover the measured growth, or shrink the change.",
                growth - allowed
            )
        };

        return Err(format!(
            "This change adds {growth} code lines of production shell without growing the \
             portable pool (vs {base_desc}: {} -> {}), so the growth is in the permanent floor \
             (`bootstrap` / `vendored`).\n\n\
             That is allowed, and it is not free. The floor is what will still be shell when \
             the epic is done, so adding to it raises the finish line. If the script could be a \
             daemon subcommand instead, it should be.\n\n\
             If the growth is right, declare it on a line of its own in a commit BODY:\n\n\
             {GROWTH_TRAILER} {growth} lines — <why this must stay shell> (#<issue>)\n\n\
             It must start at column 0, outside any ``` fence, and must not be the commit \
             subject — an indented or fenced line is prose showing the format, not a \
             declaration using it. Anywhere in the body is fine; it does not have to be the \
             last paragraph. A near-miss is reported as malformed rather than ignored.\n\n\
             The declared count must cover the measured growth, and the reason must cite an \
             issue. Declared growth is not hidden: it stays in the running total and \
             `shell-budget` prints it.{declared_note}\n\n\
             Note this compares against the MERGE-BASE, so it is measuring what YOUR change \
             did.",
            before.total(),
            now.total()
        ));
    }

    Ok(())
}

/// The portable-growth explanation, split out so both callers read the same.
fn portable_growth_message(now: &Budget, before: &Budget, base_desc: &str) -> String {
    let mut grew: Vec<(&String, u64, u64)> = Vec::new();
    for (cat, lines) in &now.by_category {
        if !PORTABLE.contains(&cat.as_str()) {
            continue;
        }
        let was = before.by_category.get(cat).copied().unwrap_or(0);
        if *lines > was {
            grew.push((cat, was, *lines));
        }
    }
    let detail = grew
        .iter()
        .map(|(c, was, now)| format!("    {c:<12} {was} -> {now}  (+{})", now - was))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "This change adds {} code lines of PORTABLE shell (vs {base_desc}: {} -> {}).\n\n{detail}\n\n\
         Portable shell is what epic #7810 is retiring — `contract` + `hook-entry`, whose logic \
         moves into the daemon behind a stub. Adding to it works directly against the epic.\n\n\
         Options, best first:\n\n\
         \x20 1. Put the new logic in the daemon instead. That is the language policy\n\
         \x20    (.loom/docs/shell-language-policy.md) and it makes this gate a non-event.\n\
         \x20 2. Remove portable shell elsewhere in the same change to pay for it.\n\
         \x20 3. If the script must stay shell forever, it may belong in `bootstrap` or\n\
         \x20    `vendored` rather than `contract` — but that is a claim about the script,\n\
         \x20    argued in scripts/shell-allowlist.txt, not a way around this number.\n\n\
         Note this compares against the MERGE-BASE, so it is measuring what YOUR change did.\n\
         Whatever main did meanwhile is not your problem and not this gate's business.",
        now.portable() - before.portable(),
        before.portable(),
        now.portable()
    )
}

#[cfg(test)]
mod tests;
