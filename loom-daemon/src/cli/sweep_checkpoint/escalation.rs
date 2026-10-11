//! `doctor-attempt` / `doctor-rejection` / `doctor-route` (#11359): durable
//! GLM-Doctor rejection state kept in the sweep checkpoint's
//! `doctor_escalation` key. Decision logic is `loom_daemon::doctor_escalation`.
use std::io::Write;
use std::path::Path;

use loom_daemon::doctor_escalation::{self as de, Rejection, Route, State};
use serde_json::{json, Value};

use super::{invalid, io_error, number, persist, Result};

const KEY: &str = "doctor_escalation";

/// Checkpoint `write` rebuilds the whole record, so carry the escalation state
/// over it. A new lineage (Curator/Builder completion) resets it deliberately.
pub(super) fn carry_forward(target: &Path, record: &mut Value) {
    if matches!(record["phase"].as_str(), Some("curator-done" | "builder-done")) {
        return;
    }
    if let Some(state) = load(target)
        .ok()
        .flatten()
        .and_then(|r| r.get(KEY).cloned())
    {
        record[KEY] = state;
    }
}

fn load(target: &Path) -> Result<Option<Value>> {
    match std::fs::read(target) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(io_error),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_error(e)),
    }
}

fn flags(args: &[String]) -> Result<std::collections::HashMap<&str, &str>> {
    let mut map = std::collections::HashMap::new();
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let value = it
            .next()
            .ok_or_else(|| invalid("option requires a value"))?;
        match flag.as_str() {
            "--pr" | "--head" | "--runtime" | "--profile" | "--attempt" | "--cap" => {
                map.insert(flag.as_str(), value.as_str());
            }
            _ => return Err(invalid("unknown option")),
        }
    }
    Ok(map)
}

fn required<'a>(f: &'a std::collections::HashMap<&str, &str>, name: &str) -> Result<&'a str> {
    f.get(name)
        .copied()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| invalid(&format!("{name} is required")))
}

fn head_value(value: &str) -> Result<&str> {
    if value.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(value)
    } else {
        Err(invalid("--head must be a commit SHA"))
    }
}

pub(super) fn run(
    root: &Path,
    target: &Path,
    verb: &str,
    args: &[String],
    out: &mut impl Write,
) -> Result<()> {
    let f = flags(args)?;
    let pr = u64::from(number(required(&f, "--pr")?)?);
    let mut record = load(target)?;
    let mut state: State = record
        .as_ref()
        .and_then(|r| r.get(KEY).cloned())
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    match verb {
        "route" => {
            let attempt: u32 = number(required(&f, "--attempt")?)?;
            let cap: u32 = number(required(&f, "--cap")?)?;
            let line = match de::route(&state, pr, attempt, cap) {
                Route::Preference => "route=preference".to_owned(),
                Route::CapExhausted => "route=cap-exhausted".to_owned(),
                Route::Claude { reason } => format!("route=claude reason={reason}"),
            };
            return writeln!(out, "{line}").map_err(io_error);
        }
        "attempt" | "rejection" => {}
        _ => return Err(invalid("unknown doctor-* verb")),
    }
    // Without a checkpoint nothing can be persisted, so the policy is inactive.
    let Some(record) = record.as_mut() else {
        return writeln!(out, "inactive no-checkpoint").map_err(io_error);
    };
    let head = head_value(required(&f, "--head")?)?;
    let mut changed = true;
    let line = if verb == "attempt" {
        let glm = de::is_glm(required(&f, "--runtime")?, f.get("--profile").copied());
        de::record_doctor(&mut state, pr, head, glm);
        format!("recorded glm={glm}")
    } else {
        let k = de::parse_k(&loom_daemon::config_resolver::resolve_effective_config(root))
            .map_err(|m| (78, m))?;
        match de::record_rejection(&mut state, pr, head, k) {
            Rejection::Counted { count, escalated } => {
                format!("counted count={count} k={k} escalated={escalated}")
            }
            Rejection::Replay => {
                changed = false;
                format!("replay count={} k={k}", state.rejections)
            }
            Rejection::NotQualifying(why) => {
                changed = false;
                format!("not-counted {why}")
            }
        }
    };
    // Identity and count are one record, one atomic write.
    if changed {
        record[KEY] = json!(state);
        persist(target.parent().unwrap_or(root), target, record)?;
    }
    writeln!(out, "{line}").map_err(io_error)
}
