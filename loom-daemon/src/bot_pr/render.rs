//! Shell-evalable rendering of the bot-PR config and verdict (#4765).
//!
//! The output contract is `KEY='value'` lines, one per line, safe to `eval` —
//! the same shape `champion-issue-promo.md` already consumes from
//! `classify-dependency-block.sh`. Every value is single-quoted with embedded
//! quotes escaped, so a PR title (untrusted external content, and routinely
//! containing `'`, `$`, backticks) can never break out into the caller's
//! shell.

use super::classify::{Disqualified, Qualified};
use super::config::BotPrConfig;

/// POSIX single-quote one value: `it's` → `'it'\''s'`.
#[must_use]
pub fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// `loom-daemon bot-pr config` output.
#[must_use]
pub fn render_config(cfg: &BotPrConfig) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "BOT_PR_ENABLED={}\n",
        sh_quote(if cfg.enabled { "true" } else { "false" })
    ));
    out.push_str(&format!(
        "BOT_PR_TRUSTED_AUTHORS={}\n",
        sh_quote(&cfg.trusted_bot_authors.join(","))
    ));
    out.push_str(&format!("BOT_PR_MAX_SEMVER={}\n", sh_quote(cfg.max_semver.as_str())));
    out
}

/// `loom-daemon bot-pr classify` output. Always emitted, on both verdicts —
/// the exit code is the predicate, these lines are the evidence the Champion
/// quotes into its pre-merge or fallback comment.
#[must_use]
pub fn render_verdict(verdict: &Result<Qualified, Disqualified>) -> String {
    let mut out = String::new();
    match verdict {
        Ok(q) => {
            out.push_str("BOT_PR_QUALIFIES='true'\n");
            out.push_str("BOT_PR_REASON='qualifies'\n");
            out.push_str(&format!(
                "BOT_PR_DETAIL={}\n",
                sh_quote(
                    "trusted bot author, dependency-manifest-only diff; critical-file \
                     exclusion waived and Doctor routing replaced by a rebase \
                     request; Judge approval, mergeable and CI-green unchanged"
                )
            ));
            out.push_str(&format!("BOT_PR_WAIVED={}\n", sh_quote(&q.waived.join(","))));
            out.push_str(&format!(
                "BOT_PR_BUMP_LEVEL={}\n",
                sh_quote(
                    q.bump_level
                        .map_or("", super::semver_guard::BumpLevel::as_str)
                )
            ));
            out.push_str("BOT_PR_OFFENDING_FILE=''\n");
        }
        Err(d) => {
            out.push_str("BOT_PR_QUALIFIES='false'\n");
            out.push_str(&format!("BOT_PR_REASON={}\n", sh_quote(d.code())));
            out.push_str(&format!("BOT_PR_DETAIL={}\n", sh_quote(&d.explain())));
            out.push_str("BOT_PR_WAIVED=''\n");
            out.push_str(&format!(
                "BOT_PR_BUMP_LEVEL={}\n",
                sh_quote(match d {
                    Disqualified::SemverExceeded { level, .. } => level.as_str(),
                    _ => "",
                })
            ));
            out.push_str(&format!(
                "BOT_PR_OFFENDING_FILE={}\n",
                sh_quote(match d {
                    Disqualified::NonManifestFile { path }
                    | Disqualified::WorkflowNotVersionPinOnly { path }
                    | Disqualified::WorkflowPatchUnavailable { path } => path.as_str(),
                    _ => "",
                })
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests;
