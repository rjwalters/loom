#!/usr/bin/env bash
# test-resync-installed-manifest-prune.sh - prune_stale_manifest_entries()
# drops stale install-metadata.json `installed_files` entries (#14012).
#
# When Loom retires a defaults/ file, remove_retired_files() deletes the
# installed copy but the installer-written manifest entry stayed behind, so
# the manifest over-claimed (16 stale entries observed in one consumer repo,
# stable across two commits). The prune reconciles `installed_files` against
# the live source tree at resync time: an entry is dropped only when it is a
# pure-copy-surface path, absent on disk, AND its defaults/ counterpart is
# gone upstream.
#
# This suite pins the four-way classification, because every wrong quadrant
# has a real cost:
#   - pruning a file that still exists on disk destroys the record of a
#     pinned fork (the consumer deliberately kept it);
#   - pruning an entry whose defaults/ counterpart still exists destroys the
#     installer's record of a file a later resync must be able to restore;
#   - leaving a genuinely retired entry in place is the original #14012 bug
#     (the manifest over-claims forever, and a one-off consumer prune only
#     helps until the next retirement lands);
#   - touching an install-once entry (.github/workflows/*) prunes a file
#     resync does not own and cannot re-verify.
#
# Usage:
#   ./.loom/scripts/tests/test-resync-installed-manifest-prune.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HELPERS_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
SCRIPT="$HELPERS_DIR/resync-installed.sh"

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib/assert.sh"

WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/test-resync-manifest-prune.XXXXXX")"
# shellcheck disable=SC2329  # invoked indirectly via the EXIT trap below
cleanup() { rm -rf "$WORKDIR" 2>/dev/null || true; }
trap cleanup EXIT

export GIT_AUTHOR_NAME="test" GIT_AUTHOR_EMAIL="test@example.com"
export GIT_COMMITTER_NAME="test" GIT_COMMITTER_EMAIL="test@example.com"

META_REL=".loom/install-metadata.json"

# has_entry <repo> <path>  -- is <path> listed in installed_files?
has_entry() {
    jq -e --arg p "$2" '.installed_files | index($p) != null' "$1/$META_REL" >/dev/null 2>&1
}

# entry_count <repo>
entry_count() {
    jq -r '.installed_files | length' "$1/$META_REL" 2>/dev/null
}

# --- consumer fixture --------------------------------------------------------
#
# make_fixture <name>
#   A consumer repo resolved from a real source checkout via
#   .loom/loom-source-path (the standard consumer shape), seeded with the full
#   four-way classification in its manifest:
#
#   .loom/hooks/retired.sh      absent on disk, absent upstream   -> PRUNE
#   .loom/hooks/keeper.sh       present on disk, shipped         -> keep
#   .loom/docs/pinned-doc.md    locally deleted, still shipped,
#                               pinned in .loom/resync-ignore    -> keep
#   .loom/docs/restored-doc.md  locally deleted, still shipped,
#                               NOT pinned                       -> keep
#                               (resync restores the file before
#                               the prune runs)
#   .github/workflows/x.yml     absent on disk, install-once,
#                               not a pure-copy surface          -> keep
#
#   The commit subject is a routine install subject, not "fixture":
#   resync-installed.sh's local-fix guard (#7864) refuses to run over a
#   non-routine subject.
make_fixture() {
    local name="$1"
    local consumer="$WORKDIR/$name" src="$WORKDIR/$name-src"
    rm -rf "$consumer" "$src"

    mkdir -p "$src/defaults/hooks" "$src/defaults/docs"
    printf 'A\n' > "$src/defaults/hooks/keeper.sh"
    chmod +x "$src/defaults/hooks/keeper.sh"
    printf 'PINNED\n' > "$src/defaults/docs/pinned-doc.md"
    printf 'RESTORED\n' > "$src/defaults/docs/restored-doc.md"
    printf '{\n  "version": "1.2.3"\n}\n' > "$src/package.json"
    git -C "$src" init -q
    git -C "$src" add -A >/dev/null 2>&1
    git -C "$src" commit -qm "source" >/dev/null 2>&1

    mkdir -p "$consumer/.loom/hooks" "$consumer/.loom/scripts" "$consumer/.loom/docs"
    git -C "$consumer" init -q
    printf 'A\n' > "$consumer/.loom/hooks/keeper.sh"
    printf 'PINNED\n' > "$consumer/.loom/docs/pinned-doc.md"
    printf 'RESTORED\n' > "$consumer/.loom/docs/restored-doc.md"
    rm "$consumer/.loom/docs/pinned-doc.md" "$consumer/.loom/docs/restored-doc.md"
    printf 'docs/pinned-doc.md  # local fork: keep the deletion\n' > "$consumer/.loom/resync-ignore"
    printf '%s\n' "$src" > "$consumer/.loom/loom-source-path"
    printf '{\n  "loom_version": "0.19.100",\n  "loom_commit": "aaaaaaaabbbbbbbbccccccccddddddddeeeeeeee",\n  "install_date": "2020-01-01",\n  "loom_source": "%s",\n  "installed_files": [\n    ".loom/hooks/retired.sh",\n    ".loom/hooks/keeper.sh",\n    ".loom/docs/pinned-doc.md",\n    ".loom/docs/restored-doc.md",\n    ".github/workflows/x.yml"\n  ]\n}\n' \
        "$src" > "$consumer/$META_REL"
    git -C "$consumer" add -A >/dev/null 2>&1
    git -C "$consumer" commit -qm "chore: install Loom v0.19.100" >/dev/null 2>&1
    echo "$consumer"
}

# --- (1) the prune itself -----------------------------------------------------
echo "Test group 1: a retired+absent manifest entry is pruned; every other quadrant is kept (#14012)"
REPO1="$(make_fixture prune-apply)"
OUT="$(cd "$REPO1" && bash "$SCRIPT" 2>&1)"
RC=$?
if [[ $RC -eq 0 ]]; then
    pass "(#14012) the resync run exits 0"
else
    fail "(#14012) the resync run exited $RC (out=$OUT)"
fi
if grep -q "manifest entry \.loom/hooks/retired\.sh" <<<"$OUT" && grep -q "retired upstream, #14012" <<<"$OUT"; then
    pass "(#14012) the prune is logged, naming the path and the reason"
else
    fail "(#14012) the prune was silent (out=$OUT)"
fi
if ! grep -q "retired\.sh" < <(jq -r '.installed_files[]' "$REPO1/$META_REL" 2>/dev/null); then
    pass "(#14012) the retired+absent entry is gone from installed_files"
else
    fail "(#14012) the retired+absent entry survived the prune"
fi
if has_entry "$REPO1" ".loom/hooks/keeper.sh"; then
    pass "(#14012) a present-on-disk entry is never pruned"
else
    fail "(#14012) a present-on-disk entry was pruned"
fi
if has_entry "$REPO1" ".loom/docs/pinned-doc.md"; then
    pass "(#14012) a shipped-but-locally-deleted (pinned) entry is never pruned"
else
    fail "(#14012) a shipped-but-locally-deleted entry was pruned"
fi
if has_entry "$REPO1" ".loom/docs/restored-doc.md"; then
    pass "(#14012) a shipped-but-locally-deleted (restored this run) entry is never pruned"
else
    fail "(#14012) an entry whose file the run restored was pruned"
fi
if [[ -f "$REPO1/.loom/docs/restored-doc.md" ]]; then
    pass "(#14012) the still-shipped deleted file was restored by the sync, not pruned away"
else
    fail "(#14012) the still-shipped deleted file was not restored"
fi
if has_entry "$REPO1" ".github/workflows/x.yml"; then
    pass "(#14012) an install-once entry outside the pure-copy surfaces is left untouched"
else
    fail "(#14012) an install-once entry was pruned"
fi
if [[ "$(entry_count "$REPO1")" == "4" ]]; then
    pass "(#14012) exactly the one stale entry was dropped (5 -> 4)"
else
    fail "(#14012) installed_files length is '$(entry_count "$REPO1")', expected 4"
fi

# --- (2) a run with nothing to prune is a byte-level no-op --------------------
#
# The #11680 contract: a resync that changed nothing must not touch the
# tracked manifest, or every host re-commits it forever. After the prune the
# manifest is consistent, so the next run must leave it byte-identical (the
# #11680 no-op keeps the volatile stamp fields; there is nothing left to
# prune).
echo ""
echo "Test group 2: a follow-up run with nothing to prune leaves the manifest byte-identical (#14012, #11680)"
BEFORE_CKSUM="$(cksum < "$REPO1/$META_REL")"
OUT2="$(cd "$REPO1" && bash "$SCRIPT" 2>&1)"
if [[ "$(cksum < "$REPO1/$META_REL")" == "$BEFORE_CKSUM" ]]; then
    pass "(#14012) the manifest is byte-identical after a run with nothing to prune"
else
    fail "(#14012) a no-prune run rewrote the manifest (#11680 churn)"
fi
if ! grep -q "pruned manifest entry" <<<"$OUT2"; then
    pass "(#14012) the follow-up run reports no prune"
else
    fail "(#14012) the follow-up run pruned something that should already be gone"
fi

# --- (3) --dry-run previews without writing ----------------------------------
echo ""
echo "Test group 3: --dry-run prints 'would prune' and does not modify the manifest (#14012)"
REPO3="$(make_fixture prune-dry-run)"
BEFORE="$(cat "$REPO3/$META_REL")"
OUT3="$(cd "$REPO3" && bash "$SCRIPT" --dry-run 2>&1)"
RC3=$?
if grep -q "would prune manifest entry .loom/hooks/retired\.sh" <<<"$OUT3"; then
    pass "(#14012) --dry-run previews the prune with the 'would prune' verb"
else
    fail "(#14012) --dry-run did not preview the prune (out=$OUT3)"
fi
if [[ "$(cat "$REPO3/$META_REL")" == "$BEFORE" ]]; then
    pass "(#14012) --dry-run left the manifest byte-identical"
else
    fail "(#14012) --dry-run MODIFIED the manifest"
fi
if [[ $RC3 -eq 2 ]]; then
    pass "(#14012) a pending prune is reported as drift (exit 2), not 'already in sync'"
else
    fail "(#14012) --dry-run with a pending prune exited $RC3, expected 2 (out=$OUT3)"
fi

# --- (4) the apply does exactly what the preview named -----------------------
#
# The plan is only useful to a consumer if it MATCHES: run the same fixture
# --dry-run, then for real, and assert both name the same single path.
echo ""
echo "Test group 4: the apply prunes exactly what the preview named (#14012)"
REPO4="$(make_fixture prune-match)"
PREVIEW="$(cd "$REPO4" && bash "$SCRIPT" --dry-run 2>&1 | sed -n 's/^  .*would prune[^ ]* manifest entry \([^ ]*\).*/\1/p')"
APPLY="$(cd "$REPO4" && bash "$SCRIPT" 2>&1 | sed -n 's/^  .*pruned[^ ]* *manifest entry \([^ ]*\).*/\1/p')"
if [[ -n "$PREVIEW" && "$PREVIEW" == "$APPLY" ]]; then
    pass "(#14012) preview and apply name the same path ('$PREVIEW')"
else
    fail "(#14012) preview named '$PREVIEW' but apply pruned '$APPLY'"
fi
if ! has_entry "$REPO4" ".loom/hooks/retired.sh" && [[ "$(entry_count "$REPO4")" == "4" ]]; then
    pass "(#14012) the apply dropped exactly the previewed entry"
else
    fail "(#14012) the apply did not drop exactly the previewed entry"
fi

# --- summary -----------------------------------------------------------------
echo ""
echo "========================================"
echo "Results: $TESTS_PASSED/$TESTS_RUN passed"
echo "========================================"
if [[ $TESTS_FAILED -gt 0 ]]; then
    echo -e "${RED}$TESTS_FAILED test(s) failed${NC}"
    exit 1
fi
echo -e "${GREEN}All tests passed${NC}"
exit 0
