//! Read one component's `run:` steps out of `ci.yml` (#10388).
//!
//! Deliberately narrow, and fail closed: it accepts only steps it can run
//! exactly as Actions would — `name`, `if: ${{ !cancelled() }}` and a `run`
//! (plain scalar or `|` block) with no `${{ }}` expression. Anything else
//! (`uses`, `with`, `env`, `shell`, `working-directory`, another `if`, a quoted
//! or folded scalar) is an error, which the caller turns into "no verdict".

/// The only step condition accepted: it means "run unless the job was
/// cancelled", which is always true for a local run.
const NOT_CANCELLED: &str = "${{ !cancelled() }}";

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

    let mut out = Vec::new();
    for s in steps {
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
        out.push(run);
    }
    if out.is_empty() {
        return Err(format!("no runnable steps under `{marker}`"));
    }
    Ok(out)
}

#[cfg(test)]
#[path = "ci_steps_tests.rs"]
mod tests;
