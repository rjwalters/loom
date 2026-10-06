//! Read one component's `run:` steps out of `ci.yml` (#10388).
//!
//! Deliberately narrow, and fail closed: it accepts only steps it can run
//! exactly as Actions would — `name`, `if: ${{ !cancelled() }}` and a `run`
//! (plain scalar or `|` block) with no `${{ }}` expression. Anything else
//! (`uses`, `with`, `env`, `shell`, `working-directory`, another `if`, a quoted
//! or folded scalar) is an error, which the caller turns into "no verdict".
//!
//! A step that would run a [`DENIED_COMMANDS`] entry (or anything under
//! `target/`) is an error too. The pin test only covers the `ci.yml` the
//! daemon was built with, and a deployed daemon reads whatever `ci.yml` the
//! merge tree carries, so this is checked on every read: a renamed install
//! step must never turn into a `curl … | sudo install` on the merging host,
//! and a step that runs the host's own toolchain or daemon would stand in for
//! the merge tree's build (a false pass).

/// The only step condition accepted: it means "run unless the job was
/// cancelled", which is always true for a local run.
const NOT_CANCELLED: &str = "${{ !cancelled() }}";

/// Commands a locally run step must never invoke: network fetches,
/// privilege escalation, package managers, and toolchains/binaries whose HOST
/// copy would stand in for the merge tree's own build. Matched against whole
/// shell words (and a path's last component), so `pipefail` is not `pip`.
pub const DENIED_COMMANDS: &[&str] = &[
    "curl",
    "wget",
    "sudo",
    "doas",
    "gh",
    "cargo",
    "rustc",
    "rustup",
    "loom-daemon",
    "npm",
    "npx",
    "pnpm",
    "yarn",
    "node",
    "pip",
    "pip3",
    "apt",
    "apt-get",
    "brew",
];

/// A path fragment no locally run step may reference: build output is the
/// host's, not the merge tree's.
pub const DENIED_PATH: &str = "target/";

#[derive(Default)]
struct Step {
    name: Option<String>,
    run: Option<String>,
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// The `run:` bodies of `component`'s steps in `ci_yml`, in order, minus the
/// steps named in `skip`.
pub fn ci_steps(ci_yml: &str, component: &str, skip: &[&str]) -> Result<Vec<String>, String> {
    let marker = format!("# component: {component}");
    let mut out = Vec::new();
    for s in parse_steps(ci_yml, component)? {
        let name = s.name.unwrap_or_default();
        if skip.contains(&name.as_str()) {
            continue;
        }
        let run = s
            .run
            .ok_or_else(|| format!("step `{name}` has no `run:`"))?;
        if run.contains("${{") {
            return Err(format!("step `{name}` uses a `${{{{ }}}}` expression"));
        }
        if let Some(cmd) = denied_command(&run) {
            return Err(format!(
                "step `{name}` runs `{cmd}`, which a local evaluation must never execute"
            ));
        }
        out.push(run);
    }
    if out.is_empty() {
        return Err(format!("no runnable steps under `{marker}`"));
    }
    Ok(out)
}

/// The `run:` body of the step named `step` under `component` — e.g. a
/// skipped install step, to read the tool version CI pins.
pub fn step_run(ci_yml: &str, component: &str, step: &str) -> Result<String, String> {
    parse_steps(ci_yml, component)?
        .into_iter()
        .find(|s| s.name.as_deref() == Some(step))
        .and_then(|s| s.run)
        .ok_or_else(|| format!("ci.yml has no `{step}` step with a `run:` under `{component}`"))
}

/// The value of a `VAR=value` assignment line in a step body (quotes
/// stripped), e.g. `VER=v0.24.2` ⇒ `v0.24.2`.
#[must_use]
pub fn assigned_value(run: &str, var: &str) -> Option<String> {
    run.lines().find_map(|l| {
        let v = l.trim().strip_prefix(var)?.strip_prefix('=')?;
        let v = v.trim().trim_matches(['"', '\'']);
        (!v.is_empty()).then(|| v.to_string())
    })
}

/// The first [`DENIED_COMMANDS`] word (or [`DENIED_PATH`] reference) in a
/// step body, ignoring full-line `#` comments.
#[must_use]
pub fn denied_command(run: &str) -> Option<String> {
    let is_word = |c: char| c.is_ascii_alphanumeric() || "._/+-".contains(c);
    for line in run.lines().filter(|l| !l.trim_start().starts_with('#')) {
        for word in line.split(|c: char| !is_word(c)).filter(|w| !w.is_empty()) {
            if word.contains(DENIED_PATH) {
                return Some(word.to_string());
            }
            let cmd = word.rsplit('/').next().unwrap_or(word);
            if DENIED_COMMANDS.contains(&cmd) {
                return Some(cmd.to_string());
            }
        }
    }
    None
}

fn parse_steps(ci_yml: &str, component: &str) -> Result<Vec<Step>, String> {
    let lines: Vec<&str> = ci_yml.lines().collect();
    let marker = format!("# component: {component}");
    let start = lines
        .iter()
        .position(|l| l.trim() == marker)
        .ok_or_else(|| format!("ci.yml has no `{marker}` marker"))?;

    let mut steps: Vec<Step> = Vec::new();
    let mut step_indent: Option<usize> = None;
    let mut i = start + 1;
    while i < lines.len() {
        let line = lines[i];
        let t = line.trim_start();
        let ind = indent_of(line);
        if t.is_empty() {
            i += 1;
            continue;
        }
        if t.starts_with('#') {
            if t.starts_with("# component:") {
                break;
            }
            i += 1;
            continue;
        }
        let (key_line, key_indent) = if let Some(rest) = t.strip_prefix("- ") {
            match step_indent {
                None => step_indent = Some(ind),
                Some(si) if si != ind => break,
                Some(_) => {}
            }
            steps.push(Step::default());
            (rest, ind + 2)
        } else {
            match step_indent {
                Some(si) if ind == si + 2 => (t, ind),
                _ => break, // the job (or file) moved on
            }
        };
        let step = steps
            .last_mut()
            .ok_or_else(|| format!("unexpected `{t}` before the first step"))?;
        let (key, value) = key_line
            .split_once(':')
            .map(|(k, v)| (k.trim(), v.trim()))
            .ok_or_else(|| format!("cannot parse step line `{t}`"))?;
        match key {
            "name" => step.name = Some(value.to_string()),
            "if" if value == NOT_CANCELLED => {}
            "if" => return Err(format!("step condition `{value}` cannot be evaluated locally")),
            "run" if value == "|" => {
                let mut body: Vec<&str> = Vec::new();
                let mut base: Option<usize> = None;
                i += 1;
                while i < lines.len() {
                    let b = lines[i];
                    if !b.trim().is_empty() && indent_of(b) <= key_indent {
                        break;
                    }
                    if !b.trim().is_empty() {
                        base.get_or_insert(indent_of(b));
                    }
                    body.push(b);
                    i += 1;
                }
                let base = base.ok_or("an empty `run: |` block")?;
                let mut text: Vec<&str> = body
                    .iter()
                    .map(|b| if b.len() >= base { &b[base..] } else { "" })
                    .collect();
                while text.last().is_some_and(|s| s.trim().is_empty()) {
                    text.pop();
                }
                step.run = Some(text.join("\n") + "\n");
                continue;
            }
            "run" if value.is_empty() || value.starts_with(['"', '\'', '>', '|']) => {
                return Err(format!("`run: {value}` is not a form this reader accepts"));
            }
            "run" => step.run = Some(format!("{value}\n")),
            other => return Err(format!("step key `{other}` cannot be run locally")),
        }
        i += 1;
    }

    Ok(steps)
}

#[cfg(test)]
#[path = "ci_steps_tests.rs"]
mod tests;
