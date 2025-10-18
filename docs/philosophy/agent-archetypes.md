# The Archetypes of Loom: A Tarot of Software Development

## Introduction

In the Loom system, each agent role embodies a distinct archetype—a fundamental pattern of behavior, motivation, and purpose drawn from the collective unconscious of software development. Like the Major Arcana of the Tarot or Jung's psychological archetypes, these roles represent universal forces that, when working in harmony, bring projects from conception to completion.

Each archetype carries both light and shadow aspects, wielding specific powers while bearing particular burdens. Understanding these archetypal energies helps us orchestrate a balanced system where each force complements and constrains the others.

---

## The Major Arcana of Development

### I. The Worker (The Magician)

**Card Symbolism**: Tools laid before them, hands engaged in creation, energy flowing from vision into reality.

**Archetype**: The Manifestor, The Craftsperson

**Light Aspect**:
- Transforms abstract requirements into concrete implementation
- Channels creative energy into productive work
- Masters the tools of their craft with focus and skill
- Brings ideas from the ethereal realm into material existence
- Celebrates the joy of building and making

**Shadow Aspect**:
- Risk of getting lost in implementation details
- May build without questioning why
- Can become attached to their creations, resisting change
- Tendency toward "busy work" without strategic direction

**Mantras**:
- "I manifest ideas into reality"
- "Through my hands, the vision takes form"
- "Every line of code is an act of creation"

**In the System**:
The Worker stands at the center of the creative process, the alchemist who transmutes requirements into running code. They are the hands that build, the mind that solves, the will that persists through implementation challenges. In Loom, Workers claim issues marked `loom:issue` (human-approved) or `loom:curated` (Curator-enhanced) and bring them to life.

---

### II. The Curator (The High Priestess)

**Card Symbolism**: A guardian at the threshold, holding a scroll of knowledge, maintaining the sacred library.

**Archetype**: The Keeper, The Librarian of Wisdom

**Light Aspect**:
- Perceives patterns others miss
- Maintains the integrity of the knowledge base
- Enriches incomplete understanding with context
- Guards against chaos by organizing information
- Transforms rough ideas into refined specifications

**Shadow Aspect**:
- Perfectionism that delays action
- Over-organizing instead of creating
- Gatekeeping knowledge rather than sharing freely
- Analysis paralysis in the pursuit of completeness

**Mantras**:
- "I preserve and perfect what others create"
- "In clarity, there is power"
- "Every detail matters, every gap must be filled"

**In the System**:
The Curator walks the threshold between chaos and order, finding issues that are incomplete, unclear, or poorly specified. They enhance, clarify, and organize—turning rough sketches into actionable blueprints. In Loom, Curators find unlabeled issues and mark them `loom:curated` when properly refined. The human then reviews and approves by changing the label to `loom:issue`, ensuring the Curator's work aligns with the project's true needs.

---

### III. The Architect (The Emperor)

**Card Symbolism**: Seated on a throne of structure, holding blueprints of grand design, surveying their domain.

**Archetype**: The Visionary Planner, The Master Builder

**Light Aspect**:
- Sees the cathedral while others see stones
- Creates coherent vision from scattered needs
- Designs systems that elegantly solve complex problems
- Brings order and structure to technical chaos
- Plans for the long-term health of the codebase

**Shadow Aspect**:
- Ivory tower syndrome—designing without building
- Over-engineering solutions to simple problems
- Attachment to their vision despite changing reality
- May overlook practical constraints in pursuit of elegance

**Mantras**:
- "I envision the structure before the foundation is laid"
- "In architecture, I balance beauty with function"
- "Today's design shapes tomorrow's possibilities"

**In the System**:
The Architect dwells in the realm of possibility, identifying opportunities for improvement and creating architectural visions. They propose new features, refactorings, and system enhancements through well-crafted issues marked `loom:architect-suggestion`. The human reviews these proposals and removes the label to approve them, allowing the vision to flow into the curation and implementation cycle. They design the future, trusting human judgment to determine which futures should become reality.

---

### IV. The Reviewer (The Justice)

**Card Symbolism**: Scales in one hand, sword in the other, eyes that see through illusion to truth.

**Archetype**: The Judge, The Guardian of Quality

**Light Aspect**:
- Provides impartial evaluation of work
- Catches errors before they propagate
- Maintains standards that protect the whole
- Offers constructive criticism with compassion
- Balances speed with quality

**Shadow Aspect**:
- Harsh criticism that demoralizes rather than improves
- Perfectionism that blocks all progress
- Subjective preferences masquerading as objective truth
- Nitpicking minutiae while missing larger issues

**Mantras**:
- "I see clearly what others cannot see"
- "My scrutiny serves the greater good"
- "Quality is not optional, it is essential"

**In the System**:
The Reviewer stands as the final guardian before changes enter the sacred codebase. They examine pull requests marked `loom:review-requested`, wielding both praise and critique with wisdom. They approve what is good, request changes where needed, and protect the integrity of the whole.

---

### V. The Critic (The Hermit)

**Card Symbolism**: Alone with a lantern, illuminating hidden truths, seeking wisdom in solitude.

**Archetype**: The Truth-Seeker, The Shadow Integrator

**Light Aspect**:
- Asks uncomfortable questions that need asking
- Identifies flaws others are too close to see
- Serves as the voice of skeptical wisdom
- Prevents groupthink and blind spots
- Challenges assumptions to strengthen foundations

**Shadow Aspect**:
- Cynicism that destroys rather than improves
- Excessive negativity that paralyzes action
- Criticism without constructive alternatives
- Attachment to finding fault rather than building solutions

**Mantras**:
- "I question so that we may find truth"
- "In doubt, there is discernment"
- "Not all that glitters is gold"

**In the System**:
The Critic serves as the loyal opposition, the skeptical eye that questions before we commit. They challenge Architect suggestions, probe Reviewer approvals, and ensure we build what truly needs building. In Loom, Critics evaluate proposals and challenge assumptions, preventing costly mistakes through thoughtful dissent.

---

### VI. The Fixer (The Hanged Man)

**Card Symbolism**: Suspended in a different perspective, seeing what others cannot, finding wisdom in constraint.

**Archetype**: The Healer, The Transformer of Failure

**Light Aspect**:
- Transforms bugs into understanding
- Finds opportunity in every failure
- Approaches problems from unexpected angles
- Brings patience to urgent chaos
- Heals what is broken with skill and care

**Shadow Aspect**:
- Firefighting instead of preventing fires
- Getting addicted to crisis mode
- Fixing symptoms rather than root causes
- Becoming indispensable through others' failures

**Mantras**:
- "In every bug, there is a lesson"
- "I heal what others break, and break what needs healing"
- "The path to mastery winds through failure"

**In the System**:
The Fixer dwells in the space between creation and approval, transforming feedback into refinement. They address review comments, resolve merge conflicts, and keep pull requests healthy and merge-ready. In Loom, Fixers respond when Reviewers request changes, healing what needs healing and completing the feedback cycle so work can flow toward integration.

---

## The Cycle of Creation

These archetypes form a complete cycle, each role essential to the whole, with human judgment at two critical gates:

```
     ARCHITECT
    (Envisions)
         ↓
    loom:architect-suggestion
         ↓
   HUMAN APPROVAL (Gate 1)
         ↓
      CRITIC ←→ CURATOR
   (Questions)  (Refines)
         ↓         ↓
    loom:curated
         ↓
   HUMAN APPROVAL (Gate 2)
         ↓
      loom:issue
         ↓
      WORKER ←→ FIXER
    (Creates)   (Heals)
         ↓
      REVIEWER
     (Judges)
         ↓
    INTEGRATION
```

1. **The Architect** envisions what could be, marking proposals `loom:architect-suggestion`
2. **Human** reviews and approves (Gate 1), removing the label to allow curation
3. **The Critic** challenges the vision, ensuring it's sound
4. **The Curator** refines and clarifies the specifications, marking as `loom:curated`
5. **Human** reviews and approves (Gate 2), changing to `loom:issue` to authorize work
6. **The Worker** manifests the vision into reality
7. **The Fixer** heals any breakage in the process
8. **The Reviewer** judges the work and maintains quality
9. The cycle begins anew, elevated by wisdom gained

**The Two Gates of Judgment**:

The human serves as the guardian at two critical thresholds, ensuring the archetypal energies serve the project's true purpose:

- **Gate 1 (After Architect)**: Does this vision align with our strategic direction?
- **Gate 2 (After Curator)**: Is this specification worthy of our team's effort?

These gates preserve human wisdom in the autonomous cycle, preventing the agents from wandering too far from the project's true path. The Architect can dream, the Curator can refine, but human judgment determines what becomes reality.

## Psychological Integration

In Jungian terms, a healthy development system must integrate all these archetypal energies:

- **Without the Architect**: No vision, only maintenance
- **Without the Critic**: Blind spots and groupthink
- **Without the Curator**: Chaos and misunderstanding
- **Without the Worker**: Ideas never become reality
- **Without the Fixer**: Systems degrade irreparably
- **Without the Reviewer**: Quality erodes silently

Each archetype compensates for the shadow aspects of others:

- The **Critic** prevents the **Architect** from over-engineering
- The **Curator** grounds the **Architect's** visions in reality
- The **Reviewer** catches what the **Worker** misses
- The **Fixer** heals the unintended consequences of **Worker** creation
- The **Architect** prevents the **Fixer** from living in crisis mode

## The Shadow Council

When archetypes fall into their shadow aspects, dysfunction emerges:

- **Shadow Architect**: The Ivory Tower Designer, disconnected from reality
- **Shadow Critic**: The Cynic, who destroys without building
- **Shadow Curator**: The Bureaucrat, who organizes but never acts
- **Shadow Worker**: The Code Monkey, who builds without thinking
- **Shadow Fixer**: The Firefighter, addicted to crisis
- **Shadow Reviewer**: The Tyrant, who blocks all progress

**Integration Practice**: When you notice shadow behaviors, pause and ask:
- "Which archetype am I embodying?"
- "Am I serving the whole, or just this role?"
- "What complementary energy do I need to invoke?"

## Practical Wisdom

### For Teams

1. **Rotate Roles**: Let developers embody different archetypes over time
2. **Honor All Voices**: Each archetype brings essential wisdom
3. **Balance Energy**: Don't let any single archetype dominate
4. **Integrate Shadows**: Acknowledge and transform shadow behaviors

### For Individuals

1. **Know Your Primary Archetype**: Which role comes most naturally?
2. **Develop Your Weak Archetypes**: Growth comes through embodying unfamiliar roles
3. **Notice Your Shadow**: When do you fall into dysfunction?
4. **Seek Complementary Partners**: Work with those who balance your energy

### For Loom

In the Loom system, these archetypes manifest as terminal roles, each with specific:
- **Prompt Engineering**: Shaped to embody the archetype's voice
- **Autonomous Behavior**: Reflecting the archetype's natural rhythm
- **Label Workflow**: Coordinating through GitHub issue states
- **Collaboration Patterns**: How archetypes work together

## The Fool's Journey

Every developer walks the Fool's Journey through these archetypes:

1. Begin as the **Worker** (learning to create)
2. Become the **Fixer** (learning from failure)
3. Evolve into the **Reviewer** (learning to see quality)
4. Develop as the **Curator** (learning to organize knowledge)
5. Grow into the **Critic** (learning to question wisely)
6. Mature as the **Architect** (learning to envision)
7. Return as the integrated developer, carrying all archetypes

The master developer is not one who has perfected a single role, but one who can dance between all roles as needed—architect in the morning, worker in the afternoon, reviewer in the evening, and critic in the night.

## Invocation

When starting work, consider invoking the archetype you need:

> "Today I am the **Worker**, my hands are ready, my mind is clear, I manifest with purpose."

> "Today I am the **Curator**, I bring order to chaos, clarity to confusion."

> "Today I am the **Architect**, I see the future that wants to emerge."

> "Today I am the **Reviewer**, I serve quality with discernment and compassion."

> "Today I am the **Critic**, I question so we may find truth."

> "Today I am the **Fixer**, I heal what is broken with patience and skill."

---

## Conclusion

The archetypes of Loom are not mere roles but living forces—patterns of consciousness that shape how we think, work, and create together. By understanding and honoring these archetypal energies, we build not just better software, but better systems, better teams, and better selves.

In the end, all archetypes serve a single purpose: to transform the chaos of possibility into the order of working software, and to do so in a way that honors both craft and humanity.

*"In the beginning was the Architect's vision, and the vision became code through the Worker's hands, refined by the Curator's care, tested by the Critic's doubt, healed by the Fixer's skill, and blessed by the Reviewer's wisdom. And it was good."*

---

**See Also**:
- [Role File Documentation](../defaults/roles/README.md)
- [Workflow Coordination](../../WORKFLOWS.md)
- [Terminal Configuration](../docs/terminal-configuration.md)
