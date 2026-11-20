---
disable-model-invocation: true
---
# Champion: Step 0.5 — Tracking-Umbrella Stand-Down (#7666)

This file is the body of `champion-epic.md` → "Step 0.5". **Read it when that
step tells you to**, after Step 0's Completion-First Check has declined to act
and before Step 1. It answers the one question the 6 structural criteria
cannot: "has this epic already been **decomposed**, by someone other than
Step 3?"

The 6 criteria describe an epic *awaiting decomposition* — they check the
phases, sizing, and success criteria Step 3 needs in order to create Phase 1
issues. An epic whose children already exist and are being worked is past that
point. Those children may come from a **Curator** decomposition pass, native
GitHub sub-issues, or a hand-written `- [ ] #N` task list, none of which carries
the `<!-- loom:epic:$EPIC_NUMBER:phase:N -->` marker that Step 2.75 and
"Detecting Phase Completion" search for — so neither phase creation nor phase
progression can see them, and a literal reading of Step 2.75 would report
`EXISTING_COUNT=0` and create a **duplicate** Phase 1 set. Champion has nothing
left to do for such an epic except watch it finish, which Step 0 already does
on every pass. A passing epic is never a stuck epic: the correct handling is
one durable note and then silence — never a structural re-evaluation, never an
operator escalation.

## 0.5a. Detect the tracking umbrella

**Reuses Step 0a's discovery — do NOT re-run `discover_epic_children`.**

```bash
# Set by Step 0a: EPIC_CHILD_STRONG_{OPEN,CLOSED}, EPIC_CHILD_SOURCES.
# `phase-marker` in EPIC_CHILD_SOURCES means discovery source (a) contributed,
# i.e. Champion decomposed this epic itself and "Phase Progression" owns it
# from here — such an epic must fall through to Step 1 exactly as before.
# An epic author may declare a tracking epic explicitly (#13874): the marker
# `<!-- loom:tracking-epic -->` ALONE on a line, outside any ``` / ~~~ code
# fence. Inline-in-prose or fenced mentions do not count. This covers epics whose
# children are listed in a table (discovery reads no table cells, #10935) and so
# discover as STRONG_OPEN=0. Mirrors `<!-- loom:tracker -->` (#10886).
DECLARED_TRACKING_EPIC=$(printf '%s\n' "$EPIC_JSON" | jq -r '.body // ""' | awk '
  /^[[:space:]]*(```|~~~)/ { fence = !fence; next }
  !fence && $0 ~ /^[[:space:]]*<!-- loom:tracking-epic -->[[:space:]]*$/ { found = 1 }
  END { print (found ? "yes" : "no") }')
case ",$EPIC_CHILD_SOURCES," in
  *,phase-marker,*)
    IS_TRACKING_UMBRELLA=no ;;                  # Champion's own decomposition (wins over the marker)
  *)
    if [ "$EPIC_CHILD_STRONG_OPEN" -gt 0 ] || [ "$DECLARED_TRACKING_EPIC" = "yes" ]; then
      IS_TRACKING_UMBRELLA=yes                  # decomposed elsewhere, or declared by its author
    else
      IS_TRACKING_UMBRELLA=no                   # undecomposed (Step 1), or complete (Step 0 owned it)
    fi ;;
esac
```

| Discovery state | `IS_TRACKING_UMBRELLA` | Why |
|---|---|---|
| `EPIC_CHILD_SOURCES` contains `phase-marker` | `no` | Champion decomposed this epic; Step 2.75 + "Phase Progression" already handle it, and standing down here would freeze phase advancement |
| `STRONG_OPEN > 0`, no `phase-marker` source | **`yes`** | Children exist by containment and are still in flight, but Step 3 did not create them — nothing to decompose, nothing to evaluate |
| `<!-- loom:tracking-epic -->` alone on a line outside a code fence, no `phase-marker` source (#13874) | **`yes`** | The author declared the epic a tracker (e.g. children listed in a table, which discovery does not read); same stand-down path, even with `STRONG_OPEN == 0`. With no discoverable children Step 0 can never close it, so the note says closing is manual (§0.5b) |
| `STRONG_OPEN == 0`, no marker | `no` | Either undecomposed (all counters 0 → Step 1) or a completion candidate, which Step 0 already acted on before this step ran |
| Weak (prose) references only | `no` | Prose is never containment — Step 0's operator ask is the only thing weak evidence may reach |

## 0.5b. Stand down — once per body hash, no counter, no escalation

```bash
if [ "$IS_TRACKING_UMBRELLA" = "yes" ]; then
  # Keyed to the SAME $BODY_HASH the guard computed, and to NOTHING else: a
  # genuine revision (new phases, changed scope) earns one fresh note, while a
  # child changing state does not — "this epic is decomposed and being worked"
  # is what the note says, and that stays true as children move. Distinct
  # marker name; NEVER the reserved rejection marker (see the guard's "Hard
  # constraint" in champion-epic.md).
  UMBRELLA_MARKER="<!-- champion:epic-tracking-umbrella:body-$BODY_HASH -->"
  if printf '%s\n' "$EPIC_JSON" | jq -e --arg m "$UMBRELLA_MARKER" \
       '.comments[] | select(.body | contains($m))' >/dev/null; then
    echo "#$EPIC_NUMBER is a tracking umbrella already noted at body revision $BODY_HASH — standing down silently (no comment, no tally, no label change)"
  else
    if [ "$EPIC_CHILD_STRONG_OPEN" -gt 0 ] || [ "$EPIC_CHILD_STRONG_CLOSED" -gt 0 ]; then
      UMBRELLA_WHY="This epic's children already exist ($EPIC_CHILD_STRONG_OPEN open / $EPIC_CHILD_STRONG_CLOSED closed, discovered via: $EPIC_CHILD_SOURCES) but were not created by Champion's own phase-issue flow, so it is a **tracking umbrella**"
      UMBRELLA_NEXT="Nothing is required of anyone. The epic stays open and keeps \`loom:epic\`; Champion re-checks it every pass and will close it automatically once its last child is closed and every named deliverable is present on \`${DEFAULT_BRANCH:-main}\`."
    else
      # Declared, zero discovered children (#13874). Step 0 closes on discovered
      # containment children only, and closing a table-listed child changes
      # nothing discovery reads, so an auto-close promise here would be false.
      UMBRELLA_WHY="This epic was declared a tracking epic by its author (\`<!-- loom:tracking-epic -->\`), so it is a **tracking umbrella**"
      UMBRELLA_NEXT="Champion cannot see this epic's children: none is linked in a form its discovery reads (a \`- [ ] #N\` task-list entry in this body, a native sub-issue, or a \`<!-- loom:parent #$EPIC_NUMBER -->\` line in the child's body). So Champion will **not** close this epic. It stays open and keeps \`loom:epic\` until its author or an operator closes it by hand once the work is done; nothing else is required. To have Champion close it instead, link the children in one of those forms."
    fi
    ./.loom/scripts/post-comment.sh "$EPIC_NUMBER" --body "$UMBRELLA_MARKER
**Champion: Epic Already Decomposed — Tracking Only**

$UMBRELLA_WHY, not an epic awaiting decomposition. Champion is not creating phase issues for it and is not re-running the pre-decomposition structural criteria against it.

$UMBRELLA_NEXT

---
*Automated by Champion role*"
  fi
  # END THE PASS FOR THIS EPIC either way: do not fall through to Step 1.
  # No label change, no tally, no escalation — continue to the next epic.
fi
```

## 0.5c. Behavioral checks and invariants

| Epic shape | Required outcome |
|---|---|
| Curator-decomposed epic, children open, first pass at this body hash | **One** "Epic Already Decomposed — Tracking Only" comment, then the pass ends |
| Epic body carries `<!-- loom:tracking-epic -->` alone on a line (not fenced, not inline), zero discovered children (#13874) | Same path: **one** note (worded "declared a tracking epic by its author"), then the pass ends — no rejection marker, no tally, no `loom:operator`. The note says Champion will **not** close the epic (it cannot see the children) and that the author or an operator closes it by hand; it never promises an automatic close. Marker in a code fence or inline prose: ignored. Marked epic with a `phase-marker` source: falls through to Step 1 |
| Same epic, every later pass with an unchanged body | Silent skip — no comment, no label, no counter. Step 0 still runs first, so an epic with discovered children closes on its own when the last child lands. A declared epic with no discoverable children stays open until its author or an operator closes it by hand |
| Same epic, body genuinely edited (new phases/scope) | New hash → one fresh stand-down note (still no evaluation, still no escalation) |
| Same epic, carrying an old, superseded "Epic Needs Revision" marker | Stand-down still wins: Step 0.5 runs **before** the guard's skip/escalate branches commit, so a rejected-then-decomposed epic never escalates on a stale finding |
| Champion-decomposed epic (phase-marker children) | Step 0.5 declines → Step 1 → unchanged Step 2/2.5/2.75/Phase Progression behavior |
| Undecomposed epic (no children) | Step 0.5 declines → Step 1 → Step 2's 6 criteria, byte-for-byte the pre-existing behavior |

**Invariants a future edit must preserve:**

- **A stand-down is never a rejection.** This step writes
  `champion:epic-tracking-umbrella:body-*` and nothing else. It must never write
  the Step 4 rejection marker, never touch `PRIOR_REJECTIONS` / `SKIP_STREAK`,
  and never apply `loom:operator` — a passing epic waiting on its own
  children is not an operator problem.
- **No escalation ladder, deliberately.** Same reasoning as "Phase
  Progression"'s own idempotency guard: an unchanged, in-progress epic is not
  stuck, it is *waiting*. Bounding a state that resolves itself when the last
  child closes would only reintroduce the noise this step removes. A declared
  epic with no discoverable children does not resolve itself (a person closes
  it), and still gets no ladder: it is silent after its one note, and nagging
  the operator about a tracker its author declared is the same noise.
- **Step 0 still runs on every pass.** The stand-down suppresses the *structural
  evaluation*, never the completion check — that is what lets an epic with
  discovered children close automatically instead of sitting open forever.
  **Exception (#13874):** a declared epic with no discoverable children is
  never a Step 0 completion candidate (0a row 1), and closing a table-listed
  child changes nothing discovery reads. Its author or an operator closes it by
  hand, and the §0.5b note must say so. It becomes auto-closable only once its
  children are linked as task-list entries, native sub-issues or
  `<!-- loom:parent #N -->` lines.
- **The `phase-marker` carve-out is load-bearing.** Widening this step to fire
  on epics Champion decomposed itself would freeze phase progression at whatever
  phase was open when the stand-down first fired.

`defaults/scripts/tests/test-champion-epic-verdict-marker-scope.sh` checks this
file statically: it must define its own marker, dedupe on `BODY_HASH`, and
carry no escalation counter and no `loom:operator` application.

---

Return to `champion-epic.md` and continue with the next epic (the pass for this
one is over), or — if `IS_TRACKING_UMBRELLA=no` — with Step 1.
