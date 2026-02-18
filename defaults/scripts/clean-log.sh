#!/bin/bash
# clean-log.sh - Strip terminal rendering noise from agent log files
#
# Agent logs captured via tmux pipe-pane contain massive amounts of
# terminal rendering artifacts (spinner characters, animation text,
# partial word fragments from redraws, permission banners, etc.).
# This script produces a cleaned version that preserves only the
# meaningful content: wrapper log lines, tool calls, agent output,
# checkpoint saves, and test results.
#
# Usage:
#   ./clean-log.sh <logfile>              # Write cleaned version to <logfile>.clean
#   ./clean-log.sh <logfile> -o <output>  # Write cleaned version to <output>
#   ./clean-log.sh <logfile> --in-place   # Overwrite the original file
#   ./clean-log.sh <logfile> --stdout     # Print to stdout
#
# The original log file is preserved by default (cleaned version gets
# a .clean suffix).  Use --in-place to overwrite the original.
#
# Environment Variables:
#   LOOM_CLEAN_LOG_KEEP_RAW=1  - Skip cleaning (no-op, for debugging)

set -euo pipefail

usage() {
    echo "Usage: $0 <logfile> [--in-place | --stdout | -o <output>]"
    echo ""
    echo "Strip terminal rendering noise from agent log files."
    echo ""
    echo "Options:"
    echo "  --in-place   Overwrite the original file"
    echo "  --stdout     Print cleaned output to stdout"
    echo "  -o <file>    Write cleaned output to <file>"
    echo "  (default)    Write to <logfile>.clean"
    exit 1
}

if [[ $# -lt 1 || "$1" == "--help" || "$1" == "-h" ]]; then
    usage
fi

INPUT_FILE="$1"
shift

if [[ ! -f "$INPUT_FILE" ]]; then
    echo "Error: File not found: $INPUT_FILE" >&2
    exit 1
fi

# Parse output mode
OUTPUT_MODE="suffix"  # default: write to <input>.clean
OUTPUT_FILE=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --in-place)
            OUTPUT_MODE="inplace"
            shift
            ;;
        --stdout)
            OUTPUT_MODE="stdout"
            shift
            ;;
        -o)
            OUTPUT_MODE="file"
            OUTPUT_FILE="$2"
            shift 2
            ;;
        *)
            echo "Unknown option: $1" >&2
            usage
            ;;
    esac
done

# No-op escape hatch for debugging
if [[ "${LOOM_CLEAN_LOG_KEEP_RAW:-}" == "1" ]]; then
    if [[ "$OUTPUT_MODE" == "stdout" ]]; then
        cat "$INPUT_FILE"
    fi
    exit 0
fi

# Use Python for the filtering — handles Unicode spinner characters
# and multi-byte patterns reliably across platforms.
clean_log() {
    python3 -c '
import re
import sys

# Spinner characters used by Claude Code TUI
SPINNERS = set("✶✻✽✳✢⏺·")

# Animation words used during thinking/processing
ANIMATION_WORDS = {
    "Nucleating", "Pollinating", "Shimmying", "Transmuting",
    "Crunching", "Pondering", "Germinating", "Synthesizing",
    "Crystallizing", "Manifesting", "Percolating", "Composing",
    "Ruminating", "Brainstorming", "Evaluating", "Theorizing",
    "Envisioning", "Distilling", "Formulating", "Catalyzing",
    "Incubating", "Calibrating", "Conjuring", "Fermenting",
    "Contemplating", "Architecting", "Deliberating", "Decoding",
    "Weaving", "Assembling", "Deconstructing", "Extrapolating",
    "Interpolating", "Meditating", "Originating", "Philosophizing",
    "Reflecting", "Simulating", "Triangulating", "Unbundling",
    "Visualizing", "Crunched",
}

# Build animation regex: optional spinner + animation word + optional ellipsis/timing
_anim_pattern = "|".join(re.escape(w) for w in sorted(ANIMATION_WORDS))
ANIMATION_RE = re.compile(
    r"^[✶✻✽✳✢⏺·\s]*(" + _anim_pattern + r")…?"
    r"(\s*\(.*\))?\s*$"
)

# OSC title-set sequences: ]0;...  (may appear mid-line)
OSC_RE = re.compile(r"\]0;[^\x07\n]*")

# Separator lines (box-drawing horizontal lines)
SEPARATOR_RE = re.compile(r"^[─━]{4,}\s*$")

# Permission mode banner
PERMISSION_RE = re.compile(r"⏵⏵\s*bypass permissions on")

# Empty prompt lines
PROMPT_RE = re.compile(r'^❯\s*(Try\s+".*")?$')

# Claude Code ASCII banner characters
BANNER_RE = re.compile(r"^[\s▐▛▜▌▝█▘░▒▓]+$")
BANNER_INFO_RE = re.compile(
    r"^[\s▐▛▜▌▝█▘░▒▓]+"
    r"\s*(Claude Code|Opus|Sonnet|Haiku|Claude Max)"
)

# Thinking/token indicators
THINKING_RE = re.compile(
    r"^\s*\("
    r"(thinking|thought for \d+s"
    r"|\d+[sm]?\s*·\s*[↓↑]\s*[\d.,]+k?\s*tokens(\s*·\s*thinking)?)"
    r"\)\s*$"
)

# Status hints
ESC_INTERRUPT_RE = re.compile(r"·\s*esc to interrupt")
CTRL_B_RE = re.compile(r"^ctrl\+b ctrl\+b")

# Lines that are purely spinner chars and/or short alphanumeric fragments
# (redraw debris).  Strip leading spinner chars, then check if whats left
# is very short with no spaces (real content has words with spaces).
def is_spinner_debris(line: str) -> bool:
    stripped = line
    # Remove leading spinner characters
    while stripped and stripped[0] in SPINNERS:
        stripped = stripped[1:]
    stripped = stripped.strip()
    # Pure spinner line
    if not stripped:
        return True
    # Short fragment without spaces — redraw debris
    # (e.g., "u", "ca", "Nl", "tg", "i…", "eain", "lea↓")
    if len(stripped) <= 5 and " " not in stripped:
        # But preserve log-header comment lines (# ...)
        if stripped.startswith("#"):
            return False
        # Preserve lines that look like numbers (test output like "364")
        # only if they are 3+ digits — single digits are noise
        if stripped.isdigit() and len(stripped) >= 3:
            return False
        return True
    return False


def clean(input_path: str) -> str:
    with open(input_path, "r", errors="replace") as f:
        lines = f.readlines()

    output = []
    blank_run = 0

    for raw_line in lines:
        line = raw_line.rstrip("\n\r")

        # Strip OSC sequences from everywhere
        line = OSC_RE.sub("", line)

        # Check each noise pattern
        if not line or line.isspace():
            blank_run += 1
            continue

        if SEPARATOR_RE.match(line):
            blank_run += 1
            continue

        if PERMISSION_RE.search(line):
            blank_run += 1
            continue

        if PROMPT_RE.match(line):
            blank_run += 1
            continue

        if BANNER_RE.match(line):
            blank_run += 1
            continue

        if BANNER_INFO_RE.match(line):
            blank_run += 1
            continue

        if THINKING_RE.match(line):
            blank_run += 1
            continue

        if ESC_INTERRUPT_RE.search(line):
            blank_run += 1
            continue

        if CTRL_B_RE.match(line):
            blank_run += 1
            continue

        if ANIMATION_RE.match(line):
            blank_run += 1
            continue

        if is_spinner_debris(line):
            blank_run += 1
            continue

        # Line has real content — strip leading spinner chars
        cleaned = line
        while cleaned and cleaned[0] in SPINNERS:
            cleaned = cleaned[1:]

        # Emit a single blank line for any gap
        if blank_run > 0 and output:
            output.append("")
        blank_run = 0

        output.append(cleaned)

    return "\n".join(output) + "\n" if output else ""


sys.stdout.write(clean(sys.argv[1]))
' "$INPUT_FILE"
}

case "$OUTPUT_MODE" in
    stdout)
        clean_log
        ;;
    inplace)
        tmp=$(mktemp "${INPUT_FILE}.tmp.XXXXXX")
        clean_log > "$tmp"
        mv "$tmp" "$INPUT_FILE"
        ;;
    file)
        clean_log > "$OUTPUT_FILE"
        ;;
    suffix)
        clean_log > "${INPUT_FILE}.clean"
        ;;
esac
