//! Aggregate ratchet on the shell epic #7810 is actually retiring (#8084).
//!
//! # Why
//!
//! Four scripts have been ported and roughly 2,200 lines of production shell
//! deleted. Over the same window the pool those ports are draining went **up**:
//! 38,374 lines immediately before the epic's first port commit, 38,439 now.
//! A net **+65**. More portable shell arrived than left, and nothing measured
//! it, so four merged ports read as progress.
//!
//! # Why `portable` and not the total
//!
//! The first cut of this gate ratcheted total production shell — 51,814 lines.
//! That number cannot reach zero and was never meant to: `bootstrap` (runs
//! before a `loom-daemon` binary exists, so requiring the binary to install
//! itself is circular) and `vendored` (owned upstream in rjwalters/repo) are
//! 13,230 lines of correctly permanent shell. Ratcheting "what we are retiring"
//! plus "what we are keeping" yields a figure with no target, so no run of it
//! ever says how far along the epic is.
//!
//! `portable` — `contract` + `hook-entry`, whose logic moves into the daemon
//! behind a stub — has a target: the ~145 lines of `stub` glue a ported file
//! leaves behind. `total` stays ratcheted so floor growth is visible rather
//! than free, but the floor is not debt.
//!
//! # Why the existing gates miss all of this
//!
//! `check-file-size-budget.sh` ratchets INDIVIDUAL files already over 1,000
//! lines; `check-shell-allowlist.sh` wants a category and reason per NEW
//! script. Neither constrains aggregate volume, and the aggregate is where the
//! growth is: 29,795 of the lines added since the 60-day mark arrived as 148
//! brand-new scripts, nearly all under the per-file threshold and each with a
//! defensible reason. `check-file-size-budget.sh`'s own header names the
//! failure mode — *"every individual addition is defensible while the aggregate
//! is the problem."*
//!
//! # What this is not
//!
//! Not a ban. Growth stays possible; it just has to be spelled out as a changed
//! number in `scripts/shell-budget-baseline.txt`, visible in the diff, instead
//! of arriving invisibly across a hundred files.
//!
//! # Updating
//!
//! ```text
//! UPDATE_SHELL_BUDGET=1 cargo test -p loom-daemon --test shell_budget_ratchet
//! ```
//!
//! Legitimate for recording shrinkage, or for a reviewed decision to admit
//! growth. A reviewer should treat an update that RAISES `portable` as the
//! thing to ask about. `origin_portable` is never regenerated — an origin that
//! moves measures nothing.

use std::path::PathBuf;

use loom_daemon::shell_budget;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("loom-daemon must have a parent directory")
        .to_path_buf()
}

fn baseline_path() -> PathBuf {
    repo_root().join("scripts/shell-budget-baseline.txt")
}

/// The ratcheted values, plus the never-regenerated origin.
struct Baseline {
    portable: u64,
    total: u64,
    files: u64,
    origin_portable: u64,
}

fn read_baseline() -> Baseline {
    let path = baseline_path();
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("baseline not found at {}: {e}", path.display()));
    let mut vals = std::collections::BTreeMap::<String, u64>::new();
    for l in text.lines() {
        let l = l.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        let mut it = l.split_whitespace();
        match (it.next(), it.next()) {
            (Some(k), Some(v)) => {
                let n = v
                    .parse()
                    .unwrap_or_else(|_| panic!("baseline key {k} has a non-numeric value {v:?}"));
                vals.insert(k.to_string(), n);
            }
            _ => panic!("unrecognised baseline line: {l:?}"),
        }
    }
    let get = |k: &str| {
        *vals
            .get(k)
            .unwrap_or_else(|| panic!("baseline is missing a `{k} <N>` entry"))
    };
    Baseline {
        portable: get("portable"),
        total: get("total"),
        files: get("files"),
        origin_portable: get("origin_portable"),
    }
}

#[test]
fn portable_shell_does_not_grow() {
    let root = repo_root();
    let budget = shell_budget::measure(&root).expect("measure");

    if std::env::var_os("UPDATE_SHELL_BUDGET").is_some() {
        let origin = shell_budget::read_origin_portable(&root)
            .expect("origin_portable must already exist — it is never regenerated");
        let text = std::fs::read_to_string(baseline_path()).expect("read baseline");
        let header: String = text
            .lines()
            .take_while(|l| l.trim_start().starts_with('#') || l.trim().is_empty())
            .map(|l| format!("{l}\n"))
            .collect();
        std::fs::write(
            baseline_path(),
            format!(
                "{header}portable {}\ntotal {}\nfiles {}\norigin_portable {origin}\n",
                budget.portable(),
                budget.total(),
                budget.file_count()
            ),
        )
        .expect("write baseline");
        eprintln!("{}", shell_budget::render_report(&budget, origin));
        return;
    }

    let base = read_baseline();

    // Always print it. A gate that only speaks up on failure teaches nobody
    // which way the number is moving, which is how +65 went unnoticed across
    // four merged ports.
    println!("{}", shell_budget::render_report(&budget, base.origin_portable));

    assert!(
        budget.unlisted.is_empty(),
        "{} production script(s) carry no allowlist entry, so every figure here is an \
         undercount — add them to scripts/shell-allowlist.txt:\n{}",
        budget.unlisted.len(),
        budget
            .unlisted
            .iter()
            .map(|p| format!("  {}", p.display()))
            .collect::<Vec<_>>()
            .join("\n")
    );

    assert!(
        budget.file_count() >= 150,
        "only {} production shell files were counted — the scope filter is too broad, and a \
         gate that measures almost nothing passes for the wrong reason",
        budget.file_count()
    );

    if budget.portable() > base.portable {
        let mut largest: Vec<_> = budget
            .by_category
            .iter()
            .filter(|(c, _)| shell_budget::PORTABLE.contains(&c.as_str()))
            .collect();
        largest.sort_by(|a, b| b.1.cmp(a.1));
        panic!(
            "PORTABLE shell grew by {} code lines ({} -> {}).\n\n\
             This is the pool epic #7810 is retiring, so adding to it works directly against \
             the epic. Options, best first:\n\n\
             \x20 1. Put the new logic in the daemon instead — that is the language policy\n\
             \x20    (.loom/docs/shell-language-policy.md) and it makes this gate a non-event.\n\
             \x20 2. Remove portable shell elsewhere to pay for it.\n\
             \x20 3. If the script genuinely must stay shell forever, it may belong in\n\
             \x20    `bootstrap` or `vendored` rather than `contract` — but that is a claim\n\
             \x20    about the script, argued in scripts/shell-allowlist.txt, not a way\n\
             \x20    around this number.\n\
             \x20 4. If the growth is genuinely right, record it:\n\
             \x20      UPDATE_SHELL_BUDGET=1 cargo test -p loom-daemon --test shell_budget_ratchet\n\
             \x20    and say WHY in the commit. A reviewer will see the raised number.\n\n\
             {}",
            budget.portable() - base.portable,
            base.portable,
            budget.portable(),
            shell_budget::render_report(&budget, base.origin_portable)
        );
    }

    assert!(
        budget.total() <= base.total,
        "total production shell grew by {} code lines ({} -> {}) without portable growing, so \
         the growth is in the permanent floor (bootstrap/vendored). That is allowed but not \
         free — record it deliberately and say why.\n\n{}",
        budget.total().saturating_sub(base.total),
        base.total,
        budget.total(),
        shell_budget::render_report(&budget, base.origin_portable)
    );

    // A large drop in file count with no corresponding line drop means the
    // filter stopped seeing whole directories.
    assert!(
        budget.file_count() + 20 >= base.files,
        "production shell file count fell from {} to {} — verify that is a real removal and \
         not a scope-filter regression",
        base.files,
        budget.file_count()
    );
}
