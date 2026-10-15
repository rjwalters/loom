# Shell language policy — new logic in Rust

**The rule**: new executable logic goes into Rust as a `loom-daemon`
subcommand, not into a new `.sh` file. Shell remains the right tool for
reaching the daemon (a thin stub that `exec`s it), for bootstrap code that has
to run before any binary exists, for vendored upstream files, and for tests of
shell that already exists.

This is
[ADR-0018](https://github.com/rjwalters/loom/blob/main/docs/adr/0018-rust-owns-behavior-shell-reaches-it.md)
("Rust owns behavior; shell exists only to reach it"), adopted by operator
ruling on issue #7762.

## Enforcement

The policy is a review standard, not a CI gate. The per-file allowlist
(`scripts/shell-allowlist.txt` + `check-shell-allowlist.sh`, #7762) and the
aggregate shell-volume ratchet (`loom-daemon shell-budget --check`, #8084)
were removed from CI by operator ruling on #12106: they cost every PR a ledger
edit and a stale-check input without catching what review would not. Judge
applies the rule when a PR adds a `.sh`: the PR should say why the logic is not
a daemon subcommand.

## What this is not

- **Not a rewrite mandate.** Existing scripts do not have to move. Migration
  order is epic #7810's problem, one phase at a time.
- **Not a size limit.** File size is not gated (#12099); Hermit may propose
  splitting a very large file.
- **Not a ban on shell.** Stubs, bootstrap, vendored files and tests of
  existing shell are all legitimate.

## Why

Over the 90 days before #7762, 360 new `.sh` files landed (137 production,
223 test), and each production script pulled roughly 1.6 test files after it.
About half of `guard-destructive-generic.sh`'s commits in a year fixed one bug
class (code-vs-quoted-data confusion, with reproduced silent ALLOWs, #7755),
and a recurring bash 3.2/4 portability class broke macOS dev machines (#7717,
#7728, #7730, #7749, #7751).

## `defaults/scripts/`

Those files ship into consumer repos where a rebuild is not always possible,
so their *names* are contract and stay. New logic behind a name still goes
into `loom-daemon`, and the file trends toward a thin stub ending in `exec`.

## Related

- [ADR-0018](https://github.com/rjwalters/loom/blob/main/docs/adr/0018-rust-owns-behavior-shell-reaches-it.md) — the accepted architectural decision.
- #7810 — the migration epic for shell that already exists.
- #12106 — removal of the allowlist and shell-budget CI gates.
