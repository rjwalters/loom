# Output-budget observation, recorded before outcome scoring

Acquisition completed for the frozen 12 cases. All 24 provider queries returned
text, with zero returned paths outside the pinned index. Eighteen response strings
reached the requested 20,000-character budget (the SDK may add formatting beyond
the limit). These are output-budget flags, not authentication or transport failures.

The frozen primary inclusion rule in `EVALUATION.md` excludes such responses.
It therefore leaves **one complete case**, too little for a useful primary
comparison. Preserve that primary result and its missingness honestly. Do not
overwrite the original acquisition status or call the 11 capped cases complete.

Before examining target-file scores, add a **post-protocol, finite-budget
sensitivity analysis**: evaluate the paths actually returned within the fixed
query budget for all 12 cases against BM25 and literal paths, using the same
metrics and k values. Availability in this sensitivity means both queries returned
parseable text, not that retrieval exhaustively covered every relevant location.
Unknown unreturned evidence remains unknown; this comparison evaluates the
operating budget's returned suggestions only.

Publish the strict primary and this sensitivity side by side, with the protocol
change explicit. There is no threshold selection, case replacement, query rewrite,
or extra provider call. The sensitivity is exploratory and cannot be promoted to
an untouched confirmatory result. A future independently sampled evaluation should
pre-register finite-budget semantics from the outset and distinguish response
completion, result coverage, and actual provider failures.
