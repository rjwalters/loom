//! Shared fixtures for the story-points calibration contract tests
//! (`static_contracts` + `execution`). Split from a single 1096-line file to
//! stay under the file-size ratchet (`check-file-size-budget.sh`); nothing
//! here changes the contracts — see the two sibling modules.
#![allow(clippy::unwrap_used)]

pub const QUERIES: &str =
    include_str!("../../../defaults/observability/story-points-calibration-queries.sql");
pub const ROLLUP: &str =
    include_str!("../../../defaults/observability/sweep-facts/sweep-facts-rollup.sql");
/// The canonical question IDs. Restated here deliberately (the
/// [`cycle_time_artifacts`]/[`sweep_facts_artifacts`] pattern): this is the
/// one place the *set* is pinned, and every artifact is checked against it
/// rather than against another artifact, so two files cannot drift together.
pub const QUESTION_IDS: &[&str] = &["CAL1", "CAL2", "CAL3", "CAL4", "CAL5", "CAL6"];

/// One CAL query's result: column names plus every row, each cell rendered
/// as text ("(null)" for SQL NULL) so assertions read like the wrangler
/// output an operator would paste into the evidence trail.
#[derive(Debug)]
pub struct CalResult {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

impl CalResult {
    pub fn cell(&self, row: usize, column: &str) -> String {
        let at = self
            .headers
            .iter()
            .position(|header| header == column)
            .unwrap_or_else(|| panic!("no column '{column}' in {:?}", self.headers));
        self.rows[row][at].clone()
    }

    /// The single row whose `column` equals `value` (numbers-as-text compare).
    pub fn one(&self, column: &str, value: &str) -> &[String] {
        self.rows
            .iter()
            .find(|row| {
                let at = self
                    .headers
                    .iter()
                    .position(|header| header == column)
                    .unwrap_or_else(|| panic!("no column '{column}'"));
                row[at] == value
            })
            .unwrap_or_else(|| panic!("no row with {column} = {value}"))
    }
}

/// Split a .sql file into (offset, statement) pairs at `;` boundaries
/// (semicolon inside a quoted literal is not a terminator; the calibration
/// file has none, and the fixture stubs are written to keep that true).
pub fn split_statements(sql: &str) -> Vec<(usize, &str)> {
    #[derive(Clone, Copy, PartialEq)]
    enum State {
        Normal,
        LineComment,
        BlockComment,
        SingleQuoted,
        DoubleQuoted,
    }
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut state = State::Normal;
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        let (next, step) = match (state, bytes[i]) {
            (State::Normal, b'-') if i + 1 < bytes.len() && bytes[i + 1] == b'-' => {
                (State::LineComment, 2)
            }
            (State::Normal, b'/') if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                (State::BlockComment, 2)
            }
            (State::Normal, b'\'') => (State::SingleQuoted, 1),
            (State::Normal, b'"') => (State::DoubleQuoted, 1),
            (State::Normal, b';') => {
                out.push((start, &sql[start..i]));
                start = i + 1;
                (State::Normal, 1)
            }
            (State::LineComment, b'\n') => (State::Normal, 1),
            (State::BlockComment, b'*') if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                (State::Normal, 2)
            }
            (State::SingleQuoted, b'\'') if i + 1 < bytes.len() && bytes[i + 1] == b'\'' => {
                (State::SingleQuoted, 2)
            }
            (State::SingleQuoted, b'\'') => (State::Normal, 1),
            (State::DoubleQuoted, b'"') => (State::Normal, 1),
            _ => (state, 1),
        };
        state = next;
        i += step;
    }
    if !sql[start..].trim().is_empty() {
        out.push((start, &sql[start..]));
    }
    out
}
