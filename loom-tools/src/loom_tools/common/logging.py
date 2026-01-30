"""CLI logging with optional color output."""

from __future__ import annotations

import io
import os
import sys
from datetime import datetime, timezone

# ANSI color codes — only emitted when stderr is a tty.
_RED = "\033[0;31m"
_GREEN = "\033[0;32m"
_YELLOW = "\033[0;33m"
_BLUE = "\033[0;34m"
_RESET = "\033[0m"


def _use_color() -> bool:
    try:
        return os.isatty(sys.stderr.fileno())
    except (OSError, ValueError, io.UnsupportedOperation):
        return False


def _timestamp() -> str:
    now = datetime.now(timezone.utc)
    return now.strftime("[%H:%M:%S]")


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
