# ADR-0006: Label-Based Workflow Coordination

## Status

Accepted

## Context

Loom orchestrates multiple AI agent terminals with different roles (Architect, Curator, Reviewer, Worker). These agents need to:

- Coordinate work without central orchestration
- Know what tasks are available
- Avoid conflicts and duplicate work
- Track work status through the pipeline

Traditional approaches (database, API, message queue) add complexity and infrastructure. GitHub already provides issues and PRs with labels - could this be leveraged?

## Decision

Use **GitHub labels as a state machine** to coordinate agent workflows:

**Label State Machine**:
1. `loom:architect-suggestion` → Issue created by Architect (requires user approval)
2. (No label) → Unlabeled issues ready for Curator enhancement
3. `loom:curated` → Curator-enhanced, awaiting human approval
4. `loom:issue` → Human-approved, ready for Worker to claim
5. `loom:in-progress` → Worker actively implementing
6. `loom:review-requested` → PR ready for Reviewer
7. `loom:approved` → Reviewer approved, ready to merge
8. `loom:blocked` → Work blocked on dependency

**Workflow**:
- **Architect**: Creates issues with `loom:architect-suggestion`
- **User**: Reviews suggestions, removes label to approve for curation
- **Curator**: Finds unlabeled issues, enhances, marks `loom:curated`
- **User**: Reviews curated issues, adds `loom:issue` to explicitly approve work
- **Worker**: Claims `loom:issue`, implements, creates PR with `loom:review-requested`
- **Reviewer**: Finds `loom:review-requested`, reviews, marks `loom:approved`
- **User**: Merges approved PRs

## Consequences

### Positive

- **No infrastructure**: Uses existing GitHub labels (no database, API, or queue)
- **Visible state**: Anyone can see work status via GitHub UI or `gh` CLI
- **Decentralized**: Agents coordinate through labels, no central orchestrator
- **Auditable**: Label history shows state transitions
- **Familiar**: GitHub labels are well-understood by developers
- **Flexible**: Easy to add new states with new labels
- **Searchable**: `gh issue list --label="loom:issue"` finds work instantly
- **Human approval gate**: Explicit `loom:issue` label prevents automatic work without oversight

### Negative

- **Rate limits**: GitHub API has rate limits (5000/hour authenticated)
- **Network dependency**: Requires GitHub connectivity
- **Label conflicts**: Multiple agents could race for same issue
- **No transactions**: Can't atomically claim an issue
- **Manual cleanup**: Stale labels if agent crashes mid-work
- **Namespace pollution**: Adds many `loom:*` labels to repository

## Alternatives Considered

### 1. Database (SQLite, PostgreSQL)

**Pros**:
- Transactional guarantees
- Fast queries
- No rate limits

**Rejected because**:
- Extra infrastructure to manage
- State hidden from GitHub UI
- Requires database schema migrations
- Complicates deployment and testing

### 2. Message Queue (Redis, RabbitMQ)

**Pros**:
- True queue semantics (FIFO)
- Atomic claim operations
- High throughput

**Rejected because**:
- Heavy infrastructure (Redis server, config)
- State not visible in GitHub
- Overkill for our use case
- Complicates local development

### 3. GitHub Projects API

**Pros**:
- Native GitHub feature
- Kanban board visualization
- Automation rules

**Rejected because**:
- More complex API than labels
- Not as scriptable via `gh` CLI
- Slower to query than labels
- Projects require manual board setup

### 4. File-Based Queue (.loom/queue.json)

**Pros**:
- No external dependencies
- Fast local access
- Full control

**Rejected because**:
- Not visible in GitHub UI
- Requires git commits to share state
- Git conflicts between agents
- No cross-repository coordination

## Implementation Details

**Finding Work** (Worker role):
```bash
# List issues oldest-first (FIFO queue)
gh issue list --label="loom:issue" --state=open --limit=10

# Claim oldest issue
gh issue edit <number> --remove-label "loom:issue" --add-label "loom:in-progress"
```

**Creating PRs** (Worker role):
```bash
gh pr create --label "loom:review-requested"
```

**Finding Reviews** (Reviewer role):
```bash
gh pr list --label="loom:review-requested" --state=open
```

**Label Cleanup** (Health Monitor):
```bash
# Reset labels on workspace restart
gh issue list --label="loom:in-progress" --state=open --json number | \
  xargs -I {} gh issue edit {} --remove-label "loom:in-progress"
```

## Race Condition Handling

Label updates are **not atomic**. If two Workers try to claim the same issue simultaneously:

1. Both check: Issue has `loom:issue`
2. Both execute: `gh issue edit <N> --add-label "loom:in-progress"`
3. **Result**: Both think they claimed it

**Mitigation**:
- Workers fetch issue state again after claiming
- If multiple `loom:in-progress` labels detected, Worker backs off
- Use `loom:blocked` label to mark conflicts
- Future: Add optimistic locking with label timestamps

## References

- Implementation: `defaults/roles/builder.md`, `defaults/roles/curator.md`, `defaults/roles/judge.md`
- Related: ADR-0008 (tmux + Daemon Architecture)
- Related: WORKFLOWS.md (detailed workflow documentation)
- GitHub Labels API: https://docs.github.com/en/rest/issues/labels
- `gh` CLI: https://cli.github.com/manual/gh_issue
