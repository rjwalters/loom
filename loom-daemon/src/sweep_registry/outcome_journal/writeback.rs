//! Post-success sweep-outcome write-back comment (Issue #9056).
//!
//! # What this is
//!
//! At a sweep's terminal `Success` transition — the SAME call site as
//! [`super::append_outcome_journal`]/[`super::append_outcome_telemetry_journal`],
//! once the [`telemetry::SweepOutcomeRecord`] has been assembled AND durably
//! appended — post ONE best-effort Markdown comment onto the sweep's originating GitHub
//! **issue** (not just the PR) summarizing that record: the Curator's
//! `loom:complexity` / `loom:points` estimate, actual token burn, wall-clock
//! duration (total and per-phase), Doctor cycle count, and the first-pass
//! Judge verdict. Deliberately excludes CI minutes — no join between sweep
//! outcomes and `loom-daemon ci-telemetry`'s `ci.run`/`ci.job` records exists
//! yet; see `defaults/docs/ci-observability.md`.
//!
//! # Opt-in, FLAGS-OFF by default
//!
//! Gated behind `autonomous.sweepOutcomeWriteback.enabled` (env override
//! [`SWEEP_OUTCOME_WRITEBACK_ENABLE_ENV`]), resolved fresh at post time —
//! **env > config > default(false)** — rather than cached on
//! [`crate::sweep_registry::SweepRegistry`] at provision time the way its
//! sibling brakes are (`noop_cooldown`, `prless_retry`, …). This is
//! deliberate: the write-back fires at most once per sweep, never on a hot
//! per-tick path, so the extra config read costs nothing, and it avoids
//! adding a field to `sweep_registry/mod.rs` — a file already over the
//! file-size ratchet's threshold and frozen at its current size (see
//! `.loom/docs/file-size-policy.md`), which every sibling brake's cached-field
//! approach would otherwise require growing.
//!
//! # Idempotency (no double-post) — keyed per SWEEP, not per issue
//!
//! Every comment opens with [`sweep_outcome_writeback_marker`] — the
//! [`SWEEP_OUTCOME_WRITEBACK_COMMENT_MARKER`] prefix plus `sweep=<sweep_id>`.
//! Before posting, the issue's existing comments are searched (one paginated
//! REST read, `--jq`-filtered server-side to bodies starting with THIS
//! sweep's full marker) for a prior write-back. A hit skips the post
//! entirely. Keying on the sweep id rather than the issue matters: a later
//! sweep on the same issue (a partial-increment slice reusing the issue
//! number, #3599/#3667; a re-opened issue; a re-dispatch) has its own
//! actuals and still posts, while the same terminal transition observed
//! twice (same `sweep_id`) does not — the defensive property this journal's normal-path
//! callers do not otherwise need, since `append_outcome_journal` itself fires
//! once per terminal transition by contract, but this is new forge-WRITE
//! behavior riding alongside a local-journal append, so it earns its own
//! belt-and-suspenders check. An unreadable check (timeout, non-zero exit,
//! forge error) fails CLOSED — i.e. skips posting this pass — rather than
//! risking a duplicate: the comment is informational and best-effort, so a
//! rare missed post is cheaper than a rare duplicate.
//!
//! # Never blocks or fails the terminal transition
//!
//! Every step here is the same best-effort shape as
//! [`super::complexity_signal`]'s issue read and
//! [`super::super::prless_retry`]'s comment helpers: a `gh` failure, timeout,
//! or missing repo context is logged and swallowed, never propagated. Called
//! AFTER every durable local write in
//! [`super::append_outcome_telemetry_journal`] (`runtime_usage::finish_sweep`,
//! `lifecycle::finish_execution`, and the `sweep.outcome` telemetry append),
//! so even a hung `gh` process cannot delay those writes. Each of the (at
//! most three) `gh` calls is individually bounded by `reap_gh_timeout()`.

use super::*;

/// Env var toggling the sweep-outcome issue write-back (Issue #9056).
/// `0`/`false`/`no`/`off` disables; `1`/`true`/`yes`/`on` enables. Overrides
/// config. Defaults OFF — unlike the dispatch-efficiency brakes
/// (`noop_cooldown`, `prless_retry`, …) this posts a forge-visible,
/// human-facing comment, so it opts IN rather than opting out.
pub const SWEEP_OUTCOME_WRITEBACK_ENABLE_ENV: &str = "LOOM_SWEEP_OUTCOME_WRITEBACK";

/// Hidden-marker PREFIX shared by every write-back comment (Issue #9056),
/// mirroring `prless_retry::PRLESS_RETRY_COMMENT_MARKER`'s convention. The
/// full marker is [`sweep_outcome_writeback_marker`], which appends the
/// sweep id so idempotency is per sweep, not per issue.
pub(crate) const SWEEP_OUTCOME_WRITEBACK_COMMENT_MARKER: &str = "<!-- loom:sweep-outcome-writeback";

/// The full hidden marker for `sweep_id`'s write-back comment:
/// `<!-- loom:sweep-outcome-writeback sweep=<sweep_id> -->`. The closing
/// ` -->` terminates the id, so `sweep=a` never prefix-matches `sweep=ab`.
#[must_use]
pub(crate) fn sweep_outcome_writeback_marker(sweep_id: &str) -> String {
    format!("{SWEEP_OUTCOME_WRITEBACK_COMMENT_MARKER} sweep={sweep_id} -->")
}

/// Resolved sweep-outcome-writeback parameters (Issue #9056).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SweepOutcomeWritebackConfig {
    /// Whether the write-back comment is posted at all. `false` (the
    /// [`Default`]) is byte-for-byte the pre-#9056 path: no extra forge read,
    /// no comment, no behavior change.
    pub enabled: bool,
}

/// The subset of `.loom/config.json -> autonomous.sweepOutcomeWriteback` this
/// module consumes (Issue #9056). Mirrors
/// `crate::sweep_registry::NoopCooldownFileConfig`'s shape: the field is
/// `Option` so an absent key falls through to the env-var / built-in-default
/// resolution.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepOutcomeWritebackFileConfig {
    /// `autonomous.sweepOutcomeWriteback.enabled`.
    pub enabled: Option<bool>,
}

/// Read `.loom/config.json -> autonomous.sweepOutcomeWriteback` (Issue
/// #9056), soft-failing to [`SweepOutcomeWritebackFileConfig::default`] on a
/// missing file, malformed JSON, or an absent block — mirrors
/// `crate::sweep_registry::read_noop_cooldown_file_config`.
#[must_use]
pub fn read_sweep_outcome_writeback_file_config(
    repo_root: &Path,
) -> SweepOutcomeWritebackFileConfig {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let Some(c) = crate::config_resolver::get_path(&effective, "autonomous.sweepOutcomeWriteback")
    else {
        return SweepOutcomeWritebackFileConfig::default();
    };
    SweepOutcomeWritebackFileConfig {
        enabled: c.get("enabled").and_then(serde_json::Value::as_bool),
    }
}

/// Resolve [`SweepOutcomeWritebackConfig`] for `repo_root` with precedence
/// **env > config > default(false)** (Issue #9056), mirroring
/// `crate::sweep_registry::resolve_noop_cooldown_config`.
#[must_use]
pub fn resolve_sweep_outcome_writeback_config(repo_root: &Path) -> SweepOutcomeWritebackConfig {
    let file = read_sweep_outcome_writeback_file_config(repo_root);
    let enabled = if let Ok(v) = std::env::var(SWEEP_OUTCOME_WRITEBACK_ENABLE_ENV) {
        matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
    } else {
        file.enabled.unwrap_or(false)
    };
    SweepOutcomeWritebackConfig { enabled }
}

/// `Xh Ym` / `Ym Zs` / `Zs` — the coarsest two units that cover `secs`,
/// dropping a zero leading unit rather than always spelling three. Negative
/// input (should never happen; durations are `max(0)`-clamped upstream) is
/// clamped to zero rather than propagating a negative value into a forge
/// comment.
fn format_duration_secs(secs: i64) -> String {
    let secs = secs.max(0);
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

/// Render the write-back Markdown body from an already-assembled
/// [`telemetry::SweepOutcomeRecord`] (Issue #9056) — pure and
/// forge-independent, so it is unit-testable without a fake `gh`. `points` is
/// passed separately rather than as a record field: unlike `complexity`, it
/// is not part of the persisted telemetry schema (see
/// [`super::points_signal`]'s module doc for why).
#[must_use]
pub(crate) fn format_sweep_outcome_comment(
    record: &telemetry::SweepOutcomeRecord,
    points: Option<&str>,
) -> String {
    let complexity = record.complexity.as_deref().unwrap_or("unmarked");
    let points = points.unwrap_or("unmarked");

    let tokens = match (record.tokens_in, record.tokens_out) {
        (Some(tin), Some(tout)) => format!("{tin} in / {tout} out"),
        (Some(tin), None) => format!("{tin} in / unknown out"),
        (None, Some(tout)) => format!("unknown in / {tout} out"),
        (None, None) => "unknown".to_string(),
    };

    let duration = format_duration_secs(record.total_duration_sec);
    let phase_line = if record.phase_durations.is_empty() {
        String::new()
    } else {
        let phases = record
            .phase_durations
            .iter()
            .map(|p| format!("{}: {}", p.phase, format_duration_secs(p.duration_sec)))
            .collect::<Vec<_>>()
            .join(", ");
        format!("\n  - Per phase: {phases}")
    };

    let doctor_cycles = record
        .doctor_cycles
        .map_or_else(|| "unknown".to_string(), |n| n.to_string());

    let judge_verdict = record
        .judge_verdicts
        .as_ref()
        .and_then(|v| v.first())
        .map_or_else(|| "unknown".to_string(), |v| v.verdict.clone());

    format!(
        "{marker}\n\
         ### Sweep Outcome\n\n\
         - **Estimate:** complexity `{complexity}`, points `{points}`\n\
         - **Tokens:** {tokens}\n\
         - **Wall-clock duration:** {duration}{phase_line}\n\
         - **Doctor cycles:** {doctor_cycles}\n\
         - **First-pass Judge verdict:** {judge_verdict}\n\n\
         _Sweep `{sweep_id}` · best-effort telemetry write-back, opt-in via \
         `autonomous.sweepOutcomeWriteback.enabled` (#9056). CI minutes are not \
         yet joined to sweep outcomes — see `defaults/docs/ci-observability.md`._",
        marker = sweep_outcome_writeback_marker(&record.sweep_id),
        sweep_id = record.sweep_id,
    )
}

impl SweepRegistry {
    /// Best-effort, opt-in write-back of `record` onto its issue (Issue
    /// #9056). No-op when: forge writes are disabled (`skip_label_flip`), the
    /// flag resolves off (the default), the fleet rate-limit breaker is
    /// suppressing forge polling, or a prior write-back comment is already
    /// present for THIS sweep (or its presence could not be verified — see the module doc's
    /// "Idempotency" section for why an unreadable check fails closed). Never
    /// blocks or fails the caller's terminal transition.
    pub(crate) fn maybe_post_sweep_outcome_writeback(
        &self,
        issue: u32,
        record: &telemetry::SweepOutcomeRecord,
    ) {
        if self.config.skip_label_flip {
            return;
        }
        if !resolve_sweep_outcome_writeback_config(&self.config.workspace_root).enabled {
            return;
        }
        if crate::rate_limit_breaker::global_is_suppressed() {
            log::debug!(
                "sweep_outcomes: skipping issue #{issue}'s sweep-outcome write-back — the \
                 rate-limit breaker is suppressing forge polling (#9056)"
            );
            return;
        }
        match self.sweep_outcome_writeback_comment_exists(issue, &record.sweep_id) {
            Some(true) => {
                log::debug!(
                    "sweep_outcomes: issue #{issue} already carries sweep {}'s outcome \
                     write-back comment — not double-posting (#9056)",
                    record.sweep_id
                );
            }
            None => {
                log::debug!(
                    "sweep_outcomes: could not verify issue #{issue}'s existing comments — \
                     skipping the write-back this pass rather than risking a duplicate (#9056)"
                );
            }
            Some(false) => {
                let points = self.fetch_points_signal(issue);
                let body = format_sweep_outcome_comment(record, points.as_deref());
                self.post_sweep_outcome_writeback_comment(issue, &body);
            }
        }
    }

    /// Whether `issue` already carries `sweep_id`'s
    /// [`sweep_outcome_writeback_marker`] comment (Issue #9056) —
    /// `Some(true)`/`Some(false)` on a successful read, `None` on any
    /// transport failure (unresolved repo, timeout, non-zero exit), mirroring
    /// `guards::read_lease_comments`'s `--jq`-filtered, `--paginate` REST read
    /// but reporting only presence, never the comment bodies themselves.
    fn sweep_outcome_writeback_comment_exists(&self, issue: u32, sweep_id: &str) -> Option<bool> {
        let gh = self
            .config
            .gh_bin
            .clone()
            .unwrap_or_else(|| PathBuf::from("gh"));
        let mut cmd = Command::new(&gh);
        cmd.arg("api")
            .arg(format!("repos/{{owner}}/{{repo}}/issues/{issue}/comments"))
            .arg("--paginate")
            .arg("--jq")
            .arg(format!(
                r#".[] | select(.body | startswith({})) | .id"#,
                serde_json::Value::String(sweep_outcome_writeback_marker(sweep_id))
            ));
        cmd.current_dir(&self.config.workspace_root);
        crate::credential_preflight::apply_gh_config_for_root(
            &mut cmd,
            &self.config.workspace_root,
        );
        crate::gh_repo_env::apply_loom_repo_override(&mut cmd);
        let timeout = reap_gh_timeout();
        let output = output_with_timeout(cmd, timeout).ok().flatten()?;
        if !output.status.success() {
            return None;
        }
        Some(!output.stdout.is_empty())
    }

    /// Post the write-back comment (Issue #9056). Best-effort like
    /// `prless_retry::post_prless_comment`: a `gh` failure is logged at warn
    /// (this is the one forge WRITE in this module, unlike its sibling reads,
    /// so a failure earns more visibility than the debug-level skips above)
    /// and never propagated.
    fn post_sweep_outcome_writeback_comment(&self, issue: u32, body: &str) {
        let gh = self
            .config
            .gh_bin
            .clone()
            .unwrap_or_else(|| PathBuf::from("gh"));
        let mut cmd = Command::new(&gh);
        cmd.arg("issue")
            .arg("comment")
            .arg(issue.to_string())
            .arg("--body")
            .arg(body);
        cmd.current_dir(&self.config.workspace_root);
        crate::credential_preflight::apply_gh_config_for_root(
            &mut cmd,
            &self.config.workspace_root,
        );
        crate::gh_repo_env::apply_loom_repo_override(&mut cmd);
        let timeout = reap_gh_timeout();
        match output_with_timeout(cmd, timeout) {
            Ok(Some(o)) if o.status.success() => {
                log::info!(
                    "sweep_outcomes: posted sweep-outcome write-back comment on issue #{issue} \
                     (#9056)"
                );
            }
            Ok(Some(o)) => {
                let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
                log::warn!(
                    "sweep_outcomes: sweep-outcome write-back comment for issue #{issue} failed \
                     ({}): {stderr} (#9056)",
                    o.status
                );
            }
            Ok(None) => log::warn!(
                "sweep_outcomes: sweep-outcome write-back comment for issue #{issue} exceeded \
                 {}s, killed (#9056)",
                timeout.as_secs()
            ),
            Err(e) => log::warn!(
                "sweep_outcomes: could not invoke {} to post issue #{issue}'s sweep-outcome \
                 write-back comment: {e} (#9056)",
                gh.display()
            ),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::tempdir;

    fn fixture_record() -> telemetry::SweepOutcomeRecord {
        telemetry::SweepOutcomeRecord {
            repo: Some("rjwalters/loom".to_string()),
            repo_unresolved: false,
            visibility: telemetry::RepoVisibility::Private,
            issue: 9056,
            sweep_id: "sweep-issue-9056-0".to_string(),
            model: Some("sonnet".to_string()),
            effort: None,
            config: std::collections::BTreeMap::new(),
            phase_durations: vec![
                telemetry::PhaseDuration::new("curator".to_string(), 120),
                telemetry::PhaseDuration::new("builder".to_string(), 3600),
            ],
            total_duration_sec: 4000,
            result: telemetry::SweepResult::Success,
            disposition: telemetry::SweepDisposition::Landed,
            pr_number: Some(42),
            tokens_in: Some(200_000),
            tokens_out: Some(15_000),
            lines_added: Some(120),
            lines_deleted: Some(30),
            tokens_by_model: None,
            tokens_unattributed: None,
            failure_class: None,
            models_used: None,
            doctor_cycles: Some(1),
            judge_verdicts: Some(vec![telemetry::JudgeVerdict {
                attempt: 1,
                verdict: "pass".to_string(),
            }]),
            runtime: None,
            provider: None,
            profile: None,
            complexity: Some("complex".to_string()),
            tokens_status: None,
            tokens_status_reason: None,
        }
    }

    // ---- Comment formatting (pure, no `gh` needed) ----

    #[test]
    fn comment_carries_the_hidden_marker_first() {
        let body = format_sweep_outcome_comment(&fixture_record(), Some("8"));
        assert!(
            body.starts_with("<!-- loom:sweep-outcome-writeback sweep=sweep-issue-9056-0 -->\n"),
            "{body}"
        );
    }

    #[test]
    fn marker_is_keyed_per_sweep_and_terminated() {
        let a = sweep_outcome_writeback_marker("sweep-a");
        let ab = sweep_outcome_writeback_marker("sweep-ab");
        assert!(a.starts_with(SWEEP_OUTCOME_WRITEBACK_COMMENT_MARKER));
        assert_ne!(a, ab);
        assert!(!ab.starts_with(&a), "one sweep id must never prefix-match another");
    }

    #[test]
    fn comment_reports_estimate_tokens_duration_doctor_and_judge() {
        let body = format_sweep_outcome_comment(&fixture_record(), Some("8"));
        assert!(body.contains("complexity `complex`"), "{body}");
        assert!(body.contains("points `8`"), "{body}");
        assert!(body.contains("200000 in / 15000 out"), "{body}");
        assert!(body.contains("1h 6m"), "{body}");
        assert!(body.contains("curator: 2m 0s"), "{body}");
        assert!(body.contains("builder: 1h 0m"), "{body}");
        assert!(body.contains("Doctor cycles:** 1"), "{body}");
        assert!(body.contains("Judge verdict:** pass"), "{body}");
    }

    #[test]
    fn missing_optional_fields_render_as_unknown_not_fabricated() {
        let mut record = fixture_record();
        record.tokens_in = None;
        record.tokens_out = None;
        record.doctor_cycles = None;
        record.judge_verdicts = None;
        record.complexity = None;
        let body = format_sweep_outcome_comment(&record, None);
        assert!(body.contains("complexity `unmarked`"), "{body}");
        assert!(body.contains("points `unmarked`"), "{body}");
        assert!(body.contains("Tokens:** unknown"), "{body}");
        assert!(body.contains("Doctor cycles:** unknown"), "{body}");
        assert!(body.contains("Judge verdict:** unknown"), "{body}");
    }

    #[test]
    fn never_mentions_ci_minutes_as_a_measured_field() {
        let body = format_sweep_outcome_comment(&fixture_record(), Some("8"));
        // The one permitted mention is the deferred-follow-up sentence itself.
        assert_eq!(body.matches("CI minutes").count(), 1, "{body}");
        assert!(body.contains("not yet joined"), "{body}");
    }

    // ---- Config resolution: env > config > default(false) ----

    #[test]
    #[serial]
    fn defaults_to_disabled_with_no_env_or_file() {
        let dir = tempdir().unwrap();
        std::env::remove_var(SWEEP_OUTCOME_WRITEBACK_ENABLE_ENV);
        assert!(!resolve_sweep_outcome_writeback_config(dir.path()).enabled);
    }

    #[test]
    #[serial]
    fn file_config_can_enable_it() {
        let dir = tempdir().unwrap();
        std::env::remove_var(SWEEP_OUTCOME_WRITEBACK_ENABLE_ENV);
        std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
        std::fs::write(
            dir.path().join(".loom/config.json"),
            r#"{"autonomous":{"sweepOutcomeWriteback":{"enabled":true}}}"#,
        )
        .unwrap();
        assert!(resolve_sweep_outcome_writeback_config(dir.path()).enabled);
    }

    #[test]
    #[serial]
    fn env_overrides_file() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
        std::fs::write(
            dir.path().join(".loom/config.json"),
            r#"{"autonomous":{"sweepOutcomeWriteback":{"enabled":true}}}"#,
        )
        .unwrap();
        std::env::set_var(SWEEP_OUTCOME_WRITEBACK_ENABLE_ENV, "0");
        let enabled = resolve_sweep_outcome_writeback_config(dir.path()).enabled;
        std::env::remove_var(SWEEP_OUTCOME_WRITEBACK_ENABLE_ENV);
        assert!(!enabled);
    }
}
