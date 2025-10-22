# Loom Quickstart Template

Welcome to the Loom Quickstart! This minimal template gets you started with the essential terminals for AI-powered development.

## What's Included

This template provides **3 pre-configured terminals**:

### 1. Shell (Primary)
- **Role**: Default shell environment
- **Mode**: Manual (no autonomous operation)
- **Use for**: Running commands, exploring the codebase, debugging

### 2. Worker
- **Role**: General development worker
- **Mode**: Manual by default (can enable autonomous mode)
- **Use for**: Implementing features, fixing bugs, writing code
- **Suggested prompt**: "Find and implement loom:ready issues"

### 3. Reviewer
- **Role**: Code review specialist
- **Mode**: Manual by default (can enable autonomous mode)
- **Use for**: Reviewing pull requests, providing feedback
- **Suggested prompt**: "Find and review PRs with loom:review-requested label"

## Getting Started

### Option 1: Use in an Existing Project

1. Copy the `.loom/` directory to your git repository root
2. Open Loom and select your repository as the workspace
3. Click "Start Workspace" to create the terminals
4. Terminals will launch with their configured roles

### Option 2: Create a New Project

1. Create a new directory for your project
2. Initialize a git repository: `git init`
3. Copy the `.loom/` directory from this template
4. Open Loom and select the new directory
5. Click "Start Workspace"

## Workflow

### Basic Development Flow

1. **Create an issue** on GitHub describing what needs to be done
2. **Add the `loom:ready` label** to signal it's ready for implementation
3. **Activate the Worker terminal** and give it a task:
   - "Find and implement loom:ready issues"
   - Or manually: "Implement issue #42"
4. **Worker creates a PR** when the feature is complete
5. **Activate the Reviewer terminal**: "Review PR #43"
6. **Reviewer provides feedback** via GitHub PR comments
7. **Merge when approved!**

### Label Workflow

This template uses GitHub labels to coordinate work:

- `loom:ready` (green) - Issue is ready for implementation
- `loom:building` (yellow) - Worker is currently working on it
- `loom:review-requested` (green) - PR is ready for review
- `loom:reviewing` (amber) - Reviewer is currently reviewing
- `loom:approved` (blue) - PR is approved and ready to merge

## Configuration

### Enable Autonomous Mode

To have terminals work automatically at intervals:

1. Click the ⚙️ icon on a terminal card
2. Check "Enable Autonomous Mode"
3. Set interval (e.g., 300000ms = 5 minutes)
4. Enter the prompt to repeat

**Recommended autonomous settings**:
- **Worker**: Disabled (manual control)
- **Reviewer**: 5 minutes, "Find and review PRs with loom:review-requested label"

### Customize Roles

To modify how terminals behave:

1. Edit files in `.loom/roles/` (creates workspace-specific customizations)
2. Or edit the system defaults in `defaults/roles/` (affects all workspaces)

Each role has two files:
- **`.md`**: The role definition and instructions
- **`.json`**: Metadata with default settings

## Next Steps

Once you're comfortable with the basics:

- **Add more Worker terminals** for parallel development
- **Try the full-stack template** with all 7 terminal types
- **Create custom roles** in `.loom/roles/` for your specific needs
- **Set up CI/CD** to auto-run tests on PRs

## Troubleshooting

### Terminals not launching?

- Ensure you're in a git repository
- Check that Claude Code is installed and in your PATH
- Review console logs for errors

### Workers not finding issues?

- Make sure you have GitHub CLI (`gh`) installed and authenticated
- Create issues with the `loom:ready` label
- Try giving explicit instructions: "Implement issue #42"

### Need help?

- Read the [main documentation](../../README.md)
- Check [WORKFLOWS.md](../../WORKFLOWS.md) for advanced patterns
- Open an issue on GitHub

## Learn More

This template demonstrates the core Loom workflow. To unlock the full power of AI-powered development, explore:

- **[Full-Stack Template](../full-stack/)** - Complete setup with Architect, Curator, Critic, and more
- **[Custom Roles](../../defaults/roles/README.md)** - Create specialized agents for your workflow
- **[Label Workflows](../../WORKFLOWS.md)** - Coordinate complex multi-agent tasks

Happy coding with Loom! 🧵✨
