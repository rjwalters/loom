# Issue #178: Prompt Library & Template System

## Vision
Enable users and agents to build a library of effective prompts that can be reused, composed, and improved over time. Transform prompt engineering from an art into a systematic, data-driven practice.

## Problem Statement

**Current Pain Points:**
- Users type similar prompts repeatedly (e.g., "review this PR", "fix linting errors", "implement feature X")
- Agents can't learn from successful prompt patterns across sessions
- No way to share effective prompts between agents, users, or team members
- Complex multi-step workflows require retyping entire sequences
- Institutional knowledge about what prompts work well is lost over time
- No visibility into which prompts lead to successful outcomes

**User's Vision:**
> "In the IDE of the future, the main work of a programmer is going to be staging sequences of requests as prompts."

This requires:
1. A library of proven, effective prompts
2. Ability to compose prompts into sequences
3. Variables and customization for different contexts
4. Learning from outcomes to improve prompts over time

## Solution Overview

Build a comprehensive prompt library system that:
1. **Captures** successful prompts from activity database (auto-discovery)
2. **Stores** templates with metadata and variables
3. **Retrieves** prompts via quick-access UI (keyboard shortcuts, search)
4. **Composes** multi-step sequences for complex workflows
5. **Tracks** effectiveness using activity database outcomes
6. **Shares** templates across agents, users, and teams
7. **Learns** which prompts work best in different contexts

## Key Features

### 1. Template Storage & Format

**File Location:** `.loom/prompts/templates/`

**Template File Format:** Markdown with YAML frontmatter

```markdown
---
id: review-pr-thorough
name: Thorough PR Review
description: Comprehensive code review checking style, tests, docs, and functionality
tags: [review, pr, quality]
role: reviewer
variables:
  - name: pr_number
    type: pr
    required: true
    description: Pull request number to review
  - name: focus_area
    type: text
    required: false
    default: "all aspects"
    description: Specific area to focus on (optional)
---

# PR Review Request

Please perform a thorough code review of PR #{{pr_number}}.

## Review Checklist
- [ ] Code style and formatting
- [ ] Test coverage
- [ ] Documentation updates
- [ ] Breaking changes noted
- [ ] Performance implications
- [ ] Security considerations

Focus area: {{focus_area}}

## Output Format
Provide your review as structured comments with:
1. Summary of changes
2. Issues found (if any) with severity
3. Suggestions for improvement
4. Final recommendation (approve/request changes)
```

**Metadata Tracking:**
```json
{
  "id": "review-pr-thorough",
  "created": "2025-10-15T10:30:00Z",
  "updated": "2025-10-15T10:30:00Z",
  "usage_count": 47,
  "success_rate": 0.89,
  "avg_completion_time_seconds": 127,
  "last_used": "2025-10-15T14:22:00Z",
  "created_by": "manual",
  "effectiveness_score": 4.2
}
```

### 2. Variable System

**Supported Variable Types:**
- `text` - Free-form text input
- `file` - File path with autocomplete
- `branch` - Git branch with autocomplete
- `issue` - GitHub issue number
- `pr` - GitHub PR number
- `code_block` - Multi-line code with syntax highlighting
- `agent` - Agent terminal ID

**Automatic Context Variables:**
- `{{workspace}}` - Current workspace path
- `{{branch}}` - Current git branch
- `{{agent_role}}` - Current agent role
- `{{timestamp}}` - Current timestamp
- `{{last_commit}}` - Last commit hash
- `{{modified_files}}` - List of modified files

### 3. Template Discovery & Auto-Suggestion

**From Activity Database:**
- Analyze successful prompts (high completion rate, low error rate)
- Identify common patterns (e.g., 85% of reviews start with "Please review PR #...")
- Suggest templatizing frequently-used prompts
- Cluster similar prompts to avoid duplicates

**Discovery UI:**
- Show "Create template from this prompt" button in activity timeline
- Highlight prompts with high reuse potential
- One-click template creation from activity history
- Auto-populate variables based on pattern analysis

### 4. Quick Access UI

**Command Palette (Cmd+K or Cmd+Shift+P):**
```
┌─ Prompt Templates ────────────────────────────┐
│ > review pr                                   │
├───────────────────────────────────────────────┤
│ 🔍 Review PR (Thorough)            [reviewer] │
│    Comprehensive code review...               │
│                                               │
│ 🔍 Review PR (Quick)               [reviewer] │
│    Fast style and test check...               │
│                                               │
│ 🐛 Fix Linting Errors              [worker]   │
│    Auto-fix common linting...                 │
│                                               │
│ ✨ Implement Feature                [worker]   │
│    Full feature workflow...                   │
└───────────────────────────────────────────────┘
```

**Features:**
- Fuzzy search by name, tags, or description
- Filter by role (show only relevant templates)
- Recently used templates at top
- Keyboard navigation (arrows + Enter)
- Preview on hover
- Show usage count and success rate

### 5. Template Editor UI

**Visual Editor Modal:**
```
┌─ Edit Template ───────────────────────────────┐
│ Name: [Thorough PR Review              ]     │
│ Description: [Comprehensive code review...  ]│
│ Tags: [review] [pr] [quality]                │
│ Role: [reviewer ▼]                           │
│                                               │
│ Variables:                                    │
│ ┌─────────────────────────────────────────┐ │
│ │ pr_number (pr) - required               │ │
│ │ focus_area (text) - optional            │ │
│ │ + Add Variable                          │ │
│ └─────────────────────────────────────────┘ │
│                                               │
│ Content:                                      │
│ ┌─────────────────────────────────────────┐ │
│ │ Please perform a thorough code review  │ │
│ │ of PR #{{pr_number}}.                  │ │
│ │                                         │ │
│ │ Focus area: {{focus_area}}             │ │
│ └─────────────────────────────────────────┘ │
│                                               │
│ [Preview] [Save] [Cancel]                    │
└───────────────────────────────────────────────┘
```

### 6. Multi-Step Sequences

**Sequence Definition:** `.loom/prompts/sequences/full-feature-workflow.json`

```json
{
  "id": "full-feature-workflow",
  "name": "Full Feature Implementation",
  "description": "Complete workflow from issue to merged PR",
  "steps": [
    {
      "name": "Claim Issue",
      "template": "claim-issue",
      "agent_role": "worker",
      "variables": {
        "issue_number": "{{issue_number}}"
      }
    },
    {
      "name": "Implement Feature",
      "template": "implement-feature",
      "agent_role": "worker",
      "wait_for_completion": true,
      "variables": {
        "issue_number": "{{issue_number}}",
        "feature_description": "{{feature_description}}"
      }
    },
    {
      "name": "Run Tests",
      "template": "run-full-test-suite",
      "agent_role": "worker",
      "conditions": [
        {"type": "previous_success", "required": true}
      ]
    },
    {
      "name": "Create PR",
      "template": "create-pr",
      "agent_role": "worker",
      "variables": {
        "issue_number": "{{issue_number}}",
        "branch": "{{branch}}"
      }
    },
    {
      "name": "Request Review",
      "template": "request-review",
      "agent_role": "worker",
      "variables": {
        "pr_number": "{{pr_number}}"
      }
    }
  ],
  "error_handling": {
    "on_step_failure": "pause_and_notify",
    "retry_count": 2,
    "retry_delay_seconds": 30
  }
}
```

**Sequence Execution UI:**
```
┌─ Running: Full Feature Implementation ───────┐
│                                               │
│ [✓] Claim Issue #142                         │
│ [✓] Implement Feature                        │
│ [→] Run Tests                     [in progress]│
│ [ ] Create PR                                │
│ [ ] Request Review                           │
│                                               │
│ Step 3 of 5 - Estimated 2m 30s remaining     │
│                                               │
│ [Pause] [Cancel] [Skip Step]                 │
└───────────────────────────────────────────────┘
```

### 7. Effectiveness Tracking

**Link Templates to Outcomes:**
```sql
-- Extend activity database with template tracking
ALTER TABLE agent_inputs ADD COLUMN template_id TEXT;
ALTER TABLE agent_inputs ADD COLUMN template_variables TEXT; -- JSON

-- Query template effectiveness
SELECT
  template_id,
  COUNT(*) as usage_count,
  AVG(CASE WHEN outputs.exit_code = 0 THEN 1 ELSE 0 END) as success_rate,
  AVG(outputs.timestamp - inputs.timestamp) as avg_completion_time
FROM agent_inputs inputs
LEFT JOIN agent_outputs outputs ON outputs.input_id = inputs.id
WHERE template_id IS NOT NULL
GROUP BY template_id;
```

**Metrics Dashboard:**
- Success rate per template
- Average completion time
- Most frequently used templates
- Templates with low success rate (candidates for improvement)
- Template usage trends over time
- Per-role template effectiveness

### 8. Agent Integration

**Autonomous Mode:**
Agents can reference templates in their role definitions:

```markdown
<!-- .loom/roles/builder.md -->
# Builder Bot

You are a development builder for the {{workspace}} repository.

## Available Templates
You have access to these prompt templates via the syntax `@template:template-id`:

- `@template:implement-feature` - Full feature implementation workflow
- `@template:fix-bug` - Bug fix with test coverage
- `@template:refactor-code` - Code refactoring with safety checks
- `@template:write-tests` - Test suite creation

## Example Usage
When you claim an issue labeled `loom:ready`, use:
```
@template:implement-feature issue_number={{issue_number}}
```
```

**Template Invocation:**
Agents can invoke templates programmatically:
```typescript
// In agent-launcher.ts
if (input.startsWith('@template:')) {
  const [_, templateId, ...variablePairs] = input.split(' ');
  const variables = parseVariables(variablePairs);
  const prompt = await renderTemplate(templateId, variables);
  sendToTerminal(terminalId, prompt);
}
```

### 9. Sharing & Collaboration

**Export Template Pack:**
```bash
# Export all templates
loom export-templates --output ./my-templates.zip

# Export specific tags
loom export-templates --tags review,pr --output ./review-templates.zip
```

**Import Template Pack:**
```bash
# Import from file
loom import-templates --file ./downloaded-templates.zip

# Import from URL
loom import-templates --url https://templates.loom.dev/best-practices.zip
```

**Community Template Gallery:**
- Website: `templates.loom.dev`
- Browse by category, role, popularity
- Rate and review templates
- One-click install
- Update notifications for installed packs

**Team Templates:**
- Store in git repository: `.loom/prompts/team/`
- Version controlled with project
- Everyone gets same templates on clone
- Customizable per developer in `.loom/prompts/local/`

## Implementation Phases

### Phase 1: Core Template System (Week 1-2)
**Goal:** Basic template storage, editing, and insertion

**Tasks:**
1. Create `.loom/prompts/` directory structure
2. Implement template file format (Markdown + YAML frontmatter)
3. Build template parser (extract variables, metadata)
4. Create template editor modal UI
5. Add "Insert Template" button to terminal input
6. Implement variable substitution engine
7. Basic template list view

**Deliverables:**
- Users can create, edit, and use templates manually
- Simple list view for browsing templates
- Basic variable substitution

### Phase 2: Quick Access & Discovery (Week 3-4)
**Goal:** Make templates easy to find and use

**Tasks:**
1. Build command palette UI (Cmd+K)
2. Implement fuzzy search
3. Add recently used templates
4. Create template preview on hover
5. Add keyboard shortcuts for top 10 templates
6. Implement role filtering (show relevant templates)
7. Add tags and categorization

**Deliverables:**
- Fast keyboard-driven template access
- Smart filtering and search
- Contextual template suggestions

### Phase 3: Activity Database Integration (Week 5-6)
**Goal:** Learn from prompt outcomes

**Tasks:**
1. Extend activity database schema for templates
2. Link template usage to outcomes
3. Calculate effectiveness metrics
4. Build template analytics dashboard
5. Implement "Create template from prompt" feature
6. Auto-suggest templatizing frequent prompts
7. Show effectiveness scores in template picker

**Deliverables:**
- Templates tracked in activity database
- Metrics on usage and success rates
- Auto-discovery of templateable prompts

### Phase 4: Sequences & Composition (Week 7-8)
**Goal:** Multi-step workflows

**Tasks:**
1. Define sequence file format (JSON)
2. Build sequence executor engine
3. Create sequence editor UI
4. Implement conditional logic (if/else)
5. Add error handling and retry logic
6. Build sequence progress UI
7. Allow sequences to span multiple agents

**Deliverables:**
- Multi-step sequences work end-to-end
- Visual sequence editor
- Robust error handling

### Phase 5: Agent Integration (Week 9-10)
**Goal:** Agents can use templates autonomously

**Tasks:**
1. Add `@template:` syntax to agent input parser
2. Allow templates in role definitions
3. Implement automatic variable population from context
4. Add template suggestions to agent prompts
5. Create agent-specific template libraries
6. Build template recommendation engine

**Deliverables:**
- Agents can invoke templates programmatically
- Context-aware variable substitution
- Agent-specific template recommendations

### Phase 6: Sharing & Community (Week 11-12)
**Goal:** Template sharing and collaboration

**Tasks:**
1. Implement export/import functionality
2. Build template gallery website
3. Add rating and review system
4. Create team template mechanism (git-based)
5. Add update notifications
6. Build template discovery API

**Deliverables:**
- Full import/export workflow
- Community template gallery
- Team collaboration features

## Technical Design

### TypeScript Interfaces

```typescript
// src/lib/template-types.ts

export interface PromptTemplate {
  id: string;
  name: string;
  description: string;
  content: string; // Markdown with {{variables}}
  tags: string[];
  role?: string; // Optional role specialization
  variables: TemplateVariable[];
  metadata: TemplateMetadata;
}

export interface TemplateVariable {
  name: string;
  type: 'text' | 'file' | 'branch' | 'issue' | 'pr' | 'code_block' | 'agent';
  required: boolean;
  default?: string;
  description?: string;
  validation?: RegExp;
}

export interface TemplateMetadata {
  created: Date;
  updated: Date;
  usageCount: number;
  successRate: number; // 0.0 to 1.0
  avgCompletionTimeSeconds: number;
  lastUsed?: Date;
  createdBy: 'manual' | 'auto-discovered' | 'imported';
  effectivenessScore: number; // 0 to 5
  version: number;
}

export interface PromptSequence {
  id: string;
  name: string;
  description: string;
  steps: SequenceStep[];
  errorHandling: ErrorHandlingStrategy;
  metadata: SequenceMetadata;
}

export interface SequenceStep {
  name: string;
  templateId: string;
  agentRole?: string;
  variables: Record<string, string>;
  waitForCompletion: boolean;
  conditions?: StepCondition[];
  timeout?: number;
}

export interface StepCondition {
  type: 'previous_success' | 'variable_set' | 'file_exists' | 'git_status';
  required: boolean;
  value?: unknown;
}

export interface ErrorHandlingStrategy {
  onStepFailure: 'stop' | 'continue' | 'pause_and_notify' | 'retry';
  retryCount: number;
  retryDelaySeconds: number;
  fallbackTemplate?: string;
}

export interface SequenceMetadata {
  created: Date;
  updated: Date;
  runCount: number;
  successRate: number;
  avgTotalTimeSeconds: number;
}
```

### Template Manager Service

```typescript
// src/lib/template-manager.ts

export class TemplateManager {
  private templatesDir: string;
  private cache: Map<string, PromptTemplate> = new Map();

  constructor(workspacePath: string) {
    this.templatesDir = path.join(workspacePath, '.loom', 'prompts', 'templates');
  }

  async loadTemplate(id: string): Promise<PromptTemplate> {
    // Load from cache or file
  }

  async saveTemplate(template: PromptTemplate): Promise<void> {
    // Save to file, update cache
  }

  async listTemplates(options?: {
    role?: string;
    tags?: string[];
    sortBy?: 'usage' | 'effectiveness' | 'recent';
  }): Promise<PromptTemplate[]> {
    // Query templates with filtering
  }

  async renderTemplate(
    templateId: string,
    variables: Record<string, string>,
    context?: RenderContext
  ): Promise<string> {
    // Load template, substitute variables, return final prompt
  }

  async suggestTemplates(prompt: string): Promise<PromptTemplate[]> {
    // Find similar templates using fuzzy matching
  }

  async analyzeEffectiveness(templateId: string): Promise<TemplateMetrics> {
    // Query activity database for template outcomes
  }

  async discoverTemplateOpportunities(): Promise<TemplateCandidate[]> {
    // Analyze activity database for frequently-used prompts
  }
}
```

### Variable Substitution Engine

```typescript
// src/lib/template-renderer.ts

export interface RenderContext {
  workspace: string;
  branch: string;
  agentRole: string;
  terminalId: string;
  timestamp: Date;
  gitStatus?: GitStatus;
}

export class TemplateRenderer {
  async render(
    content: string,
    variables: Record<string, string>,
    context: RenderContext
  ): Promise<string> {
    let result = content;

    // 1. Substitute context variables
    result = this.substituteContextVariables(result, context);

    // 2. Substitute user-provided variables
    result = this.substituteVariables(result, variables);

    // 3. Validate all required variables were substituted
    this.validateSubstitution(result);

    return result;
  }

  private substituteContextVariables(content: string, context: RenderContext): string {
    return content
      .replace(/\{\{workspace\}\}/g, context.workspace)
      .replace(/\{\{branch\}\}/g, context.branch)
      .replace(/\{\{agent_role\}\}/g, context.agentRole)
      .replace(/\{\{timestamp\}\}/g, context.timestamp.toISOString())
      .replace(/\{\{terminal_id\}\}/g, context.terminalId);
  }

  private substituteVariables(content: string, variables: Record<string, string>): string {
    for (const [key, value] of Object.entries(variables)) {
      const regex = new RegExp(`\\{\\{${key}\\}\\}`, 'g');
      content = content.replace(regex, value);
    }
    return content;
  }

  private validateSubstitution(content: string): void {
    const remainingVars = content.match(/\{\{[^}]+\}\}/g);
    if (remainingVars) {
      throw new Error(`Unsubstituted variables: ${remainingVars.join(', ')}`);
    }
  }
}
```

### Activity Database Integration

```sql
-- Extend agent_inputs table with template tracking
ALTER TABLE agent_inputs ADD COLUMN template_id TEXT;
ALTER TABLE agent_inputs ADD COLUMN template_variables TEXT; -- JSON
ALTER TABLE agent_inputs ADD COLUMN is_from_sequence BOOLEAN DEFAULT 0;
ALTER TABLE agent_inputs ADD COLUMN sequence_id TEXT;
ALTER TABLE agent_inputs ADD COLUMN sequence_step INTEGER;

-- Index for template effectiveness queries
CREATE INDEX idx_inputs_template ON agent_inputs(template_id);

-- Query: Template effectiveness
CREATE VIEW template_effectiveness AS
SELECT
  i.template_id,
  COUNT(*) as usage_count,
  AVG(CASE WHEN o.exit_code = 0 THEN 1.0 ELSE 0.0 END) as success_rate,
  AVG((julianday(o.timestamp) - julianday(i.timestamp)) * 86400) as avg_completion_seconds,
  MAX(i.timestamp) as last_used
FROM agent_inputs i
LEFT JOIN agent_outputs o ON o.input_id = i.id
WHERE i.template_id IS NOT NULL
GROUP BY i.template_id;

-- Query: Discover frequently used prompts (potential templates)
CREATE VIEW template_candidates AS
SELECT
  SUBSTR(content, 1, 100) as prompt_preview,
  COUNT(*) as occurrence_count,
  AVG(CASE WHEN o.exit_code = 0 THEN 1.0 ELSE 0.0 END) as success_rate,
  agent_role,
  GROUP_CONCAT(DISTINCT input_type) as input_types
FROM agent_inputs i
LEFT JOIN agent_outputs o ON o.input_id = i.id
WHERE i.template_id IS NULL
  AND LENGTH(i.content) > 20
GROUP BY SUBSTR(content, 1, 100), agent_role
HAVING occurrence_count >= 3
ORDER BY occurrence_count DESC;
```

## UI/UX Design

### Command Palette

**Location:** Global keyboard shortcut (Cmd+K or Cmd+Shift+P)

**Layout:**
```
┌─ Prompt Templates ────────────────────────────┐
│ 🔍 Search templates...                        │
├───────────────────────────────────────────────┤
│ Recently Used                                 │
├───────────────────────────────────────────────┤
│ ⭐ Review PR (Thorough)            [reviewer] │
│    Used 47 times · 89% success · 2m avg      │
│                                               │
│ ⭐ Fix Linting Errors              [worker]   │
│    Used 23 times · 95% success · 30s avg     │
├───────────────────────────────────────────────┤
│ All Templates (234)                           │
├───────────────────────────────────────────────┤
│ 📝 Implement Feature               [worker]   │
│    Full feature workflow with tests          │
│                                               │
│ 🐛 Bug Fix with Tests              [worker]   │
│    Fix bug, add regression test              │
│                                               │
│ 📊 Create Issue                    [issues]   │
│    Well-structured GitHub issue              │
│                                               │
│ 🏗️ Refactor Code                   [worker]   │
│    Safe refactoring with validation          │
│                                               │
│ [Showing 4 of 234 results]                   │
│                                               │
│ [Create New Template]                         │
└───────────────────────────────────────────────┘
```

**Features:**
- Fuzzy search (type "rev pr" matches "Review PR")
- Arrow keys to navigate, Enter to select
- Hover to see full preview
- Right arrow to see variables
- Cmd+1-9 for quick access to top 9
- Escape to close

### Template Editor Modal

**Triggered by:** "Create New Template" or "Edit Template" button

**Layout:**
```
┌─ Template Editor ─────────────────────────────┐
│                                               │
│ Name: [____________________________________] │
│                                               │
│ Description:                                  │
│ [__________________________________________ ] │
│ [__________________________________________ ] │
│                                               │
│ Tags: [review] [pr] [x]  + Add               │
│                                               │
│ Role: [All Roles ▼]                          │
│                                               │
│ Variables:                                    │
│ ┌─────────────────────────────────────────┐ │
│ │ pr_number (pr) · Required               │ │
│ │   Description: Pull request number      │ │
│ │   [Edit] [Remove]                       │ │
│ │                                         │ │
│ │ focus_area (text) · Optional            │ │
│ │   Default: "all aspects"                │ │
│ │   [Edit] [Remove]                       │ │
│ │                                         │ │
│ │ + Add Variable                          │ │
│ └─────────────────────────────────────────┘ │
│                                               │
│ Content:                                      │
│ ┌─────────────────────────────────────────┐ │
│ │ Please perform a thorough code review  │ │
│ │ of PR #{{pr_number}}.                  │ │
│ │                                         │ │
│ │ ## Review Checklist                    │ │
│ │ - [ ] Code style                       │ │
│ │ - [ ] Tests                            │ │
│ │                                         │ │
│ │ Focus: {{focus_area}}                  │ │
│ │                                         │ │
│ │ [Syntax: {{variable_name}}]            │ │
│ └─────────────────────────────────────────┘ │
│                                               │
│ [Preview] [Test] [Save] [Cancel]             │
└───────────────────────────────────────────────┘
```

### Template Preview & Variable Input

**Triggered by:** Selecting template from command palette

**Layout:**
```
┌─ Use Template: Review PR (Thorough) ─────────┐
│                                               │
│ Variables:                                    │
│                                               │
│ pr_number (required)                          │
│ [142_____________________] (Pull Request)    │
│                                               │
│ focus_area (optional)                         │
│ [security________________] (Text)            │
│ Default: "all aspects"                        │
│                                               │
│ Preview:                                      │
│ ┌─────────────────────────────────────────┐ │
│ │ Please perform a thorough code review  │ │
│ │ of PR #142.                            │ │
│ │                                         │ │
│ │ ## Review Checklist                    │ │
│ │ - [ ] Code style                       │ │
│ │ - [ ] Tests                            │ │
│ │                                         │ │
│ │ Focus: security                        │ │
│ └─────────────────────────────────────────┘ │
│                                               │
│ [Insert into Terminal] [Copy] [Cancel]       │
└───────────────────────────────────────────────┘
```

### Template Analytics Dashboard

**Location:** New section in workspace dashboard (from issue #177)

**Layout:**
```
┌─ Template Analytics ──────────────────────────┐
│                                               │
│ Top Templates (by usage)                      │
│ ┌─────────────────────────────────────────┐ │
│ │ 1. Review PR (Thorough)                 │ │
│ │    47 uses · 89% success · 2m 7s avg    │ │
│ │    [View Details] [Edit]                │ │
│ │                                         │ │
│ │ 2. Fix Linting Errors                   │ │
│ │    23 uses · 95% success · 30s avg      │ │
│ │    [View Details] [Edit]                │ │
│ │                                         │ │
│ │ 3. Implement Feature                    │ │
│ │    18 uses · 72% success · 8m 15s avg   │ │
│ │    [View Details] [Edit]                │ │
│ └─────────────────────────────────────────┘ │
│                                               │
│ Templates Needing Attention                   │
│ ┌─────────────────────────────────────────┐ │
│ │ ⚠️ "Create Tests" - Only 45% success    │ │
│ │    [Review Failures] [Edit Template]    │ │
│ │                                         │ │
│ │ 💡 "Fix bug X" used 8 times - templatize?│ │
│ │    [Create Template]                    │ │
│ └─────────────────────────────────────────┘ │
│                                               │
│ Usage Over Time                               │
│ [Line chart showing template usage trends]    │
│                                               │
└───────────────────────────────────────────────┘
```

## Success Metrics

### Quantitative Metrics

1. **Adoption Rate**
   - Number of templates created per user
   - Percentage of prompts using templates vs manual entry
   - Template usage growth over time

2. **Efficiency Gains**
   - Average time to enter prompt (before vs after templates)
   - Reduction in repeated prompt typing
   - Time saved per user per week

3. **Quality Improvements**
   - Success rate of templated prompts vs manual prompts
   - Error rate reduction
   - Agent completion rate improvement

4. **Library Growth**
   - Total templates created
   - Community template downloads
   - Template sharing between users

### Qualitative Metrics

1. **User Satisfaction**
   - Survey: "Templates make prompt engineering easier" (1-5)
   - Survey: "I can find relevant templates quickly" (1-5)
   - Survey: "Templates help me work more effectively" (1-5)

2. **Feature Usage**
   - Most used features (command palette, editor, sequences)
   - Least used features (candidates for improvement or removal)
   - User feedback and feature requests

### Target Goals (6 months post-launch)

- ✅ 80% of users have created at least 5 templates
- ✅ 50% of all prompts use templates
- ✅ Average prompt entry time reduced by 60%
- ✅ Template success rate 15% higher than manual prompts
- ✅ Community library has 500+ templates
- ✅ User satisfaction score 4.2+ out of 5

## Dependencies

**Required:**
- None (can be built standalone)

**Recommended:**
- Issue #174 (Agent Activity Database) - for effectiveness tracking
- Issue #177 (Visualization) - for analytics dashboard

**Synergy:**
- Works perfectly with both #174 and #177
- Templates link to activity data
- Analytics show template performance
- Discovery suggests templates from activity patterns

## Future Enhancements (Beyond Initial Implementation)

### 1. AI-Powered Features
- **Smart Suggestions:** ML model suggests templates based on context
- **Template Generation:** AI generates templates from natural language descriptions
- **Optimization:** AI analyzes failures and suggests template improvements
- **Variable Prediction:** AI predicts variable values from context

### 2. Advanced Composition
- **Visual Workflow Builder:** Drag-and-drop sequence editor
- **Parallel Execution:** Run multiple templates concurrently
- **Dynamic Branching:** Choose next step based on previous output
- **Template Inheritance:** Base templates with variations

### 3. Collaboration Features
- **Team Libraries:** Shared template repositories
- **Review Process:** Approve templates before adding to team library
- **Usage Analytics:** See which templates your team uses most
- **Template Comments:** Discuss and improve templates collaboratively

### 4. Integration
- **IDE Integration:** Use templates in VS Code, JetBrains, etc.
- **CLI Tool:** Command-line access to templates
- **API:** REST API for external tools
- **Webhooks:** Trigger templates from external events

### 5. Learning & Evolution
- **A/B Testing:** Test template variations automatically
- **Success Prediction:** Predict if template will succeed before running
- **Automated Refinement:** Templates evolve based on outcomes
- **Best Practices Library:** Curated collection of proven templates

## Risks & Mitigation

### Risk 1: Template Sprawl
**Problem:** Too many similar templates, hard to find the right one

**Mitigation:**
- Smart duplicate detection
- Suggest merging similar templates
- Tag-based organization
- Search and filter tools

### Risk 2: Stale Templates
**Problem:** Templates become outdated as codebase evolves

**Mitigation:**
- Track template effectiveness over time
- Alert when success rate drops significantly
- Version templates with changelog
- Deprecation workflow

### Risk 3: Complexity Overhead
**Problem:** Template system too complex, users revert to manual prompts

**Mitigation:**
- Start simple (Phase 1 is minimal viable)
- Progressive disclosure (advanced features optional)
- Excellent documentation and examples
- Quick wins with pre-made templates

### Risk 4: Performance
**Problem:** Template rendering and search slow down UI

**Mitigation:**
- Cache parsed templates in memory
- Index templates for fast search
- Lazy load template content
- Optimize rendering pipeline

## Open Questions

1. **Storage Format:** Markdown + YAML frontmatter vs JSON vs database?
   - **Recommendation:** Markdown + YAML (human-readable, git-friendly)

2. **Template Versioning:** How to handle template evolution?
   - **Recommendation:** Version field in metadata, migration tools

3. **Security:** Can templates be malicious? Need sandboxing?
   - **Recommendation:** Templates are just text, but validate variable substitution

4. **Sharing Privacy:** What data is included when sharing templates?
   - **Recommendation:** Strip usage data, keep only template definition

5. **Namespace Conflicts:** What if imported template has same ID?
   - **Recommendation:** Prompt user to rename or overwrite

## References

- Issue #174: Agent Activity Database
- Issue #177: Visualization and Metrics
- `src/lib/state.ts`: State management patterns
- `src/lib/config.ts`: Config file I/O patterns
- `defaults/roles/`: Role definition examples

## Success Definition

This feature is successful when:

1. ✅ Users can create, edit, and use templates with < 30 seconds of effort
2. ✅ Templates can be found in < 5 seconds via command palette
3. ✅ 50%+ of prompts use templates within 3 months of launch
4. ✅ Template success rate is measurably higher than manual prompts
5. ✅ Users report templates make them more productive (user research)
6. ✅ Community template library is active with regular contributions
7. ✅ Multi-step sequences work reliably for complex workflows
8. ✅ Agents can use templates autonomously with minimal configuration

---

**Ready for Implementation:** This issue provides complete technical design, phased implementation plan, and success metrics. No additional design work needed before starting Phase 1.
