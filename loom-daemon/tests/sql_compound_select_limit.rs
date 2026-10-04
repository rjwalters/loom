//! Static guard: no committed observability SQL may build a compound SELECT of
//! more than 5 terms (#10066).
//!
//! Cloudflare D1 caps a compound SELECT at 5 terms and refuses a 6th with
//! `too many terms in compound SELECT`. `landed-size.sql` shipped two 6-term
//! `UNION ALL` lists that bundled SQLite (default limit 500) ran without
//! complaint, so CI was green while production refused the file and LSI could
//! not go live. The execution tests now apply the same ceiling
//! (`support/d1_sqlite.rs`), but they only run the files they seed. This guard
//! covers every `.sql` under `defaults/observability/`, including files no
//! test executes.
//!
//! Scope is the whole tree, ClickHouse files included. Picking the D1 files by
//! directory does not work (top-level `cycle-time-*.sql` is ClickHouse), an
//! explicit list goes stale when a file is added, and a 5-term ceiling costs a
//! ClickHouse file nothing.
//!
//! The count is per CHAIN, not per statement. A statement can hold several
//! independent chains (a 5-term CTE beside a 2-term recursive CTE is legal on
//! D1), so the scanner tracks parenthesis groups and counts compound operators
//! inside each group separately. Comments and quoted text are skipped, so
//! prose that names `UNION ALL` is not counted. A multi-row `VALUES` has no
//! compound operator and is exempt, in SQLite and in D1 alike; that is the
//! shape a lookup table of more than 5 rows must use.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

#[path = "support/d1_sqlite.rs"]
mod d1_sqlite;

use d1_sqlite::{d1_connection, D1_MAX_COMPOUND_SELECT_TERMS};

/// One compound chain: the line of its first operator and its term count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Chain {
    line: usize,
    terms: usize,
}

/// An open parenthesis group (or the statement itself, at depth 0).
#[derive(Default)]
struct Frame {
    operators: usize,
    first_operator_line: usize,
}

impl Frame {
    fn close(self, chains: &mut Vec<Chain>) {
        if self.operators > 0 {
            chains.push(Chain {
                line: self.first_operator_line,
                terms: self.operators + 1,
            });
        }
    }
}

/// Every compound chain in `sql`, with its term count. A chain is the set of
/// `UNION` / `UNION ALL` / `INTERSECT` / `EXCEPT` operators at one parenthesis
/// depth of one statement; `N` operators make `N + 1` terms.
fn compound_chains(sql: &str) -> Vec<Chain> {
    let bytes = sql.as_bytes();
    let mut chains = Vec::new();
    let mut stack = vec![Frame::default()];
    let mut line = 1;
    let mut i = 0;
    // Advance past the closing `quote` of a quoted span, counting newlines. A
    // doubled quote (`'it''s'`) reads as two adjacent spans, which is the
    // same thing for counting purposes.
    let skip_quoted = |i: &mut usize, line: &mut usize, quote: u8| {
        *i += 1;
        while *i < bytes.len() && bytes[*i] != quote {
            if bytes[*i] == b'\n' {
                *line += 1;
            }
            *i += 1;
        }
        *i += 1;
    };
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                line += 1;
                i += 1;
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                    if bytes[i] == b'\n' {
                        line += 1;
                    }
                    i += 1;
                }
                i += 2;
            }
            quote @ (b'\'' | b'"' | b'`') => skip_quoted(&mut i, &mut line, quote),
            b'(' => {
                stack.push(Frame::default());
                i += 1;
            }
            b')' => {
                assert!(stack.len() > 1, "unbalanced ')' at line {line}");
                stack.pop().unwrap().close(&mut chains);
                i += 1;
            }
            b';' => {
                assert_eq!(stack.len(), 1, "statement ends inside parentheses at line {line}");
                std::mem::take(&mut stack[0]).close(&mut chains);
                i += 1;
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                let word = &sql[start..i];
                if ["UNION", "INTERSECT", "EXCEPT"]
                    .iter()
                    .any(|op| word.eq_ignore_ascii_case(op))
                {
                    let frame = stack.last_mut().unwrap();
                    if frame.operators == 0 {
                        frame.first_operator_line = line;
                    }
                    frame.operators += 1;
                }
            }
            _ => i += 1,
        }
    }
    assert_eq!(stack.len(), 1, "unbalanced '(' at end of input");
    stack.pop().unwrap().close(&mut chains);
    chains
}

/// The chains in `sql` that D1 would refuse.
fn over_limit(sql: &str) -> Vec<Chain> {
    let max = usize::try_from(D1_MAX_COMPOUND_SELECT_TERMS).unwrap();
    compound_chains(sql)
        .into_iter()
        .filter(|chain| chain.terms > max)
        .collect()
}

fn observability_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../defaults/observability")
}

/// Every `.sql` file under `dir`, recursively, sorted for a stable report.
fn sql_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "sql") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// A 6-term `UNION ALL`: the shape `landed-size.sql` shipped and D1 refused.
const SIX_TERM_UNION_ALL: &str = "\
CREATE VIEW six_terms (k, v) AS
          SELECT '1', 1.0
UNION ALL SELECT '2', 1.3
UNION ALL SELECT '3', 2.2
UNION ALL SELECT '5', 3.4
UNION ALL SELECT '8', 5.1
UNION ALL SELECT '13', 8.2;
";

/// CAL3's shape: a 5-term chain and a separate 2-term recursive chain in ONE
/// statement. Six operators per statement, but no chain over 5 terms, so D1
/// accepts it and a per-statement count would be a false positive.
const FIVE_TERMS_BESIDE_A_CTE_CHAIN: &str = "\
WITH
    measured(m) AS (
        SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4
        UNION ALL SELECT 5
    ),
    newton(x) AS (
        SELECT 1
        UNION ALL SELECT x + 1 FROM newton WHERE x < 3
    )
SELECT m, x FROM measured, newton;
";

/// The replacement shape: a 6-row `VALUES` is not a compound chain.
const SIX_ROW_VALUES: &str = "\
CREATE VIEW six_rows (k, v) AS
VALUES
    ('1', 1.0),
    ('2', 1.3),
    ('3', 2.2),
    ('5', 3.4),
    ('8', 5.1),
    ('13', 8.2);
";

#[test]
fn the_guard_refuses_a_six_term_union_all() {
    assert_eq!(
        over_limit(SIX_TERM_UNION_ALL),
        [Chain { line: 3, terms: 6 }],
        "the guard does not see the 6-term chain D1 refuses (#10066)"
    );
    // The ceiling the guard enforces is the one the engine enforces: the same
    // fixture fails to parse on a connection with D1's limit applied.
    let error = d1_connection()
        .execute_batch(SIX_TERM_UNION_ALL)
        .expect_err("a 6-term compound SELECT must be refused under D1's limit");
    assert!(
        error
            .to_string()
            .contains("too many terms in compound SELECT"),
        "unexpected error for the 6-term fixture: {error}"
    );
}

#[test]
fn the_guard_counts_per_chain_not_per_statement() {
    assert_eq!(
        compound_chains(FIVE_TERMS_BESIDE_A_CTE_CHAIN),
        [Chain { line: 3, terms: 5 }, Chain { line: 8, terms: 2 }],
    );
    assert!(over_limit(FIVE_TERMS_BESIDE_A_CTE_CHAIN).is_empty());
    // And the engine agrees: D1's limit is per chain, so this runs.
    d1_connection()
        .execute_batch(FIVE_TERMS_BESIDE_A_CTE_CHAIN)
        .expect("a 5-term chain beside a 2-term chain is within D1's limit");
}

#[test]
fn multi_row_values_is_exempt() {
    assert!(compound_chains(SIX_ROW_VALUES).is_empty());
    let conn = d1_connection();
    conn.execute_batch(SIX_ROW_VALUES)
        .expect("a 6-row VALUES is within D1's limit (#10066)");
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM six_rows", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 6);
}

#[test]
fn prose_and_quoted_text_are_not_counted() {
    let sql = "\
-- UNION ALL UNION ALL UNION ALL UNION ALL UNION ALL UNION ALL
/* EXCEPT INTERSECT UNION UNION UNION UNION */
SELECT 'UNION UNION UNION UNION UNION UNION', \"union\" FROM t;
";
    assert!(compound_chains(sql).is_empty());
}

#[test]
fn no_observability_sql_exceeds_the_d1_compound_select_limit() {
    let root = observability_root();
    let files = sql_files(&root);
    // The walk must actually reach the D1 artifacts; an empty walk would pass
    // vacuously.
    for d1_file in [
        "issue-effort-queries.sql",
        "story-points-queries.sql",
        "story-points-calibration-queries.sql",
        "sweep-facts/sweep-facts-rollup.sql",
        "sweep-facts/sweep-facts-queries.sql",
        "sweep-facts/issue-effort.sql",
        "sweep-facts/landed-size.sql",
    ] {
        assert!(
            files.contains(&root.join(d1_file)),
            "the scan did not reach the D1 artifact {d1_file}"
        );
    }
    let mut violations = Vec::new();
    for file in &files {
        let sql = std::fs::read_to_string(file).unwrap();
        for chain in over_limit(&sql) {
            violations.push(format!(
                "  {}:{}: {} terms",
                file.strip_prefix(&root).unwrap().display(),
                chain.line,
                chain.terms
            ));
        }
    }
    assert!(
        violations.is_empty(),
        "compound SELECT over D1's {D1_MAX_COMPOUND_SELECT_TERMS}-term limit (D1 refuses the \
         file with `too many terms in compound SELECT`, #10066):\n{}\nRewrite a lookup list \
         as a multi-row `VALUES`, which is exempt from the limit.",
        violations.join("\n")
    );
}
