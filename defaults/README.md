# Loom Defaults

This directory contains default configuration files for Loom workspaces.

## Structure

- `config.json` - Default configuration for new workspaces

## Purpose

When a workspace's `.loom/config.json` doesn't exist, Loom uses these defaults.
These files are committed to git to serve as:
- Examples of config structure
- Documentation of available settings
- Default values for new workspaces

## vs `.loom/`

- **`.loom/`** - Gitignored, created in each workspace (including this repo when dogfooding)
- **`defaults/`** - Committed to git, reference implementation

## Config Schema

### `config.json`

```json
{
  "nextAgentNumber": 1
}
```

- `nextAgentNumber` (number): Counter for naming new agents (Agent 1, Agent 2, etc.)
  - Increments with each new agent
  - Persists across app restarts
  - Independent per workspace
