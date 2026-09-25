# Remote build host (heavy cargo steps leave the laptop)

## Why

Local `cargo check --all-targets`, `cargo test`, and `cargo clippy` runs are
the slow part of `loom-daemon` work: minutes on a warm tree, far longer cold
or on a loaded machine. A saturated laptop is how verification becomes
unverifiable, so the standing practice is:

- **Edit, search, and review locally** — edits, `rg`, design and ADR work,
  git metadata (commit, log, diff review), `create-issue.sh`, and other fast
  checks.
- **Compile, test, and lint on a remote build host** — every heavy cargo step,
  including "just one quick check".

The worktree stays canonical: sync one way (local to host), and never edit on
the host anything you will not sync again from the worktree.

## Where the concrete host lives

Which host, its ssh alias, how to start and stop it, where its checkout lives,
and any usage limits are **operator-local**, not repository facts. Keep them
in operator-local config (for example `~/.config/repo/README.md` or a
gitignored `.env`) and read them from there. This guide deliberately names no
host, alias, instance, or path: those change, and a checked-in value that has
gone stale is worse than none.

If the host is unreachable (for example it idle-stopped), stop and report
rather than provisioning a new one; the operator-local config says how to
bring the existing host back.

## Environment floor on the host

A bare Linux image produced false failures that never appear on macOS or
GitHub CI. A freshly provisioned host must meet this floor before its first
test run is trusted:

1. **`jq` installed.** The `runtime_admission` conformance test runs the
   shipped shell checker, which shells out to `jq`; without it the test fails
   for an environmental reason, not a code one.
2. **git >= 2.38.** The tree-equality proof in `worktree_ops` (`landed.rs`)
   uses `git merge-tree --write-tree`, added in git 2.38. On older git the
   probe fails closed to `Landed::Unknown`, which moves two
   `worktree_ops::aggressive` tests from the expected `UnreachableHead` skip to
   a `LandedUnknown` skip. The code is correct and conservative (the same
   worktree is kept); the tests encode the CI git behaviour.

If a daemon test fails on the host but passes on CI (or the reverse), check
these two before reading the code.

## Fanning several tasks out onto one host

Several concurrent tasks (subagents, parallel issues) can share one host:

- **One task, one checkout.** Each task gets its own checkout directory on the
  host (for example `build/<issue>/`), synced from that task's worktree.
- **One shared `CARGO_TARGET_DIR`.** Point every task build at a single shared
  target directory, seeded once by copying a warm `target/`. Dependency crates
  (the bulk of a cold build) are path-independent and come back warm; only the
  Loom crates rebuild per checkout, so a new task's first build takes minutes
  rather than a full cold compile. Re-seed from a warm checkout when the
  dependency set moves substantially.
- **Serialization on the cargo lock is expected.** Concurrent builds in the
  shared directory wait on cargo's build lock. That is normal, not a failure;
  coordinate heavy repeat loops (load or repro runs) so they do not starve the
  other tasks.
- **Tasks never provision.** A task uses the existing host or stops and
  reports; it does not create a new one.

## Known flake

- `forge_listing::tests::errors_carry_stderr_for_the_rate_limit_classifier`
  has failed in a full parallel suite run while passing solo and passing on
  CI, which points at a parallel interaction in the test rather than a
  regression. **Rule:** re-run the module solo before believing a failure. If
  it recurs in more than a quarter of full runs, triage it for real.
