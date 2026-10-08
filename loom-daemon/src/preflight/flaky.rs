//! Tests that passed only on a retry (Issue #10955).
//!
//! `build-gate.sh` runs nextest with `--retries 1 --final-status-level flaky`
//! (overridable with `LOOM_BUILD_GATE_TEST_RETRIES`), so one flaky test no
//! longer fails the gate and burns a pre-flight attempt. CI keeps
//! `retries = 0`. A gate that passed only thanks to a retry must say so, so
//! the names are read out of the gate log before it is deleted.

/// The tests nextest reported `FLAKY` (failed, then passed on a retry), in
/// order of first appearance, without duplicates. nextest prints each as
/// `FLAKY <try>/<tries> [ <secs>s] <binary-id> <test-name>`; the name kept is
/// everything after the duration, so it carries the binary id too.
pub(super) fn flaky_tests(log: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for line in log.lines() {
        let Some(rest) = line.trim_start().strip_prefix("FLAKY ") else {
            continue;
        };
        let Some((_, name)) = rest.split_once("] ") else {
            continue;
        };
        let name = name.trim();
        if !name.is_empty() && !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_flaky_names_once_each_and_ignores_everything_else() {
        let log = "\
        PASS [   0.010s] loom-daemon a::passes
   TRY 2 PASS [   0.020s] loom-daemon b::flaky
       FLAKY 2/2 [   0.020s] loom-daemon b::flaky
        FAIL [   0.010s] loom-daemon c::fails
     Summary [   1.000s] 3 tests run: 2 passed (1 flaky), 1 failed
       FLAKY 2/2 [   0.020s] loom-daemon b::flaky
FLAKY 2/2 [   1.500s] loom-api d::also_flaky
FLAKY without a duration
";
        assert_eq!(flaky_tests(log), vec!["loom-daemon b::flaky", "loom-api d::also_flaky"]);
        assert!(flaky_tests("all good\n").is_empty());
    }
}
