"""Base classes for phase runners."""

from __future__ import annotations

import os
import re
import subprocess
import sys
import time
from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path
from typing import TYPE_CHECKING, Any, Protocol

from loom_tools.checkpoints import read_checkpoint
from loom_tools.claim import extend_claim
from loom_tools.common.logging import log_warning, strip_ansi
from loom_tools.common.state import read_json_file

if TYPE_CHECKING:
    from loom_tools.shepherd.context import ShepherdContext

# How often (in seconds) to poll the progress file during agent-wait-bg.sh
_HEARTBEAT_POLL_INTERVAL = 5

# Minimum characters of non-header content required for "meaningful output".
# Sessions with less than this are treated as transient spawn failures
# (e.g., Claude API error on startup).  See issues #2135, #2381, #2401.
LOW_OUTPUT_MIN_CHARS = 100

# Sentinel line written by claude-wrapper.sh just before invoking the Claude
# CLI.  Output before this marker is wrapper pre-flight boilerplate and should
# be excluded when measuring meaningful output.  See issue #2401.
_CLI_START_SENTINEL = "# CLAUDE_CLI_START"

# Sentinel written by claude-wrapper.sh when authentication pre-flight fails.
# Auth failures are not transient (e.g., parent session holds config lock) and
# should not be retried.  See issue #2508.
_AUTH_FAILURE_SENTINEL = "# AUTH_PREFLIGHT_FAILED"

# MCP pre-flight failures are detected via this sentinel written by the
# wrapper's run_preflight_checks().  Unlike auth failures, MCP failures
# *are* retryable (the MCP server may recover after a rebuild).  The
# sentinel is checked in _is_mcp_failure() alongside runtime patterns.
# See issue #2706.
_MCP_PREFLIGHT_SENTINEL = "# MCP_PREFLIGHT_FAILED"

# Maximum retries for low-output detection, with exponential backoff.
LOW_OUTPUT_MAX_RETRIES = 3
LOW_OUTPUT_BACKOFF_SECONDS = [2, 4, 8]

# If a single low-output attempt takes longer than this (in seconds),
# the failure is likely an infrastructure issue (auth timeouts, lock
# contention) rather than a transient blip.  Skip further retries to
# avoid burning 9+ minutes on futile attempts.  See issue #2519.
LOW_OUTPUT_MAX_ATTEMPT_SECONDS = 90

# Cause-specific retry strategies for low-output classification.
# Maps root cause → (max_retries, backoff_seconds).
# See issue #2518.
LOW_OUTPUT_RETRY_STRATEGIES: dict[str, tuple[int, list[int]]] = {
    "auth_timeout": (0, []),
    "auth_lock_contention": (1, [60]),
    "api_unreachable": (1, [30]),
    "wrapper_crash": (0, []),
    "unknown": (LOW_OUTPUT_MAX_RETRIES, list(LOW_OUTPUT_BACKOFF_SECONDS)),
}

# How often (in seconds) to extend the file-based claim during worker polling.
# The claim TTL is 2 hours; extending every 30 minutes provides ample margin.
_CLAIM_EXTEND_INTERVAL = 1800

# MCP failure detection patterns (case-insensitive).
# These patterns appear in Claude CLI output when the MCP server fails
# to initialize, causing an immediate exit with no useful work done.
MCP_FAILURE_PATTERNS = [
    "MCP server failed",
    "MCP.*failed",
    "mcp server failed",
]

# Minimum characters of non-header output for a session to be considered
# "productive" when checking MCP failure patterns.  Sessions with more output
# than this are assumed to have done real work — the "MCP server failed" text
# is just Claude CLI status-bar noise, not a real failure.
# See issues #2374 and #2381.
MCP_FAILURE_MIN_OUTPUT_CHARS = 500

# Maximum retries for MCP failure detection, with longer backoff.
# MCP failures are often systemic (stale build, resource contention)
# so we use longer backoff than low-output.
MCP_FAILURE_MAX_RETRIES = 3
MCP_FAILURE_BACKOFF_SECONDS = [5, 15, 30]

# Ghost session detection and retry settings.
# A "ghost session" is one that exits almost immediately (0s duration)
# with no meaningful output — the CLI spawned but never did any work.
# These are infrastructure failures (API blip, spawn race) that should
# NOT consume the main retry budget intended for legitimate failures.
# See issues #2604, #2644.
GHOST_SESSION_MAX_RETRIES = 3
GHOST_SESSION_BACKOFF_SECONDS = [10, 30, 60]

# Cause-specific retry strategies for ghost session classification.
# Maps root cause → (max_retries, backoff_seconds).
# Similar to LOW_OUTPUT_RETRY_STRATEGIES.  See issue #2644.
GHOST_RETRY_STRATEGIES: dict[str, tuple[int, list[int]]] = {
    "auth_failure": (0, []),           # Auth issues won't resolve with retries
    "mcp_init_failure": (3, [10, 30, 60]),  # MCP failures need retries; startup monitor now kills degraded sessions (issue #2652)
    "api_unreachable": (1, [30]),      # API blip might be transient
    "wrapper_crash": (0, []),          # Script errors won't self-heal
    "unknown": (GHOST_SESSION_MAX_RETRIES, list(GHOST_SESSION_BACKOFF_SECONDS)),
}

# Degraded session detection patterns.
# These appear in Claude CLI output when the session is running under
# rate limits or has entered a non-productive loop (e.g., "Crystallizing..."
# repeated).  A degraded session produces no useful work and should be
# aborted early rather than waiting for the full timeout.  See issue #2631.
DEGRADED_SESSION_PATTERNS = [
    # Rate limit warnings embedded in Claude CLI output
    re.compile(r"You've used \d+% of your weekly limit", re.IGNORECASE),
    # Crystallizing loop — Claude's extended thinking under resource pressure
    re.compile(r"Crystallizing", re.IGNORECASE),
]

# Minimum number of "Crystallizing" occurrences in the last N lines
# to classify a session as degraded (a single occurrence during normal
# thinking is not concerning).
DEGRADED_CRYSTALLIZING_THRESHOLD = 5

# Minimum lines of log tail to scan for degraded session patterns.
DEGRADED_SCAN_TAIL_LINES = 100

# How often (in seconds) to scan the log for degradation during polling.
# Must be a multiple of _HEARTBEAT_POLL_INTERVAL for alignment.
_DEGRADED_SCAN_INTERVAL = 30

# Wall-clock duration threshold (in seconds) for ghost detection.
# Ghost detection is content-first: sessions with no meaningful output
# are classified as ghosts if they also ran for less than this threshold.
# The threshold is wide (30s) because MCP initialization delays can push
# ghost sessions to 7s+ of wall-clock time while producing only spinner
# output.  See issues #2639, #2659.
#
# Sessions exceeding this threshold with no meaningful output are
# classified as low-output (exit code 6) instead of ghost (exit code 10).
GHOST_SESSION_WALL_CLOCK_THRESHOLD = 30.0

# Systemic failure patterns detected in session logs.
# These indicate infrastructure-level failures (auth timeout, API outage)
# that will NOT resolve with retries.  When detected after a low-output
# or MCP failure, the shepherd should abort immediately instead of wasting
# time on futile retry cycles.  See issue #2521.
#
# Note: check_auth_status() logs at [WARN] level (issue #2541), so only
# the fatal caller path emits [ERROR].  The primary active pattern here is
# "Authentication pre-flight check failed" (from run_preflight_checks).
# Other auth patterns are retained as defense-in-depth.
SYSTEMIC_FAILURE_PATTERNS = [
    re.compile(r"\[ERROR\]\s*Authentication check timed out", re.IGNORECASE),
    re.compile(r"\[ERROR\]\s*Authentication pre-flight check failed", re.IGNORECASE),
    re.compile(r"\[ERROR\]\s*Authentication check command failed", re.IGNORECASE),
    re.compile(r"\[ERROR\]\s*Authentication check failed", re.IGNORECASE),
    re.compile(r"\[ERROR\]\s*API endpoint unreachable", re.IGNORECASE),
]

# Regex patterns for CLI spinner/thinking noise that should not count toward
# output volume when checking for MCP failures.  The Claude CLI terminal
# capture can produce garbled spinner frames (interleaved characters from
# animated spinners) and repeated "(thinking)" lines that inflate the
# character count without representing productive work.  See issue #2465.
# Regex for spinner/thinking phrases matched per-line (already stripped).
# Matches any single capitalized word followed by ellipsis, e.g.
# "Tinkering…", "Thinking...", "Bloviating…", "Mulling...", etc.
# Claude's extended thinking mode uses many creative gerund phrases
# beyond the original fixed list, so we match the general pattern
# instead of enumerating them.  See issue #2421.
_SPINNER_PHRASE_RE = re.compile(
    r"^[A-Z][a-z]+(?:…|\.{2,3})$"
)

# Characters used by Claude CLI animated spinners.  Lines dominated by
# these characters (mixed with a few regular chars from animation frame
# interleaving) are garbled spinner fragments, not productive output.
_SPINNER_DECORATION_CHARS = frozenset("✶✻✽✳✢·✦✧★☆●○◆◇▪▫•‣⁃※✱✲✴✵✷✸✹✺⟳⟲")

# Fraction of non-whitespace characters that must be decoration chars
# for a line to be classified as garbled spinner noise.
_SPINNER_DECORATION_THRESHOLD = 0.3

# --------------------------------------------------------------------------- #
# Claude Code UI chrome filtering (issue #2435)
#
# Claude Code's startup UI (version banner, model info, tips, permission
# indicators, usage limits, etc.) generates ~2,700+ characters of terminal
# output even when a session does zero actual work.  This defeats the
# character-count thresholds in _is_low_output_session() and _is_mcp_failure().
#
# The patterns below identify known UI chrome lines so they can be stripped
# before counting output volume.
# --------------------------------------------------------------------------- #

# Block-element and box-drawing characters used in Claude Code's decorative
# banner art.  Lines dominated by these characters are UI decoration.
_UI_BLOCK_CHARS = frozenset(
    "▐▛▜▝▘█▌▀▄░▒▓│╭╮╰╯├┤┬┴┼─═║╔╗╚╝╠╣╦╩╬"
)

# Fraction of non-whitespace characters that must be block/box-drawing
# chars for a line to be classified as decorative UI art.
_UI_BLOCK_THRESHOLD = 0.5

# Per-line regex patterns matching known Claude Code UI chrome text.
# Applied to ANSI-stripped lines.
_UI_CHROME_LINE_PATTERNS = [
    # Version banner: " ▐▛███▜▌   Claude Code v2.1.29"
    re.compile(r"Claude Code\s+v\d"),
    # Model info: "Opus 4.5 · Claude Max", "Sonnet 4.5 · API", etc.
    re.compile(r"(?:Opus|Sonnet|Haiku)\s+\d+\.\d+"),
    # Working directory display (after banner art prefix)
    re.compile(r"~/"),
    # Separator lines: pure horizontal rules
    re.compile(r"^─+$"),
    # Prompt / suggestion lines
    re.compile(r"^❯"),
    re.compile(r'^Try "'),
    # Permission mode indicators
    re.compile(r"bypass permissions"),
    re.compile(r"⏵"),
    # Keyboard hints
    re.compile(r"esc to interrupt"),
    re.compile(r"shift\+tab"),
    # Usage limit warnings
    re.compile(r"You've used"),
    re.compile(r"\d+%\s+of\b"),
    re.compile(r"your weekly"),
    re.compile(r"resets\s+\w+\s+\d"),
    # Shell prompt capture: "user@host dir % "
    re.compile(r"^\w+@\w+\s+\S+\s+%\s*$"),
    # Skill/command echo: "/builder 2055"
    re.compile(r"^/\w+\s+\d+\s*$"),
    # Spinner status line: "· Photosynthesizing…"
    re.compile(r"^·\s"),
]


# Regex to match [ERROR] lines in wrapper/CLI log files.
# Format: "[timestamp] [ERROR] message" or just "[ERROR] message".
_LOG_ERROR_RE = re.compile(r"\[ERROR\]\s*(.*)")


def extract_log_errors(log_path: Path, *, max_errors: int = 3) -> list[str]:
    """Extract the last N [ERROR] lines from a session log file.

    Reads the log, strips ANSI codes, and returns the error messages
    (without timestamp prefixes) from the last ``max_errors`` lines
    matching ``[ERROR]``.  Returns an empty list if the log doesn't
    exist or contains no error lines.

    Args:
        log_path: Path to the worker session log file.
        max_errors: Maximum number of error lines to return.

    Returns:
        List of error message strings, most recent last.
    """
    if not log_path.is_file():
        return []

    try:
        content = log_path.read_text()
        stripped = strip_ansi(content)
        errors: list[str] = []
        for line in stripped.splitlines():
            m = _LOG_ERROR_RE.search(line)
            if m:
                errors.append(m.group(1).strip())
        return errors[-max_errors:]
    except OSError:
        return []


def _strip_ui_chrome(text: str) -> str:
    """Remove Claude Code UI chrome from output text.

    Strips startup UI elements that appear in every CLI session regardless
    of whether productive work was performed:

    - Version banner and model info
    - Decorative block/box-drawing banner art
    - Working directory display
    - Separator / horizontal rule lines
    - Prompt suggestions and permission mode indicators
    - Usage limit warnings
    - Shell prompt lines and keyboard hints

    This prevents the ~2,700+ characters of UI chrome from inflating the
    output volume metric used by ``_is_low_output_session()`` and
    ``_is_mcp_failure()``.  See issue #2435.

    Args:
        text: CLI output text (already ANSI-stripped).

    Returns:
        Text with UI chrome lines removed.
    """
    lines = text.splitlines()
    filtered = []
    for line in lines:
        stripped_line = line.strip()
        if not stripped_line:
            filtered.append(line)
            continue

        # Check against known UI chrome patterns
        if any(p.search(stripped_line) for p in _UI_CHROME_LINE_PATTERNS):
            continue

        # Lines dominated by block/box-drawing characters (banner art)
        non_ws = [c for c in stripped_line if not c.isspace()]
        if non_ws:
            block_count = sum(1 for c in non_ws if c in _UI_BLOCK_CHARS)
            if block_count / len(non_ws) >= _UI_BLOCK_THRESHOLD:
                continue

        filtered.append(line)

    return "\n".join(filtered)


def _strip_spinner_noise(text: str) -> str:
    """Remove CLI spinner and thinking noise from output text.

    Strips:
    - "(thinking)" lines
    - Known spinner phrases ("Tinkering...", "Thinking...", etc.)
    - Lines dominated by Unicode spinner decoration characters

    This prevents garbled terminal capture of animated spinners from
    inflating the output volume metric used by ``_is_mcp_failure()``.
    See issue #2465.

    Args:
        text: CLI output text (already ANSI-stripped).

    Returns:
        Text with spinner noise removed.
    """
    lines = text.splitlines()
    filtered = []
    for line in lines:
        stripped_line = line.strip()
        if not stripped_line:
            filtered.append(line)
            continue

        # "(thinking)" lines
        if stripped_line == "(thinking)":
            continue

        # Known spinner phrases
        if _SPINNER_PHRASE_RE.match(stripped_line):
            continue

        # Garbled spinner lines: lines where >30% of non-whitespace
        # chars are Unicode decoration characters from spinner animation
        non_ws = [c for c in stripped_line if not c.isspace()]
        if non_ws:
            deco_count = sum(1 for c in non_ws if c in _SPINNER_DECORATION_CHARS)
            if deco_count / len(non_ws) > _SPINNER_DECORATION_THRESHOLD:
                continue

        filtered.append(line)

    return "\n".join(filtered)


class PhaseStatus(Enum):
    """Result status of phase execution."""

    SUCCESS = "success"
    SKIPPED = "skipped"
    FAILED = "failed"
    SHUTDOWN = "shutdown"
    STUCK = "stuck"


@dataclass
class PhaseResult:
    """Result of phase execution."""

    status: PhaseStatus
    message: str = ""
    phase_name: str = ""
    data: dict[str, Any] = field(default_factory=dict)

    @property
    def is_success(self) -> bool:
        return self.status in (PhaseStatus.SUCCESS, PhaseStatus.SKIPPED)

    @property
    def is_shutdown(self) -> bool:
        return self.status == PhaseStatus.SHUTDOWN


class PhaseRunner(Protocol):
    """Protocol for phase execution.

    Each phase runner must implement:
    - should_skip: Check if phase should be skipped
    - run: Execute the phase
    - validate: Validate phase contract after execution
    """

    def should_skip(self, ctx: ShepherdContext) -> tuple[bool, str]:
        """Check if phase should be skipped.

        Returns:
            Tuple of (should_skip, reason)
        """
        ...

    def run(self, ctx: ShepherdContext) -> PhaseResult:
        """Execute the phase.

        Returns:
            PhaseResult with status and message
        """
        ...

    def validate(self, ctx: ShepherdContext) -> bool:
        """Validate phase contract after execution.

        Returns:
            True if contract is satisfied
        """
        ...


class BasePhase:
    """Base class for phase runners with helper methods for creating PhaseResults.

    Subclasses should set the ``phase_name`` class attribute to the name of
    the phase (e.g., "builder", "judge"). This name is automatically used
    in all PhaseResult objects created via the helper methods.

    Example usage::

        class MyPhase(BasePhase):
            phase_name = "my_phase"

            def run(self, ctx: ShepherdContext) -> PhaseResult:
                if some_error:
                    return self.failed("something went wrong", {"detail": "info"})
                return self.success("phase completed")
    """

    phase_name: str = ""

    def result(
        self,
        status: PhaseStatus,
        message: str = "",
        data: dict[str, Any] | None = None,
    ) -> PhaseResult:
        """Create a PhaseResult with this phase's name.

        Args:
            status: The status of the phase result.
            message: A human-readable message describing the result.
            data: Optional dictionary of additional data.

        Returns:
            A PhaseResult with the phase_name set automatically.
        """
        return PhaseResult(
            status=status,
            message=message,
            phase_name=self.phase_name,
            data=data or {},
        )

    def success(
        self, message: str = "", data: dict[str, Any] | None = None
    ) -> PhaseResult:
        """Create a successful PhaseResult.

        Args:
            message: A human-readable message describing the success.
            data: Optional dictionary of additional data.

        Returns:
            A PhaseResult with SUCCESS status.
        """
        return self.result(PhaseStatus.SUCCESS, message, data)

    def failed(
        self, message: str = "", data: dict[str, Any] | None = None
    ) -> PhaseResult:
        """Create a failed PhaseResult.

        Args:
            message: A human-readable message describing the failure.
            data: Optional dictionary of additional data.

        Returns:
            A PhaseResult with FAILED status.
        """
        return self.result(PhaseStatus.FAILED, message, data)

    def skipped(
        self, message: str = "", data: dict[str, Any] | None = None
    ) -> PhaseResult:
        """Create a skipped PhaseResult.

        Args:
            message: A human-readable message describing why skipped.
            data: Optional dictionary of additional data.

        Returns:
            A PhaseResult with SKIPPED status.
        """
        return self.result(PhaseStatus.SKIPPED, message, data)

    def shutdown(
        self, message: str = "", data: dict[str, Any] | None = None
    ) -> PhaseResult:
        """Create a shutdown PhaseResult.

        Args:
            message: A human-readable message describing the shutdown.
            data: Optional dictionary of additional data.

        Returns:
            A PhaseResult with SHUTDOWN status.
        """
        return self.result(PhaseStatus.SHUTDOWN, message, data)

    def stuck(
        self, message: str = "", data: dict[str, Any] | None = None
    ) -> PhaseResult:
        """Create a stuck PhaseResult.

        Args:
            message: A human-readable message describing the stuck state.
            data: Optional dictionary of additional data.

        Returns:
            A PhaseResult with STUCK status.
        """
        return self.result(PhaseStatus.STUCK, message, data)


def _read_heartbeats(
    progress_file: Path, *, phase: str | None = None
) -> list[dict[str, Any]]:
    """Read heartbeat milestones from a shepherd progress file.

    Args:
        progress_file: Path to the shepherd progress JSON file.
        phase: If provided, only return heartbeats that occurred after
            the most recent ``phase_entered`` milestone for this phase.
            This prevents stale heartbeats from earlier phases from
            being displayed.

    Returns a list of heartbeat milestone dicts, each with
    ``timestamp`` and ``data.action`` keys.
    """
    data = read_json_file(progress_file)
    if not isinstance(data, dict):
        return []

    milestones = data.get("milestones", [])

    # Find the index of the most recent phase_entered milestone for this phase.
    # Only heartbeats between that point and the next phase_entered belong to
    # the current phase, preventing stale heartbeats from earlier phases from
    # being displayed during later phases.
    start_index = 0
    if phase:
        for i, m in enumerate(milestones):
            if (
                m.get("event") == "phase_entered"
                and m.get("data", {}).get("phase") == phase
            ):
                start_index = i + 1

    # Find the end boundary: the next phase_entered after start_index
    # (for any phase). During live polling, this boundary won't exist yet
    # so end_index == len(milestones), which is the common case.
    end_index = len(milestones)
    if phase and start_index > 0:
        for i in range(start_index, len(milestones)):
            if milestones[i].get("event") == "phase_entered":
                end_index = i
                break

    return [
        m
        for m in milestones[start_index:end_index]
        if m.get("event") == "heartbeat"
    ]


def _print_heartbeat(action: str) -> None:
    """Print a heartbeat status line to stderr.

    Uses dim/gray ANSI to differentiate from cyan phase headers.
    Format: ``[YYYY-MM-DDTHH:MM:SSZ] ⟳ action``
    """
    ts = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    # \033[2m = dim, \033[0m = reset
    print(f"\033[2m[{ts}] \u27f3 {action}\033[0m", file=sys.stderr)


def _get_cli_output(stripped: str) -> str:
    """Extract non-header output produced after the CLI start sentinel.

    If the ``# CLAUDE_CLI_START`` sentinel is present, only lines after the
    **last** occurrence are considered (the wrapper may emit multiple sentinels
    when retrying).  Lines starting with ``# `` are always excluded as log
    headers.

    If no sentinel is found the session is considered a low-output session
    (the wrapper always writes the sentinel before invoking Claude, so
    its absence means Claude never started).  See issue #2405.

    Args:
        stripped: ANSI-stripped log file content.

    Returns:
        The meaningful (non-header, post-sentinel) output as a single string.
    """
    lines = stripped.splitlines()

    # Find the last sentinel index.
    sentinel_idx: int | None = None
    for i, line in enumerate(lines):
        if line.strip() == _CLI_START_SENTINEL:
            sentinel_idx = i

    if sentinel_idx is None:
        # No sentinel means Claude CLI never started — wrapper failed
        # before reaching execution.  Return empty string so callers
        # correctly treat this as low output.  See issue #2473.
        return ""

    start = sentinel_idx + 1
    return "\n".join(line for line in lines[start:] if not line.startswith("# "))


def _is_mcp_failure(log_path: Path) -> bool:
    """Check if a session log indicates an MCP server initialization failure.

    Detects cases where the Claude CLI exits immediately because the MCP
    server (mcp-loom) failed to initialize.  This is a distinct failure mode
    from generic low-output sessions (API errors, network issues) because it
    typically has a systemic cause (stale build, resource contention) that
    benefits from different retry/backoff strategy.

    To avoid false positives on productive sessions (where the Claude CLI
    status bar may show "1 MCP server failed" as informational text), the
    function checks output volume.  Sessions that produced substantial
    non-header output are assumed productive — the MCP text is status-bar
    noise, not a real failure.  See issues #2374 and #2381.

    Note: A previous implementation used ``st_mtime - st_ctime`` as a
    duration gate, but this is always ~0 for actively-written log files
    because writing updates both timestamps simultaneously.

    Args:
        log_path: Path to the worker session log file.

    Returns:
        True if the log contains MCP failure indicators **and** the session
        produced minimal output (below the output volume threshold).
    """
    if not log_path.is_file():
        return False

    try:
        content = log_path.read_text()
        stripped = strip_ansi(content)

        # Check for explicit MCP pre-flight sentinel first (most reliable).
        # The wrapper writes this when check_mcp_server() fails before
        # the CLI even starts.  No output-volume gate needed — the
        # sentinel is definitive.  See issue #2706.
        if _MCP_PREFLIGHT_SENTINEL in stripped:
            return True

        # If the session produced substantial CLI output beyond headers,
        # wrapper pre-flight, spinner noise, and UI chrome, it was
        # productive — MCP text is just status bar noise.
        # See issues #2374, #2381, #2401, #2435, #2465.
        cli_output = _get_cli_output(stripped)
        cleaned_output = _strip_ui_chrome(_strip_spinner_noise(cli_output))
        if len(cleaned_output.strip()) >= MCP_FAILURE_MIN_OUTPUT_CHARS:
            return False

        for pattern in MCP_FAILURE_PATTERNS:
            if re.search(pattern, cli_output, re.IGNORECASE):
                return True
    except OSError:
        pass
    return False


def _is_low_output_session(log_path: Path) -> bool:
    """Check if a session log indicates a low-output session (transient spawn failure).

    A session is considered low-output when the log file exists but has
    no meaningful output (< LOW_OUTPUT_MIN_CHARS non-header chars).

    This detects cases where the Claude CLI spawns but immediately exits due to
    transient API errors, without producing any substantive work.

    Note: A previous implementation also checked ``st_mtime - st_ctime`` as
    a duration gate, but this is always ~0 for actively-written log files
    because writing updates both timestamps simultaneously.  The output-size
    check alone is sufficient and reliable.  See issue #2381.

    Args:
        log_path: Path to the worker session log file.

    Returns:
        True if the session appears to be a low-output session.
    """
    if not log_path.is_file():
        # No log file at all — could be spawn failure, not low output.
        return False

    try:
        content = log_path.read_text()
        stripped = strip_ansi(content)

        # If the sentinel is absent, Claude never started — treat as low
        # output regardless of how much wrapper pre-flight output exists.
        # See issue #2405.
        if _CLI_START_SENTINEL not in stripped:
            return True

        # Exclude log header lines and wrapper pre-flight output (everything
        # before the last ``# CLAUDE_CLI_START`` sentinel) so that only
        # actual Claude CLI output counts.  See issues #2135, #2381, #2401.
        #
        # Strip UI chrome (version banner, tips, permission indicators, etc.)
        # and spinner noise before counting to prevent the ~2,700+ chars of
        # startup UI from defeating the threshold.  See issue #2435.
        cli_output = _get_cli_output(stripped)
        cleaned = _strip_ui_chrome(_strip_spinner_noise(cli_output))
        return len(cleaned.strip()) < LOW_OUTPUT_MIN_CHARS
    except OSError:
        return False


def _is_auth_failure(log_path: Path) -> bool:
    """Check if a session log indicates an authentication pre-flight failure.

    Auth failures are **systemic** when running as a subprocess of a parent
    Claude Code session (the parent holds the config lock, so retries will
    always time out).  This is distinct from generic low-output sessions which
    *are* worth retrying.

    Detection uses two methods:
    1. Sentinel: ``# AUTH_PREFLIGHT_FAILED`` written by the wrapper (issue #2508).
    2. Fallback patterns: known ``[ERROR]`` messages from ``check_auth_status``
       and other systemic failure indicators (issue #2521).

    Args:
        log_path: Path to the worker session log file.

    Returns:
        True if the log contains auth failure indicators.
    """
    if not log_path.is_file():
        return False

    try:
        content = log_path.read_text()
        stripped = strip_ansi(content)

        # Check for explicit sentinel first (most reliable)
        if _AUTH_FAILURE_SENTINEL in stripped:
            return True

        # Fallback: check for known systemic failure patterns in log text
        for pattern in SYSTEMIC_FAILURE_PATTERNS:
            if pattern.search(stripped):
                return True
    except OSError:
        pass

    return False


def _is_ghost_session(
    log_path: Path, *, wall_clock_duration: float | None = None
) -> bool:
    """Check if a session log indicates a ghost session (no meaningful work).

    A ghost session is one where the CLI process spawned but produced no
    meaningful output — only spinner noise, ``(thinking)`` lines, and
    startup boilerplate.  This is distinct from low-output sessions (which
    exceed the duration threshold) and MCP failures (which have specific
    MCP error patterns).

    Detection is **content-first**: the log is analysed for meaningful
    output *before* checking duration.  Duration acts as a secondary
    signal to distinguish ghost sessions (infrastructure failures) from
    long-running stuck sessions.  See issue #2659.

    1. If the log contains meaningful CLI output → **not a ghost**.
    2. If no meaningful output, check duration:
       - Wall-clock ≤ GHOST_SESSION_WALL_CLOCK_THRESHOLD (30s) → ghost.
       - File-timestamp fallback ≤ same threshold → ghost.
       - Duration exceeds threshold → not a ghost (classified as
         low-output via exit code 6 instead).

    The wide 30s threshold accommodates MCP initialization delays that
    can push ghost sessions to 7s+ of wall-clock time while producing
    only spinner output.  See issues #2639, #2644, #2659.

    These represent infrastructure-level failures (API blip, spawn race,
    resource contention) rather than agent failures, and should be retried
    on a separate budget to preserve the main retry budget for legitimate
    failures.  See issues #2604, #2644.

    Args:
        log_path: Path to the worker session log file.
        wall_clock_duration: Wall-clock elapsed time (in seconds) from
            ``run_worker_phase()``.  When provided, this is used instead
            of file timestamps for duration estimation.  See issue #2639.

    Returns:
        True if the session appears to be a ghost (no meaningful work).
    """
    if not log_path.is_file():
        return False

    try:
        content = log_path.read_text()
        stripped = strip_ansi(content)

        # Content-first: check for meaningful output before duration.
        # This avoids false negatives where MCP init delays push ghost
        # sessions past the old 5s duration gate.  See issue #2659.
        cli_output = _get_cli_output(stripped)
        cleaned = _strip_ui_chrome(_strip_spinner_noise(cli_output))
        if len(cleaned.strip()) >= LOW_OUTPUT_MIN_CHARS:
            # Meaningful output present — not a ghost regardless of duration.
            return False

        # No meaningful output.  Use duration as a secondary signal to
        # distinguish ghost (infrastructure failure, < 30s) from
        # stuck/killed sessions (> 30s, classified as low-output instead).
        if wall_clock_duration is not None:
            return wall_clock_duration <= GHOST_SESSION_WALL_CLOCK_THRESHOLD
        else:
            # Fallback to file timestamp heuristic (legacy path).
            # File timestamps are unreliable (see issues #2639, #2644, #2659)
            # but still useful as a rough upper bound when wall-clock timing
            # is unavailable.
            stat = log_path.stat()
            duration = stat.st_mtime - stat.st_ctime
            return duration <= GHOST_SESSION_WALL_CLOCK_THRESHOLD
    except OSError:
        return False


def _is_degraded_session(log_path: Path) -> bool:
    """Check if a session log indicates a degraded session (rate limits, Crystallizing loops).

    A degraded session is one where the builder ran but produced no useful work
    because it was resource-constrained.  This manifests as:
    - Rate limit warnings ("You've used 87% of your weekly limit")
    - Repeated "Crystallizing..." output (Claude's extended thinking under pressure)

    Degraded sessions are distinct from low-output sessions (which never started)
    and stuck sessions (which started but stopped making progress).  A degraded
    session actively produces output but the output is non-productive.

    Detection requires BOTH:
    1. A rate limit warning in the log, AND
    2. Excessive "Crystallizing" repetitions (>= DEGRADED_CRYSTALLIZING_THRESHOLD)

    This two-signal approach prevents false positives: a single rate limit warning
    during an otherwise productive session is harmless, and brief "Crystallizing"
    during normal thinking is expected.  The combination indicates the session has
    entered a non-recoverable degraded state.  See issue #2631.

    Args:
        log_path: Path to the worker session log file.

    Returns:
        True if the session shows signs of degradation.
    """
    if not log_path.is_file():
        return False

    try:
        content = log_path.read_text()
        stripped = strip_ansi(content)
        cli_output = _get_cli_output(stripped)
        if not cli_output:
            return False

        # Signal 1: Rate limit warning present
        has_rate_limit = bool(DEGRADED_SESSION_PATTERNS[0].search(cli_output))
        if not has_rate_limit:
            return False

        # Signal 2: Excessive Crystallizing repetitions
        lines = cli_output.splitlines()
        tail = lines[-DEGRADED_SCAN_TAIL_LINES:]
        crystallizing_count = sum(
            1 for line in tail if DEGRADED_SESSION_PATTERNS[1].search(line)
        )
        return crystallizing_count >= DEGRADED_CRYSTALLIZING_THRESHOLD

    except OSError:
        return False


def _scan_log_for_degradation(log_path: Path) -> bool:
    """Quick scan of a live log file for degradation patterns.

    This is a lighter-weight version of ``_is_degraded_session()`` designed
    for in-flight polling during ``run_worker_phase()``.  It reads only the
    tail of the file to minimize I/O overhead.

    Returns True if the log shows both rate limit warnings and excessive
    "Crystallizing" repetitions.  See issue #2631.
    """
    if not log_path.is_file():
        return False

    try:
        # Read only the tail to minimize I/O during polling
        with open(log_path, "rb") as f:
            f.seek(0, 2)
            file_size = f.tell()
            # Read last ~10KB (enough for DEGRADED_SCAN_TAIL_LINES of output)
            start_pos = max(0, file_size - 10_000)
            f.seek(start_pos)
            content = f.read().decode("utf-8", errors="replace")

        stripped = strip_ansi(content)
        lines = stripped.splitlines()
        tail = lines[-DEGRADED_SCAN_TAIL_LINES:]
        text = "\n".join(tail)

        has_rate_limit = bool(DEGRADED_SESSION_PATTERNS[0].search(text))
        if not has_rate_limit:
            return False

        crystallizing_count = sum(
            1 for line in tail if DEGRADED_SESSION_PATTERNS[1].search(line)
        )
        return crystallizing_count >= DEGRADED_CRYSTALLIZING_THRESHOLD

    except OSError:
        return False


def _classify_low_output_cause(log_path: Path) -> str:
    """Classify the root cause of a low-output session from the worker log.

    After detecting a low-output session, the log often contains specific error
    patterns that indicate the problem will persist for much longer — or won't
    resolve by retrying at all.  This function parses the log for known
    patterns and returns a cause string used to select a retry strategy
    from ``LOW_OUTPUT_RETRY_STRATEGIES``.

    Args:
        log_path: Path to the worker session log file.

    Returns:
        A cause string: ``"auth_timeout"``, ``"auth_lock_contention"``,
        ``"api_unreachable"``, ``"wrapper_crash"``, or ``"unknown"``
        (generic transient failure).
    """
    if not log_path.is_file():
        return "unknown"

    try:
        content = strip_ansi(log_path.read_text())
    except OSError:
        return "unknown"

    if "Authentication check timed out" in content:
        return "auth_timeout"
    if "Auth cache lock held" in content:
        return "auth_lock_contention"
    if "API endpoint" in content and "unreachable" in content:
        return "api_unreachable"
    if "syntax error near unexpected token" in content or (
        "syntax error" in content and "unexpected end of file" in content
    ):
        return "wrapper_crash"
    return "unknown"


def _session_log_path(repo_root: Path, name: str) -> Path:
    """Compute the log file path for a given session name.

    Agent sessions write their logs to ``.loom/logs/loom-{name}.log``.
    See ``agent_spawn.py`` for the naming convention.

    Args:
        repo_root: Repository root path.
        name: Session name (e.g., "judge-issue-42" or "judge-issue-42-a1").

    Returns:
        Path to the log file.
    """
    return repo_root / ".loom" / "logs" / f"loom-{name}.log"


def _classify_ghost_cause(log_path: Path) -> str:
    """Classify the root cause of a ghost session from the worker log.

    Ghost sessions (0s duration, no meaningful output) can have several root
    causes.  This function parses the log for known patterns and returns a
    cause string used to select a retry strategy from
    ``GHOST_RETRY_STRATEGIES``.

    Args:
        log_path: Path to the worker session log file.

    Returns:
        A cause string: ``"auth_failure"``, ``"mcp_init_failure"``,
        ``"api_unreachable"``, ``"wrapper_crash"``, or ``"unknown"``.

    See issue #2644.
    """
    if not log_path.is_file():
        return "unknown"

    try:
        content = strip_ansi(log_path.read_text())
    except OSError:
        return "unknown"

    # Check for auth-related failures
    if _AUTH_FAILURE_SENTINEL in content:
        return "auth_failure"
    if "Authentication check timed out" in content:
        return "auth_failure"
    if "Authentication check failed" in content:
        return "auth_failure"
    if "Auth cache lock held" in content:
        return "auth_failure"

    # Check for MCP initialization failure
    for pattern in MCP_FAILURE_PATTERNS:
        if re.search(pattern, content, re.IGNORECASE):
            return "mcp_init_failure"

    # Check for API unreachable
    if "API endpoint" in content and "unreachable" in content:
        return "api_unreachable"

    # Check for wrapper script crash
    if "syntax error near unexpected token" in content or (
        "syntax error" in content and "unexpected end of file" in content
    ):
        return "wrapper_crash"

    return "unknown"


def gather_zero_output_postmortem(
    log_path: Path,
    *,
    wall_clock_seconds: float | None = None,
    wait_exit_code: int | None = None,
    sidecar_exit_code: int | None = None,
) -> dict[str, Any]:
    """Gather post-mortem diagnostics for a zero-output CLI session.

    When a CLI session produces zero meaningful output, this function
    collects all available diagnostic signals into a structured dict
    for logging and failure classification.  This enables faster root-cause
    analysis compared to manually inspecting log files.

    Diagnostics gathered:
    - Whether the CLI started (``CLAUDE_CLI_START`` sentinel presence)
    - Pre-flight failure sentinels (auth, MCP)
    - Wrapper and wait exit codes
    - CLI lifetime estimate (time between first and last log timestamps)
    - Rate limit indicators in the log
    - Log errors (``[ERROR]`` lines)
    - CLI output volume (chars after stripping UI chrome and spinners)
    - Log tail (last 15 lines for context)

    All operations are best-effort; OSError and parse failures are caught
    and recorded as ``"unavailable"`` in the relevant field.

    See issue #2766.

    Args:
        log_path: Path to the worker session log file.
        wall_clock_seconds: Wall-clock elapsed time (seconds) from the
            caller, if available.
        wait_exit_code: Exit code from agent-wait-bg.sh.
        sidecar_exit_code: Exit code from the wrapper sidecar file,
            if it differed from ``wait_exit_code``.

    Returns:
        Dict with diagnostic fields.  Always includes a ``"summary"``
        key with a human-readable one-line summary.
    """
    diag: dict[str, Any] = {}

    # -- Exit codes --
    diag["wait_exit_code"] = wait_exit_code
    diag["sidecar_exit_code"] = sidecar_exit_code

    # -- Wall-clock duration --
    diag["wall_clock_seconds"] = (
        round(wall_clock_seconds, 1) if wall_clock_seconds is not None else None
    )

    if not log_path.is_file():
        diag["log_exists"] = False
        diag["summary"] = (
            f"no log file at {log_path} "
            f"(wait_exit={wait_exit_code}, wall={wall_clock_seconds}s)"
        )
        return diag

    diag["log_exists"] = True

    try:
        content = log_path.read_text()
    except OSError as exc:
        diag["log_read_error"] = str(exc)
        diag["summary"] = f"failed to read log: {exc}"
        return diag

    stripped = strip_ansi(content)

    # -- Sentinel detection --
    diag["cli_started"] = _CLI_START_SENTINEL in stripped
    diag["auth_preflight_failed"] = _AUTH_FAILURE_SENTINEL in stripped
    diag["mcp_preflight_failed"] = _MCP_PREFLIGHT_SENTINEL in stripped

    # -- CLI output analysis --
    cli_output = _get_cli_output(stripped)
    cleaned = _strip_ui_chrome(_strip_spinner_noise(cli_output))
    diag["cli_output_chars"] = len(cli_output.strip())
    diag["cli_output_chars_cleaned"] = len(cleaned.strip())

    # -- CLI lifetime estimate from log timestamps --
    # Parse timestamps like [2026-02-18T07:08:04Z] to estimate how long
    # the CLI process ran.
    _TS_RE = re.compile(r"\[(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z)\]")
    timestamps = _TS_RE.findall(stripped)
    if len(timestamps) >= 2:
        try:
            from datetime import datetime, timezone

            first = datetime.fromisoformat(timestamps[0].replace("Z", "+00:00"))
            last = datetime.fromisoformat(timestamps[-1].replace("Z", "+00:00"))
            log_duration = (last - first).total_seconds()
            diag["log_duration_seconds"] = log_duration
            diag["cli_crashed_on_startup"] = log_duration < 5.0
        except (ValueError, TypeError):
            diag["log_duration_seconds"] = None
            diag["cli_crashed_on_startup"] = None
    else:
        diag["log_duration_seconds"] = None
        diag["cli_crashed_on_startup"] = None

    # -- Rate limit indicators --
    rate_limit_patterns = [
        re.compile(r"You've used \d+% of your weekly limit", re.IGNORECASE),
        re.compile(r"Stop and wait for limit to reset", re.IGNORECASE),
        re.compile(r"rate limit", re.IGNORECASE),
        re.compile(r"429", re.IGNORECASE),
    ]
    rate_limit_hits = [
        p.pattern for p in rate_limit_patterns if p.search(cli_output)
    ]
    diag["rate_limit_indicators"] = rate_limit_hits
    diag["has_rate_limit"] = bool(rate_limit_hits)

    # -- Log errors --
    diag["log_errors"] = extract_log_errors(log_path)

    # -- Log tail --
    lines = content.splitlines()
    diag["log_tail"] = lines[-15:] if len(lines) > 15 else lines

    # -- Human-readable summary --
    parts: list[str] = []
    if not diag["cli_started"]:
        parts.append("CLI never started (no CLAUDE_CLI_START sentinel)")
    elif diag["cli_output_chars_cleaned"] == 0:
        parts.append("CLI started but produced zero output")
    else:
        parts.append(f"CLI output: {diag['cli_output_chars_cleaned']} chars (cleaned)")

    if diag["auth_preflight_failed"]:
        parts.append("auth pre-flight FAILED")
    if diag["mcp_preflight_failed"]:
        parts.append("MCP pre-flight FAILED")
    if diag["has_rate_limit"]:
        parts.append("rate limit detected")

    if diag["log_duration_seconds"] is not None:
        parts.append(f"log duration: {diag['log_duration_seconds']:.0f}s")
    if wall_clock_seconds is not None:
        parts.append(f"wall: {wall_clock_seconds:.0f}s")

    exit_info = []
    if wait_exit_code is not None:
        exit_info.append(f"wait={wait_exit_code}")
    if sidecar_exit_code is not None:
        exit_info.append(f"sidecar={sidecar_exit_code}")
    if exit_info:
        parts.append(f"exit({', '.join(exit_info)})")

    if diag["log_errors"]:
        parts.append(f"last error: {diag['log_errors'][-1]}")

    diag["summary"] = "; ".join(parts)
    return diag


def run_worker_phase(
    ctx: ShepherdContext,
    *,
    role: str,
    name: str,
    timeout: int,
    phase: str | None = None,
    worktree: Path | None = None,
    pr_number: int | None = None,
    args: str | None = None,
    planning_timeout: int = 0,
    attempt: int = 0,
) -> int:
    """Run a phase worker and wait for completion.

    This wraps the agent-spawn.sh → agent-wait-bg.sh → agent-destroy.sh flow.
    While waiting, polls the shepherd progress file for heartbeat milestones
    and prints them to stderr so the operator can see ongoing activity.

    Args:
        ctx: Shepherd context
        role: Worker role (e.g., "builder", "judge")
        name: Base session name (e.g., "builder-issue-42")
        timeout: Timeout in seconds
        phase: Phase name for activity detection
        worktree: Optional worktree path
        pr_number: Optional PR number
        args: Optional arguments for the worker
        planning_timeout: If > 0, abort the worker if it stays in the
            ``planning`` checkpoint stage for more than this many seconds.
            Only effective when *worktree* is set.  See issue #2443.
        attempt: Retry attempt number (0-based).  When > 0, a suffix is
            appended to the session name to prevent tmux state pollution
            from rapid create/destroy cycles.  See issue #2639.

    Returns:
        Exit code from agent-wait-bg.sh (or synthetic):
        - 0: Success
        - 3: Shutdown signal
        - 4: Agent stuck after retry
        - 5: Failures are pre-existing (Doctor only)
        - 6: Low output detected (session produced no meaningful output)
        - 7: MCP server failure detected (session exited due to MCP init failure)
        - 8: Planning stall detected (stuck in planning checkpoint)
        - 9: Auth pre-flight failure (not retryable, see issue #2508)
        - 10: Ghost session detected (instant exit, no work — see issue #2604)
        - 11: Degraded session detected (rate limits + Crystallizing loop, see issue #2631)
        - Other: Error
    """
    # Use unique session name per retry attempt to prevent tmux state
    # pollution (pipe-pane degradation) from rapid create/destroy cycles.
    # See issue #2639.
    actual_name = f"{name}-a{attempt}" if attempt > 0 else name

    scripts_dir = ctx.scripts_dir

    # Track wall-clock time for reliable ghost session detection.
    # File timestamps (st_mtime - st_ctime) are unreliable when pipe-pane
    # fails to capture output.  See issue #2639.
    phase_start = time.monotonic()

    # Guard against missing scripts directory.  This can happen when the
    # working tree is on a branch that predates the Loom installation (the
    # branch was created before .loom/scripts/ was added to the repo).
    # See issue #2147.
    spawn_script = scripts_dir / "agent-spawn.sh"
    if not spawn_script.is_file():
        log_warning(
            f"Script not found: {spawn_script} — "
            "the branch may predate Loom installation"
        )
        return 1

    # Build spawn command
    spawn_cmd = [
        str(spawn_script),
        "--role",
        role,
        "--name",
        actual_name,
        "--on-demand",
    ]

    if args:
        spawn_cmd.extend(["--args", args])

    if worktree:
        spawn_cmd.extend(["--worktree", str(worktree)])

    # Remove any stale sidecar exit code file from a previous run to avoid
    # reading outdated state after this session completes.  See issue #2737.
    exit_code_file = ctx.repo_root / ".loom" / "exit-codes" / f"{actual_name}.exit"
    try:
        exit_code_file.unlink(missing_ok=True)
    except OSError:
        pass

    # Spawn the worker
    # Redirect to DEVNULL to suppress output - agent logs are captured to
    # .loom/logs/<session>.log for debugging purposes
    #
    # Disable wrapper-level retries (LOOM_MAX_RETRIES=1) because
    # run_phase_with_retry() manages retries with better observability
    # (milestones, backoff).  Without this, the wrapper retries up to 5
    # times internally *and* the Python code retries up to 3 times on top,
    # causing up to 15 total CLI invocations instead of the intended 3.
    # See issue #2516.
    #
    # Pass LOOM_SHEPHERD_TASK_ID so subprocess claude-wrapper.sh can skip
    # the auth pre-flight check (see issue #2524).
    spawn_env = os.environ.copy()
    spawn_env["LOOM_MAX_RETRIES"] = "1"
    spawn_env["LOOM_SHEPHERD_TASK_ID"] = ctx.config.task_id
    spawn_result = subprocess.run(
        spawn_cmd,
        cwd=ctx.repo_root,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        env=spawn_env,
        check=False,
    )

    if spawn_result.returncode != 0:
        return 1

    # Build wait command
    wait_script = scripts_dir / "agent-wait-bg.sh"
    if not wait_script.is_file():
        log_warning(
            f"Script not found: {wait_script} — "
            "the branch may predate Loom installation"
        )
        return 1

    wait_cmd = [
        str(wait_script),
        actual_name,
        "--timeout",
        str(timeout),
        "--poll-interval",
        str(ctx.config.poll_interval),
        "--issue",
        str(ctx.config.issue),
    ]

    if phase:
        wait_cmd.extend(["--phase", phase])
        # Work-producing roles need longer idle thresholds
        if phase in ("builder", "doctor", "judge"):
            wait_cmd.extend(["--min-session-age", "120"])

    if worktree:
        wait_cmd.extend(["--worktree", str(worktree)])

    if pr_number:
        wait_cmd.extend(["--pr", str(pr_number)])

    wait_cmd.extend(["--task-id", ctx.config.task_id])

    # Use --max-idle for stuck termination (sets critical threshold + action=retry).
    # Default 600s (10 min) matches agent-wait-bg.sh default.  See issue #2406.
    wait_cmd.extend(["--max-idle", "600"])

    env = os.environ.copy()
    env.pop("CLAUDECODE", None)  # Prevent nested session guard from blocking subprocess

    # Launch wait process (non-blocking) so we can poll for heartbeats
    wait_proc = subprocess.Popen(
        wait_cmd,
        cwd=ctx.repo_root,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        env=env,
    )

    # Poll progress file for heartbeat updates while waiting
    progress_file = ctx.progress_dir / f"shepherd-{ctx.config.task_id}.json"
    seen_heartbeats = 0
    last_claim_extend = time.monotonic()
    agent_id = f"shepherd-{ctx.config.task_id}"

    # Planning stall detection state (issue #2443).
    # Track when we first observe the checkpoint at "planning" stage.
    # If it stays there beyond planning_timeout, terminate the worker.
    _planning_first_seen: float | None = None

    # Degraded session detection state (issue #2631).
    # Periodically scan the log for rate limit + Crystallizing patterns
    # and abort early rather than waiting for the full timeout.
    _last_degraded_scan: float = time.monotonic()

    while wait_proc.poll() is None:
        heartbeats = _read_heartbeats(progress_file, phase=phase)
        for hb in heartbeats[seen_heartbeats:]:
            action = hb.get("data", {}).get("action", "")
            if action:
                _print_heartbeat(action)
        seen_heartbeats = len(heartbeats)

        # Extend file-based claim periodically to prevent TTL expiry
        # during long worker phases.  See issue #2405.
        claim_elapsed = time.monotonic() - last_claim_extend
        if claim_elapsed >= _CLAIM_EXTEND_INTERVAL:
            extend_claim(ctx.repo_root, ctx.config.issue, agent_id)
            last_claim_extend = time.monotonic()

        # Check for planning stall
        if planning_timeout > 0 and worktree is not None:
            checkpoint = read_checkpoint(worktree)
            if checkpoint is not None and checkpoint.stage == "planning":
                if _planning_first_seen is None:
                    _planning_first_seen = time.monotonic()
                elif time.monotonic() - _planning_first_seen > planning_timeout:
                    elapsed = int(time.monotonic() - _planning_first_seen)
                    log_warning(
                        f"Planning stall detected: builder stuck in planning "
                        f"checkpoint for {elapsed}s (limit {planning_timeout}s), "
                        f"terminating"
                    )
                    wait_proc.terminate()
                    wait_proc.wait(timeout=30)
                    # Clean up the worker session before returning
                    destroy_script = scripts_dir / "agent-destroy.sh"
                    if destroy_script.is_file():
                        subprocess.run(
                            [str(destroy_script), actual_name, "--force"],
                            cwd=ctx.repo_root,
                            stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL,
                            check=False,
                        )
                    return 8  # Planning stall
            else:
                # Checkpoint advanced past planning (or doesn't exist yet)
                _planning_first_seen = None

        # In-flight degraded session detection (issue #2631).
        # Periodically scan the log for rate limit + Crystallizing patterns.
        # Abort early to avoid wasting the entire builder time budget on a
        # session that will never produce useful output.
        degraded_elapsed = time.monotonic() - _last_degraded_scan
        if degraded_elapsed >= _DEGRADED_SCAN_INTERVAL:
            _last_degraded_scan = time.monotonic()
            degraded_log_path = _session_log_path(ctx.repo_root, actual_name)
            if _scan_log_for_degradation(degraded_log_path):
                log_warning(
                    f"Degraded session detected: {role} session '{actual_name}' "
                    f"shows rate limit warnings and Crystallizing loop, "
                    f"terminating early (log: {degraded_log_path})"
                )
                wait_proc.terminate()
                wait_proc.wait(timeout=30)
                destroy_script = scripts_dir / "agent-destroy.sh"
                if destroy_script.is_file():
                    subprocess.run(
                        [str(destroy_script), actual_name, "--force"],
                        cwd=ctx.repo_root,
                        stdout=subprocess.DEVNULL,
                        stderr=subprocess.DEVNULL,
                        check=False,
                    )
                return 11  # Degraded session

        time.sleep(_HEARTBEAT_POLL_INTERVAL)

    # Check for any final heartbeats written before process exit
    heartbeats = _read_heartbeats(progress_file, phase=phase)
    for hb in heartbeats[seen_heartbeats:]:
        action = hb.get("data", {}).get("action", "")
        if action:
            _print_heartbeat(action)

    wait_exit = wait_proc.returncode

    # Check for sidecar exit code file written by claude-wrapper.sh.
    # When agent-wait returns 0 (shell idle, no claude process), the
    # wrapper's actual exit code is lost.  The sidecar file preserves it
    # so failure classifiers below can fire correctly.  See issue #2737.
    exit_code_file = ctx.repo_root / ".loom" / "exit-codes" / f"{actual_name}.exit"
    sidecar_exit: int | None = None
    try:
        if exit_code_file.exists():
            sidecar_exit = int(exit_code_file.read_text().strip())
            if sidecar_exit != 0 and wait_exit == 0:
                log_warning(
                    f"Sidecar exit code {sidecar_exit} overrides agent-wait "
                    f"exit 0 for {role} session '{actual_name}' "
                    f"(wrapper exited non-zero but agent-wait missed it)"
                )
                wait_exit = sidecar_exit
            exit_code_file.unlink(missing_ok=True)
    except (ValueError, OSError) as exc:
        log_warning(f"Failed to read sidecar exit file {exit_code_file}: {exc}")

    # Clean up the worker session
    destroy_script = scripts_dir / "agent-destroy.sh"
    if destroy_script.is_file():
        destroy_cmd = [str(destroy_script), actual_name, "--force"]
        subprocess.run(
            destroy_cmd,
            cwd=ctx.repo_root,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )

    # Wall-clock elapsed time for reliable ghost detection.
    # File timestamps are unreliable when pipe-pane fails.  See issue #2639.
    elapsed = time.monotonic() - phase_start

    # Detect failure modes from the session log file and return synthetic
    # exit codes so the retry layer can handle each appropriately.
    #
    # Check on ALL exit codes, not just 0.  A degraded CLI session may exit
    # with a non-zero code (e.g., 2 for API error) while still producing no
    # meaningful output — this is functionally a low-output session
    # and should be retried rather than treated as a builder error.
    # See issue #2446.
    #
    # Compute log path from the actual session name (which may include
    # an attempt suffix for retry isolation).  See issue #2639.
    log_path = _session_log_path(ctx.repo_root, actual_name)

    # Gather post-mortem diagnostics once for all zero-output failure paths.
    # This is cheap (single file read + pattern matching) and provides
    # structured diagnostic data for logging and failure classification.
    # Only gathered on non-zero exits to avoid overhead on success.
    # See issue #2766.
    postmortem: dict[str, Any] | None = None
    if wait_exit != 0:
        postmortem = gather_zero_output_postmortem(
            log_path,
            wall_clock_seconds=elapsed,
            wait_exit_code=wait_exit,
            sidecar_exit_code=sidecar_exit,
        )
        # Stash on context so callers (builder._gather_diagnostics,
        # cli._record_fallback_failure) can access it.  See issue #2766.
        ctx.last_postmortem = postmortem

    # Check for auth pre-flight failure first (exit code 9).  Auth failures
    # are NOT transient when a parent Claude session holds the config lock,
    # so retrying is futile.  See issue #2508.
    # Only reclassify non-zero exits — a successful process (exit 0) should
    # never be overridden by log pattern matching.  See issue #2540.
    if wait_exit != 0 and _is_auth_failure(log_path):
        assert postmortem is not None
        log_warning(
            f"Auth pre-flight failure for {role} session '{actual_name}': "
            f"authentication check failed (not retryable, log: {log_path})"
        )
        log_warning(f"Post-mortem: {postmortem['summary']}")
        return 9

    # Check for ghost session (exit code 10) — no meaningful work produced.
    # Content-first detection: sessions with only spinner/boilerplate output
    # and duration ≤ 30s are infrastructure failures retried on a separate
    # budget.  Check before MCP/low-output to avoid consuming their retry
    # budgets.  See issues #2604, #2644, #2659.
    if wait_exit != 0 and _is_ghost_session(log_path, wall_clock_duration=elapsed):
        assert postmortem is not None
        cause = _classify_ghost_cause(log_path)
        log_warning(
            f"Ghost session detected for {role} session '{actual_name}' "
            f"(cause: {cause}, {elapsed:.1f}s wall-clock, "
            f"exit code {wait_exit}, log: {log_path})"
        )
        log_warning(f"Post-mortem: {postmortem['summary']}")
        return 10

    # Check for degraded session (exit code 11) — rate limits + Crystallizing.
    # Degraded sessions produce output but it's non-productive garbage.
    # This is NOT retryable: the underlying resource constraint (rate limits)
    # will persist until the limit resets.  Check before MCP/low-output
    # because degraded sessions may have substantial output volume.
    # See issue #2631.
    if _is_degraded_session(log_path):
        if postmortem is not None:
            log_warning(f"Post-mortem: {postmortem['summary']}")
        log_warning(
            f"Degraded session detected for {role} session '{actual_name}': "
            f"rate limit warnings and Crystallizing loop "
            f"(exit code {wait_exit}, log: {log_path})"
        )
        return 11

    # Check for MCP failure (exit code 7) — more specific than low-output,
    # with different retry/backoff strategy.  See issues #2135, #2279.
    #
    # Unlike auth/ghost/low-output, MCP failure is checked regardless of exit
    # code.  The Claude CLI can exit 0 when the MCP server fails mid-session:
    # it starts, receives the prompt, can't use MCP tools, produces nothing,
    # and exits "cleanly."  The _is_mcp_failure() function already gates on
    # low output volume, so productive sessions aren't misclassified.
    # See issue #2767.
    if _is_mcp_failure(log_path):
        # Gather postmortem lazily for exit-0 MCP failures (postmortem is
        # only pre-gathered for non-zero exits above).  See issue #2766.
        if postmortem is None:
            postmortem = gather_zero_output_postmortem(
                log_path,
                wall_clock_seconds=elapsed,
                wait_exit_code=wait_exit,
                sidecar_exit_code=sidecar_exit,
            )
            ctx.last_postmortem = postmortem
        log_warning(
            f"MCP server failure detected for {role} session '{actual_name}' "
            f"(exit code {wait_exit}, log: {log_path})"
        )
        log_warning(f"Post-mortem: {postmortem['summary']}")
        return 7
    if wait_exit != 0 and _is_low_output_session(log_path):
        assert postmortem is not None
        log_warning(
            f"Low output detected for {role} session '{actual_name}' "
            f"(exit code {wait_exit}, log: {log_path})"
        )
        log_warning(f"Post-mortem: {postmortem['summary']}")
        return 6

    return wait_exit


def run_phase_with_retry(
    ctx: ShepherdContext,
    *,
    role: str,
    name: str,
    timeout: int,
    max_retries: int,
    phase: str | None = None,
    worktree: Path | None = None,
    pr_number: int | None = None,
    args: str | None = None,
    planning_timeout: int = 0,
) -> int:
    """Run a phase with automatic retry on stuck, low-output, or MCP failure.

    On exit code 4 (stuck), retries up to max_retries times.
    On exit code 6 (low output), classifies the root cause from the
    worker log and uses a cause-specific retry strategy from
    ``LOW_OUTPUT_RETRY_STRATEGIES``.  See issue #2518.
    On exit code 7 (MCP failure), retries up to MCP_FAILURE_MAX_RETRIES
    times with longer backoff (MCP failures are often systemic).
    On exit code 8 (planning stall), returns immediately (not retryable).
    On exit code 9 (auth failure), returns immediately (not retryable).
    On exit code 11 (degraded session), returns immediately (not retryable;
    rate limits won't resolve with retries).  See issue #2631.
    On exit code 10 (ghost session), classifies the root cause from the
    worker log and uses a cause-specific retry strategy from
    ``GHOST_RETRY_STRATEGIES``.  Retries on a **separate** budget that does
    not deplete the main retry counters.  Uses escalating backoff (default
    [10, 30, 60]s) instead of a fixed delay.  See issues #2604, #2644.

    For exit codes 6 and 7, if a single attempt takes longer than
    LOW_OUTPUT_MAX_ATTEMPT_SECONDS, retries are skipped entirely
    because the failure is likely an infrastructure issue rather than
    a transient blip.  See issue #2519.

    Returns:
        Exit code: 0=success, 3=shutdown, 4=stuck after retries,
                   6=low-output after retries, 7=MCP failure after retries,
                   8=planning stall, 9=auth failure,
                   10=ghost session after retries,
                   11=degraded session, other=error
    """
    stuck_retries = 0
    low_output_retries = 0
    mcp_failure_retries = 0
    ghost_retries = 0
    attempt = 0  # Total attempt counter for unique session names (issue #2639)

    while True:
        attempt_start = time.monotonic()
        exit_code = run_worker_phase(
            ctx,
            role=role,
            name=name,
            timeout=timeout,
            phase=phase,
            worktree=worktree,
            pr_number=pr_number,
            args=args,
            planning_timeout=planning_timeout,
            attempt=attempt,
        )
        # Compute the actual session name used for this attempt (must
        # match the logic in run_worker_phase for log file lookups).
        actual_name = f"{name}-a{attempt}" if attempt > 0 else name
        attempt += 1
        attempt_elapsed = time.monotonic() - attempt_start

        # --- Planning stall (exit code 8) ---
        # Not retryable: the builder was unable to progress past
        # the planning stage, indicating an issue with the task or
        # the agent's ability to proceed.  See issue #2443.
        if exit_code == 8:
            return 8

        # --- Auth pre-flight failure (exit code 9) ---
        # Not retryable: auth timeouts when a parent Claude session holds
        # the config lock will always fail.  Retrying wastes ~45s per
        # attempt with zero chance of success.  See issue #2508.
        if exit_code == 9:
            return 9

        # --- Degraded session (exit code 11) ---
        # Not retryable: the builder session was degraded by rate limits
        # and entered a Crystallizing loop.  The underlying rate limit
        # won't resolve with retries — the daemon should back off or
        # pick a different issue.  See issue #2631.
        if exit_code == 11:
            return 11

        # --- Pre-retry approval check (judge phase only) ---
        # If the judge already completed its work (applied loom:pr or
        # loom:changes-requested) before the MCP/low-output/ghost failure
        # occurred, skip the retry entirely.  See issue #2335.
        if exit_code in (6, 7, 10) and phase == "judge" and ctx.pr_number is not None:
            ctx.label_cache.invalidate_pr(ctx.pr_number)
            if ctx.has_pr_label("loom:pr") or ctx.has_pr_label(
                "loom:changes-requested"
            ):
                log_warning(
                    f"Judge already completed (PR #{ctx.pr_number} has outcome label), "
                    f"skipping retry despite exit code {exit_code}"
                )
                return 0

        # --- Ghost session handling (exit code 10) ---
        # Ghost sessions are infrastructure-level failures (0s duration, no
        # output) that should NOT deplete the main retry budget.  They use a
        # separate counter so that legitimate failures (stuck, low-output,
        # MCP) retain their full retry budget.  See issues #2604, #2644.
        if exit_code == 10:
            # Classify root cause and select cause-specific retry strategy.
            # Use the actual session name (may include attempt suffix) for
            # correct log file lookup.  See issues #2639, #2644.
            log_path = _session_log_path(ctx.repo_root, actual_name)
            ghost_cause = _classify_ghost_cause(log_path)
            cause_max_retries, cause_backoff = GHOST_RETRY_STRATEGIES.get(
                ghost_cause, GHOST_RETRY_STRATEGIES["unknown"]
            )

            # Fail fast for causes that won't resolve with retries.
            if cause_max_retries == 0:
                log_warning(
                    f"Ghost session for {role} classified as '{ghost_cause}': "
                    f"not retryable, failing fast"
                )
                return 10

            ghost_retries += 1
            if ghost_retries > cause_max_retries:
                errors = extract_log_errors(log_path)
                detail = f": {errors[-1]}" if errors else ""
                log_warning(
                    f"Ghost session ({ghost_cause}) persisted for {role} after "
                    f"{cause_max_retries} retries{detail}"
                )
                return 10

            # Use cause-specific escalating backoff schedule.
            backoff_idx = min(
                ghost_retries - 1, max(0, len(cause_backoff) - 1)
            )
            backoff = cause_backoff[backoff_idx]

            ctx.report_milestone(
                "error",
                error=f"ghost session detected for {role} (cause: {ghost_cause}, 0s duration, no output)",
                will_retry=True,
            )
            ctx.report_milestone(
                "heartbeat",
                action=(
                    f"retrying ghost session {role} "
                    f"(cause: {ghost_cause}, "
                    f"attempt {ghost_retries}/{cause_max_retries}, "
                    f"backoff {backoff}s)"
                ),
            )

            time.sleep(backoff)
            continue

        # --- MCP failure handling (exit code 7) ---
        # MCP failures are systemic (stale build, resource contention) so
        # use longer backoff than generic low-output sessions.  See issue #2279.
        if exit_code == 7:
            # Same elapsed-time guard as low-output.  See issue #2519.
            if attempt_elapsed > LOW_OUTPUT_MAX_ATTEMPT_SECONDS:
                log_warning(
                    f"MCP failure attempt for {role} took {attempt_elapsed:.0f}s "
                    f"(>{LOW_OUTPUT_MAX_ATTEMPT_SECONDS}s), "
                    f"likely infrastructure issue — not retrying"
                )
                return 7

            mcp_failure_retries += 1
            if mcp_failure_retries > MCP_FAILURE_MAX_RETRIES:
                log_path = _session_log_path(ctx.repo_root, actual_name)
                errors = extract_log_errors(log_path)
                cause = f": {errors[-1]}" if errors else ""
                log_warning(
                    f"MCP server failure persisted for {role} after "
                    f"{MCP_FAILURE_MAX_RETRIES} retries{cause}"
                )
                return 7  # Caller should treat as failure

            backoff_idx = min(
                mcp_failure_retries - 1, len(MCP_FAILURE_BACKOFF_SECONDS) - 1
            )
            backoff = MCP_FAILURE_BACKOFF_SECONDS[backoff_idx]

            ctx.report_milestone(
                "error",
                error=f"MCP server failure detected for {role}",
                will_retry=True,
            )
            ctx.report_milestone(
                "heartbeat",
                action=(
                    f"retrying MCP failure {role} "
                    f"(attempt {mcp_failure_retries}/{MCP_FAILURE_MAX_RETRIES}, "
                    f"backoff {backoff}s)"
                ),
            )

            time.sleep(backoff)
            continue

        # --- Low-output handling (exit code 6) ---
        if exit_code == 6:
            # Classify root cause from the worker log and select a
            # cause-specific retry strategy.  See issue #2518.
            log_path = _session_log_path(ctx.repo_root, actual_name)
            cause = _classify_low_output_cause(log_path)
            # Surface the classification on the context so callers
            # (e.g., _gather_diagnostics) can include it.  See issue #2562.
            ctx.last_low_output_cause = cause
            cause_max_retries, cause_backoff = LOW_OUTPUT_RETRY_STRATEGIES.get(
                cause, LOW_OUTPUT_RETRY_STRATEGIES["unknown"]
            )

            # Fail fast for causes that won't resolve with retries
            # (e.g., auth_timeout — 0 retries).
            if cause_max_retries == 0:
                log_warning(
                    f"Low output for {role} classified as '{cause}': "
                    f"not retryable, failing fast"
                )
                return 6

            # If the attempt took a long time, the failure is likely an
            # infrastructure issue — retrying won't help.  See issue #2519.
            if attempt_elapsed > LOW_OUTPUT_MAX_ATTEMPT_SECONDS:
                log_warning(
                    f"Low-output attempt for {role} took {attempt_elapsed:.0f}s "
                    f"(>{LOW_OUTPUT_MAX_ATTEMPT_SECONDS}s), "
                    f"likely infrastructure issue — not retrying "
                    f"(cause: {cause})"
                )
                return 6

            low_output_retries += 1
            if low_output_retries > cause_max_retries:
                errors = extract_log_errors(log_path)
                detail = f": {errors[-1]}" if errors else ""
                log_warning(
                    f"Low output ({cause}) persisted for {role} after "
                    f"{cause_max_retries} retries{detail}"
                )
                return 6  # Caller should treat as failure

            # Use cause-specific backoff schedule
            backoff_idx = min(
                low_output_retries - 1, max(0, len(cause_backoff) - 1)
            )
            backoff = cause_backoff[backoff_idx]

            ctx.report_milestone(
                "error",
                error=f"low output detected for {role} (cause: {cause})",
                will_retry=True,
            )
            ctx.report_milestone(
                "heartbeat",
                action=(
                    f"retrying low-output {role} "
                    f"(cause: {cause}, "
                    f"attempt {low_output_retries}/{cause_max_retries}, "
                    f"backoff {backoff}s)"
                ),
            )

            time.sleep(backoff)
            continue

        # --- Stuck handling (exit code 4) ---
        if exit_code != 4:
            return exit_code

        stuck_retries += 1
        if stuck_retries > max_retries:
            return 4  # Still stuck after max retries

        # Report retry milestone
        ctx.report_milestone(
            "heartbeat",
            action=f"retrying stuck {role} (attempt {stuck_retries})",
        )

        # Allow cleanup (tmux session teardown, MCP port release) to
        # complete before spawning the retry.  Without this delay, the
        # new wrapper's pre-flight checks race against the previous
        # session's resource cleanup and can hang or fail silently.
        # See issue #2472.
        time.sleep(5)
