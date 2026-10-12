//! The `Forge egress:` block on `loom-daemon status` (#9984).
//!
//! Two facts, both client-side and host-local like the `Pending restart:`
//! line next to it: a **fresh `assert`** run in this CLI process (this
//! caller's own `gh` + `GH_CONFIG_DIR`), and the **daemon's last `doctor`**,
//! read from the cache the daemon writes (`<loom_dir>/forge-egress-doctor.json`,
//! [`loom_daemon::forge_egress::gate`]). The daemon's report is authoritative
//! for what dispatched work will hit; the fresh assert is what *this* shell
//! would hit.
//!
//! **Unconfigured is visible** (#10168): no policy ⇒ the `policy.unconfigured`
//! notice (non-fatal; a finding on a managed host) is printed and the JSON key
//! carries it.

use loom_daemon::forge_egress::{self, gate, report::Severity, Report};
use serde_json::{json, Value};

fn workspace() -> std::path::PathBuf {
    loom_daemon::repo_root::find_repo_root_from_cwd()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default()
}

fn verdict(code: i64) -> &'static str {
    match code {
        0 => "aligned",
        1 => "findings",
        _ => "verification incomplete",
    }
}

fn severity_mark(s: Severity) -> &'static str {
    match s {
        Severity::Notice => "NOTICE",
        Severity::Finding => "FINDING",
        Severity::Incomplete => "INCOMPLETE",
    }
}

fn cached() -> Option<Value> {
    gate::cache_path().and_then(|p| gate::read_cache(&p))
}

/// Pure rendering of the block (`None` ⇒ print nothing).
fn render(
    fresh: &Report,
    cache: Option<&Value>,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<String> {
    let mut out = Vec::new();
    if fresh.is_configured() {
        let j = fresh.to_json();
        let p = &j["policy"];
        out.push(format!(
            "Forge egress:  routing: {} (assert, this shell; enforcement {}) — policy {} ({})",
            verdict(i64::from(fresh.exit_code())),
            p["enforcement"].as_str().unwrap_or("required"),
            p["path"].as_str().unwrap_or("-"),
            p["origin"].as_str().unwrap_or("-"),
        ));
        for f in &fresh.routing {
            let mark = severity_mark(f.severity);
            let fix = f.to_json()["remedy"].as_str().unwrap_or("").to_string();
            out.push(format!("               {mark} {} — fix: {fix}", f.code));
        }
    } else {
        out.push(format!(
            "Forge egress:  no policy resolves for this shell (routing: {})",
            verdict(i64::from(fresh.exit_code()))
        ));
        for f in &fresh.routing {
            let fix = f.to_json()["remedy"].as_str().unwrap_or("").to_string();
            out.push(format!(
                "               {} {} — {}; fix: {fix}",
                severity_mark(f.severity),
                f.code,
                f.invariant
            ));
        }
    }
    if let Some(c) = cache {
        let r = &c["report"];
        let age = c["written_at"]
            .as_str()
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map_or_else(
                || "?".to_string(),
                |t| format!("{}m", (now - t.with_timezone(&chrono::Utc)).num_minutes().max(0)),
            );
        let codes: Vec<&str> = r["routing"]["findings"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|f| f["code"].as_str())
            .collect();
        out.push(format!(
            "               daemon doctor ({age} ago, pid {}): routing {}{} · git {} · runtime {} · telemetry {}",
            c["daemon_pid"],
            verdict(r["exit_code"].as_i64().unwrap_or(2)),
            if codes.is_empty() { String::new() } else { format!(" [{}]", codes.join(", ")) },
            verdict(r["git"]["exit_code"].as_i64().unwrap_or(2)),
            verdict(r["runtime"]["exit_code"].as_i64().unwrap_or(2)),
            verdict(r["telemetry"]["exit_code"].as_i64().unwrap_or(2)),
        ));
        // The daemon's findings + remedies, unless the fresh assert above
        // already printed the same code (the cached remedy is pre-redacted).
        let fresh_codes = fresh.routing_codes();
        for f in r["routing"]["findings"].as_array().into_iter().flatten() {
            let code = f["code"].as_str().unwrap_or("?");
            if fresh_codes.iter().any(|c| c == code) {
                continue;
            }
            let mark = if f["severity"].as_str() == Some("incomplete") {
                "INCOMPLETE"
            } else {
                "FINDING"
            };
            out.push(format!(
                "               {mark} {code} (daemon) — fix: {}",
                f["remedy"].as_str().unwrap_or("")
            ));
        }
    }
    Some(out.join("\n"))
}

/// The `hosts.yml` publication rollback line (#9986), `None` when the last
/// publication was not rolled back.
fn rollback_line(rollback: Option<&Value>) -> Option<String> {
    let r = rollback?;
    let codes: Vec<&str> = r["codes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    Some(format!(
        "               FINDING publication rolled back at {} for {} [{}] — previous profile restored",
        r["at"].as_str().unwrap_or("?"),
        r["dir"].as_str().unwrap_or("?"),
        codes.join(", ")
    ))
}

/// Print the block.
pub fn print() {
    let ws = workspace();
    if let Some(block) =
        render(&forge_egress::assert_for(&ws), cached().as_ref(), chrono::Utc::now())
    {
        println!("{block}");
        if let Some(line) = rollback_line(forge_egress::publication::read_rollback(&ws).as_ref()) {
            println!("{line}");
        }
    }
}

/// The `--json` counterpart.
#[must_use]
pub fn json() -> Value {
    let fresh = forge_egress::assert_for(&workspace());
    let cache = cached();
    json!({
        "assert": fresh.to_json(),
        "last_doctor": cache,
        "publication_rollback": forge_egress::publication::read_rollback(&workspace()),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use loom_daemon::forge_egress::policy::PolicySources;

    #[test]
    fn rollback_line_names_codes_and_dir_only() {
        let rec = json!({"dir": "/w/.loom/gh-config", "codes": ["apiconfig.api-host-missing"], "at": "t"});
        let line = rollback_line(Some(&rec)).unwrap();
        assert!(line.contains("apiconfig.api-host-missing") && line.contains("/w/.loom/gh-config"));
        assert!(rollback_line(None).is_none());
    }

    #[test]
    fn unconfigured_renders_the_notice_line() {
        let r = forge_egress::run_with(
            &PolicySources::default(),
            std::path::Path::new("/x"),
            forge_egress::Mode::Assert,
        );
        let out = render(&r, None, chrono::Utc::now()).unwrap();
        assert!(out.contains("no policy resolves"), "{out}");
        assert!(out.contains("NOTICE policy.unconfigured"), "{out}");
    }

    #[test]
    fn cached_findings_render_prominently_with_codes() {
        let r = forge_egress::run_with(
            &PolicySources::default(),
            std::path::Path::new("/x"),
            forge_egress::Mode::Assert,
        );
        let cache = json!({
            "written_at": "2026-10-03T00:00:00Z",
            "daemon_pid": 42,
            "report": {"exit_code": 1, "routing": {"exit_code": 1, "findings": [{"code": "toolchain.below-api-host-floor", "severity": "finding", "remedy": "upgrade to the pinned gh 2.102.0"}]},
                       "git": {"exit_code": 2}, "runtime": {"exit_code": 2}, "telemetry": {"exit_code": 0}},
        });
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-03T00:05:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let out = render(&r, Some(&cache), now).unwrap();
        assert!(out.contains("routing findings [toolchain.below-api-host-floor]"), "{out}");
        assert!(out.contains("5m ago, pid 42"), "{out}");
        assert!(
            out.contains(
                "FINDING toolchain.below-api-host-floor (daemon) — fix: upgrade to the pinned gh"
            ),
            "the remedy is shown: {out}"
        );
    }
}
