//! A Curator completion's pre-build estimate. Historical issues are never inferred.
pub mod completion;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A blind estimate of expected work, independent of error-cost routing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Estimate {
    pub difficulty: u8,
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
    pub model: Option<String>,
    pub runtime: Option<String>,
    pub estimated_at: DateTime<Utc>,
    pub completion_id: String,
    pub loom: crate::telemetry::provenance::Provenance,
}

/// Only standalone HTML comments count. Inline/code-fence examples never do.
/// Absence is legitimate historical data; malformed concrete markers fail closed.
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
        anyhow::ensure!(values.len() == 4, "estimate requires four fields");
        let get = |key| {
            values
                .get(key)
                .copied()
                .ok_or_else(|| anyhow::anyhow!("missing estimate field: {key}"))
        };
        let estimate = Estimate {
            difficulty: get("difficulty")?.parse()?,
            lines: get("lines")?.parse()?,
            files: get("files")?.parse()?,
            doctor_risk: get("doctor_risk")?.parse()?,
        };
        anyhow::ensure!((1..=5).contains(&estimate.difficulty), "difficulty outside 1..5");
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
    const MARKER: &str = "<!-- loom:estimate difficulty=3 lines=400 files=4 doctor_risk=0.2 -->";

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
            ("difficulty=3", "difficulty=0"),
            ("difficulty=3", "difficulty=6"),
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
}
