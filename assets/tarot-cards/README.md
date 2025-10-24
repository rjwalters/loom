# Tarot Card Brand Assets

Visual assets for Loom's archetypal agent roles, styled as tarot cards to match the mystical/archetypal branding aesthetic.

## Role Cards

Each agent role has a corresponding tarot card image that represents its archetypal force:

| Role | Archetype | File | Description |
|------|-----------|------|-------------|
| **Builder** | The Magician | `worker.svg` | Transforms ideas into reality through manifestation and creative energy |
| **Curator** | The High Priestess | `curator.svg` | Refines chaos into clarity through intuition and knowledge organization |
| **Architect** | The Emperor | `architect.svg` | Envisions structure and design through systematic vision and authority |
| **Judge** | Justice | `reviewer.svg` | Maintains quality through impartial discernment and balanced judgment |
| **Hermit** | The Hermit | `critic.svg` | Questions to find truth through introspective wisdom and skepticism |
| **Doctor** | The Hanged Man | `fixer.svg` | Heals what is broken through patient transformation and perspective shifts |
| **Guide** | The Star | `guide.svg` | Illuminates priorities through focused guidance and clarity |
| **Driver** | The Chariot | `driver.svg` | Masters direct action through willpower and human agency |

## Design Specifications

**Style**: Tarot card aesthetic with mystical, symbolic imagery
**Format**: SVG (scalable vector graphics)
**Color Scheme**: Works with both light and dark themes
**Consistency**: Unified visual language across all cards

## Usage

These assets are used throughout the Loom UI:

- **Terminal Settings Modal**: Display role card when selecting agent role
- **Role Selection Dropdown**: Icon preview for each role option
- **Documentation**: Visual reference in README and WORKFLOWS
- **About/Help**: Explain archetypal system to users

## Implementation

To use a tarot card image in the UI:

```typescript
// Import the SVG
import workerCard from '@/assets/tarot-cards/worker.svg';

// Use in component
<img src={workerCard} alt="Worker - The Magician" class="w-32 h-48" />
```

## Attribution

**Design Approach**: TBD (AI-generated, custom artwork, or licensed imagery)
**License**: TBD
**Attribution**: TBD

## Philosophy

These visual assets embody the archetypal framework described in [docs/philosophy/agent-archetypes.md](../../docs/philosophy/agent-archetypes.md). Each card represents a universal pattern in software development, drawing from Tarot's Major Arcana and Jungian depth psychology.

> *Like the Tarot's Major Arcana, each role is essential to the whole. When working in harmony, they transform chaos into creation.*
