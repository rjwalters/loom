# `docker/`

This directory holds the Docker build context for Loom's fleet-worker image,
currently just the [`worker/`](worker/README.md) subdirectory — see that
README for the full `FROM` contract, bootstrap seams, and build/publish
details. Shape decision: **`loom-daemon` stays on the host**; the image
published from here (`ghcr.io/rjwalters/loom-worker:<version>`) is only the
pinned sweep-execution-environment base image a worker runs *inside*, not the
daemon itself.
