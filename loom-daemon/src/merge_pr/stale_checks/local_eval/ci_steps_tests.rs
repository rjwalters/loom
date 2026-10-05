use super::*;
use crate::merge_pr::stale_checks::local_eval::CHEAP_CHECKS;

const CI_YML: &str = include_str!("../../../../../.github/workflows/ci.yml");

const SAMPLE: &str = "jobs:
  structural-checks:
    name: Structural Checks
    steps:
      - uses: actions/checkout@v4

      # component: Alpha
      # prose
      - name: Self-test
        if: ${{ !cancelled() }}
        run: bash scripts/a.sh --self-test
      - name: Check
        if: ${{ !cancelled() }}
        # a comment between keys
        run: |
          set -e
          # a shell comment stays
          if [ -f x ]; then
            echo y
          fi

      # component: Beta
      - name: Install tool
        run: |
          curl -o /tmp/t https://example.invalid/t
      - name: Uses env
        env:
          A: b
        run: echo $A
  other-job:
    name: Other
";

#[test]
fn reads_plain_and_block_steps_verbatim() {
    let got = ci_steps(SAMPLE, "Alpha", &[]).unwrap();
    assert_eq!(
        got,
        vec![
            "bash scripts/a.sh --self-test\n".to_string(),
            "set -e\n# a shell comment stays\nif [ -f x ]; then\n  echo y\nfi\n".to_string(),
        ]
    );
}

#[test]
fn unsupported_step_keys_fail_closed() {
    let e = ci_steps(SAMPLE, "Beta", &["Install tool"]).unwrap_err();
    assert!(e.contains("env"), "{e}");
}

#[test]
fn missing_marker_fails_closed() {
    assert!(ci_steps(SAMPLE, "Gamma", &[]).is_err());
}

#[test]
fn expressions_and_conditions_fail_closed() {
    let y = "      # component: X\n      - name: a\n        run: echo ${{ github.sha }}\n";
    assert!(ci_steps(y, "X", &[]).unwrap_err().contains("expression"));
    let y = "      # component: X\n      - name: a\n        if: github.event_name == 'pull_request'\n        run: echo\n";
    assert!(ci_steps(y, "X", &[]).unwrap_err().contains("condition"));
    let y = "      # component: X\n      - uses: foo/bar@v1\n";
    assert!(ci_steps(y, "X", &[]).is_err());
}

#[test]
fn every_allowlisted_component_is_runnable_from_the_real_ci_yml() {
    // The pin: if ci.yml grows a step the reader cannot run faithfully (or a
    // skip name drifts), this fails here rather than every merge quietly
    // falling back to the re-date loop.
    for k in CHEAP_CHECKS {
        let steps = ci_steps(CI_YML, k.component, k.skip_steps)
            .unwrap_or_else(|e| panic!("{}: {e}", k.component));
        assert!(
            steps.iter().any(|s| s.contains(k.script)),
            "{}: no step runs {}",
            k.component,
            k.script
        );
        let all = ci_steps(CI_YML, k.component, &[]);
        for skip in k.skip_steps {
            assert!(
                CI_YML.contains(&format!("- name: {skip}")),
                "{}: skip step `{skip}` is not in ci.yml",
                k.component
            );
        }
        if k.skip_steps.is_empty() {
            assert_eq!(all.unwrap(), steps);
        }
    }
}
