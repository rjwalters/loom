//! Expand Loom's role invocation without relying on a Claude slash-command parser.
use super::LaunchError;
use std::path::{Path, PathBuf};

pub fn role_invocation(prompt: &str) -> Option<(&str, &str)> {
    let rest = prompt.strip_prefix("/loom:")?;
    let (role, arguments) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    Some((role, arguments.trim()))
}

/// Conservative ceiling on the fully expanded prompt (issue #8506, fix step
/// 2). This is **not** a reflection of Linux's `MAX_ARG_STRLEN` (128 KiB) —
/// that limit only ever applied because the prompt used to ride on `argv`.
/// Now that both native harnesses read it off `stdin` (`harness::command`),
/// there is no OS-level argument-length ceiling left to respect, and a
/// synthetic 300 KiB `CLAUDE.md` is expected to launch successfully (see the
/// issue's acceptance criteria). This is a belt-and-suspenders sanity bound
/// against a genuinely pathological expansion (a corrupted or runaway
/// `CLAUDE.md`/`AGENTS.md`) that could otherwise buffer an unbounded amount
/// of memory before `exec`, reported as a clear, classified failure instead
/// of a downstream OOM or a cryptic harness error.
pub const MAX_PROMPT_BYTES: usize = 8 * 1024 * 1024;

/// One contribution to the expanded prompt, tracked so a size failure can
/// name the specific source that pushed the total over the limit (issue
/// #8506 fix step 2) instead of just reporting an opaque total.
struct Contribution {
    label: &'static str,
    path: Option<PathBuf>,
    bytes: usize,
}

pub fn expand(root: &Path, prompt: &str) -> Result<String, LaunchError> {
    let Some((role, arguments)) = role_invocation(prompt) else {
        return enforce_size_limit(
            prompt.to_string(),
            &[Contribution {
                label: "prompt",
                path: None,
                bytes: prompt.len(),
            }],
        );
    };
    let canonical = crate::runtime_admission::canonical_role(role)
        .ok_or_else(|| LaunchError::config("unknown Loom role in prompt"))?;
    let (path, role_text) = if canonical == "sweep-lifecycle" {
        (
            std::path::PathBuf::from("bundled native-sweep.md"),
            include_str!("../../../defaults/docs/native-sweep.md").to_string(),
        )
    } else {
        let installed = root.join(".loom/roles").join(format!("{canonical}.md"));
        let path = if installed.is_file() {
            installed
        } else {
            root.join("defaults/roles").join(format!("{canonical}.md"))
        };
        let text = std::fs::read_to_string(&path).map_err(|e| {
            LaunchError::config(format!("cannot read role instructions {}: {e}", path.display()))
        })?;
        (path, text)
    };
    let role_text = role_text.replace("$ARGUMENTS", arguments);
    let mut contributions = vec![Contribution {
        label: "role instructions",
        path: Some(path.clone()),
        bytes: role_text.len(),
    }];
    let mut expanded =
        format!("Loom role instructions (source: {}):\n{}", path.display(), role_text);
    // Both harnesses discover repository rules, but explicit inclusion also covers
    // launches from worktrees whose primary checkout owns the installed role files.
    for file in ["AGENTS.md", "CLAUDE.md"] {
        let path = root.join(file);
        if path.is_file() {
            let text = std::fs::read_to_string(&path)
                .map_err(|e| LaunchError::config(format!("cannot read {}: {e}", path.display())))?;
            contributions.push(Contribution {
                label: file,
                path: Some(path.clone()),
                bytes: text.len(),
            });
            expanded
                .push_str(&format!("\n\nRepository instructions ({}):\n{text}", path.display()));
        }
    }
    expanded.push_str("\n\nNative runtime: use loom_read/loom_edit/loom_write/loom_bash and Loom's CLI helpers. Do not invoke model workers or unavailable MCP/Task tools. Execute this role in the current session. Guard denials are failures, not approval prompts.\n");
    enforce_size_limit(expanded, &contributions)
}

/// Fail closed with a classified, actionable error — naming the total size,
/// the limit, and the single largest contributing source — rather than let
/// an oversized prompt reach `exec` and surface as a bare OS error (issue
/// #8506 fix step 2). The returned error's exit code is `126`, matching what
/// `worker_spawn::exec` itself reports for a genuine launch failure, so a
/// caller cannot tell this pre-exec check apart from the OS-level failure it
/// is standing in for.
fn enforce_size_limit(
    expanded: String,
    contributions: &[Contribution],
) -> Result<String, LaunchError> {
    let total = expanded.len();
    if total <= MAX_PROMPT_BYTES {
        return Ok(expanded);
    }
    let offender = contributions
        .iter()
        .max_by_key(|c| c.bytes)
        .map(|c| match &c.path {
            Some(path) => format!("{} ({}, {} bytes)", c.label, path.display(), c.bytes),
            None => format!("{} ({} bytes)", c.label, c.bytes),
        })
        .unwrap_or_else(|| "unknown source".to_string());
    Err(LaunchError::launch_failure(format!(
        "expanded prompt is {total} bytes, exceeding the {MAX_PROMPT_BYTES}-byte limit; largest contributor: {offender}"
    )))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn role_invocation_parses_role_and_arguments() {
        assert_eq!(
            role_invocation("/loom:builder do the thing"),
            Some(("builder", "do the thing"))
        );
        assert_eq!(role_invocation("/loom:builder"), Some(("builder", "")));
        assert_eq!(role_invocation("plain text prompt"), None);
    }

    #[test]
    fn a_free_form_prompt_under_the_limit_passes_through_unchanged() {
        assert_eq!(
            enforce_size_limit(
                "hello".to_string(),
                &[Contribution {
                    label: "prompt",
                    path: None,
                    bytes: 5
                }]
            )
            .unwrap(),
            "hello"
        );
    }

    #[test]
    fn a_prompt_over_the_limit_is_a_classified_launch_failure_naming_the_offender() {
        let big = "x".repeat(MAX_PROMPT_BYTES + 1);
        let err = enforce_size_limit(
            big.clone(),
            &[
                Contribution {
                    label: "role instructions",
                    path: Some(PathBuf::from("/r/role.md")),
                    bytes: 10,
                },
                Contribution {
                    label: "CLAUDE.md",
                    path: Some(PathBuf::from("/r/CLAUDE.md")),
                    bytes: big.len(),
                },
            ],
        )
        .unwrap_err();
        assert_eq!(err.code, 126);
        assert!(err.message.contains(&(big.len()).to_string()), "{}", err.message);
        assert!(err.message.contains(&MAX_PROMPT_BYTES.to_string()), "{}", err.message);
        assert!(err.message.contains("CLAUDE.md"), "{}", err.message);
        assert!(err.message.contains("/r/CLAUDE.md"), "{}", err.message);
    }

    #[test]
    fn a_prompt_exactly_at_the_limit_is_not_a_failure() {
        let at_limit = "x".repeat(MAX_PROMPT_BYTES);
        assert!(enforce_size_limit(
            at_limit,
            &[Contribution {
                label: "prompt",
                path: None,
                bytes: MAX_PROMPT_BYTES
            }]
        )
        .is_ok());
    }
}
