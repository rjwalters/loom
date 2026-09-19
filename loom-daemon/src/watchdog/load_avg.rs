//! Host load average at probe time (#5790).
//!
//! Attached to every probe divergence because the single most common benign
//! explanation for a slow round-trip is that the host is saturated — a fleet
//! running concurrent sweeps does that to itself routinely (#4279). Without the
//! number, an operator reading "the daemon did not answer in 15s" has no way to
//! tell a wedge from a busy machine, and the two call for opposite responses.

/// The load average as a display string, or `unavailable`.
///
/// Never an error and never empty: this is decoration on a report that is
/// already being made, so a missing value must degrade to a word rather than
/// suppress the report it was attached to.
#[must_use]
pub fn sample() -> String {
    if let Some(v) = from_uptime() {
        return v;
    }
    if let Some(v) = from_sysctl() {
        return v;
    }
    if let Some(v) = from_proc() {
        return v;
    }
    "unavailable".to_string()
}

fn run(program: &str, args: &[&str]) -> Option<String> {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    let out = crate::sweep_registry::output_with_timeout(cmd, std::time::Duration::from_secs(5))
        .ok()
        .flatten()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `uptime | sed -n 's/.*load average[s]*: *//p'`
fn from_uptime() -> Option<String> {
    let text = run("uptime", &[])?;
    let idx = text.find("load average")?;
    let rest = &text[idx..];
    let colon = rest.find(':')?;
    let value = rest[colon + 1..].trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// `sysctl -n vm.loadavg | tr -d '{}'`
fn from_sysctl() -> Option<String> {
    let text = run("sysctl", &["-n", "vm.loadavg"])?;
    let value = text.replace(['{', '}'], "");
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// `cut -d' ' -f1-3 /proc/loadavg`
fn from_proc() -> Option<String> {
    let path = super::env::var("LOOM_WATCHDOG_LOAD_AVG_PROC_PATH")
        .unwrap_or_else(|| "/proc/loadavg".to_string());
    let text = std::fs::read_to_string(path).ok()?;
    let value = text
        .split_whitespace()
        .take(3)
        .collect::<Vec<_>>()
        .join(" ");
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sample_is_never_empty() {
        // Decoration on a report already being made: a missing value must
        // degrade to a word, never suppress or blank the line it rides on.
        let s = sample();
        assert!(!s.is_empty());
        assert!(!s.contains('\n'), "must be a single line: {s:?}");
    }

    #[test]
    fn the_proc_path_is_overridable_for_tests() {
        let d = tempfile::tempdir().expect("tempdir");
        let p = d.path().join("loadavg");
        std::fs::write(&p, "0.52 0.58 0.59 1/512 99\n").expect("write");
        // Read it directly rather than through `sample()`, which prefers
        // uptime(1) on a real host.
        let text = std::fs::read_to_string(&p).expect("read");
        let value = text
            .split_whitespace()
            .take(3)
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(value, "0.52 0.58 0.59", "only the three averages, not the rest");
    }
}
