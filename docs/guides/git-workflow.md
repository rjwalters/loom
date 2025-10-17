# Git Workflow

## Branch Strategy

- `main`: Always stable, ready to release
- `feature/issue-X-description`: Feature branches from issues
- PR required for merge to main

## Commit Convention

```
<type>: <short description>

<longer description>

<footer>
```

Example:
```
Implement initial layout structure with terminal management

Build core UI layout with header, primary terminal view, mini terminal row...

Closes #2
```

## PR Process

1. Create feature branch from main
2. Implement feature
3. Test manually (`pnpm tauri:dev`)
4. **CRITICAL: Run `pnpm check:ci`** - This runs the exact same checks as CI
5. Fix any errors found by local CI checks
6. Create PR with detailed description
7. Merge after review

## IMPORTANT: AI Agent Pre-PR Checklist

**For all AI agents (Worker, Architect, Curator, Reviewer):**

Before creating or updating a Pull Request, you MUST run:

```bash
pnpm check:ci
```

This command runs the complete CI suite locally:
- Biome linting and formatting
- Rust formatting (rustfmt)
- Clippy with all CI flags (`--workspace --all-targets --all-features --locked -D warnings`)
- Cargo check
- Frontend build (TypeScript compilation + Vite)
- All tests (daemon integration tests)

### Why This Matters for AI Agents

1. **Prevent CI Failures**: Running `pnpm check:ci` catches issues locally before pushing
2. **Save Time**: Fix issues immediately instead of waiting for remote CI to fail
3. **Match CI Exactly**: Uses the exact same commands and flags as GitHub Actions
4. **Avoid Wasted Cycles**: Don't create PRs that will fail CI checks

### Common Mistakes AI Agents Make

- Running `cargo clippy` directly instead of `pnpm clippy` (misses CI flags)
- Running `biome check` without `--write` flag (doesn't auto-fix)
- Skipping tests or not running full build
- Not checking format issues before commit

### Required Before PR Creation

```bash
# Step 1: Run full CI suite locally
pnpm check:ci

# Step 2: If any errors, fix them and re-run
# (repeat until clean)

# Step 3: Commit changes
git add -A
git commit -m "Your commit message"

# Step 4: Push and create PR
git push
gh pr create ...
```

### If `pnpm check:ci` Fails

1. Read the error output carefully
2. Fix the issues (format strings, unused variables, type errors, etc.)
3. Run `pnpm check:ci` again
4. Only proceed with PR when it passes clean
