# Tarot Card Brand Assets

Visual assets for Loom's archetypal agent roles, styled as tarot cards to match the mystical/archetypal branding aesthetic.

## Role Cards

Each agent role has a corresponding tarot card image that represents its archetypal force:

| Role | Archetype | File | Description |
|------|-----------|------|-------------|
| **Builder** | The Magician | `builder.svg` | Transforms ideas into reality through manifestation and creative energy |
| **Curator** | The High Priestess | `curator.svg` | Refines chaos into clarity through intuition and knowledge organization |
| **Architect** | The Emperor | `architect.svg` | Envisions structure and design through systematic vision and authority |
| **Champion** | Strength | `champion.svg` | Promotes quality work and merges what is ready through steady resolve |
| **Judge** | Justice | `judge.svg` | Maintains quality through impartial discernment and balanced judgment |
| **Hermit** | The Hermit | `hermit.svg` | Questions to find truth through introspective wisdom and skepticism |
| **Doctor** | The Hanged Man | `doctor.svg` | Heals what is broken through patient transformation and perspective shifts |
| **Guide** | The Star | `guide.svg` | Illuminates priorities through focused guidance and clarity |
| **Driver** | The Chariot | `driver.svg` | Masters direct action through willpower and human agency |

## Design Specifications

**Style**: Tarot card aesthetic with mystical, symbolic imagery
**Format**: SVG (scalable vector graphics)
**Color Scheme**: Works with both light and dark themes
**Consistency**: Unified visual language across all cards

## Usage

These are brand assets; nothing in the codebase consumes them today. They are
available for:

- **Documentation**: Visual reference in READMEs and workflow docs
- **UI surfaces**: Role pickers, settings modals, about/help pages
- **External use**: Presentations, articles, and other Loom branding

## Implementation

To use a tarot card image in a UI:

```typescript
// Import the SVG
import builderCard from '@/assets/tarot-cards/builder.svg';

// Use in component
<img src={builderCard} alt="Builder - The Magician" class="w-32 h-48" />
```

## Attribution

**Design Approach**: TBD (AI-generated, custom artwork, or licensed imagery)
**License**: TBD
**Attribution**: TBD

## Philosophy

These visual assets embody the archetypal framework described in [docs/philosophy/agent-archetypes.md](../../docs/philosophy/agent-archetypes.md). Each card represents a universal pattern in software development, drawing from Tarot's Major Arcana and Jungian depth psychology.

> *Like the Tarot's Major Arcana, each role is essential to the whole. When working in harmony, they transform chaos into creation.*
