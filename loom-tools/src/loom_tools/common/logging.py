"""CLI logging with optional color output."""

from __future__ import annotations

import io
import os
import re
import sys
from datetime import datetime, timezone

# ANSI color codes — only emitted when stderr is a tty.
_RED = "\033[0;31m"
_GREEN = "\033[0;32m"
_YELLOW = "\033[0;33m"
_BLUE = "\033[0;34m"
_RESET = "\033[0m"

# Regex pattern to match ANSI escape sequences
# Matches: ESC [ ... (letter) and ESC ] ... (BEL or ESC \)
_ANSI_ESCAPE_PATTERN = re.compile(
    r"""
    \x1b  # ESC character
    (?:
        \[  # CSI sequences: ESC [
        [?0-9;]*  # parameters (including ? for private modes)
        [A-Za-z]  # final character
        |
        \]  # OSC sequences: ESC ]
        .*?  # payload
        (?:\x07|\x1b\\)  # terminated by BEL or ESC \
        |
        [()][0-9AB]  # Character set selection: ESC ( or ESC )
        |
        [=>]  # Keypad modes: ESC = or ESC >
    )
    """,
    re.VERBOSE,
)


def _use_color() -> bool:
    try:
        return os.isatty(sys.stderr.fileno())
    except (OSError, ValueError, io.UnsupportedOperation):
        return False


def _timestamp() -> str:
    now = datetime.now(timezone.utc)
    return now.strftime("[%Y-%m-%dT%H:%M:%SZ]")


def _emit(color: str, label: str, message: str) -> None:
    ts = _timestamp()
    if _use_color():
        line = f"{color}{ts} [{label}]{_RESET} {message}"
    else:
        line = f"{ts} [{label}] {message}"
    print(line, file=sys.stderr)


def log_info(message: str) -> None:
    """Log an informational message to stderr."""
    _emit(_BLUE, "INFO", message)


def log_warning(message: str) -> None:
    """Log a warning message to stderr."""
    _emit(_YELLOW, "WARN", message)


def log_error(message: str) -> None:
    """Log an error message to stderr."""
    _emit(_RED, "ERROR", message)


def log_success(message: str) -> None:
    """Log a success message to stderr."""
    _emit(_GREEN, "OK", message)


def strip_ansi(text: str) -> str:
    """Remove ANSI escape sequences from text.

    Strips terminal control sequences including:
    - CSI sequences (colors, cursor movement, etc.): ESC [ ... m
    - OSC sequences (window titles, etc.): ESC ] ... BEL
    - Character set selection: ESC ( B, ESC ) 0, etc.
    - Keypad modes: ESC =, ESC >

    Args:
        text: Text potentially containing ANSI escape sequences.

    Returns:
        Text with all ANSI escape sequences removed.

    Example:
        >>> strip_ansi("\\x1b[31mred text\\x1b[0m")
        'red text'
    """
    return _ANSI_ESCAPE_PATTERN.sub("", text)
