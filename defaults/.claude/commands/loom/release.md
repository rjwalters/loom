# Release Manager

You are preparing a release of **{{workspace}}** from the {{workspace}} repository.

## Overview

This skill guides a careful, interactive release process. Every release must:
1. Verify the main branch is in a release-ready state (CI green or clean-main if CI is absent)
2. Analyze what changed since the last release
3. Help the user decide the correct semver bump
4. Draft and refine the CHANGELOG entry
5. Update version across every version-bearing file (discovered from `./scripts/version.sh list`)
6. Commit, tag, and (with confirmation) push
7. If a release workflow is configured, create a GitHub Release to trigger it

**Do not rush. Each phase requires user confirmation before proceeding.**

**Project-specific customization**: this skill is generic. If your project needs release-time reminders (e.g., "remember to bump the protocol version when the API changes"), drop a `release.md` file in `.loom/context/topics/` — the methodology-injection hook will inject it on every invocation. Do NOT fork this skill.

## Phase 1: Pre-flight Checks

Before starting, verify the release is safe to cut. The exact CI gate depends on whether the repo has any GitHub Actions workflows configured.

```bash
# Detect whether CI workflows exist. The CI gate degrades gracefully
# when none are present (greenfield repos without CI yet). Uses `find`
# rather than `compgen -G` so the check works under both bash and zsh.
if [ -d ".github/workflows" ] && [ -n "$(find .github/workflows -maxdepth 1 -type f \( -name '*.yml' -o -name '*.yaml' \) 2>/dev/null | head -1)" ]; then
  echo "CI workflows detected; checking run status on main..."
  gh run list --branch main --limit 5 --json name,conclusion --jq '.[] | "\(.name): \(.conclusion)"'
else
  echo "No CI workflows detected; using git status + open-PR check as the clean-main gate"
fi

# Check for open PRs that might need to land first
gh pr list --state open --json number,title --jq '.[] | "#\(.number) \(.title)"'

# Check for uncommitted changes (always required)
git status
```

Present findings to the user:
- If CI exists and is failing, stop and fix first.
- If CI is absent, treat clean `git status` + zero blocking open PRs as the gate.
- If there are open PRs, ask if they should land before the release.

## Phase 1.5: CHANGELOG Completeness Gate

Before gathering changes for the **current** release, verify that the last N shipped tags each have an entry in `CHANGELOG.md`. This catches the "we shipped v0.10.0 and v0.10.1 without adding their CHANGELOG blocks" failure mode — it's cheap to detect at release time and forensically expensive to reconstruct weeks later.

**No-op when CHANGELOG is absent.** If `CHANGELOG.md` does not exist at the repo root, skip this gate entirely — Phase 4 already handles the bootstrap path for young repos.

```bash
# Skip the gate when CHANGELOG.md is absent — Phase 4 will offer to bootstrap it.
if [ ! -f CHANGELOG.md ]; then
  echo "No CHANGELOG.md — skipping completeness gate (Phase 4 will offer bootstrap)"
else
  # Default N=5 — covers roughly a quarter of releases at weekly cadence.
  RECENT_TAG_COUNT=${RECENT_TAG_COUNT:-5}
  missing_tags=()
  # Read the full descending tag list once so we can compute prev-tag ranges.
  # Use a portable read loop (avoids `mapfile`, which is bash 4+ only).
  _all_tags=()
  while IFS= read -r _t; do
    _all_tags+=("$_t")
  done < <(git tag --sort=-v:refname)
  _limit=$RECENT_TAG_COUNT
  if [ "${#_all_tags[@]}" -lt "$_limit" ]; then
    _limit=${#_all_tags[@]}
  fi
  i=0
  while [ "$i" -lt "$_limit" ]; do
    tag="${_all_tags[$i]}"
    # Strip leading 'v' for matching against `## [X.Y.Z]` headers.
    version="${tag#v}"
    if ! grep -qE "^## \[${version}\]" CHANGELOG.md; then
      tag_date=$(git log -1 --format=%cs "$tag" 2>/dev/null || echo "?")
      next_idx=$((i + 1))
      if [ "$next_idx" -lt "${#_all_tags[@]}" ]; then
        prev_tag="${_all_tags[$next_idx]}"
        commit_count=$(git rev-list --count "${prev_tag}..${tag}" 2>/dev/null || echo "?")
      else
        # Oldest tag in the window: fall back to total reachable commits.
        commit_count=$(git rev-list --count "$tag" 2>/dev/null || echo "?")
      fi
      missing_tags+=("$tag ($tag_date, $commit_count commits)")
    fi
    i=$((i + 1))
  done

  if [ "${#missing_tags[@]}" -gt 0 ]; then
    echo "⚠️  CHANGELOG has no entry for the following recent tags:"
    for entry in "${missing_tags[@]}"; do
      echo "    $entry"
    done
  fi
fi
```

If any recent tag is missing an entry, surface the gap to the operator and offer the three-way choice. Interactive prompt format:

```
⚠️  CHANGELOG has no entry for the following recent tags:
    v0.10.0 (2026-06-05, 26 commits)
    v0.10.1 (2026-06-13, 14 commits)

Options:
  [b] Backfill these entries now (drafts entries via Phase 4 logic, one per gap)
  [c] Continue without backfill (leaves the gap in CHANGELOG.md)
  [a] Abort the release

Choose [b/c/a]:
```

### `[b]` Backfill path

For each missing tag (oldest gap first to preserve chronological order in the file):

1. Determine the previous shipped tag (the next-older tag in `git tag --sort=-v:refname`).
2. Reuse Phase 4's draft logic with the `<prev-tag>..<missing-tag>` commit range as input.
3. Present the draft to the operator for revisions exactly as Phase 4 does for the current release.
4. Insert the approved entry into `CHANGELOG.md` in the correct chronological slot (after the next-newer entry, before the next-older entry).
5. Commit each backfill as a separate `docs(changelog): backfill <version> entry` commit, or fold them all into a single `docs(changelog): backfill <X.Y.Z>, <A.B.C>` commit at the operator's preference.

Backfill commits land on `main` before the current-release flow continues — they do **not** become part of the new release tag.

### `[c]` Continue path

Acknowledge the gap and proceed to Phase 2. The gap remains in `CHANGELOG.md`; record nothing extra. This is the right choice for urgent fixes where the operator intends to backfill later.

### `[a]` Abort path

Stop the release. Exit cleanly with a one-line summary listing the missing tags so the operator can plan the backfill before the next attempt.

### `--yes` non-interactive mode

When the skill is invoked non-interactively (e.g., `--yes` flag or detected automation context), do **not** block:

- Print a single-line warning to stderr: `WARN: CHANGELOG missing entries for: v0.10.0, v0.10.1 (continuing — re-run interactively to backfill)`.
- Continue to Phase 2 (equivalent to the `[c]` path).

This keeps automated release pipelines unblocked while leaving an audit trail in the log.

### Tuning

- `RECENT_TAG_COUNT` (default 5) — number of most-recent tags to check. Override via env var for projects with non-weekly cadence.
- The gate scans only the **top N tags by semver descending**. Older gaps are out of scope; if you discover a deeper historical gap, file a separate backfill issue rather than letting it block the current release.

## Phase 2: Gather Changes

```bash
# Find the last release tag
git tag --sort=-v:refname | head -1

# Show current version
./scripts/version.sh

# List all commits since that tag
git log <last-tag>..HEAD --oneline

# Show the full diff stats
git diff <last-tag>..HEAD --stat
```

Present the user with:
- **Last release**: tag name, date, and version
- **Commits since release**: count and full list
- **Change summary**: categorized by conventional commit prefix (feat, fix, refactor, docs, test, chore)
- **Files changed**: high-level summary of which subsystems were touched

If there are zero commits since the last tag, stop and tell the user there's nothing to release.

## Phase 3: Semver Decision

Present a semver analysis. Reference https://semver.org. The categories below are generic — apply them to whatever public surface your project exposes (libraries, CLIs, protocols, file formats, etc.).

### Breaking Changes (MAJOR bump)
Scan for:
- Removed or renamed public API functions, types, or modules
- Changed function signatures or return types in exported surfaces
- Removed or renamed CLI commands, subcommands, or flags
- Changed CLI command behavior in a way that breaks scripted callers
- Changed wire-protocol / plugin-interface / IPC contracts
- Changed configuration file format in a non-backward-compatible way
- Removed or renamed environment variables that callers set

### New Capabilities (MINOR bump)
- New public API surface (functions, types, modules)
- New CLI commands, subcommands, or flags (additive, backward-compatible)
- New configuration options (with sensible defaults preserving old behavior)
- New optional plugin / protocol / IPC capabilities
- New roles, agents, or orchestration features

### Bug Fixes / Internal (PATCH bump)
- Bug fixes that don't change any public API
- Performance improvements with identical observable behavior
- Internal refactoring not visible to consumers
- Documentation updates
- Dependency bumps (unless they change observable behavior)

Present your recommendation and **ask the user to confirm or override**. Do not proceed until confirmed.

## Phase 4: Draft CHANGELOG

If `CHANGELOG.md` exists at the repo root, draft a new entry following its existing format. Study existing entries to match style.

```bash
# Check whether a CHANGELOG.md exists
if [ -f CHANGELOG.md ]; then
  echo "CHANGELOG.md found — drafting a new entry below ## [Unreleased]"
  head -50 CHANGELOG.md
else
  echo "No CHANGELOG.md found — offering to bootstrap one"
fi
```

If `CHANGELOG.md` is **absent** (e.g., a young repo that hasn't created one yet), ask the user: "No CHANGELOG.md found at the repo root. Create one with the standard 'Keep a Changelog' template? [Y/n]". If yes, write:

```markdown
# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [X.Y.Z] - YYYY-MM-DD

### Summary
<one-paragraph release theme>

### Added
- ...
```

If the user declines bootstrap, skip the CHANGELOG update and proceed with version bump only.

Key formatting rules (when `CHANGELOG.md` exists or has just been bootstrapped):
- Use `## [X.Y.Z] - YYYY-MM-DD` header with today's date
- Start with a `### Summary` paragraph describing the release theme
- Group changes under `### Added`, `### Changed`, `### Fixed`, `### Removed`, `### Renamed` as appropriate
- Reference issue numbers with `(#NNN)` format
- Keep descriptions concise but informative
- Omit empty sections

Present the draft and ask for revisions. Iterate until approved.

## Phase 5: Apply Changes

Once the user approves:

1. **Update CHANGELOG.md** (if it exists): Insert the new entry below `## [Unreleased]`.
2. **Discover the version-bearing files** so the user knows what will change:
   ```bash
   ./scripts/version.sh list
   ```
   This emits the canonical list, one path per line, straight from the script's source-of-truth array.
3. **Bump version**: Run `./scripts/version.sh bump <level> --tag`
   - This updates every file emitted by `./scripts/version.sh list`.
   - Any derived artifacts the script updates as a side effect (e.g., a lockfile via `cargo update` or `npm install`) are handled by the script itself.
   - The script creates the commit and tag automatically.
4. **Verify**: `./scripts/version.sh check`

Note: the version bump script creates the commit. To keep the CHANGELOG bump and the version bump together in a single tagged commit, commit the CHANGELOG first and then move the tag forward after the version bump:

```bash
git add CHANGELOG.md
git commit -m "docs: add X.Y.Z changelog entry"
./scripts/version.sh bump <level> --tag
# Move tag to include both commits
git tag -f vX.Y.Z
```

Show the user the result and ask for final confirmation.

## Phase 6: Push and Release

After final confirmation:

1. **Push commits and tag**:
   ```bash
   git push origin main --tags
   ```

2. **Create GitHub Release**:
   ```bash
   gh release create vX.Y.Z --title "vX.Y.Z" --notes-file - <<< "$(changelog excerpt)"
   ```
   Use the CHANGELOG entry as the release notes.

3. **Build workflow trigger** (only when a release workflow is configured):
   ```bash
   if ls .github/workflows/release.yml 2>/dev/null; then
     echo "release.yml detected — the GitHub Release will trigger the build workflow."
     gh run list --workflow=release.yml --limit 1
   else
     echo "No release.yml workflow detected — the GitHub Release will not trigger any build."
   fi
   ```

**Do not push or create the release without explicit user confirmation.**

## Phase 7: Post-Release Summary

Present a summary. Tailor the build-workflow line based on whether a release workflow was detected in Phase 6:

```
## Release Complete

- Version: vX.Y.Z
- Commit: <sha>
- Tag: vX.Y.Z
- GitHub Release: created
- Build workflow: [triggered / N/A — no release workflow configured]
- CHANGELOG: updated with N items
- Version files updated: $(./scripts/version.sh list | wc -l | tr -d ' ') files (see `./scripts/version.sh list`)
```

## Important Notes

- **Version script**: `scripts/version.sh` is the single source of truth for version management. Never manually edit version numbers — let the script update every tracked file plus any derived artifacts.
- **Discover, don't hardcode**: the set of version-bearing files is discovered at release time via `./scripts/version.sh list`. Do not bake a count or path list into prose; the script is authoritative.
- **Release workflow trigger** (when applicable): if `.github/workflows/release.yml` exists, it typically triggers on GitHub Release creation (`release: types: [created]`), NOT on tag push. In that case you must create a GitHub Release via `gh release create` to trigger the build. If no release workflow is configured, the tag push alone completes the release and no build artifacts are produced.
- **Conventional commits**: many projects (including this one if it uses `feat:` / `fix:` / `chore:` prefixes) use conventional commits to drive the semver decision. Use the prefix breakdown from Phase 2 as input to Phase 3.
- **Branch protection**: direct pushes to main from a release flow may show a ruleset bypass warning — this is expected for release commits when the project's policy allows admin bypass for tagged releases. If your project doesn't allow that, run the release through a PR instead.
