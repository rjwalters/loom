# GitHub Authentication Guide

Loom uses the `gh` CLI for all GitHub interactions — label management, PR creation, reviews, merges, and issue coordination. By default, `gh auth login` grants access to all repositories the authenticated user can reach. For tighter security, you can scope Loom's access to a single repository using a fine-grained personal access token (PAT).

## Quick Start

```bash
# 1. Create a fine-grained PAT (see steps below)
# 2. Export it before running Loom
export GH_TOKEN=github_pat_xxx

# 3. Verify
gh auth status
```

## Required Token Permissions

A fine-grained PAT scoped to the target repository needs these permissions:

| Permission | Level | Used By | Purpose |
|---|---|---|---|
| Issues | Read & Write | Builder, Curator, Champion, Shepherd | Label coordination, issue creation and editing |
| Pull requests | Read & Write | Builder, Judge, Champion, Doctor | PR creation, reviews, merges |
| Contents | Read & Write | Builder, Champion | Push branches, merge PRs, delete branches |
| Checks | Read | Auditor, Judge | CI status verification |
| Metadata | Read | All roles | Implicit, always granted with any other permission |

## Creating a Fine-Grained PAT

1. Go to [GitHub token settings](https://github.com/settings/tokens?type=beta)
2. Click **Generate new token**
3. Set a descriptive name (e.g., `loom-<repo-name>`)
4. Set an expiration (90 days recommended; renew before it expires)
5. Under **Repository access**, select **Only select repositories** and choose the target repo
6. Under **Permissions**, expand **Repository permissions** and set:
   - **Contents**: Read and write
   - **Issues**: Read and write
   - **Pull requests**: Read and write
   - **Checks**: Read-only
7. Click **Generate token** and copy the value immediately — it won't be shown again

## Using the Token

The `gh` CLI checks for `GH_TOKEN` (or `GITHUB_TOKEN`) before using its default credential store. Set the variable in the shell session where Loom runs:

```bash
# Option A: Export in current session
export GH_TOKEN=github_pat_xxxxxxxxxxxxxxxxxxxx

# Option B: Add to shell profile (~/.zshrc, ~/.bashrc)
export GH_TOKEN=github_pat_xxxxxxxxxxxxxxxxxxxx

# Option C: Use a secrets manager or .env file (not committed)
source .env  # where .env contains: export GH_TOKEN=github_pat_xxx
```

When using Tauri App Mode, set the variable before launching the app so all spawned terminals inherit it.

## Verifying Authentication

```bash
# Check which auth method is active
gh auth status

# Expected output with a fine-grained PAT:
#   github.com
#     ✓ Logged in to github.com account <user> (GH_TOKEN)
#     ...
#     Token scopes: (none)   ← fine-grained PATs show no classic scopes

# Test repository access
gh repo view <owner>/<repo> --json name

# Test issue access
gh issue list --repo <owner>/<repo> --limit 1

# Test PR access
gh pr list --repo <owner>/<repo> --limit 1
```

If `gh auth status` shows the default credential instead of `GH_TOKEN`, verify the variable is exported in the same shell session.

## Troubleshooting

### Token not being picked up

- Confirm `echo $GH_TOKEN` shows the token value
- The variable must be **exported**, not just set: `export GH_TOKEN=...`
- If using Tauri App Mode, restart the app after setting the variable

### Permission errors (403 / insufficient scope)

- Verify the PAT is scoped to the correct repository
- Check that all required permissions are granted (see table above)
- Fine-grained PATs do not show classic scopes in `gh auth status` — this is expected

### Token expired

- Fine-grained PATs have an expiration date set at creation
- Generate a new token and update the `GH_TOKEN` value
- Consider setting a calendar reminder before expiration

## Security Notes

- **Never commit tokens** to the repository. Add `.env` to `.gitignore` if using an env file.
- Fine-grained PATs are more secure than classic tokens because they limit both repository and permission scope.
- Use the minimum permissions required. The table above lists exactly what Loom needs.
- Rotate tokens periodically — 90-day expiration is a reasonable default.
