//! The systemd unit no longer carries the dispatcher's autonomy env (#11951).
//!
//! # The ruling
//!
//! On a Linux (`systemd --user`) host, the host's own supervisor environment
//! owns the dispatcher's autonomy settings: a drop-in under
//! `~/.config/systemd/user/<unit>.d/`, or `.loom/config.json -> autonomous`.
//! The unit this script renders carries none of [`UNRENDERED_KEYS`]. launchd
//! plists on macOS are out of scope and unchanged: Mac holds depend on the
//! plist carrying the env.
//!
//! # Why
//!
//! The renderer harvested the start script's whole process env into the unit,
//! including its own FLAGS-OFF defaults. A start without flags therefore wrote
//! `LOOM_WORK_FINDER=0` into the base unit, and once the work finder is the
//! only item-dispatch path, that line is a full dispatch outage that nothing
//! reports: the daemon is healthy, role ticks run, and no sweep ever starts.
//! Who last ran the start script, with which flags, decided each host's
//! autonomy, and the carry-forward merge (#5344) kept whatever it wrote.
//!
//! # What this module does
//!
//! - [`is_unrendered_key`] is the renderer's and the carry-forward merge's
//!   filter: the keys are never rendered, and an installed unit's copies are
//!   purged on the next re-render, never carried forward ([`purge_notice`]).
//! - [`Layer`] reads what decides each loop once the unit is re-rendered: a
//!   drop-in value, else the config, else the daemon's default (off). The
//!   downgrade guard compares against that, not against lines in the base unit.
//! - [`unmet_intent`] refuses a start whose `--work-finder` / `--health-gate`
//!   (or exported `=1`) nothing on the host will honour, rather than starting
//!   a daemon with the loop silently off.
//! - [`effective_marker_value`] is what the autonomy-desired marker records:
//!   the value the started daemon actually runs with, not the start script's
//!   own resolved default.

use std::path::{Path, PathBuf};

use super::args::Want;
use super::out;

/// The keys a rendered systemd unit never carries (#11951).
pub const UNRENDERED_KEYS: [&str; 4] = [
    "LOOM_WORK_FINDER",
    "LOOM_MAIN_HEALTH_GATE",
    "LOOM_FORCE_SCOPE",
    "LOOM_GUARD_DECISION_LOG",
];

/// The two dispatch loops, with the flag that asks for each and the config key
/// that also enables it.
const LOOPS: [(&str, &str, &str); 2] = [
    ("LOOM_WORK_FINDER", "--work-finder", "autonomous.workFinder.enabled"),
    ("LOOM_MAIN_HEALTH_GATE", "--health-gate", "autonomous.mainHealthGate.enabled"),
];

/// Is `key` one the systemd renderer must not write?
#[must_use]
pub fn is_unrendered_key(key: &str) -> bool {
    UNRENDERED_KEYS.contains(&key)
}

/// The daemon's own truthiness test for a loop's env value.
fn truthy(v: &str) -> bool {
    matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

/// `<unit path>.d` — the user drop-in directory systemd reads for the unit.
#[must_use]
pub fn dropin_dir(unit_path: &Path) -> PathBuf {
    let mut s = unit_path.as_os_str().to_owned();
    s.push(".d");
    PathBuf::from(s)
}

/// Split one `Environment=` value into its `KEY=VALUE` assignments, the way
/// systemd does: whitespace separates them, double or single quotes group,
/// and a backslash escapes the next character.
#[must_use]
pub fn parse_environment(value: &str) -> Vec<(String, String)> {
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (_, '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
                in_word = true;
            }
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                in_word = true;
            }
            (None, c) if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            (None, c) => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    words
        .into_iter()
        .filter_map(|w| {
            let eq = w.find('=')?;
            Some((w[..eq].to_string(), w[eq + 1..].to_string()))
        })
        .collect()
}

/// The value `key` gets from the `*.conf` drop-ins in `dir`, applied in file
/// name order: a later assignment wins, and an empty `Environment=` resets
/// every earlier one. `None` when no drop-in sets it.
///
/// Reads the user drop-in directory only. `EnvironmentFile=` and system-wide
/// drop-in directories are not followed; a host that sets autonomy there is
/// read as "no drop-in", which makes the guard more cautious, never less.
#[must_use]
pub fn dropin_value(dir: &Path, key: &str) -> Option<String> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "conf") && p.is_file())
        .collect();
    files.sort();
    let mut value = None;
    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        for line in text.lines() {
            let Some(rest) = line.trim_start().strip_prefix("Environment=") else {
                continue;
            };
            if rest.trim().is_empty() {
                value = None;
                continue;
            }
            for (k, v) in parse_environment(rest) {
                if k == key {
                    value = Some(v);
                }
            }
        }
    }
    value
}

/// What decides each dispatch loop once the unit is re-rendered without it.
pub struct Layer {
    pub dropin_dir: PathBuf,
    /// Per [`LOOPS`] entry: the drop-in value, if a drop-in sets it.
    pub dropin: [Option<String>; 2],
    /// Per [`LOOPS`] entry: the resolved config's `enabled`, if set.
    pub config: [Option<bool>; 2],
}

impl Layer {
    /// Read the layer for the unit at `unit_path`, serving `repo_root`.
    #[must_use]
    pub fn read(unit_path: &Path, repo_root: &Path) -> Self {
        let dir = dropin_dir(unit_path);
        Self {
            dropin: LOOPS.map(|(k, _, _)| dropin_value(&dir, k)),
            config: [
                crate::work_finder::read_work_finder_config(repo_root).enabled,
                crate::main_health_gate::read_autonomous_gate_config(repo_root).enabled,
            ],
            dropin_dir: dir,
        }
    }

    fn index(key: &str) -> Option<usize> {
        LOOPS.iter().position(|(k, _, _)| *k == key)
    }

    /// The drop-in value for `key`, if one sets it.
    #[must_use]
    pub fn dropin_for(&self, key: &str) -> Option<&str> {
        self.dropin[Self::index(key)?].as_deref()
    }

    /// Will the daemon run with `key` on after the re-render? Env (drop-in)
    /// beats config, and both loops default off.
    #[must_use]
    pub fn on_after_render(&self, key: &str) -> bool {
        let Some(i) = Self::index(key) else {
            return false;
        };
        match &self.dropin[i] {
            Some(v) => truthy(v),
            None => self.config[i] == Some(true),
        }
    }

    /// Is `key` decided by something this start cannot change? A drop-in
    /// value is out of the start's reach, and so is a config that turns the
    /// loop on. Either way the start's own resolved `0` is never written, so
    /// there is nothing for the downgrade check to compare.
    #[must_use]
    pub fn decides(&self, key: &str) -> bool {
        self.dropin_for(key).is_some() || self.on_after_render(key)
    }
}

/// Print the purge notice for the autonomy lines an installed base unit
/// carries, which a re-render removes rather than carries forward.
pub fn purge_notice(installed: &Path, purged: &[(String, String)]) {
    out::warn("");
    out::warn(&format!(
        "NOTICE (#11951): removing {} autonomy env line(s) from the installed {}:",
        purged.len(),
        installed.display()
    ));
    for (k, v) in purged {
        out::warn(&format!("  - {k}={v}"));
    }
    out::warn("  A systemd unit no longer carries the dispatcher's autonomy settings. The");
    out::warn(&format!(
        "  host's supervisor env owns them: a drop-in under {},",
        dropin_dir(installed).display()
    ));
    out::warn("  or .loom/config.json -> autonomous (and guards.* for the two guard keys).");
}

/// The flag or export that asked for `key` on in THIS invocation, if any.
fn asked_on(want: Want, pre_exported: &str, flag: &str, key: &str) -> Option<String> {
    if want == Want::On {
        return Some(flag.to_string());
    }
    (want == Want::Unset && truthy(pre_exported)).then(|| format!("exported {key}={pre_exported}"))
}

/// A loop this invocation asked to be on that the re-rendered unit cannot turn
/// on, because nothing on the host sets it on. Prints one block per loop and
/// returns `true` when any was found, so the caller refuses a real start.
///
/// `--from-config` alone asks for nothing; it composes with the flags (#4353),
/// so `--from-config --work-finder` is still checked.
#[must_use]
pub fn unmet_intent(layer: &Layer, wants: [(Want, &str); 2]) -> bool {
    let mut found = false;
    for ((key, flag, config_key), (want, pre)) in LOOPS.into_iter().zip(wants) {
        let Some(asked) = asked_on(want, pre, flag, key) else {
            continue;
        };
        if layer.on_after_render(key) {
            continue;
        }
        found = true;
        let why = match layer.dropin_for(key) {
            Some(v) => format!("a drop-in sets {key}={v}"),
            None => "no drop-in sets it and the config does not enable it".to_string(),
        };
        out::warn("");
        out::warn(&format!(
            "WARNING (#11951): {asked} asks for {key}=1, but a systemd unit no longer carries"
        ));
        out::warn(&format!("  autonomy env, and {why}: the daemon would run with it OFF."));
        out::warn(&format!(
            "  Remediation: set Environment={key}=1 in a drop-in under {},",
            layer.dropin_dir.display()
        ));
        out::warn(&format!("  or {config_key}: true in .loom/config.json."));
    }
    found
}

/// The value the marker records for `key` after a systemd start: the started
/// daemon's own env when readable (`/proc/<pid>/environ`), else the drop-in
/// value, else what the daemon resolves from config (`1`/`0`), else `0`.
///
/// Recording the start script's resolved default here is what let a bad start
/// write `work_finder=0` that a later re-render replayed (#11951).
#[must_use]
pub fn effective_marker_value(main_pid: Option<&str>, layer: &Layer, key: &str) -> String {
    if let Some(v) = main_pid.and_then(|pid| proc_env_value(pid, key)) {
        return v;
    }
    if let Some(v) = layer.dropin_for(key) {
        return v.to_string();
    }
    if layer.on_after_render(key) { "1" } else { "0" }.to_string()
}

/// `key`'s value in `/proc/<pid>/environ`, or `None` when it is unset or the
/// file cannot be read (any non-Linux host). Reads one named key only.
fn proc_env_value(pid: &str, key: &str) -> Option<String> {
    if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let bytes = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    let prefix = format!("{key}=");
    bytes.split(|b| *b == 0).find_map(|entry| {
        let entry = String::from_utf8_lossy(entry);
        entry.strip_prefix(&prefix).map(ToString::to_string)
    })
}

#[cfg(test)]
#[path = "systemd_autonomy_tests.rs"]
mod tests;
