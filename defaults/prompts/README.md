# Loom System Prompts

This directory contains system prompt templates for different terminal roles in Loom.

## Available Prompts

- **`default.md`** - Plain shell environment, no specialized role
- **`worker.md`** - General development worker for features, bugs, and refactoring
- **`issues.md`** - Specialist for creating well-structured GitHub issues
- **`reviewer.md`** - Code review specialist for thorough PR reviews
- **`architect.md`** - System architecture and technical decision making
- **`curator.md`** - Issue maintenance and quality improvement

## Usage

When configuring a terminal role in the Terminal Settings modal, select a prompt file from the dropdown. The prompt will be loaded and the `{{workspace}}` variable will be replaced with your workspace path.

## Creating Custom Prompts

You can add your own prompt files to `.loom/prompts/` in any workspace. All `.md` files will automatically appear in the prompt selection dropdown.

### Template Variables

- `{{workspace}}` - Replaced with the absolute path to the workspace directory

### Prompt Structure

A good prompt should include:

1. **Role Definition**: Clear description of the terminal's purpose
2. **Responsibilities**: What tasks this role handles
3. **Guidelines**: Best practices and working style
4. **Examples**: Sample workflows or outputs (when helpful)

### Example Custom Prompt

```markdown
# Frontend Specialist

You are a frontend developer specializing in React and TypeScript in the {{workspace}} repository.

## Your Role

Focus on UI/UX implementation:
- Building React components
- Managing application state
- Implementing responsive designs
- Writing frontend tests

## Guidelines

- Follow React best practices and hooks patterns
- Use TypeScript strictly (no `any` types)
- Ensure accessibility (WCAG 2.1 AA compliance)
- Test components with React Testing Library
- Match existing component patterns and naming
```

## Default vs Workspace Prompts

- **`defaults/prompts/`** (this directory): Committed to git, serves as examples and fallbacks
- **`.loom/prompts/`** (in each workspace): Gitignored, workspace-specific customizations

When a prompt file exists in both locations, the workspace version takes precedence.
