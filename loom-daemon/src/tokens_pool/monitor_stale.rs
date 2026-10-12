//! Is llm-monitor's `ranking.json` going stale? (#11295)
//!
//! loom-daemon posts `llm-monitor.stale` on loom-ui when it is
//! ([`crate::loom_ui_alerts::watches`]). A host with no `ranking.json` at all
//! does not run llm-monitor, and has nothing to say.
//!
//! llm-monitor's quota step-change alert (`evaluateStepChangeAlerts`) is not in
//! `ranking.json` yet (it reaches a log line and the macOS popover only), so
//! there is nothing to post for it until llm-monitor writes it there.

use std::path::Path;

use chrono::{DateTime, Utc};

use super::monitor::parse_iso8601;

/// A `ranking.json` this old is an alert. llm-monitor writes it every poll
/// (minutes); token selection already stops trusting it after ten minutes.
pub const STALE_AFTER_SECS: i64 = 30 * 60;

/// `Ok` when `<monitor_dir>/ranking.json` is fresh or absent, `Err(why)` when
/// it is stale, undated or unreadable.
///
/// # Errors
/// Stale, undated, unparseable or unreadable.
pub fn check(monitor_dir: &Path, now: DateTime<Utc>) -> Result<(), String> {
    let path = monitor_dir.join("ranking.json");
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("{} cannot be read: {e}", path.display())),
    };
    let data: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("{} is not JSON: {e}", path.display()))?;
    let generated = data.get("generated_at").and_then(serde_json::Value::as_str);
    let Some(at) = parse_iso8601(generated) else {
        return Err(format!("{} has no usable generated_at", path.display()));
    };
    let age = (now - at).num_seconds();
    if age > STALE_AFTER_SECS {
        return Err(format!(
            "{} was last written {} minutes ago (at {}); is llm-monitor running?",
            path.display(),
            age / 60,
            at.to_rfc3339()
        ));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn write(dir: &Path, body: &str) {
        std::fs::write(dir.join("ranking.json"), body).unwrap();
    }

    #[test]
    fn fresh_absent_stale_and_undated() {
        let dir = tempfile::tempdir().unwrap();
        let now = Utc::now();
        assert!(check(dir.path(), now).is_ok(), "absent: not monitored here");
        let at = |secs: i64| (now - chrono::Duration::seconds(secs)).to_rfc3339();
        write(dir.path(), &format!(r#"{{"schema":1,"generated_at":"{}"}}"#, at(60)));
        assert!(check(dir.path(), now).is_ok());
        write(
            dir.path(),
            &format!(r#"{{"schema":1,"generated_at":"{}"}}"#, at(STALE_AFTER_SECS + 60)),
        );
        assert!(check(dir.path(), now)
            .unwrap_err()
            .contains("31 minutes ago"));
        write(dir.path(), r#"{"schema":1}"#);
        assert!(check(dir.path(), now).unwrap_err().contains("generated_at"));
        write(dir.path(), "nope");
        assert!(check(dir.path(), now).is_err());
    }
}
