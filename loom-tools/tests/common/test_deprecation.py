"""Tests for ``loom_tools.common.deprecation`` (issue #3376, epic #3372).

The deprecation helper is intentionally tiny — these tests pin the contract
so Phase 3 (deletion) cannot land without first updating the call sites:

1. Calling ``warn_deprecated`` prints a multi-line block to stderr.
2. The block contains the component name, the replacement description, and
   the reference (defaulting to ``#3372``).
3. ``LOOM_SUPPRESS_DEPRECATION=1`` silences the output entirely.
4. Any other value (or unset) does NOT silence it.
5. The function returns ``None`` and never raises.
"""

from __future__ import annotations

import io
import sys

import pytest

from loom_tools.common.deprecation import warn_deprecated


# ---------------------------------------------------------------------------
# Basic emission
# ---------------------------------------------------------------------------


def test_warn_deprecated_writes_to_stderr(
    capsys: pytest.CaptureFixture[str],
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Output goes to stderr, not stdout."""
    monkeypatch.delenv("LOOM_SUPPRESS_DEPRECATION", raising=False)

    warn_deprecated("loom-daemon", "spawn-loop.sh + GH Actions")

    captured = capsys.readouterr()
    assert captured.out == ""
    assert "DEPRECATED" in captured.err
    assert "loom-daemon" in captured.err


def test_warn_deprecated_contains_component_replacement_and_ref(
    capsys: pytest.CaptureFixture[str],
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """All three required pieces of information appear in the warning."""
    monkeypatch.delenv("LOOM_SUPPRESS_DEPRECATION", raising=False)

    warn_deprecated(
        "/shepherd skill",
        replacement="/loom:sweep <issue>",
        ref="#3372",
    )

    err = capsys.readouterr().err
    assert "/shepherd skill" in err
    assert "/loom:sweep <issue>" in err
    assert "#3372" in err


def test_warn_deprecated_default_ref_is_epic(
    capsys: pytest.CaptureFixture[str],
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """When ``ref`` is omitted, the default points at the umbrella epic."""
    monkeypatch.delenv("LOOM_SUPPRESS_DEPRECATION", raising=False)

    warn_deprecated("some-component", "some-replacement")

    assert "#3372" in capsys.readouterr().err


def test_warn_deprecated_mentions_suppression_envvar(
    capsys: pytest.CaptureFixture[str],
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Output documents the suppression env var so operators can find it.

    Without this hint, the only way to discover ``LOOM_SUPPRESS_DEPRECATION``
    is to grep the codebase — which downstream installers can't realistically
    do.  Document it inline.
    """
    monkeypatch.delenv("LOOM_SUPPRESS_DEPRECATION", raising=False)

    warn_deprecated("X", "Y")

    assert "LOOM_SUPPRESS_DEPRECATION" in capsys.readouterr().err


# ---------------------------------------------------------------------------
# Suppression semantics
# ---------------------------------------------------------------------------


def test_warn_deprecated_suppressed_when_env_set(
    capsys: pytest.CaptureFixture[str],
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """``LOOM_SUPPRESS_DEPRECATION=1`` silences the warning."""
    monkeypatch.setenv("LOOM_SUPPRESS_DEPRECATION", "1")

    warn_deprecated("loom-daemon", "spawn-loop")

    captured = capsys.readouterr()
    assert captured.err == ""
    assert captured.out == ""


def test_warn_deprecated_not_suppressed_by_other_values(
    capsys: pytest.CaptureFixture[str],
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Only the exact value ``"1"`` suppresses; other truthy strings do not.

    This matches the bash helper's behaviour (``[[ "$VAR" == "1" ]]``) so
    Python and shell call sites share a single mental model.
    """
    for value in ("", "0", "true", "yes", "on"):
        monkeypatch.setenv("LOOM_SUPPRESS_DEPRECATION", value)
        warn_deprecated("component", "replacement")
        err = capsys.readouterr().err
        assert "DEPRECATED" in err, (
            f"LOOM_SUPPRESS_DEPRECATION={value!r} should NOT suppress the warning"
        )


def test_warn_deprecated_returns_none(monkeypatch: pytest.MonkeyPatch) -> None:
    """Return value is always ``None`` — both emitting and suppressed paths."""
    monkeypatch.delenv("LOOM_SUPPRESS_DEPRECATION", raising=False)
    assert warn_deprecated("X", "Y") is None

    monkeypatch.setenv("LOOM_SUPPRESS_DEPRECATION", "1")
    assert warn_deprecated("X", "Y") is None


# ---------------------------------------------------------------------------
# Flush behaviour (regression guard)
# ---------------------------------------------------------------------------


def test_warn_deprecated_flushes_immediately(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Output reaches stderr immediately even when stderr is block-buffered.

    Entry-point CLIs may invoke ``warn_deprecated`` and then immediately
    redirect stderr (e.g. shepherd's ``sys.stderr = sys.stdout``); without
    ``flush=True`` the warning could be lost.
    """
    monkeypatch.delenv("LOOM_SUPPRESS_DEPRECATION", raising=False)

    buf = io.StringIO()
    monkeypatch.setattr(sys, "stderr", buf)

    warn_deprecated("component", "replacement")

    # The warning should already be in the buffer — no manual flush needed.
    assert "DEPRECATED" in buf.getvalue()
