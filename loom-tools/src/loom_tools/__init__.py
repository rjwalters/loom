"""`loom-search`: the one Python module that survived epic #4081.

Loom's Python `loom-tools` package was retired in epic #4081 Phase 4 (#4557) —
the orchestration layer is now the Rust `loom-daemon` binary plus bash scripts,
with **no Python on the core daemon path** (nothing here is imported by the
daemon, the installer, or any `defaults/scripts/*.sh` entry point).

What remains is the opt-in, off-by-default semantic-search feature:
:mod:`loom_tools.semantic_search` (console script `loom-search`) and its
optional embeddings backend :mod:`loom_tools.embedders`, plus the three
`loom_tools.common` helpers they import. Installing this package is only ever
required to use `loom-search` — see `defaults/docs/semantic-search.md` and
`docs/adr/0013-loom-tools-python-retirement.md`.
"""

__version__ = "0.1.0"

__all__ = ["__version__"]
