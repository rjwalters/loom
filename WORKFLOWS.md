# Loom Agent Workflows

This document describes the label-based workflows that coordinate multiple AI agents working together on a codebase.

## Overview

Loom uses GitHub labels as a coordination protocol. Each agent type has a specific role and watches for issues/PRs with particular labels. This creates a complete pipeline from idea generation through implementation to code review.

### 🌙 The Archetypal Cycle

In Loom, development follows an ancient pattern of archetypal forces working in harmony:

1. 🏛️ **The Architect** envisions → creates proposals (`loom:proposal`)
2. 🔍 **The Critic** questions → identifies bloat and simplification opportunities (`loom:critic-suggestion`)
3. 📚 **The Curator** refines → enhances and marks issues ready (`loom:ready`)
4. 🔮 **The Worker** manifests → implements and creates PRs (`loom:review-requested`)
5. 🔧 **The Fixer** heals → addresses review feedback (`loom:changes-requested` → `loom:review-requested`)
6. ⚖️ **The Reviewer** judges → maintains quality through discernment (`loom:pr`)

*Like the Tarot's Major Arcana, each role is essential to the whole. See [Agent Archetypes](docs/philosophy/agent-archetypes.md) for the mystical framework.*

**Color-coded workflow:**
- 🔵 **Blue** = Human action needed
  - Issues: `loom:proposal` (Architect suggestion awaiting approval)
  - PRs: `loom:pr` (Approved PR ready to merge)
- 🟢 **Green** = Loom bot action needed
  - Issues: `loom:ready` (Issue ready for Worker)
  - PRs: `loom:review-requested` (PR ready for Reviewer)
- 🟡 **Amber** = Work in progress
  - Issues: `loom:in-progress` (Worker implementing)
  - PRs: `loom:changes-requested` (Fixer addressing review feedback)
- 🔴 **Red** = Blocked or urgent
  - `loom:blocked` (Blocked, needs help)
  - `loom:urgent` (High priority)

See [scripts/LABEL_WORKFLOW.md](scripts/LABEL_WORKFLOW.md) for detailed documentation.

## Priority System

Issues can have an optional priority label to ensure urgent work gets immediate attention:

| Priority | Label | Worker Behavior |
|----------|-------|-----------------|
| 🔴 **Urgent** | `loom:urgent` | Workers check first, before all other issues |
| 🟢 **Normal** | *(no priority label)* | Workers use FIFO (oldest first) |

### Who Manages Priority Labels?

- **Triage Agent**: Continuously monitors `loom:ready` issues and maintains top 3 as `loom:urgent`
- **User**: Ultimate authority, can override any time
- **Worker**: Should NOT add (conflict of interest)

**Maximum Urgent: 3 Issues**

The Triage agent enforces a strict limit: **never more than 3 issues marked `loom:urgent`**. This constraint prevents "everything is urgent" syndrome and forces real prioritization decisions.

### When to Use Urgent Priority

The Triage agent marks issues as `loom:urgent` based on:

✅ **Strategic Impact**:
- Blocks 2+ other high-value issues
- Foundation for entire roadmap area
- Unblocks entire team/workflow

✅ **Time Sensitivity**:
- Security vulnerabilities requiring patches
- Critical bugs affecting users
- Production issues needing hotfixes
- User explicitly requested urgency

✅ **Effort vs Value**:
- Quick wins (< 1 day) with major impact
- Low risk, high reward opportunities

❌ **DO NOT mark urgent**:
- Nice to have but not blocking anything
- Can wait until next sprint
- Large effort with uncertain value

**Most issues should be normal priority** (no label). Urgent means "must be done NOW, before anything else."

## Dependency Tracking with Task Lists

Loom uses GitHub's native task list feature to track issue dependencies explicitly. This allows issues to declare prerequisites and automatically updates when dependencies are completed.

### How It Works

Issues can include a **Dependencies** section with a GitHub task list linking to prerequisite issues:

```markdown
## Dependencies

- [ ] #123: Database migration system
- [ ] #456: User authentication API

This issue cannot proceed until all dependencies above are complete.
```

**Key Benefits:**
- ✅ GitHub automatically checks boxes when linked issues close
- ✅ Visual progress indicator in issue cards
- ✅ Clear "ready to start" signal when all boxes checked
- ✅ Machine-readable for agent decision-making

### Agent Responsibilities

#### Architect: Adding Dependencies

When creating proposals, Architect should add Dependencies sections for issues that require prerequisite work:

```bash
gh issue create --title "Implement user profile page" --body "$(cat <<'EOF'
## Problem
Users need a way to view and edit their profile information.

## Dependencies

- [ ] #100: User authentication system
- [ ] #101: Database schema for user profiles

This feature requires authentication and database schema to be complete first.

## Proposed Solution
...
EOF
)"
```

**When to add dependencies:**
- Issue requires infrastructure/framework not yet built
- Implementation must wait for other features
- Multi-phase feature with sequential steps

#### Curator: Checking Dependencies

Before marking an issue as `loom:ready`, Curator must verify all dependencies are complete:

**Decision Logic:**

1. **If Dependencies section exists:**
   - Check if all task list items are checked (✅)
   - **All checked** → Safe to mark `loom:ready`
   - **Any unchecked** → Add `loom:blocked` label, do NOT mark `loom:ready`

2. **If NO Dependencies section:**
   - Issue has no blockers → Safe to mark `loom:ready`

**Example workflow:**
```bash
# View issue to check dependencies
gh issue view 42

# If all dependencies checked:
gh issue edit 42 --add-label "loom:ready"

# If unchecked dependencies exist:
gh issue edit 42 --add-label "loom:blocked"
gh issue comment 42 --body "Blocked by unchecked dependencies: #100, #101"
```

When Curator discovers new dependencies during enhancement, they should:
1. Add Dependencies section to the issue
2. Add `loom:blocked` label
3. Leave comment explaining the blocker

#### Worker: Verifying Before Claiming

Before claiming a `loom:ready` issue, Worker must check for dependencies:

```bash
# View issue to check Dependencies section
gh issue view 42

# If Dependencies section exists with unchecked boxes:
gh issue edit 42 --remove-label "loom:ready" --add-label "loom:blocked"
gh issue comment 42 --body "Cannot claim - blocked by unchecked dependencies"

# If all dependencies checked (or no Dependencies section):
gh issue edit 42 --remove-label "loom:ready" --add-label "loom:in-progress"
# Proceed with implementation
```

If Worker discovers a dependency during implementation:
1. Add Dependencies section to the issue
2. Add `loom:blocked` label to issue
3. Create comment explaining the blocker
4. Either wait or switch to another issue

### Dependency Lifecycle Example

```
┌─────────────────────────────────────────────────────────────┐
│ 1. ARCHITECT CREATES DEPENDENT ISSUE                        │
│    Issue #42: "Implement user profile page"                │
│    Body includes:                                           │
│    ## Dependencies                                          │
│    - [ ] #100: User authentication system                  │
│    - [ ] #101: Database schema for user profiles          │
│    Label: loom:proposal                                     │
└─────────────────────┬───────────────────────────────────────┘
                      │
                      ↓
┌─────────────────────────────────────────────────────────────┐
│ 2. USER APPROVES                                            │
│    Removes loom:proposal label                              │
└─────────────────────┬───────────────────────────────────────┘
                      │
                      ↓
┌─────────────────────────────────────────────────────────────┐
│ 3. CURATOR CHECKS DEPENDENCIES                              │
│    Views issue #42                                          │
│    Sees unchecked boxes for #100, #101                     │
│    Decision: Dependencies not complete                      │
│    Action: gh issue edit 42 --add-label "loom:blocked"     │
│    (Does NOT add loom:ready)                                │
└─────────────────────┬───────────────────────────────────────┘
                      │
                      ↓
┌─────────────────────────────────────────────────────────────┐
│ 4. DEPENDENCIES COMPLETE                                    │
│    Worker closes #100 → GitHub auto-checks box             │
│    Worker closes #101 → GitHub auto-checks box             │
│    Issue #42 now shows: [x] #100, [x] #101                │
└─────────────────────┬───────────────────────────────────────┘
                      │
                      ↓
┌─────────────────────────────────────────────────────────────┐
│ 5. CURATOR UNBLOCKS                                         │
│    Sees all dependency boxes checked on #42                 │
│    gh issue edit 42 --remove-label "loom:blocked"          │
│    gh issue edit 42 --add-label "loom:ready"               │
└─────────────────────┬───────────────────────────────────────┘
                      │
                      ↓
┌─────────────────────────────────────────────────────────────┐
│ 6. WORKER CLAIMS AND IMPLEMENTS                             │
│    Verifies all dependencies checked                        │
│    Claims issue #42                                         │
│    Implements feature                                       │
└─────────────────────────────────────────────────────────────┘
```

### Best Practices

**For Architects:**
- Only add Dependencies for true blockers (not nice-to-haves)
- Keep dependency descriptions brief but clear
- Explain why the dependency exists if not obvious
- For independent work, explicitly state "No dependencies"

**For Curators:**
- Always check Dependencies section before marking `loom:ready`
- Use `loom:blocked` label when dependencies are incomplete
- Add comment explaining what's blocking when marking as blocked
- Monitor blocked issues and unblock when dependencies complete

**For Workers:**
- Always verify dependencies before claiming `loom:ready` issues
- If you discover a blocker mid-implementation, add it to the issue immediately
- Don't try to implement dependencies yourself - create separate issues
- Mark as `loom:blocked` and explain the situation in a comment

### Troubleshooting

**Issue has unchecked dependencies but is marked loom:ready**
→ Worker should mark as `loom:blocked` when they discover this
→ Curator should re-check and correct the labeling

**Dependency is complete but checkbox not checked**
→ GitHub auto-checks when issues close via PR merge
→ Manually check box if issue was closed directly

**Circular dependencies between issues**
→ This is a design problem - escalate to User for manual resolution
→ Architect should avoid creating circular dependencies

## Agent Types

### 1. Architect Bot
**Role**: Improvement proposal generator

**Watches for**: N/A (proactively scans codebase)

**Creates**:
- Issues with `loom:proposal` label (blue badge - awaiting user approval)

**Interval**: 15 minutes (recommended autonomous)

**Scope**: Scans codebase across all domains:
- **Architecture & Features**: New features, API design, system improvements
- **Code Quality**: Refactoring, consistency, duplication, unused code
- **Documentation**: Outdated docs, missing explanations, API documentation
- **Testing**: Missing coverage, flaky tests, edge cases
- **CI/Build/Tooling**: Failing jobs, slow builds, outdated dependencies
- **Performance & Security**: Optimizations, vulnerabilities, resource leaks

**Workflow**:
```
1. Check if there are already 3+ open proposals (don't spam)
2. If < 3 proposals: Scan codebase for improvement opportunities
3. Create comprehensive issue with proposal
4. Add loom:proposal label immediately (blue badge)
5. Wait for user to remove loom:proposal label (approval)
```

**Important**: Architect does NOT triage issues created by others. Only creates proposals.

### 2. Curator Bot
**Role**: Enhances approved issues and marks them ready for implementation

**Watches for**:
- Issues without `loom:proposal` label (user has approved them)
- Excludes issues already marked `loom:ready` or `loom:in-progress`

**Creates**:
- `loom:ready` - Issues ready for worker implementation

**Interval**: 5 minutes (recommended autonomous)

**Workflow**:
```
1. Find approved issues (no loom:proposal label, not yet ready/in-progress)
2. Review issue description and requirements
3. Add implementation details, test plans, code references
4. Document multiple implementation options if complex
5. Add loom:ready when enhancement complete
```

### 3. Triage Bot
**Role**: Dynamic priority management for ready issues

**Watches for**:
- `loom:ready` - Issues ready to be prioritized

**Creates/Manages**:
- `loom:urgent` - Maintains exactly top 3 priorities

**Interval**: 15 minutes (recommended autonomous)

**Workflow**:
```
1. Review all loom:ready issues
2. Assess strategic priority based on:
   - Impact on users/product vision
   - Blocks other high-value work
   - Time-sensitive (security, bugs, urgent requests)
   - Effort vs value ratio
3. Apply loom:urgent to top 3 issues only
4. Remove loom:urgent from issues no longer critical
5. When adding 4th urgent:
   - Demote least critical of current 3 (with explanation)
   - Promote new top priority (with reasoning)
```

**Key Constraint**: Never have more than 3 issues marked `loom:urgent`.

### 4. Worker Bot
**Role**: Implements features and fixes bugs

**Watches for**:
- `loom:ready` - Issues ready to be implemented

**Creates**:
- `loom:in-progress` - Claims issue for implementation
- `loom:review-requested` - PRs ready for Reviewer
- `loom:blocked` - When stuck on implementation

**Interval**: Disabled by default (on-demand, one Worker per PR)

**Workflow**:
```
1. Find loom:ready issues (green badges)
2. Claim by removing loom:ready, adding loom:in-progress
3. Implement, test, commit
4. Create PR with "Closes #X", add loom:review-requested (green - ready for Reviewer)
5. Monitor PR and address Reviewer feedback
6. If blocked: add loom:blocked with explanation
```

### 5. Reviewer Bot
**Role**: Reviews pull requests

**Watches for**:
- `loom:review-requested` - PRs ready for review (green badges)

**Creates**:
- `loom:changes-requested` - PR needs fixes from Fixer (amber)
- `loom:pr` - Approved PRs ready for human to merge (blue)

**Interval**: 5 minutes (recommended autonomous)

**Workflow**:
```
1. Find loom:review-requested PRs (green badges)
2. Check out branch, run tests, review code
3. If changes needed: gh pr review --request-changes, change label to loom:changes-requested (amber)
4. If approved: gh pr review --approve, change label to loom:pr (blue)
```

### 6. Fixer Bot
**Role**: PR health specialist, addresses review feedback and resolves conflicts

**Watches for**:
- `loom:changes-requested` - PRs with changes requested by Reviewer (amber badges)
- PRs with merge conflicts: `gh pr list --search "conflicts:>0"`

**Creates**:
- `loom:review-requested` - After addressing feedback, signals PR is ready for re-review (green)

**Interval**: 5-10 minutes (recommended autonomous or manual)

**Workflow**:
```
1. Find PRs needing fixes:
   - gh pr list --label="loom:changes-requested" (review feedback)
   - gh pr list --search "conflicts:>0" (merge conflicts)
2. Check out PR branch: gh pr checkout <number>
3. Read reviewer comments and understand feedback
4. Address the requested changes and/or resolve conflicts
5. Run pnpm check:ci to ensure all checks pass
6. Commit and push fixes
7. Signal ready for re-review:
   - Remove loom:changes-requested (amber badge)
   - Add loom:review-requested (green badge)
8. Comment on PR to notify reviewer
```

**Relationship with Reviewer**:
- Reviewer manages initial review workflow and approval
- Fixer addresses technical issues and signals completion
- Fixer transitions `loom:changes-requested` → `loom:review-requested` after fixes (completing the cycle)

### 7. Critic Bot
**Role**: Code simplification specialist, identifies bloat and suggests removals

**Watches for**: N/A (proactively scans codebase and reviews open issues)

**Creates**:
- Issues with `loom:critic-suggestion` label (standalone removal proposals)
- Comments with `<!-- CRITIC-SUGGESTION -->` marker (simplification suggestions on existing issues)

**Interval**: 15 minutes (recommended autonomous)

**Workflow**:
```
# Standalone Removal Proposals:
1. Scan codebase for unused code, dependencies, over-engineering
2. Verify with evidence (searches, dead code analysis)
3. Create issue with loom:critic-suggestion label (blue badge)
4. User reviews and removes label to approve (or closes to reject)

# Simplification Comments:
1. Review open issues for simplification opportunities
2. Check if planned work includes unnecessary complexity
3. Add comment with <!-- CRITIC-SUGGESTION --> marker
4. Assignee can adopt, adapt, or ignore the suggestion
```

**Key Principles**:
- Evidence-based suggestions only (show proof with rg/git commands)
- Two approaches: standalone issues for independent bloat, comments for scope reduction
- Respects assignee decisions on comments
- Quality over quantity (0-1 suggestions per interval)

### 7. Issues Bot
**Role**: Creates well-structured GitHub issues from user requests

**Watches for**: N/A (manual invocation)

**Creates**: Well-formatted issues with proper structure

**Interval**: Disabled (manual only)

**Workflow**:
```
1. User provides feature request or bug report
2. Structure into clear issue format
3. Add acceptance criteria, test plan
4. Include code references and context
5. Create issue (no label initially)
```

### 8. Default (Plain Shell)
**Role**: Standard terminal for manual commands

No automation. Used for manual git operations, system commands, etc.

## Complete Workflow Example

### Feature Implementation Flow

```
┌─────────────────────────────────────────────────────────────┐
│ 1. ARCHITECT CREATES PROPOSAL                               │
│    gh issue create                                          │
│    Title: "Add search functionality to terminal history"    │
│    gh issue edit <#> --add-label "loom:proposal"            │
│    (Blue badge - awaiting user approval)                    │
└─────────────────────┬───────────────────────────────────────┘
                      │
                      ↓
┌─────────────────────────────────────────────────────────────┐
│ 2. USER REVIEWS AND APPROVES                                │
│    Reviews issue with loom:proposal (blue badge)            │
│    Removes loom:proposal label to approve                   │
│    (Or closes issue to reject)                              │
└─────────────────────┬───────────────────────────────────────┘
                      │
                      ↓
┌─────────────────────────────────────────────────────────────┐
│ 3. CURATOR ENHANCES ISSUE                                   │
│    Finds approved issue #42 (no loom:proposal)              │
│    Adds implementation details:                             │
│    - Multiple implementation options                        │
│    - Dependencies and risks                                 │
│    - Test plan checklist                                    │
│    Adds loom:ready (green badge)                            │
└─────────────────────┬───────────────────────────────────────┘
                      │
                      ↓
┌─────────────────────────────────────────────────────────────┐
│ 4. WORKER IMPLEMENTS                                        │
│    Finds loom:ready issue #42 (green badge)                 │
│    Updates: removes loom:ready, adds loom:in-progress       │
│    (Amber badge)                                            │
│    Implements feature, writes tests                         │
│    Creates PR: "Closes #42", adds loom:review-requested     │
│    (Green badge - ready for review)                         │
└─────────────────────┬───────────────────────────────────────┘
                      │
                      ↓
┌─────────────────────────────────────────────────────────────┐
│ 5. REVIEWER REVIEWS PR                                      │
│    Finds loom:review-requested PR #50 (green badge)         │
│    Checks out branch, runs tests                            │
│    Reviews code, provides feedback                          │
│    If approved: gh pr review --approve                      │
│    Updates: removes loom:review-requested, adds loom:pr     │
│    (Blue badge - ready for user to merge)                   │
└─────────────────────┬───────────────────────────────────────┘
                      │
                      ↓
┌─────────────────────────────────────────────────────────────┐
│ 6. USER MERGES PR                                           │
│    Reviews loom:pr PR (blue badge)                          │
│    Merges to main                                           │
│    Issue #42 automatically closes                           │
└─────────────────────────────────────────────────────────────┘
```

## Label Reference

### Issue Labels

| Label | Color | Created By | Meaning |
|-------|-------|-----------|---------|
| `loom:proposal` | 🔵 Blue | Architect | Proposal awaiting user approval |
| `loom:critic-suggestion` | 🔵 Blue | Critic | Removal/simplification proposal awaiting user approval |
| `loom:ready` | 🟢 Green | Curator | Issue ready for Worker to implement |
| `loom:in-progress` | 🟡 Amber | Worker | Worker actively implementing |
| `loom:blocked` | 🔴 Red | Worker | Implementation blocked, needs help |
| `loom:urgent` | 🔴 Dark Red | User/Architect/Curator | High priority, work on first |

### PR Labels

| Label | Color | Created By | Meaning |
|-------|-------|-----------|---------|
| `loom:review-requested` | 🟢 Green | Worker/Fixer | PR ready for Reviewer |
| `loom:changes-requested` | 🟡 Amber | Reviewer | PR needs fixes from Fixer |
| `loom:pr` | 🔵 Blue | Reviewer | Approved PR ready for human to merge |

**Key insights**:
- **Blue badges** = Human action needed
- **Green badges** = Bot action needed
- **Amber badges** = Work in progress
- **Red badges** = Blocked or urgent
- Users control the flow by removing `loom:proposal` to approve Architect suggestions

## Commands Reference

### Architect
```bash
# Check existing proposals (don't spam)
gh issue list --label="loom:proposal" --state=open

# Create new improvement proposal (any domain)
gh issue create --title "..." --body "..."

# Add proposal label (blue badge - awaiting user approval)
gh issue edit <number> --add-label "loom:proposal"
```

### User (Manual)
```bash
# Find proposals awaiting approval (blue badges)
gh issue list --label="loom:proposal" --state=open

# Approve a proposal (remove blue badge)
gh issue edit <number> --remove-label "loom:proposal"

# Reject a proposal
gh issue close <number> --comment "Not needed because..."

# Find PRs ready to merge (blue badges)
gh pr list --label="loom:pr" --state=open

# Merge approved PR
gh pr merge <number>
```

### Critic
```bash
# Create standalone removal proposal
gh issue create --title "Remove [thing]: [reason]" --body "..." --label "loom:critic-suggestion"

# Check existing critic suggestions (don't spam)
gh issue list --label="loom:critic-suggestion" --state=open

# Find open issues to potentially comment on
gh issue list --state=open --json number,title --jq '.[] | "\(.number): \(.title)"'

# Add simplification comment to existing issue
gh issue comment <number> --body "$(cat <<'EOF'
<!-- CRITIC-SUGGESTION -->
## 🔍 Simplification Opportunity
[Your suggestion with evidence]
EOF
)"
```

### Curator
```bash
# Find approved issues (no loom:proposal, not yet ready/in-progress)
gh issue list --state=open --json number,title,labels \
  --jq '.[] | select(([.labels[].name] | inside(["loom:proposal", "loom:ready", "loom:in-progress"]) | not)) | "#\(.number) \(.title)"'

# Mark issue as ready (add green badge)
gh issue edit <number> --add-label "loom:ready"
```

### Triage
```bash
# Find all ready issues
gh issue list --label="loom:ready" --state=open --json number,title,labels,body

# Find currently urgent issues
gh issue list --label="loom:urgent" --state=open

# Mark issue as urgent (with explanation)
gh issue edit <number> --add-label "loom:urgent"
gh issue comment <number> --body "🚨 **Marked as urgent** - [Explain reasoning]"

# Remove urgent label (with explanation)
gh issue edit <number> --remove-label "loom:urgent"
gh issue comment <number> --body "ℹ️ **Removed urgent label** - [Explain priority shift]"
```

### Worker
```bash
# Find ready issues (green badges)
gh issue list --label="loom:ready" --state=open

# Claim issue (green → amber)
gh issue edit <number> --remove-label "loom:ready" --add-label "loom:in-progress"

# Create PR with green badge (ready for Reviewer)
gh pr create --title "..." --body "Closes #X" --label "loom:review-requested"

# Mark blocked (amber → red)
gh issue edit <number> --add-label "loom:blocked"
gh issue comment <number> --body "Blocked because..."
```

### Reviewer
```bash
# Find PRs ready to review (green badges)
gh pr list --label="loom:review-requested" --state=open

# Check out and test
gh pr checkout <number>
pnpm check:all

# Approve PR (green → blue)
gh pr review <number> --approve --body "LGTM!"
gh pr edit <number> --remove-label "loom:review-requested" --add-label "loom:pr"

# Request changes (green → amber)
gh pr review <number> --request-changes --body "Issues found..."
gh pr edit <number> --remove-label "loom:review-requested" --add-label "loom:changes-requested"
```

### Fixer
```bash
# Find PRs with changes requested (amber badges)
gh pr list --label="loom:changes-requested" --state=open

# Find PRs with merge conflicts
gh pr list --state=open --search "is:open conflicts:>0"

# Check out PR branch
gh pr checkout <number>

# Read reviewer feedback
gh pr view <number> --comments

# Address issues, fix conflicts, etc.
# ... make your changes ...

# Verify everything works
pnpm check:ci

# Commit and push
git add .
git commit -m "Address review feedback and resolve conflicts"
git push

# Signal ready for re-review (amber → green)
gh pr edit <number> --remove-label "loom:changes-requested" --add-label "loom:review-requested"
gh pr comment <number> --body "✅ Feedback addressed and conflicts resolved. Ready for re-review!"
```

## Configuration

Each role has default settings in `.loom/roles/<role>.json`:

```json
{
  "name": "Curator Bot",
  "description": "Processes unlabeled issues",
  "defaultInterval": 300000,
  "defaultIntervalPrompt": "Find unlabeled issues...",
  "autonomousRecommended": true,
  "suggestedWorkerType": "claude"
}
```

Users can override these defaults in the Terminal Settings modal.

## Best Practices

### For Users

1. **Review suggestions promptly**: Architect, worker, and reviewer suggestions need approval
2. **Remove suggestion labels to accept**: Unlabeled = approved for processing
3. **Close unwanted suggestions**: Don't leave suggestions hanging
4. **Review PRs before merging**: Approved ≠ automatically merge

### For Agents

1. **Stay in your lane**: Don't do other roles' work
2. **Complete current task first**: Don't get sidetracked by discoveries
3. **Document thoroughly**: Future agents need context
4. **Use labels correctly**: Label workflow keeps everyone coordinated
5. **Reference issues**: Always link to related work

### For Autonomous Operation

1. **Curator + Reviewer + Architect**: Best combination for autonomous mode
2. **Worker**: Usually manual, autonomous only for maintenance work
3. **Interval settings**: Curator/Reviewer 5min, Architect 15min
4. **Monitor blocked issues**: Auto-resolve or escalate to user

## Troubleshooting

### Issue stuck without labels
→ Curator should pick it up within 5 minutes (if autonomous)
→ Manually add `loom:ready` if urgent

### Issue labeled loom:ready but not claimed
→ Worker agents may be disabled
→ Manually assign or claim with different worker

### PR labeled loom:review-requested but not reviewed
→ Reviewer agent may be disabled
→ Manually review or remove label to skip

### Multiple agents claiming same issue/PR
→ Labels should prevent this (first agent removes trigger label)
→ If race condition: coordinate manually, one agent backs off

## Future Enhancements

- **Automatic label transitions**: Remove manual label management
- **Priority labels**: `P0`, `P1`, `P2` for urgent vs normal vs low priority
- **Specialization labels**: `frontend`, `backend`, `ui`, `api` for agent specialization
- **Automated merging**: Auto-merge approved PRs after CI passes
- **Workload balancing**: Distribute issues across multiple worker agents
- **Progress tracking**: Dashboards showing agent activity and velocity
