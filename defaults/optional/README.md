# Optional Loom Components

This directory contains optional components that are **not** installed by default but can be manually added to your workspace.

## Available Components

### `github-workflows/label-external-issues.yml`

A GitHub Actions workflow that automatically labels issues created by non-collaborators with an `external` label and posts a welcome comment. Useful for repositories that expect contributions from external users.

**Why not installed by default?** In single-contributor repos (the common case for Loom-managed projects), this workflow fires on every issue event and generates "No jobs were run" email notifications from GitHub, creating spam during active Loom sessions.

**To install manually:**

```bash
# From your workspace root
mkdir -p .github/workflows
cp /path/to/loom/defaults/optional/github-workflows/label-external-issues.yml .github/workflows/

# Replace template variables with your repo info
sed -i '' 's/{{REPO_OWNER}}/your-username/g; s/{{REPO_NAME}}/your-repo/g' \
  .github/workflows/label-external-issues.yml

# Create the required 'external' label
gh label create external --description "External contribution requiring manual triage" --color "6B7280"
```
