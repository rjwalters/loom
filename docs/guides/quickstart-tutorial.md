# Quickstart Tutorial: Your First Issue in MOM Mode

**Duration:** 10-15 minutes
**Mode:** Manual Orchestration Mode (MOM)
**Goal:** Learn the complete Loom workflow from issue to merged PR

## What is Manual Orchestration Mode (MOM)?

MOM is where you manually run Claude Code terminals with specialized role assignments (Builder, Judge, Curator, etc.) to coordinate development work through GitHub labels. Unlike Tauri App Mode (automated orchestration), MOM gives you direct control over each agent's actions through slash commands.

## Prerequisites

Before starting this tutorial, ensure you have:

- ✅ Loom installed in your repository (`loom-daemon init` completed)
- ✅ GitHub CLI (`gh`) installed and authenticated
- ✅ Claude Code installed and available via `claude` command
- ✅ Git configured with your identity

**Verify your setup:**
```bash
# Check GitHub CLI authentication
gh auth status

# Check Claude Code is available
which claude

# Verify Loom files exist
ls .loom/
```

## Scenario: Implementing a Simple Feature

We'll walk through a complete workflow: creating an issue for adding a new color theme, curating it, implementing it, reviewing it, and merging it.

---

## Step 1: Create an Issue

First, create a GitHub issue for the feature we want to implement.

```bash
gh issue create \
  --title "Add sunset color theme" \
  --body "Add a new warm sunset color theme option to complement existing themes" \
  --label "enhancement"
```

**Expected output:**
```
✓ Created issue #42
```

Make note of the issue number (we'll use `42` in this example).

---

## Step 2: Curate the Issue (Curator Role)

The Curator role enhances issues with implementation details, acceptance criteria, and test plans.

### Launch Curator Terminal

Open a new terminal and start Claude Code with the Curator role:

```bash
claude code "/curator"
```

**What this does:** Loads the Curator role definition from `.loom/roles/curator.md` and provides context about issue curation workflow.

### Find and Enhance the Issue

The Curator will automatically look for issues needing enhancement. When prompted, tell it to work on issue #42:

```
Please curate issue #42
```

**What the Curator does:**
1. Reads the issue description
2. Adds implementation guidance (which files to modify, approach options)
3. Creates acceptance criteria checklist
4. Adds a test plan
5. Marks the issue as `loom:curated`

**Expected label transition:**
```
No labels → loom:curated
```

### Review the Enhancement

Check the updated issue:

```bash
gh issue view 42
```

You should see the Curator's enhancement comment with implementation details.

### Approve for Work

Once you review the Curator's enhancement, approve it for implementation:

```bash
gh issue edit 42 --add-label "loom:issue"
```

**Label transition:**
```
loom:curated + loom:issue (ready for Builder to claim, curated label preserved)
```

---

## Step 3: Claim and Implement (Builder Role)

Now we'll implement the feature as a Builder.

### Launch Builder Terminal

In a new terminal (or the same one after exiting Curator):

```bash
claude code "/builder"
```

### Claim the Issue

Tell the Builder to work on issue #42:

```
Let's implement issue #42
```

The Builder will:
1. Claim the issue by updating labels
2. Create a worktree for isolated development
3. Implement the feature
4. Run tests
5. Commit changes
6. Create a pull request

### What Happens Behind the Scenes

**1. Claim the issue:**
```bash
gh issue edit 42 --remove-label "loom:issue" --add-label "loom:building"
```

**Label transition:**
```
loom:issue → loom:building
```

**2. Create worktree:**
```bash
./.loom/scripts/worktree.sh 42
cd .loom/worktrees/issue-42
```

This creates an isolated workspace at `.loom/worktrees/issue-42` with a new branch `feature/issue-42`.

**3. Implement the feature:**

The Builder will make code changes, following the implementation guidance from the Curator.

**4. Run tests:**
```bash
pnpm check:ci
```

**5. Commit:**
```bash
git add -A
git commit -m "Add sunset color theme

Implements warm sunset colors for theme switching.
Includes light and dark variants.

Closes #42"
```

**6. Push and create PR:**
```bash
git push -u origin feature/issue-42
gh pr create --label "loom:review-requested" \
  --title "Add sunset color theme" \
  --body "Implements issue #42..."
```

**Expected output:**
```
✓ Created pull request #43
```

**Label transition (on PR):**
```
No labels → loom:review-requested
```

---

## Step 4: Review the PR (Judge Role)

The Judge role performs thorough code reviews.

### Launch Judge Terminal

```bash
claude code "/judge"
```

### Find and Review the PR

Tell the Judge to review the PR:

```
Please review PR #43
```

**What the Judge does:**
1. Checks out the PR branch
2. Reviews code changes
3. Runs tests
4. Checks for issues (formatting, logic errors, missing tests)
5. Either approves or requests changes

### Judge Approves

If everything looks good, the Judge will:

```bash
gh pr review 43 --approve --body "LGTM!

✅ Code follows project conventions
✅ Tests pass
✅ Implementation matches issue requirements"

gh pr edit 43 --remove-label "loom:review-requested" --add-label "loom:pr"
```

**Label transition:**
```
loom:review-requested → loom:pr (approved, ready to merge)
```

### If Changes Needed

If the Judge finds issues, it will request changes:

```bash
gh pr review 43 --request-changes --body "Needs fixes:
- [ ] Add dark mode variant
- [ ] Fix color contrast"

gh pr edit 43 --remove-label "loom:review-requested" --add-label "loom:building"
```

Then the Builder would address the feedback and re-request review.

---

## Step 5: Merge and Close

Once the PR is approved (`loom:pr` label), you can merge it:

```bash
./.loom/scripts/merge-pr.sh 43
```

**Expected output:**
```
Merging PR #43: Fix widget alignment
Branch: feature/issue-42
PR #43 merged successfully
Branch 'feature/issue-42' deleted
Done
```

The issue (#42) will automatically close because the PR had "Closes #42" in the description.

---

## Complete Label Workflow Diagram

Here's how labels flow through the entire process:

```
Issue Created (no labels)
    ↓
Curator enhances → loom:curated
    ↓
Human approves → loom:issue
    ↓
Builder claims → loom:building
    ↓
Builder creates PR → loom:review-requested (on PR)
    ↓
Judge reviews → loom:pr (approved)
    ↓
Human merges → Issue closed
```

---

## What You've Learned

Congratulations! You've completed your first Loom workflow. You now know:

✅ **Slash Commands**: How to launch role-specific Claude Code sessions
✅ **Label Workflow**: How GitHub labels coordinate agent work
✅ **Worktrees**: How isolated development environments work
✅ **Role Coordination**: How Curator, Builder, and Judge roles interact
✅ **Complete Cycle**: From issue creation to merged PR

## Next Steps

### Explore More Roles

Try these other roles:

- **`/architect`** - Create architectural proposals and design documents
- **`/hermit`** - Identify code bloat and suggest simplifications
- **`/doctor`** - Fix bugs and maintain existing PRs
- **`/guide`** - Prioritize and organize the issue backlog

### Customize Roles

Create custom roles for your team's workflow:

```bash
# Create a custom role
mkdir -p .loom/roles
cp defaults/roles/builder.md .loom/roles/my-custom-role.md
# Edit my-custom-role.md to customize
```

See [defaults/roles/README.md](../../defaults/roles/README.md) for details.

### Automate with Tauri App Mode

Once comfortable with MOM, try **Tauri App Mode** for fully automated orchestration:

```bash
# Open the Loom desktop app
open -a Loom  # or launch from Applications

# Select your workspace
# Click "Start Engine"
# Watch agents work autonomously
```

---

## Troubleshooting

### Issue: "Claude Code not found"

**Solution:**
```bash
# Install Claude Code
brew install anthropics/claude/claude-code

# Or download from https://claude.com/code
```

### Issue: "gh: command not found"

**Solution:**
```bash
# Install GitHub CLI
brew install gh

# Authenticate
gh auth login
```

### Issue: "Worktree already exists"

**Solution:**
```bash
# IMPORTANT: First navigate OUT of any worktree directory
cd /path/to/main/repo

# Remove old worktree (only from main repo, not from inside a worktree!)
git worktree remove .loom/worktrees/issue-42 --force
git worktree prune

# Try again
./.loom/scripts/worktree.sh 42
```

**Note**: Running `git worktree remove` while your shell is in the worktree will corrupt your shell state. Always navigate out first!

### Issue: "Permission denied" when creating issues/PRs

**Solution:**
```bash
# Re-authenticate with GitHub CLI
gh auth refresh

# Verify permissions
gh auth status
```

---

## Additional Resources

- **[Getting Started Guide](getting-started.md)** - Installation and setup
- **[Workflows Documentation](../workflows.md)** - Complete label workflow reference
- **[Role Definitions](../../defaults/roles/README.md)** - Learn about each role
- **[Git Workflow Guide](git-workflow.md)** - Worktree management and branching strategy
- **[Architecture Patterns](architecture-patterns.md)** - Understand Loom's design

---

**Questions or feedback?** Open an issue at https://github.com/rjwalters/loom/issues
