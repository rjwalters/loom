# Working with AI: Insights from Building Loom

*Meta-reflections on the development process*

---

## The Touch Typing Transition

When you first learn touch typing, you're slower than hunt-and-peck. Your fingers don't know where the keys are. You watch your speed drop from 40 WPM to 15 WPM and wonder if you're going backwards.

But you're not—you're building muscle memory that will eventually let you type 80+ WPM without looking.

**Cut-and-paste context control is hunt-and-peck typing.** It's slow and manual:
- Carefully select which context to include
- Explicitly paste file contents
- Think about what the model needs to see
- Manually manage the conversation flow

But this friction is *teaching you*:
- What context actually matters
- How to state problems clearly
- How to architect prompts that work
- The difference between vague and precise

**Writing effective prompts at the right level of abstraction is touch typing.**

Once you internalize what the model needs and how to communicate intent clearly, you work much faster—automatically, like touch typing. You'll know instinctively what to ask for and how.

The slowness is temporary. The skill is permanent.

---

## The Spectrum of Precision

**Claude is a very good programmer** and can handle almost anything if you state the problem clearly.

Here's a thought experiment: You could instruct an LLM to write code character-by-character:
```
"Write the letter 'f'"
"Now write 'u'"
"Now write 'n'..."
```

This would work. But it would be absurdly inefficient and miss the entire point.

**There's a continuum of instruction quality:**

```
Too Vague                    Sweet Spot              Uselessly Precise
│                                 │                            │
"Make it better"          "Add a button that           "Write char 'f',
"Fix the UI"              saves the current             then char 'u',
"Improve performance"     state to localStorage         then char 'n'..."
                          and shows a toast
                          confirmation"
```

The programmer's new job is to **find the right middle ground**:
- Describe *what* you want and *why* (intent + constraints)
- Leave the *how* flexible (implementation details)
- Be precise about requirements, vague about syntax

*Note: CSS and visual design remain hard because you need to see it to judge it. This isn't an LLM limitation—natural language is simply imprecise for spatial relationships and aesthetics.*

---

## Documentation as Memory

When working with AI, **documentation stops being an afterthought and becomes the primary artifact.**

Here's what actually happens when building with Claude Code:
- 30-50% of the lines you write are documentation
- You update docs constantly, not at the end
- The README becomes the AI's working memory
- CLAUDE.md becomes instructions to your future self

**Because the AI doesn't remember your last session.**

Every new conversation is a blank slate. The only way to preserve context, decisions, and intent across sessions is to write it down:

```markdown
# API Design Decisions

We use POST for all mutations (not PATCH) because GraphQL conventions...
Auth tokens go in headers, not query params, because security scanners...
Error codes follow RFC 7807 because clients need machine-readable errors...
```

This isn't documentation for documentation's sake. It's **externalizing your working memory** so the next AI session (tomorrow's conversation, next week's bug fix) can pick up exactly where you left off.

**The documents become you.**

When Claude reads `CLAUDE.md`, it's reading your voice, your decisions, your architectural intent. The AI doesn't remember you, but it remembers what you wrote. The documentation literally becomes the continuity of identity across sessions.

**The shift:**
- Old world: Code is primary, docs are secondary
- New world: **Docs ARE executable instructions**—they tell the AI what to build and why

You're not writing comments for humans anymore. You're writing memory for machines.

And weirdly, this makes codebases *more* maintainable for humans too. Because clear intent expressed in natural language beats clever code every time.

---

## Let Claude Write All the Code

Controversial take: **Code written by the same model has semantic consistency.**

When Claude writes function A and function B:
- They use similar patterns
- They follow consistent conventions
- They make compatible assumptions
- They "feel" like they belong together

When a human writes function A and Claude writes function B:
- Naming conventions might clash
- Architectural patterns might diverge  
- Implicit assumptions might conflict
- Integration takes extra effort

**Your energy is better spent:**
- Creating GitHub issues that define *what* to build
- Reviewing pull requests to verify it solves the actual problem
- Providing feedback on architectural direction
- Deciding what's worth building in the first place

Not writing the implementation yourself.

This doesn't mean humans can't code anymore. It means the bottleneck has shifted from **"writing code quickly"** to **"specifying the right thing to build clearly."**

Which is what it always should have been.

---

## The Machine-Readable Debugging Surface

**This is the most important insight:**

To get humans out of the debug loop, you need to create a surface that AI agents can read directly.

### The Old Loop (Human-in-the-Middle)
1. Human observes bug (clicks button, nothing happens)
2. Human describes bug in natural language: "When I click the save button, it doesn't seem to work"
3. Claude asks: "What browser? Any console errors? What state was the app in?"
4. Human checks: "Chrome, let me look... there's an error about localStorage"
5. Claude asks: "What's the exact error message?"
6. Human copies it over...

This loop is **slow, lossy, and requires constant human attention.**

### The New Loop (Machine-Readable)
1. Agent reads console logs directly: `"[ERROR] localStorage.setItem failed: QuotaExceededError"`
2. Agent reads test output: `"Test 'should_save_state' failed at line 42"`
3. Agent reads instrumentation: `"Button onClick fired → saveState() → localStorage full"`
4. Agent fixes the issue and verifies with tests
5. Agent creates PR with the fix

This loop is **fast, precise, and autonomous.**

### How to Build This

**1. Instrument everything with programmatic hooks:**
```typescript
// Every button, every event, every state transition
button.addEventListener('click', () => {
  console.log('[UI] Save button clicked', { userId, timestamp });
  saveState();
});
```

**2. Write tests that verify behavior programmatically:**
```rust
#[test]
fn test_terminal_create() {
    // Not "does it look right?"
    // But "does it behave correctly?"
    assert_eq!(terminal.id, expected_id);
    assert!(terminal.is_connected());
}
```

**3. Add linting and formatting:**
- Enforces consistency without human review
- Catches issues before they become bugs
- Creates a single source of truth for style

**4. Use MCP (Model Context Protocol) to expose logs and state:**
```bash
# Agent can now programmatically check state
mcp__loom-ui__read_console_log({ lines: 100 })
mcp__loom-terminals__list_terminals()
mcp__loom-logs__read_daemon_log()
```

**When Claude writes the code and can read its own instrumentation:**
- The feedback loop closes
- Agents autonomously verify their work
- Test failures are unambiguous
- Humans move from "tester/describer" to "architect/goal-setter"

---

## Loom Embodies These Principles

Loom isn't just built *with* these insights—it's built *for* them:

- **GitHub as the orchestration protocol**: Labels and issues are the machine-readable surface
- **Autonomous terminals with roles**: Each agent has a clear archetype and specialty
- **MCP servers for instrumentation**: UI state, logs, and terminals are all programmatically accessible
- **Tests, linting, CI**: Quality gates that agents can verify without humans
- **Proposals require human approval**: Keeps humans in the strategic loop

The goal: **Create a system where humans define intent and agents autonomously implement, test, and verify their own work.**

---

## The Real Shift

Software development is transitioning from:

**"Can I write this code fast enough?"**

To:

**"Can I specify what I actually want clearly enough?"**

The craft isn't disappearing. It's evolving. We're becoming **architects of intent** rather than **typists of syntax**.

And that's exactly where human creativity belongs.
