"""Support modules for the `loom-search` carve-out.

Epic #4081 Phase 4 (#4557) retired the Python `loom-tools` package. Only the
three modules still imported by :mod:`loom_tools.semantic_search` survive here:

- :mod:`loom_tools.common.config` — ``env_bool`` (the ``LOOM_SEARCH_*``
  overrides).
- :mod:`loom_tools.common.config_resolver` — the ``.loom/config.json`` tier
  chain, and one of the three implementations bound by the #4039
  cross-language conformance fixture (``tests/fixtures/config_resolver/``).
- :mod:`loom_tools.common.repo` — ``find_repo_root``.

Everything else that used to live here (``paths``, ``state``, ``git``,
``forge``/``github``/``gitea``/``cached_forge``, ``logging``,
``issue_failures``, ``time_utils``, ``tmux_session``, ``claude_config``) went
native in the Rust ``loom-daemon`` binary or was deleted outright — see
``docs/adr/0013-loom-tools-python-retirement.md``.

This package deliberately re-exports nothing: importing
``loom_tools.common.config`` must not drag in a module the carve-out no longer
ships.
"""
