//! Best-effort attribution of host CPU load to the processes producing it
//! (Issue #8478).
//!
//! # Why this exists
//!
//! [`crate::admission_brake`] already knows two things when it escalates to
//! `STARVING`: the host is over its load-per-core threshold, and **zero sweeps
//! are in flight**. Since #6102 it also names Loom's own concurrent role-runner
//! agents. What it has never been able to say is *whose load it is* when the
//! answer is "nobody's, as far as Loom is concerned".
//!
//! On 2026-09-20 that gap cost twelve hours. A Builder on a macOS dispatch host
//! escaped its scheduling band with `launchctl submit` — which creates a
//! **KeepAlive** launchd job, so launchd re-ran the one-shot scripts every time
//! they exited. Long after the owning sweep had ended, 25 `ngspice` processes
//! (reparented to `launchd`, owned by no session) held an 18-core host at load
//! average 58. The brake did exactly what it was built to do: `187 deferred
//! (host saturated)`, then `STARVING`, then `STARVATION ESCAPE HATCH`. Every one
//! of those lines was correct and none of them named `ngspice`. The diagnosis
//! required a human running `ps` by hand.
//!
//! This module closes that: it turns "held admission for 12h04m with 0 sweeps in
//! flight" into "...and the CPU belongs to `ngspice ×25`, parent `launchd[1]`,
//! reparented to pid 1". See `.loom/docs/long-running-compute.md` for the rule
//! that forbids creating such a process in the first place.
//!
//! # Best-effort, always
//!
//! Attribution is a **diagnostic garnish on a log line**, never a precondition
//! for emitting it. Every failure mode — no `ps` on the host, a `ps` that hangs,
//! unparseable output, a platform whose `ps` takes different flags — resolves to
//! `None`, and the caller logs its unattributed message exactly as it did before
//! this module existed. That is why the public entry point
//! ([`attribution_clause`]) returns a `String` that is simply empty on failure:
//! the call site cannot accidentally propagate an error into the hold path.
//!
//! The subprocess runs under [`crate::proc_exec::run_bounded`] with
//! [`PROBE_TIMEOUT`], so a wedged `ps` bounds at a fraction of a second rather
//! than stalling the work-finder tick that is asking.
//!
//! # `%cpu` is not instantaneous, and that is fine here
//!
//! `ps`'s `pcpu` column is an average, not a sample: lifetime-average on Linux
//! (procps), a decaying recent average on macOS/BSD. For *ranking* the handful
//! of processes eating a saturated host — the only question asked here — either
//! is adequate, and both are vastly better than the nothing this replaces. It is
//! deliberately **not** used for any decision: no hold, no release, no escape
//! hatch, no cap consults this module. It only ever appends words to a message.
//!
//! # Deliberately unfiltered
//!
//! It would be easy to subtract "Loom's own" processes and report only the
//! remainder as foreign. That was rejected: the incident's `ngspice` processes
//! *were* originally spawned by a Loom Builder, and were foreign by the time
//! they mattered precisely because the session that spawned them was gone. Any
//! ownership filter keyed on process name would have hidden the culprit. So the
//! report is the unfiltered top-N by CPU, **annotated** with each group's parent
//! command — the parent chain is what distinguishes a live Loom session's child
//! (`parent claude[9001]`) from an escaped one (`parent launchd[1]`, reparented
//! to pid 1). Naming the load and letting the reader judge ownership beats
//! guessing at ownership and naming nothing.

use std::process::Command;
use std::time::Duration;

/// How long the `ps` probe may take before its process group is terminated and
/// the attribution is abandoned. Short on purpose: the caller is a work-finder
/// tick, and a missing garnish costs nothing while a stalled tick costs
/// dispatch.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// How many command groups the rendered clause names. Three is enough to
/// identify a runaway fan-out (the incident's single `ngspice` group would have
/// been first) without turning one log line into a process listing.
pub const TOP_N: usize = 3;

/// Groups whose summed `%cpu` is below this are dropped before rendering — on a
/// saturated host they are noise, and the whole point of the clause is to name
/// the few processes that matter.
pub const MIN_GROUP_PCT_CPU: f64 = 1.0;

/// One row of the `ps` sample.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcSample {
    pub pid: i32,
    pub ppid: i32,
    /// `ps`'s `pcpu` column, in percent-of-one-core (so `1843.0` on an 18-core
    /// host means ~18 cores' worth).
    pub pct_cpu: f64,
    /// Basename of the `comm` column. macOS prints an absolute executable path
    /// here where Linux prints a short name, so it is always reduced to the
    /// basename for a stable, comparable group key.
    pub command: String,
}

/// Every sample sharing one command name, aggregated — the unit the clause
/// reports, because "25 separate `ngspice` lines" is the finding and 25
/// separate log entries would bury it.
#[derive(Debug, Clone, PartialEq)]
pub struct ConsumerGroup {
    pub command: String,
    /// How many processes carry this command name.
    pub instances: usize,
    /// Summed `%cpu` across the group.
    pub pct_cpu: f64,
    /// The group's most common parent, as `(command, pid)`. `None` when the
    /// parent pid appears in no sample (it exited between `ps` rows, or is
    /// outside this user's visibility).
    pub parent: Option<(String, i32)>,
    /// `true` when any member's parent is pid 1 (`launchd` on macOS, `init` on
    /// Linux). A compute process in that state was orphaned by whatever spawned
    /// it — the signature of the detached/escaped job this module was written
    /// for. Note that a *service* legitimately parented to pid 1 (the daemon
    /// itself, under its own launchd/systemd job) reads the same way, which is
    /// why this is reported as a fact rather than as a verdict.
    pub reparented_to_init: bool,
}

/// Parse `ps -Ao pid=,ppid=,pcpu=,comm=` output into samples.
///
/// Pure and total: any line that does not yield all four fields is skipped
/// rather than failing the parse, because a partial sample still names the top
/// consumer and a rejected one names nothing.
///
/// Splits on **runs** of whitespace, not single characters: `ps` right-aligns
/// its numeric columns, so every row carries padding runs an
/// `splitn(4, char::is_whitespace)` would turn into empty fields. The trailing
/// `comm` column is the only one that can itself contain a space, so it is
/// rejoined from the remainder — single-spaced, which is lossless for the
/// purpose here since it is immediately reduced to its basename.
#[must_use]
pub fn parse_ps_output(stdout: &str) -> Vec<ProcSample> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let mut fields = line.split_whitespace();
        let Some(Ok(pid)) = fields.next().map(str::parse::<i32>) else {
            continue;
        };
        let Some(Ok(ppid)) = fields.next().map(str::parse::<i32>) else {
            continue;
        };
        let Some(Ok(pct_cpu)) = fields.next().map(str::parse::<f64>) else {
            continue;
        };
        let command = basename(&fields.collect::<Vec<_>>().join(" "));
        if command.is_empty() {
            continue;
        }
        out.push(ProcSample {
            pid,
            ppid,
            pct_cpu,
            command,
        });
    }
    out
}

/// Last path component of `comm`, or the whole string when it has none. macOS
/// `ps` prints `/Applications/…/ngspice`; Linux prints `ngspice`. Grouping on
/// the raw column would treat those as different commands across a
/// heterogeneous fleet.
fn basename(command: &str) -> String {
    command
        .rsplit('/')
        .next()
        .unwrap_or(command)
        .trim()
        .to_string()
}

/// Aggregate samples into the top `limit` command groups by summed `%cpu`.
///
/// Pure — takes the whole sample set, so it is unit-testable against synthetic
/// process tables without a `ps` on the test host. Groups under
/// [`MIN_GROUP_PCT_CPU`] are dropped; ties break on command name so the output
/// is deterministic for a given sample (a log line that reorders between ticks
/// is harder to diff than one that does not).
#[must_use]
pub fn top_consumers(samples: &[ProcSample], limit: usize) -> Vec<ConsumerGroup> {
    use std::collections::HashMap;

    // pid -> command, so a group's ppid can be resolved to a parent name.
    let by_pid: HashMap<i32, &str> = samples
        .iter()
        .map(|s| (s.pid, s.command.as_str()))
        .collect();

    let mut grouped: HashMap<&str, (usize, f64, Vec<i32>)> = HashMap::new();
    for sample in samples {
        let entry = grouped
            .entry(sample.command.as_str())
            .or_insert((0, 0.0, Vec::new()));
        entry.0 += 1;
        entry.1 += sample.pct_cpu;
        entry.2.push(sample.ppid);
    }

    let mut groups: Vec<ConsumerGroup> = grouped
        .into_iter()
        .filter(|(_, (_, pct_cpu, _))| *pct_cpu >= MIN_GROUP_PCT_CPU)
        .map(|(command, (instances, pct_cpu, ppids))| {
            let reparented_to_init = ppids.contains(&1);
            ConsumerGroup {
                command: command.to_string(),
                instances,
                pct_cpu,
                parent: most_common_parent(&ppids, &by_pid),
                reparented_to_init,
            }
        })
        .collect();

    groups.sort_by(|a, b| {
        b.pct_cpu
            .partial_cmp(&a.pct_cpu)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.command.cmp(&b.command))
    });
    groups.truncate(limit);
    groups
}

/// The most frequent ppid in `ppids`, resolved to `(command, pid)`. Ties break
/// on the lower pid for determinism. `None` when no ppid resolves to a sampled
/// command — pid 1 is special-cased to its conventional name since `launchd` /
/// `init` does not always appear in an unprivileged `ps`.
fn most_common_parent(
    ppids: &[i32],
    by_pid: &std::collections::HashMap<i32, &str>,
) -> Option<(String, i32)> {
    use std::collections::HashMap;
    let mut counts: HashMap<i32, usize> = HashMap::new();
    for ppid in ppids {
        *counts.entry(*ppid).or_insert(0) += 1;
    }
    let mut ranked: Vec<(i32, usize)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    for (ppid, _) in ranked {
        if let Some(command) = by_pid.get(&ppid) {
            return Some(((*command).to_string(), ppid));
        }
        if ppid == 1 {
            return Some((init_process_name().to_string(), 1));
        }
    }
    None
}

/// Conventional name of pid 1 on this platform. Used only when pid 1 itself is
/// absent from the `ps` sample, which is the common case for an unprivileged
/// probe.
const fn init_process_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "launchd"
    } else {
        "init"
    }
}

/// Render groups as the clause appended to a starvation log line. Empty string
/// for an empty group list, so a caller can concatenate unconditionally — the
/// same contract [`crate::admission_brake`]'s `role_agent_clause` established
/// for the #6102 attribution it sits next to.
#[must_use]
pub fn format_clause(groups: &[ConsumerGroup]) -> String {
    if groups.is_empty() {
        return String::new();
    }
    let rendered: Vec<String> = groups.iter().map(format_group).collect();
    let mut clause = format!(
        " — TOP CPU (best-effort host-wide `ps` sample, includes work Loom does not own): {}",
        rendered.join(", ")
    );
    if groups.iter().any(|g| g.reparented_to_init) {
        clause.push_str(
            ". A compute process reparented to pid 1 is owned by NO live Loom session — see \
             .loom/docs/long-running-compute.md (#8478)",
        );
    }
    clause
}

/// One group, e.g. `ngspice ×25 (1843% cpu, parent launchd[1], reparented to
/// pid 1)`.
fn format_group(group: &ConsumerGroup) -> String {
    let parent = group.parent.as_ref().map_or_else(
        || "parent unknown".to_string(),
        |(command, pid)| format!("parent {command}[{pid}]"),
    );
    let orphan_note = if group.reparented_to_init {
        ", reparented to pid 1"
    } else {
        ""
    };
    format!(
        "{} ×{} ({:.0}% cpu, {parent}{orphan_note})",
        group.command, group.instances, group.pct_cpu
    )
}

/// Run the `ps` probe and return its stdout, or `None` on any failure.
///
/// `-A` (every process) is BSD/POSIX syntax accepted by both macOS's `ps` and
/// procps' on Linux; `-o <col>=` suppresses the header on both. A non-zero exit,
/// a timeout, a missing binary, or non-UTF-8 output all yield `None`.
fn probe_ps() -> Option<String> {
    let mut cmd = Command::new("ps");
    cmd.args(["-Ao", "pid=,ppid=,pcpu=,comm="]);
    match crate::proc_exec::run_bounded(cmd, PROBE_TIMEOUT) {
        Ok(crate::proc_exec::Completion::Exited(output)) if output.status.success() => {
            String::from_utf8(output.stdout).ok()
        }
        _ => None,
    }
}

/// The attribution clause for this host right now, or an empty string when the
/// probe could not answer.
///
/// This is the only function [`crate::admission_brake`] calls. It never returns
/// an error and never panics: an empty string means "say nothing extra", which
/// is precisely the pre-#8478 behaviour, so the starvation log line fires
/// identically whether or not attribution succeeded.
#[must_use]
pub fn attribution_clause() -> String {
    let Some(stdout) = probe_ps() else {
        log::debug!(
            "foreign_load: ps probe unavailable or timed out; starvation log line will not be \
             attributed (#8478)"
        );
        return String::new();
    };
    format_clause(&top_consumers(&parse_ps_output(&stdout), TOP_N))
}

#[cfg(test)]
mod tests;
