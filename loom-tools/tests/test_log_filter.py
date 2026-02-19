"""Tests for loom_tools.log_filter."""

from __future__ import annotations

import io
import tempfile
from pathlib import Path
from unittest.mock import MagicMock, patch

from loom_tools.log_filter import (
    SPINNERS,
    clean_file,
    clean_line,
    is_tui_noise,
    main,
)


# ---------------------------------------------------------------------------
# TestCleanLine — unit tests for clean_line()
# ---------------------------------------------------------------------------


class TestCleanLine:
    """Unit tests for the clean_line() pure function."""

    # -- ANSI stripping (delegates to strip_ansi) --

    def test_ansi_color_codes_stripped(self) -> None:
        assert clean_line("\x1b[31mError\x1b[0m") == "Error"

    def test_ansi_bold_and_color_stripped(self) -> None:
        assert clean_line("\x1b[1;32mOK\x1b[0m") == "OK"

    def test_ansi_24bit_color_stripped(self) -> None:
        assert clean_line("\x1b[38;2;226;141;109mtext\x1b[39m") == "text"

    # -- Carriage return processing --

    def test_carriage_return_keeps_last_segment(self) -> None:
        """Spinner animation: only the final segment after \\r is kept."""
        assert clean_line("frame1\rframe2\rframe3") == "frame3"

    def test_carriage_return_single(self) -> None:
        assert clean_line("old\rnew") == "new"

    def test_carriage_return_with_ansi(self) -> None:
        """CR processing happens before ANSI stripping."""
        assert clean_line("old\r\x1b[32mnew\x1b[0m") == "new"

    # -- Backspace removal --

    def test_backspace_erases_preceding_char(self) -> None:
        assert clean_line("ab\x08c") == "ac"

    def test_multiple_backspaces(self) -> None:
        assert clean_line("abc\x08\x08d") == "ad"

    def test_leading_backspace_ignored(self) -> None:
        assert clean_line("\x08hello") == "hello"

    def test_backspace_erases_all(self) -> None:
        """More backspaces than characters empties the string."""
        result = clean_line("a\x08\x08")
        # After erasing 'a' the leading backspace is stripped
        assert result is None or result.strip() == ""

    # -- Control character removal --

    def test_null_byte_removed(self) -> None:
        assert clean_line("hel\x00lo") == "hello"

    def test_bell_removed(self) -> None:
        assert clean_line("alert\x07!") == "alert!"

    def test_vertical_tab_removed(self) -> None:
        assert clean_line("a\x0bb") == "ab"

    def test_form_feed_removed(self) -> None:
        assert clean_line("a\x0cb") == "ab"

    def test_tab_preserved(self) -> None:
        assert clean_line("\tindented") == "\tindented"

    # -- Unicode control/format character removal --

    def test_zero_width_space_removed(self) -> None:
        """U+200B (zero-width space) is category Cf and should be removed."""
        assert clean_line("he\u200bllo") == "hello"

    def test_zero_width_joiner_removed(self) -> None:
        """U+200D (zero-width joiner) is category Cf."""
        assert clean_line("a\u200db") == "ab"

    def test_left_to_right_mark_removed(self) -> None:
        """U+200E (left-to-right mark) is category Cf."""
        assert clean_line("text\u200e!") == "text!"

    def test_soft_hyphen_removed(self) -> None:
        """U+00AD (soft hyphen) is category Cf."""
        assert clean_line("hy\u00adphen") == "hyphen"

    def test_normal_unicode_preserved(self) -> None:
        """Regular Unicode text is not affected."""
        assert clean_line("cafe\u0301") == "cafe\u0301"  # combining accent

    # -- Blank line suppression --

    def test_empty_string_returns_none(self) -> None:
        assert clean_line("") is None

    def test_whitespace_only_returns_none(self) -> None:
        assert clean_line("   ") is None

    def test_tabs_only_returns_none(self) -> None:
        assert clean_line("\t\t") is None

    def test_control_chars_only_returns_none(self) -> None:
        """Line with only control characters becomes blank -> None."""
        assert clean_line("\x00\x01\x02") is None

    # -- Combined cleaning --

    def test_ansi_and_carriage_return(self) -> None:
        assert clean_line("\x1b[31mold\x1b[0m\r\x1b[32mnew\x1b[0m") == "new"

    def test_ansi_and_backspace(self) -> None:
        assert clean_line("\x1b[31mab\x08c\x1b[0m") == "ac"

    def test_all_artifacts_combined(self) -> None:
        """Input with CR, ANSI, backspace, and control chars."""
        raw = "junk\r\x1b[31mhe\x00l\x08lo\x1b[0m"
        # CR keeps "\x1b[31mhe\x00l\x08lo\x1b[0m"
        # ANSI strip -> "he\x00l\x08lo"
        # Backspace: l\x08 -> erase l, giving "he\x00lo"
        # Control char \x00 removed -> "helo"
        assert clean_line(raw) == "helo"

    # -- Edge cases --

    def test_very_long_line(self) -> None:
        line = "x" * 10000
        assert clean_line(line) == line

    def test_only_newline_chars_suppressed(self) -> None:
        """A line of only \\n and \\t is whitespace -> None."""
        assert clean_line("\n\t\n") is None

    def test_mixed_cr_and_backspace(self) -> None:
        """CR processed first, then backspace erases preceding char."""
        raw = "abc\rxy\x08z"
        # CR -> "xy\x08z", backspace erases 'y' -> "xz"
        assert clean_line(raw) == "xz"

    def test_plain_text_unchanged(self) -> None:
        assert clean_line("Hello, world!") == "Hello, world!"

    # -- Trailing \r regression tests (issue #2230) --

    def test_trailing_cr_preserves_content(self) -> None:
        """Line ending with \\r should return content, not None."""
        assert clean_line("echo HELLO\r") == "echo HELLO"

    def test_trailing_cr_with_ansi(self) -> None:
        """ANSI-wrapped line ending with \\r should return stripped content."""
        assert clean_line("\x1b[32mecho HELLO\x1b[0m\r") == "echo HELLO"

    def test_trailing_cr_only(self) -> None:
        """A bare \\r with no content should return None (blank)."""
        assert clean_line("\r") is None

    def test_trailing_multiple_cr(self) -> None:
        """Multiple trailing \\r characters should still preserve content."""
        assert clean_line("content\r\r\r") == "content"


# ---------------------------------------------------------------------------
# TestIsTuiNoise — unit tests for is_tui_noise()
# ---------------------------------------------------------------------------


class TestIsTuiNoise:
    """Unit tests for Claude Code TUI noise detection."""

    # -- Spinner characters --

    def test_pure_spinner_line(self) -> None:
        assert is_tui_noise("\u2736") is True  # ✶

    def test_multiple_spinners(self) -> None:
        assert is_tui_noise("\u2736\u273b\u273d") is True  # ✶✻✽

    def test_spinner_with_short_fragment(self) -> None:
        """Spinner char + short fragment = redraw debris."""
        assert is_tui_noise("\u2736ca") is True

    # -- Animation words --

    def test_animation_word_with_ellipsis(self) -> None:
        assert is_tui_noise("Nucleating\u2026") is True

    def test_animation_word_plain(self) -> None:
        assert is_tui_noise("Pollinating") is True

    def test_animation_word_with_timing(self) -> None:
        assert is_tui_noise("Synthesizing\u2026 (2s)") is True

    def test_spinner_plus_animation(self) -> None:
        assert is_tui_noise("\u2736 Nucleating\u2026") is True

    # -- Thinking indicators --

    def test_thinking_simple(self) -> None:
        assert is_tui_noise("(thinking)") is True

    def test_thought_for_seconds(self) -> None:
        assert is_tui_noise("(thought for 2s)") is True

    def test_token_count_with_thinking(self) -> None:
        assert is_tui_noise("(30s \u00b7 \u2193 760 tokens \u00b7 thinking)") is True

    def test_token_count_without_thinking(self) -> None:
        assert is_tui_noise("(5s \u00b7 \u2191 1.2k tokens)") is True

    # -- Separator lines --

    def test_thin_separator(self) -> None:
        assert is_tui_noise("\u2500" * 20) is True  # ─

    def test_thick_separator(self) -> None:
        assert is_tui_noise("\u2501" * 20) is True  # ━

    def test_short_separator_not_noise(self) -> None:
        """Fewer than 4 separator chars with surrounding text is not a separator."""
        assert is_tui_noise("a \u2500\u2500\u2500 b") is False

    # -- Permission banners --

    def test_permission_banner(self) -> None:
        assert is_tui_noise("\u23f5\u23f5 bypass permissions on (shift+tab to cycle)") is True

    # -- Prompt lines --

    def test_empty_prompt(self) -> None:
        assert is_tui_noise("\u276f") is True  # ❯

    def test_prompt_with_suggestion(self) -> None:
        assert is_tui_noise('\u276f Try "help"') is True

    # -- Banner characters --

    def test_banner_block_chars(self) -> None:
        assert is_tui_noise("  \u2590\u259b\u259c\u258c\u259d\u2588  ") is True

    def test_banner_with_product_name(self) -> None:
        assert is_tui_noise("  \u2590\u2588 Claude Code") is True

    # -- Status hints --

    def test_esc_interrupt_hint(self) -> None:
        assert is_tui_noise("some text \u00b7 esc to interrupt") is True

    def test_ctrl_b_hint(self) -> None:
        assert is_tui_noise("ctrl+b ctrl+b to exit") is True

    # -- Short fragment debris --

    def test_single_char_debris(self) -> None:
        assert is_tui_noise("u") is True

    def test_two_char_debris(self) -> None:
        assert is_tui_noise("ca") is True

    def test_five_char_debris(self) -> None:
        assert is_tui_noise("eain\u2193") is True

    def test_comment_line_preserved(self) -> None:
        """Lines starting with # are not debris."""
        assert is_tui_noise("# log") is False

    def test_three_digit_number_preserved(self) -> None:
        """3+ digit numbers from test output are preserved."""
        assert is_tui_noise("364") is False

    def test_single_digit_is_debris(self) -> None:
        assert is_tui_noise("3") is True

    # -- Real content not flagged --

    def test_real_log_line(self) -> None:
        assert is_tui_noise("[OK] Checkpoint saved: stage=planning") is False

    def test_real_command(self) -> None:
        assert is_tui_noise("git commit -m 'fix: resolve issue'") is False

    def test_real_error(self) -> None:
        assert is_tui_noise("Error: File not found: /tmp/foo") is False

    def test_long_text_not_debris(self) -> None:
        assert is_tui_noise("This is a real line of output") is False


# ---------------------------------------------------------------------------
# TestCleanFile — integration tests for clean_file()
# ---------------------------------------------------------------------------


class TestCleanFile:
    """Integration tests for the file post-processing pipeline."""

    @staticmethod
    def _clean(content: str) -> str:
        """Write content to a temp file, clean it, return result."""
        with tempfile.NamedTemporaryFile(mode="w", suffix=".log", delete=False) as f:
            f.write(content)
            f.flush()
            path = f.name
        try:
            return clean_file(path)
        finally:
            Path(path).unlink(missing_ok=True)

    def test_plain_text_passthrough(self) -> None:
        result = self._clean("[OK] Checkpoint saved\ngit commit -m 'fix'\n")
        assert result == "[OK] Checkpoint saved\ngit commit -m 'fix'\n"

    def test_ansi_stripped(self) -> None:
        """ANSI sequences are stripped before pattern matching."""
        result = self._clean("\x1b[32m\u2736 Nucleating\u2026\x1b[0m\n")
        assert result == ""

    def test_ansi_on_real_content(self) -> None:
        """ANSI on real content is stripped, content preserved."""
        result = self._clean("\x1b[31mError: something failed\x1b[0m\n")
        assert result == "Error: something failed\n"

    def test_spinner_lines_removed(self) -> None:
        result = self._clean("\u2736\n\u273b\n\u273d\nreal content\n")
        assert result == "real content\n"

    def test_animation_lines_removed(self) -> None:
        result = self._clean("Nucleating\u2026\nPollinating\u2026\nreal content\n")
        assert result == "real content\n"

    def test_thinking_lines_removed(self) -> None:
        result = self._clean("(thinking)\n(thought for 2s)\nreal content\n")
        assert result == "real content\n"

    def test_separator_lines_removed(self) -> None:
        content = "\u2500" * 40 + "\nreal content\n"
        result = self._clean(content)
        assert result == "real content\n"

    def test_blank_run_collapsing(self) -> None:
        """Multiple blank/noise lines collapse to a single separator."""
        content = "line one\n\n\n\nline two\n"
        result = self._clean(content)
        assert result == "line one\n\nline two\n"

    def test_leading_spinner_stripped_from_content(self) -> None:
        """Leading spinner chars are stripped from real content lines."""
        result = self._clean("\u2736 [OK] Checkpoint saved\n")
        assert result == " [OK] Checkpoint saved\n"

    def test_mixed_noise_and_content(self) -> None:
        """End-to-end: noise interspersed with real content."""
        content = (
            "\u2736\n"
            "Nucleating\u2026\n"
            "(thinking)\n"
            "[OK] Checkpoint saved: stage=planning\n"
            "\u2500" * 40 + "\n"
            "\n"
            "Pollinating\u2026\n"
            "git commit -m 'fix: resolve issue'\n"
        )
        result = self._clean(content)
        lines = result.strip().split("\n")
        assert "[OK] Checkpoint saved: stage=planning" in lines[0]
        assert "git commit -m 'fix: resolve issue'" in lines[-1]

    def test_empty_file(self) -> None:
        result = self._clean("")
        assert result == ""

    def test_permission_banner_removed(self) -> None:
        content = "\u23f5\u23f5 bypass permissions on (shift+tab to cycle)\nreal content here\n"
        result = self._clean(content)
        assert result == "real content here\n"

    def test_prompt_lines_removed(self) -> None:
        content = "\u276f\nreal content\n"
        result = self._clean(content)
        assert result == "real content\n"

    def test_real_log_with_embedded_ansi(self) -> None:
        """Regression: ANSI-wrapped spinner should be detected after stripping."""
        content = "\x1b[32m\u2736\x1b[0m Nucleating\u2026\n"
        result = self._clean(content)
        assert result == ""


# ---------------------------------------------------------------------------
# TestMain — integration tests for main()
# ---------------------------------------------------------------------------


class TestMain:
    """Integration tests for the stdin->stdout main() pipeline."""

    @staticmethod
    def _run_main(input_text: str) -> str:
        """Helper: run main() with given stdin, capture stdout."""
        stdin = io.StringIO(input_text)
        stdout = io.StringIO()
        with patch("sys.stdin", stdin), patch("sys.stdout", stdout):
            main()
        return stdout.getvalue()

    def test_basic_passthrough(self) -> None:
        output = self._run_main("hello\nworld\n")
        assert output == "hello\nworld\n"

    def test_blank_lines_suppressed(self) -> None:
        output = self._run_main("hello\n\n\nworld\n")
        assert output == "hello\nworld\n"

    def test_duplicate_collapsing_two(self) -> None:
        """Two identical lines: first printed, second summarised."""
        output = self._run_main("same\nsame\n")
        assert output == "same\n  [repeated 1 more time]\n"

    def test_duplicate_collapsing_many(self) -> None:
        """Five identical lines: first + 'repeated 4 more times'."""
        output = self._run_main("dup\ndup\ndup\ndup\ndup\n")
        assert output == "dup\n  [repeated 4 more times]\n"

    def test_duplicate_then_different(self) -> None:
        """Duplicate summary flushed when a new line appears."""
        output = self._run_main("a\na\na\nb\n")
        assert output == "a\n  [repeated 2 more times]\nb\n"

    def test_trailing_duplicate_flushed_at_eof(self) -> None:
        """Duplicate count is emitted in the finally block on EOF."""
        output = self._run_main("x\nx\nx\n")
        assert output == "x\n  [repeated 2 more times]\n"

    def test_empty_stdin_no_output(self) -> None:
        output = self._run_main("")
        assert output == ""

    def test_broken_pipe_graceful(self) -> None:
        """BrokenPipeError on stdout.write is caught silently."""
        stdin = io.StringIO("hello\n")
        mock_stdout = MagicMock()
        mock_stdout.write.side_effect = BrokenPipeError
        with patch("sys.stdin", stdin), patch("sys.stdout", mock_stdout):
            main()  # Should not raise

    def test_mixed_clean_suppressed_duplicate(self) -> None:
        """End-to-end: real lines, blank lines, and duplicates."""
        input_text = "alpha\n\nbeta\nbeta\nbeta\n\ngamma\n"
        output = self._run_main(input_text)
        assert output == "alpha\nbeta\n  [repeated 2 more times]\ngamma\n"

    def test_ansi_stripped_before_dedup(self) -> None:
        """Lines differing only by ANSI codes are treated as duplicates."""
        input_text = "\x1b[31mhello\x1b[0m\n\x1b[32mhello\x1b[0m\n"
        output = self._run_main(input_text)
        assert output == "hello\n  [repeated 1 more time]\n"

    def test_singular_repeated_message(self) -> None:
        """'repeated 1 more time' uses singular form."""
        output = self._run_main("z\nz\n")
        assert "  [repeated 1 more time]\n" in output

    def test_plural_repeated_message(self) -> None:
        """'repeated N more times' uses plural for N > 1."""
        output = self._run_main("z\nz\nz\n")
        assert "  [repeated 2 more times]\n" in output

    def test_whitespace_only_lines_suppressed_in_stream(self) -> None:
        output = self._run_main("hello\n   \n\t\nworld\n")
        assert output == "hello\nworld\n"

    def test_cr_spinner_frames_deduplicated(self) -> None:
        """Simulated spinner: CR-separated frames collapse to unique outputs."""
        # Each line has spinner frames separated by CR
        input_text = "frame1\rframe2\rframe3\nframe1\rframe2\rframe3\n"
        output = self._run_main(input_text)
        assert output == "frame3\n  [repeated 1 more time]\n"

    def test_spinner_lines_suppressed(self) -> None:
        """TUI spinner chars are filtered out in real-time stream mode."""
        output = self._run_main("\u2736\n\u273b\n\u273d\nreal content\n")
        assert output == "real content\n"

    def test_animation_words_suppressed(self) -> None:
        """TUI animation words are filtered out in real-time stream mode."""
        output = self._run_main("Nucleating\u2026\nPollinating\u2026\nreal content\n")
        assert output == "real content\n"

    def test_thinking_indicators_suppressed(self) -> None:
        """Thinking indicators are filtered out in real-time stream mode."""
        output = self._run_main("(thinking)\n(thought for 2s)\nreal content\n")
        assert output == "real content\n"

    def test_separator_lines_suppressed(self) -> None:
        """Separator lines are filtered out in real-time stream mode."""
        output = self._run_main("\u2500" * 40 + "\nreal content\n")
        assert output == "real content\n"

    def test_permission_banner_suppressed(self) -> None:
        """Permission banners are filtered out in real-time stream mode."""
        output = self._run_main(
            "\u23f5\u23f5 bypass permissions on (shift+tab to cycle)\nreal content\n"
        )
        assert output == "real content\n"

    def test_leading_spinner_stripped_from_content(self) -> None:
        """Leading spinner chars are stripped from real content in stream mode."""
        output = self._run_main("\u2736 real content here\n")
        assert "real content here" in output

    def test_noise_and_content_mixed(self) -> None:
        """End-to-end: noise interspersed with real content is filtered."""
        input_text = (
            "\u2736\n"
            "Nucleating\u2026\n"
            "(thinking)\n"
            "[OK] Checkpoint saved\n"
            "\u2500" * 40 + "\n"
            "Pollinating\u2026\n"
            "git commit -m 'fix'\n"
        )
        output = self._run_main(input_text)
        assert "[OK] Checkpoint saved" in output
        assert "git commit -m 'fix'" in output
        assert "Nucleating" not in output
        assert "Pollinating" not in output
        assert "(thinking)" not in output


# ---------------------------------------------------------------------------
# TestToolCallMarker — regression tests for ⏺ (U+23FA) preservation
# Issue #2835: ⏺ was incorrectly included in SPINNERS, causing tool call
# lines to be stripped and thinking stall detection to false-positive.
# ---------------------------------------------------------------------------


class TestToolCallMarker:
    """⏺ (U+23FA BLACK CIRCLE FOR RECORD) must NEVER be treated as a spinner.

    Claude Code uses ⏺ as the tool call marker.  Loom's thinking stall
    detector counts occurrences of ⏺ in captured logs to determine whether
    the agent made any tool calls.  If ⏺ is in SPINNERS (and thus stripped by
    _strip_leading_spinners), the detector sees zero markers and incorrectly
    classifies the session as a thinking stall — even when the agent was
    actively making tool calls.  See issue #2835.
    """

    _TOOL_CALL_MARKER = "\u23fa"  # ⏺

    def test_tool_call_marker_not_in_spinners(self) -> None:
        """⏺ (U+23FA) must not be a member of the SPINNERS set."""
        assert self._TOOL_CALL_MARKER not in SPINNERS, (
            "⏺ (U+23FA) is in SPINNERS — this will cause thinking stall "
            "false positives because tool call lines will be stripped. "
            "See issue #2835."
        )

    def test_other_spinners_still_present(self) -> None:
        """The legitimate TUI spinner characters must remain in SPINNERS."""
        for ch in "\u2736\u273b\u273d\u2733\u2722\u00b7":
            assert ch in SPINNERS, f"Spinner char {ch!r} unexpectedly removed from SPINNERS"

    def test_tool_call_line_not_noise(self) -> None:
        """A line starting with ⏺ is a tool call and must not be flagged as TUI noise."""
        tool_call_line = "\u23fa Read(file_path: /foo/bar.py)"
        assert is_tui_noise(tool_call_line) is False, (
            "Tool call line starting with ⏺ was classified as TUI noise"
        )

    def test_tool_call_line_with_args_not_noise(self) -> None:
        """A typical tool call line (⏺ + tool name + args) must not be noise.

        Real tool calls always include text after ⏺, e.g.:
          ⏺ Read(file_path: /repo/src/foo.py)
          ⏺ Bash(command: git status)
        These are substantially longer than the short-fragment threshold and
        must survive the TUI noise filter intact.
        """
        # Real tool call lines have spaces and are > 5 chars, so debris check passes
        assert is_tui_noise("\u23fa Read(file_path: /repo/src/foo.py)") is False
        assert is_tui_noise("\u23fa Bash(command: git status)") is False
        assert is_tui_noise("\u23fa Update Todos") is False

    def test_clean_file_preserves_tool_call_lines(self) -> None:
        """clean_file must keep lines containing ⏺ intact (issue #2835 regression)."""
        content = (
            "Nucleating\u2026\n"
            "\u23fa Read(file_path: /repo/src/foo.py)\n"
            "(thinking)\n"
            "\u23fa Bash(command: pytest tests/)\n"
            "git commit -m 'fix'\n"
        )
        with tempfile.NamedTemporaryFile(mode="w", suffix=".log", delete=False) as f:
            f.write(content)
            path = f.name
        try:
            result = clean_file(path)
        finally:
            Path(path).unlink(missing_ok=True)

        assert "\u23fa Read(file_path: /repo/src/foo.py)" in result, (
            "Tool call line with ⏺ was stripped by clean_file"
        )
        assert "\u23fa Bash(command: pytest tests/)" in result, (
            "Tool call line with ⏺ was stripped by clean_file"
        )
        assert "Nucleating" not in result
        assert "(thinking)" not in result

    def test_main_preserves_tool_call_lines(self) -> None:
        """main() (real-time pipe filter) must not strip ⏺-prefixed lines."""
        input_text = (
            "Nucleating\u2026\n"
            "\u23fa Read(file_path: /src/foo.py)\n"
            "(thinking)\n"
            "\u23fa Bash(command: git status)\n"
            "real output here\n"
        )
        stdin = io.StringIO(input_text)
        stdout = io.StringIO()
        with patch("sys.stdin", stdin), patch("sys.stdout", stdout):
            main()
        output = stdout.getvalue()

        assert "\u23fa Read(file_path: /src/foo.py)" in output, (
            "Tool call line with ⏺ was stripped by main() pipe filter"
        )
        assert "\u23fa Bash(command: git status)" in output, (
            "Tool call line with ⏺ was stripped by main() pipe filter"
        )
        assert "Nucleating" not in output
        assert "(thinking)" not in output

    def test_v2140_animation_words_filtered(self) -> None:
        """Animation words added in Claude Code v2.1.40+ must be filtered."""
        new_words = [
            "Frosting\u2026",
            "Befuddling\u2026",
            "Moseying\u2026",
            "Sashaying\u2026",
            "Waltzing\u2026",
            "Lollygagging\u2026",
            "Gallivanting\u2026",
        ]
        for word in new_words:
            assert is_tui_noise(word) is True, (
                f"v2.1.40+ animation word {word!r} not detected as TUI noise"
            )

    def test_v2140_animation_words_filtered_in_stream(self) -> None:
        """New animation words are suppressed by the real-time pipe filter."""
        input_text = (
            "Frosting\u2026\n"
            "Befuddling\u2026\n"
            "\u23fa Read(file_path: /src/foo.py)\n"
            "Lollygagging\u2026\n"
            "real output\n"
        )
        stdin = io.StringIO(input_text)
        stdout = io.StringIO()
        with patch("sys.stdin", stdin), patch("sys.stdout", stdout):
            main()
        output = stdout.getvalue()

        assert "Frosting" not in output
        assert "Befuddling" not in output
        assert "Lollygagging" not in output
        assert "\u23fa Read(file_path: /src/foo.py)" in output
        assert "real output" in output
