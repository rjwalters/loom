# ADR-0018: Rust Owns Behavior; Shell Exists Only to Reach It

## Status

**Accepted** — operator ruling 2026-09-16 (issue #7777). Drafted as #7777 for
discussion; section structure follows ADR-0016.

No implementation ships with this ADR. The downstream cluster carries the
work: epic #7810 (migration program), #7758 (irreducible bootstrap-surface
inventory), #7762 (language policy — narrows to the default rule stated
here), and #7794 (`--help` split). Two follow-on slices carry the execution
contract and the compatibility contract; a third carries the first complete
migration.

## Context

### The directory does not look the way people assume

`defaults/scripts/` is **105 files, 21,988 code lines, median 122 lines, max
1,853**. 76 of 105 are under 200 lines. It is not a pile of mega-scripts. The
genuinely large shell surfaces are elsewhere: `defaults/scripts/tests/`
(206 files, ~91k lines) and `defaults/hooks/` (8 files, 13.6k).

### A migration is already ~45% done and undocumented

| state | count |
|---|---|
| thin stubs over `loom-daemon` (<40 code lines) | 15 |
| larger, but already call `loom-daemon` | 32 |
| pure shell, no `loom-daemon` at all | 58 |

Six are literally four lines of `exec`. There is **no README and no doc**
stating what the directory is organized by, so three migration states coexist
with nothing marking which is which. That — not size, and not language alone —
is why the directory reads as a rat's nest.

### The names are a public interface

**58 distinct scripts are invoked from role prompts**; 11 more from CI
workflows. The script *names* are the API agents call. Moving logic behind a
name is cheap; renaming is not.

### What shell is actually costing

- **Lexing text in shell**: 52 of 99 commits to `guard-destructive-generic.sh`
  in 12 months are code-vs-quoted-data confusion, with reproduced silent
  ALLOWs (#7755, #7760).
- **Portability**: a recurring bash 3.2/4 class breaking macOS dev machines
  (#7717, #7728, #7730, #7749, #7751).
- **Retry/backoff policy in bash**: `claude-wrapper.sh` is 2,932 lines
  described as *"Resilient Claude CLI wrapper with retry logic."*

### Supporting measurements (from the #7777 discussion)

Measurements offered on the issue, rather than opinion, since the ADR asks for
the case to be made on cost. They were taken over a single day of working in
the shell surface and are recorded here as the evidence the ruling rested on.

**The guard layer is the sharpest instance of "shell owns behavior":**

| | |
|---|---|
| `guard-destructive-generic.sh` | **8,976 lines** — largest non-daemon file in the repo |
| all `defaults/hooks/` | 13,725 lines |
| latency | **~23ms per invocation**, measured over 20 runs; two guards on `PreToolUse`/`Bash`, so **~46ms on every Bash tool call** |
| commits to that one file, 12 months | **101** |
| of those, in the code-vs-quoted-data class | **49** |
| commits across all guard hooks, 12 months | 173 |

(The 52-of-99 and 49-of-101 figures above are two independent counts of the
same commit history taken on different days with different classification
cut-offs; both land at roughly half.)

This is not a script that *reaches* behavior. It is a 9,000-line regex engine
implementing a security boundary, on the hot path of every tool call, that
must decide "is this token executed or quoted?" — a question a tokenizer
answers exactly and a regex answers approximately, 49 times a year.

**Seven bash-3.2 incompatibilities surfaced in about a day:** #7721 (parse),
#7717 (`local -A`, `${x,,}`), #7733 (empty array under `set -u`), #7749
(`declare -A`), #7730 (Rust-embedded driver), #7751 (six more scripts), #7783
(`post-verdict.sh`). Two bear directly on the compatibility contract:

- **#7733 was found underneath #7717.** Fixing the first crash merely exposed
  the second. A static scan for known-bad constructs would have found
  `local -A` and missed the empty-array case entirely.
- **#7783 fails when the PR is healthy.** `${_gate_all_ids[*]}` is unbound
  precisely when there is nothing wrong — so on macOS, *"no Judge can render
  any verdict at all"*. The failure mode is inverted, which no denylist would
  predict.

None of this class is expressible in Rust. There is no runtime "this data
structure does not exist in your interpreter, so writes silently go elsewhere
while the process reports success."

**The cost is not only correctness — it is the gates themselves.** A single
day's fail-open cluster was all shell: #7745 (resync exits 0 when its check
crashes), #7761 (`PreToolUse` guards `exit 0` = allow when absent), #7755
(lossy scan → silent ALLOW), #7743 (version gate answering the wrong
question). Each is a small, defensible shell decision; collectively they are a
set of gates that cannot distinguish "checked, fine" from "could not check".

**An experiential note.** While testing and committing a change *about* the
code-vs-data problem, the guard blocked **four** of the author's own inputs
for containing destructive strings as data: a `grep` pattern, two literals
inside a JSON test fixture, and the commit message documenting the test
result — which had to be supplied via `git commit -F`. That is the ADR's
thesis demonstrated on itself: the boundary cannot tell a command from a
string about a command, and no amount of masking has fixed it in 49 attempts.

### Caveat: porting does not fix flakiness

Porting does **not** fix flakiness, and claiming it will would weaken the
case. Of 25 flake issues in 60 days, roughly 8 are in code that is **already
Rust** — including one that flaked, was fixed (#7025), and flaked again
(#7307). The categorisation is in #7789. The shell-specific classes (SIGPIPE
under `pipefail`, process/pid handshakes) are about a third.

The honest framing is narrower and stronger: Rust removes
**interpreter-compatibility** and **quoting** defect classes outright, and
makes the rest *diagnosable*. It does not remove concurrency or wall-clock
flakiness.

## Decision

**Rust owns behavior. Shell exists only where it is necessary to reach that
behavior.**

### The boundary is responsibility, not line count

| | |
|---|---|
| **Shell** | Find the executable, report if it is missing, replace itself with that executable (`exec`). |
| **Rust** | Interpret arguments, inspect state, make decisions, invoke tools, handle failures, produce results. |

A 20-line shell script implementing retry policy is on the **wrong** side. A
longer shim handling unavoidable executable discovery may be on the **right**
side. `exec` matters beyond brevity: it avoids leaving a shell intermediary
that must forward signals.

`agent-spawn.sh` already demonstrates the target: 14 visible lines, no spawn
policy, ending in `exec "$DAEMON_BIN" agent-spawn "$@"`, plus 126 lines of
discovery helpers **shared across 30 scripts**.

### What justifies retaining shell

1. **Bootstrapping before Rust is available.** Something must download or
   build the executable; requiring the executable to install itself is
   circular.
2. **Existing invocation contracts.** Agents, CI and operators invoke specific
   `.sh` paths, sometimes explicitly through `bash`. Keeping the name as a
   wrapper avoids gratuitous compatibility work.
3. **Environment-specific executable discovery**, which must happen before the
   binary runs.

These are exceptions, not implementation domains. A hook needs an executable
entry point; it does not inherently need a shell implementation. If a hook can
invoke the binary directly, a shim may be unnecessary.

### Moving to Rust must mean more than translating Bash

The failure mode is **shell-shaped Rust**: everything stays a string, commands
are assembled through `sh -c`, functions pass raw stdout. The required shape
instead:

- Parse external data into typed structures at the boundary.
- Keep decisions in ordinary Rust functions.
- Invoke `git` / `gh` / `tmux` with explicit argument arrays.
- Distinguish command failure, malformed output, missing state, and legitimate
  empty results.
- Isolate external effects enough to test decisions without launching an
  environment.

This does **not** mean reimplementing Git or replacing mature CLIs with
libraries. It does **not** require a running daemon: a `loom-daemon`
subcommand may execute locally and exit. Sharing a binary must not imply
depending on the background service.

### The execution boundary, and what it must not overload

A raw process runner should **not** distinguish malformed JSON, missing domain
state, or a legitimate empty collection. Those belong above it. The defect is
**losing information before those layers can decide**.

| Layer | What it distinguishes |
|---|---|
| Process execution | could not spawn, timeout, wait failure, completed process |
| Process outcome | success, nonzero exit status, signal termination |
| Output interpretation | valid response, malformed response, unexpected shape |
| Domain operation | found, legitimately absent, empty collection, operation failure |

A nonzero exit is not universally an execution error — some commands use it
for an ordinary negative answer. Preserve it; let the adapter interpret it.

`stdout: String` is **not** inherently shell-shaped Rust. Silently
substituting an empty string when execution or decoding failed **is**.

#### Measured state of the current boundary

`loom-daemon/src/script_helpers/mod.rs`, `run_gh` / `run_git`:

```rust
Err(e) => GhResult { success: false, stdout: String::new(), stderr: e.to_string() }
```

```rust
fn from_output(out: &Output) -> Self {
    Self { success: out.status.success(),
           stdout: String::from_utf8_lossy(&out.stdout).to_string(), ... }
}
```

Four things collapse: **spawn failure vs. ran-and-failed** (both
`success:false, stdout:""`), **the exit code**, **signal termination**, and
**invalid UTF-8**. Duplicated verbatim in `run_git`.

Its doc comment states the provenance plainly: *"Port of
`validate_phase._run_gh` … exactly like the Python `check=False` +
captured-output contract … mirroring Python's `text=True`."* It is a
transliteration that preserved the source language's semantics.

**There is currently no structured-output consumer to migrate.** In the sole
consumer, `validate_phase.rs`:

| | |
|---|---|
| `serde_json::from_*` sites | **0** |
| `--json` invocations | **16** |
| `--jq` invocations | **16** |
| `success` + string-inspection sites | **37** |

Every structured call asks `gh` for JSON and immediately flattens it by
shelling out to `jq`, then string-matches. The recurring idiom
`if r.success && !r.trimmed_stdout().is_empty()` is how the code survives not
knowing whether empty means *fetch failed*, *no labels*, or *jq matched
nothing*. In `validate_curator`, `if !r.success` reports "Could not fetch
issue labels" for a missing `gh`, expired auth, **and** an issue that
genuinely has zero labels.

**Scope consequence:** the first slice must *create* the typed path, not
convert one. Blast radius is small — `run_gh` has exactly one consumer file.

### Compatibility contract (shim ↔ binary)

Version skew is a real, currently unguarded surface, and it already exists in
the mixed state: `agent-spawn.sh` assumes the resolved binary supports
`agent-spawn`; `loom_locate_daemon_bin` checks **presence only**.
`defaults/scripts/` installs at install time and is not refreshed by
`git pull` (#5874), while the binary self-updates on its own cadence.

**One local capability probe, answered by the executable, without contacting
the daemon or loading workspace state:**

```
loom-daemon supports agent-spawn.v1
```

- The caller uses **exit status**, not parsed help text or JSON.
- The capability names a supported **invocation/output contract** — not an
  issue number, commit, or mere presence of a command name.

The shared invocation helper: resolve the executable → check the required
capability → on failure name the selected path, explain the incompatibility or
the inability to verify it, and give upgrade guidance → `exec`.

An older binary lacking the probe must yield *"cannot verify compatibility /
upgrade required"*, **not** a misleading claim that a command is absent. The
helper translates that without understanding clap's error prose.

**Deployment order matters:** ship the probe and its implementation *before*
switching installed wrappers. The check protects against skew; it does not
substitute for sequencing.

Start with one extra local process invocation per command. **Measure before**
introducing caching and its invalidation problems. Ordinary invocation must
never auto-update anything.

### Enforcement: a template, not a theory of "substantive"

"No new substantive shell logic" is the right default rule, but CI cannot
evaluate *substantive* — and **grep is the wrong mechanism**, for the same
reason it fails elsewhere in this repo: it cannot reliably distinguish syntax
from comments, quoted strings, or heredocs. (That class has produced three
separate defects in recent work: #7718's raw-string dedent, #7741's
assignment inference, and a survey that miscounted by matching comment text.)

Make the wrapper **mechanically recognizable** instead:

- fixed entry-point template
- literal required capability and subcommand
- unchanged argument forwarding
- a call to one approved invocation helper
- no arbitrary extra statements

CI compares wrappers against the template, or generates them from a small
declarative manifest and checks for drift. Neither requires understanding
arbitrary shell.

Three categories, explicitly separated:

| category | treatment |
|---|---|
| **Finished wrappers** | template-enforced |
| **Shared invocation/bootstrap helpers** | narrowly scoped, separately reviewed and tested |
| **Legacy implementations** | tracked migration backlog — never falsely declared compliant |

The shared helper is **not** exempt because its lines amortize across callers;
its breadth makes its correctness *more* important. `locate-daemon-bin.sh` can
invoke `cargo metadata` — its latency and failure behavior need an explicit
contract.

### Sequencing

1. **Execution contract** — lossless subprocess results, exercised by two
   existing adapters.
2. **Compatibility contract** — capability probe, standard invocation helper,
   deployment ordering.
3. **One complete migration** — typed domain logic, CLI contract, thin
   wrapper, migrated tests, deleted shell implementation.
4. **Broader migration** — independent concerns on the established
   boundaries.

Steps 1 and 2 may proceed in parallel. Neither may become a months-long
prerequisite program.

> **The rule: do not let the migration multiply a known bad abstraction, and
> do not design its replacement without production consumers.**

Fix the boundary first, but prove it through **real existing call sites** —
not a standalone framework project.

## Consequences

### Positive

- The half-finished migration becomes stated policy with a mechanically
  checkable boundary.
- Script names remain stable indefinitely; only implementations move.
- Removes the lexing-in-shell class at its source for ported code.
- Typed failure modes replace `success && !empty`.

### Negative

- Creates a shim↔binary compatibility surface that does not exist today, and
  one extra local process invocation per command.
- Requires a toolchain where a script previously sufficed — must be confirmed
  against CI and dev-machine constraints.
- The shell test mass does **not** automatically disappear. Parsing and policy
  tests move to Rust; tests proving the old command names still work stay at
  the invocation boundary.
- Ports must preserve more than successful output: exit codes, stdout vs
  stderr, working-directory behavior, environment handling, signals and
  cleanup are all observable contracts. That argues for focused integration
  tests, not for keeping the implementation in shell.
- Porting does not remove concurrency or wall-clock flakiness (see "Caveat"
  above) — those classes need their own fixes regardless of language, and
  this ADR must not be cited as the remedy for them.

## Alternatives Considered

**Keep a substantial shell orchestration layer.** Rejected. "Shell is good at
process orchestration" is weaker than it sounds: orchestration involving
retries, timeouts, cleanup, concurrency, structured output or recovery is
application logic, and Rust represents those requirements far more
explicitly. `claude-wrapper.sh` (2,932 lines of retry policy) is the
counter-example in-tree.

**Enforce by line count.** Rejected: it would bless a 20-line retry policy
and flag a 130-line discovery shim. Responsibility is the correct axis.

**Enforce by grep/regex over script bodies.** Rejected: same lexical-matching
failure class documented elsewhere in this repo.

**Port scripts first, refactor the boundary later under pressure.** Rejected:
multiplies a known-bad abstraction across 50 call sites before it can be
changed.

**Build the boundary as a standalone framework first.** Rejected: designing
the replacement without production consumers. Hence "two existing adapters"
in step 1.

## References

- #7777 — this ADR's draft and discussion record (operator ruling 2026-09-16)
- #7810 — downstream migration epic
- #7762 — language policy (narrows to the default rule now that this is accepted)
- #7758 — re-deriving the irreducible bootstrap surface
- #7794 — `--help` split
- #7755 / #7760 — lexing-in-shell root cause and the tokenizer question
- #7795 — argues against deepening the guard (tokenizer) before the ask tier
  is sized; complementary to this, not in tension
- #7789 — the flake categorisation the caveat cites
- #7751 / #7783 — the bash-3.2 sweep
- #7765 — `worktree.sh` forge-blindness (invocation-contract fragility)
- #5874 — why `defaults/` changes need a VERSION bump (the skew mechanism)
- ADR-0013 — the prior "one Rust binary plus bash" retirement of the Python
  package; this ADR states where the bash side of that split ends
- ADR-0016 — prior art for bounded tokenization and fail-closed ambiguity
