#!/usr/bin/env bash
# release-decision.sh — whether a green `main` CI run releases its commit (#10826).
#
# `.github/workflows/release.yml` runs on `workflow_run` when the `CI` workflow
# completes. This script is the pure decision behind its `resolve` job: the
# workflow gathers the facts (event fields, VERSION at the tested commit, the
# existing tags, ancestry against `main`) and acts on the answer; nothing here
# touches the network, so every case is unit-tested by test-release-decision.sh.
#
# Inputs (environment):
#   WR_CONCLUSION    github.event.workflow_run.conclusion
#   WR_EVENT         github.event.workflow_run.event
#   WR_HEAD_BRANCH   github.event.workflow_run.head_branch
#   WR_HEAD_REPO     github.event.workflow_run.head_repository.full_name
#   REPO             github.repository
#   HEAD_SHA         github.event.workflow_run.head_sha (the TESTED commit)
#   VERSION_AT_HEAD  `VERSION` read from HEAD_SHA's tree (never from main's tip)
#   EXISTING_TAGS    newline-separated tag names already on the remote
#   RELEASE_EXISTS   `true` when a Release named v<VERSION_AT_HEAD> exists
#   ANCESTRY         compare status of HEAD_SHA...main: identical|ahead|behind|
#                    diverged; empty = could not be determined
#
# Output (stdout, key=value): decision=publish|skip|refuse, tag=, sha=, reason=
#
# Exit: 0 decision made (publish or skip); 1 refuse — the run is not a green
# push to this repo's main, which the job `if:` should already have excluded,
# so reaching here is a wiring bug and must be red; 2 bad or unknown input
# (fails closed: nothing is released on a guess). Shell, not a loom-daemon
# subcommand, because it runs in a hosted job before any binary is built —
# building the daemon to decide whether to build the daemon would be circular.
set -euo pipefail

emit() { # decision reason
  printf 'decision=%s\ntag=%s\nsha=%s\nreason=%s\n' "$1" "${tag:-}" "${HEAD_SHA:-}" "$2"
}

tag=""
conclusion="${WR_CONCLUSION:-}"
if [[ "$conclusion" != "success" ]]; then
  emit refuse "CI concluded '${conclusion:-<none>}', not success: only a green main commit is released"
  exit 1
fi
if [[ "${WR_EVENT:-}" != "push" ]]; then
  emit refuse "CI run event is '${WR_EVENT:-<none>}', not push: PR and merge-queue runs never release"
  exit 1
fi
if [[ "${WR_HEAD_BRANCH:-}" != "main" ]]; then
  emit refuse "CI run branch is '${WR_HEAD_BRANCH:-<none>}', not main"
  exit 1
fi
if [[ -z "${REPO:-}" || "${WR_HEAD_REPO:-}" != "$REPO" ]]; then
  emit refuse "CI run head repository '${WR_HEAD_REPO:-<none>}' is not '${REPO:-<none>}'"
  exit 1
fi

if [[ ! "${HEAD_SHA:-}" =~ ^[0-9a-f]{40}$ ]]; then
  emit refuse "HEAD_SHA '${HEAD_SHA:-}' is not a 40-hex commit"
  exit 2
fi
version="${VERSION_AT_HEAD:-}"
if [[ ! "$version" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)$ ]]; then
  emit refuse "VERSION at ${HEAD_SHA:0:12} is '${version}', not X.Y.Z"
  exit 2
fi
tag="v${version}"

# 0 when $1 (X.Y.Z) is strictly greater than $2 (X.Y.Z); numeric per field.
version_gt() {
  local IFS=.
  # shellcheck disable=SC2206
  local a=($1) b=($2) i
  for i in 0 1 2; do
    ((10#${a[$i]} > 10#${b[$i]})) && return 0
    ((10#${a[$i]} < 10#${b[$i]})) && return 1
  done
  return 1
}

if [[ "${RELEASE_EXISTS:-}" == "true" ]] || grep -qxF -- "$tag" <<<"${EXISTING_TAGS:-}"; then
  emit skip "${tag} already exists (Release or tag): this version is already released"
  exit 0
fi

higher=""
while IFS= read -r t; do
  [[ "$t" =~ ^v([0-9]+\.[0-9]+\.[0-9]+)$ ]] || continue
  if version_gt "${BASH_REMATCH[1]}" "$version" && { [[ -z "$higher" ]] || version_gt "${BASH_REMATCH[1]}" "${higher#v}"; }; then
    higher="$t"
  fi
done <<<"${EXISTING_TAGS:-}"
if [[ -n "$higher" ]]; then
  emit skip "${higher} is already released and is newer than ${tag}: a late green run never publishes an older version"
  exit 0
fi

case "${ANCESTRY:-}" in
  identical|ahead) ;;
  behind|diverged)
    emit skip "${HEAD_SHA:0:12} is not reachable from main (compare: ${ANCESTRY}): history was rewritten, nothing is released"
    exit 0
    ;;
  *)
    emit refuse "could not determine whether ${HEAD_SHA:0:12} is reachable from main (compare: '${ANCESTRY:-}')"
    exit 2
    ;;
esac

emit publish "green main CI at ${HEAD_SHA:0:12} carries unreleased VERSION ${version}"
