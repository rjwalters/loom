# `tests/`

Repo-root shell test suites for surfaces that live **outside** `defaults/` —
the installer, the uninstaller, the Claude-path guard hooks, and the Hermit
stateless-ceremony analyzer.

Suites for the `defaults/` tree itself live in
[`defaults/scripts/tests/`](../defaults/scripts/tests/), not here.

## Layout

| Directory | Covers |
|-----------|--------|
| [`install/`](install/) | `install.sh` / `uninstall.sh` / `scripts/install/*` — forge detection, git-repo detection, daemon build + provisioning, hook preservation, manifest freshness, pnpm runnability, and uninstall safety (label block, sibling preservation) |
| [`hooks/`](hooks/) | The Claude-path guard hooks — `guard-destructive.sh`, its dispatcher, and the Loom-workflow guard |
| [`hermit/`](hermit/) | The Hermit stateless-ceremony detector, with Python fixtures in [`hermit/fixtures/`](hermit/fixtures/) covering both true positives (dispatch tables, genuine stateless classes) and true negatives (stateful classes, `self` method + state) |

## Running

Each suite is standalone and takes no arguments:

```bash
bash tests/install/test-forge-detect.sh
bash tests/hooks/test-guard-destructive.sh
bash tests/hermit/test-stateless-ceremony.sh
```

To run everything CI runs, in one pass:

```bash
bash defaults/scripts/tests/run-ci-suites.sh
```

## CI wiring

Every suite in this directory is registered in
[`defaults/scripts/tests/ci-wired.txt`](../defaults/scripts/tests/ci-wired.txt)
and executed by `run-ci-suites.sh` in the `ci.yml` shell-suite job.

**Adding a suite here is not enough to make CI run it** — it must also be added
to `ci-wired.txt`. `check-ci-suite-manifest.sh` guards that invariant in CI, so
an unregistered suite fails the manifest check rather than silently never
running. This directory was itself unwired until #4769 (`hooks/`) and #5275
(`install/`, `hermit/`), when the paths-filter in `ci.yml` matched these paths
but nothing ever invoked them.
