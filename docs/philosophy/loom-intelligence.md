# Loom Intelligence: The Learning IDE

*A vision for transforming agent activity data into continuous learning*

---

## Vision

Loom is not just a tool for running AI agents - it's a **learning system that gets smarter over time**. By analyzing agent activity, codebase evolution, and resource consumption, we can answer questions that fundamentally improve how software is built:

- Which approaches actually work?
- What's the true cost of different strategies?
- How do we get faster and better over time?
- Can the system teach itself what patterns succeed?

This document defines the **product vision** for transforming raw agent data into actionable intelligence.

---

## The Strategic Questions

These are the questions that, once answered, transform Loom from "AI terminals" into "a learning IDE":

### 🎯 Agent Effectiveness

**Which agents/roles are most effective?**
- Success rate by agent role (Builder, Judge, Architect, etc.)
- Time-to-completion by role
- Quality metrics (tests passing, PR approval rate, rework frequency)
- Specialization patterns (which roles excel at which task types?)

**Which prompts lead to the best outcomes?**
- Correlation: prompt patterns → PR merge speed
- Correlation: prompt patterns → test pass rates
- Correlation: prompt patterns → human approval rates
- Anti-patterns: prompts that consistently fail or require rework

**How many prompts does it take?**
- Prompts per feature (is it trending down over time?)
- Prompts per bug fix
- Prompts per PR
- Iteration depth (how many tries before success?)

### 💰 Resource Efficiency

**What's the true cost of development?**
- Token consumption per feature
- API costs per PR
- Cost per line of code
- Cost per issue resolved
- Budget burn rate and runway

**Which approaches are cost-effective?**
- Manual vs autonomous mode efficiency
- Long-form prompts vs iterative dialog
- Different models (Sonnet vs Opus) for different tasks
- Role specialization ROI

### 📈 Velocity & Trends

**Are we getting faster?**
- Issue resolution time (trending?)
- PR cycle time (claim → merge)
- Time in each workflow state
- Velocity by week/month

**What's our actual throughput?**
- Features shipped per week
- Bugs fixed per week
- Lines of code per day
- Issues opened vs closed rate

### 🔍 Quality & Patterns

**What correlates with quality?**
- Test coverage vs PR approval rate
- Prompt length vs success rate
- Review cycles vs final merge
- Time spent vs quality outcome

**What patterns predict failure?**
- Warning signs before failed builds
- Patterns in stuck PRs
- Common causes of rework
- When to switch agent roles

### 🧠 Learning & Improvement

**Can the system improve itself?**
- Identify successful prompt templates automatically
- Suggest better prompts based on historical success
- Detect when an agent is stuck (intervention signal)
- Auto-tune autonomous intervals based on effectiveness
- Recommend role assignments based on task type

---

## Product Features (User-Facing)

### Feature 1: Agent Dashboard
**"Show me what's happening"**

```
┌─────────────────────────────────────────────┐
│  Loom Intelligence Dashboard                │
├─────────────────────────────────────────────┤
│                                             │
│  📊 Today's Activity                        │
│  ├─ 6 active agents                         │
│  ├─ 47 prompts sent                         │
│  ├─ 3 features shipped                      │
│  ├─ $12.47 spent                            │
│  └─ 38k tokens used                         │
│                                             │
│  🎯 Agent Performance                       │
│  ├─ Builder: 85% success (27/32 prompts)   │
│  ├─ Judge: 100% success (10/10 reviews)    │
│  └─ Architect: 60% approved (3/5 proposals)│
│                                             │
│  📈 This Week vs Last Week                  │
│  ├─ Features: 12 (+3) ↗                    │
│  ├─ Cycle time: 4.2hrs (-0.8hrs) ↘         │
│  └─ Cost/feature: $42 (-$8) ↘              │
│                                             │
└─────────────────────────────────────────────┘
```

### Feature 2: Prompt Library
**"Learn from what worked"**

- Browse successful prompts by category
- Filter by agent role, success rate, task type
- "Save as template" for reuse
- Auto-suggest similar successful prompts
- Show success metrics per template

```
Top Builder Prompts (Last 30 Days)

1. "Find loom:issue, create worktree, implement"
   ✅ 92% success | ⚡ 3.2hrs avg | 💰 $4.20 avg
   Used: 23 times | Last: 2 hours ago
   [Use This Template]

2. "Review PR #X, address all feedback"
   ✅ 88% success | ⚡ 1.8hrs avg | 💰 $2.10 avg
   Used: 15 times | Last: 1 day ago
   [Use This Template]
```

### Feature 3: Cost Analytics
**"Where's the money going?"**

- Real-time budget tracking
- Cost breakdown by agent, task, time period
- Projections and runway calculations
- Cost-effectiveness comparisons
- Alerts when approaching budget limits

```
Monthly Budget: $500 | Used: $347 (69%)

Cost Breakdown:
├─ Builder role: $245 (71%)
├─ Judge role: $52 (15%)
├─ Architect: $38 (11%)
└─ Other: $12 (3%)

Most Expensive Feature: "Multi-workspace support"
├─ 127 prompts over 3 days
├─ $87 total cost
└─ ⚠️ Above average - may need prompt optimization

Runway: 12 days at current burn rate
```

### Feature 4: Learning Insights
**"Make me smarter"**

- Weekly intelligence reports
- Pattern detection and recommendations
- "Did you know?" insights
- Performance trends and predictions
- Anomaly detection

```
💡 Weekly Insights

✨ Best Pattern This Week
"Starting with tests" increased success rate by 34%
→ Consider test-first for new features

⚠️ Warning Sign Detected
3 PRs stuck in review for >48hrs
→ Possible reviewer bottleneck or unclear specs

📈 Improvement Detected
Average prompts-per-feature down from 8.2 to 6.4
→ Prompt quality improving! Keep it up.

🎯 Recommendation
Architect proposals have 85% rejection rate on Fridays
→ Consider scheduling brainstorming for Monday/Tuesday
```

### Feature 5: Playback & Replay
**"Show me what happened"**

- Timeline visualization of agent activity
- Replay prompt sequences for a feature/bug
- See the full context of a task
- Compare successful vs failed attempts
- Share/export task histories

```
Feature #42: "Add dark mode toggle"
Timeline: 2024-10-18 to 2024-10-20 (2.3 days)

├─ [Builder-1] 2:15 PM - Create worktree
│  ⚡ 15 seconds | ✅ Success
├─ [Builder-1] 2:16 PM - Implement toggle component
│  ⚡ 4.2 min | ✅ Success | 47 lines changed
├─ [Builder-1] 2:23 PM - Run tests
│  ⚡ 23 seconds | ❌ Failed (3 tests)
├─ [Healer-1] 2:25 PM - Fix failing tests
│  ⚡ 3.1 min | ✅ Success | 12 lines changed
├─ [Builder-1] 2:31 PM - Create PR
│  ⚡ 18 seconds | ✅ Success
├─ [Judge-2] 4:42 PM - Review PR
│  ⚡ 2.1 min | 🔄 Request changes (CSS issues)
├─ [Healer-1] 10:05 AM - Address review feedback
│  ⚡ 5.3 min | ✅ Success | 8 lines changed
└─ [Judge-2] 10:47 AM - Approve PR
   ⚡ 1.2 min | ✅ Approved

Total: 7 prompts | 16.2 active minutes | $4.35 cost
Outcome: ✅ Merged in 2.3 days
```

### Feature 6: Comparative Analysis
**"What's working better?"**

- A/B test different approaches
- Compare agent configurations
- Benchmark against historical baseline
- Model comparison (Sonnet vs Opus)
- Role effectiveness comparisons

```
Experiment: Manual vs Autonomous Builder Mode
Duration: 2 weeks | Sample: 20 features each

┌─────────────────────┬─────────────┬──────────────┐
│ Metric              │ Manual      │ Autonomous   │
├─────────────────────┼─────────────┼──────────────┤
│ Avg Time to PR      │ 3.2 hours   │ 4.8 hours    │
│ Success Rate        │ 95%         │ 78%          │
│ Cost per Feature    │ $8.20       │ $6.10        │
│ Human Intervention  │ High        │ Low          │
│ Test Pass Rate      │ 92%         │ 84%          │
└─────────────────────┴─────────────┴──────────────┘

Recommendation: Use manual mode for critical features,
autonomous for routine tasks and maintenance.
```

---

## Data Requirements

To answer these questions, we need to collect and correlate:

### Already Tracking (Issue #174 ✅)
- Terminal inputs (prompts)
- Terminal outputs (results)
- Timestamps
- Agent roles
- Workspace/branch context

### Need to Add

**GitHub Correlation:**
- Link prompts → specific commits
- Link prompts → specific PRs
- Link prompts → specific issues
- Track label transitions with timestamps
- Track PR review cycles (requested → changes → approved)

**Codebase Metrics:**
- Lines added/removed per prompt
- Files changed per prompt
- Test coverage deltas
- Build/test pass rates
- Commit frequency

**Resource Tracking:**
- LLM API tokens per prompt
- Model used (Sonnet, Opus, etc.)
- Cost calculations
- Session duration
- Idle vs active time

**Quality Metrics:**
- Test pass/fail outcomes
- Lint/format check results
- PR approval/rejection
- Time in review
- Rework frequency

**Derived Metrics:**
- Success rate (by role, by prompt pattern, by time of day)
- Cost efficiency (tokens per feature, cost per PR)
- Velocity trends (features per week, cycle time)
- Prompt effectiveness scores

---

## Database Schema Extensions

Building on #174's foundation:

```sql
-- Link prompts to codebase changes
CREATE TABLE prompt_changes (
  id INTEGER PRIMARY KEY,
  input_id INTEGER REFERENCES agent_inputs(id),
  commit_hash TEXT,
  files_changed INTEGER,
  lines_added INTEGER,
  lines_removed INTEGER,
  tests_added INTEGER,
  tests_modified INTEGER
);

-- Link prompts to GitHub entities
CREATE TABLE prompt_github (
  id INTEGER PRIMARY KEY,
  input_id INTEGER REFERENCES agent_inputs(id),
  issue_number INTEGER,
  pr_number INTEGER,
  label_before TEXT,
  label_after TEXT,
  event_type TEXT -- 'issue_created', 'pr_created', 'pr_merged', etc.
);

-- Track resource consumption
CREATE TABLE resource_usage (
  id INTEGER PRIMARY KEY,
  input_id INTEGER REFERENCES agent_inputs(id),
  model TEXT NOT NULL,
  tokens_input INTEGER,
  tokens_output INTEGER,
  cost_usd REAL,
  duration_seconds INTEGER
);

-- Track quality outcomes
CREATE TABLE quality_metrics (
  id INTEGER PRIMARY KEY,
  input_id INTEGER REFERENCES agent_inputs(id),
  tests_passed INTEGER,
  tests_failed INTEGER,
  lint_errors INTEGER,
  format_errors INTEGER,
  pr_approved BOOLEAN,
  pr_changes_requested BOOLEAN,
  human_rating INTEGER -- 1-5 stars, optional
);

-- Materialized views for fast queries
CREATE VIEW agent_effectiveness AS
SELECT
  agent_role,
  COUNT(*) as total_prompts,
  SUM(CASE WHEN q.tests_passed > 0 THEN 1 ELSE 0 END) as successful_prompts,
  AVG(r.cost_usd) as avg_cost,
  AVG(r.duration_seconds) as avg_duration
FROM agent_inputs i
LEFT JOIN quality_metrics q ON i.id = q.input_id
LEFT JOIN resource_usage r ON i.id = r.input_id
GROUP BY agent_role;
```

---

## Technical Architecture

### Data Collection Points

**1. Daemon (`loom-daemon`):**
- Hook `send_input` → record prompts ✅ (already done)
- Hook output polling → record results ✅ (already done)
- Add: Capture git diff after each prompt
- Add: Track LLM API metadata (tokens, model, cost)
- Add: Measure session timing

**2. Frontend (`src/lib/`):**
- Track UI events (user actions)
- Capture human ratings/feedback
- Record manual interventions
- Log agent state transitions

**3. GitHub Integration:**
- Webhook listener for PR/issue events
- Correlate events with active terminals
- Track label transitions
- Measure PR cycle times

**4. CI/CD Integration:**
- Report test results back to database
- Track build success/failure
- Capture lint/format check results
- Link to originating prompts

### Analytics Engine

**Query Layer:**
- SQL views for common metrics
- Caching for expensive aggregations
- Real-time vs batch processing
- Export to CSV/JSON

**Intelligence Layer:**
- Pattern detection algorithms
- Anomaly detection
- Success prediction models
- Prompt template extraction
- Recommendation engine

---

## Privacy & Ethics

### Sensitive Data Handling

**What could be sensitive:**
- Code that reveals business logic
- API keys/secrets in outputs
- Personal information in comments
- Proprietary algorithms

**Mitigations:**
- Local-only by default (`.loom/activity.db` never leaves machine)
- Regex-based redaction for known secret patterns
- Opt-in for any external sharing/export
- Sanitization tools before export
- Clear documentation of what's tracked

### User Control

- **Opt-out capability:** Disable tracking per workspace
- **Granular controls:** Choose what to track (inputs only? outputs too?)
- **Data retention:** Auto-delete after N days (configurable)
- **Export/delete:** User owns all data, can export or purge anytime

### Ethical Considerations

**Transparency:**
- Document exactly what's tracked
- Make data inspection easy (SQLite browser, UI)
- No hidden metrics or tracking

**Consent:**
- Installation process explains tracking
- Checkbox during setup (not buried in TOS)
- Easy to disable post-installation

**Security:**
- Database encrypted at rest (optional)
- No phone-home by default
- External APIs require explicit opt-in

---

## Implementation Roadmap

### Phase 1: Foundation (Current)
- ✅ Basic input/output tracking (#174 complete)
- 🔄 Terminal activity visualization (#177 in progress)
- 📋 Enhanced installation (#442)
- 📋 Offline mode (#443)

### Phase 2: Correlation & Context
- Link prompts to git commits
- Link prompts to GitHub issues/PRs
- Track test outcomes
- Capture resource usage (tokens, cost)
- Basic success metrics

### Phase 3: Intelligence & Learning
- Prompt pattern extraction
- Success correlation analysis
- Cost analytics
- Velocity tracking
- Simple recommendations

### Phase 4: Advanced Analytics
- Predictive models (success prediction)
- Automated prompt optimization
- A/B testing framework
- Comparative analysis tools
- External API for data access

### Phase 5: Autonomous Learning
- Agents read their own metrics
- Self-tuning based on effectiveness
- Auto-generated prompt templates
- Intervention detection (when to ask human)
- Continuous improvement loops

---

## Success Criteria

We'll know this is working when:

### Short-term (6 months)
- [ ] Can answer: "Which agent role is most cost-effective?"
- [ ] Can answer: "What's my average cost per feature?"
- [ ] Can export all activity data for analysis
- [ ] Dashboard shows real-time agent status
- [ ] Prompt library has >20 successful templates

### Medium-term (12 months)
- [ ] System suggests better prompts based on history
- [ ] Can predict which tasks will succeed/fail
- [ ] Cost per feature trending down 30%
- [ ] Prompts per feature trending down 20%
- [ ] Automated weekly intelligence reports

### Long-term (24 months)
- [ ] Agents autonomously improve their prompts
- [ ] System detects when agent is stuck and intervenes
- [ ] External researchers using Loom data for AI research
- [ ] Community sharing successful prompt patterns
- [ ] Loom becomes demonstrably faster/cheaper over time

---

## Related Work

### Existing Research
- **OpenAI's agent benchmarking** - SWE-bench, HumanEval
- **Devin AI** - Agent development environment
- **Cursor analytics** - Code completion effectiveness
- **GitHub Copilot metrics** - Acceptance rates, productivity

### What Makes Loom Different
- **Multi-agent orchestration** vs single-agent coding
- **Full workflow tracking** vs just code completion
- **Learning system** vs static tool
- **Role specialization** vs general-purpose agent
- **Local-first privacy** vs cloud telemetry

---

## Open Questions

1. **Model integration:** Should we use an LLM to analyze the data?
   - Could Claude analyze its own effectiveness
   - Generate insights automatically
   - Suggest prompt improvements

2. **Community features:** Should successful patterns be shareable?
   - Public prompt library (opt-in)
   - Anonymized benchmarks
   - Best practices from aggregate data

3. **Pricing model:** How does this affect Loom's business model?
   - Free tier with basic analytics
   - Pro tier with advanced intelligence
   - Enterprise with custom models

4. **Research opportunities:** Can this data advance AI research?
   - Study how agents learn and improve
   - Identify bottlenecks in AI coding
   - Publish anonymized findings

5. **Competitive advantage:** Is this data a moat?
   - More usage → better insights → better tool → more usage
   - Network effects from community patterns
   - Proprietary prompt optimization

---

## The North Star

This vision can be summarized in a single principle:

> **Loom should be an IDE that learns.**

Every prompt is training data. Every success teaches the system. Every failure makes it smarter. Over time, Loom doesn't just help you write code—it helps you write code *better*.

The foundation is already in place (#174). Now we build the intelligence layer.

---

## Alignment with Loom's Philosophy

From [working-with-ai.md](working-with-ai.md):

> "To get humans out of the debug loop, you need to create a surface that AI agents can read directly."

Loom Intelligence IS that machine-readable surface. When agents can:
- Read their own effectiveness metrics
- Learn which patterns succeed
- Self-correct based on outcomes
- Continuously improve their prompts

We achieve the transformation described in our philosophy:

> Software development is transitioning from: "Can I write this code fast enough?" To: "Can I specify what I actually want clearly enough?"

Intelligence closes the loop. Agents not only execute—they learn, adapt, and improve. The craft evolves from writing code to orchestrating an ever-improving system.

---

**See Also:**
- [Agent Archetypes](agent-archetypes.md) - The roles that generate the data
- [Working with AI](working-with-ai.md) - The philosophy that demands this vision
- [Issue #444](https://github.com/rjwalters/loom/issues/444) - Implementation tracking
- [Issue #174](https://github.com/rjwalters/loom/issues/174) - Database foundation
- [Issue #177](https://github.com/rjwalters/loom/issues/177) - Activity visualization
