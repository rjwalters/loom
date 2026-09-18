//! Prompt-cache prefix stability for scheduled role ticks (#8066) — the
//! property that a role tick's resolved prompt carries no per-invocation
//! volatile content, so the injected prefix stays shareable across sessions.
//! Split out of `role_runner/tests.rs` so the over-threshold parent shrinks
//! rather than grows (`.loom/docs/file-size-policy.md`).

use super::*;

/// #8066: the role-tick prompt must carry **no per-invocation volatile
/// content** — no issue/PR number, no timestamp, no run id.
///
/// This is the property that makes a role session's injected prefix (role
/// prompt + skills + repo `CLAUDE.md`, ~20–80k tokens) shareable across
/// sessions at all: Anthropic's prompt cache matches a request prefix exactly,
/// at content-block boundaries, so one volatile byte anywhere in the expanded
/// command body re-writes the whole block. Production transcripts confirm the
/// prefix does hit fully today (`cache_creation_input_tokens = 0` with
/// `cache_read_input_tokens = 109,056` on a judge tick) whenever the same
/// account re-runs the role inside the cache TTL.
///
/// Interpolating context into a tick's prompt — "review PR #123", a dispatch
/// timestamp, a run id — would silently end that, so it is pinned here rather
/// than left to the next reader of `resolve_role_prompt`. Full trace,
/// experiments and numbers: `defaults/docs/prompt-prefix-cache-ordering.md`.
/// The sweep half of the same property is pinned by
/// `sweep_registry::dispatch::prompt_shape_tests`.
#[test]
#[serial]
fn test_role_tick_prompt_carries_no_volatile_content() {
    std::env::remove_var(ARCHITECT_MAX_PROPOSALS_ENV);
    let config = RoleRunnerConfig::default();

    for spec in DEFAULT_ROLES.iter() {
        let prompt = resolve_role_prompt(spec, &config);

        // Byte-stable: resolving twice under the same config is identical.
        assert_eq!(
            prompt,
            resolve_role_prompt(spec, &config),
            "{}: prompt must be byte-stable across resolutions",
            spec.name
        );

        // Exactly one of the two permitted shapes — `/loom:<role>`, or
        // architect's `--max-proposals <n>` actuator cap (#5656). Anything
        // else (an issue number, a timestamp, a run id) fails here.
        let expected = if spec.name == ARCHITECT_ROLE {
            format!(
                "/loom:{} --max-proposals {}",
                spec.name,
                resolve_architect_max_proposals(&config)
            )
        } else {
            format!("/loom:{}", spec.name)
        };
        assert_eq!(
            prompt, expected,
            "{}: role-tick prompts must stay a bare slash-command reference \
             (no volatile per-invocation content) — see #8066",
            spec.name
        );

        // Belt-and-braces on the shape above: the only digits allowed in any
        // role prompt are architect's cap, so a stray issue number or epoch
        // timestamp is caught even if the expected-string form is relaxed
        // later.
        if spec.name != ARCHITECT_ROLE {
            assert!(
                !prompt.chars().any(char::is_numeric),
                "{}: prompt contains digits ({prompt:?}) — a volatile \
                 identifier would break cross-session prefix caching (#8066)",
                spec.name
            );
        }
    }
}
