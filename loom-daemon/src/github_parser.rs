//! GitHub CLI output parser for detecting GitHub events from terminal output.
//!
//! This module parses terminal output to detect when agents interact with GitHub
//! through the `gh` CLI, capturing:
//! - Issue creation/closing
//! - PR creation/merging/closing
//! - Label changes
//! - Review submissions
//!
//! # Example
//!
//! ```ignore
//! use github_parser::parse_github_events;
//!
//! let output = "Creating pull request for feature/issue-42 into main...\nhttps://github.com/owner/repo/pull/123";
//! let events = parse_github_events(output);
//! // Returns: [GitHubEvent { event_type: PrCreated, pr_number: Some(123), ... }]
//! ```

use crate::activity::{PromptGitHubEvent, PromptGitHubEventType};
use regex::Regex;

/// A parsed GitHub event from terminal output
#[derive(Debug, Clone)]
pub struct ParsedGitHubEvent {
    pub event_type: PromptGitHubEventType,
    pub issue_number: Option<i32>,
    pub pr_number: Option<i32>,
    pub labels_added: Vec<String>,
    pub labels_removed: Vec<String>,
}

impl ParsedGitHubEvent {
    /// Convert to a `PromptGitHubEvent` for database storage
    pub fn to_prompt_github_event(&self, input_id: Option<i64>) -> PromptGitHubEvent {
        let (label_before, label_after) =
            if !self.labels_added.is_empty() || !self.labels_removed.is_empty() {
                // For label changes, we track removed as "before" and added as "after"
                (
                    if self.labels_removed.is_empty() {
                        None
                    } else {
                        Some(self.labels_removed.clone())
                    },
                    if self.labels_added.is_empty() {
                        None
                    } else {
                        Some(self.labels_added.clone())
                    },
                )
            } else {
                (None, None)
            };

        PromptGitHubEvent {
            id: None,
            input_id,
            issue_number: self.issue_number,
            pr_number: self.pr_number,
            label_before,
            label_after,
            event_type: self.event_type.clone(),
        }
    }
}

/// Parse terminal output for GitHub CLI events
///
/// Detects patterns from `gh` CLI output such as:
/// - `https://github.com/owner/repo/issues/123` (issue created)
/// - `https://github.com/owner/repo/pull/456` (PR created)
/// - `Merged pull request #789`
/// - `Closed issue #123`
/// - Label change patterns
#[allow(clippy::too_many_lines)]
pub fn parse_github_events(output: &str) -> Vec<ParsedGitHubEvent> {
    let mut events = Vec::new();

    // Pattern: Issue/PR URL from gh create commands
    // Matches: https://github.com/owner/repo/issues/123
    // Matches: https://github.com/owner/repo/pull/456
    if let Ok(url_re) = Regex::new(r"https://github\.com/[^/]+/[^/]+/(issues|pull)/(\d+)") {
        for cap in url_re.captures_iter(output) {
            if let (Some(type_match), Some(number_match)) = (cap.get(1), cap.get(2)) {
                if let Ok(number) = number_match.as_str().parse::<i32>() {
                    let event = match type_match.as_str() {
                        "issues" => ParsedGitHubEvent {
                            event_type: PromptGitHubEventType::IssueCreated,
                            issue_number: Some(number),
                            pr_number: None,
                            labels_added: Vec::new(),
                            labels_removed: Vec::new(),
                        },
                        "pull" => ParsedGitHubEvent {
                            event_type: PromptGitHubEventType::PrCreated,
                            issue_number: None,
                            pr_number: Some(number),
                            labels_added: Vec::new(),
                            labels_removed: Vec::new(),
                        },
                        _ => continue,
                    };
                    events.push(event);
                }
            }
        }
    }

    // Pattern: PR merged
    // Matches: "Merged pull request #123" or "Pull request #123 has been merged"
    // Also matches: "merged" in gh pr merge output
    if let Ok(merge_re) = Regex::new(r"(?i)(?:merged|merging)\s+(?:pull\s+request\s+)?#?(\d+)") {
        for cap in merge_re.captures_iter(output) {
            if let Some(number_match) = cap.get(1) {
                if let Ok(number) = number_match.as_str().parse::<i32>() {
                    events.push(ParsedGitHubEvent {
                        event_type: PromptGitHubEventType::PrMerged,
                        issue_number: None,
                        pr_number: Some(number),
                        labels_added: Vec::new(),
                        labels_removed: Vec::new(),
                    });
                }
            }
        }
    }

    // Pattern: Issue closed
    // Matches: "Closed issue #123" or "Issue #123 closed"
    if let Ok(close_issue_re) =
        Regex::new(r"(?i)(?:closed?\s+issue\s+#?(\d+)|issue\s+#?(\d+)\s+closed)")
    {
        for cap in close_issue_re.captures_iter(output) {
            let number = cap.get(1).or_else(|| cap.get(2));
            if let Some(number_match) = number {
                if let Ok(number) = number_match.as_str().parse::<i32>() {
                    events.push(ParsedGitHubEvent {
                        event_type: PromptGitHubEventType::IssueClosed,
                        issue_number: Some(number),
                        pr_number: None,
                        labels_added: Vec::new(),
                        labels_removed: Vec::new(),
                    });
                }
            }
        }
    }

    // Pattern: PR closed (without merge)
    // Matches: "Closed pull request #123"
    if let Ok(close_pr_re) = Regex::new(r"(?i)closed?\s+pull\s+request\s+#?(\d+)") {
        for cap in close_pr_re.captures_iter(output) {
            if let Some(number_match) = cap.get(1) {
                if let Ok(number) = number_match.as_str().parse::<i32>() {
                    events.push(ParsedGitHubEvent {
                        event_type: PromptGitHubEventType::PrClosed,
                        issue_number: None,
                        pr_number: Some(number),
                        labels_added: Vec::new(),
                        labels_removed: Vec::new(),
                    });
                }
            }
        }
    }

    // Pattern: Label added via gh issue/pr edit --add-label
    // Output typically shows: "Updated issue #123" or similar after label changes
    // We also look for the command pattern in case it's echoed
    // Regex matches: --add-label value OR --add-label "value" OR --add-label 'value'
    if let Ok(label_add_re) = Regex::new(r#"--add-label[=\s]+["']?([^"'\s,]+(?:,[^"'\s,]+)*)["']?"#)
    {
        for cap in label_add_re.captures_iter(output) {
            if let Some(labels_match) = cap.get(1) {
                let labels: Vec<String> = labels_match
                    .as_str()
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();

                if !labels.is_empty() {
                    // Try to find associated issue/PR number
                    let (issue_number, pr_number) = extract_issue_pr_from_context(output);

                    events.push(ParsedGitHubEvent {
                        event_type: PromptGitHubEventType::LabelAdded,
                        issue_number,
                        pr_number,
                        labels_added: labels,
                        labels_removed: Vec::new(),
                    });
                }
            }
        }
    }

    // Pattern: Label removed via gh issue/pr edit --remove-label
    if let Ok(label_remove_re) =
        Regex::new(r#"--remove-label[=\s]+["']?([^"'\s,]+(?:,[^"'\s,]+)*)["']?"#)
    {
        for cap in label_remove_re.captures_iter(output) {
            if let Some(labels_match) = cap.get(1) {
                let labels: Vec<String> = labels_match
                    .as_str()
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();

                if !labels.is_empty() {
                    let (issue_number, pr_number) = extract_issue_pr_from_context(output);

                    events.push(ParsedGitHubEvent {
                        event_type: PromptGitHubEventType::LabelRemoved,
                        issue_number,
                        pr_number,
                        labels_added: Vec::new(),
                        labels_removed: labels,
                    });
                }
            }
        }
    }

    // Pattern: Review submitted
    // gh pr review output: "Approved pull request #123"
    if let Ok(review_re) = Regex::new(
        r"(?i)(approved|requested\s+changes\s+(?:on|for))\s+(?:pull\s+request\s+)?#?(\d+)",
    ) {
        for cap in review_re.captures_iter(output) {
            if let Some(number_match) = cap.get(2) {
                if let Ok(number) = number_match.as_str().parse::<i32>() {
                    events.push(ParsedGitHubEvent {
                        event_type: PromptGitHubEventType::ReviewSubmitted,
                        issue_number: None,
                        pr_number: Some(number),
                        labels_added: Vec::new(),
                        labels_removed: Vec::new(),
                    });
                }
            }
        }
    }

    events
}

/// Extract issue or PR number from command context
fn extract_issue_pr_from_context(output: &str) -> (Option<i32>, Option<i32>) {
    // Try to find "issue edit N" or "issue N" patterns
    if let Ok(issue_re) = Regex::new(r"(?i)gh\s+issue\s+(?:edit\s+)?#?(\d+)") {
        if let Some(cap) = issue_re.captures(output) {
            if let Some(m) = cap.get(1) {
                if let Ok(n) = m.as_str().parse::<i32>() {
                    return (Some(n), None);
                }
            }
        }
    }

    // Try to find "pr edit N" patterns
    if let Ok(pr_re) = Regex::new(r"(?i)gh\s+pr\s+(?:edit\s+)?#?(\d+)") {
        if let Some(cap) = pr_re.captures(output) {
            if let Some(m) = cap.get(1) {
                if let Ok(n) = m.as_str().parse::<i32>() {
                    return (None, Some(n));
                }
            }
        }
    }

    (None, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_issue_created() {
        let output = "Creating issue...\nhttps://github.com/owner/repo/issues/42";
        let events = parse_github_events(output);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, PromptGitHubEventType::IssueCreated);
        assert_eq!(events[0].issue_number, Some(42));
    }

    #[test]
    fn test_parse_pr_created() {
        let output = "Creating pull request for feature/test into main in owner/repo\n\nhttps://github.com/owner/repo/pull/123";
        let events = parse_github_events(output);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, PromptGitHubEventType::PrCreated);
        assert_eq!(events[0].pr_number, Some(123));
    }

    #[test]
    fn test_parse_pr_merged() {
        let output = "Merged pull request #456 (squash)";
        let events = parse_github_events(output);
        assert!(!events.is_empty());
        let merge_event = events
            .iter()
            .find(|e| e.event_type == PromptGitHubEventType::PrMerged);
        assert!(merge_event.is_some());
        if let Some(evt) = merge_event {
            assert_eq!(evt.pr_number, Some(456));
        }
    }

    #[test]
    fn test_parse_issue_closed() {
        let output = "Closed issue #789";
        let events = parse_github_events(output);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, PromptGitHubEventType::IssueClosed);
        assert_eq!(events[0].issue_number, Some(789));
    }

    #[test]
    fn test_parse_label_added() {
        let output = "gh issue edit 42 --add-label \"loom:building\"";
        let events = parse_github_events(output);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, PromptGitHubEventType::LabelAdded);
        assert_eq!(events[0].labels_added, vec!["loom:building"]);
        assert_eq!(events[0].issue_number, Some(42));
    }

    #[test]
    fn test_parse_label_removed() {
        let output = "gh issue edit 42 --remove-label loom:issue";
        let events = parse_github_events(output);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, PromptGitHubEventType::LabelRemoved);
        assert_eq!(events[0].labels_removed, vec!["loom:issue"]);
    }

    #[test]
    fn test_parse_review_approved() {
        let output = "Approved pull request #123";
        let events = parse_github_events(output);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, PromptGitHubEventType::ReviewSubmitted);
        assert_eq!(events[0].pr_number, Some(123));
    }

    #[test]
    fn test_parse_multiple_events() {
        let output = "gh issue edit 42 --remove-label loom:issue --add-label loom:building";
        let events = parse_github_events(output);
        assert_eq!(events.len(), 2);

        let remove_event = events
            .iter()
            .find(|e| e.event_type == PromptGitHubEventType::LabelRemoved);
        let add_event = events
            .iter()
            .find(|e| e.event_type == PromptGitHubEventType::LabelAdded);

        assert!(remove_event.is_some());
        assert!(add_event.is_some());
        if let Some(evt) = remove_event {
            assert_eq!(evt.labels_removed, vec!["loom:issue"]);
        }
        if let Some(evt) = add_event {
            assert_eq!(evt.labels_added, vec!["loom:building"]);
        }
    }

    #[test]
    fn test_no_false_positives() {
        let output = "Compiling loom-daemon v0.1.0\nFinished release target";
        let events = parse_github_events(output);
        assert!(events.is_empty());
    }

    #[test]
    fn test_to_prompt_github_event() {
        let parsed = ParsedGitHubEvent {
            event_type: PromptGitHubEventType::LabelAdded,
            issue_number: Some(42),
            pr_number: None,
            labels_added: vec!["loom:building".to_string()],
            labels_removed: Vec::new(),
        };

        let event = parsed.to_prompt_github_event(Some(100));
        assert_eq!(event.input_id, Some(100));
        assert_eq!(event.issue_number, Some(42));
        assert_eq!(event.label_after, Some(vec!["loom:building".to_string()]));
        assert!(event.label_before.is_none());
    }
}
