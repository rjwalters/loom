# Loom Agent Workflows

This guide describes the agent workflows for Loom orchestration in this repository.

**Loom Version**: {{LOOM_VERSION}}

## Agent Archetypes

Loom uses specialized agent roles based on universal archetypes. Each role embodies a specific pattern of behavior and responsibility in the development workflow.

### The Eight Roles

#### 1. Builder (The Magician)
**Mode**: Manual
**File**: `builder.md`
**Purpose**: Transform ideas into working code

The Builder manifests features and fixes through skilled implementation. They claim approved issues, implement solutions, and create pull requests.

**Workflow**:
```
loom:issue → claim → implement → test → PR (loom:review-requested)
```

**Key Activities**:
- Claim `loom:issue` labeled issues
- Create worktree for isolated development
- Implement features or fix bugs
- Write tests and documentation
- Create PR with `loom:review-requested` label

#### 2. Judge (The Justice)
**Mode**: Autonomous (5 min intervals)
**File**: `judge.md`
**Purpose**: Ensure quality and fairness through review

The Judge evaluates pull requests with objectivity and thoroughness. They provide constructive feedback and make approval decisions.

**Workflow**:
```
loom:review-requested → review → approve/request-changes → loom:pr or back to author
```

**Key Activities**:
- Find PRs with `loom:review-requested` label
- Review code for quality, correctness, and style
- Test the changes locally
- Approve or request changes
- Update labels appropriately

#### 3. Curator (The Hermit)
**Mode**: Autonomous (5 min intervals)
**File**: `curator.md`
**Purpose**: Maintain and enhance the issue backlog

The Curator brings wisdom and clarity to issues. They enhance vague issues with technical details and context.

**Workflow**:
```
unlabeled issue → enhance → loom:curated → (approval) → loom:issue
```

**Key Activities**:
- Find issues without workflow labels
- Add technical details and acceptance criteria
- Link related issues and documentation
- Mark as `loom:curated` for human approval
- After approval, mark as `loom:issue`

#### 4. Architect (The Emperor)
**Mode**: Autonomous (15 min intervals)
**File**: `architect.md`
**Purpose**: Design system structure and make technical decisions

The Architect brings order through careful planning. They create architectural proposals for significant changes.

**Workflow**:
```
analyze → create proposal → loom:architect → (approval) → loom:issue
```

**Key Activities**:
- Analyze system architecture
- Identify improvement opportunities
- Create detailed architectural proposals
- Document decisions and tradeoffs
- Label proposals with `loom:architect`

#### 5. Hermit (The Fool)
**Mode**: Autonomous (15 min intervals)
**File**: `hermit.md`
**Purpose**: Simplify through removal and letting go

The Hermit identifies what can be removed or simplified. They propose removing unused code and reducing complexity.

**Workflow**:
```
analyze → identify bloat → create removal proposal → loom:hermit → (approval) → loom:issue
```

**Key Activities**:
- Analyze codebase complexity
- Find unused or redundant code
- Identify over-engineered solutions
- Create simplification proposals
- Label proposals with `loom:hermit`

#### 6. Doctor (The Star)
**Mode**: Manual
**File**: `doctor.md`
**Purpose**: Fix bugs and maintain health

The Doctor brings hope through fixing what's broken. They address bugs, PR feedback, and maintenance tasks.

**Workflow**:
```
bug report OR PR feedback → fix → test → commit → push
```

**Key Activities**:
- Claim bug reports or blocked issues
- Address PR review feedback
- Fix failing tests or builds
- Maintain existing PRs
- Resolve `loom:blocked` issues

#### 7. Guide (The Hierophant)
**Mode**: Autonomous (15 min intervals)
**File**: `guide.md`
**Purpose**: Organize and prioritize work

The Guide brings structure through organization. They triage issues, set priorities, and maintain workflow clarity.

**Workflow**:
```
review backlog → update priorities → organize labels → document status
```

**Key Activities**:
- Review entire issue backlog
- Update priority labels
- Organize issues by category
- Document project status
- Remove stale labels

#### 8. Driver (The Chariot)
**Mode**: Manual
**File**: `driver.md`
**Purpose**: Direct action and execution

The Driver executes commands directly without specific role constraints. A plain shell for custom tasks.

**Workflow**:
```
receive command → execute → report
```

**Key Activities**:
- Execute user commands directly
- Perform ad-hoc tasks
- Debug and investigate
- Run custom scripts

## Label-Based Coordination

Agents coordinate autonomously through GitHub labels. No direct communication is required.

### Label State Machine

```
┌─────────────────────────────────────────────────┐
│                  Issue Lifecycle                 │
└─────────────────────────────────────────────────┘

(created)
   │
   ↓ Curator enhances
loom:curated ──→ (human approves) ──→ loom:issue
                                         │
                                         ↓ Builder claims
                                   loom:building
                                         │
                                         ↓ Implementation complete
                                      (closed)

┌─────────────────────────────────────────────────┐
│                   PR Lifecycle                   │
└─────────────────────────────────────────────────┘

(created by Builder)
   │
   ↓
loom:review-requested ──→ Judge reviews ──→ loom:pr
   ↑                                           │
   │                                           ↓ Human merges
   │                                        (merged)
   │
   └─── (changes requested, back to Builder)

┌─────────────────────────────────────────────────┐
│                Proposal Lifecycle                │
└─────────────────────────────────────────────────┘

Architect creates ──→ loom:architect ──→ (human approves) ──→ loom:issue
Hermit creates ──→ loom:hermit ──→ (human approves) ──→ loom:issue
```

### Label Definitions

**Workflow States**:
- `loom:issue` - Approved for work, ready for Builder to claim
- `loom:building` - Issue being implemented OR PR under review/revision
- `loom:review-requested` - PR ready for Judge to review
- `loom:pr` - PR approved, ready for human to merge

**Proposals** (require human approval):
- `loom:architect` - Architectural proposal by Architect
- `loom:hermit` - Simplification proposal by Hermit
- `loom:curated` - Enhanced issue by Curator

**Status Indicators**:
- `loom:blocked` - Work blocked, needs help or clarification
- `loom:urgent` - Critical issue requiring immediate attention

## Autonomous Operation

Agents can run autonomously at configured intervals using either:
1. **`loom-daemon` dispatch + GitHub Actions cron**: `mcp__loom__dispatch_sweep` enqueues `/loom:sweep` children against the running Rust `loom-daemon` (operator-driven, multi-account); `.github/workflows/loom-*.yml` run periodic support roles on cron schedules
2. **Manual Orchestration Mode**: Multiple Claude Code terminals with periodic commands

### Autonomous Agents

**Judge** (5 min intervals):
```
Every 5 minutes: Find and review PRs with loom:review-requested label
```

**Curator** (5 min intervals):
```
Every 5 minutes: Find unlabeled issues, enhance them, mark as loom:curated
```

**Architect** (15 min intervals):
```
Every 15 minutes: Analyze codebase, create architectural proposals
```

**Hermit** (15 min intervals):
```
Every 15 minutes: Identify complexity, create simplification proposals
```

**Guide** (15 min intervals):
```
Every 15 minutes: Review issue backlog, update priorities and organization
```

### Manual Agents

**Builder**, **Doctor**, **Driver** require human direction:
- Builder: Claim specific issues when ready to implement
- Doctor: Respond to bug reports or PR feedback
- Driver: Execute user commands as requested

## Coordination Examples

### Example 1: Feature Development

1. **Curator** finds vague issue #42, enhances it with technical details
   - Adds `loom:curated` label
   - Human/Champion reviews and adds `loom:issue` (curated label is preserved)

2. **Builder** (you) claims the issue
   ```bash
   gh issue edit 42 --remove-label "loom:issue" --add-label "loom:building"
   ./.loom/scripts/worktree.sh 42
   cd .loom/worktrees/issue-42
   # ... implement feature ...
   git push -u origin feature/issue-42
   gh pr create --label "loom:review-requested"
   ```

3. **Judge** automatically finds PR #123
   - Reviews code
   - Approves and changes label: `loom:review-requested` → `loom:pr`

4. **Human** merges PR #123

### Example 2: Architectural Change

1. **Architect** analyzes codebase
   - Creates issue #44: "Proposal: Migrate to microservices architecture"
   - Adds `loom:architect` label
   - Includes detailed analysis, tradeoffs, implementation plan

2. **Human** reviews proposal
   - Discusses with team
   - Decides to proceed
   - Removes `loom:architect`, adds `loom:issue`

3. **Builder** claims and implements (same as Example 1)

### Example 3: Code Simplification

1. **Hermit** identifies complex module
   - Creates issue #45: "Simplify authentication middleware"
   - Adds `loom:hermit` label
   - Documents complexity metrics, removal plan

2. **Human** approves simplification
   - Removes `loom:hermit`, adds `loom:issue`

3. **Builder** claims and simplifies (same as Example 1)

## Best Practices

### For Autonomous Agents

1. **Always check labels first** - Don't work on issues/PRs already in progress
2. **Update labels immediately** - Signal state changes to other agents
3. **Be thorough** - Autonomous work should be high quality
4. **Fail safely** - If unsure, add `loom:blocked` and stop
5. **Document reasoning** - Explain decisions in issue/PR comments

### For Manual Agents

1. **Claim before starting** - Update labels to prevent conflicts
2. **Use worktrees** - Isolate work in `.loom/worktrees/issue-{number}`
3. **Test thoroughly** - Run full test suite before creating PR
4. **Write clear commits** - Explain what and why
5. **Link issues** - Use "Closes #N" in PR descriptions

### For Humans

1. **Review proposals promptly** - Don't block autonomous agents
2. **Merge approved PRs** - Keep the pipeline flowing
3. **Provide clear feedback** - Help agents learn and improve
4. **Adjust intervals** - Fine-tune autonomous agent timing
5. **Monitor and intervene** - Step in when agents get stuck

## Configuration

### Setting Up Autonomous Agents (post-v0.10.0)

> **The Python `loom-daemon` brain was removed in v0.10.0 and replaced by
> the Rust `loom-daemon` binary.** Historical guidance like "start the
> daemon, it manages roles on a schedule, and spawns shepherds per issue"
> no longer matches the codebase — the Python brain, the shepherd pool,
> and the `/shepherd` slash command were all deleted as part of the
> shepherd/daemon deprecation epic (#3372). The Tier 2 surface today is the
> Rust `loom-daemon` binary, which exposes MCP-level dispatch
> (`mcp__loom__dispatch_sweep`, `mcp__loom__list_sweeps`,
> `mcp__loom__cancel_sweep`, plus the event bus) for token-rotated
> `/loom:sweep` children, alongside GitHub Actions cron for the periodic
> support roles. For the breaking-change inventory and per-CLI replacement
> table, see
> [`docs/migration/v0.10.0-shepherd-deprecation.md`](migration/v0.10.0-shepherd-deprecation.md)
> and [`.loom/docs/daemon-reference.md`](../.loom/docs/daemon-reference.md).

The autonomous responsibilities the daemon used to own are now split
across two daemon-free mechanisms. They can be enabled independently.

**1. Per-issue lifecycle: `loom-daemon` dispatches `/loom:sweep <N>`**

The Rust `loom-daemon` binary is the multi-issue dispatch backend.
Operators enqueue work with `mcp__loom__dispatch_sweep --issue <N>`,
which detaches one `claude -p "/loom:sweep N"` child (with multi-account
token rotation via `spawn-claude.sh`). Each child runs the full
Curator → Builder → Judge → Doctor → Merge lifecycle for one issue and
exits. There is no shepherd pool, no `daemon-state.json`, no
work-generation cooldowns — by default the daemon does not poll the
forge; dispatch is operator-driven. (An opt-in, default-off autonomous
work finder (#3810) can poll open `loom:issue` items and auto-dispatch
sweeps when explicitly enabled.)

```bash
mcp__loom__dispatch_sweep --issue 123   # enqueue a sweep for issue 123
mcp__loom__list_sweeps                  # enumerate running sweeps
mcp__loom__get_sweep_status --sweep_id <id>
mcp__loom__cancel_sweep --sweep_id <id> # SIGTERM → grace → SIGKILL
```

Per-sweep logs live at `.loom/logs/sweep-issue-<N>.log` (tailable via
`mcp__loom__tail_sweep_log`). Sweep checkpoints
(`.loom/sweep-checkpoint/issue-<N>.json`) survive crashes — a re-dispatched
sweep resumes from the last completed phase. (The v0.9.x `spawn-loop.sh`
launcher and its `.loom/spawn-loop-state.json` state file were removed in
v0.11.0.)

**2. Periodic support roles: GitHub Actions cron**

The periodic roles the old daemon ran in-process (Judge, Curator,
Champion, Auditor, Guide) are now GitHub Actions workflows under
`.github/workflows/loom-*.yml`. Each workflow checks out the repo,
installs the Claude CLI, and runs `claude -p "/<role>"
--dangerously-skip-permissions` for one tick of work — no Loom-side
state file, no long-running process. Cron schedules approximate the
daemon's historical intervals:

| Workflow            | Role        | Schedule (commented) |
|---------------------|-------------|----------------------|
| `loom-judge.yml`    | `/judge`    | `*/5 * * * *`        |
| `loom-curator.yml`  | `/curator`  | `*/5 * * * *`        |
| `loom-champion.yml` | `/champion` | `*/10 * * * *`       |
| `loom-auditor.yml`  | `/auditor`  | `*/10 * * * *`       |
| `loom-guide.yml`    | `/guide`    | `*/15 * * * *`       |

Workflows ship with `schedule:` blocks **commented out** so forks
don't accidentally burn Actions minutes. To opt in: add a
`CLAUDE_API_KEY` repository secret, uncomment the `schedule:` /
`- cron:` lines in each workflow you want to enable, and optionally
smoke-test via the Actions UI's **Run workflow** button.

Architect and Hermit cadence (work-generation triggers) is
intentionally out of scope for now — see follow-up #3381.

### Setting Up Manual Orchestration Mode

```bash
# Terminal 1: Judge
/judge

# Terminal 2: Curator
/curator

# Terminal 3: Architect
/architect

# Terminal 4: Builder (you)
/builder
```

## Troubleshooting

### Labels Not Updating

```bash
# Check repository labels
gh label list

# Re-sync labels
gh label sync --file .github/labels.yml
```

### Conflicting Work

If two agents claim the same issue:
```bash
# Check who claimed first
gh issue view 42 --json timeline

# Yield if you claimed second
gh issue edit 42 --remove-label "loom:building"
```

### Stale Labels

Guide role should clean these up automatically, but you can do it manually:
```bash
# Find stale in-progress issues (no activity for 7 days)
gh issue list --label "loom:building" --json number,updatedAt

# Remove stale label
gh issue edit 42 --remove-label "loom:building"
```

## Resources

- **Main Documentation**: `.loom/CLAUDE.md` (comprehensive usage guide)
- **Role Definitions**: `.loom/roles/*.md` (detailed role guidelines)
- **Loom Repository**: https://github.com/rjwalters/loom

---

**Generated by Loom Installation Process**
Last updated: {{INSTALL_DATE}}
