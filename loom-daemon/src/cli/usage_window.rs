use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

/// Resolve the range used by the Codex, OpenCode, and Pi usage commands.
pub(super) fn resolve(
    since: &str,
    until: Option<&str>,
    all_time: bool,
) -> Result<Option<(DateTime<Utc>, DateTime<Utc>)>> {
    if all_time {
        return Ok(None);
    }
    let now = Utc::now();
    let since = crate::sweep_outcome_summary::parse_since(since, now)?;
    let until = match until {
        Some(raw) => DateTime::parse_from_rfc3339(raw)
            .with_context(|| format!("--until {raw:?} is not RFC3339"))?
            .with_timezone(&Utc),
        None => now,
    };
    Ok(Some((since, until)))
}
