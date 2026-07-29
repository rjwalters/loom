"""Tier B pluggable vector-embeddings for ``loom-search`` (#4370, follow-up to #4339).

Implements the ``Embedder`` protocol declared in
:mod:`loom_tools.semantic_search` and a provider factory. Two providers:

- ``"none"`` (default): no embeddings; ranking stays pure BM25, byte-identical
  to the v1 (#4339) behavior. This module is never imported on that path.
- ``"local"``: a small local ONNX model via the optional ``fastembed``
  package, gated behind the ``loom-tools[search]`` extra. Imported lazily
  (inside :class:`FastEmbedEmbedder`, not at module import time) so a
  ``provider=none`` host never pulls in ``fastembed`` even transitively.

A remote-API provider is explicitly **out of scope** for this PR — see the
follow-up issue filed at ``loom:triage`` for that work.
"""

from __future__ import annotations

import logging
from typing import Protocol

logger = logging.getLogger(__name__)

#: Default fastembed model — small (~130MB), CPU-only, ONNX-backed.
DEFAULT_LOCAL_MODEL = "BAAI/bge-small-en-v1.5"

#: Shared install hint surfaced in every missing-dependency error/warning.
INSTALL_HINT = "pip install 'loom-tools[search]'"

#: Valid values for ``search.embeddings.provider``.
VALID_PROVIDERS = frozenset({"none", "local"})


class Embedder(Protocol):
    """A vector-embedding provider. Mirrors the stub declared in ``semantic_search``."""

    def embed(self, text: str) -> list[float]:
        """Return a vector embedding for ``text``."""
        ...


class MissingEmbeddingDependencyError(RuntimeError):
    """Raised when a provider is configured but its optional dependency is absent."""


class UnknownEmbeddingsProviderError(ValueError):
    """Raised when ``search.embeddings.provider`` names an unrecognized provider."""


class FastEmbedEmbedder:
    """Local, CPU-only ONNX embeddings via the optional ``fastembed`` package.

    The one-time model download (on first construction, first repo, first
    model name) is the only outbound network call this provider makes; every
    subsequent ``embed()`` call is fully offline. See
    ``defaults/docs/semantic-search.md`` for the documented threat model.
    """

    def __init__(self, model_name: str = DEFAULT_LOCAL_MODEL) -> None:
        try:
            from fastembed import TextEmbedding
        except ImportError as exc:
            raise MissingEmbeddingDependencyError(
                "search.embeddings.provider=local requires the 'fastembed' "
                f"package, which is not installed. Install it with: {INSTALL_HINT}"
            ) from exc
        self.model_name = model_name
        self._model = TextEmbedding(model_name=model_name)

    def embed(self, text: str) -> list[float]:
        (vector,) = self._model.embed([text])
        return [float(x) for x in vector]


def create_embedder(provider: str, *, model_name: str = DEFAULT_LOCAL_MODEL) -> Embedder | None:
    """Provider factory.

    Args:
        provider: One of :data:`VALID_PROVIDERS`. ``"none"`` returns ``None``
            (no embeddings) without importing anything else.
        model_name: Model identifier passed through to the underlying
            provider. Also stored verbatim as the ``embeddings.model`` column
            so different models never collide.

    Returns:
        An :class:`Embedder` instance, or ``None`` when ``provider == "none"``.

    Raises:
        MissingEmbeddingDependencyError: ``provider`` is configured but its
            optional dependency is not importable. Callers decide whether
            that is a hard error (index time) or a caught, degraded warning
            (query time) — this factory always raises so both call sites can
            share one code path.
        UnknownEmbeddingsProviderError: ``provider`` is not a recognized value.
    """
    if provider == "none":
        return None
    if provider == "local":
        return FastEmbedEmbedder(model_name=model_name)
    raise UnknownEmbeddingsProviderError(
        f"Unknown search.embeddings.provider: {provider!r} (expected one of {sorted(VALID_PROVIDERS)})"
    )
