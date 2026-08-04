# Release policy — loom

Procedural steps `/repo:release` (from `rjwalters/repo`, co-installed under
`.claude/commands/repo/release.md` — that file is gitignored in *this* repo,
see `.gitignore`'s `.claude/commands` entry, so it cannot be edited here) binds
at named seams. See `/repo:release`'s own "Extension points" section for the
seam contract. This file exists to close the CHANGELOG maintenance gaps
described in #5196 without forking the shared command.

## seam: pre-changelog-style

Use `./scripts/changelog.sh draft <last-tag>..HEAD` to generate the mechanical
skeleton for this release's entry: it parses conventional commits in the
range and buckets them into `### Added` / `### Changed` / `### Removed` /
`### Fixed` (dropping non-shipping `test`/`chore`/`ci`/`build` commits, and
surfacing anything with an unrecognized or missing conventional prefix under
`### Other` rather than dropping it) with every `(#NNN)` ref preserved
verbatim. Treat that output as the raw material only — regroup it into the
thematic sub-sections this project's CHANGELOG already uses (see the existing
entries for the house style) and write the `### Summary` paragraph yourself;
the script does not (and should not) generate that narrative.

**Keep `## [Unreleased]` alive across the fold.** The default draft step
folds an existing `## Unreleased` heading into the new version heading by
renaming it in place — that rename is exactly what left this project's
CHANGELOG with *no* `## [Unreleased]` heading at all after v0.18.0 (#5196):
nothing re-added it, so the section stayed missing until the next release
reconstructed everything from git log by hand. Immediately after the fold
consumes `## Unreleased` (i.e. as part of producing the string that Phase 5
inserts), re-insert a **fresh, empty** `## [Unreleased]` heading directly
above the newly-named version heading — where `## Unreleased` used to sit.
Do this every release, even when Unreleased had no accumulated content to
fold, so the heading is a permanent fixture rather than something the next
release has to remember to add back.

## seam: pre-push

Before pushing, re-verify CHANGELOG.md completeness against the range that is
**actually about to ship** — not whatever range Phase 3/4 drafted earlier in
this session. By this point Phase 5 has already created the version commit
and tag, so `$last..v$NEW` (the previous release tag through the tag Phase 5
just created) is now fixed and immutable; recompute it fresh rather than
reusing an earlier variable, since main can advance between drafting and
tagging in a long interactive release session (observed on v0.18.0: 5 commits
landed mid-release and had to be caught and re-folded by hand):

```bash
./scripts/changelog.sh verify "$last..v$NEW" CHANGELOG.md
```

- **No `MISSING:` lines** — proceed with the push.
- **Any `MISSING: #NNN <- <subject>` lines** — a shipping commit in the final
  tagged range has no trace anywhere in CHANGELOG.md. Before pushing (nothing
  is public yet, so this is still cheap to fix): decide whether it was an
  intentional editorial consolidation (multiple commits folded into one
  curated bullet that cites a different, representative `#NNN` — expected and
  fine) or a genuine drop. For a genuine drop, regenerate the raw material
  with `./scripts/changelog.sh draft "$last..v$NEW"`, fold the missing item(s)
  into the already-written entry, amend the version commit
  (`git commit --amend`), and re-tag (`git tag -f -a "v$NEW" -m "v$NEW"`)
  before proceeding to push. Never push first and fix after — the tag is
  public the moment it's pushed.

## Link-reference block

Not a seam (nothing in `/repo:release` reads it), recorded here for
provenance: CHANGELOG.md's trailing `[Unreleased]: …` / `[0.2.3]: …` /
`[0.2.0]: …` / `[0.1.0]: …` reference-link block was dropped in #5196 rather
than backfilled. Only 4 of 19 released versions ever had one (0.3.0 through
0.18.0 never did), so the Keep-a-Changelog reference-link convention was
already abandoned in practice; backfilling comparison URLs for all 15 missing
versions to resurrect a convention this project never consistently followed
was judged not worth the upkeep it would then require on every future
release. Do not re-add a partial block — either maintain it for every version
going forward or leave it out entirely.
