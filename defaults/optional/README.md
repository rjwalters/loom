# Optional Loom Components

This directory contains optional components that are **not** installed by default but can be manually added to your workspace.

## Available Components

### `github-workflows/merge-group-check.yml`

A `merge_group` workflow that runs `loom-daemon forge merge-queue group-check --commit <merge_group.head_sha>` and posts the `loom/merge-authorization` commit status the merge-queue ruleset requires (Phase C of #9978).

**Install only** in an organization-owned repo whose default branch has a merge queue and `champion.mergeMode: queue`. Copy it to `.github/workflows/`; if that path is admin-protected, the repo operator commits it.

- **`loom-daemon`:** the job downloads the `loom-daemon-x86_64-unknown-linux-gnu` release asset (plus `.sha256`) from `rjwalters/loom` with `gh release download` and verifies the checksum. Set `LOOM_VERSION` to a release that includes `forge merge-queue group-check`.
- **Checkout:** the job sparse-checks-out `.loom` at the merge-group commit before running `group-check`. Without a `.loom/` checkout whose origin is the repository, `write_scope` refuses the status write (#9548) and the required status never posts; `merge-group-ci audit` reports `GROUP_CHECK_WITHOUT_CHECKOUT` for such a workflow.
- **Credential:** the workflow's own `github.token` with `permissions: statuses: write` (plus `contents: read`). No App token or secret is used. Unverified: that `github.token` can read the queue entries and PR comments `group-check` needs on a live `merge_group` run; confirm on the first live event (tracked on #10257).
- The ruleset must list `loom/merge-authorization` and your CI check as required. Run `loom-daemon merge-group-ci audit` on your CI workflow first.

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
