"""Centralized environment variable parsing utilities.

Provides consistent, typed access to environment variables with
sensible defaults and error handling.

Examples::

    from loom_tools.common.config import env_bool, env_int, env_str, env_list

    # Boolean with common truthy/falsy values
    enabled = env_bool("MY_FEATURE_ENABLED", default=True)

    # Integer with default on invalid
    timeout = env_int("MY_TIMEOUT", default=300)

    # Float with default on invalid
    rate = env_float("MY_RATE_LIMIT", default=1.5)

    # String with default
    name = env_str("MY_NAME", default="default")

    # List from comma-separated values
    tags = env_list("MY_TAGS", sep=",", default=["a", "b"])
"""

from __future__ import annotations

import os


def env_str(name: str, default: str = "") -> str:
    """Get string environment variable.

    Args:
        name: Environment variable name
        default: Default value if not set

    Returns:
        The environment variable value, or default if not set
    """
    return os.environ.get(name, default)


def env_bool(name: str, default: bool = False) -> bool:
    """Get boolean environment variable.

    True values: "true", "1", "yes", "on" (case-insensitive)
    False values: "false", "0", "no", "off" (case-insensitive)
    Missing or invalid: returns default

    Args:
        name: Environment variable name
        default: Default value if not set or invalid

    Returns:
        Boolean value parsed from environment variable
    """
    val = os.environ.get(name)
    if val is None:
        return default
    lower = val.lower()
    if lower in ("true", "1", "yes", "on"):
        return True
    if lower in ("false", "0", "no", "off"):
        return False
    return default


def env_int(name: str, default: int = 0) -> int:
    """Get integer environment variable.

    Args:
        name: Environment variable name
        default: Default value if not set or invalid

    Returns:
        Integer value parsed from environment variable, or default on error
    """
    val = os.environ.get(name)
    if val is None:
        return default
    try:
        return int(val)
    except ValueError:
        return default


def env_float(name: str, default: float = 0.0) -> float:
    """Get float environment variable.

    Args:
        name: Environment variable name
        default: Default value if not set or invalid

    Returns:
        Float value parsed from environment variable, or default on error
    """
    val = os.environ.get(name)
    if val is None:
        return default
    try:
        return float(val)
    except ValueError:
        return default


def env_list(
    name: str, sep: str = ",", default: list[str] | None = None
) -> list[str]:
    """Get list environment variable (comma-separated by default).

    Empty items after stripping whitespace are filtered out.

    Args:
        name: Environment variable name
        sep: Separator character (default: comma)
        default: Default value if not set

    Returns:
        List of strings parsed from environment variable
    """
    val = os.environ.get(name)
    if val is None:
        return default if default is not None else []
    return [item.strip() for item in val.split(sep) if item.strip()]
