//! Every role queue query excludes `loom:frozen` (#11743, AC "Each role's queue
//! query excludes `loom:frozen` (a grep-able check in CI)").
//!
//! `loom:frozen` means "not now, don't touch": no agent may read a frozen item
//! as work, comment on it, or relabel it, and only a human removes the label.
//! The role prompts select work with `gh issue list` / `gh pr list` queries, so
//! a query that forgets the exclusion is exactly how a role would pick a frozen
//! item up again. This lint keeps every such query honest.
//!
//! # The rule
//!
//! Scanned: every regular `*.md` file in `defaults/.claude/commands/loom/`
//! (the `defaults/roles/*.md` symlinks point into it, so nothing is missed).
//!
//! Only **code inside fenced blocks** is read; prose and inline backticks are
//! out of scope (they are not run as written). Within a fence, a *statement*
//! is a line plus every following line it continues onto with a trailing `\`
//! or `|`. Comment-only lines start no statement.
//!
//! A statement is a **queue query** when it both
//! 1. lists items: `issue list`, `pr list`, `gh search issues|prs`, the
//!    REST `search/issues` endpoint, or a REST `…/issues?…labels=…` listing;
//!    and
//! 2. positively selects by a Loom label: `--label loom:…` / `--label "$VAR"`
//!    (a variable label is assumed to be a Loom one), a search qualifier
//!    `label:loom:…` that is not negated with `-`, or a REST `labels=loom:…`
//!    / `labels=$VAR` query parameter (`%3A` for the colon counts too).
//!
//! A queue query passes when the statement itself, or the comment lines
//! directly above it, contain one of:
//! - `loom:frozen` — an explicit `-label:loom:frozen`, a jq exclusion naming
//!   it, or a comment saying why this query includes frozen items on purpose
//!   (a dedup lookup or epic child discovery is not a work queue);
//! - `$EXCL`, when the same file sets `EXCL` from `skip-labels.sh` or
//!   `hard-exclusion-labels.sh` (both read the label registry, where
//!   `loom:frozen` is a hard exclusion);
//! - a direct call to `skip-labels.sh` or `hard-exclusion-labels.sh`;
//! - a reference (`$VAR`, `${VAR}`, `$ENV.VAR`) to a variable the same file
//!   sets from `loom-daemon labels list --property hold|park` on a line whose
//!   fallback also names `loom:frozen` (#11870). The registry marks
//!   `loom:frozen` as both a hold and a park, which
//!   `frozen_is_a_hold_and_a_park_in_the_registry` pins, so such a set always
//!   contains it.
//!
//! Known blind spots, by design: a query that selects only in its jq filter
//! (no `--label` / `labels=`), and queries in prose. Both are left to review rather than a
//! fuzzier heuristic that would flag non-queue `gh` calls.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

static LISTS_ITEMS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\b(?:issue|pr) list\b|search/issues|\bgh search (?:issues|prs)\b|/issues\?\S*\blabels=",
    )
    .unwrap()
});

/// `--label loom:x`, `--label="loom:x"`, `--label "$L"`, a non-negated
/// `label:loom:x` search qualifier, or a REST `labels=loom:x` / `labels=$L`
/// parameter.
static SELECTS_LABEL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"--label[= ]+["']?(?:loom:|\$)|(?:^|[\s"'+(])label:["']?loom:|[?&]labels=(?:loom(?::|%3A)|\$)"#,
    )
    .unwrap()
});

static EXCLUDES_FROZEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"loom:frozen|skip-labels\.sh|hard-exclusion-labels\.sh").unwrap());

static USES_EXCL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\$\{?EXCL\b").unwrap());

/// `VAR=$(loom-daemon labels list --property hold|park …|| printf '…loom:frozen…')`.
static SETS_REGISTRY_SET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?m)\b([A-Za-z_][A-Za-z0-9_]*)=\$\(\s*loom-daemon labels list --property (?:hold|park)\b[^\n]*loom:frozen",
    )
    .unwrap()
});

static SETS_EXCL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"EXCL=.*(?:skip-labels|hard-exclusion-labels)\.sh").unwrap());

/// One unfiltered queue query: 1-based line number and the joined statement.
#[derive(Debug)]
struct Violation {
    line: usize,
    statement: String,
}

fn is_fence(line: &str) -> bool {
    line.trim_start().starts_with("```")
}

fn is_comment(line: &str) -> bool {
    line.trim_start().starts_with('#')
}

fn continues(line: &str) -> bool {
    let t = line.trim_end();
    t.ends_with('\\') || t.ends_with('|')
}

/// Scans one markdown document and returns its unfiltered queue queries.
fn scan(doc: &str) -> Vec<Violation> {
    let lines: Vec<&str> = doc.lines().collect();
    let file_sets_excl = SETS_EXCL.is_match(doc);
    let registry_set_refs: Vec<Regex> = SETS_REGISTRY_SET
        .captures_iter(doc)
        .map(|c| {
            let v = regex::escape(&c[1]);
            Regex::new(&format!(r"\$\{{?{v}\b|\bENV\.{v}\b")).unwrap()
        })
        .collect();
    let mut out = Vec::new();
    let mut in_fence = false;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if is_fence(line) {
            in_fence = !in_fence;
            i += 1;
            continue;
        }
        if !in_fence || is_comment(line) || line.trim().is_empty() {
            i += 1;
            continue;
        }
        let start = i;
        let mut statement = line.to_string();
        while continues(lines[i]) && i + 1 < lines.len() && !is_fence(lines[i + 1]) {
            i += 1;
            statement.push('\n');
            statement.push_str(lines[i]);
        }
        i += 1;

        if !(LISTS_ITEMS.is_match(&statement) && SELECTS_LABEL.is_match(&statement)) {
            continue;
        }
        let mut leading = String::new();
        let mut j = start;
        while j > 0 && is_comment(lines[j - 1]) {
            j -= 1;
            leading.push_str(lines[j]);
            leading.push('\n');
        }
        let scope = format!("{leading}{statement}");
        let ok = EXCLUDES_FROZEN.is_match(&scope)
            || (file_sets_excl && USES_EXCL.is_match(&scope))
            || registry_set_refs.iter().any(|r| r.is_match(&scope));
        if !ok {
            out.push(Violation {
                line: start + 1,
                statement,
            });
        }
    }
    out
}

fn commands_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../defaults/.claude/commands/loom")
}

#[test]
fn every_role_queue_query_excludes_frozen() {
    let dir = commands_dir();
    let mut entries: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .filter(|p| !fs::symlink_metadata(p).unwrap().file_type().is_symlink())
        .collect();
    entries.sort();
    assert!(entries.len() > 10, "expected the role prompts under {}", dir.display());

    let mut report = String::new();
    let mut scanned_queries = 0usize;
    for path in &entries {
        let doc = fs::read_to_string(path).unwrap();
        scanned_queries += doc.matches("issue list").count() + doc.matches("pr list").count();
        for v in scan(&doc) {
            let name = path.file_name().unwrap().to_string_lossy();
            report.push_str(&format!("\n{name}:{}:\n{}\n", v.line, v.statement));
        }
    }
    assert!(scanned_queries > 0, "no `issue list` / `pr list` found: is the path right?");
    assert!(
        report.is_empty(),
        "role queue queries that do not exclude `loom:frozen` (#11743). Add \
         `-label:loom:frozen` to the search, a jq exclusion naming it, or route \
         the query through `$EXCL` from skip-labels.sh / hard-exclusion-labels.sh. \
         If the query includes frozen items on purpose (a dedup lookup, not a \
         work queue), say so in a comment naming `loom:frozen` directly above it.\n{report}"
    );
}

// ---- self-tests of the scanner ------------------------------------------------

#[test]
fn flags_unfiltered_label_query_in_a_fence() {
    let doc = "```bash\ngh issue list --label \"loom:issue\" --state open\n```\n";
    let v = scan(doc);
    assert_eq!(v.len(), 1, "{v:?}");
    assert_eq!(v[0].line, 2);
}

#[test]
fn flags_variable_label_and_positive_search_qualifier() {
    let doc = "```bash\nfor L in a b; do\n  gh issue list --label \"$L\" --json number\ndone\n\
               gh pr list --search \"is:open label:loom:pr\"\n```\n";
    assert_eq!(scan(doc).len(), 2);
}

#[test]
fn passes_explicit_search_exclusion_and_jq_exclusion() {
    let doc = "```bash\ngh issue list --label loom:issue --search \"-label:loom:frozen\"\n\
               gh pr list --label loom:pr --json number,labels \\\n  \
               --jq '.[] | select(any(.labels[]?.name; . == \"loom:frozen\") | not)'\n```\n";
    assert!(scan(doc).is_empty());
}

#[test]
fn passes_shared_filter_only_when_the_file_sets_it_from_the_registry_scripts() {
    let q = "gh issue list --label loom:issue --json number,labels --jq \".[] | select($EXCL)\"";
    let good = format!("```bash\nEXCL=\"$(./.loom/scripts/skip-labels.sh --jq-not)\"\n{q}\n```\n");
    assert!(scan(&good).is_empty());
    let bad = format!("```bash\nEXCL='true'\n{q}\n```\n");
    assert_eq!(scan(&bad).len(), 1);
}

#[test]
fn passes_documented_inclusion_in_the_leading_comment() {
    let doc = "```bash\n# loom:frozen phases count too: dedup, not a work queue.\n\
               X=$(gh issue list \\\n  --label=\"loom:epic-phase\" \\\n  --state=all)\n```\n";
    assert!(scan(doc).is_empty());
}

#[test]
fn ignores_prose_comments_and_negated_or_unlabelled_queries() {
    let doc = "Run `gh issue list --label loom:issue` by hand.\n\
               ```bash\n# gh issue list --label loom:issue\n\
               gh pr list --state open --json number\n\
               gh pr list --search \"-label:loom:blocked\"\n\
               gh issue edit 1 --add-label loom:issue\n```\n\
               gh issue list --label loom:issue\n";
    assert!(scan(doc).is_empty(), "{:?}", scan(doc));
}

#[test]
fn flags_a_rest_labels_listing_and_passes_its_jq_exclusion() {
    let q = "gh api --paginate \"repos/{owner}/{repo}/issues?state=open&labels=loom:issue\" \\\n  \
             --jq '[.[] | select(.pull_request == null)";
    assert_eq!(scan(&format!("```bash\n{q}]'\n```\n")).len(), 1);
    let ok = format!("```bash\n{q} | select(all(.labels[].name; . != \"loom:frozen\"))]'\n```\n");
    assert!(scan(&ok).is_empty());
    let enc = "```bash\ngh api \"repos/o/r/issues?labels=loom%3Apr&state=open\"\n```\n";
    assert_eq!(scan(enc).len(), 1);
    // A REST read with no label selector is not a queue query.
    assert!(scan("```bash\ngh api \"repos/o/r/issues?state=open\"\n```\n").is_empty());
}

#[test]
fn a_continuation_does_not_run_past_the_closing_fence() {
    let doc = "```bash\ngh issue list --state open |\n```\ngh issue list --label loom:issue\n";
    assert!(scan(doc).is_empty());
}

#[test]
fn passes_a_registry_hold_set_only_when_its_fallback_names_frozen() {
    let set = "HOLDS=$(loom-daemon labels list --property hold --with-retired 2>/dev/null || printf 'loom:blocked\\nloom:frozen'); export HOLDS";
    let q = "gh issue list --label loom:curated --json number,labels \\\n  --jq '($ENV.HOLDS | split(\"\\n\")) as $h | .[] | select(any(.labels[].name; IN($h[])) | not)'";
    assert!(scan(&format!("```bash\n{set}\n{q}\n```\n")).is_empty());
    let shell =
        "HQ=$(loom-daemon labels list --property park 2>/dev/null || printf 'loom:frozen')\n\
                 gh issue list --label loom:issue --search \"$(sed 's/^/-label:/' <<<\"$HQ\")\"";
    assert!(scan(&format!("```bash\n{shell}\n```\n")).is_empty());
    // A set whose fallback does not name loom:frozen does not count.
    let weak = "HOLDS=$(loom-daemon labels list --property hold 2>/dev/null || printf 'loom:blocked'); export HOLDS";
    assert_eq!(scan(&format!("```bash\n{weak}\n{q}\n```\n")).len(), 1);
    // Nor does an unrelated variable.
    let other = "HOLDSX=$(echo loom:frozen)";
    assert_eq!(scan(&format!("```bash\n{other}\n{q}\n```\n")).len(), 1);
}

#[test]
fn frozen_is_a_hold_and_a_park_in_the_registry() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../defaults/labels.json");
    let reg: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let labels = reg.get("labels").unwrap_or(&reg);
    let frozen = labels
        .as_array()
        .and_then(|a| a.iter().find(|l| l["name"] == "loom:frozen"))
        .or_else(|| labels.get("loom:frozen"))
        .expect("loom:frozen in defaults/labels.json");
    assert_eq!(frozen["hold"], true, "loom:frozen must stay a hold (#11870)");
    assert_eq!(frozen["park"], true, "loom:frozen must stay a park (#11870)");
}
