# ADR-0016: Write-Target Confinement Design — Bounded Tokenization Plus Reused Same-Command Literal Declaration, No Control-Flow Inference

## Status

Accepted (design decision for Epic #6172 Phase 1, issue #6235). Implementation
is deliberately deferred to a Phase 2 follow-on issue — see "Consequences →
Follow-on: Phase 2 implementation" below. No guard code changes ship in this
ADR's PR.

## Context

`defaults/hooks/guard-destructive-generic.sh`'s Bash-tool write-confinement
check (worktree isolation, #4178) fails closed whenever a write target's root
path component is an unresolved shell variable (`$VAR/rest`) — the "root
unknown" branch documented in `.loom/docs/guard-hooks.md` § "Unresolvable
`$…` targets fail closed" (#4921). That default is correct, but it is also
real, recurring friction: #6172 cites a live 2026-08-13 denial of
`sed -i '' ... "$S/om-installed.sh"` from an ordinary interactive session as
one representative case.

PR #5397 attempted to relieve that friction with a narrow carve-out: infer
that `$VAR` is safely bound to a small literal set of values when the raw
command text contains an enclosing `for VAR in tok1 tok2; do`. Judge found
**three sequential, independently-confirmed bypasses** in that single helper
(`_wt_scan_forloop_binding()`), each a distinct defect class:

1. **Position/reassignment-unawareness** — the binding check never verified
   the write's own occurrence was textually inside the matched loop's body,
   so a throwaway loop anywhere in the command (even in dead code, even
   *after* the write) "bound" an unrelated, unresolvably-reassigned variable.
2. **Decoy-reference** — after (1) was fixed to require *some* reference to
   `$VAR` inside the loop body, a single unrelated mention (`echo "$p"`, a
   `touch` using it) satisfied that check while the real write, using a value
   reassigned via `$(...)`, sailed through unverified.
3. **Literal-substring `done`-match** — the loop-body span was computed with
   `${after_do%%done*}` / `${after_do#*done}`, a plain substring split on the
   four characters `d`,`o`,`n`,`e` with no keyword boundary. Any identifier
   merely *containing* "done" (`is_done=1`) truncated the body early, letting
   a same-body reassignment escape the reassignment check entirely.

Champion's verdict on the PR, after the third rejection, is the load-bearing
diagnosis this ADR must not repeat:

> Three consecutive rounds each produced a *new* bypass class in the same
> function, all stemming from the same root cause — ad-hoc substring/regex
> parsing of shell loop bodies without real positional or keyword awareness.
> This is the 'defect list migrating around the same area with no end in
> sight' pattern, not a converging bug chain.

The operator closed PR #5397 as not viable (2026-08-14) and filed epic #6172
for a redesign, explicitly requiring a design decision **before** any
implementation (this issue, #6235, Phase 1). #6172 floats two candidate
approaches: (A) static analysis with real positional/keyword awareness (a
proper shell tokenizer, not substring matching), or (B) an explicit
declaration mechanism — "a sanctioned prelude that declares a resolved,
literal write root the guard can then verify cheaply... that may be sounder
than any amount of shell parsing." It also flags two adjacent friction
reports to fold in or explicitly scope out (#6123, #6110), and a fresh
2026-08-14 false-positive datapoint (a `sed -i` argument-position mis-parse)
to cover in the redesign's test matrix.

### What already exists in the guard, verified directly

Before choosing an approach, this issue's investigation established two facts
about the *current* guard (verified against `origin/main` @ `06df09c8`,
2026-08-14) that materially change the design space:

**1. A sound same-command literal-assignment resolver already exists and is
already fail-closed on every ambiguity class #6172 asks about.**
`_VARRESOLVE_AWK`'s `record_assign()` / `resolve_var()`
(`guard-destructive-generic.sh` ~lines 1608–1686, added for #4881, shared with
`parse_force_ops()` per #6152) resolves a `$NAME`/`${NAME}` write-target token
against a same-command `NAME=literal-value` assignment found earlier in the
same raw command string — no loop/control-flow reasoning at all, just a
last-token-wins map with a poison value on conflict. Directly verified against
the live hook:

| Command | Verdict | Why |
|---|---|---|
| `p=/tmp/outside/x; echo pwned > $p/f.txt` | ALLOW | literal same-command assignment resolves to a target outside the checkout |
| `p=<main-checkout>/f.txt; echo pwned > $p` | DENY, names the real resolved path | literal assignment resolves *inside* the checkout — correctly denied, not merely "resolved" |
| `p=/tmp/a; p=/tmp/b; echo pwned > $p/f.txt` | DENY (unresolved) | conflicting assignment to the same name poisons to `AMBIG`, per `record_assign()`'s own contract |
| `p=$(cat /tmp/f); echo pwned > $p/f.txt` | DENY (unresolved) | assigned value itself starts with `$` (unresolved by this single-pass resolver) — `resolve_var()` refuses to guess |
| `p=$OTHER; echo pwned > $p/f.txt` | DENY (unresolved) | chained-variable RHS, same refusal |
| `echo pwned > $p/f.txt` (no assignment) | DENY (unresolved) | baseline #4921 behavior, unchanged |

This means Option A is **not** a green field — the guard already contains a
bounded, provably fail-closed static-analysis primitive for exactly the
"resolve a variable from surrounding text" problem. What #5397 added on top of
it (`_wt_scan_forloop_binding()`) was a *categorically different* kind of
inference — inferring a *bounded set* of possible values from *control-flow
membership* (is this write inside this loop's body?) rather than a *single*
value from an *unconditional* assignment. That distinction is the whole story
of why #5397 failed three times in a row: "is this occurrence lexically inside
that block, considering nesting, dead branches, and textual order" is a
genuinely hard parsing problem that a hand-rolled scanner kept getting wrong
in new ways; "did this exact name get assigned a plain literal earlier in this
flat token stream, with no branch/loop reasoning required" is not.

**2. A live, previously unreported unsound false-negative was found and
root-caused in the course of investigating the 2026-08-14 sed false positive
(see "Sed / argument-position false positive" below) — it silently ALLOWS a
write that should deny, not merely mis-names one.** This matters for scope:
the redesign's test matrix must close an active bypass, not just a
misleading-diagnostic annoyance.

## Decision

**Adopt a hybrid design, but a narrowly-scoped one: (1) keep and audit the
existing bounded, per-write-idiom tokenizer (a restricted form of Option A —
argument-*position* awareness only, never value inference across control
flow), and (2) formalize the existing same-command literal-assignment
resolver (`record_assign()`/`resolve_var()`, #4881) as the one sanctioned
declaration mechanism (Option B) for converting an otherwise-unresolvable
`$VAR`-rooted write target into a known one. Retire — and never reintroduce —
any inference that tries to determine a variable's value from its
relationship to surrounding control-flow structures (loops, conditionals,
case statements, function bodies). That category, not "insufficiently precise
parsing," is what made #5397 unsound.**

### Why each half is sound

**Bounded tokenization (Option A, restricted).** The write-confinement scan
already works idiom-by-idiom: for a small, fixed, enumerable set of write
shapes (`>`, `>>`, `tee`, `sed -i` in both GNU-attached and BSD-separate `-i`
forms, `cp`, `mv`), a dedicated per-idiom classifier decides which
already-quote-delimited shell word is the destination-path operand versus an
option-argument (like sed's script). This is real positional awareness — it
is not the substring/regex matching that produced #5397's bypasses — but it
is deliberately **not** a general shell parser: it never asks "what is this
variable's value," only "which token, among this fixed idiom's arguments, is
the path operand." That question has a small, closed, auditable answer per
idiom (already partly implemented — e.g. #5674's BSD `-i` empty-suffix
handling), unlike "what does this variable evaluate to," which is
undecidable in general (command substitution, `read`, prior process state).
Bounding Option A to this question, and never letting it grow into inferring
values from control flow, is what keeps it sound.

**Same-command literal declaration (Option B, reusing #4881).** Rather than
inventing a new pseudo-syntax (`# loom:write-root <path>` or a bespoke
`LOOM_WRITE_ROOT=` convention), the sanctioned declaration mechanism is
**ordinary shell assignment syntax the guard already resolves soundly**:
`VAR=/absolute/literal/path; <write using $VAR>` in the same Bash tool call.
This is preferable to a bespoke annotation for three reasons:

1. It is real, unambiguous shell semantics with a single well-defined
   meaning — there is no separate "does this annotation actually govern this
   write" inference to get wrong, which is exactly the class of bug that sank
   #5397 (inferring whether a *construct* governs a *specific occurrence*).
   A `NAME=value` prefix's scope is not something the guard infers; it is
   something bash itself defines, and the guard's resolver already matches
   that definition (same-command, unconditional, last-flat-token-wins with
   poison-on-conflict).
2. **The declaration can never grant an allow beyond what typing the literal
   path directly would already grant.** After resolution, the *exact same*
   "does this resolved absolute path land inside the main checkout while a
   managed worktree exists" containment test runs — the one already used for
   every literal-path write today. A declaration of `p=<main-checkout>/evil`
   still denies (confirmed in the table above: a literal assignment resolving
   *inside* the checkout is still denied, not silently trusted). This is the
   crux of the soundness argument: **the mechanism only removes
   unresolvability; it never weakens containment.** Even an agent that
   declares a false or self-serving value gains nothing beyond what writing
   the literal path outright would already have granted — consistent with
   the guard's documented threat model (`.loom/docs/guard-hooks.md` § "What
   the floor is not": a blast-radius limiter on mistakes and injected
   instructions, not a sandbox against a fully adversarial operator with
   shell access).
3. It requires **no new denial-relevant code** — `record_assign()`/
   `resolve_var()` is already shipped, already exercises the ambiguity
   contract below, and Phase 2's job is mostly audit, tests, and
   documentation/discoverability (teaching the deny message to point at this
   pattern), not new parsing logic. A smaller, more auditable surface change
   is a better fit for "sound, not just more coverage" than adding a second,
   parallel resolution mechanism would be.

### What this explicitly does NOT do

No further static inference of a variable's *value* from its relationship to
surrounding shell control flow is attempted, ever: no `for`-loop body
scanning, no `if`/`case` branch reachability reasoning, no tracking a
variable across a function boundary, no reasoning about what a loop's Nth
iteration would bind. This is not a weaker version of #5397's approach — it
is the deliberate absence of that entire category of logic. The category
itself, not any one implementation of it, is what produced three
independently-discovered bypasses in three review rounds; removing the
category removes the risk at its source rather than patching its next
manifestation.

## Ambiguity behavior (fail-closed, no exceptions)

| Ambiguity | Behavior | Basis |
|---|---|---|
| Nested loops, loop-bound variables at all, shadowed loop names, multiple bindings via a loop construct | **Deny.** No loop-based binding inference exists in this design — never reintroduced. A bare `$VAR` write target with no same-command *literal assignment* is unresolved regardless of any surrounding loop. | Structural: the inference category that would resolve this does not exist. |
| Unresolvable reassignment: command substitution (`$(...)`, backticks), `read`, a chained unresolved `$OTHER` | **Deny.** `resolve_var()` returns the token unchanged when the mapped value itself is not a plain literal (starts with `$`, or was never captured because `read`/pipelines don't produce a `NAME=value` token at all). | Verified directly (table above); Phase 2 adds this as a *named, tested* contract rather than an implicit one. |
| Multiple/conflicting same-name assignments in one command | **Deny.** `record_assign()` poisons to `AMBIG` on the second differing assignment to the same name. | Verified directly (table above). |
| No assignment found for the referenced name | **Deny.** Baseline #4921 behavior, unchanged. | Verified directly (table above). |
| Anything the bounded per-idiom tokenizer cannot classify (unrecognized command shape, unterminated/unbalanced quote, an idiom wrapped in a pipe/subshell the extractor does not specifically model) | **Deny**, via the existing fallback: an unclassified/unresolved token is emitted raw and cwd-prefixed into a candidate absolute path, which is then judged by the same containment test as every other target. "I don't understand this command" never falls through to an allow. | Existing #4921 fallback contract, unchanged by this design. |

Fail-closed here is not a separate policy layer bolted on top of the
mechanism — it falls out of the mechanism's own shape: there is no
ambiguity-resolution code path left that could have a soundness bug, because
the class of logic that previously tried to resolve ambiguity (control-flow
inference) is not present at all.

## Sed / argument-position false positive (2026-08-14)

Root-caused directly against `origin/main` @ `06df09c8` (2026-08-14), not
merely reasoned about:

```bash
sed -i '' 's/x/y #958/' $SP/file.md
```

silently **ALLOWS** (verified: exit 0, no `hookSpecificOutput` at all) even
though `$SP/file.md` is exactly the fail-closed shape #4921 exists to catch.
The originally reported command (`sed -i '' 's/\*\*Blocked by 3a\*\*
(per-block em-export/**Blocked by #958** (3a: per-block em-export/'
$SP/issue-3b.md`) reproduces the same underlying defect.

**Root cause.** `COMMAND_NO_COMMENT` (`guard-destructive-generic.sh` ~line
3855) strips `#…end-of-line` whenever a `#` is preceded by whitespace or
starts the line — **quote-unaware by its own header comment**, which
explicitly accepts that tradeoff on the stated assumption that the stripped
copy is "used only for the *narrowing* ASK/DDL matches... the worst case is a
missed ask on quoted data, never a missed catastrophic block." That
assumption no longer holds: `COMMAND_ASK_SCAN` (built from
`COMMAND_NO_COMMENT`, ~line 3890) is also the exact input
`extract_write_targets()` scans to compute the worktree-write-confinement
**deny** (`WRITE_TARGETS=$(extract_write_targets "$COMMAND_ASK_SCAN" "$CWD" |
head -20)`, ~line 5161) — which is not an ASK-tier check, it is the hard-deny
path #4178/#4921 depend on. A `#` inside *any* quoted argument reachable by
the write-confinement scan — a sed script, a `--body`/`-m` prose string, a
markdown heading being copied, a PR/issue reference like `#958` — truncates
the scanned text at that point, and everything textually after it (up to and
including the real write-target operand) silently vanishes from the scan.
Depending on where in the argument list the `#` lands, the observable failure
is either:

- the target is removed from the scan entirely → **silent ALLOW** (the case
  reproduced above, and the more dangerous of the two — it is not merely
  confusing, it is an active unsound bypass distinct from anything #5397
  touched), or
- an earlier fragment is still visible and gets misidentified as the target →
  a **deny naming the wrong text**, the symptom in the original 2026-08-14
  operator report.

Both are the same defect surfacing two ways depending on truncation position,
not two separate bugs.

**Required Phase 2 test-matrix coverage** (both directions, so a fix cannot
silently regress into either failure mode):

1. `sed -i '' 's/x/y #958/' $SP/file.md`, `$SP` resolving inside the
   checkout → must **deny**, naming `$SP/file.md` (or its resolved
   candidate), never a sed-script fragment.
2. The exact originally-reported repro → same requirement.
3. A `#` inside a quoted `--body`/`-m`/`--title` argument, followed later in
   the same command by an unrelated `>`/`tee`/`cp` write → the later write
   target must still be scanned and correctly named (proves the fix isn't
   sed-specific).
4. A literal, non-main-checkout `#`-containing write (e.g. `sed -i ''
   's/x/y #z/' /tmp/scratch.md`) → must still **allow** — the fix must not
   turn every `#`-bearing sed command into a deny.
5. A genuine end-of-line shell comment with no attached write idiom (`echo hi
   # this really is a comment`) → unaffected; regression guard on the
   ASK/DDL tier's existing, correctly-scoped behavior.

**Fix shape for Phase 2** (design-level; not implemented in this PR): stop
routing `extract_write_targets()`'s input through the comment-stripped
`COMMAND_ASK_SCAN`/`COMMAND_NO_COMMENT` chain. Either (a) give the
write-confinement scan its own working copy, built the same way
`COMMAND_ASK_SCAN` is except *without* the `#`-stripping step (it still needs,
and already gets independently, the heredoc/`mask_gt()` masking
`COMMAND_ASK_SCAN` also carries), or (b) make the `#`-stripper itself
quote-aware, using the same quote-state walker the file already maintains in
three other places (`mask_gt()`, `strip_target_quoting()`,
`mark_expandable_dollars()`), so quoted data is never mistaken for a comment
by *any* consumer, not just this one. (b) is likely preferable long-term — it
also closes the ASK/DDL tier's own currently-accepted "missed ask on quoted
data" gap — but either resolves this specific defect. Phase 2 picks one and
justifies the choice against the existing quote-scanner infrastructure rather
than adding a fourth, independent masking pass.

## #6123 and #6110 status

Both are **explicitly scoped out** of this redesign's Phase 2, with reason —
neither is a variable-resolution/ambiguity problem, and this design's
soundness argument specifically forbids it from changing their outcome.

**#6123** (gitignored build-artifact writes denied, e.g. `mkdir -p dist && cp
target/release/loom-daemon dist/loom-daemon-x86_64-unknown-linux-gnu`): the
write target in that report is a fully literal, already-resolved relative
path (`dist/...`) — never variable-rooted, never unresolvable. This
redesign's declaration mechanism converts an *unresolvable* target into a
*resolved* one; by the soundness argument above, it explicitly cannot and
must not change the verdict for a target that is already resolved and lands
inside the main checkout while a worktree exists. #6123 is therefore a pure
denial-floor / gitignore-awareness **policy** question — precisely the one
`guard-destructive-generic.sh`'s own `#5315` DECISION note and Champion's
NEEDS REVISION verdict on #6123 already identified and deliberately
deferred — not a soundness/ambiguity defect this redesign is chartered to
fix. It was closed "consolidated into #6172, not rejected" (2026-08-14) with
no PR; that friction remains genuinely open, but not as a Phase 2 AC. If the
operator wants it solved, it needs its own explicit policy decision issue
(narrowing the "ungated denial floor" is not a call a Builder should make
unilaterally, per `CLAUDE.md` § guard hooks) — named below as a candidate
future issue, not folded into Phase 2. The existing `guards.worktreeIsolation:
false` session-wide escape hatch (made discoverable by PR #6143) remains the
sanctioned interim workaround, exactly as #6123's own Curator enhancement
already concluded.

**#6110** (no sanctioned write path for interactive main-checkout sessions):
closed via PR #6143, which fixed deny-message discoverability of the
*existing* `guards.worktreeIsolation`/`LOOM_GUARD_WORKTREE_ISOLATION` escape
hatch and explicitly excluded reopening "the deferred #5315/#6123
denial-floor question." The underlying friction it left open — no scoped,
*per-write* affordance, only a blanket session-wide toggle — is structurally
the same question as #6123 (a literal, already-known write target
intentionally targeting the main checkout), and out of this redesign's scope
for the identical reason: this redesign only removes *ambiguity*, never
*expands what a known target may do*. #6172's own body speculated that a
declaration mechanism "would serve #6123 and #6110 too" — investigated
directly here and found **not** to apply: the declaration mechanism's
soundness rests specifically on never changing the verdict for an
already-known target, which is exactly the verdict #6110 would need changed.
**Judged not resolved by this design.** The existing session-wide escape
hatch stays the sanctioned path. A more granular, per-write escape hatch for
literal main-checkout writes, if wanted, is a distinct policy decision
requiring explicit human sign-off (same rationale as #6123) — named as a
candidate future issue below, not a Phase 2 AC.

## Consequences

### Positive

- Eliminates the entire bug category that produced #5397's three sequential
  bypasses (control-flow-scoped value inference) by never reintroducing it,
  rather than attempting a fourth, more careful patch of the same shape.
- Requires effectively no new denial-relevant logic for the declaration half
  — `record_assign()`/`resolve_var()` (#4881) is already shipped and already
  verified fail-closed on every ambiguity class this ADR enumerates. Phase 2
  is mostly audit, tests, and discoverability, which is a smaller, more
  reviewable surface than new parsing code.
- Directly closes an *active*, previously unreported unsound false-negative
  (the `COMMAND_NO_COMMENT` quote-unawareness bug) discovered in the course
  of this investigation — this was not merely a misleading-diagnostic
  annoyance, it was a silent guard bypass independent of #5397's class.
- Gives Builders/operators a concrete, teachable workaround for the
  unresolved-`$VAR` friction (`VAR=/literal/path; <write>` in the same
  command) instead of only "spell the whole path out literally," without
  weakening the guard.
- Keeps the guard's implementation bounded and auditable: a small, enumerable
  set of write idioms with per-idiom argument-position rules, plus one
  general-purpose, already-tested literal-assignment resolver — no shell
  AST, no new external dependency, no growth in the hook's runtime cost
  profile.

### Negative

- Does not resolve #6123 or #6110's residual friction (writing a
  *known, literal* main-checkout target, or a gitignored build artifact) —
  both remain gated behind the existing session-wide
  `guards.worktreeIsolation:false` escape hatch, with no more granular
  affordance than exists today. Operators hitting that friction still pay
  the same cost as before this ADR; only the unresolved-`$VAR` friction is
  addressed.
- The same-command scope of `record_assign()`/`resolve_var()` means a
  variable exported or assigned in an *earlier*, separate Bash tool call
  (not the same command string) still cannot be resolved — each
  `PreToolUse` hook invocation only sees the current command's text, never
  prior shell state. This is a structural limit of a stateless
  per-invocation hook, not something this design (or any purely
  text-scanning design) can close; the workaround remains re-declaring the
  literal value in the same command as the write.
- The Phase 2 audit may find that the `COMMAND_NO_COMMENT` defect (or its
  fix) affects other write idioms beyond `sed`, since all of them share the
  same `COMMAND_ASK_SCAN` input — this ADR flags that as likely but does not
  pre-verify it for every idiom; Phase 2's test matrix must check each one.

### Follow-on: Phase 2 implementation

Concrete enough to file directly once this design is agreed:

1. **Formalize the ambiguity contract with tests.** Add named regression
   cases in `tests/hooks/test-guard-destructive.sh` pinning
   `record_assign()`/`resolve_var()`'s already-correct behavior for: (a)
   conflicting same-name assignment (`AMBIG`), (b) unresolvable RHS
   (`$(...)`, backticks, chained `$VAR`, no assignment produced by `read`),
   and (c) no assignment found. These are implicit in the code today; make
   them an explicit, citable contract per this ADR's ambiguity table.
2. **Confirm no control-flow-scoped inference exists on `main`.** #5397 was
   never merged, so this is expected to be a no-op confirmation rather than a
   deletion — verify at Phase 2 start against the then-current `main`, not
   this ADR's snapshot.
3. **Teach the declaration pattern.** Update the unresolved-`$VAR` deny
   message(s) in `guard-destructive-generic.sh` to name the same-command
   `VAR=/literal/path; <write>` workaround explicitly (mirroring how PR
   #6143 taught the `guards.worktreeIsolation` escape hatch), and add a
   `.loom/docs/guard-hooks.md` subsection documenting the pattern, its
   soundness argument (declaration only removes ambiguity, never weakens
   containment), and the ambiguity table above.
4. **Fix the `COMMAND_NO_COMMENT`/`COMMAND_ASK_SCAN` quote-unawareness bug**
   feeding `extract_write_targets()`, per "Sed / argument-position false
   positive" above, with its 5-case test matrix as the acceptance bar.
5. **Audit every other write idiom** (`>`, `>>`, `tee`, `cp`, `mv`) sharing
   `COMMAND_ASK_SCAN` for the same class of defect — the sed case is the
   *reported* instance, but the shared input plausibly affects all of them.
   Add matching regression cases for whichever are confirmed affected.
6. **Add permanent regression coverage for all three #5397 repro shapes**
   (position/reassignment, decoy-reference, substring-`done`-match) as
   standing DENY assertions — not because the vulnerable code exists on
   `main` today (it does not; #5397 was never merged), but as a guard
   against ever reintroducing any form of control-flow-scoped binding
   inference in the future, in this guard or a lookalike added elsewhere.
7. **Explicitly out of scope** (see "#6123 and #6110 status" above): any
   change to whether an already-resolved literal path inside the main
   checkout is denied. That verdict stays exactly as strict as it is today.
   If a per-write escape hatch for literal main-checkout writes (serving
   #6123/#6110's residual friction) is wanted, file it as a **separate**,
   explicitly policy-scoped issue requiring human sign-off — do not fold it
   into Phase 2's acceptance criteria.

Non-goals, restated for Phase 2's own scope discipline: no shell AST or
general parser, no control-flow inference of any kind, no change to the
main-checkout denial floor for already-known targets.

## Alternatives Considered

- **Pure Option A — a general shell parser/tokenizer with real control-flow
  awareness** (embedding or shelling out to a full bash-grammar library).
  Rejected: even a mathematically correct *parser* only recovers syntax, not
  runtime *values*. #5397's three bypasses were not failures to recognize a
  `for` loop's syntactic boundaries — by the third iteration the boundary
  detection was in fact correct — they were failures to soundly infer what a
  variable would *evaluate to* from surrounding non-executed text, which
  remains unsound regardless of parser precision: a shell's runtime value is
  data-dependent (command substitution, environment, prior process history)
  and not recoverable from static text in the general case. Also
  operationally heavy: vendoring a full shell-grammar parser into a bash
  guard hook (today's only external dependencies are `jq`/`python3`/GNU
  coreutils) is a materially larger, slower, harder-to-audit surface for a
  hook that must stay cheap and fail-open on missing tooling.
- **A curated allowlist of "trusted" text shapes** (e.g. re-attempting the
  `for`-loop idiom with yet more careful boundary/reassignment checks).
  Rejected for the reason #5397 itself was rejected: any rule expressed as
  "this TEXT SHAPE implies that VALUE" reintroduces the same
  value-inference-from-text problem with a different shape to get subtly
  wrong next time — Champion's "defect list migrating around the same area"
  diagnosis applies to a fourth iteration exactly as it did to the third.
- **A brand-new bespoke declaration syntax** (`# loom:write-root <path>`
  comment, or a dedicated `LOOM_WRITE_ROOT=` convention distinct from
  ordinary shell assignment). Considered, rejected in favor of reusing
  #4881's existing `record_assign()`/`resolve_var()`: a bespoke annotation is
  a *new* pseudo-language the guard must define, document, and reason about
  from scratch (what counts as "immediately before," can it be spoofed, does
  it survive `&&`/`;`/subshells) — exactly the kind of guard-specific
  heuristic that has produced bypasses before. A real `VAR=literal-value`
  assignment is already unambiguous, pre-existing shell grammar with an
  already-implemented, already-verified-fail-closed resolver; reusing it
  keeps Phase 2 to documentation/tests/audit rather than new
  denial-relevant code.
- **Leave the guard exactly as-is** (fail closed on every unresolved `$VAR`,
  accept the friction, do nothing). Rejected: the friction is real and
  recurring (#4921, #6172's own `$S/om-installed.sh` example), and — more
  importantly — the `COMMAND_NO_COMMENT` quote-unawareness bug found during
  this investigation is an *active* unsound false-negative independent of
  any redesign choice. "Do nothing" is not actually zero-risk; it is an
  unreviewed, undocumented risk already present in production.

## References

- Source issue: [#6235](https://github.com/rjwalters/loom/issues/6235)
  (Epic #6172 Phase 1)
- Epic: [#6172](https://github.com/rjwalters/loom/issues/6172) — Redesign the
  variable-rooted write-target analysis in the worktree-isolation guard
- Closed PR: [#5397](https://github.com/rjwalters/loom/pull/5397) — three
  Judge-confirmed bypasses of the `for`-loop carve-out (position/
  reassignment-unawareness, decoy-reference, substring-`done`-match); closed
  not viable
- Implemented issue behind #5397: [#5385](https://github.com/rjwalters/loom/issues/5385)
- [#4921](https://github.com/rjwalters/loom/issues/4921) — unresolved `$VAR`
  write-confinement fail-closed baseline
- [#4178](https://github.com/rjwalters/loom/issues/4178) — the original
  Bash-tool write-confinement incident (sweep #4063 edited live guard hooks
  in the main checkout)
- [#6123](https://github.com/rjwalters/loom/issues/6123) — gitignored
  build-artifact writes denied; closed, consolidated into #6172, explicitly
  scoped out here (denial-floor policy question, not ambiguity)
- [#6110](https://github.com/rjwalters/loom/issues/6110) — no sanctioned
  per-write path for interactive main-checkout sessions; closed via PR
  [#6143](https://github.com/rjwalters/loom/pull/6143) (discoverability
  only); residual friction explicitly scoped out here for the same reason
  as #6123
- `defaults/hooks/guard-destructive-generic.sh` — `_VARRESOLVE_AWK` /
  `record_assign()` / `resolve_var()` (~lines 1608–1686, #4881, shared per
  #6152); `COMMAND_NO_COMMENT` (~lines 3841–3859); `COMMAND_ASK_SCAN`
  (~line 3890); `extract_write_targets()` (~line 4296+) and its `sed` branch
  (~lines 4597–4656, #5674); the worktree-write-confinement deny block
  (~line 5000+, `WRITE_TARGETS` at ~line 5161)
- `.loom/docs/guard-hooks.md` § "Worktree Isolation Guard Opt-Out" (the
  `guards.worktreeIsolation` / `LOOM_GUARD_WORKTREE_ISOLATION` escape hatch)
  and its "Unresolvable `$…` targets fail closed" / "Quoted targets are
  still absolute" / "Quoted `cd` arguments are still absolute" subsections
  (#4921, #4926, #4933, #5363 — the guard's prior soundness-hardening
  history for this same check)
