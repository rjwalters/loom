#!/usr/bin/env bash
# FROZEN COPY of merge-pr.sh's closing-reference analysis as it stood
# immediately before #8191 ported it to Rust.
#
# This is a TEST FIXTURE, not a live script. Nothing sources it in production.
#
# It exists so `tests/merge_pr_refs_differential.rs` can keep comparing the
# Rust against the exact implementation it replaced, forever, rather than only
# at the moment of the port. Reading the functions out of the live merge-pr.sh
# stopped being possible the instant that file started delegating; reading them
# from git history would pin the test to a moving ref.
#
# DO NOT "fix" anything here. Its value is being a faithful record of the
# retired behaviour, including the behaviour that is arguably wrong (an
# unclosed fence swallowing the rest of the body, the closing-keyword scan not
# stripping fences at all). If the Rust should diverge from this, that is a
# deliberate behaviour change that belongs in its own issue, and this file
# should be left alone while the test's expectation is updated with a comment
# saying why.
_strip_fenced_code_blocks() {
  awk '
    /^[[:space:]]*```/ { infence = !infence; next }
    !infence { print }
  '
}

_partial_increment_refs() {
  { printf '%s\n' "$1" \
      | _strip_fenced_code_blocks \
      | sed -E 's/`[^`]*`//g' \
      | grep -oiE '^[[:space:]]*([-*+>]|[0-9]+\.)?[[:space:]]*(Part of|Contributes to)[[:space:]]+#[0-9]+' \
      | grep -oE '#[0-9]+' \
      | tr -d '#' \
      | sort -un; } || true
}

_closing_refs_stdin() {
  { grep -oiE '\b(close[sd]?|fix(e[sd])?|resolve[sd]?)\b[[:space:]]+#[0-9]+' \
      | grep -oE '[0-9]+' \
      | sort -un; } || true
}

_body_closing_refs() {
  { printf '%s\n' "$1" | _closing_refs_stdin; } || true
}

_closing_ref_snippets() {
  { printf '%s\n' "$1" \
      | grep -oiE "\\b(close[sd]?|fix(e[sd])?|resolve[sd]?)\\b[[:space:]]+#$2\\b" \
      | sort -u | tr '\n' '|' | sed 's/|$//; s/|/", "/g'; } || true
}

_partial_increment_ref_snippets() {
  { printf '%s\n' "$1" \
      | _strip_fenced_code_blocks \
      | sed -E 's/`[^`]*`//g' \
      | grep -oiE "^[[:space:]]*([-*+>]|[0-9]+\\.)?[[:space:]]*(Part of|Contributes to)[[:space:]]+#$2\\b" \
      | sed -E 's/^[[:space:]]+//' \
      | sort -u | tr '\n' '|' | sed 's/|$//; s/|/", "/g'; } || true
}
