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

#[test]
fn denied_commands_fail_closed_at_read_time() {
    // The lychee install step, renamed so `skip_steps` no longer matches it:
    // it must never run on the merging host.
    let y = "      # component: X\n      - name: Install lychee (renamed)\n        run: |\n          curl -fsSL https://example.invalid/l.tgz -o /tmp/l.tgz\n          sudo install -m 0755 /tmp/l /usr/local/bin/l\n";
    let e = ci_steps(y, "X", &["Install lychee (pinned, checksum-verified)"]).unwrap_err();
    assert!(e.contains("curl"), "{e}");
    for (body, want) in [
        ("wget https://example.invalid/x", "wget"),
        ("cargo test -p loom-daemon", "cargo"),
        ("./target/debug/loom-daemon secret-scan", "./target/debug/loom-daemon"),
        ("loom-daemon shell-budget --check", "loom-daemon"),
        ("/usr/bin/sudo true", "sudo"),
        ("x=$(npm run lint)", "npm"),
        ("pnpm check:ci", "pnpm"),
        ("node scripts/x.js", "node"),
        ("pip install foo", "pip"),
        ("gh api repos/x/y", "gh"),
    ] {
        assert_eq!(denied_command(body).as_deref(), Some(want), "{body}");
        let y = format!("      # component: X\n      - name: a\n        run: {body}\n");
        assert!(ci_steps(&y, "X", &[]).is_err(), "{body}");
    }
}

#[test]
fn the_denylist_matches_whole_words_only() {
    for body in [
        "set -euo pipefail",
        "bash scripts/check-node-ids.sh",
        "echo curly",
        "# a comment naming cargo and curl is not a command",
        "grep -c target_dir x",
    ] {
        assert_eq!(denied_command(body), None, "{body}");
    }
}

#[test]
fn step_run_and_assigned_value_read_a_skipped_install_step() {
    let y = "      # component: X\n      - name: Install t\n        run: |\n          set -e\n          VER=\"v1.2.3\"\n          curl -o t https://example.invalid/t-${VER}\n      - name: Use t\n        run: t --check\n";
    let body = step_run(y, "X", "Install t").unwrap();
    assert_eq!(assigned_value(&body, "VER").as_deref(), Some("v1.2.3"));
    assert_eq!(assigned_value(&body, "SHA"), None);
    assert!(step_run(y, "X", "No such step").is_err());
}

#[test]
fn the_real_ci_yml_pins_a_lychee_version_the_local_run_can_read() {
    for k in CHEAP_CHECKS {
        for pin in k.pins {
            let body = step_run(CI_YML, k.component, pin.step)
                .unwrap_or_else(|e| panic!("{}: {e}", k.component));
            let ver = assigned_value(&body, pin.var)
                .unwrap_or_else(|| panic!("{}: no {}= in `{}`", k.component, pin.var, pin.step));
            assert!(
                ver.trim_start_matches('v').split('.').count() >= 2,
                "{}: `{ver}` is not a version",
                k.component
            );
            assert!(
                k.skip_steps.contains(&pin.step),
                "{}: a pinned install step must be skipped, never run",
                k.component
            );
        }
    }
    assert!(
        CHEAP_CHECKS
            .iter()
            .any(|k| k.pins.iter().any(|p| p.tool == "lychee")),
        "Dangling Link Check's lychee must be version-pinned"
    );
}
