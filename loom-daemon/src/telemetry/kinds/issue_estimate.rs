//! A Curator completion's pre-build estimate. Historical issues are never inferred.
pub mod completion;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// An observational forecast of the landed change. No consumer may route,
/// gate or order on it: story points are the only decision-bearing size
/// (`docs/design/issue-sizing.md` §5c). Journaled records that still carry
/// the retired `difficulty` field deserialize; serde ignores it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Estimate {
    pub lines: u32,
    pub files: u32,
    pub doctor_risk: f64,
}

/// One completed curation; retries keep completion_id and estimated_at.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IssueEstimateRecord {
    pub repo: String,
    pub issue: u32,
    #[serde(flatten)]
    pub estimate: Estimate,
    /// The story points the `curation-complete` gate resolved (#11663).
    /// Absent on records journaled before the gate.
    #[serde(default)]
    pub points: Option<u32>,
    pub model: Option<String>,
    pub runtime: Option<String>,
    pub estimated_at: DateTime<Utc>,
    pub completion_id: String,
    pub loom: crate::telemetry::provenance::Provenance,
}

/// Only standalone HTML comments count. Inline/code-fence examples never do.
/// Absence is legitimate historical data; malformed concrete markers fail closed.
/// A legacy `difficulty=` field is accepted and ignored.
pub fn parse(body: &str) -> anyhow::Result<Option<Estimate>> {
    let mut fence: Option<(char, usize)> = None;
    let mut result = None;
    for line in body.lines() {
        let text = line.trim();
        let leading = text.chars().next().unwrap_or(' ');
        let count = text.chars().take_while(|c| *c == leading).count();
        if matches!(leading, '`' | '~') && count >= 3 {
            match fence {
                None => fence = Some((leading, count)),
                Some((ch, n)) if ch == leading && count >= n => fence = None,
                _ => {}
            }
            continue;
        }
        if fence.is_some()
            || line.starts_with("    ")
            || line.starts_with('\t')
            || !text.starts_with("<!-- loom:estimate")
        {
            continue;
        }
        anyhow::ensure!(result.is_none(), "duplicate concrete estimate marker");
        let fields = text
            .strip_prefix("<!-- loom:estimate ")
            .and_then(|s| s.strip_suffix(" -->"))
            .ok_or_else(|| anyhow::anyhow!("malformed estimate marker"))?;
        let mut values = std::collections::BTreeMap::new();
        for field in fields.split_whitespace() {
            let (key, value) = field
                .split_once('=')
                .ok_or_else(|| anyhow::anyhow!("malformed estimate field"))?;
            anyhow::ensure!(values.insert(key, value).is_none(), "duplicate estimate field");
        }
        values.remove("difficulty");
        anyhow::ensure!(values.len() == 3, "estimate requires lines, files and doctor_risk");
        let get = |key| {
            values
                .get(key)
                .copied()
                .ok_or_else(|| anyhow::anyhow!("missing estimate field: {key}"))
        };
        let estimate = Estimate {
            lines: get("lines")?.parse()?,
            files: get("files")?.parse()?,
            doctor_risk: get("doctor_risk")?.parse()?,
        };
        anyhow::ensure!(
            estimate.doctor_risk.is_finite() && (0.0..=1.0).contains(&estimate.doctor_risk),
            "risk outside 0..1"
        );
        result = Some(estimate);
    }
    Ok(result)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    const MARKER: &str = "<!-- loom:estimate lines=400 files=4 doctor_risk=0.2 -->";

    #[test]
    fn concrete_only_and_historical_absence() {
        assert_eq!(parse("historical issue").unwrap(), None);
        assert_eq!(
            parse(&format!("`{MARKER}`\n```html\n{MARKER}\n```\n    {MARKER}")).unwrap(),
            None
        );
        assert_eq!(parse(MARKER).unwrap().unwrap().lines, 400);
        assert!(parse(&format!("{MARKER}\n{MARKER}")).is_err());
    }

    #[test]
    fn invalid_values_are_rejected() {
        for (before, after) in [
            ("lines=400", "lines=-1"),
            ("files=4", "files=1.5"),
            ("doctor_risk=0.2", "doctor_risk=NaN"),
            ("doctor_risk=0.2", "doctor_risk=inf"),
            ("doctor_risk=0.2", "doctor_risk=-0.1"),
            ("doctor_risk=0.2", "doctor_risk=1.1"),
        ] {
            assert!(parse(&MARKER.replace(before, after)).is_err(), "{after}");
        }
    }

    #[test]
    fn three_field_marker_parses_and_legacy_difficulty_is_ignored() {
        let three = parse(MARKER).unwrap().unwrap();
        assert_eq!((three.lines, three.files, three.doctor_risk), (400, 4, 0.2));
        let legacy = "<!-- loom:estimate difficulty=3 lines=400 files=4 doctor_risk=0.2 -->";
        assert_eq!(parse(legacy).unwrap().unwrap(), three);
    }

    #[test]
    fn duplicate_unknown_and_missing_fields_still_error() {
        for marker in [
            "<!-- loom:estimate lines=1 lines=2 files=4 doctor_risk=0.2 -->",
            "<!-- loom:estimate difficulty=1 difficulty=2 lines=1 files=4 doctor_risk=0.2 -->",
            "<!-- loom:estimate size=3 lines=1 files=4 doctor_risk=0.2 -->",
            "<!-- loom:estimate difficulty=3 files=4 doctor_risk=0.2 -->",
        ] {
            assert!(parse(marker).is_err(), "{marker}");
        }
    }

    #[test]
    fn journaled_record_with_difficulty_and_without_points_deserializes() {
        let json = serde_json::json!({
            "repo": "o/r", "issue": 1, "difficulty": 2, "lines": 12, "files": 1,
            "doctor_risk": 0.1, "model": null, "runtime": "codex",
            "estimated_at": "2026-10-01T00:00:00Z",
            "completion_id": "91670540-172a-46f1-8278-3f12662fa371",
            "loom": crate::telemetry::provenance::Provenance::current(),
        });
        let r: IssueEstimateRecord = serde_json::from_value(json).unwrap();
        assert_eq!(r.estimate.lines, 12);
        assert_eq!(r.points, None);
    }
}
