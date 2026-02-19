"""Log filter for tmux pipe-pane output and post-processing agent logs.

Handles two modes:

1. **Stdin pipeline** (default, no args):
   Real-time filter for tmux pipe-pane.  Strips ANSI escapes, carriage
   returns, backspaces, control characters, blank lines, and consecutive
   duplicate lines.

2. **File post-processing** (``--file <path>``):
   Deep cleaning of captured agent logs.  Applies everything from mode 1
   plus Claude Code TUI noise removal: spinner characters, animation text,
   thinking indicators, permission banners, separator lines, ASCII art
   banners, and short redraw-debris fragments.

Usage::

    # Real-time pipe-pane filter
    python3 -m loom_tools.log_filter >> /path/to/log.log

    # Post-process a captured log file
    python3 -m loom_tools.log_filter --file /path/to/log.log
"""

from __future__ import annotations

import re
import sys
import unicodedata

from loom_tools.common.logging import strip_ansi

# Characters to strip beyond ANSI sequences
_CONTROL_CHARS = re.compile(r"[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]")

# ---------------------------------------------------------------------------
# Claude Code TUI noise patterns (used in deep/file cleaning mode)
# ---------------------------------------------------------------------------

# Spinner characters used by the Claude Code TUI thinking animation.
# NOTE: ⏺ (U+23FA) is intentionally excluded — it is the tool call marker
# used by _is_thinking_stall_session() to detect productive sessions.
# Stripping ⏺ would cause false-positive thinking stall detection.
# See issue #2835.
SPINNERS = set("\u2736\u273b\u273d\u2733\u2722\u00b7")  # ✶✻✽✳✢·

# Animation words displayed during thinking/processing.
# Covers multiple Claude Code versions — newer versions (v2.1.40+) use
# words like "Frosting", "Befuddling", "Moseying", etc. that were not
# present in older versions.  See issue #2835.
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
    # Newer animation words observed in Claude Code v2.1.40+ (issue #2835)
    "Befuddling", "Frosting", "Moseying", "Sashaying", "Waltzing",
    "Ambling", "Beguiling", "Brooding", "Bumbling", "Dawdling",
    "Dithering", "Floundering", "Fretting", "Fumbling", "Gallivanting",
    "Humming", "Idling", "Lollygagging", "Meandering", "Milling",
    "Mulling", "Noodling", "Perambulating", "Perusing", "Pondering",
    "Pottering", "Puttering", "Rambling", "Ruminating", "Sifting",
    "Stewing", "Tinkering", "Toiling", "Wandering", "Whirring",
}

# Build animation regex: optional spinner + animation word + optional ellipsis/timing.
# ⏺ is excluded from the spinner prefix class — see SPINNERS note above.
_anim_pattern = "|".join(re.escape(w) for w in sorted(ANIMATION_WORDS))
_ANIMATION_RE = re.compile(
    "^[✶✻✽✳✢·\\s]*("
    + _anim_pattern
    + ")…?"
    "(\\s*\\(.*\\))?\\s*$"
)

# Separator lines (box-drawing horizontal lines)
_SEPARATOR_RE = re.compile("^[─━]{4,}\\s*$")

# Permission mode banner
_PERMISSION_RE = re.compile("⏵⏵\\s*bypass permissions on")

# Empty prompt lines
_PROMPT_RE = re.compile('^❯\\s*(Try\\s+".*")?\\s*$')

# Claude Code ASCII banner characters
_BANNER_RE = re.compile("^[\\s▐▛▜▌▝█▘░▒▓]+$")
_BANNER_INFO_RE = re.compile(
    "^[\\s▐▛▜▌▝█▘░▒▓]+"
    "\\s*(Claude Code|Opus|Sonnet|Haiku|Claude Max)"
)

# Thinking/token indicators
_THINKING_RE = re.compile(
    "^\\s*\\("
    "(thinking|thought for \\d+s"
    "|\\d+[sm]?\\s*·\\s*[↓↑]\\s*[\\d.,]+k?\\s*tokens(\\s*·\\s*thinking)?)"
    "\\)\\s*$"
)

# Status hints
_ESC_INTERRUPT_RE = re.compile("·\\s*esc to interrupt")
_CTRL_B_RE = re.compile("^ctrl\\+b ctrl\\+b")


def is_tui_noise(line: str) -> bool:
    """Return True if *line* is Claude Code TUI rendering noise.

    Expects *line* to already have ANSI sequences stripped (call
    ``clean_line`` first or use ``strip_ansi``).
    """
    if _ANIMATION_RE.match(line):
        return True
    if _SEPARATOR_RE.match(line):
        return True
    if _PERMISSION_RE.search(line):
        return True
    if _PROMPT_RE.match(line):
        return True
    if _BANNER_RE.match(line):
        return True
    if _BANNER_INFO_RE.match(line):
        return True
    if _THINKING_RE.match(line):
        return True
    if _ESC_INTERRUPT_RE.search(line):
        return True
    if _CTRL_B_RE.match(line):
        return True
    return _is_spinner_debris(line)


def _is_spinner_debris(line: str) -> bool:
    """Return True if *line* is spinner chars and/or short redraw fragments."""
    stripped = line
    # Remove leading spinner characters
    while stripped and stripped[0] in SPINNERS:
        stripped = stripped[1:]
    stripped = stripped.strip()
    # Pure spinner line
    if not stripped:
        return True
    # Short fragment without spaces -- redraw debris
    # (e.g., "u", "ca", "Nl", "tg", "i…", "eain", "lea↓")
    if len(stripped) <= 5 and " " not in stripped:
        # Preserve log-header comment lines (# ...)
        if stripped.startswith("#"):
            return False
        # Preserve 3+ digit numbers (test output like "364")
        if stripped.isdigit() and len(stripped) >= 3:
            return False
        return True
    return False


def _strip_leading_spinners(line: str) -> str:
    """Remove leading spinner characters from *line*."""
    i = 0
    while i < len(line) and line[i] in SPINNERS:
        i += 1
    return line[i:]


# ---------------------------------------------------------------------------
# Core cleaning (shared between modes)
# ---------------------------------------------------------------------------


def clean_line(raw: str) -> str | None:
    """Clean a single line of terminal output.

    Returns the cleaned line, or None if the line should be suppressed.
    """
    # Strip trailing \r before splitting so that "content\r" doesn't
    # resolve to an empty last segment (which would be suppressed as blank).
    raw = raw.rstrip("\r")

    # Process carriage returns: keep only the last segment
    # This handles spinner animation where lines are overwritten with \r
    if "\r" in raw:
        segments = raw.split("\r")
        raw = segments[-1]

    # Strip ANSI escape sequences
    line = strip_ansi(raw)

    # Remove backspace characters and the character they erase
    while "\x08" in line:
        # Remove char + backspace pair, or leading backspace
        line = re.sub(r"[^\x08]\x08", "", line, count=1)
        line = re.sub(r"^\x08+", "", line)

    # Remove remaining control characters (except \n, \t)
    line = _CONTROL_CHARS.sub("", line)

    # Remove Unicode control/format characters (category Cc/Cf) except common ones
    cleaned = []
    for ch in line:
        cat = unicodedata.category(ch)
        if cat == "Cc" and ch not in ("\n", "\t"):
            continue
        if cat == "Cf":
            continue
        cleaned.append(ch)
    line = "".join(cleaned)

    # Suppress blank/whitespace-only lines
    if not line.strip():
        return None

    return line


# ---------------------------------------------------------------------------
# File post-processing (deep clean)
# ---------------------------------------------------------------------------


def clean_file(input_path: str) -> str:
    """Deep-clean a captured agent log file.

    Applies ``clean_line`` (ANSI strip, CR, backspace, control chars) then
    removes Claude Code TUI noise patterns and collapses blank runs into
    single separators.

    Returns the cleaned text as a string.
    """
    with open(input_path, "r", errors="replace") as f:
        lines = f.readlines()

    output: list[str] = []
    blank_run = 0

    for raw_line in lines:
        line = raw_line.rstrip("\n\r")

        # First pass: strip ANSI and low-level artifacts
        cleaned = clean_line(line)
        if cleaned is None:
            blank_run += 1
            continue

        # Second pass: TUI noise patterns (on ANSI-stripped text)
        if is_tui_noise(cleaned):
            blank_run += 1
            continue

        # Strip leading spinner characters from real content
        cleaned = _strip_leading_spinners(cleaned)

        # Emit a single blank line for any gap
        if blank_run > 0 and output:
            output.append("")
        blank_run = 0

        output.append(cleaned)

    return "\n".join(output) + "\n" if output else ""


# ---------------------------------------------------------------------------
# CLI entry points
# ---------------------------------------------------------------------------


def _is_tui_noise_realtime(line: str) -> bool:
    """TUI noise check safe for real-time streaming (pipe-pane filter).

    Applies all ``is_tui_noise()`` checks except the short-fragment debris
    heuristic is only triggered when the line starts with a spinner character.
    This prevents false-positive suppression of legitimate short content lines
    (e.g. single-word test output, short variable names) in a real-time
    stream where we cannot use file-level context.

    See issue #2798.
    """
    if _ANIMATION_RE.match(line):
        return True
    if _SEPARATOR_RE.match(line):
        return True
    if _PERMISSION_RE.search(line):
        return True
    if _PROMPT_RE.match(line):
        return True
    if _BANNER_RE.match(line):
        return True
    if _BANNER_INFO_RE.match(line):
        return True
    if _THINKING_RE.match(line):
        return True
    if _ESC_INTERRUPT_RE.search(line):
        return True
    if _CTRL_B_RE.match(line):
        return True
    # Only apply spinner debris check when the line has a spinner-char prefix.
    # Without a spinner prefix, short text (e.g. "OK", "yes") could be real
    # content, so we do not suppress it.
    if line and line[0] in SPINNERS:
        return _is_spinner_debris(line)
    return False


def main() -> None:
    """Read stdin line by line, clean, deduplicate, and write to stdout."""
    prev_line: str | None = None
    dup_count = 0

    try:
        for raw_line in iter(sys.stdin.readline, ""):
            # Remove trailing newline for processing
            raw = raw_line.rstrip("\n")

            cleaned = clean_line(raw)
            if cleaned is None:
                continue

            # Filter TUI noise (spinner chars, animation words, banners, etc.)
            # Uses the realtime-safe variant that avoids false-positives on
            # short content lines without spinner prefixes.  See issue #2798.
            if _is_tui_noise_realtime(cleaned):
                continue

            # Strip leading spinner characters from surviving lines so that
            # subsequent deduplication treats spinner-prefixed and plain lines
            # as the same content (e.g. "✶ Fixing…" and "✻ Fixing…" collapse).
            cleaned = _strip_leading_spinners(cleaned)
            if not cleaned.strip():
                continue

            # Collapse consecutive duplicate lines
            if cleaned == prev_line:
                dup_count += 1
                continue

            # If we had duplicates, emit a summary
            if dup_count > 0 and prev_line is not None:
                sys.stdout.write(f"  [repeated {dup_count} more time{'s' if dup_count > 1 else ''}]\n")
                sys.stdout.flush()

            sys.stdout.write(cleaned + "\n")
            sys.stdout.flush()
            prev_line = cleaned
            dup_count = 0

    except (BrokenPipeError, KeyboardInterrupt):
        pass
    finally:
        # Flush any trailing duplicate count
        if dup_count > 0 and prev_line is not None:
            try:
                sys.stdout.write(f"  [repeated {dup_count} more time{'s' if dup_count > 1 else ''}]\n")
                sys.stdout.flush()
            except (BrokenPipeError, OSError):
                pass


def main_file(path: str) -> None:
    """Post-process a captured log file, writing cleaned output to stdout."""
    sys.stdout.write(clean_file(path))


if __name__ == "__main__":
    if len(sys.argv) >= 3 and sys.argv[1] == "--file":
        main_file(sys.argv[2])
    else:
        main()
