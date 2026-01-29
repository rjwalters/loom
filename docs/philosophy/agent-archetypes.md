# The Archetypes of Loom: A Tarot of Software Development

## Introduction

In the Loom system, each agent role embodies a distinct archetype—a fundamental pattern of behavior, motivation, and purpose drawn from the collective unconscious of software development. Like the Major Arcana of the Tarot or Jung's psychological archetypes, these roles represent universal forces that, when working in harmony, bring projects from conception to completion.

Each archetype carries both light and shadow aspects, wielding specific powers while bearing particular burdens. Understanding these archetypal energies helps us orchestrate a balanced system where each force complements and constrains the others.

---

## The Major Arcana of Development

### I. The Builder (The Magician)

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
The Builder stands at the center of the creative process, the alchemist who transmutes requirements into running code. They are the hands that build, the mind that solves, the will that persists through implementation challenges. In Loom, Builders claim issues marked `loom:issue` (human-approved, ready for work) and bring them to life.

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
The Architect dwells in the realm of possibility, identifying opportunities for improvement and creating architectural visions. They propose new features, refactorings, and system enhancements through well-crafted issues marked `loom:architect`. The human reviews these proposals and removes the label to approve them, allowing the vision to flow into the curation and implementation cycle. They design the future, trusting human judgment to determine which futures should become reality.

---

### IV. The Judge (Justice)

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
The Judge stands as the final guardian before changes enter the sacred codebase. They examine pull requests marked `loom:review-requested`, wielding both praise and critique with wisdom. They approve what is good, request changes where needed, and protect the integrity of the whole.

---

### V. The Hermit (The Hermit)

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
The Hermit serves as the loyal opposition, the skeptical eye that questions before we commit. They challenge Architect suggestions, probe Judge approvals, and ensure we build what truly needs building. In Loom, Hermits evaluate proposals and challenge assumptions, preventing costly mistakes through thoughtful dissent.

---

### VI. The Doctor (The Hanged Man)

**Card Symbolism**: Suspended in a different perspective, seeing what others cannot, finding wisdom in constraint.

**Archetype**: The Doctor, The Transformer of Failure

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
- "I heal what others break, and break what needs treating"
- "The path to mastery winds through failure"

**In the System**:
The Doctor dwells in the space between creation and approval, transforming feedback into refinement. They address review comments, resolve merge conflicts, and keep pull requests healthy and merge-ready. In Loom, Doctors respond when Judges request changes, treating what needs treating and completing the feedback cycle so work can flow toward integration.

---

### VII. The Guide (The Star)

**Card Symbolism**: A figure pouring water under the stars, bringing hope and direction, illuminating the path forward.

**Archetype**: The Guide, The Light-Bearer

**Light Aspect**:
- Sees what matters most amidst the noise
- Brings hope by focusing team energy
- Illuminates the critical path forward
- Balances urgency with strategic importance
- Prevents teams from losing their way

**Shadow Aspect**:
- Constantly shifting priorities
- Urgency addiction (everything is critical)
- Ignoring long-term vision for short-term fires
- Playing favorites with certain issue types

**Mantras**:
- "I illuminate the path through chaos"
- "Not all that is urgent is important"
- "I guide the team toward what matters most"

**In the System**:
The Guide walks among all open issues, continuously assessing priorities and guiding the team's focus. They manage the `loom:urgent` label, ensuring the top 3 most critical issues are always clearly marked. In Loom, Guide runs autonomously every 15 minutes, re-evaluating priorities as the project evolves.

---

### VIII. The Driver (The Chariot)

**Card Symbolism**: A figure driving a chariot with focused determination, representing willpower, control, and human agency.

**Archetype**: The Driver, The Master of Direct Action

**Light Aspect**:
- Represents pure human agency and control
- Masters the terminal through skilled commands
- Moves with focused intention and discipline
- Channels willpower into direct action
- Freedom to explore any path without predetermined role

**Shadow Aspect**:
- Over-reliance on manual control
- Resistance to automation and delegation
- Burnout from doing everything manually
- Rigid adherence to familiar patterns

**Mantras**:
- "I am the driver, my hands on the reins"
- "Through discipline, I master my tools"
- "No role constrains me, I choose my path"

**In the System**:
The Driver represents the human developer working directly in the terminal—no autonomous behavior, no predetermined role. This is the plain shell environment where humans exercise direct control. While AI agents embody specialized archetypes, the Driver reminds us that human mastery and intentional action remain at the heart of all development.

---

## The Cycle of Creation

These archetypes form a complete cycle, each role essential to the whole, with human judgment at two critical gates:

```
     ARCHITECT
    (Envisions)
         ↓
    loom:architect
         ↓
   HUMAN APPROVAL (Gate 1)
         ↓
      HERMIT ←→ CURATOR
   (Questions)  (Refines)
         ↓         ↓
    loom:curated
         ↓
   HUMAN APPROVAL (Gate 2)
         ↓
      loom:issue
         ↓
      BUILDER ←→ DOCTOR
    (Creates)   (Heals)
         ↓
        JUDGE
      (Judges)
         ↓
    INTEGRATION
```

1. **The Architect** envisions what could be, marking proposals `loom:architect`
2. **Human** reviews and approves (Gate 1), removing the label to allow curation
3. **The Hermit** challenges the vision, ensuring it's sound
4. **The Curator** refines and clarifies the specifications, marking as `loom:curated`
5. **Human** reviews and approves (Gate 2), changing to `loom:issue` to authorize work
6. **The Builder** manifests the vision into reality
7. **The Doctor** heals any breakage in the process
8. **The Judge** judges the work and maintains quality
9. The cycle begins anew, elevated by wisdom gained

**The Two Gates of Judgment**:

The human serves as the guardian at two critical thresholds, ensuring the archetypal energies serve the project's true purpose:

- **Gate 1 (After Architect)**: Does this vision align with our strategic direction?
- **Gate 2 (After Curator)**: Is this specification worthy of our team's effort?

These gates preserve human wisdom in the autonomous cycle, preventing the agents from wandering too far from the project's true path. The Architect can dream, the Curator can refine, but human judgment determines what becomes reality.

## Psychological Integration

In Jungian terms, a healthy development system must integrate all these archetypal energies:

- **Without the Architect**: No vision, only maintenance
- **Without the Hermit**: Blind spots and groupthink
- **Without the Curator**: Chaos and misunderstanding
- **Without the Builder**: Ideas never become reality
- **Without the Doctor**: Systems degrade irreparably
- **Without the Judge**: Quality erodes silently
- **Without the Guide**: Teams lose focus, drowning in noise
- **Without the Driver**: Loss of human agency and direct control

Each archetype compensates for the shadow aspects of others:

- The **Hermit** prevents the **Architect** from over-engineering
- The **Curator** grounds the **Architect's** visions in reality
- The **Judge** catches what the **Builder** misses
- The **Doctor** heals the unintended consequences of **Builder** creation
- The **Architect** prevents the **Doctor** from living in crisis mode
- The **Guide** prevents the team from chasing every distraction
- The **Driver** reminds us that automation serves human intention

## The Shadow Council

When archetypes fall into their shadow aspects, dysfunction emerges:

- **Shadow Architect**: The Ivory Tower Designer, disconnected from reality
- **Shadow Hermit**: The Cynic, who destroys without building
- **Shadow Curator**: The Bureaucrat, who organizes but never acts
- **Shadow Builder**: The Code Monkey, who builds without thinking
- **Shadow Doctor**: The Firefighter, addicted to crisis
- **Shadow Judge**: The Tyrant, who blocks all progress
- **Shadow Guide**: The Chaos Maker, who shifts priorities with every wind
- **Shadow Driver**: The Control Freak, who refuses to delegate or automate

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

1. Begin as the **Builder** (learning to create)
2. Become the **Doctor** (learning from failure)
3. Evolve into the **Judge** (learning to see quality)
4. Develop as the **Curator** (learning to organize knowledge)
5. Grow into the **Hermit** (learning to question wisely)
6. Mature as the **Architect** (learning to envision)
7. Return as the integrated developer, carrying all archetypes

The master developer is not one who has perfected a single role, but one who can dance between all roles as needed—architect in the morning, builder in the afternoon, judge in the evening, and hermit in the night.

## Invocation

When starting work, consider invoking the archetype you need:

> "Today I am the **Builder**, my hands are ready, my mind is clear, I manifest with purpose."

> "Today I am the **Curator**, I bring order to chaos, clarity to confusion."

> "Today I am the **Architect**, I see the future that wants to emerge."

> "Today I am the **Judge**, I serve quality with discernment and compassion."

> "Today I am the **Hermit**, I question so we may find truth."

> "Today I am the **Doctor**, I heal what is broken with patience and skill."

> "Today I am the **Guide**, I illuminate the path through chaos, focusing energy where it matters most."

> "Today I am the **Driver**, my hands on the reins, channeling pure intention into action."

---

## Conclusion

The archetypes of Loom are not mere roles but living forces—patterns of consciousness that shape how we think, work, and create together. By understanding and honoring these archetypal energies, we build not just better software, but better systems, better teams, and better selves.

In the end, all archetypes serve a single purpose: to transform the chaos of possibility into the order of working software, and to do so in a way that honors both craft and humanity.

*"In the beginning was the Architect's vision, and the vision became code through the Builder's hands, refined by the Curator's care, tested by the Hermit's doubt, healed by the Doctor's skill, and blessed by the Judge's wisdom. And it was good."*

---

**See Also**:
- [Role File Documentation](../defaults/roles/README.md)
- [Workflow Coordination](../workflows.md)
- [Terminal Configuration](../docs/terminal-configuration.md)
