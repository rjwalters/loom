"""Stdin-to-stdout log filter for tmux pipe-pane output.

Replaces the sed-based ANSI stripping with a Python filter that handles:
- ANSI escape sequence stripping (via strip_ansi)
- Carriage return processing (keep only final line segment)
- Backspace character removal
- Blank/whitespace-only line suppression
- Consecutive duplicate line collapsing (spinner frame deduplication)
- Non-printable character cleanup

Usage in pipe-pane:
    python3 -m loom_tools.log_filter >> /path/to/log.log
"""

from __future__ import annotations

import re
import sys
import unicodedata

from loom_tools.common.logging import strip_ansi

# Characters to strip beyond ANSI sequences
_CONTROL_CHARS = re.compile(r"[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]")


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


if __name__ == "__main__":
    main()
