#!/usr/bin/env bash
# scripts/install/gitattributes.sh — install-metadata.json merge=ours wiring (#4528)
#
# `.loom/install-metadata.json` is a machine-local install stamp: every
# host's `resync-installed.sh` re-writes loom_version, loom_commit, and
# last_resync (plus loom_source, an absolute host-specific path) on every
# run. Because the file is COMMITTED (it must stay tracked — it is the
# authoritative ownership manifest consumed by verify-install.sh and
# uninstall-loom.sh), any two hosts that each commit a resync and then
# `git merge`/`git pull` the other's commit collide on this file, every
# time, on the exact same lines.
#
# The fix: a `merge=ours` attribute for the path, so a real conflict on this
# file always resolves to "keep our side" instead of stopping for manual
# resolution. This is safe specifically BECAUSE the file is fully re-derived
# by the next resync regardless of which side "wins" a given merge — nothing
# is permanently lost, only a stamp gets refreshed a cycle later than it
# otherwise would have.
#
# A `merge=ours` attribute alone does nothing: git-attributes(5) requires the
# `ours` driver to be defined in LOCAL (never committed) git config —
# `git config merge.ours.driver true`. `.gitattributes` is committed and
# shared; the driver config is not, so every write site below (fresh
# install, quick install, and resync) sets it, which also self-heals any
# existing install the first time one of those paths next runs.
#
# Source this file with:
#     source "$LOOM_ROOT/scripts/install/gitattributes.sh"
# then call:
#     ensure_install_metadata_merge_driver "$TARGET_PATH"

_GITATTRS_MERGE_BEGIN="# BEGIN LOOM-MANAGED (merge drivers, #4528)"
_GITATTRS_MERGE_END="# END LOOM-MANAGED (merge drivers, #4528)"
_GITATTRS_MERGE_RULE=".loom/install-metadata.json merge=ours"

ensure_install_metadata_merge_driver() {
  local target="$1"
  local ga="$target/.gitattributes"

  if [[ ! -f "$ga" ]] || ! grep -qF "$_GITATTRS_MERGE_RULE" "$ga" 2>/dev/null; then
    {
      [[ -s "$ga" ]] && printf '\n'
      printf '%s\n' "$_GITATTRS_MERGE_BEGIN"
      printf '%s\n' "# install-metadata.json is a machine-local install stamp (loom_version,"
      printf '%s\n' "# loom_commit, last_resync, loom_source) that every host's resync"
      printf '%s\n' "# re-writes -- always keep our side on a merge conflict; the file is"
      printf '%s\n' "# fully re-derived by the next resync regardless of which side \"wins\"."
      printf '%s\n' "$_GITATTRS_MERGE_RULE"
      printf '%s\n' "$_GITATTRS_MERGE_END"
    } >> "$ga"
  fi

  # Idempotent: only write local git config when not already set.
  local current
  current="$(git -C "$target" config --get merge.ours.driver 2>/dev/null || true)"
  if [[ "$current" != "true" ]]; then
    git -C "$target" config merge.ours.driver true 2>/dev/null || true
  fi
}
