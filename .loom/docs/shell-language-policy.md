# Shell language policy — new logic in Rust, new shell from a closed list

**The rule**: new executable logic goes into Rust as a `loom-daemon`
subcommand. A new `.sh` file is admissible only with a reason from the six
categories below, recorded in `scripts/shell-allowlist.txt`, and
`scripts/check-shell-allowlist.sh` fails CI when a tracked `.sh` exists that is
not listed there.

This is the narrowed, enforced form of
[ADR-0018](https://github.com/rjwalters/loom/blob/main/docs/adr/0018-rust-owns-behavior-shell-reaches-it.md)
("Rust owns behavior; shell exists only to reach it"), adopted by operator
ruling on issue #7762.

## What this is not

- **Not a rewrite mandate.** Nothing on the baseline list has to move. The
  ~500 scripts that exist today are recorded, not conscripted. Migration order
  is epic #7810's problem, one phase at a time.
- **Not a size limit.** How large an existing file may grow is a separate,
  already-built mechanism — the #7711 ratchet in
  [`file-size-policy.md`](file-size-policy.md) and
  `scripts/check-file-size-budget.sh`. This policy is unchanged by, and does
  not change, that one.
- **Not a ban on shell.** Five of the six categories remain open to new files.
  What is no longer possible is adding a `.sh` without saying which one it is.

## Why a forcing function rather than a paragraph

Shell is not legacy being wound down here — it is what gets written. Over the
90 days before #7762, 360 new `.sh` files landed (137 production, 223 test),
and each production script pulls roughly 1.6 test files after it, so the
language choice compounds.

The costs are measured, not asserted: about half of `guard-destructive-generic.sh`'s
commits in a year fix one bug class (code-vs-quoted-data confusion, with
reproduced silent ALLOWs — #7755); a recurring bash 3.2/4 portability class
breaks macOS dev machines (#7717, #7728, #7730, #7749, #7751); the largest
files in the repo are shell tests.

A prose-only policy would rot, and that is not a general worry — it is the
exact failure #7755 documents in this repo: a safety contract written as a
comment, silently violated the moment someone added a consumer. So the policy
gets the treatment `defaults/scripts/tests/ci-wired.txt` gets: a committed
list, a required reason per entry, and a check that fails on anything in
neither state.

## The six categories

These are #7758's categorical verdicts. Do **not** invent a second taxonomy.

| Category | Means | Available to a NEW file? |
|---|---|---|
| `bootstrap` | Runs before a `loom-daemon` binary is available, or after it is removed. Requiring the binary to install itself is circular. | Yes |
| `hook-entry` | A `PreToolUse` / `SessionStart` hook needs a shell-invocable command. The stub stays; the logic behind it is a port target. | Yes |
| `vendored` | The canonical copy lives in [rjwalters/repo](https://github.com/rjwalters/repo) and is re-vendored at release. Structural change goes upstream. | Yes |
| `stub` | Shape-A stub: under 40 code lines **and** its last code line is `exec`. This is the trivial-glue cap. | Yes |
| `test` | Shell because its subject is shell. Follows its subject — never an independent reason to keep either in shell. | Yes |
| `contract` | An existing invocation contract: the file's *name* is consumed by role prompts, CI workflows, hooks, or consumer repos. | **No** |

`contract` is baseline-only, and the checker enforces that structurally rather
than by convention: an entry claiming `contract` under `# @section new` fails.
"Something already calls it by name" cannot be true of a file that did not
exist yet.

`stub` is the one category cheap enough to be tempting, so it is the one the
checker verifies rather than takes on trust: a `stub` entry whose file is 40+
code lines, or whose last code line is not `exec`, fails. That is what keeps
the category from becoming the rubber stamp the whole list exists to prevent.

## The four objections, as settled

Issue #7762 named four judgment calls that had to be answered before adopting
the policy. The operator ruled on all four on 2026-09-16; they are decisions
now, not open questions.

**1. Where does the trivial-glue cap sit?** A 15-line wrapper is genuinely
faster to write in shell, and without an explicit cap the category becomes a
rubber stamp. The cap is the **shape-A stub**: under 40 code lines **and**
ending in `exec`. Both halves are required and both are machine-checked.
Anything larger, or anything that does not end in `exec`, is not glue — it
needs a category-1–5 reason or it is a `loom-daemon` subcommand. `exec` matters
beyond brevity: it leaves no shell intermediary that has to forward signals.

**2. How far does the policy reach?** Its ceiling is bounded by **#7795's
decision-log data** (which guard sites steer toward a safe alternative versus
stall a headless run), **not** by #7760 — #7760 is closed as superseded. The
vendored guard suite stays `vendored` until upstream changes it; that does not
cap this policy. Shell tests follow their subject, so the 223-of-360 test share
is a consequence of categories 1–3, never an independent argument.

**3. Does CI or a dev machine gain a build dependency?** No. The allowlist
check is a shell/grep scan over the tree — it lists every `.sh` and requires
each to be listed with a reason. It needs no `cargo`, no build, and no
`loom-daemon` binary, which is also why the checker itself is legitimately
shell (category `bootstrap`). No workflow gains a toolchain dependency from
this policy.

**4. Does it apply to `defaults/scripts/`?** Yes — that directory is exactly
the `contract` category, and `contract` is **not an exemption**. Those files
ship into consumer repos where a rebuild is not always possible, so the *name*
stays. New logic behind the name still goes into `loom-daemon`, and the file
trends toward a shape-A stub. `contract` is the fifth valid reason for an
*existing* `.sh`; it is never a reason for a new one.

## Scope: what the allowlist covers, and what it deliberately does not

The checked set is `git ls-files '*.sh'`, minus `.loom/**` and build output
(`target/`, `node_modules/`, `dist/`).

- **`.loom/` is excluded because it is an install mirror, not a source.**
  `.loom/scripts` is a symlink to `defaults/scripts` in this repo and a resync
  copy in a consumer repo; `.loom/hooks/*.sh` is a tracked resync copy of
  `defaults/hooks/*.sh`, kept byte-identical by
  `scripts/check-hooks-defaults-parity.sh`. Listing either would double-count
  every entry against its `defaults/` source and churn the manifest on every
  `chore: resync installed Loom surfaces` commit. This is the same exemption
  `check-file-size-budget.sh` already makes, for the same reason.
- **Untracked files are out of scope**, because CI checks out a commit —
  anything that can reach `main` is tracked by the time the gate sees it.
- **`git ls-files` rather than `find`**, so a stray build artifact or a local
  scratch script never fails someone else's PR.

## Adding a new `.sh`

1. First ask whether it is a `loom-daemon` subcommand. The friction is one
   match arm in `main.rs` plus a module; `script_helpers` already provides
   `run_gh` (with caching), `run_git`, `log_*`, atomic `write_json_file` and
   `now_iso`, so the hand-rolled per-script versions do not have to be written
   again.
2. If it genuinely must be shell, append it under `# @section new` in
   `scripts/shell-allowlist.txt`:

   ```
   <repo-relative path>  <category>  <one-line reason>
   ```

3. Run `bash scripts/check-shell-allowlist.sh` before pushing.

There is intentionally no `--update` / `--fix` mode. Regenerating the manifest
from the tree would make every unlisted file self-justifying, which is the
whole failure the gate exists to prevent — the reason has to be typed by
whoever adds the file.

## Commands

```bash
bash scripts/check-shell-allowlist.sh              # the gate CI runs
bash scripts/check-shell-allowlist.sh --list       # every in-scope .sh + category
bash scripts/check-shell-allowlist.sh --self-test  # verify the gate still works
bash scripts/check-shell-allowlist.sh --help
```

CI runs the self-test and then the gate in the `Shell Allowlist` job on every
PR, unfiltered by path — a new `.sh` in a directory nobody expected is exactly
the case it exists to catch (the same reasoning as the conflict-marker and
shell-syntax jobs, see [`ci-principles.md`](ci-principles.md)). The checker is
also self-tested on the macOS leg of the `Shell Syntax` job, which asserts its
`bash` really is 3.x, so the gate that guards against the bash 3.2 class is
itself exercised under bash 3.2.

## The aggregate ratchet (#8084)

The allowlist gates each NEW script. It does not measure total volume, and
volume is where the growth actually was:

| when | files | production shell code lines |
|---|---|---|
| 90 days ago | 93 | 14,958 |
| 60 days ago | 96 | 15,568 |
| 30 days ago | 186 | 40,735 |
| now | 236 | 52,493 |

Of the lines added since the 60-day mark, 29,795 arrived as 148 brand-new
scripts — nearly all individually under the per-file threshold, each with a
defensible allowlist reason.

### It measures the pool being retired, not the total

Total production shell cannot reach zero and was never meant to. A quarter of
it is shell that must stay shell:

| category | lines | why it is permanent |
|---|---|---|
| `bootstrap` | 8,658 | runs before a `loom-daemon` binary exists, or after it is removed; requiring the binary to install itself is circular |
| `vendored` | 4,999 | owned upstream in rjwalters/repo; its structure is not ours to change |

Ratcheting "what we are retiring" plus "what we are keeping" gives a figure
with no target, so no run of it says how far along the epic is. What the epic
retires is the **portable** pool — `contract` + `hook-entry`, whose logic moves
into the daemon behind a stub ("The name stays; logic ports behind it"):

| category | lines | files |
|---|---|---|
| `contract` | 36,296 | 188 |
| `hook-entry` | 2,395 | 8 |
| **portable** | **38,691** | **196** |

Its target is the ~145 lines of `stub` glue a ported file leaves behind. That
is what the ratchet gates. `total` is ratcheted as a secondary signal so floor
growth is visible rather than free, but the floor is not debt.

### Where it runs, and why that took two attempts

```bash
loom-daemon shell-budget           # the progress report
loom-daemon shell-budget --json    # the same, for scripting
loom-daemon shell-budget --check   # report + exit 1 when over budget (what CI runs)
```

The `Shell Budget Ratchet` job runs unconditionally, like every other
structural ratchet. The first cut lived inside `Rust Unit Tests`, which is
gated on the `backend` paths filter — `loom-daemon/**` but **not**
`scripts/**` or `defaults/**`. A PR that added a shell script skipped the job
entirely and the ratchet first fired on the push to `main`. A gate skipped on
exactly the changes it targets is not a gate; see
[`ci-principles.md`](ci-principles.md), path-filtering is an optimisation, not
a correctness tool.

The report prints on every run, pass or fail. A gate that only speaks on
failure teaches nobody which way the number is moving — which is how the
portable pool grew **+317 since the epic's first port commit**, across four
merged ports, with nobody noticing.

It is Rust, not a script, for the obvious reason: a `.sh` enforcing "stop
adding shell" would have to exempt itself from its own count.

### There is no baseline to regenerate

The gate compares against the **merge-base**, not a committed number. It asks
the only question it cares about — *does this change add portable shell?* —
so whatever `main` did meanwhile is not this change's doing and not the gate's
business.

That is a deliberate correction. The first version committed an absolute number
and required regenerating it whenever `main` moved. A number in the tree is a
snapshot of one tree, so every other tree disagrees with it: this file's own
baseline went stale twice in a single day, and the identically-shaped role-prompt
ratchet **failed at its own merge commit** and red-lined `main` for 8 consecutive
commits (#8073, #8105).

The deeper problem was the instruction it produced. "Regenerate the baseline on
every merge" teaches people to regenerate without looking — which is exactly the
reflex a ratchet exists to prevent. The gate was training the behaviour it was
built to stop.

`scripts/shell-budget-baseline.txt` now holds one value, `origin_portable`, and
it is the progress **denominator**, not a gate input. It must never be
regenerated: an origin that moves measures nothing. A test pins it.

## Related

- [ADR-0018](https://github.com/rjwalters/loom/blob/main/docs/adr/0018-rust-owns-behavior-shell-reaches-it.md) — the accepted architectural decision this narrows.
- #7810 — the migration epic for shell that already exists.
- #7758 — the categorical verdicts used as this taxonomy, and the irreducible bootstrap core.
- [`file-size-policy.md`](file-size-policy.md) — the #7711 ratchet on existing files, unchanged by this policy.
- #8084 — the aggregate shell-volume ratchet above, the companion to this per-file allowlist.
