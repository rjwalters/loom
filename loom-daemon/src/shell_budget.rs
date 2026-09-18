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

/// One measurement of the tree.
#[derive(Debug, Clone, Default)]
pub struct Budget {
    /// Code lines per allowlist category.
    pub by_category: BTreeMap<String, u64>,
    /// Files per allowlist category.
    pub files_by_category: BTreeMap<String, u64>,
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

/// The commit-message trailer that declares deliberate growth in the permanent
/// floor (`bootstrap` / `vendored`).
///
/// The gate's failure message used to end "if that is right, say why in the
/// commit" while `check_against_rev` returned `Err` unconditionally — it
/// promised an escape hatch that did not exist, and three Judge-approved
/// safety PRs sat red against it with no in-repo remedy (#8154). This is that
/// hatch, made real and made narrow.
pub const GROWTH_TRAILER: &str = "Shell-Budget-Growth:";

/// A parsed `Shell-Budget-Growth:` trailer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrowthDeclaration {
    /// Lines of floor growth the author is declaring.
    pub lines: u64,
    /// The stated reason, verbatim after the line count.
    pub reason: String,
    /// The issue the reason references. Required — an override that cites no
    /// issue is a bare escape hatch, which is the thing this must not become.
    pub issue: u64,
}

/// Why a `Shell-Budget-Growth:` line was not accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MalformedDeclaration {
    /// The offending line, trimmed.
    pub line: String,
    /// What was wrong with it.
    pub why: &'static str,
}

/// Parse every `Shell-Budget-Growth:` trailer out of a block of commit
/// messages.
///
/// Returns the accepted declarations and, separately, the lines that look like
/// an attempt but are not usable. Malformed lines are reported rather than
/// ignored: a typo'd override that silently degrades to "no override" fails the
/// build with a message about growth, never about the typo, and the author
/// re-reads the wrong thing.
///
/// Accepted shape, liberal about the separator:
///
/// ```text
/// Shell-Budget-Growth: 59 lines — guard against silent revert of a local fix (#7870)
/// ```
#[must_use]
pub fn parse_growth_declarations(
    text: &str,
) -> (Vec<GrowthDeclaration>, Vec<MalformedDeclaration>) {
    let mut ok = Vec::new();
    let mut bad = Vec::new();

    for raw in text.lines() {
        // Column 0 only. Git trailers are unindented by convention, and an
        // INDENTED line in a commit body is prose showing the format, not a
        // declaration using it.
        //
        // This is not hypothetical: the commit that introduced this parser
        // contained an indented example of its own trailer, and an earlier cut
        // of this function trimmed first — so the PR granted itself 59 lines
        // of growth attributed to an unmerged issue, and a squash-merge would
        // have written that into `main`'s cumulative figure permanently. The
        // fix for "text that looks like documentation is read as enforcement"
        // cannot itself have that bug.
        if raw.starts_with([' ', '\t']) {
            continue;
        }
        let line = raw.trim_end();
        let Some(rest) = strip_trailer_prefix(line) else {
            continue;
        };
        let rest = rest.trim();

        // Leading integer: the declared line count.
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() {
            bad.push(MalformedDeclaration {
                line: line.to_string(),
                why: "no leading line count — expected `<n> lines — <reason> (#issue)`",
            });
            continue;
        }
        let Ok(lines) = digits.parse::<u64>() else {
            bad.push(MalformedDeclaration {
                line: line.to_string(),
                why: "line count does not fit in a u64",
            });
            continue;
        };

        let reason = rest[digits.len()..].trim_start();
        let reason = reason
            .strip_prefix("lines")
            .or_else(|| reason.strip_prefix("line"))
            .unwrap_or(reason);
        let reason = reason
            .trim_start()
            .trim_start_matches(['-', '\u{2014}', '\u{2013}', ':'])
            .trim();

        if reason.is_empty() {
            bad.push(MalformedDeclaration {
                line: line.to_string(),
                why: "no reason given after the line count",
            });
            continue;
        }

        let Some(issue) = first_issue_reference(reason) else {
            bad.push(MalformedDeclaration {
                line: line.to_string(),
                why: "reason cites no issue — an override must reference `#<issue>`",
            });
            continue;
        };

        ok.push(GrowthDeclaration {
            lines,
            reason: reason.to_string(),
            issue,
        });
    }

    (ok, bad)
}

/// Case-insensitive match on the trailer key, so `shell-budget-growth:` works.
fn strip_trailer_prefix(line: &str) -> Option<&str> {
    let key = GROWTH_TRAILER;
    // `get` rather than a slice: real commit messages are not ASCII, and
    // `line[..20]` panics outright when byte 20 lands inside a multi-byte
    // character. Scanning the epic's own history hit exactly that on an
    // em-dash — the unit tests were all ASCII and never saw it.
    let head = line.get(..key.len())?;
    if head.eq_ignore_ascii_case(key) {
        Some(&line[key.len()..])
    } else {
        None
    }
}

/// The first `#<digits>` in the text.
fn first_issue_reference(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if *b != b'#' {
            continue;
        }
        let digits: String = text[i + 1..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if !digits.is_empty() {
            return digits.parse().ok();
        }
    }
    None
}

/// Read every commit message in `base_rev..HEAD` and parse its trailers.
///
/// # Errors
/// Returns a message when `git log` cannot be run.
pub fn collect_growth_declarations(
    root: &Path,
    base_rev: &str,
) -> Result<(Vec<GrowthDeclaration>, Vec<MalformedDeclaration>), String> {
    let out = Command::new("git")
        .current_dir(root)
        .args(["log", "--format=%B", &format!("{base_rev}..HEAD")])
        .output()
        .map_err(|e| format!("could not run git log: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git log {base_rev}..HEAD failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(parse_growth_declarations(&String::from_utf8_lossy(&out.stdout)))
}

/// A script whose allowlist category changed between two revisions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recategorised {
    /// The script's path.
    pub path: String,
    /// Its category at the base revision.
    pub from: String,
    /// Its category now.
    pub to: String,
}

/// Scripts whose allowlist category differs between `base_rev` and the working
/// tree.
///
/// # Errors
/// Returns a message when the base revision's allowlist cannot be read.
pub fn recategorised_since(root: &Path, base_rev: &str) -> Result<Vec<Recategorised>, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["show", &format!("{base_rev}:scripts/shell-allowlist.txt")])
        .output()
        .map_err(|e| format!("could not run git show: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git show {base_rev}:scripts/shell-allowlist.txt failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let before = parse_allowlist(&String::from_utf8_lossy(&out.stdout));
    let now_text = std::fs::read_to_string(root.join("scripts/shell-allowlist.txt"))
        .map_err(|e| format!("could not read scripts/shell-allowlist.txt: {e}"))?;
    let now = parse_allowlist(&now_text);

    let mut moved: Vec<Recategorised> = Vec::new();
    for (path, cat_now) in &now {
        if let Some(cat_before) = before.get(path) {
            if cat_before != cat_now {
                moved.push(Recategorised {
                    path: path.clone(),
                    from: cat_before.clone(),
                    to: cat_now.clone(),
                });
            }
        }
    }
    Ok(moved)
}

/// Everything `check_against_rev` needs beyond the two measurements.
#[derive(Debug, Default, Clone, Copy)]
pub struct GrowthContext<'a> {
    /// `Shell-Budget-Growth:` trailers found in the compared commit range.
    pub declared: &'a [GrowthDeclaration],
    /// Scripts whose allowlist category moved in this change.
    pub recategorised: &'a [Recategorised],
}

impl<'a> GrowthContext<'a> {
    /// A context with no declarations and no recategorisation — the shape
    /// every pre-#8154 caller had.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            declared: &[],
            recategorised: &[],
        }
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
        // The total leg exists because moving a 639-line file `contract` ->
        // `bootstrap` while ADDING 200 portable lines makes NET portable fall,
        // so the portable leg above cannot see it; only the total rises. A
        // trailer that covers the total therefore buys exactly that case —
        // new portable shell, declared as floor growth. Review reproduced it.
        //
        // Recategorising is already disallowed by the policy, so the override
        // simply does not apply when any category moved. The growth is still
        // refusable on its merits; it just cannot be bought with a trailer.
        if !ctx.recategorised.is_empty() {
            let moved = ctx
                .recategorised
                .iter()
                .map(|r| format!("    {}  {} -> {}", r.path, r.from, r.to))
                .collect::<Vec<_>>()
                .join("\n");
            return Err(format!(
                "This change adds {growth} code lines of production shell (vs {base_desc}: {} -> \
                 {}) AND moves {} script(s) between allowlist categories:\n\n{moved}\n\n\
                 A `{GROWTH_TRAILER}` declaration does NOT apply to a change that recategorises. \
                 Moving a file out of `contract` while adding portable shell makes net portable \
                 FALL, so only the total rises — declaring that total would buy new portable \
                 shell, which no trailer may do.\n\n\
                 Split the change: recategorise in one PR (argued on its own), grow in another.",
                before.total(),
                now.total(),
                ctx.recategorised.len()
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
             If the growth is right, declare it with a commit trailer naming the amount and the \
             issue that argues it. It must start at column 0 — an indented line is prose \
             showing the format, not a declaration using it:\n\n\
             {GROWTH_TRAILER} {growth} lines — <why this must stay shell> (#<issue>)\n\n\
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
mod tests {
    use super::*;

    fn budget_of(pairs: &[(&str, u64)]) -> Budget {
        let mut b = Budget::default();
        for (c, n) in pairs {
            b.by_category.insert((*c).to_string(), *n);
            b.files_by_category.insert((*c).to_string(), 1);
        }
        b
    }

    fn budget_with(pairs: &[(&str, u64, u64)]) -> Budget {
        let mut b = Budget::default();
        for (c, lines, files) in pairs {
            b.by_category.insert((*c).to_string(), *lines);
            b.files_by_category.insert((*c).to_string(), *files);
        }
        b
    }

    #[test]
    fn adding_portable_shell_fails_and_names_the_category() {
        let before = budget_with(&[("contract", 100, 2), ("bootstrap", 50, 1)]);
        let now = budget_with(&[("contract", 130, 3), ("bootstrap", 50, 1)]);
        let err = check_against_rev(&now, &before, "origin/main (abc)", &GrowthContext::none())
            .expect_err("must fail");
        assert!(err.contains("adds 30 code lines of PORTABLE"), "{err}");
        assert!(err.contains("contract     100 -> 130"), "{err}");
        assert!(err.contains("MERGE-BASE"), "must say what it compared against: {err}");
    }

    #[test]
    fn growth_in_the_permanent_floor_is_caught_too() {
        // Regression: the first cut of the merge-base redesign dropped the
        // total check entirely, so a 500-line `bootstrap` script passed where
        // it used to fail. Portable is what the epic retires, but floor growth
        // raises the finish line and must be deliberate.
        let before = budget_with(&[("contract", 100, 2), ("bootstrap", 50, 1)]);
        let now = budget_with(&[("contract", 100, 2), ("bootstrap", 550, 2)]);
        let err = check_against_rev(&now, &before, "origin/main (abc)", &GrowthContext::none())
            .expect_err("must fail");
        assert!(err.contains("adds 500 code lines of production shell"), "{err}");
        assert!(err.contains("permanent floor"), "{err}");
    }

    #[test]
    fn recategorising_to_hide_an_addition_is_caught() {
        // Regression: moving a 639-line file `contract` -> `bootstrap` while
        // ADDING 200 portable lines reported -439 and passed. Portable falls,
        // but total rises, so the total leg catches it.
        let before = budget_with(&[("contract", 1000, 10), ("bootstrap", 50, 1)]);
        let now = budget_with(&[("contract", 561, 9), ("bootstrap", 889, 2)]);
        assert!(now.portable() < before.portable(), "portable falls, as in the report");
        let err = check_against_rev(&now, &before, "origin/main (abc)", &GrowthContext::none())
            .expect_err("must fail");
        assert!(err.contains("production shell"), "{err}");
    }

    #[test]
    fn a_change_that_removes_shell_passes() {
        let before = budget_with(&[("contract", 100, 2), ("bootstrap", 50, 1)]);
        let now = budget_with(&[("contract", 40, 1), ("bootstrap", 50, 1)]);
        assert!(check_against_rev(&now, &before, "base", &GrowthContext::none()).is_ok());
    }

    #[test]
    fn a_change_that_moves_shell_sideways_passes() {
        // Add and remove an equal amount: net zero, allowed by design — it is
        // option 2 in the failure message.
        let before = budget_with(&[("contract", 100, 2), ("hook-entry", 20, 1)]);
        let now = budget_with(&[("contract", 80, 2), ("hook-entry", 40, 2)]);
        assert_eq!(now.portable(), before.portable());
        assert!(check_against_rev(&now, &before, "base", &GrowthContext::none()).is_ok());
    }

    #[test]
    fn an_unchanged_tree_passes() {
        let b = budget_with(&[("contract", 100, 2)]);
        assert!(check_against_rev(&b, &b, "base", &GrowthContext::none()).is_ok());
    }

    // --- #8154: declared floor growth ---

    fn decl(lines: u64) -> Vec<GrowthDeclaration> {
        parse_growth_declarations(&format!(
            "Shell-Budget-Growth: {lines} lines — must stay shell, see (#7870)"
        ))
        .0
    }

    #[test]
    fn a_declaration_covering_the_growth_admits_floor_growth() {
        // The case #8154 was filed for: #7870 adds 59 lines to a `vendored`
        // script it cannot port (blocked upstream by #7758), and the only
        // alternative the gate offered was deleting the guard it was adding.
        let before = budget_with(&[("contract", 100, 2), ("vendored", 500, 1)]);
        let now = budget_with(&[("contract", 100, 2), ("vendored", 559, 1)]);
        assert!(check_against_rev(
            &now,
            &before,
            "base",
            &GrowthContext {
                declared: &decl(59),
                recategorised: &[]
            }
        )
        .is_ok());
    }

    #[test]
    fn a_declaration_larger_than_the_growth_also_admits_it() {
        // Declaring a ceiling and coming in under it is honest, not a defect.
        let before = budget_with(&[("bootstrap", 500, 1)]);
        let now = budget_with(&[("bootstrap", 520, 1)]);
        assert!(check_against_rev(
            &now,
            &before,
            "base",
            &GrowthContext {
                declared: &decl(59),
                recategorised: &[]
            }
        )
        .is_ok());
    }

    #[test]
    fn a_declaration_short_of_the_growth_still_fails_and_says_by_how_much() {
        // The override is a declared amount, not a blanket pass. Declaring 10
        // and growing 500 must not buy the other 490.
        let before = budget_with(&[("bootstrap", 50, 1)]);
        let now = budget_with(&[("bootstrap", 550, 2)]);
        let err = check_against_rev(
            &now,
            &before,
            "base",
            &GrowthContext {
                declared: &decl(10),
                recategorised: &[],
            },
        )
        .expect_err("must fail");
        assert!(err.contains("You declared 10 line(s)"), "{err}");
        assert!(err.contains("490 short"), "must name the shortfall: {err}");
    }

    #[test]
    fn undeclared_growth_remains_default_deny() {
        let before = budget_with(&[("bootstrap", 50, 1)]);
        let now = budget_with(&[("bootstrap", 550, 2)]);
        assert!(check_against_rev(&now, &before, "base", &GrowthContext::none()).is_err());
    }

    #[test]
    fn a_declaration_never_admits_portable_growth() {
        // The trailer buys a bigger permanent floor. It must not buy more of
        // the thing the epic exists to retire, or the gate is decorative.
        let before = budget_with(&[("contract", 100, 2)]);
        let now = budget_with(&[("contract", 130, 3)]);
        // Without this the test would pass vacuously if `decl` ever returned
        // an empty vec — it would then be asserting the default-deny path.
        assert_eq!(decl(9999).len(), 1, "the fixture must actually declare");
        let err = check_against_rev(
            &now,
            &before,
            "base",
            &GrowthContext {
                declared: &decl(9999),
                recategorised: &[],
            },
        )
        .expect_err("must fail");
        assert!(err.contains("PORTABLE"), "{err}");
    }

    #[test]
    fn the_failure_message_describes_the_format_the_code_accepts() {
        // The whole bug in #8154 was a message promising something the code did
        // not implement. Pin them together: the shape the message prints must
        // parse, and must then admit the growth it was printed for.
        let before = budget_with(&[("bootstrap", 50, 1)]);
        let now = budget_with(&[("bootstrap", 550, 2)]);
        let err = check_against_rev(&now, &before, "base", &GrowthContext::none())
            .expect_err("must fail");

        assert!(err.contains(GROWTH_TRAILER), "message must name the trailer: {err}");

        // Lift the literal template out of the message and make it real.
        let line = err
            .lines()
            .find(|l| l.contains(GROWTH_TRAILER))
            .expect("message must show the trailer line");
        let concrete = line
            .replace("<why this must stay shell>", "cannot be ported yet")
            .replace("<issue>", "8154");
        let (ok, bad) = parse_growth_declarations(&concrete);
        assert!(bad.is_empty(), "the message's own template must parse: {bad:?}");
        assert_eq!(ok.len(), 1, "from {concrete:?}");
        assert_eq!(ok[0].lines, 500, "must carry the measured growth");
        assert!(check_against_rev(
            &now,
            &before,
            "base",
            &GrowthContext {
                declared: &ok,
                recategorised: &[]
            }
        )
        .is_ok());
    }

    #[test]
    fn declarations_accumulate_across_commits_in_the_range() {
        let (ok, bad) = parse_growth_declarations(
            "feat: one\n\nShell-Budget-Growth: 30 lines — first half (#8154)\n\n             feat: two\n\nShell-Budget-Growth: 29 lines — second half (#8154)\n",
        );
        assert!(bad.is_empty(), "{bad:?}");
        assert_eq!(ok.len(), 2);
        let before = budget_with(&[("vendored", 500, 1)]);
        let now = budget_with(&[("vendored", 559, 1)]);
        assert!(check_against_rev(
            &now,
            &before,
            "base",
            &GrowthContext {
                declared: &ok,
                recategorised: &[]
            }
        )
        .is_ok());
    }

    #[test]
    fn a_declaration_without_an_issue_is_malformed_not_accepted() {
        // Ask #1: the reason must reference an issue, so the override cannot be
        // a bare escape hatch a Builder grants itself in passing.
        let (ok, bad) =
            parse_growth_declarations("Shell-Budget-Growth: 59 lines — because I said so");
        assert!(ok.is_empty(), "{ok:?}");
        assert_eq!(bad.len(), 1);
        assert!(bad[0].why.contains("cites no issue"), "{:?}", bad[0]);
    }

    #[test]
    fn a_declaration_without_a_count_or_reason_is_malformed() {
        let (ok, bad) = parse_growth_declarations(
            "Shell-Budget-Growth: lines — no count (#1)\nShell-Budget-Growth: 12 lines\n",
        );
        assert!(ok.is_empty(), "{ok:?}");
        assert_eq!(bad.len(), 2, "{bad:?}");
        assert!(bad[0].why.contains("no leading line count"), "{:?}", bad[0]);
        assert!(bad[1].why.contains("no reason"), "{:?}", bad[1]);
    }

    #[test]
    fn the_trailer_parses_across_plausible_separators_and_casing() {
        // Authors type what the message shows, but not byte-for-byte. An
        // em-dash, a hyphen, a colon and a lowercase key all mean the same
        // thing, and a near-miss that silently means "no override" is the
        // failure mode this whole issue is about.
        for body in [
            "Shell-Budget-Growth: 59 lines — why (#7870)",
            "Shell-Budget-Growth: 59 lines - why (#7870)",
            "Shell-Budget-Growth: 59 lines: why (#7870)",
            "Shell-Budget-Growth: 59 why (#7870)",
            "shell-budget-growth: 59 lines — why (#7870)",
            "Shell-Budget-Growth: 1 line — why (#7870)",
        ] {
            let (ok, bad) = parse_growth_declarations(body);
            assert!(bad.is_empty(), "{body:?} -> {bad:?}");
            assert_eq!(ok.len(), 1, "{body:?}");
            assert_eq!(ok[0].issue, 7870, "{body:?}");
        }
    }

    #[test]
    fn an_indented_trailer_is_prose_not_a_declaration() {
        // Review found this on the very PR that added the parser: the commit
        // message contained an INDENTED example of the trailer, the parser
        // trimmed before matching, and the PR granted itself 59 lines
        // attributed to an unmerged issue. A squash-merge would have written
        // that into `main`'s cumulative figure permanently.
        let body = "fix(shell-budget): make the message real\n\n\
                    If the growth is right, declare it like this:\n\n\
                    \x20   Shell-Budget-Growth: 59 lines — an example (#7870)\n\n\
                    That is all.\n";
        let (ok, bad) = parse_growth_declarations(body);
        assert!(ok.is_empty(), "an indented example must not declare: {ok:?}");
        assert!(bad.is_empty(), "nor should it be reported as malformed: {bad:?}");

        // The same text at column 0 IS a declaration — otherwise this test
        // would pass simply because the parser stopped working.
        let real = "Shell-Budget-Growth: 59 lines — an example (#7870)\n";
        assert_eq!(parse_growth_declarations(real).0.len(), 1);
    }

    #[test]
    fn a_tab_indented_trailer_is_also_prose() {
        let (ok, bad) = parse_growth_declarations("\tShell-Budget-Growth: 9 lines — x (#1)\n");
        assert!(ok.is_empty() && bad.is_empty(), "{ok:?} {bad:?}");
    }

    #[test]
    fn a_declaration_cannot_launder_portable_growth_through_recategorisation() {
        // Review reproduced this: +20 brand-new PORTABLE lines while flipping a
        // file `contract` -> `bootstrap` makes NET portable FALL, so the
        // portable leg cannot see it and only the total rises. A trailer
        // covering the total would then buy new portable shell — which no
        // trailer may do. It is exactly the hole the total leg was added to
        // close (`recategorising_to_hide_an_addition_is_caught`).
        let before = budget_with(&[("contract", 1000, 10), ("bootstrap", 50, 1)]);
        let now = budget_with(&[("contract", 381, 9), ("bootstrap", 709, 2)]);
        assert!(now.portable() < before.portable(), "net portable must fall");
        assert!(now.total() > before.total(), "only the total rises");

        let moved = [Recategorised {
            path: "defaults/scripts/c.sh".to_string(),
            from: "contract".to_string(),
            to: "bootstrap".to_string(),
        }];
        let d = decl(9999);
        let err = check_against_rev(
            &now,
            &before,
            "base",
            &GrowthContext {
                declared: &d,
                recategorised: &moved,
            },
        )
        .expect_err("a recategorising change must not be buyable");
        assert!(err.contains("recategorises"), "{err}");
        assert!(err.contains("defaults/scripts/c.sh"), "must name the file: {err}");
    }

    #[test]
    fn recategorisation_alone_without_growth_still_passes() {
        // The veto is scoped to the growth path. A pure recategorisation that
        // grows nothing is not this gate's business.
        let before = budget_with(&[("contract", 1000, 10), ("bootstrap", 50, 1)]);
        let now = budget_with(&[("contract", 361, 9), ("bootstrap", 689, 2)]);
        assert_eq!(now.total(), before.total());
        let moved = [Recategorised {
            path: "defaults/scripts/c.sh".to_string(),
            from: "contract".to_string(),
            to: "bootstrap".to_string(),
        }];
        assert!(check_against_rev(
            &now,
            &before,
            "base",
            &GrowthContext {
                declared: &[],
                recategorised: &moved
            },
        )
        .is_ok());
    }

    #[test]
    fn two_enormous_declarations_do_not_overflow() {
        let before = budget_with(&[("bootstrap", 50, 1)]);
        let now = budget_with(&[("bootstrap", 60, 1)]);
        let d = vec![
            GrowthDeclaration {
                lines: u64::MAX,
                reason: "a (#1)".into(),
                issue: 1,
            },
            GrowthDeclaration {
                lines: u64::MAX,
                reason: "b (#2)".into(),
                issue: 2,
            },
        ];
        // Debug would panic on a plain sum; release would wrap to a small
        // number and then REFUSE growth it should allow.
        assert!(check_against_rev(
            &now,
            &before,
            "base",
            &GrowthContext {
                declared: &d,
                recategorised: &[]
            },
        )
        .is_ok());
    }

    #[test]
    fn a_multibyte_char_at_the_key_boundary_does_not_panic() {
        // Regression: `strip_trailer_prefix` sliced `line[..20]`, and 20 bytes
        // into "docs(cache): measure — …" is the middle of an em-dash, so
        // scanning real history panicked. Every line here has a multi-byte
        // character straddling or near the key's byte length.
        let corpus = "docs(cache): measure — falsify the hypothesis\n\
                      fix: résumé the loop after a rollback — see #1\n\
                      — leading em-dash\n\
                      日本語のコミットメッセージです\n\
                      Shell-Budget-Growth: 9 lines — naïve café (#8154)\n";
        let (ok, bad) = parse_growth_declarations(corpus);
        assert!(bad.is_empty(), "{bad:?}");
        assert_eq!(ok.len(), 1, "{ok:?}");
        assert_eq!(ok[0].lines, 9);
        assert_eq!(ok[0].issue, 8154);
    }

    #[test]
    fn unrelated_prose_is_not_mistaken_for_a_declaration() {
        let (ok, bad) = parse_growth_declarations(
            "fix: mention Shell-Budget-Growth: in the docs\n\n             This commit talks about the trailer but does not declare one.\n",
        );
        // The mention is mid-line, so it is not a trailer at all.
        assert!(ok.is_empty(), "{ok:?}");
        assert!(bad.is_empty(), "{bad:?}");
    }

    // --- git-backed: comparison() resolution ---

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    fn init_repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().expect("tempdir");
        git(d.path(), &["init", "-q", "-b", "main"]);
        git(d.path(), &["config", "user.email", "t@example.com"]);
        git(d.path(), &["config", "user.name", "t"]);
        std::fs::write(d.path().join("a.txt"), "1\n").expect("write");
        git(d.path(), &["add", "-A"]);
        git(d.path(), &["commit", "-q", "-m", "c1"]);
        d
    }

    #[test]
    fn on_the_base_branch_it_compares_against_the_parent_not_itself() {
        let d = init_repo();
        std::fs::write(d.path().join("b.txt"), "2\n").expect("write");
        git(d.path(), &["add", "-A"]);
        git(d.path(), &["commit", "-q", "-m", "c2"]);
        let c = comparison(d.path(), "main").expect("comparison");
        assert!(c.desc.contains("HEAD~1"), "{}", c.desc);
    }

    #[test]
    fn a_root_commit_with_no_parent_errors_rather_than_comparing_with_itself() {
        // Comparing a tree with itself passes without measuring anything. On a
        // direct push that is a gate nobody is reviewing AND nothing is
        // checking, which is worse than the chore it replaced.
        let d = init_repo();
        let err = comparison(d.path(), "main").expect_err("must refuse");
        assert!(err.contains("no parent"), "{err}");
    }

    #[test]
    fn a_branch_compares_against_the_merge_base_not_the_advanced_tip() {
        let d = init_repo();
        git(d.path(), &["checkout", "-q", "-b", "feature"]);
        std::fs::write(d.path().join("f.txt"), "f\n").expect("write");
        git(d.path(), &["add", "-A"]);
        git(d.path(), &["commit", "-q", "-m", "feature work"]);
        // main advances independently.
        git(d.path(), &["checkout", "-q", "main"]);
        std::fs::write(d.path().join("m.txt"), "m\n").expect("write");
        git(d.path(), &["add", "-A"]);
        git(d.path(), &["commit", "-q", "-m", "main advances"]);
        let base_sha = String::from_utf8_lossy(
            &Command::new("git")
                .arg("-C")
                .arg(d.path())
                .args(["rev-parse", "HEAD~1"])
                .output()
                .expect("git")
                .stdout,
        )
        .trim()
        .to_string();
        git(d.path(), &["checkout", "-q", "feature"]);

        let c = comparison(d.path(), "main").expect("comparison");
        assert_eq!(c.rev, base_sha, "must pick the common ancestor, not main's tip");
    }

    #[test]
    fn an_unresolvable_base_errors_rather_than_measuring_a_different_tree() {
        // Falling back to the base ref looks harmless and is not: it measures a
        // DIFFERENT tree, so a branch that adds shell can report a negative
        // delta and pass. Reproduced in review on a depth-1 clone.
        let d = init_repo();
        let err = comparison(d.path(), "origin/nonexistent").expect_err("must refuse");
        assert!(err.contains("no merge-base"), "{err}");
        assert!(err.contains("fetch-depth"), "must name the usual cause: {err}");
    }

    #[test]
    fn the_production_floor_uses_the_typed_category_not_the_filter_it_guards() {
        // The floor exists to catch `is_production_shell` being wrong, so it
        // must not be computed with it. Review reproduced what happens when it
        // is: narrowing the filter to drop `defaults/` made every gate pass
        // while the report announced "net vs epic start -34222 — retired since
        // the epic began". A fabricated 34,000-line win, green.
        let mut cats = BTreeMap::new();
        cats.insert("defaults/scripts/a.sh".to_string(), "contract".to_string());
        cats.insert("defaults/scripts/b.sh".to_string(), "bootstrap".to_string());
        cats.insert("defaults/scripts/tests/test-a.sh".to_string(), "test".to_string());
        assert_eq!(
            production_entry_count(&cats),
            2,
            "counts the two non-test entries, regardless of what the path filter thinks"
        );
    }

    #[test]
    fn a_narrowed_scope_filter_is_reported_as_a_filter_bug_not_as_progress() {
        let msg = production_filter_error(236, 64);
        assert!(msg.contains("too narrow"), "{msg}");
        assert!(
            msg.contains("manufactures progress that did not happen"),
            "the message must name the consequence, not just the mismatch: {msg}"
        );
    }

    #[test]
    fn the_typed_category_and_the_path_filter_agree_on_the_real_allowlist() {
        // If these ever disagree, one of them is wrong and the floor becomes
        // either slack or a false alarm. Pinning the agreement makes that
        // visible the day it happens rather than the day it matters.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root");
        let text =
            std::fs::read_to_string(root.join("scripts/shell-allowlist.txt")).expect("allowlist");
        let cats = parse_allowlist(&text);
        let by_category = production_entry_count(&cats);
        let by_path = cats.keys().filter(|p| is_production_shell(p)).count();
        assert_eq!(
            by_category, by_path,
            "the allowlist's typed categories and is_production_shell disagree about which \
             scripts are production — one of them is wrong"
        );
    }

    #[test]
    fn portable_excludes_the_permanent_floor() {
        let b = budget_of(&[
            ("contract", 100),
            ("hook-entry", 10),
            ("bootstrap", 999),
            ("vendored", 999),
            ("stub", 5),
        ]);
        assert_eq!(b.portable(), 110, "only contract + hook-entry are retirable");
        assert_eq!(b.floor(), 1998);
        assert_eq!(b.total(), 2113);
    }

    #[test]
    fn the_report_says_plainly_when_the_pool_grew() {
        let b = budget_of(&[("contract", 38439), ("bootstrap", 100), ("stub", 145)]);
        let out = render_report(&b, 38374);
        assert!(out.contains("+65"), "{out}");
        assert!(out.contains("has GROWN"), "the direction must not be buried: {out}");
    }

    #[test]
    fn the_report_says_plainly_when_the_pool_shrank() {
        let b = budget_of(&[("contract", 30000), ("stub", 145)]);
        let out = render_report(&b, 38374);
        assert!(out.contains("-8374"), "{out}");
        assert!(out.contains("retired since"), "{out}");
    }

    #[test]
    fn unlisted_scripts_are_surfaced_because_they_make_the_count_wrong() {
        let mut b = budget_of(&[("contract", 10)]);
        b.unlisted
            .push(PathBuf::from("defaults/scripts/mystery.sh"));
        let out = render_report(&b, 10);
        assert!(out.contains("WARNING"), "{out}");
        assert!(out.contains("undercount"), "{out}");
        assert!(out.contains("mystery.sh"), "{out}");
    }

    #[test]
    fn code_lines_matches_the_file_size_ratchets_rule() {
        assert_eq!(code_lines("#!/usr/bin/env bash\n\nset -e\n# note\nfoo\n"), 2);
        assert_eq!(code_lines("   # indented comment\n"), 0);
        assert_eq!(code_lines("  echo hi   # trailing comment\n"), 1);
    }

    #[test]
    fn the_scope_rule_is_not_fooled_by_substrings() {
        assert!(is_production_shell("defaults/scripts/latest/thing.sh"));
        assert!(is_production_shell("defaults/scripts/testable.sh"));
        assert!(!is_production_shell("defaults/scripts/tests/test-x.sh"));
        assert!(!is_production_shell("scripts/test-installer.sh"));
        assert!(!is_production_shell(".loom/hooks/guard.sh"));
    }

    #[test]
    fn an_allowlist_line_needs_both_a_path_and_a_category() {
        let m = parse_allowlist("a.sh contract reason here\n# comment\n\nb.sh\nc.sh bootstrap\n");
        assert_eq!(m.get("a.sh").map(String::as_str), Some("contract"));
        assert_eq!(m.get("c.sh").map(String::as_str), Some("bootstrap"));
        assert!(!m.contains_key("b.sh"), "a path with no category is not an entry");
    }
}
