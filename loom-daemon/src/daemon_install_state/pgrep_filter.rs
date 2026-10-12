//! Identity filter for `pgrep -x loom-daemon` candidates (#10110).
//!
//! Split out of `daemon_install_state.rs` so the over-threshold parent does
//! not grow (`.loom/docs/file-size-policy.md`). `pgrep` matches by process
//! name only, so session-exec workers and concurrent `health`/`status` probes
//! all look like the dispatcher; this module drops the proven
//! non-dispatchers while keeping every unknown identity.

use super::{probe_output, DAEMON_PROCESS_NAME, PROBE_TIMEOUT};
use std::collections::HashMap;
use std::process::Command;

/// Upper bound on how many candidate pids get their command line inspected.
/// Candidates past the cap are kept uninspected (unknown identity stays
/// conservative). All inspected pids share ONE bounded `ps` call.
const MAX_INSPECTED_CANDIDATES: usize = 64;

/// Whether a candidate's command line identifies it as the autonomous
/// dispatcher, a proven non-dispatcher, or cannot be told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateRole {
    /// The bare `loom-daemon` form (no subcommand) - the dispatcher.
    Dispatcher,
    /// A CLI subcommand invocation (`session-exec worker`, `health`,
    /// `status`, ...). The dispatcher is only ever launched without a
    /// subcommand, so these can never be dispatcher liveness evidence.
    NonDispatcher,
    /// Command line unparseable or not recognisably `loom-daemon`.
    Unknown,
}

/// Classify one `ps -o args=` command line. Pure; see [`CandidateRole`].
fn classify_candidate(args: &str) -> CandidateRole {
    let mut tokens = args.split_whitespace();
    let Some(argv0) = tokens.next() else {
        return CandidateRole::Unknown;
    };
    // A path containing spaces splits wrongly here and fails this check,
    // landing in Unknown (kept) - the conservative direction.
    if argv0.rsplit('/').next() != Some(DAEMON_PROCESS_NAME) {
        return CandidateRole::Unknown;
    }
    match tokens.next() {
        None => CandidateRole::Dispatcher,
        // Only a kebab-case word is a recognisable subcommand; a flag or
        // anything odd is not proven either way.
        Some(t)
            if t.starts_with(|c: char| c.is_ascii_lowercase())
                && t.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') =>
        {
            CandidateRole::NonDispatcher
        }
        Some(_) => CandidateRole::Unknown,
    }
}

/// Drop proven non-dispatcher candidates from `pids`, given the `ps -o pid=,args=`
/// stdout for them (`None` when the probe failed/timed out).
///
/// Conservative rules: no usable `ps` answer (`None`, or no parsable line at
/// all) keeps every pid; a pid with an `Unknown` identity is kept; a pid absent
/// from an otherwise parsable answer has exited and is dropped; pids beyond
/// [`MAX_INSPECTED_CANDIDATES`] are kept uninspected.
fn filter_candidates_by_ps(pids: Vec<u32>, ps_stdout: Option<&str>) -> Vec<u32> {
    let Some(out) = ps_stdout else {
        return pids;
    };
    let mut roles: HashMap<u32, CandidateRole> = HashMap::new();
    for line in out.lines() {
        let line = line.trim_start();
        let Some((pid, rest)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        if let Ok(pid) = pid.parse::<u32>() {
            roles.insert(pid, classify_candidate(rest));
        }
    }
    if roles.is_empty() {
        return pids;
    }
    pids.into_iter()
        .enumerate()
        .filter(|(i, pid)| {
            if *i >= MAX_INSPECTED_CANDIDATES {
                return true;
            }
            match roles.get(pid) {
                Some(CandidateRole::NonDispatcher) | None => false,
                Some(CandidateRole::Dispatcher | CandidateRole::Unknown) => true,
            }
        })
        .map(|(_, pid)| pid)
        .collect()
}

/// Remove proven non-dispatcher processes (session-exec workers, concurrent
/// `health`/`status` probes, other CLI subcommands) from the name-matched
/// candidates, using a single bounded `ps` call.
pub(super) fn filter_dispatcher_candidates(pids: Vec<u32>) -> Vec<u32> {
    if pids.is_empty() {
        return pids;
    }
    let list = pids
        .iter()
        .take(MAX_INSPECTED_CANDIDATES)
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let mut cmd = Command::new("ps");
    cmd.args(["-p", &list, "-o", "pid=,args="]);
    // ps exits 1 when some/all pids are gone; judge by stdout, not status.
    let stdout = probe_output(cmd, PROBE_TIMEOUT).and_then(|o| String::from_utf8(o.stdout).ok());
    filter_candidates_by_ps(pids, stdout.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DISPATCHER: &str = "/usr/local/bin/loom-daemon";

    #[test]
    fn classify_candidate_roles() {
        assert_eq!(classify_candidate(DISPATCHER), CandidateRole::Dispatcher);
        assert_eq!(classify_candidate("loom-daemon"), CandidateRole::Dispatcher);
        for cmd in [
            "session-exec worker --x 1",
            "health --json",
            "status",
            "serve --port 1",
        ] {
            assert_eq!(
                classify_candidate(&format!("{DISPATCHER} {cmd}")),
                CandidateRole::NonDispatcher,
                "{cmd}"
            );
        }
        assert_eq!(classify_candidate(""), CandidateRole::Unknown);
        assert_eq!(classify_candidate("/bin/other thing"), CandidateRole::Unknown);
        assert_eq!(classify_candidate(&format!("{DISPATCHER} --weird")), CandidateRole::Unknown);
        assert_eq!(classify_candidate(&format!("{DISPATCHER} \u{1}\u{2}")), CandidateRole::Unknown);
    }

    #[test]
    fn worker_and_probe_only_sets_yield_no_candidates() {
        let ps = format!(
            "  10 {DISPATCHER} session-exec worker a\n  11 {DISPATCHER} health --json\n  12 {DISPATCHER} status\n"
        );
        assert!(filter_candidates_by_ps(vec![10, 11, 12], Some(&ps)).is_empty());
    }

    #[test]
    fn mixed_set_keeps_only_the_dispatcher() {
        let ps = format!(
            "10 {DISPATCHER} session-exec worker\n20 {DISPATCHER}\n11 {DISPATCHER} health\n"
        );
        assert_eq!(filter_candidates_by_ps(vec![10, 20, 11], Some(&ps)), vec![20]);
    }

    #[test]
    fn dispatcher_only_set_is_preserved() {
        let ps = format!("20 {DISPATCHER}\n");
        assert_eq!(filter_candidates_by_ps(vec![20], Some(&ps)), vec![20]);
    }

    #[test]
    fn unknown_identity_is_kept() {
        // Failed/timed-out ps, empty output, malformed output: keep everything.
        assert_eq!(filter_candidates_by_ps(vec![1, 2], None), vec![1, 2]);
        assert_eq!(filter_candidates_by_ps(vec![1, 2], Some("")), vec![1, 2]);
        assert_eq!(filter_candidates_by_ps(vec![1, 2], Some("garbage\n???")), vec![1, 2]);
        // Unrecognised argv0 on a parsable line is kept too.
        assert_eq!(filter_candidates_by_ps(vec![1], Some("1 /bin/x y\n")), vec![1]);
    }

    #[test]
    fn exited_pid_is_dropped_and_overflow_is_kept_uninspected() {
        let ps = format!("20 {DISPATCHER}\n");
        assert_eq!(filter_candidates_by_ps(vec![99, 20], Some(&ps)), vec![20]);
        let mut pids: Vec<u32> = (1000..1000 + MAX_INSPECTED_CANDIDATES as u32 + 2).collect();
        let tail: Vec<u32> = pids[MAX_INSPECTED_CANDIDATES..].to_vec();
        pids.push(1);
        let got = filter_candidates_by_ps(pids, Some(&ps));
        assert_eq!(&got[..], &tail[..].iter().copied().chain([1]).collect::<Vec<_>>()[..]);
    }
}
