"""Operator-managed allowlist for the OAuth token pool.

The ``.allowlist`` file at ``.loom/tokens/.allowlist`` constrains which
bootstrapped accounts the spawn-time selector (#3235) is allowed to
choose. When the file is absent, all ``.token`` files are eligible.

File format: one account name (token file stem, no extension) per line.
``#`` introduces a line comment. Empty lines are ignored.

Validation: account names must match an existing ``<name>.token`` file
under ``.loom/tokens/`` **exactly** — no fuzzy/substring matching.
This is intentional: operators usually pin a specific account, and
substring matches have caused selection of the wrong account in
practice.

Concurrency: writes use the same ``mkdir``-based lock primitive as
``bad_tokens.py``. Reads are unsynchronised — readers see a consistent
file because writers atomically rename a temp file into place.

Public API:
    list_accounts(workspace) -> list[str]
    read_allowlist(workspace) -> list[str]
    write_allowlist(workspace, names) -> None
    add_to_allowlist(workspace, names) -> tuple[list[str], list[str]]
    remove_from_allowlist(workspace, names) -> tuple[list[str], list[str]]
    clear_allowlist(workspace) -> bool
    AllowlistError, UnknownAccountError
"""

from __future__ import annotations

import os
import tempfile
from pathlib import Path

# Reuse the lock primitive from bad_tokens — same constraints (mkdir-only,
# no flock on stock macOS) and same workspace.
from loom_tools.tokens.bad_tokens import _MkdirLock


class AllowlistError(Exception):
    """Base class for allowlist failures."""


class UnknownAccountError(AllowlistError):
    """A name does not correspond to an existing ``.token`` file."""

    def __init__(self, name: str, available: list[str]):
        super().__init__(
            f"Unknown account '{name}'. "
            f"Available: {', '.join(available) if available else '(none)'}",
        )
        self.name = name
        self.available = available


def _tokens_dir(workspace: Path | str) -> Path:
    return Path(workspace) / ".loom" / "tokens"


def _allowlist_path(workspace: Path | str) -> Path:
    return _tokens_dir(workspace) / ".allowlist"


def _lock_path(workspace: Path | str) -> Path:
    return _tokens_dir(workspace) / ".allowlist.lock"


def list_accounts(workspace: Path | str) -> list[str]:
    """Return all bootstrapped account names (token file stems), sorted.

    Args:
        workspace: Repo root containing ``.loom/tokens/``.

    Returns:
        Sorted list of stems (no ``.token`` extension). Empty if the
        tokens directory is missing or holds no ``.token`` files.
    """
    d = _tokens_dir(workspace)
    if not d.is_dir():
        return []
    return sorted(p.stem for p in d.glob("*.token"))


def _strip_comment(line: str) -> str:
    return line.split("#", 1)[0].strip()


def read_allowlist(workspace: Path | str) -> list[str]:
    """Return the current allowlist, in file order.

    Comments and blank lines are filtered out. Duplicates are preserved
    in order of first appearance.

    Args:
        workspace: Repo root containing ``.loom/tokens/``.

    Returns:
        List of account names. Empty if the file is missing or all
        lines are comments / blank.
    """
    path = _allowlist_path(workspace)
    if not path.is_file():
        return []
    seen: set[str] = set()
    out: list[str] = []
    try:
        for raw in path.read_text(encoding="utf-8").splitlines():
            name = _strip_comment(raw)
            if name and name not in seen:
                out.append(name)
                seen.add(name)
    except OSError:
        return []
    return out


def _validate_names(workspace: Path | str, names: list[str]) -> list[str]:
    """Resolve names against bootstrapped accounts using EXACT match.

    Returns the validated list (deduplicated, original order). Raises
    UnknownAccountError on the first unknown name.
    """
    available = list_accounts(workspace)
    available_set = set(available)
    seen: set[str] = set()
    resolved: list[str] = []
    for raw in names:
        name = raw.strip()
        if not name:
            continue
        if name not in available_set:
            raise UnknownAccountError(name, available)
        if name not in seen:
            resolved.append(name)
            seen.add(name)
    return resolved


def _atomic_write(path: Path, content: str) -> None:
    """Write ``content`` to ``path`` atomically via mktemp + os.replace.

    The temp file lives in the same directory as the target so the
    rename is on the same filesystem.
    """
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp_str = tempfile.mkstemp(
        prefix=path.name + ".",
        suffix=".tmp",
        dir=str(path.parent),
    )
    tmp = Path(tmp_str)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as fh:
            fh.write(content)
        os.replace(tmp, path)
    except Exception:
        try:
            tmp.unlink(missing_ok=True)
        except OSError:
            pass
        raise


def write_allowlist(
    workspace: Path | str,
    names: list[str],
) -> list[str]:
    """Replace the allowlist with exactly ``names`` (validated).

    Args:
        workspace: Repo root containing ``.loom/tokens/``.
        names: Account names to set as the allowlist. Must match
            existing ``.token`` files exactly.

    Returns:
        The validated, deduplicated list that was written.

    Raises:
        UnknownAccountError: If any name does not match a bootstrapped
            account.
        FileNotFoundError: If ``.loom/tokens/`` does not exist.
    """
    d = _tokens_dir(workspace)
    if not d.is_dir():
        raise FileNotFoundError(
            f"Tokens dir does not exist: {d}. "
            f"Run `loom-tokens bootstrap` first.",
        )
    resolved = _validate_names(workspace, names)
    body = "\n".join(resolved) + ("\n" if resolved else "")
    path = _allowlist_path(workspace)
    with _MkdirLock(_lock_path(workspace)):
        if resolved:
            _atomic_write(path, body)
        else:
            # Empty allowlist == no constraint. Remove the file rather
            # than leaving an empty one (semantics match
            # ``clear_allowlist``).
            try:
                path.unlink()
            except FileNotFoundError:
                pass
    return resolved


def add_to_allowlist(
    workspace: Path | str,
    names: list[str],
) -> tuple[list[str], list[str]]:
    """Add ``names`` to the existing allowlist.

    Args:
        workspace: Repo root containing ``.loom/tokens/``.
        names: Names to add. Must match existing accounts.

    Returns:
        ``(added, skipped_existing)`` — the names that were newly added
        and the names that were already present.

    Raises:
        UnknownAccountError: If any name is unknown.
        FileNotFoundError: If ``.loom/tokens/`` does not exist.
    """
    d = _tokens_dir(workspace)
    if not d.is_dir():
        raise FileNotFoundError(
            f"Tokens dir does not exist: {d}. "
            f"Run `loom-tokens bootstrap` first.",
        )
    resolved = _validate_names(workspace, names)
    with _MkdirLock(_lock_path(workspace)):
        existing = read_allowlist(workspace)
        existing_set = set(existing)
        added: list[str] = []
        skipped: list[str] = []
        for n in resolved:
            if n in existing_set:
                skipped.append(n)
            else:
                existing.append(n)
                existing_set.add(n)
                added.append(n)
        if added:
            _atomic_write(
                _allowlist_path(workspace),
                "\n".join(existing) + "\n",
            )
    return added, skipped


def remove_from_allowlist(
    workspace: Path | str,
    names: list[str],
) -> tuple[list[str], list[str]]:
    """Remove ``names`` from the allowlist.

    Removing the last account drops the file entirely (no constraint).

    Args:
        workspace: Repo root containing ``.loom/tokens/``.
        names: Names to remove. Must match existing accounts.

    Returns:
        ``(removed, skipped_missing)`` — the names that were dropped
        and the names that weren't in the allowlist to begin with.

    Raises:
        UnknownAccountError: If any name is not a bootstrapped account.
        FileNotFoundError: If ``.loom/tokens/`` does not exist.
    """
    d = _tokens_dir(workspace)
    if not d.is_dir():
        raise FileNotFoundError(
            f"Tokens dir does not exist: {d}. "
            f"Run `loom-tokens bootstrap` first.",
        )
    resolved = _validate_names(workspace, names)
    to_remove = set(resolved)
    with _MkdirLock(_lock_path(workspace)):
        existing = read_allowlist(workspace)
        removed: list[str] = []
        skipped: list[str] = []
        kept: list[str] = []
        for n in existing:
            if n in to_remove:
                removed.append(n)
            else:
                kept.append(n)
        # Names that were validated but never in the file
        present_set = set(existing)
        for n in resolved:
            if n not in present_set:
                skipped.append(n)
        path = _allowlist_path(workspace)
        if kept:
            _atomic_write(path, "\n".join(kept) + "\n")
        else:
            try:
                path.unlink()
            except FileNotFoundError:
                pass
    return removed, skipped


def clear_allowlist(workspace: Path | str) -> bool:
    """Remove the allowlist file entirely.

    Args:
        workspace: Repo root containing ``.loom/tokens/``.

    Returns:
        ``True`` if a file was removed, ``False`` if no allowlist was
        active.
    """
    path = _allowlist_path(workspace)
    with _MkdirLock(_lock_path(workspace)):
        try:
            path.unlink()
            return True
        except FileNotFoundError:
            return False
