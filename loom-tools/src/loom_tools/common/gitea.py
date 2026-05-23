"""Gitea implementation of the ``ForgeClient`` protocol.

Uses Gitea's REST API v1 via ``requests.Session``. Supports two
authentication modes:

* **Token auth** (default): ``Authorization: token <T>`` header.
* **Basic auth**: HTTP Basic ``username + password``. Triggered when a
  username is configured. The password is taken from the existing
  ``token`` field (or ``GITEA_TOKEN`` env var) for backward compatibility
  -- no new ``password`` field is introduced.

Authentication config (env vars take priority over ``.loom/config.json``):

* ``GITEA_TOKEN`` / ``forge.gitea.token`` -- token (or password in Basic mode).
* ``GITEA_USERNAME`` / ``forge.gitea.username`` -- if set, switches to Basic Auth.
* ``LOOM_ALLOW_INSECURE_BASIC_AUTH=1`` -- override the HTTPS guard.

Base URL is required and comes from ``.loom/config.json`` ``forge.gitea.url``.
Basic Auth over ``http://`` is refused by default to avoid leaking
credentials; set ``LOOM_ALLOW_INSECURE_BASIC_AUTH=1`` to permit it.
"""

from __future__ import annotations

import logging
import os
import re
import subprocess
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from typing import Any, Sequence

import requests

from loom_tools.common.forge import (
    EntityType,
    ForgeCIStatus,
    ForgeIssue,
    ForgePullRequest,
    get_forge_config,
)

logger = logging.getLogger(__name__)

# Default request timeout in seconds
_DEFAULT_TIMEOUT = 30

# Rate limit retry configuration
_RATE_LIMIT_MAX_RETRIES = 3
_RATE_LIMIT_INITIAL_BACKOFF = 1.0  # seconds

# Default page size for paginated requests
_DEFAULT_PAGE_LIMIT = 50


class GiteaForge:
    """Gitea implementation of the ``ForgeClient`` protocol.

    Wraps Gitea's REST API v1 behind the forge-agnostic interface. Uses
    ``requests.Session`` with either ``Authorization: token {value}``
    headers (token auth, the default) or per-request ``auth=(user, pass)``
    (HTTP Basic Auth, triggered by ``GITEA_USERNAME`` /
    ``forge.gitea.username``).

    Label operations require integer IDs. A lazy name-to-ID cache is
    maintained and auto-populated on the first label operation.
    """

    def __init__(self, cwd: Path | None = None) -> None:
        self._cwd = cwd
        self._nwo_cache: str | None = None
        self._label_cache: dict[str, int] | None = None
        self._default_branch_cache: str | None = None
        # Populated only when Basic Auth is in use. None means token auth.
        self._auth: tuple[str, str] | None = None

        # Load config
        forge_config = get_forge_config(cwd)
        gitea_config = forge_config.get("gitea", {})
        if not isinstance(gitea_config, dict):
            gitea_config = {}

        # Base URL (required)
        self._base_url = gitea_config.get("url", "").rstrip("/")
        if not self._base_url:
            raise ValueError(
                "Gitea base URL is required. Set forge.gitea.url in "
                ".loom/config.json (e.g. \"https://gitea.example.com\")"
            )

        # API token / password: env var takes priority.
        # In Basic Auth mode, this carries the password.
        token = os.environ.get("GITEA_TOKEN", "") or gitea_config.get("token", "")
        if not token:
            raise ValueError(
                "Gitea API token is required. Set GITEA_TOKEN env var or "
                "forge.gitea.token in .loom/config.json"
            )

        # Username: env var first, then config. If set, switches to Basic Auth.
        username = (
            os.environ.get("GITEA_USERNAME", "")
            or gitea_config.get("username", "")
        )

        # Build session
        self._session = requests.Session()
        self._session.headers.update({
            "Content-Type": "application/json",
            "Accept": "application/json",
        })

        if username:
            # HTTP Basic Auth mode. RFC 7617 disallows ':' in the username
            # (it would corrupt the user:pass split).
            if ":" in username:
                raise ValueError(
                    "Gitea username must not contain ':' (HTTP Basic Auth "
                    "disallows colons in usernames)."
                )
            # Refuse Basic Auth over http:// unless explicitly allowed.
            if self._base_url.startswith("http://") and (
                os.environ.get("LOOM_ALLOW_INSECURE_BASIC_AUTH", "") != "1"
            ):
                raise ValueError(
                    "Gitea Basic Auth requires HTTPS to avoid leaking "
                    "credentials. Set forge.gitea.url to an https:// URL, "
                    "or set LOOM_ALLOW_INSECURE_BASIC_AUTH=1 to override "
                    "(not recommended)."
                )
            # Do NOT add an Authorization header in Basic mode; requests
            # will compute the Basic header from `auth=` on each call.
            self._auth = (username, token)
        else:
            # Token auth (existing behavior, unchanged).
            self._session.headers["Authorization"] = f"token {token}"

    # ------------------------------------------------------------------
    # Internal helpers
    # ------------------------------------------------------------------

    def _api_url(self, path: str) -> str:
        """Build a full API URL from a relative path."""
        return f"{self._base_url}/api/v1/{path.lstrip('/')}"

    def _request(
        self,
        method: str,
        path: str,
        *,
        json: Any = None,
        params: dict[str, Any] | None = None,
    ) -> requests.Response | None:
        """Make an API request, handling errors uniformly.

        Returns the ``Response`` on success (2xx), or ``None`` on failure.
        Logs appropriate messages for auth failures, network errors, etc.

        Retries with exponential backoff on HTTP 429 (rate limited).
        """
        url = self._api_url(path)
        backoff = _RATE_LIMIT_INITIAL_BACKOFF

        for attempt in range(_RATE_LIMIT_MAX_RETRIES + 1):
            try:
                resp = self._session.request(
                    method, url, json=json, params=params,
                    timeout=_DEFAULT_TIMEOUT,
                    auth=self._auth,  # None for token auth, (user, pass) for Basic
                )
            except requests.ConnectionError:
                logger.error("Connection error reaching Gitea at %s", url)
                return None
            except requests.Timeout:
                logger.error("Request timed out: %s %s", method, url)
                return None

            # Handle rate limiting with retry + exponential backoff
            if resp.status_code == 429:
                if attempt < _RATE_LIMIT_MAX_RETRIES:
                    # Use Retry-After header if present, otherwise use backoff
                    retry_after = resp.headers.get("Retry-After")
                    if retry_after:
                        try:
                            wait = float(retry_after)
                        except (ValueError, TypeError):
                            wait = backoff
                    else:
                        wait = backoff
                    logger.warning(
                        "Gitea rate limited (429) for %s %s, "
                        "retrying in %.1fs (attempt %d/%d)",
                        method, url, wait, attempt + 1, _RATE_LIMIT_MAX_RETRIES,
                    )
                    time.sleep(wait)
                    backoff *= 2  # exponential backoff
                    continue
                else:
                    logger.error(
                        "Gitea rate limited (429) for %s %s, "
                        "exhausted all %d retries",
                        method, url, _RATE_LIMIT_MAX_RETRIES,
                    )
                    return None

            # Not rate limited, break out of retry loop
            break

        if resp.status_code in (401, 403):
            if resp.status_code == 403:
                scope_hint = (
                    " Ensure the token has 'repo' scope (or fine-grained "
                    "read/write permissions for issues, PRs, and labels)."
                )
            elif self._auth is not None:
                scope_hint = (
                    " Verify the username/password are correct. (Basic Auth "
                    "mode is in use because GITEA_USERNAME / "
                    "forge.gitea.username is set.)"
                )
            else:
                scope_hint = (
                    " Verify the token is valid and has not expired. If this "
                    "instance only supports password auth, set "
                    "GITEA_USERNAME or forge.gitea.username to switch to "
                    "Basic Auth."
                )
            logger.error(
                "Gitea auth failed (%d) for %s %s. "
                "Check GITEA_TOKEN env var or forge.gitea.token config.%s",
                resp.status_code, method, url, scope_hint,
            )
            return None

        if resp.status_code >= 500:
            logger.error(
                "Gitea server error (%d) for %s %s", resp.status_code, method, url,
            )
            return None

        if resp.status_code >= 400:
            # 4xx (not auth). 404 is expected (not found, etc.), so keep
            # those at debug level. Other 4xx (405 Method Not Allowed, 409
            # Conflict, 422 Unprocessable) usually indicate an API misuse
            # that the caller will want to see — log at warning with the
            # response body so the failure mode is visible without a debugger.
            if resp.status_code == 404:
                logger.debug(
                    "Gitea %d for %s %s: %s",
                    resp.status_code, method, url, resp.text[:200],
                )
            else:
                logger.warning(
                    "Gitea %d for %s %s: %s",
                    resp.status_code, method, url, resp.text[:500],
                )
            return None

        return resp

    def _request_paginated(
        self,
        method: str,
        path: str,
        *,
        params: dict[str, Any] | None = None,
        limit: int | None = None,
    ) -> list[dict[str, Any]]:
        """Make paginated API requests, collecting all pages.

        Loops through pages until no more results or ``limit`` is reached.
        Returns the combined list of JSON objects from all pages.

        If ``limit`` is provided, stops after collecting that many items
        (uses it as the per-page size too for efficiency).
        """
        all_items: list[dict[str, Any]] = []
        page = 1
        per_page = min(limit, _DEFAULT_PAGE_LIMIT) if limit else _DEFAULT_PAGE_LIMIT

        effective_params = dict(params or {})

        while True:
            effective_params["page"] = page
            effective_params["limit"] = per_page

            resp = self._request(method, path, params=effective_params)
            if resp is None:
                break

            try:
                items = resp.json()
            except ValueError:
                break

            if not isinstance(items, list):
                break

            all_items.extend(
                item for item in items if isinstance(item, dict)
            )

            # If caller specified a limit and we have enough, stop
            if limit is not None and len(all_items) >= limit:
                all_items = all_items[:limit]
                break

            # If we got fewer items than per_page, we've reached the last page
            if len(items) < per_page:
                break

            page += 1

        return all_items

    def _repo_path(self) -> str | None:
        """Return ``repos/{owner}/{repo}`` prefix, or None."""
        nwo = self.get_repo_nwo()
        if not nwo:
            return None
        return f"repos/{nwo}"

    # ------------------------------------------------------------------
    # Label name → ID resolution
    # ------------------------------------------------------------------

    def _populate_label_cache(self) -> None:
        """Fetch all repository labels and populate the name→ID cache.

        Uses pagination to fetch all labels (not just the first page).
        """
        rp = self._repo_path()
        if not rp:
            self._label_cache = {}
            return

        labels = self._request_paginated("GET", f"{rp}/labels")

        self._label_cache = {
            label["name"]: label["id"]
            for label in labels
            if isinstance(label, dict) and "name" in label and "id" in label
        }

    def _resolve_label_ids(self, names: Sequence[str]) -> list[int]:
        """Resolve label names to Gitea integer IDs, using cache.

        Missing labels are silently skipped. If a label is not found,
        the cache is invalidated once to allow for newly created labels.
        """
        if self._label_cache is None:
            self._populate_label_cache()
        assert self._label_cache is not None  # noqa: S101

        ids: list[int] = []
        missing: list[str] = []
        for name in names:
            lid = self._label_cache.get(name)
            if lid is not None:
                ids.append(lid)
            else:
                missing.append(name)

        # Retry once if any labels were missing (cache may be stale)
        if missing:
            self._label_cache = None
            self._populate_label_cache()
            assert self._label_cache is not None  # noqa: S101
            for name in missing:
                lid = self._label_cache.get(name)
                if lid is not None:
                    ids.append(lid)
                else:
                    logger.warning("Gitea label %r not found in repository", name)

        return ids

    def _resolve_label_id(self, name: str) -> int | None:
        """Resolve a single label name to its ID."""
        ids = self._resolve_label_ids([name])
        return ids[0] if ids else None

    # ------------------------------------------------------------------
    # State normalization
    # ------------------------------------------------------------------

    @staticmethod
    def _normalize_state(state: str, merged: bool = False) -> str:
        """Normalize Gitea's lowercase states to uppercase.

        Gitea returns ``"open"`` / ``"closed"``. For PRs with
        ``merged=true``, we return ``"MERGED"``.
        """
        if merged:
            return "MERGED"
        return state.upper()

    # ------------------------------------------------------------------
    # Conversion helpers
    # ------------------------------------------------------------------

    def _to_forge_issue(self, data: dict[str, Any]) -> ForgeIssue:
        """Convert a Gitea issue JSON object to ``ForgeIssue``."""
        labels_data = data.get("labels", [])
        label_names = [
            lbl["name"] for lbl in labels_data
            if isinstance(lbl, dict) and "name" in lbl
        ]
        return ForgeIssue(
            number=data.get("number", 0),
            state=self._normalize_state(data.get("state", "open")),
            title=data.get("title", ""),
            url=data.get("html_url", ""),
            labels=label_names,
            body=data.get("body"),
        )

    def _to_forge_pr(self, data: dict[str, Any]) -> ForgePullRequest:
        """Convert a Gitea PR JSON object to ``ForgePullRequest``."""
        labels_data = data.get("labels", [])
        label_names = [
            lbl["name"] for lbl in labels_data
            if isinstance(lbl, dict) and "name" in lbl
        ]
        merged = data.get("merged", False) is True
        head_info = data.get("head", {})
        head_branch = (
            head_info.get("ref") or head_info.get("label")
            if isinstance(head_info, dict) else None
        )
        return ForgePullRequest(
            number=data.get("number", 0),
            state=self._normalize_state(data.get("state", "open"), merged=merged),
            title=data.get("title", ""),
            url=data.get("html_url", ""),
            labels=label_names,
            head_branch=head_branch,
            body=data.get("body"),
        )

    # ------------------------------------------------------------------
    # ForgeClient.forge_type
    # ------------------------------------------------------------------

    @property
    def forge_type(self) -> str:
        """Identifier for the forge backend."""
        return "gitea"

    # ------------------------------------------------------------------
    # Issue operations
    # ------------------------------------------------------------------

    def get_issue(self, number: int) -> ForgeIssue | None:
        """Fetch a single issue by number."""
        rp = self._repo_path()
        if not rp:
            return None
        resp = self._request("GET", f"{rp}/issues/{number}")
        if resp is None:
            return None
        try:
            data = resp.json()
        except ValueError:
            return None
        if not isinstance(data, dict):
            return None
        # Gitea issues endpoint may include PRs — filter them out
        if data.get("pull_request") is not None:
            return None
        return self._to_forge_issue(data)

    def list_issues(
        self,
        *,
        labels: Sequence[str] | None = None,
        state: str = "open",
        limit: int | None = None,
    ) -> list[ForgeIssue]:
        """List issues matching the given filters.

        Uses pagination to fetch all matching issues (not just the first
        page of 50). Pass ``limit`` to cap the total number of results.
        """
        rp = self._repo_path()
        if not rp:
            return []
        params: dict[str, Any] = {
            "type": "issues",  # exclude PRs
            "state": state,
        }
        if labels:
            params["labels"] = ",".join(labels)

        items = self._request_paginated(
            "GET", f"{rp}/issues", params=params, limit=limit,
        )
        return [self._to_forge_issue(d) for d in items]

    def create_issue(
        self,
        title: str,
        body: str,
        labels: Sequence[str] | None = None,
    ) -> ForgeIssue | None:
        """Create a new issue."""
        rp = self._repo_path()
        if not rp:
            return None
        payload: dict[str, Any] = {"title": title, "body": body}
        if labels:
            label_ids = self._resolve_label_ids(labels)
            if label_ids:
                payload["labels"] = label_ids
        resp = self._request("POST", f"{rp}/issues", json=payload)
        if resp is None:
            return None
        try:
            data = resp.json()
        except ValueError:
            return None
        if not isinstance(data, dict):
            return None
        return self._to_forge_issue(data)

    def close_issue(self, number: int) -> bool:
        """Close an issue."""
        rp = self._repo_path()
        if not rp:
            return False
        resp = self._request("PATCH", f"{rp}/issues/{number}", json={"state": "closed"})
        return resp is not None

    def comment_on_issue(self, number: int, body: str) -> bool:
        """Add a comment to an issue."""
        rp = self._repo_path()
        if not rp:
            return False
        resp = self._request(
            "POST", f"{rp}/issues/{number}/comments", json={"body": body},
        )
        return resp is not None

    # ------------------------------------------------------------------
    # Pull request operations
    # ------------------------------------------------------------------

    def get_pull_request(self, number: int) -> ForgePullRequest | None:
        """Fetch a single pull request by number."""
        rp = self._repo_path()
        if not rp:
            return None
        resp = self._request("GET", f"{rp}/pulls/{number}")
        if resp is None:
            return None
        try:
            data = resp.json()
        except ValueError:
            return None
        if not isinstance(data, dict):
            return None
        return self._to_forge_pr(data)

    def list_pull_requests(
        self,
        *,
        labels: Sequence[str] | None = None,
        state: str = "open",
        head: str | None = None,
        search: str | None = None,
        limit: int | None = None,
    ) -> list[ForgePullRequest]:
        """List pull requests matching the given filters.

        Uses pagination to fetch all matching PRs (not just the first
        page of 50). Pass ``limit`` to cap the total number of results.
        """
        if search:
            logger.warning(
                "Gitea does not support 'search' parameter for pull request listing; "
                "ignoring search=%r", search,
            )

        rp = self._repo_path()
        if not rp:
            return []
        params: dict[str, Any] = {"state": state}
        if labels:
            params["labels"] = ",".join(labels)

        items = self._request_paginated(
            "GET", f"{rp}/pulls", params=params, limit=limit,
        )

        prs = [self._to_forge_pr(d) for d in items]

        # Client-side head branch filtering (Gitea API doesn't support it natively)
        if head:
            prs = [pr for pr in prs if pr.head_branch == head]

        return prs

    def create_pull_request(
        self,
        title: str,
        body: str,
        head: str,
        base: str | None = None,
        labels: Sequence[str] | None = None,
    ) -> ForgePullRequest | None:
        """Create a new pull request."""
        rp = self._repo_path()
        if not rp:
            return None

        if base is None:
            base = self.get_repo_default_branch() or "main"

        payload: dict[str, Any] = {
            "title": title,
            "body": body,
            "head": head,
            "base": base,
        }
        if labels:
            label_ids = self._resolve_label_ids(labels)
            if label_ids:
                payload["labels"] = label_ids

        resp = self._request("POST", f"{rp}/pulls", json=payload)
        if resp is None:
            return None
        try:
            data = resp.json()
        except ValueError:
            return None
        if not isinstance(data, dict):
            return None
        return self._to_forge_pr(data)

    def close_pull_request(
        self, number: int, comment: str | None = None,
    ) -> bool:
        """Close a pull request, optionally leaving a comment."""
        rp = self._repo_path()
        if not rp:
            return False
        # Comment first (PRs share the issue comment API in Gitea)
        if comment:
            self._request(
                "POST", f"{rp}/issues/{number}/comments", json={"body": comment},
            )
        resp = self._request(
            "PATCH", f"{rp}/pulls/{number}", json={"state": "closed"},
        )
        return resp is not None

    def merge_pull_request(
        self, number: int, method: str = "squash",
    ) -> bool:
        """Merge a pull request.

        Gitea computes the ``mergeable`` flag asynchronously after a PR is
        created (or after the head branch is updated). Issuing a merge
        request before that computation finishes returns HTTP 405 with a
        body like ``"Please try again later"``. To make the call robust
        for callers that create-then-merge (e.g. integration tests), we
        briefly poll ``GET /pulls/{number}`` for ``mergeable: true``
        before attempting the merge. The poll is short (a few seconds);
        production callers that wait on CI should use
        :meth:`auto_merge_pull_request` instead.
        """
        rp = self._repo_path()
        if not rp:
            return False

        # Best-effort wait for Gitea to compute mergeability. Don't fail
        # if the poll itself errors — fall through to the merge attempt
        # and let the merge response be the source of truth.
        deadline = time.time() + 10.0
        while time.time() < deadline:
            pr_resp = self._request("GET", f"{rp}/pulls/{number}")
            if pr_resp is None:
                break
            try:
                pr_data = pr_resp.json()
            except ValueError:
                break
            if not isinstance(pr_data, dict):
                break
            if pr_data.get("merged") is True:
                # Already merged — nothing to do.
                return True
            mergeable = pr_data.get("mergeable")
            if mergeable is True:
                break
            if mergeable is False:
                # Definitive: Gitea knows the PR can't be merged
                # (conflicts, draft, etc.). Don't waste a merge call.
                logger.warning(
                    "Gitea PR #%d is not mergeable; skipping merge attempt",
                    number,
                )
                return False
            # mergeable is None — still computing; back off briefly
            time.sleep(0.5)

        payload = {
            "Do": method,
            "delete_branch_after_merge": True,
        }
        resp = self._request("POST", f"{rp}/pulls/{number}/merge", json=payload)
        return resp is not None

    def auto_merge_pull_request(
        self,
        number: int,
        method: str = "squash",
        poll_interval: int = 30,
        timeout: int = 600,
    ) -> bool:
        """Poll CI status and merge when checks pass.

        Gitea has no native auto-merge queue, so this polls the CI
        status of the PR's head commit and merges once all checks
        are green. If no CI checks are found, merges immediately
        (assumes no branch protection requires checks).

        Parameters
        ----------
        number:
            PR number.
        method:
            Merge method (``"squash"``, ``"merge"``, ``"rebase"``).
        poll_interval:
            Seconds between CI status polls.
        timeout:
            Maximum seconds to wait for CI before giving up.
        """
        rp = self._repo_path()
        if not rp:
            return False

        # Get PR to extract head SHA
        resp = self._request("GET", f"{rp}/pulls/{number}")
        if resp is None:
            logger.error("Cannot fetch PR #%d for auto-merge", number)
            return False
        try:
            pr_data = resp.json()
        except ValueError:
            logger.error("Invalid JSON from PR #%d", number)
            return False
        if not isinstance(pr_data, dict):
            return False

        # Check if already merged
        if pr_data.get("merged") is True:
            logger.info("PR #%d is already merged", number)
            return True

        head_info = pr_data.get("head", {})
        head_sha = head_info.get("sha") if isinstance(head_info, dict) else None
        if not head_sha:
            logger.warning(
                "Cannot determine head SHA for PR #%d, attempting immediate merge",
                number,
            )
            return self.merge_pull_request(number, method=method)

        logger.info(
            "Starting poll-and-merge for PR #%d (sha=%s, interval=%ds, timeout=%ds)",
            number, head_sha[:8], poll_interval, timeout,
        )

        elapsed = 0
        while elapsed < timeout:
            ci_result = self._get_raw_ci_state(head_sha)

            if ci_result == "passing":
                logger.info("CI passing for PR #%d, merging", number)
                return self.merge_pull_request(number, method=method)

            if ci_result == "failing":
                logger.warning("CI failing for PR #%d", number)
                return False

            if ci_result == "no_checks":
                logger.info(
                    "No CI checks found for PR #%d, merging immediately",
                    number,
                )
                return self.merge_pull_request(number, method=method)

            # CI pending — wait and retry
            logger.info(
                "CI pending for PR #%d, waiting %ds... (%d/%ds elapsed)",
                number, poll_interval, elapsed, timeout,
            )
            time.sleep(poll_interval)
            elapsed += poll_interval

        logger.warning(
            "Timeout waiting for CI on PR #%d after %ds", number, timeout,
        )
        return False

    def _get_raw_ci_state(self, sha: str) -> str:
        """Query raw CI state for a commit, distinguishing pending from passing.

        Returns one of: ``"passing"``, ``"failing"``, ``"pending"``,
        ``"no_checks"``.

        Unlike ``get_commit_ci_status()`` which treats pending as passing
        (optimistic for dashboard display), this method distinguishes
        pending checks so the auto-merge poll loop can wait for completion.
        """
        rp = self._repo_path()
        if not rp:
            return "no_checks"

        # Fetch commit statuses
        resp = self._request("GET", f"{rp}/commits/{sha}/statuses")
        statuses: list[dict[str, Any]] = []
        if resp is not None:
            try:
                data = resp.json()
                if isinstance(data, list):
                    statuses = data
            except ValueError:
                pass

        # Group by context, take latest
        latest_by_context: dict[str, str] = {}
        for s in statuses:
            if not isinstance(s, dict):
                continue
            ctx = s.get("context", "unknown")
            if ctx not in latest_by_context:
                latest_by_context[ctx] = self._classify_gitea_status(
                    s.get("status", ""),
                )

        if not latest_by_context:
            return "no_checks"

        has_failure = any(v == "failure" for v in latest_by_context.values())
        if has_failure:
            return "failing"

        has_pending = any(v == "pending" for v in latest_by_context.values())
        if has_pending:
            return "pending"

        return "passing"

    def comment_on_pull_request(self, number: int, body: str) -> bool:
        """Add a comment to a pull request (shares issue comment API)."""
        rp = self._repo_path()
        if not rp:
            return False
        resp = self._request(
            "POST", f"{rp}/issues/{number}/comments", json={"body": body},
        )
        return resp is not None

    def get_pull_request_reviews(
        self, number: int,
    ) -> list[dict[str, Any]]:
        """Fetch reviews for a pull request."""
        rp = self._repo_path()
        if not rp:
            return []
        resp = self._request("GET", f"{rp}/pulls/{number}/reviews")
        if resp is None:
            return []
        try:
            reviews = resp.json()
        except ValueError:
            return []
        if not isinstance(reviews, list):
            return []
        # Normalize review state to uppercase
        for review in reviews:
            if isinstance(review, dict) and "state" in review:
                review["state"] = review["state"].upper()
        return reviews

    # ------------------------------------------------------------------
    # Label operations
    # ------------------------------------------------------------------

    def add_labels(
        self, entity_type: EntityType, number: int, labels: Sequence[str],
    ) -> bool:
        """Add labels to an issue or PR (same endpoint in Gitea)."""
        rp = self._repo_path()
        if not rp:
            return False
        label_ids = self._resolve_label_ids(labels)
        if not label_ids:
            return False
        resp = self._request(
            "POST", f"{rp}/issues/{number}/labels", json={"labels": label_ids},
        )
        return resp is not None

    def remove_labels(
        self, entity_type: EntityType, number: int, labels: Sequence[str],
    ) -> bool:
        """Remove labels from an issue or PR (one call per label)."""
        rp = self._repo_path()
        if not rp:
            return False
        success = True
        for name in labels:
            lid = self._resolve_label_id(name)
            if lid is None:
                continue
            resp = self._request("DELETE", f"{rp}/issues/{number}/labels/{lid}")
            if resp is None:
                success = False
        return success

    def transition_labels(
        self,
        entity_type: EntityType,
        number: int,
        add: Sequence[str] | None = None,
        remove: Sequence[str] | None = None,
    ) -> bool:
        """Add and remove labels in sequence (no atomic API in Gitea)."""
        success = True
        if remove:
            if not self.remove_labels(entity_type, number, remove):
                success = False
        if add:
            if not self.add_labels(entity_type, number, add):
                success = False
        return success

    # ------------------------------------------------------------------
    # CI status
    # ------------------------------------------------------------------

    # Gitea commit status values -> Loom internal status
    _STATUS_MAP_FAILURE = frozenset({"failure", "error"})
    _STATUS_MAP_PENDING = frozenset({"pending", "warning"})
    _STATUS_MAP_SUCCESS = frozenset({"success"})

    # Gitea Actions run conclusion -> Loom internal status
    _ACTIONS_FAILURE = frozenset({"failure", "cancelled"})
    _ACTIONS_PENDING_STATUSES = frozenset({"queued", "waiting", "running", "in_progress"})

    def _classify_gitea_status(self, status: str) -> str:
        """Map a Gitea status value to Loom internal status.

        Returns ``"success"``, ``"failure"``, or ``"pending"``.
        """
        s = status.lower()
        if s in self._STATUS_MAP_FAILURE:
            return "failure"
        if s in self._STATUS_MAP_PENDING:
            return "pending"
        if s in self._STATUS_MAP_SUCCESS:
            return "success"
        return "pending"  # unknown values treated as pending

    def _aggregate_commit_statuses(
        self, statuses: list[dict[str, Any]],
    ) -> ForgeCIStatus:
        """Aggregate a list of Gitea commit status objects.

        Groups by ``context`` (keeping only the latest per context),
        then derives the worst-case overall status.
        """
        if not statuses:
            return ForgeCIStatus(
                status="unknown", message="No CI statuses found",
            )

        # Group by context, keep latest (first in list = most recent)
        latest_by_context: dict[str, dict[str, Any]] = {}
        for s in statuses:
            if not isinstance(s, dict):
                continue
            ctx = s.get("context", "unknown")
            if ctx not in latest_by_context:
                latest_by_context[ctx] = s

        failed_runs: list[str] = []
        has_pending = False
        for ctx, s in latest_by_context.items():
            classified = self._classify_gitea_status(s.get("status", ""))
            if classified == "failure":
                failed_runs.append(ctx)
            elif classified == "pending":
                has_pending = True

        total = len(latest_by_context)
        if failed_runs:
            return ForgeCIStatus(
                status="failing",
                failed_runs=failed_runs,
                total_runs=total,
                message=f"CI failing: {len(failed_runs)} check(s) failed",
            )

        if has_pending:
            return ForgeCIStatus(
                status="passing",
                total_runs=total,
                message="CI passing (some checks pending)",
            )

        return ForgeCIStatus(
            status="passing",
            total_runs=total,
            message="CI passing",
        )

    def _fetch_actions_runs(self, rp: str, ref: str) -> list[dict[str, Any]] | None:
        """Fetch Gitea Actions workflow runs for a branch/ref.

        Returns ``None`` if the Actions API is unavailable (404) or
        on any error, enabling callers to fall back to commit statuses only.

        Only available on Gitea 1.19+.
        """
        resp = self._request(
            "GET",
            f"{rp}/actions/runs",
            params={"branch": ref, "limit": 10},
        )
        if resp is None:
            return None

        try:
            data = resp.json()
        except ValueError:
            return None

        # Gitea Actions API returns {"workflow_runs": [...]} or just [...]
        if isinstance(data, dict):
            runs = data.get("workflow_runs", [])
        elif isinstance(data, list):
            runs = data
        else:
            return None

        if not isinstance(runs, list):
            return None

        return runs

    def _aggregate_actions_runs(
        self, runs: list[dict[str, Any]],
    ) -> ForgeCIStatus:
        """Aggregate Gitea Actions workflow runs into a ForgeCIStatus.

        Groups by workflow name, keeps only the latest run per workflow.
        """
        if not runs:
            return ForgeCIStatus(
                status="unknown", message="No Actions runs found",
            )

        latest_by_name: dict[str, dict[str, Any]] = {}
        for run in runs:
            if not isinstance(run, dict):
                continue
            name = run.get("name", "Unknown")
            if name not in latest_by_name:
                latest_by_name[name] = run

        failed_runs: list[str] = []
        for name, run in latest_by_name.items():
            status = run.get("status", "").lower()
            conclusion = run.get("conclusion", "").lower()

            # If not completed, skip (pending)
            if status in self._ACTIONS_PENDING_STATUSES:
                continue

            if conclusion in self._ACTIONS_FAILURE:
                failed_runs.append(name)

        total = len(latest_by_name)
        if failed_runs:
            return ForgeCIStatus(
                status="failing",
                failed_runs=failed_runs,
                total_runs=total,
                message=f"CI failing: {len(failed_runs)} workflow(s) failed",
            )

        return ForgeCIStatus(
            status="passing",
            total_runs=total,
            message="CI passing",
        )

    def _merge_ci_results(
        self,
        commit_result: ForgeCIStatus,
        actions_result: ForgeCIStatus | None,
    ) -> ForgeCIStatus:
        """Merge commit status and Actions run results.

        If both sources are available, combines them. If either source
        reports failure, the merged result is failing.
        """
        if actions_result is None or actions_result.status == "unknown":
            return commit_result
        if commit_result.status == "unknown":
            return actions_result

        # Merge: combine failed runs and total counts
        all_failed = list(set(commit_result.failed_runs + actions_result.failed_runs))
        total = commit_result.total_runs + actions_result.total_runs

        if all_failed:
            return ForgeCIStatus(
                status="failing",
                failed_runs=all_failed,
                total_runs=total,
                message=f"CI failing: {len(all_failed)} check(s) failed",
            )

        return ForgeCIStatus(
            status="passing",
            total_runs=total,
            message="CI passing",
        )

    def get_default_branch_ci_status(self) -> ForgeCIStatus:
        """Get CI status for the latest commit on the default branch.

        Queries both commit statuses and Gitea Actions runs (if available).
        Gitea Actions API (1.19+) is feature-detected via 404 fallback.
        """
        default_branch = self.get_repo_default_branch()
        if not default_branch:
            return ForgeCIStatus(
                status="unknown", message="Cannot determine default branch",
            )
        return self.get_commit_ci_status(default_branch)

    def get_commit_ci_status(self, sha: str) -> ForgeCIStatus:
        """Get CI status for a specific commit or ref.

        Queries both commit statuses and Gitea Actions runs (if available),
        then merges the results. Handles Gitea version differences by
        gracefully falling back when Actions API returns 404.
        """
        rp = self._repo_path()
        if not rp:
            return ForgeCIStatus(
                status="unknown", message="Cannot determine repository",
            )

        # 1. Fetch commit statuses (available in all Gitea versions)
        resp = self._request(
            "GET", f"{rp}/commits/{sha}/statuses",
        )

        commit_statuses: list[dict[str, Any]] = []
        if resp is not None:
            try:
                data = resp.json()
                if isinstance(data, list):
                    commit_statuses = data
            except ValueError:
                pass

        commit_result = self._aggregate_commit_statuses(commit_statuses)

        # 2. Try Gitea Actions runs (1.19+, graceful 404 fallback)
        actions_runs = self._fetch_actions_runs(rp, sha)
        actions_result: ForgeCIStatus | None = None
        if actions_runs is not None:
            actions_result = self._aggregate_actions_runs(actions_runs)

        # 3. Merge both results
        return self._merge_ci_results(commit_result, actions_result)

    # ------------------------------------------------------------------
    # Repository metadata
    # ------------------------------------------------------------------

    def get_repo_nwo(self) -> str | None:
        """Return the ``owner/repo`` identifier, parsed from git remote."""
        if self._nwo_cache is not None:
            return self._nwo_cache

        try:
            result = subprocess.run(
                ["git", "remote", "get-url", "origin"],
                cwd=self._cwd,
                text=True,
                capture_output=True,
                check=False,
            )
            if result.returncode != 0 or not result.stdout.strip():
                return None
            nwo = self._parse_nwo(result.stdout.strip())
            if nwo:
                self._nwo_cache = nwo
            return nwo
        except OSError:
            return None

    @staticmethod
    def _parse_nwo(url: str) -> str | None:
        """Extract ``owner/repo`` from a git remote URL."""
        # SSH: git@host:owner/repo.git
        ssh_match = re.match(r"git@[^:]+:(.+?)(?:\.git)?$", url)
        if ssh_match:
            return ssh_match.group(1)
        # HTTPS: https://host/owner/repo.git
        https_match = re.match(r"https?://[^/]+/(.+?)(?:\.git)?$", url)
        if https_match:
            return https_match.group(1)
        return None

    def get_repo_default_branch(self) -> str | None:
        """Return the default branch name from Gitea's repo API."""
        if self._default_branch_cache is not None:
            return self._default_branch_cache

        rp = self._repo_path()
        if not rp:
            return None
        resp = self._request("GET", rp)
        if resp is None:
            return None
        try:
            data = resp.json()
        except ValueError:
            return None
        if isinstance(data, dict):
            branch = data.get("default_branch")
            if isinstance(branch, str):
                self._default_branch_cache = branch
                return branch
        return None

    # ------------------------------------------------------------------
    # Batch operations
    # ------------------------------------------------------------------

    def get_issues_batch(
        self, numbers: Sequence[int],
    ) -> dict[int, ForgeIssue | None]:
        """Fetch multiple issues concurrently."""
        results: dict[int, ForgeIssue | None] = {}

        def _fetch(num: int) -> tuple[int, ForgeIssue | None]:
            return (num, self.get_issue(num))

        with ThreadPoolExecutor(max_workers=4) as pool:
            futures = [pool.submit(_fetch, n) for n in numbers]
            for f in futures:
                num, issue = f.result()
                results[num] = issue

        return results

    # Patterns that indicate a PR closes an issue (case-insensitive)
    _CLOSING_KEYWORDS = ("Closes", "Fixes", "Resolves")

    def find_pull_request_for_issue(
        self, issue: int, state: str = "open",
    ) -> int | None:
        """Find a PR associated with a given issue.

        Searches by branch naming convention first, then falls back to
        body content matching for closing keywords (Closes, Fixes,
        Resolves).
        """
        # Try branch naming convention
        prs = self.list_pull_requests(
            head=f"feature/issue-{issue}", state=state,
        )
        if prs:
            return prs[0].number

        # Fall back: list all PRs and search body for closing references.
        # Uses pagination (no silent truncation at 50).
        all_prs = self.list_pull_requests(state=state)
        closing_pattern = re.compile(
            rf"(?:{'|'.join(self._CLOSING_KEYWORDS)})\s+#{issue}\b",
            re.IGNORECASE,
        )
        for pr in all_prs:
            if pr.body and closing_pattern.search(pr.body):
                return pr.number

        return None
