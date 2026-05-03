"""Account health probe + ranking for the agent token pool.

Probes each bootstrapped OAuth account by sending a minimal Anthropic
``POST /v1/messages`` request and parses rate-limit headers to derive
session (5h) and weekly (7d) utilization plus the next 7d reset time.

Resilient to header renames: matches by **suffix** (e.g.
``-5h-utilization``, ``-7d-utilization``, ``-7d-reset``) so that any
prefix change in the ``anthropic-ratelimit-*`` family still maps to
our internal fields. The full header set is logged on the first run
of the process for visibility.

OAuth header format: tokens shaped ``sk-ant-oat01-*`` (Claude Code
OAuth) require ``Authorization: Bearer <token>`` plus the
``anthropic-beta: oauth-2025-04-20`` header. Plain ``sk-ant-api*`` API
keys use ``x-api-key: <token>`` instead.  We auto-detect by token
prefix (verified empirically: lean-genius's ``check-accounts.sh``
sends ``x-api-key`` and that fails on ``oat01`` tokens with 401, but
succeeds on plain API keys).

Status assignment:

* ``available`` — utilizations < 95 percent
* ``exhausted`` — 7d_utilization >= 0.95
* ``rate_limited`` — current 429 (transient, distinct from exhausted)
* ``blocked`` — 401 auth failure or token listed in ``.bad_tokens``

The CLI command writes ``.loom/tokens/.ranking`` atomically when
``--ranking`` is passed (write-then-rename in the same directory so no
partial file is ever visible mid-write).
"""

from __future__ import annotations

import json
import logging
import random
import time
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterable

import requests

logger = logging.getLogger(__name__)

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

ANTHROPIC_MESSAGES_URL = "https://api.anthropic.com/v1/messages"
ANTHROPIC_VERSION = "2023-06-01"
ANTHROPIC_OAUTH_BETA = "oauth-2025-04-20"
USER_AGENT = "loom-tokens/0.1 (claude-code-compatible)"
DEFAULT_PROBE_MODEL = "claude-haiku-4-5-20251001"
DEFAULT_PROBE_PROMPT = "hi"
DEFAULT_TIMEOUT_SECONDS = 15
EXHAUSTED_THRESHOLD = 0.95

# Suffix patterns matched case-insensitively against header names. Keeping
# these as suffixes (not full names) makes the parser resilient to any
# rename of the ``anthropic-ratelimit-tokens-*`` prefix segment (for
# example, a future ``anthropic-ratelimit-input-tokens-7d-utilization``
# would still match ``-7d-utilization``).
HEADER_SUFFIX_5H_UTIL = "-5h-utilization"
HEADER_SUFFIX_7D_UTIL = "-7d-utilization"
HEADER_SUFFIX_7D_RESET = "-7d-reset"
HEADER_SUFFIX_5H_STATUS = "-5h-status"

# Module-level "have we logged the full header set yet" flag. We log on the
# first SUCCESSFUL probe so users can see which headers Anthropic is
# actually sending without stamping the screen with stack traces from
# error responses.
_FIRST_RUN_HEADERS_LOGGED = False


# ---------------------------------------------------------------------------
# Data structures
# ---------------------------------------------------------------------------


@dataclass
class AccountResult:
    """Per-account probe result."""

    name: str
    status: str  # available | exhausted | rate_limited | blocked | error | skipped
    s5h_utilization: float | None = None
    s7d_utilization: float | None = None
    s7d_reset: str | None = None
    error: str | None = None

    def to_dict(self) -> dict[str, Any]:
        out: dict[str, Any] = {
            "name": self.name,
            "status": self.status,
            "5h_utilization": self.s5h_utilization,
            "7d_utilization": self.s7d_utilization,
            "7d_reset": self.s7d_reset,
        }
        if self.error:
            out["error"] = self.error
        return out


@dataclass
class ProbeReport:
    """Aggregate probe report across all accounts."""

    ranked_at: str
    accounts: list[AccountResult] = field(default_factory=list)

    def to_dict(self) -> dict[str, Any]:
        return {
            "ranked_at": self.ranked_at,
            "accounts": [a.to_dict() for a in self.accounts],
        }


# ---------------------------------------------------------------------------
# Token discovery (ties into #3234's bootstrap output)
# ---------------------------------------------------------------------------


def discover_tokens(tokens_dir: Path) -> list[tuple[str, str]]:
    """Return ``[(account_name, token), ...]`` for bootstrapped accounts.

    Reads every ``*.token`` file in *tokens_dir* and skips entries listed
    in ``.bad_tokens`` (one account name per line, ``#`` comments allowed).
    Skipped accounts are returned with an empty token string so callers
    can still emit them as ``status: blocked`` in the ranking — that way
    #3235's selector knows not to attempt them.
    """
    if not tokens_dir.is_dir():
        return []

    bad: set[str] = set()
    bad_file = tokens_dir / ".bad_tokens"
    if bad_file.is_file():
        for raw in bad_file.read_text().splitlines():
            line = raw.strip()
            if not line or line.startswith("#"):
                continue
            bad.add(line)

    tokens: list[tuple[str, str]] = []
    for path in sorted(tokens_dir.glob("*.token")):
        name = path.stem
        if name in bad:
            tokens.append((name, ""))  # signal: known-bad, do not probe
            continue
        try:
            token = path.read_text().strip()
        except OSError:
            continue
        if token:
            tokens.append((name, token))
    return tokens


# ---------------------------------------------------------------------------
# Header parsing
# ---------------------------------------------------------------------------


def _find_header_by_suffix(
    headers: dict[str, str] | Any, suffix: str
) -> str | None:
    """Case-insensitive header lookup by suffix.

    Iterates header names rather than indexing because we want pattern
    matching on the name, not exact match. Works with both ``dict`` and
    ``requests.structures.CaseInsensitiveDict`` (both expose ``.items()``).
    """
    suffix_lower = suffix.lower()
    for name, value in headers.items():
        if name.lower().endswith(suffix_lower):
            return value
    return None


def _parse_float(raw: str | None) -> float | None:
    if raw is None:
        return None
    try:
        return float(raw.strip())
    except (ValueError, AttributeError):
        return None


def _epoch_to_iso(raw: str | None) -> str | None:
    """Convert an integer-seconds reset timestamp to ISO-8601 UTC.

    The 7d-reset header is delivered as a unix timestamp (seconds since
    epoch). Some providers serialise it as a float; normalise both. If
    parsing fails we return the raw string so downstream code can still
    inspect it.
    """
    if raw is None:
        return None
    raw = raw.strip()
    if not raw:
        return None
    try:
        ts = float(raw)
    except ValueError:
        return raw  # already ISO-8601 or unparseable — pass through
    try:
        return datetime.fromtimestamp(ts, tz=timezone.utc).strftime(
            "%Y-%m-%dT%H:%M:%SZ"
        )
    except (OverflowError, OSError, ValueError):
        return raw


def parse_rate_limit_headers(headers: dict[str, str] | Any) -> dict[str, Any]:
    """Extract rate-limit fields from a response headers mapping.

    Returns a dict with ``5h_utilization``, ``7d_utilization``,
    ``7d_reset`` (ISO-8601), and ``5h_status``. Missing fields are
    ``None``.
    """
    return {
        "5h_utilization": _parse_float(
            _find_header_by_suffix(headers, HEADER_SUFFIX_5H_UTIL)
        ),
        "7d_utilization": _parse_float(
            _find_header_by_suffix(headers, HEADER_SUFFIX_7D_UTIL)
        ),
        "7d_reset": _epoch_to_iso(
            _find_header_by_suffix(headers, HEADER_SUFFIX_7D_RESET)
        ),
        "5h_status": _find_header_by_suffix(headers, HEADER_SUFFIX_5H_STATUS),
    }


def _maybe_log_first_run_headers(headers: Any) -> None:
    """Log the full header set on the first successful probe.

    Idempotent across multiple calls in the same process. We log to
    stderr (via ``logger.info``) so JSON-mode stdout output is not
    polluted.
    """
    global _FIRST_RUN_HEADERS_LOGGED
    if _FIRST_RUN_HEADERS_LOGGED:
        return
    _FIRST_RUN_HEADERS_LOGGED = True
    try:
        names = sorted(name for name, _ in headers.items())
    except Exception:  # pragma: no cover — defensive
        return
    rl_names = [n for n in names if "ratelimit" in n.lower()]
    logger.info("Anthropic API response header set (first probe of run):")
    for n in names:
        logger.info("  %s", n)
    if rl_names:
        logger.info("Rate-limit headers detected: %d", len(rl_names))


# ---------------------------------------------------------------------------
# HTTP probe
# ---------------------------------------------------------------------------


def _build_headers(token: str) -> dict[str, str]:
    """Return the right header set for *token*.

    OAuth tokens (``sk-ant-oat01-*``) use ``Authorization: Bearer`` plus
    ``anthropic-beta``. API keys (``sk-ant-api*``) use ``x-api-key``.
    The token-prefix sniff matches Anthropic's own convention; if it's
    wrong for a given account, the result is a 401 and the account
    ends up as ``blocked`` (which is the right behaviour: the token
    can't be used).
    """
    base: dict[str, str] = {
        "anthropic-version": ANTHROPIC_VERSION,
        "content-type": "application/json",
        "user-agent": USER_AGENT,
    }
    if token.startswith("sk-ant-oat"):
        base["authorization"] = f"Bearer {token}"
        base["anthropic-beta"] = ANTHROPIC_OAUTH_BETA
    else:
        # Plain API key (or unknown shape — fall through to x-api-key,
        # which is the historical default).
        base["x-api-key"] = token
    return base


def probe_account(
    name: str,
    token: str,
    *,
    model: str = DEFAULT_PROBE_MODEL,
    probe_prompt: str = DEFAULT_PROBE_PROMPT,
    timeout: float = DEFAULT_TIMEOUT_SECONDS,
    session: requests.Session | None = None,
) -> AccountResult:
    """Probe a single account, returning an AccountResult.

    Probe failures (network errors, 5xx, timeout) are mapped to
    ``status="error"`` with a description in ``error``; they DO NOT
    raise. 401 -> blocked, 429 -> rate_limited.
    """
    if not token:
        # known-bad token (in .bad_tokens) — surface for selector visibility
        return AccountResult(name=name, status="blocked", error="bad_token_listed")

    body = {
        "model": model,
        "max_tokens": 1,
        "messages": [{"role": "user", "content": probe_prompt}],
    }
    headers = _build_headers(token)

    sess = session or requests
    try:
        resp = sess.post(
            ANTHROPIC_MESSAGES_URL,
            headers=headers,
            json=body,
            timeout=timeout,
        )
    except requests.Timeout:
        logger.warning("probe %s: timeout after %ss", name, timeout)
        return AccountResult(name=name, status="error", error="timeout")
    except requests.ConnectionError as exc:
        logger.warning("probe %s: connection error: %s", name, exc)
        return AccountResult(name=name, status="error", error=f"connection: {exc}")
    except requests.RequestException as exc:
        logger.warning("probe %s: request exception: %s", name, exc)
        return AccountResult(name=name, status="error", error=str(exc))

    code = resp.status_code

    if code == 401:
        return AccountResult(name=name, status="blocked", error="auth_401")

    if code == 429:
        # Even a 429 may include rate-limit headers; capture them.
        parsed = parse_rate_limit_headers(resp.headers)
        return AccountResult(
            name=name,
            status="rate_limited",
            s5h_utilization=parsed["5h_utilization"],
            s7d_utilization=parsed["7d_utilization"],
            s7d_reset=parsed["7d_reset"],
        )

    if code >= 500:
        logger.warning("probe %s: upstream %d", name, code)
        return AccountResult(
            name=name, status="error", error=f"http_{code}"
        )

    if code >= 400:
        # 4xx other than 401/429 — probe payload was bad. Treat as error
        # so the selector can still try the account, but log loudly.
        logger.warning("probe %s: client error %d: %s", name, code, resp.text[:200])
        return AccountResult(
            name=name, status="error", error=f"http_{code}"
        )

    # 2xx — successful probe. Log header set on first run.
    _maybe_log_first_run_headers(resp.headers)

    parsed = parse_rate_limit_headers(resp.headers)
    s7d_util = parsed["7d_utilization"]

    if s7d_util is not None and s7d_util >= EXHAUSTED_THRESHOLD:
        status = "exhausted"
    else:
        status = "available"

    return AccountResult(
        name=name,
        status=status,
        s5h_utilization=parsed["5h_utilization"],
        s7d_utilization=parsed["7d_utilization"],
        s7d_reset=parsed["7d_reset"],
    )


# ---------------------------------------------------------------------------
# Ranking + atomic write
# ---------------------------------------------------------------------------


_STATUS_RANK = {
    "available": 0,
    "rate_limited": 1,
    "exhausted": 2,
    "blocked": 3,
    "error": 4,
    "skipped": 5,
}


def _sort_key(account: AccountResult) -> tuple[int, str]:
    """Ranking sort: available first, then by 7d_reset ascending.

    Accounts without a reset timestamp sort last within their status
    bucket. ``error``/``skipped`` accounts stay at the bottom so they
    are tried last, not first.
    """
    rank = _STATUS_RANK.get(account.status, 99)
    reset = account.s7d_reset or "9999-12-31T23:59:59Z"
    return (rank, reset)


def build_report(results: Iterable[AccountResult]) -> ProbeReport:
    """Build a sorted ProbeReport from probe results."""
    ranked_at = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    sorted_results = sorted(results, key=_sort_key)
    return ProbeReport(ranked_at=ranked_at, accounts=sorted_results)


def write_ranking_atomic(report: ProbeReport, ranking_path: Path) -> None:
    """Write *report* to *ranking_path* atomically.

    Writes to ``<path>.tmp`` in the same directory (so the rename is on
    the same filesystem and uses ``rename(2)``), then ``Path.replace``s
    onto the target. This guarantees readers see either the old file or
    the new file, never a partial.
    """
    ranking_path.parent.mkdir(parents=True, exist_ok=True)
    tmp = ranking_path.with_suffix(ranking_path.suffix + ".tmp")
    tmp.write_text(json.dumps(report.to_dict(), indent=2) + "\n")
    tmp.replace(ranking_path)


# ---------------------------------------------------------------------------
# Run + report formatting
# ---------------------------------------------------------------------------


def run_check(
    tokens_dir: Path,
    *,
    write_ranking: bool = False,
    probe_prompt: str = DEFAULT_PROBE_PROMPT,
    model: str = DEFAULT_PROBE_MODEL,
    stagger: bool = True,
    session: requests.Session | None = None,
) -> ProbeReport:
    """Probe all accounts and (optionally) write ``.ranking``.

    Probes are issued sequentially with 0.5-1.5s jitter between them
    (lean-genius pattern) when *stagger* is true. Tests can pass
    ``stagger=False`` to skip the sleep.
    """
    pairs = discover_tokens(tokens_dir)
    if not pairs:
        logger.warning("no tokens found in %s", tokens_dir)

    results: list[AccountResult] = []
    for i, (name, token) in enumerate(pairs):
        if i > 0 and stagger and token:
            time.sleep(0.5 + random.random())
        result = probe_account(
            name,
            token,
            probe_prompt=probe_prompt,
            model=model,
            session=session,
        )
        results.append(result)

    report = build_report(results)

    if write_ranking:
        ranking_path = tokens_dir / ".ranking"
        write_ranking_atomic(report, ranking_path)
        logger.info(
            "wrote ranking to %s (%d accounts)",
            ranking_path,
            len(report.accounts),
        )

    return report


def format_table(report: ProbeReport) -> str:
    """Human-readable status table, sorted with best accounts first."""
    lines: list[str] = []
    lines.append(f"Token pool ranking (probed at {report.ranked_at})")
    lines.append("=" * 78)
    lines.append(
        f"{'Account':<28} {'5h util':>9} {'7d util':>9} {'Status':<13} {'7d resets':<22}"
    )
    lines.append("-" * 78)
    for a in report.accounts:
        s5 = f"{a.s5h_utilization:.2f}" if a.s5h_utilization is not None else "-"
        s7 = f"{a.s7d_utilization:.2f}" if a.s7d_utilization is not None else "-"
        reset = a.s7d_reset or "-"
        lines.append(
            f"{a.name:<28} {s5:>9} {s7:>9} {a.status:<13} {reset:<22}"
        )

    counts: dict[str, int] = {}
    for a in report.accounts:
        counts[a.status] = counts.get(a.status, 0) + 1
    summary = ", ".join(f"{n} {s}" for s, n in sorted(counts.items()))
    lines.append("")
    lines.append(f"Total {len(report.accounts)}: {summary}")
    return "\n".join(lines)


# ---------------------------------------------------------------------------
# CLI dispatch lives in ``loom_tools.tokens.cli`` (alongside the bootstrap
# subcommand from #3234). This module exposes the building blocks
# (``run_check``, ``format_table``, ``DEFAULT_PROBE_PROMPT``) that the
# dispatcher composes.
# ---------------------------------------------------------------------------
