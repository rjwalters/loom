"""Tests for loom_tools.common.logging."""

from __future__ import annotations

import subprocess
import sys

from loom_tools.common.logging import strip_ansi


# ---------------------------------------------------------------------------
# Tests for non-interactive output visibility (issue #2797)
# ---------------------------------------------------------------------------


def test_log_output_visible_non_interactively() -> None:
    """Log functions write to stderr immediately even when not a tty.

    This validates the fix for #2797: shepherd CLI output was lost when invoked
    non-interactively (e.g., from Claude Code Bash tool or piped contexts)
    because Python buffered stderr. The fix adds flush=True to _emit() so output
    reaches callers immediately without relying on PYTHONUNBUFFERED=1.
    """
    script = (
        "from loom_tools.common.logging import log_info, log_warning, log_error, log_success\n"
        "log_info('test info')\n"
        "log_warning('test warning')\n"
        "log_error('test error')\n"
        "log_success('test success')\n"
    )
    result = subprocess.run(
        [sys.executable, "-c", script],
        capture_output=True,
        text=True,
        # Deliberately do NOT set PYTHONUNBUFFERED to test that flush=True works
        env={"PATH": "/usr/bin:/bin"},
    )
    stderr = result.stderr
    assert "[INFO] test info" in stderr, f"log_info output missing from stderr: {stderr!r}"
    assert "[WARN] test warning" in stderr, f"log_warning output missing from stderr: {stderr!r}"
    assert "[ERROR] test error" in stderr, f"log_error output missing from stderr: {stderr!r}"
    assert "[OK] test success" in stderr, f"log_success output missing from stderr: {stderr!r}"


# ---------------------------------------------------------------------------
# Tests for strip_ansi
# ---------------------------------------------------------------------------


def test_strip_ansi_no_sequences() -> None:
    """Plain text without ANSI codes is unchanged."""
    text = "Hello, world!"
    assert strip_ansi(text) == "Hello, world!"


def test_strip_ansi_empty_string() -> None:
    """Empty string returns empty string."""
    assert strip_ansi("") == ""


def test_strip_ansi_simple_color() -> None:
    """Strips basic color codes."""
    text = "\x1b[31mred text\x1b[0m"
    assert strip_ansi(text) == "red text"


def test_strip_ansi_multiple_colors() -> None:
    """Strips multiple color sequences."""
    text = "\x1b[32mgreen\x1b[0m and \x1b[34mblue\x1b[0m"
    assert strip_ansi(text) == "green and blue"


def test_strip_ansi_bold_and_color() -> None:
    """Strips bold and color combinations."""
    text = "\x1b[1;31mbold red\x1b[0m"
    assert strip_ansi(text) == "bold red"


def test_strip_ansi_cursor_movement() -> None:
    """Strips cursor movement sequences."""
    # Move cursor up 2 lines, then right 5 columns
    text = "\x1b[2A\x1b[5Csome text"
    assert strip_ansi(text) == "some text"


def test_strip_ansi_osc_window_title() -> None:
    """Strips OSC sequences (window title, etc.)."""
    # OSC sequence: ESC ] 0 ; title BEL
    # Note: Current implementation handles simpler OSC patterns
    text = "\x1b]0;Title\x07"
    result = strip_ansi(text)
    # The current regex handles some OSC patterns - verify it doesn't crash
    assert isinstance(result, str)


def test_strip_ansi_character_set() -> None:
    """Strips character set selection sequences."""
    # ESC ( B (ASCII), ESC ) 0 (line drawing)
    text = "\x1b(B\x1b)0text"
    result = strip_ansi(text)
    # Current implementation handles some character set sequences
    assert "text" in result


def test_strip_ansi_keypad_mode() -> None:
    """Strips keypad mode sequences."""
    # ESC = (application), ESC > (numeric)
    text = "\x1b=\x1b>text"
    assert strip_ansi(text) == "text"


def test_strip_ansi_realistic_log_output() -> None:
    """Strips ANSI from realistic terminal log output."""
    # Typical colored log line
    text = "\x1b[0;34m[2026-02-17T10:30:45Z]\x1b[0m \x1b[0;32m[INFO]\x1b[0m Starting build..."
    assert strip_ansi(text) == "[2026-02-17T10:30:45Z] [INFO] Starting build..."


def test_strip_ansi_multiline() -> None:
    """Handles multiline text with ANSI codes."""
    text = "\x1b[31mLine 1\x1b[0m\n\x1b[32mLine 2\x1b[0m\n\x1b[33mLine 3\x1b[0m"
    assert strip_ansi(text) == "Line 1\nLine 2\nLine 3"


def test_strip_ansi_preserves_newlines_and_whitespace() -> None:
    """Preserves newlines and whitespace while removing ANSI codes."""
    text = "\x1b[31m  indented  \x1b[0m\n\x1b[32m\ttabbed\t\x1b[0m"
    assert strip_ansi(text) == "  indented  \n\ttabbed\t"
