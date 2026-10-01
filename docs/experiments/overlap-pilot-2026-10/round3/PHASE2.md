# Phase 2: patch-isolated outcomes and a narrow retrieval comparison

This extension is frozen after the paired topology diagnostic, before computing
patch-isolated outcomes or acquiring new predictions. It does not turn the
previously inspected cohort into a holdout.

## Patch-only endpoint

Use the same 121 pairs, retaining all exclusions. For each PR require its recorded
own commits to be a contiguous single-parent chain ending at the pinned head.
The parent of the first own commit is its patch base. Compute the net binary
patch from that base to the head, preserving deletions, modes, and renames.

Choose the unique Git merge base of the two patch bases. Reconstruct both patches
independently on it only when every changed path has exactly the same preimage
blob and mode as at that PR's patch base. Apply using a private Git index; require
each changed postimage to equal the original PR head. Publish reconstruction
failures as unknown, with path and reason. These strict checks intentionally
trade coverage for a well-defined counterfactual.

Create synthetic commits for the two reconstructed trees with that same parent;
run both merge orientations. This endpoint asks whether the two **final net
patches**, transplanted without alteration to the declared common source, have a
textual collision. It does not recover pre-repair heads or semantic interactions.
Compare with both previous instruments, and report full counts including unknown.

## Historical title/body footprint qualification

Independently audit the 242 issue/PR records for a narrower recoverable task:
predict the final net changed-file footprint from an unedited issue title/body.

- Query authoritative issue metadata, preserving the response and observation time.
- Require `lastEditedAt` to be null, and current title/body to equal the saved
  input. Omit comments explicitly; do not claim this captures all requirements.
- Require the PR's contiguous single-parent own-commit chain; exclude merges,
  empty patches, and inconsistent head identity.
- Choose the latest first-parent main commit whose committer date is at or before
  issue creation. Also require author date <= creation, main ancestry, and source
  ancestry of the patch base. Reject source ancestry containing the PR's head.
- Require issue creation before the earliest author AND committer timestamp of
  all own commits. Timestamps/forge metadata support reconstruction, but cannot
  prove the absence of earlier unobserved implementation or model memorization.

This is a retrospective test of today's retrieval on historical inputs, not a
prospective dispatch study. Publish qualified/excluded counts and reasons before
choosing cases. Select at most 12 eligible issue IDs by ascending SHA-256 of
`round3-footprint-v1:<issue>`, independent of changed-file count and retrieval
results. No fitted thresholds or train/test claims at this size.

## Frozen comparisons, if credentials and index budgets permit

For exactly those cases compare two-query Augment DirectContext (implementation
sites; tests/consumers), a literal issue-path baseline, and BM25 over the identical
pinned regular-text file corpus. Record policy, exact query text/options, complete
corpus manifest, exclusions and checksums before scoring any prediction.

Use top-k file metrics at k=5,10,20 and the union of all returned files. Rank
retrieval files by first appearance in the frozen two-query response order;
BM25 uses standard k1=1.2, b=0.75, lowercase alphanumeric/underscore tokens split
on underscores, no learned stop list. Ties are path-lexicographic. No tuning.
Report per-issue set precision/recall and macro means, paired differences and
descriptive uncertainty; a small selected historical corpus cannot calibrate
conflict probabilities or justify enforcement. No line/symbol metrics this round.

Protect the evaluation boundary: retrieval and BM25 receive issue inputs and the
historical tree only; outcome processing reads actual PR patches afterwards.
Persist raw responses and explicit query errors. A failed retrieval is unavailable,
not empty success. Report file coverage of the index separately; do not silently
restrict recall's actual-change denominator to indexable files.
