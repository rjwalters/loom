#!/usr/bin/env bash
# memory-budget.sh — cross-platform total-memory detection + per-sweep memory
# budget math, mirroring lib/cpu-budget.sh's shape (issue #5111/#5979) for the
# memory axis (issue #7430, epic #6896 Phase 3: per-sweep resource limits and
# containment observability).
#
# Motivation: spawn-claude.sh's containerized dispatch mode (issue #7429)
# shipped with no `--cpus`/`--memory` docker flags by its own explicit scope
# note — a containerized sweep could still exhaust host memory even though
# CPU is already budgeted (issue #5111) and divided across concurrent sweeps
# (issue #5979). This gives the memory axis the same treatment: a per-sweep
# share of the host's usable RAM, divided across sweeps currently in flight,
# so N concurrent containerized sweeps' declared `--memory` caps sum to no
# more than the host's usable total — never a repeat of the #5979 "each
# sweep independently claims the whole budget" bug, this time on the memory
# axis instead of CPU.
#
# Source this file (do not exec). Defines:
#
#   loom_mem_total_mb
#       Echoes the host's total physical memory in MiB. Resolution order:
#       `/proc/meminfo`'s `MemTotal:` line (Linux, reported in KiB) ->
#       `sysctl -n hw.memsize` (macOS/BSD, reported in bytes) -> `4096` (a
#       conservative 4 GiB last-resort fail-safe). Never echoes 0 or a
#       non-numeric value — every caller does budget arithmetic on the
#       result.
#
#   loom_mem_budget_mb <total_mb> <reserved_mb> [in_flight_sweeps]
#       Echoes max(512, floor(max(512, total_mb - reserved_mb) /
#       in_flight_sweeps)) — this sweep's SHARE of the host's memory, mirroring
#       `loom_cpu_budget_cores`'s division (issue #5979). `in_flight_sweeps`
#       defaults to 1. Always at least 512 MiB, so a small host or a large
#       concurrent-sweep count never computes a budget too small for a
#       container to even start.

loom_mem_total_mb() {
    local kb="" bytes="" mb=""
    if [[ -r /proc/meminfo ]]; then
        kb="$(awk '/^MemTotal:/ { print $2; exit }' /proc/meminfo 2>/dev/null || true)"
        if [[ "$kb" =~ ^[0-9]+$ ]] && ((kb > 0)); then
            mb=$((kb / 1024))
        fi
    fi
    if ! [[ "$mb" =~ ^[0-9]+$ ]] || [[ "$mb" -eq 0 ]]; then
        bytes="$(sysctl -n hw.memsize 2>/dev/null || true)"
        if [[ "$bytes" =~ ^[0-9]+$ ]] && ((bytes > 0)); then
            mb=$((bytes / 1024 / 1024))
        fi
    fi
    if ! [[ "$mb" =~ ^[0-9]+$ ]] || [[ "$mb" -eq 0 ]]; then
        mb=4096
    fi
    echo "$mb"
}

loom_mem_budget_mb() {
    local total="$1" reserved="$2" in_flight="${3:-1}"
    if ! [[ "$total" =~ ^[0-9]+$ ]]; then
        total=4096
    fi
    if ! [[ "$reserved" =~ ^[0-9]+$ ]]; then
        reserved=0
    fi
    if ! [[ "$in_flight" =~ ^[0-9]+$ ]] || ((in_flight < 1)); then
        in_flight=1
    fi
    local budget=$((total - reserved))
    if ((budget < 512)); then
        budget=512
    fi
    budget=$((budget / in_flight))
    if ((budget < 512)); then
        budget=512
    fi
    echo "$budget"
}

# --- Per-scope MemoryMax from the observed per-repo peak (issue #11094, slice 2) ---
#
# The daemon's work finder records every agent scope's `memory.peak` into a
# per-repo rolling history (`~/.loom/ram-peaks.json`, loom-daemon ram_peaks.rs,
# `repos: {<repo dir name>: [peak_bytes...]}`) and charges admission with it.
# This is the containment half: a scope gets `MemoryMax` sized from the SAME
# history so an over-budget build (a 13 GB `rustc`) is OOM-killed INSIDE its
# own cgroup — with `OOMPolicy=continue` only that command fails and the agent
# sees the error — instead of the kernel's global OOM killer picking a victim.
#
# MemoryMax, not MemoryHigh: MemoryHigh only throttles/reclaims and never
# kills, so on a no-swap host a runaway build would stall instead of failing.
#
# Conservative by construction: NO history (or no jq / unreadable file) means
# NO limit, so a fresh repo behaves exactly as before. With history the limit
# is max(floor, high-water * pct/100); defaults pct=200, floor=4096 MiB.
#
#   LOOM_SWEEP_MEMORY_MAX            0|off|false|no disables; a positive
#                                    integer = explicit MiB limit (no history
#                                    needed); unset = derive from history.
#   LOOM_SWEEP_MEMORY_MAX_PCT        multiple of the observed peak (default 200).
#   LOOM_SWEEP_MEMORY_MAX_MIN_MB     floor for a derived limit (default 4096).
#   LOOM_RAM_PEAKS_PATH              history file (same override as the daemon).
#
# loom_mem_scope_limit_mb <repo_key> [total_mb]
#   Echoes the MemoryMax in MiB, or nothing when no limit should be applied
#   (disabled, no history, or the limit would be >= host RAM and so pointless).
loom_mem_scope_limit_mb() {
    local repo="$1" total_mb="${2:-0}" setting="${LOOM_SWEEP_MEMORY_MAX:-}"
    local limit="" pct="${LOOM_SWEEP_MEMORY_MAX_PCT:-200}" floor="${LOOM_SWEEP_MEMORY_MAX_MIN_MB:-4096}"
    case "$setting" in
        0 | off | false | no | OFF | FALSE | NO) return 0 ;;
    esac
    [[ "$pct" =~ ^[0-9]+$ ]] && ((pct >= 100)) || pct=200
    [[ "$floor" =~ ^[0-9]+$ ]] || floor=4096
    if [[ "$setting" =~ ^[0-9]+$ ]] && ((setting > 0)); then
        limit="$setting"
    else
        local peaks="${LOOM_RAM_PEAKS_PATH:-${HOME:-}/.loom/ram-peaks.json}" peak=""
        [[ -n "$repo" && -r "$peaks" ]] && command -v jq >/dev/null 2>&1 || return 0
        peak="$(jq -r --arg r "$repo" '(.repos[$r] // []) | map(select(type == "number")) | max // empty' "$peaks" 2>/dev/null || true)"
        [[ "$peak" =~ ^[0-9]+$ ]] && ((peak > 0)) || return 0
        limit=$((peak * pct / 100 / 1048576))
        ((limit < floor)) && limit="$floor"
    fi
    if [[ "$total_mb" =~ ^[0-9]+$ ]] && ((total_mb > 0 && limit >= total_mb)); then
        return 0
    fi
    echo "$limit"
}
