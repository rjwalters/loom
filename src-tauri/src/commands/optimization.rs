//! Automated Prompt Optimization Engine
//!
//! Automatically suggests improvements to prompts based on historical success patterns.
//! This is Phase 4 (Advanced Analytics) of the activity database system.
//!
//! Features:
//! - Template matching: Map prompts to known successful patterns
//! - Feature optimization: Adjust length, structure, specificity
//! - A/B suggestions: Offer variants to test
//! - Learning loop: Track acceptance and outcome of suggestions

use rusqlite::{params, Connection, Result as SqliteResult};
use serde::{Deserialize, Serialize};
use std::path::Path;

// ============================================================================
// Types
// ============================================================================

/// An optimization suggestion for a prompt
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct OptimizationSuggestion {
    pub id: Option<i64>,
    /// The original prompt text
    pub original_prompt: String,
    /// The suggested optimized prompt
    pub optimized_prompt: String,
    /// The type of optimization applied
    pub optimization_type: String,
    /// Reasoning/evidence for why this optimization should work
    pub reasoning: String,
    /// Confidence score (0.0 to 1.0)
    pub confidence: f64,
    /// Reference pattern ID if matched
    pub matched_pattern_id: Option<i64>,
    /// Expected improvement in success rate (as a percentage)
    pub expected_improvement: f64,
    /// Whether this suggestion was accepted by the user
    pub accepted: Option<bool>,
    /// Outcome after acceptance (if tracked)
    pub outcome: Option<String>,
    /// Timestamp when suggestion was created
    pub created_at: Option<String>,
}

/// Result of analyzing a prompt for optimization opportunities
#[derive(Debug, Serialize, Deserialize)]
pub struct PromptAnalysis {
    /// The analyzed prompt
    pub prompt: String,
    /// Word count of the prompt
    pub word_count: i32,
    /// Character count
    pub char_count: i32,
    /// Detected category/intent
    pub category: Option<String>,
    /// Specificity score (0-1, higher = more specific)
    pub specificity_score: f64,
    /// Structure quality score (0-1)
    pub structure_score: f64,
    /// List of detected issues with the prompt
    pub issues: Vec<PromptIssue>,
    /// Whether optimization is recommended
    pub needs_optimization: bool,
}

/// A detected issue with a prompt
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PromptIssue {
    /// Issue type: "too_short", "too_long", "vague", "missing_context", etc.
    pub issue_type: String,
    /// Human-readable description
    pub description: String,
    /// Severity: "low", "medium", "high"
    pub severity: String,
}

/// Optimization rule that defines how to improve prompts
#[derive(Debug, Serialize, Deserialize)]
pub struct OptimizationRule {
    pub id: Option<i64>,
    /// Rule name
    pub name: String,
    /// Rule type: "length", "structure", "specificity", "pattern"
    pub rule_type: String,
    /// Condition to trigger the rule (JSON)
    pub condition: String,
    /// Template for the optimization suggestion
    pub suggestion_template: String,
    /// Expected improvement percentage
    pub expected_improvement: f64,
    /// Whether the rule is active
    pub active: bool,
    /// Number of times applied
    pub times_applied: i32,
    /// Success rate when this rule's suggestions are accepted
    pub success_rate: f64,
}

/// Summary of optimization activity
#[derive(Debug, Serialize, Deserialize)]
pub struct OptimizationStats {
    pub total_suggestions: i32,
    pub accepted_suggestions: i32,
    pub rejected_suggestions: i32,
    pub pending_suggestions: i32,
    pub acceptance_rate: f64,
    pub avg_improvement_when_accepted: f64,
    pub suggestions_by_type: Vec<OptimizationTypeStats>,
}

/// Statistics for a specific optimization type
#[derive(Debug, Serialize, Deserialize)]
pub struct OptimizationTypeStats {
    pub optimization_type: String,
    pub count: i32,
    pub acceptance_rate: f64,
    pub avg_improvement: f64,
}

// ============================================================================
// Database Setup
// ============================================================================

/// Open connection to activity database and ensure optimization schema exists
fn open_optimization_db(workspace_path: &str) -> SqliteResult<Connection> {
    let loom_dir = Path::new(workspace_path).join(".loom");
    let db_path = loom_dir.join("activity.db");

    // Ensure .loom directory exists
    if !loom_dir.exists() {
        std::fs::create_dir_all(&loom_dir)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    }

    let conn = Connection::open(&db_path)?;

    // Create optimization-specific tables
    conn.execute_batch(
        r"
        -- Optimization suggestions table
        CREATE TABLE IF NOT EXISTS optimization_suggestions (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            original_prompt TEXT NOT NULL,
            optimized_prompt TEXT NOT NULL,
            optimization_type TEXT NOT NULL,
            reasoning TEXT NOT NULL,
            confidence REAL NOT NULL DEFAULT 0.5,
            matched_pattern_id INTEGER,
            expected_improvement REAL DEFAULT 0.0,
            accepted INTEGER,
            outcome TEXT,
            created_at TEXT DEFAULT (datetime('now')),
            accepted_at TEXT,
            outcome_recorded_at TEXT,
            FOREIGN KEY (matched_pattern_id) REFERENCES prompt_patterns(id)
        );

        CREATE INDEX IF NOT EXISTS idx_optimization_type ON optimization_suggestions(optimization_type);
        CREATE INDEX IF NOT EXISTS idx_optimization_accepted ON optimization_suggestions(accepted);
        CREATE INDEX IF NOT EXISTS idx_optimization_created ON optimization_suggestions(created_at DESC);

        -- Optimization rules table
        CREATE TABLE IF NOT EXISTS optimization_rules (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL UNIQUE,
            rule_type TEXT NOT NULL,
            condition TEXT NOT NULL,
            suggestion_template TEXT NOT NULL,
            expected_improvement REAL DEFAULT 0.1,
            active INTEGER DEFAULT 1,
            times_applied INTEGER DEFAULT 0,
            success_count INTEGER DEFAULT 0,
            failure_count INTEGER DEFAULT 0,
            success_rate REAL DEFAULT 0.0,
            created_at TEXT DEFAULT (datetime('now')),
            updated_at TEXT DEFAULT (datetime('now'))
        );

        CREATE INDEX IF NOT EXISTS idx_rules_type ON optimization_rules(rule_type);
        CREATE INDEX IF NOT EXISTS idx_rules_active ON optimization_rules(active);
        ",
    )?;

    // Initialize default optimization rules if none exist
    let rule_count: i32 = conn
        .query_row("SELECT COUNT(*) FROM optimization_rules", [], |row| row.get(0))
        .unwrap_or(0);

    if rule_count == 0 {
        initialize_default_rules(&conn)?;
    }

    Ok(conn)
}

/// Initialize default optimization rules
fn initialize_default_rules(conn: &Connection) -> SqliteResult<()> {
    let default_rules = vec![
        (
            "short_prompt",
            "length",
            r#"{"min_words": 0, "max_words": 5}"#,
            "Add more context: {{original}}. Consider specifying the scope, expected behavior, and any constraints.",
            0.15,
        ),
        (
            "long_prompt",
            "length",
            r#"{"min_words": 100, "max_words": 999}"#,
            "Condense to key points: {{original_condensed}}",
            0.10,
        ),
        (
            "vague_prompt",
            "specificity",
            r##"{"keywords": ["fix", "update", "change", "modify"], "lacks": ["#", "function", "file", "error"]}"##,
            "Be more specific: Instead of '{{original}}', try: '{{original}} in {{suggested_scope}}'",
            0.20,
        ),
        (
            "missing_issue_ref",
            "structure",
            r##"{"lacks_pattern": "#[0-9]+"}"##,
            "Reference the issue: {{original}} (see issue #N for details)",
            0.12,
        ),
        (
            "imperative_voice",
            "structure",
            r#"{"starts_with": ["can you", "could you", "please", "would you"]}"#,
            "Use imperative: {{action_form}}",
            0.08,
        ),
        (
            "pattern_match_build",
            "pattern",
            r#"{"category": "build", "min_success_rate": 0.7}"#,
            "Use proven pattern: {{matched_pattern}}",
            0.25,
        ),
        (
            "pattern_match_fix",
            "pattern",
            r#"{"category": "fix", "min_success_rate": 0.7}"#,
            "Use proven pattern: {{matched_pattern}}",
            0.25,
        ),
        (
            "add_test_mention",
            "structure",
            r#"{"lacks": ["test", "spec", "verify"]}"#,
            "Include testing: {{original}}. Include unit tests.",
            0.15,
        ),
    ];

    for (name, rule_type, condition, template, expected_improvement) in default_rules {
        conn.execute(
            "INSERT OR IGNORE INTO optimization_rules (name, rule_type, condition, suggestion_template, expected_improvement)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![name, rule_type, condition, template, expected_improvement],
        )?;
    }

    Ok(())
}

// ============================================================================
// Prompt Analysis Functions
// ============================================================================

/// Analyze a prompt and identify optimization opportunities
#[tauri::command]
pub fn analyze_prompt(workspace_path: &str, prompt: &str) -> Result<PromptAnalysis, String> {
    let _conn = open_optimization_db(workspace_path)
        .map_err(|e| format!("Failed to open database: {e}"))?;

    let word_count = prompt.split_whitespace().count() as i32;
    let char_count = prompt.chars().count() as i32;

    // Detect category based on keywords
    let category = detect_prompt_category(prompt);

    // Calculate specificity score
    let specificity_score = calculate_specificity_score(prompt);

    // Calculate structure score
    let structure_score = calculate_structure_score(prompt);

    // Identify issues
    let mut issues = Vec::new();

    // Check for length issues
    if word_count < 5 {
        issues.push(PromptIssue {
            issue_type: "too_short".to_string(),
            description: "Prompt is very short and may lack necessary context".to_string(),
            severity: "high".to_string(),
        });
    } else if word_count > 100 {
        issues.push(PromptIssue {
            issue_type: "too_long".to_string(),
            description: "Prompt is very long and may be difficult to follow".to_string(),
            severity: "medium".to_string(),
        });
    }

    // Check for vague language
    let vague_words = ["fix", "update", "change", "modify", "improve", "handle"];
    let has_vague_words = vague_words
        .iter()
        .any(|w| prompt.to_lowercase().contains(w));
    let has_specifics = prompt.contains('#')
        || prompt.contains("function")
        || prompt.contains("file")
        || prompt.contains("error")
        || prompt.contains("test");

    if has_vague_words && !has_specifics {
        issues.push(PromptIssue {
            issue_type: "vague".to_string(),
            description: "Prompt contains vague action words without specific targets".to_string(),
            severity: "medium".to_string(),
        });
    }

    // Check for missing issue reference
    let has_issue_ref = regex::Regex::new(r"#\d+")
        .map(|re| re.is_match(prompt))
        .unwrap_or(false);
    if !has_issue_ref && word_count > 10 {
        issues.push(PromptIssue {
            issue_type: "missing_issue_ref".to_string(),
            description: "Consider referencing an issue number for traceability".to_string(),
            severity: "low".to_string(),
        });
    }

    // Check for passive/request voice
    let passive_starts = ["can you", "could you", "please", "would you", "i need"];
    let is_passive = passive_starts
        .iter()
        .any(|s| prompt.to_lowercase().starts_with(s));
    if is_passive {
        issues.push(PromptIssue {
            issue_type: "passive_voice".to_string(),
            description: "Use imperative voice for clearer instructions".to_string(),
            severity: "low".to_string(),
        });
    }

    // Check for missing test mention
    let test_words = ["test", "spec", "verify", "check", "validate"];
    let mentions_testing = test_words.iter().any(|w| prompt.to_lowercase().contains(w));
    if !mentions_testing && category.as_deref() == Some("build") {
        issues.push(PromptIssue {
            issue_type: "missing_test_mention".to_string(),
            description: "Consider mentioning testing requirements".to_string(),
            severity: "low".to_string(),
        });
    }

    let needs_optimization = !issues.is_empty() || specificity_score < 0.5 || structure_score < 0.5;

    Ok(PromptAnalysis {
        prompt: prompt.to_string(),
        word_count,
        char_count,
        category,
        specificity_score,
        structure_score,
        issues,
        needs_optimization,
    })
}

/// Detect the category/intent of a prompt
fn detect_prompt_category(prompt: &str) -> Option<String> {
    let prompt_lower = prompt.to_lowercase();

    // Build/implement patterns
    if prompt_lower.contains("implement")
        || prompt_lower.contains("create")
        || prompt_lower.contains("add")
        || prompt_lower.contains("build")
    {
        return Some("build".to_string());
    }

    // Fix patterns
    if prompt_lower.contains("fix")
        || prompt_lower.contains("bug")
        || prompt_lower.contains("error")
        || prompt_lower.contains("issue")
    {
        return Some("fix".to_string());
    }

    // Refactor patterns
    if prompt_lower.contains("refactor")
        || prompt_lower.contains("restructure")
        || prompt_lower.contains("reorganize")
        || prompt_lower.contains("clean up")
    {
        return Some("refactor".to_string());
    }

    // Review patterns
    if prompt_lower.contains("review")
        || prompt_lower.contains("check")
        || prompt_lower.contains("analyze")
    {
        return Some("review".to_string());
    }

    // Curate patterns
    if prompt_lower.contains("curate")
        || prompt_lower.contains("enhance")
        || prompt_lower.contains("describe")
    {
        return Some("curate".to_string());
    }

    None
}

/// Calculate how specific a prompt is (0.0 to 1.0)
fn calculate_specificity_score(prompt: &str) -> f64 {
    let mut score = 0.5; // Start neutral

    // Positive indicators
    if regex::Regex::new(r"#\d+")
        .map(|re| re.is_match(prompt))
        .unwrap_or(false)
    {
        score += 0.15; // Has issue reference
    }

    if prompt.contains('/') || prompt.contains('.') {
        score += 0.10; // Likely has file paths
    }

    if prompt.contains("function")
        || prompt.contains("method")
        || prompt.contains("class")
        || prompt.contains("struct")
    {
        score += 0.10; // References code constructs
    }

    if prompt.contains("error") || prompt.contains("warning") {
        score += 0.05; // References specific problems
    }

    // Count specific technical words
    let technical_words = [
        "api",
        "database",
        "query",
        "request",
        "response",
        "endpoint",
        "module",
        "component",
        "interface",
        "type",
        "schema",
        "migration",
        "test",
        "spec",
    ];
    let technical_count = technical_words
        .iter()
        .filter(|w| prompt.to_lowercase().contains(*w))
        .count();
    score += (technical_count as f64 * 0.03).min(0.15);

    // Negative indicators
    let vague_words = [
        "something",
        "stuff",
        "things",
        "some",
        "maybe",
        "probably",
        "might",
    ];
    let vague_count = vague_words
        .iter()
        .filter(|w| prompt.to_lowercase().contains(*w))
        .count();
    score -= vague_count as f64 * 0.05;

    score.clamp(0.0, 1.0)
}

/// Calculate structural quality of a prompt (0.0 to 1.0)
fn calculate_structure_score(prompt: &str) -> f64 {
    let mut score = 0.5;

    let word_count = prompt.split_whitespace().count();

    // Optimal length (10-50 words)
    if (10..=50).contains(&word_count) {
        score += 0.15;
    } else if word_count < 5 || word_count > 100 {
        score -= 0.15;
    }

    // Starts with action verb
    let action_verbs = [
        "implement",
        "create",
        "add",
        "fix",
        "update",
        "refactor",
        "review",
        "build",
        "test",
        "remove",
        "delete",
        "modify",
        "change",
    ];
    let first_word = prompt
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_lowercase();
    if action_verbs.contains(&first_word.as_str()) {
        score += 0.15;
    }

    // Not a question
    if !prompt.ends_with('?') {
        score += 0.05;
    }

    // Has proper punctuation
    if prompt.ends_with('.') || prompt.ends_with('!') {
        score += 0.05;
    }

    // Uses bullet points or structured format
    if prompt.contains("- ") || prompt.contains("* ") || prompt.contains("1.") {
        score += 0.10;
    }

    score.clamp(0.0, 1.0)
}

// ============================================================================
// Optimization Suggestion Functions
// ============================================================================

/// Generate optimization suggestions for a prompt
#[tauri::command]
pub fn generate_optimization_suggestions(
    workspace_path: &str,
    prompt: &str,
    max_suggestions: Option<i32>,
) -> Result<Vec<OptimizationSuggestion>, String> {
    let conn = open_optimization_db(workspace_path)
        .map_err(|e| format!("Failed to open database: {e}"))?;

    let max_suggestions = max_suggestions.unwrap_or(3);
    let mut suggestions = Vec::new();

    // Analyze the prompt first
    let analysis = analyze_prompt(workspace_path, prompt)?;

    // Apply rules based on analysis
    let rules: Vec<(i64, String, String, String, String, f64)> = conn
        .prepare(
            "SELECT id, name, rule_type, condition, suggestion_template, expected_improvement
             FROM optimization_rules
             WHERE active = 1
             ORDER BY expected_improvement DESC",
        )
        .map_err(|e| format!("Failed to prepare query: {e}"))?
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?))
        })
        .map_err(|e| format!("Failed to query rules: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect rules: {e}"))?;

    for (_rule_id, rule_name, rule_type, condition, template, expected_improvement) in rules {
        if suggestions.len() >= max_suggestions as usize {
            break;
        }

        if let Some(suggestion) = apply_rule(
            &analysis,
            &rule_name,
            &rule_type,
            &condition,
            &template,
            expected_improvement,
        ) {
            suggestions.push(suggestion);
        }
    }

    // Try to find matching successful patterns
    if suggestions.len() < max_suggestions as usize {
        if let Some(pattern_suggestion) = find_pattern_based_suggestion(&conn, prompt, &analysis) {
            suggestions.push(pattern_suggestion);
        }
    }

    // Sort by confidence
    suggestions.sort_by(|a, b| b.confidence.partial_cmp(&a.confidence).unwrap());

    // Store suggestions in database
    for suggestion in &suggestions {
        conn.execute(
            "INSERT INTO optimization_suggestions (original_prompt, optimized_prompt, optimization_type, reasoning, confidence, matched_pattern_id, expected_improvement)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                suggestion.original_prompt,
                suggestion.optimized_prompt,
                suggestion.optimization_type,
                suggestion.reasoning,
                suggestion.confidence,
                suggestion.matched_pattern_id,
                suggestion.expected_improvement
            ],
        )
        .map_err(|e| format!("Failed to store suggestion: {e}"))?;
    }

    Ok(suggestions)
}

/// Apply a single optimization rule to a prompt
fn apply_rule(
    analysis: &PromptAnalysis,
    rule_name: &str,
    rule_type: &str,
    condition: &str,
    template: &str,
    expected_improvement: f64,
) -> Option<OptimizationSuggestion> {
    let prompt = &analysis.prompt;

    // Parse condition as JSON
    let condition_json: serde_json::Value = serde_json::from_str(condition).ok()?;

    // Check if rule applies based on type
    let applies = match rule_type {
        "length" => {
            let min_words = condition_json.get("min_words")?.as_i64()? as i32;
            let max_words = condition_json.get("max_words")?.as_i64()? as i32;
            analysis.word_count >= min_words && analysis.word_count <= max_words
        }
        "specificity" => {
            // Check for keywords that indicate vagueness
            if let Some(keywords) = condition_json.get("keywords").and_then(|k| k.as_array()) {
                let has_keywords = keywords
                    .iter()
                    .filter_map(|k| k.as_str())
                    .any(|k| prompt.to_lowercase().contains(k));

                let lacks = condition_json
                    .get("lacks")
                    .and_then(|l| l.as_array())
                    .map(|arr| {
                        !arr.iter()
                            .filter_map(|k| k.as_str())
                            .any(|k| prompt.to_lowercase().contains(k))
                    })
                    .unwrap_or(true);

                has_keywords && lacks
            } else {
                false
            }
        }
        "structure" => {
            // Check structural issues
            if let Some(starts_with) = condition_json.get("starts_with").and_then(|s| s.as_array())
            {
                starts_with
                    .iter()
                    .filter_map(|s| s.as_str())
                    .any(|s| prompt.to_lowercase().starts_with(s))
            } else if let Some(lacks) = condition_json.get("lacks").and_then(|l| l.as_array()) {
                !lacks
                    .iter()
                    .filter_map(|l| l.as_str())
                    .any(|l| prompt.to_lowercase().contains(l))
            } else if let Some(pattern) =
                condition_json.get("lacks_pattern").and_then(|p| p.as_str())
            {
                regex::Regex::new(pattern)
                    .map(|re| !re.is_match(prompt))
                    .unwrap_or(false)
            } else {
                false
            }
        }
        _ => false,
    };

    if !applies {
        return None;
    }

    // Generate optimized prompt from template
    let optimized = generate_optimized_prompt(prompt, template, analysis);

    // Calculate confidence based on rule success rate and analysis
    let base_confidence = 0.5 + (expected_improvement * 2.0);
    let confidence = base_confidence.min(0.9);

    Some(OptimizationSuggestion {
        id: None,
        original_prompt: prompt.to_string(),
        optimized_prompt: optimized.clone(),
        optimization_type: rule_type.to_string(),
        reasoning: format!(
            "Rule '{}' suggests: {}",
            rule_name,
            get_reasoning_for_rule(rule_type, analysis)
        ),
        confidence,
        matched_pattern_id: None,
        expected_improvement,
        accepted: None,
        outcome: None,
        created_at: None,
    })
}

/// Generate an optimized prompt from a template
fn generate_optimized_prompt(original: &str, template: &str, analysis: &PromptAnalysis) -> String {
    let mut result = template.to_string();

    // Replace placeholders
    result = result.replace("{{original}}", original);

    // Create condensed version (first 50 words)
    let condensed: String = original
        .split_whitespace()
        .take(50)
        .collect::<Vec<_>>()
        .join(" ");
    result = result.replace("{{original_condensed}}", &condensed);

    // Convert to action form (remove passive starters)
    let action_form = convert_to_action_form(original);
    result = result.replace("{{action_form}}", &action_form);

    // Suggest scope based on category
    let scope = match analysis.category.as_deref() {
        Some("build") => "the specific file or module",
        Some("fix") => "the error message or failing test",
        Some("refactor") => "the target function or class",
        _ => "the affected component",
    };
    result = result.replace("{{suggested_scope}}", scope);

    result
}

/// Convert a prompt to imperative/action form
fn convert_to_action_form(prompt: &str) -> String {
    let prompt_lower = prompt.to_lowercase();

    // Common passive patterns to action verbs
    let replacements = [
        ("can you ", ""),
        ("could you ", ""),
        ("please ", ""),
        ("would you ", ""),
        ("i need you to ", ""),
        ("i want you to ", ""),
        ("i'd like you to ", ""),
    ];

    let mut result = prompt.to_string();
    for (pattern, replacement) in replacements {
        if prompt_lower.starts_with(pattern) {
            result = result[pattern.len()..].to_string();
            if !replacement.is_empty() {
                result = format!("{replacement}{result}");
            }
            break;
        }
    }

    // Capitalize first letter
    if let Some(first_char) = result.chars().next() {
        result = format!("{}{}", first_char.to_uppercase(), &result[first_char.len_utf8()..]);
    }

    result
}

/// Get reasoning text for a rule type
fn get_reasoning_for_rule(rule_type: &str, analysis: &PromptAnalysis) -> String {
    match rule_type {
        "length" => {
            if analysis.word_count < 5 {
                format!(
                    "Your prompt has only {} words. Adding context improves success rates by 15-20%.",
                    analysis.word_count
                )
            } else {
                format!(
                    "Your prompt has {} words. Concise prompts (10-50 words) tend to perform better.",
                    analysis.word_count
                )
            }
        }
        "specificity" => {
            format!(
                "Specificity score: {:.0}%. Adding concrete references (issue numbers, file paths, function names) can improve success by 20%.",
                analysis.specificity_score * 100.0
            )
        }
        "structure" => {
            format!(
                "Structure score: {:.0}%. Using imperative voice and clear action verbs improves clarity.",
                analysis.structure_score * 100.0
            )
        }
        "pattern" => {
            "This optimization is based on historically successful prompts with similar intent."
                .to_string()
        }
        _ => "This optimization is based on best practices analysis.".to_string(),
    }
}

/// Find a pattern-based optimization suggestion
fn find_pattern_based_suggestion(
    conn: &Connection,
    prompt: &str,
    analysis: &PromptAnalysis,
) -> Option<OptimizationSuggestion> {
    // Normalize prompt for matching
    let normalized = normalize_prompt(prompt);

    // Find similar successful patterns
    let words: Vec<&str> = normalized
        .split_whitespace()
        .filter(|w| w.len() >= 3)
        .take(5)
        .collect();

    if words.is_empty() {
        return None;
    }

    // Build LIKE conditions
    let like_conditions: Vec<String> = words
        .iter()
        .map(|w| format!("pattern_text LIKE '%{w}%'"))
        .collect();
    let where_clause = like_conditions.join(" OR ");

    let query = format!(
        "SELECT id, pattern_text, success_rate, times_used, category
         FROM prompt_patterns
         WHERE ({where_clause})
           AND success_rate >= 0.7
           AND times_used >= 3
         ORDER BY success_rate DESC, times_used DESC
         LIMIT 1"
    );

    let pattern: Option<(i64, String, f64, i32, Option<String>)> = conn
        .query_row(&query, [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
        })
        .ok();

    if let Some((pattern_id, pattern_text, success_rate, times_used, _category)) = pattern {
        let improvement = (success_rate - 0.5).max(0.1);

        Some(OptimizationSuggestion {
            id: None,
            original_prompt: prompt.to_string(),
            optimized_prompt: pattern_text.clone(),
            optimization_type: "pattern".to_string(),
            reasoning: format!(
                "This pattern has been used {} times with a {:.0}% success rate. Adapting your prompt to this proven structure can improve outcomes.",
                times_used,
                success_rate * 100.0
            ),
            confidence: success_rate * 0.9,
            matched_pattern_id: Some(pattern_id),
            expected_improvement: improvement,
            accepted: None,
            outcome: None,
            created_at: None,
        })
    } else {
        None
    }
}

/// Normalize a prompt for pattern matching
fn normalize_prompt(prompt: &str) -> String {
    // Replace issue/PR numbers with placeholder
    let pattern = regex::Regex::new(r"#\d+")
        .map(|re| re.replace_all(prompt, "#N").to_string())
        .unwrap_or_else(|_| prompt.to_string());

    // Normalize whitespace
    let pattern = regex::Regex::new(r"\s+")
        .map(|re| re.replace_all(&pattern, " ").to_string())
        .unwrap_or(pattern);

    pattern.trim().to_lowercase()
}

// ============================================================================
// Suggestion Tracking Functions
// ============================================================================

/// Record that a suggestion was accepted or rejected
#[tauri::command]
pub fn record_suggestion_decision(
    workspace_path: String,
    suggestion_id: i64,
    accepted: bool,
) -> Result<(), String> {
    let conn = open_optimization_db(&workspace_path)
        .map_err(|e| format!("Failed to open database: {e}"))?;

    conn.execute(
        "UPDATE optimization_suggestions
         SET accepted = ?1, accepted_at = datetime('now')
         WHERE id = ?2",
        params![accepted, suggestion_id],
    )
    .map_err(|e| format!("Failed to record decision: {e}"))?;

    // Update rule statistics if this was based on a rule
    if let Some(optimization_type) = get_suggestion_type(&conn, suggestion_id) {
        update_rule_stats(&conn, &optimization_type, accepted)?;
    }

    Ok(())
}

/// Record the outcome of an accepted suggestion
#[tauri::command]
pub fn record_suggestion_outcome(
    workspace_path: String,
    suggestion_id: i64,
    outcome: String,
) -> Result<(), String> {
    let conn = open_optimization_db(&workspace_path)
        .map_err(|e| format!("Failed to open database: {e}"))?;

    conn.execute(
        "UPDATE optimization_suggestions
         SET outcome = ?1, outcome_recorded_at = datetime('now')
         WHERE id = ?2",
        params![outcome, suggestion_id],
    )
    .map_err(|e| format!("Failed to record outcome: {e}"))?;

    Ok(())
}

/// Get the optimization type for a suggestion
fn get_suggestion_type(conn: &Connection, suggestion_id: i64) -> Option<String> {
    conn.query_row(
        "SELECT optimization_type FROM optimization_suggestions WHERE id = ?1",
        [suggestion_id],
        |row| row.get(0),
    )
    .ok()
}

/// Update rule statistics based on suggestion acceptance
fn update_rule_stats(
    conn: &Connection,
    optimization_type: &str,
    accepted: bool,
) -> Result<(), String> {
    // Find rules of this type
    let rules: Vec<i64> = conn
        .prepare("SELECT id FROM optimization_rules WHERE rule_type = ?1")
        .map_err(|e| format!("Failed to prepare query: {e}"))?
        .query_map([optimization_type], |row| row.get(0))
        .map_err(|e| format!("Failed to query rules: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect rules: {e}"))?;

    for rule_id in rules {
        if accepted {
            conn.execute(
                "UPDATE optimization_rules
                 SET times_applied = times_applied + 1,
                     success_count = success_count + 1,
                     success_rate = CAST(success_count + 1 AS REAL) / (times_applied + 1),
                     updated_at = datetime('now')
                 WHERE id = ?1",
                [rule_id],
            )
            .map_err(|e| format!("Failed to update rule: {e}"))?;
        } else {
            conn.execute(
                "UPDATE optimization_rules
                 SET times_applied = times_applied + 1,
                     failure_count = failure_count + 1,
                     success_rate = CAST(success_count AS REAL) / (times_applied + 1),
                     updated_at = datetime('now')
                 WHERE id = ?1",
                [rule_id],
            )
            .map_err(|e| format!("Failed to update rule: {e}"))?;
        }
    }

    Ok(())
}

// ============================================================================
// Statistics and Query Functions
// ============================================================================

/// Get optimization statistics
#[tauri::command]
pub fn get_optimization_stats(workspace_path: &str) -> Result<OptimizationStats, String> {
    let conn = open_optimization_db(workspace_path)
        .map_err(|e| format!("Failed to open database: {e}"))?;

    // Total suggestions
    let total_suggestions: i32 = conn
        .query_row("SELECT COUNT(*) FROM optimization_suggestions", [], |row| row.get(0))
        .unwrap_or(0);

    // Accepted suggestions
    let accepted_suggestions: i32 = conn
        .query_row("SELECT COUNT(*) FROM optimization_suggestions WHERE accepted = 1", [], |row| {
            row.get(0)
        })
        .unwrap_or(0);

    // Rejected suggestions
    let rejected_suggestions: i32 = conn
        .query_row("SELECT COUNT(*) FROM optimization_suggestions WHERE accepted = 0", [], |row| {
            row.get(0)
        })
        .unwrap_or(0);

    // Pending suggestions
    let pending_suggestions: i32 = conn
        .query_row(
            "SELECT COUNT(*) FROM optimization_suggestions WHERE accepted IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);

    // Acceptance rate
    let decided = accepted_suggestions + rejected_suggestions;
    let acceptance_rate = if decided > 0 {
        accepted_suggestions as f64 / decided as f64
    } else {
        0.0
    };

    // Average improvement when accepted
    let avg_improvement_when_accepted: f64 = conn
        .query_row(
            "SELECT COALESCE(AVG(expected_improvement), 0.0)
             FROM optimization_suggestions
             WHERE accepted = 1",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0.0);

    // Stats by type
    let mut stmt = conn
        .prepare(
            "SELECT optimization_type,
                    COUNT(*),
                    CAST(SUM(CASE WHEN accepted = 1 THEN 1 ELSE 0 END) AS REAL) /
                    NULLIF(SUM(CASE WHEN accepted IS NOT NULL THEN 1 ELSE 0 END), 0),
                    AVG(expected_improvement)
             FROM optimization_suggestions
             GROUP BY optimization_type",
        )
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let suggestions_by_type: Vec<OptimizationTypeStats> = stmt
        .query_map([], |row| {
            Ok(OptimizationTypeStats {
                optimization_type: row.get(0)?,
                count: row.get(1)?,
                acceptance_rate: row.get::<_, Option<f64>>(2)?.unwrap_or(0.0),
                avg_improvement: row.get::<_, Option<f64>>(3)?.unwrap_or(0.0),
            })
        })
        .map_err(|e| format!("Failed to query types: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect types: {e}"))?;

    Ok(OptimizationStats {
        total_suggestions,
        accepted_suggestions,
        rejected_suggestions,
        pending_suggestions,
        acceptance_rate,
        avg_improvement_when_accepted,
        suggestions_by_type,
    })
}

/// Get recent optimization suggestions
#[tauri::command]
pub fn get_recent_suggestions(
    workspace_path: &str,
    limit: Option<i32>,
) -> Result<Vec<OptimizationSuggestion>, String> {
    let conn = open_optimization_db(workspace_path)
        .map_err(|e| format!("Failed to open database: {e}"))?;

    let limit = limit.unwrap_or(10);

    let mut stmt = conn
        .prepare(
            "SELECT id, original_prompt, optimized_prompt, optimization_type, reasoning,
                    confidence, matched_pattern_id, expected_improvement, accepted, outcome, created_at
             FROM optimization_suggestions
             ORDER BY created_at DESC
             LIMIT ?1",
        )
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let suggestions = stmt
        .query_map([limit], |row| {
            Ok(OptimizationSuggestion {
                id: row.get(0)?,
                original_prompt: row.get(1)?,
                optimized_prompt: row.get(2)?,
                optimization_type: row.get(3)?,
                reasoning: row.get(4)?,
                confidence: row.get(5)?,
                matched_pattern_id: row.get(6)?,
                expected_improvement: row.get(7)?,
                accepted: row.get::<_, Option<i32>>(8)?.map(|i| i != 0),
                outcome: row.get(9)?,
                created_at: row.get(10)?,
            })
        })
        .map_err(|e| format!("Failed to query suggestions: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect suggestions: {e}"))?;

    Ok(suggestions)
}

/// Get all active optimization rules
#[tauri::command]
pub fn get_optimization_rules(workspace_path: &str) -> Result<Vec<OptimizationRule>, String> {
    let conn = open_optimization_db(workspace_path)
        .map_err(|e| format!("Failed to open database: {e}"))?;

    let mut stmt = conn
        .prepare(
            "SELECT id, name, rule_type, condition, suggestion_template, expected_improvement,
                    active, times_applied, success_rate
             FROM optimization_rules
             ORDER BY expected_improvement DESC",
        )
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let rules = stmt
        .query_map([], |row| {
            Ok(OptimizationRule {
                id: row.get(0)?,
                name: row.get(1)?,
                rule_type: row.get(2)?,
                condition: row.get(3)?,
                suggestion_template: row.get(4)?,
                expected_improvement: row.get(5)?,
                active: row.get::<_, i32>(6)? != 0,
                times_applied: row.get(7)?,
                success_rate: row.get(8)?,
            })
        })
        .map_err(|e| format!("Failed to query rules: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect rules: {e}"))?;

    Ok(rules)
}

/// Toggle a rule's active status
#[tauri::command]
pub fn toggle_optimization_rule(
    workspace_path: String,
    rule_id: i64,
    active: bool,
) -> Result<(), String> {
    let conn = open_optimization_db(&workspace_path)
        .map_err(|e| format!("Failed to open database: {e}"))?;

    conn.execute(
        "UPDATE optimization_rules SET active = ?1, updated_at = datetime('now') WHERE id = ?2",
        params![active, rule_id],
    )
    .map_err(|e| format!("Failed to toggle rule: {e}"))?;

    Ok(())
}

/// Refine optimization rules based on outcomes (learning loop)
#[tauri::command]
pub fn refine_optimization_rules(workspace_path: &str) -> Result<i32, String> {
    let conn = open_optimization_db(workspace_path)
        .map_err(|e| format!("Failed to open database: {e}"))?;

    let mut rules_updated = 0;

    // Get rules with enough data to refine
    let rules: Vec<(i64, i32, f64)> = conn
        .prepare(
            "SELECT id, times_applied, success_rate
             FROM optimization_rules
             WHERE times_applied >= 10",
        )
        .map_err(|e| format!("Failed to prepare query: {e}"))?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .map_err(|e| format!("Failed to query rules: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect rules: {e}"))?;

    for (rule_id, times_applied, success_rate) in rules {
        // Deactivate rules with very low acceptance rates
        if success_rate < 0.2 && times_applied >= 20 {
            conn.execute(
                "UPDATE optimization_rules SET active = 0, updated_at = datetime('now') WHERE id = ?1",
                [rule_id],
            )
            .map_err(|e| format!("Failed to deactivate rule: {e}"))?;
            rules_updated += 1;
        }

        // Adjust expected_improvement based on actual outcomes
        let actual_improvement: f64 = conn
            .query_row(
                "SELECT AVG(os.expected_improvement)
                 FROM optimization_suggestions os
                 WHERE os.optimization_type = (SELECT rule_type FROM optimization_rules WHERE id = ?1)
                   AND os.accepted = 1
                   AND os.outcome = 'success'",
                [rule_id],
                |row| row.get(0),
            )
            .unwrap_or(0.0);

        if actual_improvement > 0.0 {
            conn.execute(
                "UPDATE optimization_rules
                 SET expected_improvement = ?1, updated_at = datetime('now')
                 WHERE id = ?2",
                params![actual_improvement, rule_id],
            )
            .map_err(|e| format!("Failed to update expected improvement: {e}"))?;
            rules_updated += 1;
        }
    }

    Ok(rules_updated)
}
