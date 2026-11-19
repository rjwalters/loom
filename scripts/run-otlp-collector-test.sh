#!/usr/bin/env bash
# The one definition of the `otlp_collector --ignored` invocation (#12396).
#
# release.yml ("Decode released binary telemetry"), ci-daily.yml and ci.yml's
# `otlp-collector` job all call this, so a change that breaks the command breaks
# every one of them -- PR CI can no longer miss a failure the release hits
# (v0.19.1168: #12123 compiled a deliberately leaky fixture into the `--ignored`
# run; only release.yml ran it).
#
# Extra arguments (e.g. `--release --target <t>`) pass through to `cargo test`,
# before the `--test` selection. Needs Docker and the pinned collector image.
set -euo pipefail

exec cargo test --package loom-daemon --features otlp "$@" \
  --test otlp_collector -- --ignored --nocapture
