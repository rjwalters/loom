//! Calibration of Champion's merge-risk self-releases (#12952): the pure half
//! of `loom-daemon champion-hold self-release-calibration`. It parses the
//! recorded confidence out of a trusted self-release comment, applies the
//! maturity (7-day revert window) filter, recognises revert evidence and
//! buckets the rows by confidence. All forge/git I/O lives in
//! `cli/champion_hold_cmd.rs`.
//!
//! Revert detection is a heuristic (a `This reverts commit <sha>` message or a
//! `Reverts owner/repo#N` PR body); the verb's header says so.

use chrono::{DateTime, Duration, Utc};

use super::MERGE_RISK_SELF_RELEASE_MARKER;

/// The revert window a row must have fully elapsed before it is reported.
pub const REVERT_WINDOW_DAYS: i64 = 7;

/// Lower edges of the confidence buckets; the first is the 0.9 default
/// threshold, so a self-release at exactly 0.9 lands in the first bucket.
pub const BUCKET_EDGES: [f64; 2] = [0.90, 0.95];

/// The confidence in the last self-release record among `trusted` comment
/// bodies (oldest first) that has the marker and a valid `Confidence` line.
/// With several records (one per head) the last one is the one on the merged
/// head. `Err` when a marker is present but no confidence parses.
pub fn recorded_confidence(trusted: &[String]) -> Result<Option<f64>, String> {
    let mut found: Option<Result<f64, String>> = None;
    for body in trusted {
        if !body
            .trim_start()
            .starts_with(MERGE_RISK_SELF_RELEASE_MARKER)
        {
            continue;
        }
        found = Some(parse_confidence(body));
    }
    found.transpose()
}

/// Parse the `- **Confidence**: <0-1> (threshold ...)` line.
pub fn parse_confidence(body: &str) -> Result<f64, String> {
    let line = body
        .lines()
        .find_map(|l| l.trim_start().strip_prefix("- **Confidence**:"))
        .ok_or("self-release record has no Confidence line")?;
    let tok = line.split_whitespace().next().unwrap_or("");
    let v: f64 = tok
        .parse()
        .map_err(|_| format!("confidence {tok:?} is not a number"))?;
    if v.is_finite() && (0.0..=1.0).contains(&v) {
        Ok(v)
    } else {
        Err(format!("confidence {v} is outside [0,1]"))
    }
}

/// Whether the PR merged at `merged_at` has had its whole revert window by `now`.
pub fn is_mature(merged_at: DateTime<Utc>, now: DateTime<Utc>, min_age_days: i64) -> bool {
    now - merged_at >= Duration::days(min_age_days)
}

/// Whether `text` (a commit message or PR body) reverts `sha` / `pr`.
pub fn reverts(text: &str, sha: &str, repo: &str, pr: u64) -> bool {
    let lower = text.to_ascii_lowercase();
    let sha = sha.to_ascii_lowercase();
    if !sha.is_empty() && lower.contains(&format!("this reverts commit {sha}")) {
        return true;
    }
    let repo = repo.to_ascii_lowercase();
    lower
        .match_indices(&format!("reverts {repo}#{pr}"))
        .any(|(i, m)| {
            !lower[i + m.len()..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit())
        })
}

/// The bucket label a confidence falls in.
pub fn bucket(conf: f64) -> &'static str {
    if conf >= BUCKET_EDGES[1] {
        "[0.95,1.00]"
    } else {
        "[0.90,0.95)"
    }
}

/// Bucket labels in report order.
pub const BUCKETS: [&str; 2] = ["[0.90,0.95)", "[0.95,1.00]"];

/// One mature self-released PR.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub pr: u64,
    pub confidence: f64,
    pub merged_at: DateTime<Utc>,
    pub reverted: bool,
}

/// Summary of one bucket. `hit_rate` is the share NOT reverted (the release
/// call was right); `None` when the bucket is empty.
#[derive(Debug, Clone, PartialEq)]
pub struct BucketSummary {
    pub bucket: &'static str,
    pub n: usize,
    pub reverted: usize,
    pub hit_rate: Option<f64>,
}

/// Per-bucket counts and hit rates, in [`BUCKETS`] order. Confidences below
/// the first edge (a lowered threshold) fall in the first bucket.
pub fn summarize(rows: &[Row]) -> Vec<BucketSummary> {
    BUCKETS
        .iter()
        .map(|b| {
            let in_b: Vec<&Row> = rows.iter().filter(|r| bucket(r.confidence) == *b).collect();
            let n = in_b.len();
            let reverted = in_b.iter().filter(|r| r.reverted).count();
            BucketSummary {
                bucket: b,
                n,
                reverted,
                hit_rate: (n > 0).then(|| (n - reverted) as f64 / n as f64),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(conf: &str) -> String {
        format!(
            "{MERGE_RISK_SELF_RELEASE_MARKER}\n**Champion self-release** (head abc):\n\n- **Axis**: x\n- **Confidence**: {conf} (threshold 0.9)\n- **Evidence**: y\n"
        )
    }

    #[test]
    fn confidence_valid_and_invalid() {
        assert_eq!(parse_confidence(&rec("0.93")), Ok(0.93));
        assert!(parse_confidence(&rec("abc")).is_err());
        assert!(parse_confidence(&rec("1.5")).is_err());
        assert!(parse_confidence(&rec("-0.1")).is_err());
        assert!(parse_confidence(MERGE_RISK_SELF_RELEASE_MARKER).is_err());
    }

    #[test]
    fn recorded_confidence_uses_marker_and_last_record() {
        assert_eq!(recorded_confidence(&["hello".into()]), Ok(None));
        let v = vec![rec("0.91"), "noise".into(), rec("0.97")];
        assert_eq!(recorded_confidence(&v), Ok(Some(0.97)));
        assert!(recorded_confidence(&[rec("nan")]).is_err());
    }

    #[test]
    fn maturity_at_exactly_seven_days() {
        let now: DateTime<Utc> = "2026-10-31T00:00:00Z".parse().unwrap();
        assert!(is_mature("2026-10-24T00:00:00Z".parse().unwrap(), now, 7));
        assert!(!is_mature("2026-10-24T00:00:01Z".parse().unwrap(), now, 7));
    }

    #[test]
    fn revert_matching() {
        let sha = "deadbeef";
        assert!(reverts("Revert x\n\nThis reverts commit DEADBEEF.", sha, "o/r", 5));
        assert!(!reverts("This reverts commit cafe", sha, "o/r", 5));
        assert!(reverts("Reverts o/r#5", sha, "o/r", 5));
        assert!(!reverts("Reverts o/r#55", sha, "o/r", 5));
        assert!(!reverts("unrelated", sha, "o/r", 5));
    }

    #[test]
    fn buckets_and_hit_rate() {
        let t: DateTime<Utc> = "2026-10-01T00:00:00Z".parse().unwrap();
        let row = |pr, confidence, reverted| Row {
            pr,
            confidence,
            merged_at: t,
            reverted,
        };
        assert_eq!(bucket(0.90), "[0.90,0.95)");
        assert_eq!(bucket(0.95), "[0.95,1.00]");
        assert_eq!(bucket(1.0), "[0.95,1.00]");
        let s = summarize(&[row(1, 0.9, true), row(2, 0.92, false), row(3, 0.99, false)]);
        assert_eq!((s[0].n, s[0].reverted, s[0].hit_rate), (2, 1, Some(0.5)));
        assert_eq!((s[1].n, s[1].reverted, s[1].hit_rate), (1, 0, Some(1.0)));
        let z = summarize(&[]);
        assert_eq!((z[0].n, z[0].hit_rate), (0, None));
    }
}
