//! `PreToolUse` argument guard for the `mcp__loom__*` tool namespace (Issue
//! #9108).
//!
//! # The hole this closes
//!
//! Loom's `PreToolUse` wiring matched exactly three things: `Bash` (twice) and
//! `Edit|Write`. **No matcher covered MCP tool calls**, even though `mcp-loom`
//! is registered at user scope and is therefore callable from every agent Loom
//! spawns — and at least one of its tools (`get_agent_metrics`) joined its raw
//! MCP arguments into a shell command line until #9107 replaced that with an
//! `execFile` argv. So the one tool class an agent could call freely was the
//! one class that bypassed every guard hook in the system, and
//! `defaults/docs/guard-hooks.md`'s catalog had no MCP category to extend.
//!
//! This module is the decision half of that category. The matcher
//! (`mcp__loom__.*`) is a **namespace wildcard**, not an enumerated tool list,
//! so a tool added to `mcp-loom` tomorrow is covered with no edit here — which
//! is the property an enumerated matcher would not have.
//!
//! # Defence in depth, not a substitute for server-side validation
//!
//! `get_agent_metrics`'s own shell join was #9107's job and stays #9107's job —
//! it landed there (`execFile` argv plus server-side allow-lists), and this
//! module does not replace it. A hook decides *whether a call may happen*; only
//! the tool decides what its arguments may *become*. Both layers are wanted:
//! this one denies the payload before the server sees it, that one stops the
//! payload mattering if this guard is ever absent (a machine with no per-repo
//! hook copies, a repo that turned the category off) — and this one covers
//! every *other* mcp-loom tool, including ones added after #9107, which carry
//! no server-side allow-list of their own.
//!
//! # Two rules
//!
//! 1. **Shell metacharacters → DENY** ([`SHELL_METACHARACTERS`], plus the
//!    two-character `$(`). Applied namespace-wide to every string leaf of
//!    `tool_input` at any depth. This is deliberately deny-by-default: a field
//!    a future tool adds is scanned without anyone remembering to add it.
//!    The exemptions are an explicit, reviewed `(tool, field-path)` list
//!    ([`FREE_TEXT_FIELDS`]) — fields whose documented purpose is to carry
//!    arbitrary text to a non-shell sink, where metacharacters are the point
//!    rather than a smell.
//! 2. **A documented-enum field off its allow-list → DENY**
//!    ([`ENUM_ALLOWLISTS`]). Scoped to `(tool, field)` pairs taken from
//!    `mcp-loom`'s own `inputSchema` `enum:` declarations, NOT to a bare field
//!    name: `configure_terminal.role` legitimately takes non-role values such
//!    as `claude-code-worker`, so a namespace-wide `role` enum would false-deny
//!    a documented call. Rule 1 is the namespace-wide layer; rule 2 is the
//!    precise one.
//!
//! # Values are never echoed
//!
//! Neither the deny reason nor the decision-log record contains an argument
//! *value* — only the tool name, the offending field path, and which
//! metacharacter class fired. An MCP argument can carry a token
//! (`dispatch_sweep.idempotency_key`) or free prose, and a deny reason goes
//! straight back into the agent's context while the decision log aggregates
//! across sessions. The shell guards redact; this one simply never has the
//! value in hand.
//!
//! # Failure mode: allow
//!
//! Same contract as every other Loom guard. An unreadable payload, a missing
//! `tool_name`, a config read that fails — all resolve to allow. A guard that
//! wedges every MCP call in a headless sweep on its own parse bug is worse than
//! the exposure it removes, and the absent-hook case is covered a level up by
//! `hook-wiring.sh`'s fail-closed rung 5.

use std::path::Path;

/// The `guards.*` key and env override, following the established guard-toggle
/// convention (`guards.worktreeIsolation` / `LOOM_GUARD_WORKTREE_ISOLATION`).
pub const TOGGLE_CONFIG_KEY: &str = "guards.mcpToolArgs";
/// Env override for [`TOGGLE_CONFIG_KEY`]. `0`/`false`/`no`/`off` disables;
/// `1`/`true`/`yes`/`on` forces on even when the repo config says otherwise.
pub const TOGGLE_ENV_VAR: &str = "LOOM_GUARD_MCP_TOOL_ARGS";

/// Tool-name prefix this guard owns — the same namespace the `mcp__loom__.*`
/// `PreToolUse` matcher selects. A payload naming anything else is allowed
/// untouched: the matcher should already have filtered it, and a guard that
/// second-guesses its own wiring would deny tool calls it was never wired for.
pub const TOOL_PREFIX: &str = "mcp__loom__";

/// Single-character shell metacharacters that end a command word: chaining,
/// piping, redirection, command substitution, and a literal newline.
///
/// Identical in intent to `guard-destructive-generic.sh`'s read-only fast-path
/// structural test, which rejects fast-path eligibility on exactly this set —
/// the set that turns "an argument" into "another command". The two-character
/// `$(` is checked separately ([`SUBSTITUTION_OPEN`]) because a bare `$` is
/// ordinary in a path or a prompt.
pub const SHELL_METACHARACTERS: [char; 7] = [';', '|', '&', '<', '>', '`', '\n'];

/// The command-substitution opener, checked as a two-character sequence so a
/// lone `$` (a `$HOME`-style path, a price in prose) does not fire.
pub const SUBSTITUTION_OPEN: &str = "$(";

/// `(tool, dotted field path)` pairs exempt from the metacharacter scan
/// (rule 1), because carrying arbitrary text to a NON-shell sink is the
/// documented purpose of the field.
///
/// Deliberately keyed on the pair, never on the bare field name: `input` is
/// keystrokes for a tmux pane on `send_terminal_input` and would be an
/// unreviewed hole on any future tool that happens to name a field `input`.
/// Adding a pair here is a deliberate, reviewable act; forgetting to add one
/// costs a false deny, which is the safe direction.
///
/// `send_terminal_input.input` is the load-bearing entry and the residual
/// exposure this category knowingly accepts: it writes literal keystrokes into
/// a tmux pane that IS a shell, so a metacharacter deny there would break the
/// operator surface it exists for. Shell-safety for that sink is the operator's
/// and the server's, not this guard's — see `guard-hooks.md` for the note.
pub const FREE_TEXT_FIELDS: [(&str, &str); 5] = [
    // Literal keystrokes for a tmux pane; `;`/`|`/newline are the point.
    ("send_terminal_input", "input"),
    // An agent prompt sent on an interval timer — free prose.
    ("create_terminal", "interval_prompt"),
    ("configure_terminal", "role_config.interval_prompt"),
    // Operator free-text note recorded against a watch.
    ("register_watch", "note"),
    // Opaque per-topic JSON payload; the bus does not interpret it.
    ("publish_event", "payload"),
];

/// `(tool, field, allowed values)` — the documented `enum:` sets from
/// `mcp-loom`'s own `inputSchema`s (rule 2).
///
/// Scoped per tool on purpose. `get_agent_metrics.role` is an enum of Loom role
/// names; `configure_terminal.role` is not (`claude-code-worker` is its own
/// documented example), so a namespace-wide `role` allow-list would deny a
/// documented call. Only the enum-typed fields of the one tool that spawns a
/// subprocess today are listed; extending this is a per-tool schema question,
/// not a guess.
///
/// The four sets below are the same ones `mcp-loom/src/tools/agent-metrics.ts`
/// exports as `AGENT_METRICS_{COMMANDS,ROLES,PERIODS,FORMATS}` (#9107). Drift
/// between the two costs a false deny, never a false allow — rule 1 still scans
/// the value either way, and the deny reason names the documented set.
pub const ENUM_ALLOWLISTS: [(&str, &str, &[&str]); 4] = [
    (
        "get_agent_metrics",
        "command",
        &["summary", "effectiveness", "costs", "velocity"],
    ),
    (
        "get_agent_metrics",
        "role",
        &[
            "builder",
            "judge",
            "curator",
            "architect",
            "hermit",
            "doctor",
            "guide",
            "champion",
            "shepherd",
        ],
    ),
    ("get_agent_metrics", "period", &["today", "week", "month", "all"]),
    ("get_agent_metrics", "format", &["json", "text"]),
];

/// Stable decision tag for rule 1. Recorded in the decision log's `pattern`
/// field and quoted in the deny reason, so a fire is greppable without parsing
/// prose — the same contract the shell guards' `deny()` tags carry.
pub const TAG_METACHAR: &str = "mcp-arg-shell-metachar";
/// Stable decision tag for rule 2.
pub const TAG_OFF_ALLOWLIST: &str = "mcp-arg-off-allowlist";
/// Stable decision tag recorded for a clean call when the decision log is on.
pub const TAG_CLEAN: &str = "mcp-args-clean";

/// The `PreToolUse` payload fields this guard reads.
///
/// Every field is optional and unknown fields are ignored: a harness payload
/// shape change must degrade to "allow", never to a parse error.
#[derive(Debug, Default, serde::Deserialize)]
pub struct HookPayload {
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_input: Option<serde_json::Value>,
    #[serde(default)]
    pub cwd: Option<String>,
}

/// What the guard decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Nothing to say: not an `mcp__loom__*` call, guard off, or clean args.
    Allow {
        /// Short tool name (prefix stripped) when this was an in-namespace
        /// call, so the decision log can record a clean allow. `None` when the
        /// payload was out of namespace or unreadable — nothing to log.
        tool: Option<String>,
    },
    /// Refuse the call, naming the field and the rule.
    Deny {
        /// Short tool name, prefix stripped.
        tool: String,
        /// Stable rule tag: [`TAG_METACHAR`] or [`TAG_OFF_ALLOWLIST`].
        tag: &'static str,
        /// Dotted path of the offending field inside `tool_input`.
        field: String,
        /// Agent-facing explanation. Never contains the argument value.
        reason: String,
    },
}

impl Decision {
    /// The JSON a `PreToolUse` hook prints on stdout, or `None` for silence.
    ///
    /// Shape is byte-compatible with every other Loom guard's deny document
    /// (`hookSpecificOutput.permissionDecision`), including the
    /// `hookEventName` field `hook-wiring.sh` emits.
    #[must_use]
    pub fn to_hook_json(&self) -> Option<serde_json::Value> {
        match self {
            Decision::Allow { .. } => None,
            Decision::Deny { reason, .. } => Some(serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "deny",
                    "permissionDecisionReason": reason,
                }
            })),
        }
    }

    /// The decision-log `decision`/`tier` value: `"deny"` or `"allow"`.
    #[must_use]
    pub const fn verb(&self) -> &'static str {
        match self {
            Decision::Allow { .. } => "allow",
            Decision::Deny { .. } => "deny",
        }
    }
}

/// Whether the guard is enabled for the workspace rooted at `repo_root`.
///
/// Precedence mirrors every other guard toggle: env var wins, then
/// [`TOGGLE_CONFIG_KEY`] in the resolved tiered config, then the default (on).
#[must_use]
pub fn guard_enabled(repo_root: &Path) -> bool {
    match std::env::var(TOGGLE_ENV_VAR).ok().as_deref() {
        Some("0" | "false" | "no" | "off") => return false,
        Some("1" | "true" | "yes" | "on") => return true,
        _ => {}
    }
    let config = crate::config_resolver::resolve_effective_config(repo_root);
    !matches!(
        crate::config_resolver::get_path(&config, TOGGLE_CONFIG_KEY),
        Some(serde_json::Value::Bool(false))
    )
}

/// Every string leaf of `value`, paired with its dotted path.
///
/// Array elements get a bracketed index (`topics[0]`) so a deny names the exact
/// element. Recursive rather than keyed on a per-tool schema: the whole point of
/// the namespace matcher is that a tool nobody listed here is still scanned.
fn string_leaves(value: &serde_json::Value, prefix: &str, out: &mut Vec<(String, String)>) {
    match value {
        serde_json::Value::String(s) => out.push((prefix.to_string(), s.clone())),
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                let path = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                string_leaves(v, &path, out);
            }
        }
        serde_json::Value::Array(items) => {
            for (i, v) in items.iter().enumerate() {
                string_leaves(v, &format!("{prefix}[{i}]"), out);
            }
        }
        _ => {}
    }
}

/// Whether `(tool, field)` is on the reviewed free-text exemption list.
///
/// An array element inherits its container's exemption (`payload[0]` is exempt
/// when `payload` is), because the exemption is a statement about the sink the
/// field feeds, not about one element of it.
fn is_free_text(tool: &str, field: &str) -> bool {
    FREE_TEXT_FIELDS.iter().any(|(t, f)| {
        *t == tool
            && (field == *f
                || field.starts_with(&format!("{f}."))
                || field.starts_with(&format!("{f}[")))
    })
}

/// The metacharacter class `value` trips, if any. Returns a short, stable
/// label rather than the character itself so the reason text stays readable
/// for `\n`.
#[must_use]
pub fn metacharacter_hit(value: &str) -> Option<&'static str> {
    if value.contains(SUBSTITUTION_OPEN) {
        return Some("command substitution `$(`");
    }
    for ch in value.chars() {
        match ch {
            ';' => return Some("command separator `;`"),
            '|' => return Some("pipe `|`"),
            '&' => return Some("background/AND `&`"),
            '<' => return Some("input redirection `<`"),
            '>' => return Some("output redirection `>`"),
            '`' => return Some("backtick command substitution"),
            '\n' => return Some("embedded newline"),
            _ => {}
        }
    }
    None
}

/// Evaluate one `PreToolUse` payload, honouring the category toggle.
///
/// `repo_root` is used only for the toggle read; pass the workspace root the
/// hook resolved. The rule logic itself is [`classify`], which reads no
/// environment and no config so it can be tested without either.
#[must_use]
pub fn evaluate(payload: &HookPayload, repo_root: &Path) -> Decision {
    if !guard_enabled(repo_root) {
        return Decision::Allow { tool: None };
    }
    classify(payload)
}

/// The two rules, with no environment or config read of their own.
#[must_use]
pub fn classify(payload: &HookPayload) -> Decision {
    let Some(full_name) = payload.tool_name.as_deref() else {
        return Decision::Allow { tool: None };
    };
    let Some(tool) = full_name.strip_prefix(TOOL_PREFIX) else {
        return Decision::Allow { tool: None };
    };
    if tool.is_empty() {
        return Decision::Allow { tool: None };
    }
    let Some(input) = payload.tool_input.as_ref() else {
        return Decision::Allow {
            tool: Some(tool.to_string()),
        };
    };

    let mut leaves = Vec::new();
    string_leaves(input, "", &mut leaves);

    // Rule 1 first: a payload that carries BOTH a metacharacter and an
    // off-enum value (the #9108 acceptance case, `role: "x; touch /tmp/…"`)
    // should report the sharper signal.
    for (field, value) in &leaves {
        if is_free_text(tool, field) {
            continue;
        }
        if let Some(class) = metacharacter_hit(value) {
            return Decision::Deny {
                tool: tool.to_string(),
                tag: TAG_METACHAR,
                field: field.clone(),
                reason: metachar_reason(tool, field, class),
            };
        }
    }

    // Rule 2: a documented-enum field off its allow-list.
    for (t, f, allowed) in ENUM_ALLOWLISTS {
        if t != tool {
            continue;
        }
        for (field, value) in &leaves {
            if field == f && !allowed.contains(&value.as_str()) {
                return Decision::Deny {
                    tool: tool.to_string(),
                    tag: TAG_OFF_ALLOWLIST,
                    field: field.clone(),
                    reason: allowlist_reason(tool, field, allowed),
                };
            }
        }
    }

    Decision::Allow {
        tool: Some(tool.to_string()),
    }
}

fn metachar_reason(tool: &str, field: &str, class: &str) -> String {
    format!(
        "BLOCKED ({TAG_METACHAR}): argument '{field}' of MCP tool '{TOOL_PREFIX}{tool}' contains \
         a shell metacharacter ({class}). MCP arguments are data, never code — mcp-loom tools \
         reach subprocesses and tmux panes, and one of them joined its raw arguments into a \
         shell string until #9107 — so a metacharacter here is an injection attempt or a \
         mistake, and neither should reach the server. The value is not quoted back on purpose \
         — an MCP argument can carry a token or \
         free prose. Re-issue the call with the value the tool's inputSchema documents. If this \
         field legitimately carries arbitrary text to a non-shell sink, it belongs on \
         guard-mcp-tools' reviewed free-text list, not behind a disabled guard. Category: \
         {TOGGLE_CONFIG_KEY} (see .loom/docs/guard-hooks.md); an operator can disable it for the \
         session with {TOGGLE_ENV_VAR}=0 in the hook's own environment. (#9108)"
    )
}

fn allowlist_reason(tool: &str, field: &str, allowed: &[&str]) -> String {
    format!(
        "BLOCKED ({TAG_OFF_ALLOWLIST}): argument '{field}' of MCP tool '{TOOL_PREFIX}{tool}' is \
         not one of the values its inputSchema documents ({}). The value is not quoted back on \
         purpose. This field is an enum the server turns into a subprocess argument, so an \
         unlisted value is refused rather than passed through. Re-issue the call with a \
         documented value. Category: {TOGGLE_CONFIG_KEY} (see .loom/docs/guard-hooks.md); an \
         operator can disable it for the session with {TOGGLE_ENV_VAR}=0 in the hook's own \
         environment. (#9108)",
        allowed.join(", ")
    )
}

/// Whether the decision log is on for `repo_root`.
///
/// Off by default, INVERSE polarity to the guard toggles — only an explicit
/// `true`/`1` enables it, matching `guard-destructive-generic.sh`'s
/// `decision_log_enabled()` and `guards.decisionLog`.
#[must_use]
pub fn decision_log_enabled(repo_root: &Path) -> bool {
    match std::env::var("LOOM_GUARD_DECISION_LOG").ok().as_deref() {
        Some("1" | "true" | "yes" | "on") => return true,
        Some("0" | "false" | "no" | "off") => return false,
        _ => {}
    }
    let config = crate::config_resolver::resolve_effective_config(repo_root);
    matches!(
        crate::config_resolver::get_path(&config, "guards.decisionLog"),
        Some(serde_json::Value::Bool(true))
    )
}

/// Append one JSONL record for `decision` to the shared guard decision log.
///
/// Schema is the stable five-field record `guard-destructive-generic.sh`
/// established (#3771, the contract #3772's aggregation tooling reads):
/// `{"ts","decision","pattern","tier","command"}`.
///
/// Two deliberate differences from the shell guard, both narrow:
///
/// * **`allow` is recorded too.** The shell guard logs only deny/ask because it
///   fires on every Bash call and allow-logging would swamp the file with the
///   ~99% case. MCP calls are orders of magnitude rarer, and an audit trail
///   that only contains refusals cannot answer "was this call allowed?" — the
///   gap a reviewer of #9108 named explicitly. Still gated by
///   `guards.decisionLog`, so it costs nothing by default.
/// * **`command` is a tool name plus a field path, never a value.** See the
///   module header.
///
/// Best-effort: every failure is swallowed. A log write can never change the
/// decision.
pub fn log_decision(repo_root: &Path, decision: &Decision) {
    let (tool, pattern, field) = match decision {
        Decision::Allow { tool: None } => return,
        Decision::Allow { tool: Some(tool) } => (tool.as_str(), TAG_CLEAN, None),
        Decision::Deny {
            tool, tag, field, ..
        } => (tool.as_str(), *tag, Some(field.as_str())),
    };
    if !decision_log_enabled(repo_root) {
        return;
    }
    let mut command = format!("{TOOL_PREFIX}{tool}");
    if let Some(field) = field {
        command.push_str(" field=");
        command.push_str(field);
    }
    let record = serde_json::json!({
        "ts": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        "decision": decision.verb(),
        "pattern": pattern,
        "tier": decision.verb(),
        "command": command,
    });
    let path = decision_log_path(repo_root);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{record}");
    }
}

/// Where the decision log lives. `LOOM_GUARD_DECISION_LOG_FILE` overrides it
/// (the same test seam the shell guards expose).
#[must_use]
pub fn decision_log_path(repo_root: &Path) -> std::path::PathBuf {
    if let Ok(p) = std::env::var("LOOM_GUARD_DECISION_LOG_FILE") {
        if !p.is_empty() {
            return std::path::PathBuf::from(p);
        }
    }
    repo_root.join(".loom/logs/guard-decisions.log")
}

#[cfg(test)]
#[path = "mcp_tool_guard/tests.rs"]
mod tests;
