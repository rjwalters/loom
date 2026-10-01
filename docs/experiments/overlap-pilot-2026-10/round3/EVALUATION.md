# Frozen evaluation for the 12-issue footprint study

Primary descriptive comparison: **macro recall at 10 files, Augment minus BM25**.
Secondary views: precision at requested k, precision among returned files, recall
at k=5/20, and literal-path suggestions. Report every view; do not pick a winner
by searching k afterwards. No scheduling or conflict-probability conclusion is
permitted from file-footprint performance.

The actual target is the net changed-path set of the qualified contiguous own
commit chain. It includes new paths, deleted paths, and both old/new names for
renames (`git diff --no-renames --name-only`), including paths absent from the
historical index. Report indexable-target coverage separately. All denominators
are explicit. For k:

- `precision_at_k = hits / k`, including an unfilled result slot as no hit.
- `precision_returned = hits / number_returned`, undefined for zero returned.
- `recall = hits / number_actual_paths`.

Deduplicate paths before truncating rankings. A known empty baseline is a valid
empty prediction. Provider failures, partial queries, and output-limit cases
remain unavailable for the primary paired comparison, never zero-success rows;
publish excluded case IDs and coverage.

Report paired per-issue differences and macro/micro summaries. Include a uniform
random-from-the-same-corpus expectation as a sanity reference, not as a strong
baseline. Provide descriptive 95% bootstrap ranges by resampling whole source-
revision groups (seed 20261001, 5,000 resamples), preserving equal weighting of
issues in each resample. Shared source revision is only a partial dependence
control; component/episode dependence, narrow selection, and the small number of
groups preclude a generalization or significance claim.

Independently verify set metrics on hand-enumerated cases before scoring the
study: exact hit, disjoint sets, duplicate predictions, short rankings, empty
known predictions, and a new actual path absent from the corpus. Require all
precision/recall values in [0,1]. These are measurement invariants, not proof
of the retrieval hypothesis. Run verification and evaluation on AWS.

The acquisition command receives only frozen issue inputs and pinned source.
The evaluator reads outcomes after acquisition completes and validates the
saved input/index/response hashes. Exact provider/model internals and billed
cost are unexposed by the SDK; label them unknown rather than inventing a value.
