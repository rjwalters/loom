//! The `Issue`/`PrSet` begin step of a split dispatch (Issues #6592, #9573),
//! moved out of `dispatch.rs` to hold that file at its size ratchet.

use super::*;

impl SweepRegistry {
    /// First, lock-scoped step of a split `Issue`/`PrSet` dispatch (Issue
    /// #6592): idempotency dedup, the FULL `Issue`-kind guard chain
    /// (workspace-commands / closed-issue / open-PR / park-label / backoff /
    /// live-claim / peer-claim), claim-lock acquisition, the forge label
    /// flip, the #3887 dispatch stagger, and finally `Command::spawn()` for
    /// the child — everything through obtaining a live [`Child`] handle.
    /// Deliberately does **not** poll the child for its account-selection log
    /// line: that is the one genuinely multi-second wait in the whole
    /// dispatch path (bounded by `TOKEN_NAME_CAPTURE_TIMEOUT`, up to 5s), and
    /// it does not need the registry mutex — only the child handle and its
    /// log file. Splitting it out here is what lets the IPC layer release
    /// the registry mutex for that wait (Issue #6592's "double-blocking
    /// hazard": before this split, `handle_request`'s `DispatchSweep` arm
    /// held the registry mutex through the FULL guard chain AND the poll,
    /// serializing concurrent dispatches behind each other's poll wait and
    /// starving unrelated requests like `ListSweeps` on the same mutex).
    ///
    /// `PrSet` dispatch has no single-issue guard chain (see
    /// [`Self::dispatch_prset_inner`]'s doc comment) and no long poll to
    /// split around, so it is dispatched here fully synchronously and its
    /// result returned as [`BeginIssueDispatch::Done`] — unchanged behavior,
    /// still holding the mutex for its whole (comparatively short) duration.
    ///
    /// Returns [`BeginIssueDispatch::Done`] when the final [`DispatchOutcome`]
    /// is already known — an idempotency hit, a guard refusal, a `PrSet`
    /// result, or a spawn failure — and [`BeginIssueDispatch::Spawned`] when
    /// the child has been spawned and the caller must now poll it (via
    /// [`poll_and_classify_spawned_child`], OUTSIDE any lock it wants to
    /// release) and then call [`Self::finish_issue_dispatch`].
    ///
    /// # Idempotency-key dedup inside the unlocked-poll window
    ///
    /// Step 1's dedup consults
    /// [`find_running_by_key`](Self::find_running_by_key), which matches only
    /// entries already in `self.entries` — and this dispatch's entry is not
    /// inserted until [`finish_issue_dispatch`](Self::finish_issue_dispatch),
    /// after the poll, under a re-taken lock. So a same-idempotency-key retry
    /// that lands **between** a `Spawned` return here and the matching
    /// `finish_issue_dispatch` (i.e. inside the window the mutex is released
    /// for, bounded by `TOKEN_NAME_CAPTURE_TIMEOUT`, up to ~5s) MISSES step 1's
    /// short-circuit.
    ///
    /// Issue #9572 closed this gap: step 1b consults a second, in-memory
    /// `self.inflight_idempotency` map that step 3.05 (below) writes right
    /// after THIS attempt claims the lock — well before the window step 1's
    /// doc paragraph above describes even opens — so a same-key retry landing
    /// inside the window gets the same graceful `Done(Ok(DispatchOutcome {
    /// was_new: false, .. }))` hand-back a retry gets before or after it,
    /// naming the SAME `sweep_id`, rather than falling through to the guard
    /// chain. **No double-spawn results, exactly as before** — this is a
    /// strictly earlier, and therefore *more* precise, catch than the two
    /// guard-chain mechanisms that used to be the only backstop here (the
    /// #4556 live-claim guard's argv-scan leg, and the atomic `acquire_lock`
    /// mkdir's `lock collision`) — both remain in place for a same-key retry
    /// that somehow still misses the in-flight map (e.g. a key mismatch), and
    /// for every OTHER duplicate-dispatch shape they have always covered.
    /// Pinned by
    /// `same_key_retry_during_the_unlocked_poll_window_returns_the_inflight_sweep_id`
    /// in this module's tests (renamed from ...`_is_refused_not_double_spawned`
    /// — the refusal this issue replaces was itself the fix's whole point).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn begin_prepared_issue_dispatch(
        &mut self,
        kind: &SweepKind,
        idempotency_key: Option<String>,
        model: DispatchModel<'_>,
        effort: Option<&str>,
        depends_on: Option<u32>,
        resume_bypass_pr: Option<u32>,
        prepared_launch: Option<private_dispatch::PreparedLaunch>,
        dispatch_origin: DispatchOrigin,
    ) -> Result<BeginIssueDispatch> {
        Self::begin_issue_via(
            &mut registry_access::Held(self),
            kind,
            idempotency_key,
            model,
            effort,
            depends_on,
            resume_bypass_pr,
            prepared_launch,
            dispatch_origin,
        )
    }

    /// [`Self::begin_prepared_issue_dispatch`] for a caller that owns the
    /// `Arc<Mutex<SweepRegistry>>` (Issue #9573): the same guard chain in the
    /// same order with the same refusals, but the mutex is **not held across
    /// any forge `gh` call or `thread::sleep`**. See [`registry_access`] for
    /// the lock-scope audit. Blocking; call from `spawn_blocking` /
    /// `block_in_place`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn begin_prepared_issue_dispatch_shared(
        registry: &Mutex<SweepRegistry>,
        kind: &SweepKind,
        idempotency_key: Option<String>,
        model: DispatchModel<'_>,
        effort: Option<&str>,
        depends_on: Option<u32>,
        prepared_launch: Option<private_dispatch::PreparedLaunch>,
        dispatch_origin: DispatchOrigin,
    ) -> Result<BeginIssueDispatch> {
        Self::begin_issue_via(
            &mut registry_access::Shared::new(registry),
            kind,
            idempotency_key,
            model,
            effort,
            depends_on,
            None,
            prepared_launch,
            dispatch_origin,
        )
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn begin_issue_via<A: registry_access::RegistryAccess>(
        acc: &mut A,
        kind: &SweepKind,
        idempotency_key: Option<String>,
        model: DispatchModel<'_>,
        effort: Option<&str>,
        depends_on: Option<u32>,
        resume_bypass_pr: Option<u32>,
        prepared_launch: Option<private_dispatch::PreparedLaunch>,
        dispatch_origin: DispatchOrigin,
    ) -> Result<BeginIssueDispatch> {
        // 1/1b. Idempotency dedup (`idempotency_dedup`), re-checked below once the
        //       lock-free forge guards are done (#9573).
        if let Some(done) = acc
            .reg()
            .idempotency_dedup(kind, idempotency_key.as_deref())
        {
            return Ok(done);
        }

        let sr = acc.reg();
        // #10974: a daemon roll is pausing agents; nothing new may start.
        sr.roll_gate.admit(kind)?;
        // Count this dispatch as mid-spawn in the SAME lock hold as `admit`
        // (#10831 H4, kept atomic under #9573): the `Shared` path drops the
        // lock for the forge guards below, so a roll that closes the gate in
        // that window must still see this admitted dispatch in
        // `mid_spawn_dispatches()` and wait for it. Every early return drops
        // the guard; a `Spawned` hand-back carries it to `finish_issue_dispatch`.
        let mid_spawn = sr.roll_gate.enter();

        // Runtime admission is the first decision for a new dispatch:
        // before account selection, claim lock, forge mutation,
        // log header, or child spawn. A full sweep remains one runtime and is
        // checked against Builder's (strongest lifecycle) requirements.
        let (admission, resolved_model, selection) = if let Some(prepared) = prepared_launch {
            (prepared.admission, prepared.model, prepared.selection)
        } else {
            let admission = if sr.config.skip_label_flip {
                // hermetic unit fixtures do not install runtime manifests
                crate::runtime_preference::DispatchAdmission::none()
            } else {
                // The admitted runtime and optional backstop reservation travel
                // together through begin/poll/finish; early returns release the slot.
                match crate::runtime_preference::resolve_for_dispatch(
                    &sr.config.workspace_root,
                    "sweep-lifecycle",
                    None,
                ) {
                    Ok(admission) => admission,
                    Err(rejection) => {
                        // Refused work still gets an event representation (#4494):
                        // `sweep.global.dispatch` describes admitted work only, so
                        // without this the refusal was invisible on the bus. This
                        // is a PURE publish — no claim lock, no account selection,
                        // no log header, no forge call — so the pre-claim
                        // side-effect contract is preserved.
                        sr.emit_event(Event::SweepGlobalRuntimeRejected {
                            kind: kind.clone(),
                            role: rejection.role.clone(),
                            runtime: rejection.runtime.clone(),
                            runtime_source: rejection.source.clone(),
                            unmet_capabilities: rejection.unmet_capabilities.clone(),
                            reason: rejection.reason.clone(),
                            // Stamped by `emit_event` -> `set_repo_if_absent`.
                            repo: None,
                        });
                        return Err(anyhow::Error::new(rejection));
                    }
                }
            };

            // Resolve implicit defaults/experiments only after the ONE runtime
            // admission above; explicit pins remain explicit even after fallback.
            let resolved_model = model.resolve(&sr.config, kind, admission.admitted.as_ref());
            (admission, resolved_model, None)
        };
        let model_source = model.source();
        let model = resolved_model.as_deref();

        // 2. `Issue` and `PrSet` dispatch diverge here (Issue #5342): `PrSet`
        //    has no single issue number to run the issue-keyed guard chain
        //    (2.4-2.9 below) against — it claims no `loom:building` label and
        //    is tracked via a per-PR claim lock instead (see
        //    `dispatch_prset_inner` / `SweepRegistry::pr_lock_dir`).
        let issue_number = match kind {
            SweepKind::Issue(n) => *n,
            // Nothing dispatches a step through the sweep path: the step
            // engine's spawn (#11906) claims and launches it. Refuse loudly
            // rather than run the issue guard chain on a PR or a Curator item.
            SweepKind::Step { item, role } => {
                anyhow::bail!(
                    "dispatch refused: a {role} step on #{item} is not dispatched as a sweep \
                     (the step engine spawns steps, #11906)"
                );
            }
            SweepKind::PrSet(prs) => {
                return Ok(BeginIssueDispatch::Done(sr.dispatch_prset_inner(
                    prs,
                    kind,
                    idempotency_key,
                    model,
                    effort,
                    admission,
                    selection.as_ref(),
                )));
            }
        };

        // 2.4 Workspace-commands guard (Issue #4027). A workspace registered
        //     (or hot-added, #3926) without ever running `loom-daemon init` —
        //     e.g. a bare `git clone` on a second daemon host — has `.git`/
        //     `.loom` so it "looks like" a workspace, but lacks the
        //     install-not-committed `.claude/commands/loom/` slash commands.
        //     Dispatching `/loom:sweep <N>` into it insta-crashes the child on
        //     `Unknown command: /loom:sweep` within seconds, and because it
        //     exits before any checkpoint/worktree exists, the reaper reverts
        //     `loom:building` -> `loom:issue` and the work-finder re-dispatches
        //     on the next tick: an infinite fast-fail loop burning a rotated
        //     token roughly every tick, forever. Checked FIRST — before even
        //     the closed-issue guard's `gh` probe below — because it is a
        //     single local `stat` versus a subprocess spawn: a misconfigured
        //     workspace should cost as little as possible per tick, and zero
        //     tokens either way. Skipped when label flips are disabled (test
        //     fixtures exercising pure in-memory dispatch mechanics without a
        //     fully Loom-managed workspace on disk), mirroring the #4088
        //     closed-issue guard's skip condition below.
        let skip_label_flip = sr.config.skip_label_flip;
        if !skip_label_flip && !sr.config.has_sweep_command() {
            return Err(WorkspaceCommandsMissingDispatchError {
                workspace: sr.config.workspace_root.clone(),
            }
            .into());
        }

        // 2.45 Workspace hold (Issue #10719): the installed Loom here cannot
        //      work with this daemon (W3/W4). Structural like 2.4, so `force`
        //      does not bypass it; before any lock, label flip or forge call.
        crate::workspace_hold::guard(&sr.config.workspace_root)?;

        // 2.5 Closed-issue guard (Issue #4088, widened in #4504). All three
        //     watchdogs (startup #3887, mid-build-death #3895, review-stall
        //     #3910) re-dispatch through this method, and `gh issue edit`
        //     succeeds on a closed issue, so nothing else stops a watchdog
        //     false-positive from re-claiming an issue whose PR already merged.
        //     Placing the guard here — before the lock/label flip — covers all
        //     three call sites with one check. #4504 widened the probe from a
        //     `state == "CLOSED"` string match to a REST payload that also
        //     reports PR-ness, so a dispatch number that resolves to a pull
        //     request (open, closed, or merged — issues and PRs share one number
        //     namespace) is refused too instead of slipping through the fail-open
        //     arm. Best-effort and fail-open: a forge lookup error returns `None`
        //     and dispatch proceeds, so a `gh` outage can never wedge the daemon.
        //     Skipped when label flips are disabled (test fixtures without `gh`
        //     credentials).
        //
        //     Issue #7606: before spending THIS REST round trip, consult the
        //     2.6 guard's own verified-open-PR memo (#6788,
        //     `fresh_open_pr_memo`) — a still-fresh entry means a *previous*
        //     dispatch attempt already verified an open linked PR for this
        //     issue, so 2.6 would refuse it again regardless of what this
        //     probe answers. Refusing right here instead skips BOTH this
        //     REST call and 2.6's own probe attempt, at zero forge cost — the
        //     memo lookup is a pure in-memory read. Fail-open is unaffected:
        //     `fresh_open_pr_memo` returns `None` on a missing/expired/disabled
        //     memo, which falls straight through to the unchanged 2.5/2.6
        //     checks below, so a memo MISS can never be mistaken for a
        //     verified refusal. The refusal also arms this issue's #4485
        //     backoff ladder (`record_open_pr_guard_backoff`) so a
        //     work-finder-driven re-dispatch is filtered out via
        //     `WorkDispatcher::backed_off` before `dispatch()` (and this
        //     REST call) is even attempted again — see that method's doc
        //     comment for the full rationale.
        //
        //     W9: a resume never consults the memo (`dispatch_open_pr_memo`,
        //     see `guards/refusal_memo.rs`), and the probe below is a
        //     conditional read whose verdict comes from the body it serves.
        if !skip_label_flip {
            if let Some(memo) = sr.dispatch_open_pr_memo(issue_number, resume_bypass_pr) {
                sr.record_open_pr_guard_backoff(issue_number);
                return Err(OpenPrDispatchError {
                    issue: issue_number,
                    pr: memo.pr,
                }
                .into());
            }

            // #11429: a closed issue is refused as `already_landed`.
            // Lock-free (#9573): a forge REST read.
            let refusal = acc.unlocked(|r| r.closed_issue_refusal(issue_number));
            acc.reg().apply_landed_refusal(issue_number, refusal)?;
        }

        // 2.6 Open-PR guard (Issue #4123). Every in-memory dedup signal — the
        //     idempotency key, the in-flight set, the `loom:building` label —
        //     is scoped to the running sweep's lifetime and clears when the
        //     parent exits (`reconstruct()` even drops the idempotency key on a
        //     daemon restart). So an issue whose approved PR is still open looks
        //     identical to fresh work the moment its sweep dies, and every
        //     caller that routes through `dispatch()` — the work-finder, the
        //     epic supervisor, the IPC/CLI dispatch, and all three watchdogs
        //     (startup #3887, mid-build-death #3895, review-stall #3910) — would
        //     re-dispatch it, redoing finished work against a scarce token pool.
        //     The forge's closes-graph is the one durable signal that survives
        //     process death and restarts, so this guard consults it, right after
        //     the closed-issue guard and before the lock/label flip so a single
        //     check covers all six call sites. Keys on PR *openness* only, never
        //     on review labels — driving an open PR forward is the
        //     Judge/Champion/Doctor path's job, not the issue work-finder's.
        //     Best-effort and fail-open: any forge error/timeout/unparseable
        //     output returns `None` and dispatch proceeds, so a `gh` outage (or
        //     a Gitea workspace — this is GitHub-only, like `issue_is_closed_or_pr`)
        //     can never wedge the daemon. Skipped when label flips are disabled
        //     (test fixtures without `gh` credentials), mirroring 2.5.
        //
        //     Issue #6593: the refusal is not a dead end — `OpenPrDispatchError`'s
        //     `Display` names the reachable alternative for this candidate class,
        //     a `SweepKind::PrSet` dispatch (#5342) for the PR the probe found,
        //     which drives Judge/Doctor -> Merge on it without rebuilding. This
        //     guard deliberately does NOT auto-convert: an issue-keyed dispatch
        //     claims an issue and flips `loom:building`, a PrSet dispatch claims
        //     PR locks instead, so silently substituting one for the other would
        //     hand the caller a sweep it did not ask for (and a different
        //     idempotency/dedup domain). Naming it is the caller's cue to re-issue.
        //
        //     Issue #4256: `resume_bypass_pr` — set only via
        //     `dispatch_resume_after_crash` (direct/test callers) or
        //     `reap_once_impl`'s own inlined `begin_issue_dispatch` call
        //     (production, Issue #6691) — exempts a resume of THIS issue's own
        //     crashed sweep from the guard, but only when it names the exact PR
        //     the guard would find; any other PR (or none) still refuses
        //     normally, so a stale/mismatched resume can never widen into a
        //     blanket bypass. Both call sites trace back to `Self::reap_once`
        //     (directly, or via its lock-releasing sibling
        //     `reap_once_releasing_poll_lock`); no other caller can set it.
        if !skip_label_flip {
            // Fail-open (#4452): only a VERIFIED `Open(pr)` blocks; both
            // `NoneOpen` and `ProbeFailed` fall through and proceed, so a forge
            // outage can never wedge dispatch (unchanged pre-#4452 behavior).
            // W9: live (memo-free) for a resume — see `dispatch_open_pr_probe`.
            // Lock-free (#9573): the open-PR probe is the multi-transport forge read.
            let probe = acc.unlocked(|r| r.dispatch_open_pr_probe(issue_number, resume_bypass_pr));
            if let OpenPrProbe::Open(pr) = probe {
                if resume_bypass_pr != Some(pr) {
                    // Issue #7606: a verified refusal here is exactly the
                    // same event the 2.5-position memo short-circuit above
                    // arms the ladder for — see
                    // `record_open_pr_guard_backoff`'s doc comment.
                    acc.reg().record_open_pr_guard_backoff(issue_number);
                    return Err(OpenPrDispatchError {
                        issue: issue_number,
                        pr,
                    }
                    .into());
                }
                log::info!(
                    "issue #{issue_number}: reaper-driven resume dispatch bypassing the #4123 \
                     open-PR guard for its own PR #{pr} (#4256)"
                );
            }
        }

        // #11429: a `landed` outcome on this host's journal refuses as
        //     `already_landed`. It runs after 2.6, so an issue whose PR is
        //     still open gets `OpenPrDispatchError` first, which the
        //     review-stall watchdog converts to a PrSet dispatch (#7649). A
        //     crash resume of the issue's own sweep is exempt.
        if resume_bypass_pr.is_none() {
            // Lock-free (#9573): `journal_repo_slug` may resolve the repo over `gh`.
            let refusal = acc.unlocked(|r| r.recent_landing_refusal(issue_number, Utc::now()));
            acc.reg().apply_landed_refusal(issue_number, refusal)?;
        }

        // 2.7 Park-label guard (Issue #4444). The work-finder's `SKIP_LABELS`
        //     hard-skip is enforced only in *its own* candidate query, so it
        //     covers exactly one of the six dispatch routes. Every other route
        //     — all three watchdogs (#3887 / #3895 / #3910), the reaper's
        //     checkpoint-resume (#4256), the epic supervisor, the IPC/CLI
        //     `dispatch_sweep` — funnels through here, and until this guard
        //     existed none of them ever re-read the forge labels. A
        //     `loom:blocked` / `loom:operator-only` park applied *after* the
        //     original dispatch was therefore invisible to every re-dispatch
        //     path, and the daemon overrode a deliberate human/agent park
        //     (observed on #4366). Placing the check here — before the
        //     lock/label flip — covers all routes with one probe.
        //
        //     Three properties are load-bearing:
        //
        //     - It guards on `PARK_LABELS` only, NOT the full `SKIP_LABELS`
        //       set. `loom:building` is legitimately present on a watchdog
        //       re-dispatch or a checkpoint-resume of the daemon's OWN claim,
        //       so refusing it would break the review-stall watchdog's
        //       cancel-and-re-dispatch and the reaper's resume.
        //     - It is NOT exempted by `resume_bypass_pr`. The #4256 bypass
        //       covers step 2.6 (its own open PR) and nothing else: a park
        //       applied after the crash must still stop the resume, which is
        //       the exact defect this guard fixes.
        //     - It probes over **REST** (`gh api repos/{owner}/{repo}/issues/N`),
        //       a separate rate-limit bucket from the GraphQL calls 2.5/2.6
        //       ride, so the park still holds while the GraphQL quota is
        //       exhausted — the condition under which the #4123 guard failed
        //       open during the 2026-07-29 incident.
        //
        //     Best-effort and fail-open, mirroring 2.5/2.6: any forge
        //     error/timeout/unresolvable repo returns `None` and dispatch
        //     proceeds, so a `gh` outage can never wedge the daemon. Skipped
        //     entirely when label flips are disabled (test fixtures without
        //     `gh` credentials).
        //
        //     The label set this guard reads is ALSO what the #9432 story-point
        //     resolution below consumes: one REST read, two consumers, no
        //     second fetch (epic #9429: "read the issue's `points:*` label from
        //     the issue data the dispatch path already resolves").
        let dispatch_labels = if skip_label_flip {
            None
        } else {
            // Lock-free (#9573): a forge REST read.
            acc.unlocked(|r| r.guard_issue_labels(issue_number))
        };
        if let Some(labels) = dispatch_labels.as_deref() {
            if let Some(label) = acc.unlocked(|r| r.first_park_label_in(issue_number, labels)) {
                log::info!(
                    "issue #{issue_number}: refusing dispatch — the issue carries `{label}`, a \
                     deliberate park that every dispatch route must respect (#4444 park-label \
                     guard); clear the label to re-enable automation"
                );
                return Err(ParkedIssueDispatchError {
                    issue: issue_number,
                    label,
                }
                .into());
            }
        }

        // 2.71 Story-point size (Issue #9432, epic #9429). Resolved here — from
        //      the labels step 2.7 just read, never a second forge call — and
        //      carried on this dispatch's `sweep.global.dispatch` event so
        //      `sweep.started` telemetry reports the assigned size of the work
        //      now in flight. Pure label folding: zero forge cost, and a
        //      stacked/malformed points label is logged loudly and resolves to
        //      no value rather than to a guess (see `crate::story_points`).
        //
        //      `None` here covers "unsized issue", "unreadable/skipped label
        //      read" and "defective labels" alike — the attribute is simply
        //      omitted downstream, never emitted as `0`.
        let story_points = dispatch_labels
            .as_deref()
            .and_then(|labels| crate::story_points::resolve_story_points(issue_number, labels));

        // 2.75 Noop-cooldown guard (Issue #6917, follow-up to #6670/#6740).
        //      `record_noop_release` (`noop_cooldown.rs`, exposed over IPC as
        //      `RecordNoopRelease`) lets a sweep that concluded "no
        //      actionable delta this pass" arm a cooldown so the work finder
        //      does not immediately re-offer the same candidate. The
        //      tick-based work-finder loop already consults this state
        //      before re-selecting a candidate (`work_finder.rs`,
        //      `WorkDispatcher::noop_cooldown`) — but every OTHER dispatch
        //      route funnels through THIS method (the IPC/CLI
        //      `{"Issue": <N>}` RPC behind `loom-daemon dispatch <N>` /
        //      `--claim-owned <N>`, the epic supervisor, and all three
        //      watchdogs) without ever reading it, so a direct re-dispatch
        //      could re-claim the same issue within the very cooldown window
        //      its last sweep deliberately armed.
        //
        //      `noop_cooldown_dispatch_block` is a pure in-memory lookup with
        //      no `gh` dependency — like the 2.8 backoff guard immediately
        //      below, this is NOT gated on `skip_label_flip`, and a refusal
        //      costs no lock, no label write, and no forge round trip.
        //
        //      Issue #9928: it reads the FLEET-AWARE window (this host's own
        //      ∪ any live one a peer host broadcast under #7477), not the
        //      host-local `noop_cooldown_remaining` this guard originally
        //      called. The work finder's advisory pre-filter has consulted the
        //      unioned view since #7477, but THIS guard — the one every route
        //      above funnels through, including the work finder's own — did
        //      not, so a window armed by host A left every dispatch reaching
        //      this point on hosts B/C/D free to re-claim the same issue
        //      inside it: four hosts re-dispatching one unchanged tracker
        //      issue in ~10 minutes, the exact round-robin #7477 exists to
        //      stop. See `noop_cooldown_dispatch_block`'s own doc comment.
        //
        //      Placed AFTER the 2.7 park-label guard, deliberately: when an
        //      issue is both parked and mid-cooldown, the park is the more
        //      actionable, durable operator decision and must win — this
        //      guard never runs for an issue 2.7 already refused, so guard
        //      ordering here cannot change that outcome.
        //
        //      Not exempted by `resume_bypass_pr`, mirroring 2.7's own
        //      rationale: a cooldown armed by the issue's own last (possibly
        //      since-crashed) sweep still means "nothing has changed since
        //      that check" and a resume must respect it exactly like a fresh
        //      dispatch would.
        // Back under the lock for the rest of the pre-claim guards and the
        // local claim (#9573): everything from here through step 3.05 is
        // in-memory or local-filesystem work, so the lock is never dropped
        // between the guards below and the `acquire_lock` they protect. The
        // dedup is re-run first: a same-key dispatch that claimed the lock
        // while the forge guards above ran lock-free is handed back here
        // exactly as a retry arriving before or after it would be.
        let sr = acc.reg();
        if let Some(done) = sr.idempotency_dedup(kind, idempotency_key.as_deref()) {
            return Ok(done);
        }
        if let Some(remaining) = sr.noop_cooldown_dispatch_block(issue_number, Utc::now()) {
            log::info!(
                "sweep_registry: refusing to dispatch issue #{issue_number} — a live no-op \
                 release cooldown is armed on this host or a fleet peer, {}s remaining \
                 (#6670/#6917/#9928 noop-cooldown guard); the last sweep found nothing had \
                 changed since its previous check.",
                remaining.as_secs()
            );
            return Err(NoopCooldownDispatchError {
                issue: issue_number,
                retry_after_secs: remaining.as_secs(),
            }
            .into());
        }

        // 2.8 Per-issue dispatch backoff (Issue #4485). The quarantine backstop
        //     (#3939) only engages after three *tally-eligible* insta-crashes,
        //     and both the account-exhaustion (#4122) and claude-wrapper
        //     pre-flight (#4386) carve-outs deliberately leave that tally
        //     untouched — so an issue whose every dispatch dies in that shape
        //     was re-dispatched on every tick forever, flapping
        //     `loom:issue`/`loom:building` at the reap→restore→re-poll cadence
        //     (~90 label events in 7 minutes on #4398). This guard caps the
        //     *rate* rather than the *cause*: any no-progress terminal outcome
        //     arms an exponential per-issue window (see
        //     `record_dispatch_failure`) and dispatch refuses until it elapses.
        //
        //     Placed with the other pre-flip guards (2.4-2.7) so one check
        //     covers every dispatch call site — work-finder, epic supervisor,
        //     IPC/CLI, and all three watchdogs — and, critically, so a refusal
        //     costs **no lock, no label write, and no forge round trip of its
        //     own**: the refusal itself can never contribute to a flap. Unlike
        //     2.4-2.7 it is NOT gated on `skip_label_flip`: it is pure
        //     in-memory bookkeeping with no `gh` dependency, and the flap it
        //     prevents is driven by dispatch cadence, not by credentials.
        //
        //     Runs *after* the 2.7 park-label guard, deliberately. When an issue
        //     is both parked and inside a backoff window, the park is the
        //     durable operator decision and the more actionable refusal, so it
        //     wins and the skip is attributed to `labeled-skip` rather than to
        //     `backoff-skip` (which advertises an imminent auto-retry that a
        //     park forbids). The two guards share no state: a refusal here never
        //     spawns a sweep, so no refusal — park or backoff — can ever call
        //     `record_dispatch_failure` and arm/extend a window, and a backoff
        //     window keeps decaying on wall-clock time while an issue sits
        //     parked. Clearing the park therefore re-exposes any *still-live*
        //     window, which is correct: the park did not prove the failing
        //     dispatch loop fixed.
        //
        //     Exempt: the reaper-driven resume (#4256, `resume_bypass_pr`),
        //     which re-dispatches an issue whose own PR is open and is already
        //     bounded by `MAX_RESUME_ATTEMPTS`. Never a work-finder loop.
        if resume_bypass_pr.is_none() {
            if let Some(remaining) = sr.dispatch_backoff_remaining(issue_number, Utc::now()) {
                let consecutive = sr.dispatch_failure_count(issue_number);
                let window = sr.dispatch_backoff.get(&issue_number);
                backoff_cause::log_refused(issue_number, window, remaining);
                return Err(DispatchBackoffError {
                    issue: issue_number,
                    consecutive,
                    retry_after_secs: remaining.as_secs(),
                }
                .into());
            }
        }

        // 2.9 Live-claim guard (Issue #4556). The single hard, dispatch-time
        //     refusal for an issue whose sweep is *confirmed still running*.
        //
        //     Every guard before this one, and `acquire_lock` below, keys on
        //     state that a re-dispatch path has already invalidated by the time
        //     it dispatches:
        //
        //     - `acquire_lock` refuses only on the lock **existing**, and every
        //       reaper / cancel / watchdog path releases that lock *first*, on
        //       the strength of its own dead-sweep verdict.
        //     - The `loom:building` label is reverted by
        //       `claim_reconciliation` the moment a recorded PID looks dead
        //       (confirmed on #4275 at 03:08:15Z), re-exposing the issue to the
        //       work-finder.
        //     - The in-memory entry set (`issue_has_active_sweep`,
        //       `in_flight()`) is scoped to ONE daemon process — invisible to a
        //       second `loom-daemon` instance on the same host, which is where
        //       3 of the 7 #4275 dispatches came from.
        //
        //     `live_claim::probe` asks the strictly stronger question instead:
        //     is a sweep process for this issue *alive right now*? Its three
        //     evidence legs (live lock owner / machine-level `~/.loom/sweeps.json`
        //     journal / `/proc` scan for a `/loom:sweep <N>` process rooted in
        //     this workspace) each survive a lock release, a label revert, a
        //     daemon restart, AND a second daemon instance.
        //
        //     Placed with the other pre-flip guards so ONE check covers all six
        //     dispatch routes — work-finder, epic supervisor, IPC/CLI, and all
        //     three watchdogs — and so a refusal costs no lock, no label write,
        //     and no forge round trip. Deliberately NOT exempted by
        //     `resume_bypass_pr`: a checkpoint-resume of a crashed sweep is
        //     still a duplicate if the "crashed" sweep turns out to be alive.
        //     Not gated on `skip_label_flip` either — it is pure local
        //     filesystem bookkeeping with no `gh` dependency.
        //
        //     Fail-open: `probe` returns `None` on every ambiguity (missing or
        //     corrupt `owner.json`, unreadable journal, no `/proc`), and treats
        //     a zombie PID as dead, so a garbage file can never wedge an issue.
        if let Some(evidence) = sr.live_claim_evidence(issue_number) {
            log::warn!(
                "sweep_registry: refusing to dispatch issue #{issue_number} — {evidence} \
                 (#4556 live-claim guard). This is the duplicate-dispatch storm guard; the \
                 live sweep keeps its claim and runs its own lifecycle."
            );
            return Err(LiveClaimDispatchError {
                issue: issue_number,
                evidence,
            }
            .into());
        }

        // 2.95 Peer-claim guard (Issue #5789, upgrading #4028's soft claim from
        //      detection into enforcement). `self.peer_claimed_issues()` is a
        //      free, in-memory lookup — no `gh` round trip, no lock, nothing to
        //      unwind — fed by the safehouse coordination task's inbound read
        //      of peer advertisements. If a **peer** host (never this host's own
        //      ad — `PeerClaimView::observe_at` ignores self-claims) still has a
        //      live claim on this issue, dispatching here would duplicate a
        //      sweep another host already started. This closes the same gap for
        //      every `dispatch()` call site (work-finder, epic supervisor,
        //      IPC/CLI, all three watchdogs) that the work-finder's own
        //      pre-tick `peer_claimed()` filter only ever covered for ITS one
        //      route (#4028's original scope).
        //
        //      A no-op (empty set) when no view is attached (`safehouse.enabled`
        //      false) or the mutex is poisoned — same fail-open contract as
        //      `peer_claimed_issues` itself, so a safehouse outage can never
        //      wedge dispatch.
        if sr.peer_claimed_issues().contains(&issue_number) {
            sr.collision_count += 1;
            // #5921: the proof-of-mechanism counter — a peer-claim view can
            // be non-empty (received > 0) yet never actually prevent a
            // duplicate if this guard were ever bypassed; this increments
            // exactly when it did.
            if let Some(view) = &sr.peer_claims {
                match view.lock() {
                    Ok(mut v) => v.record_dispatch_skipped(),
                    Err(poisoned) => poisoned.into_inner().record_dispatch_skipped(),
                }
            }
            log::warn!(
                "sweep_registry: BACKING OFF dispatch of issue #{issue_number} — a peer host's \
                 soft-claim advertisement is still live for this issue (#4028/#5789 peer-claim \
                 enforcement, running collision count={count}). Refusing to duplicate a sweep \
                 another host already claimed; no lock was acquired and no advertisement was \
                 published by this host for this issue.",
                count = sr.collision_count,
            );
            return Err(CollisionDispatchError {
                issue: issue_number,
                source: CollisionSource::PeerClaim,
            }
            .into());
        }

        // 3. Acquire the claim lock atomically.
        let sweep_id = generate_sweep_id(kind);
        if let Some(selection) = &selection {
            selection.rebind(&sr.config.workspace_root, &sweep_id)?;
        }
        sr.acquire_lock(issue_number, &sweep_id)?;

        // 3.05 Record the in-flight idempotency key (Issue #9572), now that
        //      this attempt has actually claimed the lock and its `sweep_id`
        //      is allocated — see `inflight_idempotency`'s doc comment and
        //      step 1b above for the window this closes. From here on, every
        //      early `Err` return in this function MUST clear this entry (see
        //      the three sites below) so a failed attempt never wedges a
        //      later, legitimate same-key dispatch; the success path clears
        //      it in `finish_issue_dispatch` instead, once the attempt is
        //      visible via the ordinary `self.entries`-scoped dedup.
        if let Some(ref key) = idempotency_key {
            sr.inflight_idempotency
                .insert(key.clone(), sweep_id.clone());
        }

        // 3a. Soft cross-host claim (Issue #4028): advertise this claim over the
        //     shared safehouse room **before** the non-atomic label flip below,
        //     so a peer daemon backs off far faster than the `loom:building`
        //     label propagates. Best-effort, non-blocking, fail-open (a no-op
        //     when `safehouse.enabled` is false) — the room broadcast is a soft
        //     claim/fast backoff, NOT a mutex; the forge label remains the
        //     human-visible claim signal and Phase 2 is the atomic authority.
        sr.publish_peer_claim(peer_claims::ClaimKind::Advertise, issue_number);

        // From here the lock is released for every forge write, the sole-claim
        // confirmation and the stagger sleep (#9573); the local claim above
        // (lock dir + in-flight key) is what keeps a concurrent same-issue
        // dispatch out meanwhile.

        // 4. Flip the forge label loom:issue -> loom:building (best-effort
        //    when the dispatcher has gh credentials; tests opt out via
        //    `skip_label_flip`).
        //
        // `episode_start` anchors the claim-then-verify-order tie-break below
        // (4c, Issue #6287) to THIS dispatch attempt's own local clock,
        // captured before the flip so it bounds "which lease comments belong
        // to this claim episode" without re-reading `Utc::now()` after the
        // round trips below have already spent wall-clock time.
        let episode_start = Utc::now();
        let mut lease_order_yield: Option<(String, String)> = None;
        // #10345: set when the yield is the leaseless-label variant, with the
        // local window of this dispatcher's own flip, so step 4d can tell a
        // phantom (own flip misread) from a genuine hand-claim.
        let mut leaseless_flip_window: Option<(DateTime<Utc>, DateTime<Utc>)> = None;
        // #11304: set once the flip below succeeds, so the post-claim state
        // re-check (4e) only runs for a claim this dispatch actually took.
        let mut claim_flipped = false;
        if !skip_label_flip {
            // 4a. Cross-host collision guard (Issue #4085, Phase 0 of #4028;
            //     upgraded from detection-only to enforcement by #5789): read
            //     the pre-flip label state and, when it shows a peer host
            //     already claimed this issue, BACK OFF instead of proceeding.
            //     A no-op (returns `None`, nothing logged/counted) when
            //     detection is disabled (default off — this opt-in probe costs
            //     one extra `gh issue view` round trip), so the disabled flip
            //     path stays byte-for-byte unchanged. Must run BEFORE the flip:
            //     once this host flips, a collided issue is indistinguishable
            //     from a clean one — and this is also the fallback net for the
            //     race the cheaper 2.95 peer-claim guard above can miss (no
            //     safehouse view attached, or the peer's ad simply hadn't
            //     arrived yet): a host that proceeded past 2.95 still gets
            //     caught — and logged — here when the collision is confirmed
            //     against the forge's own label state.
            //     Not for a crash resume (#11852): the `loom:building` it sees
            //     is the crashed sweep's own, kept while its PR is open.
            let collision_probe = if acc.reg().detect_collisions && resume_bypass_pr.is_none() {
                Some(acc.unlocked(|r| r.classify_preflip_labels(issue_number)))
            } else {
                None
            };
            if let Some(CollisionClass::Collision { labels }) =
                collision_probe.map(|class| acc.reg().record_collision_class(issue_number, class))
            {
                log::warn!(
                    "sweep_registry: BACKING OFF dispatch of issue #{issue_number} — the \
                     pre-flip forge-label read confirmed another host already claimed it \
                     (#4085/#5789 enforcement); reverting the claim lock and peer-claim \
                     advertisement this host had already applied instead of flipping the label \
                     and duplicating the sweep."
                );
                // #9572: also releases the in-flight idempotency key (3.05).
                acc.reg()
                    .unwind_local_claim(issue_number, &sweep_id, idempotency_key.as_ref());
                return Err(CollisionDispatchError {
                    issue: issue_number,
                    source: CollisionSource::ForgeLabel { labels },
                }
                .into());
            }
            match acc.unlocked(|r| r.flip_label_to_building(issue_number)) {
                Ok(()) => {
                    claim_flipped = true;
                    // 4b. Write the lease record (Issue #6179, Epic #6165
                    //     Phase 1): a best-effort forge comment documenting
                    //     which host/sweep now holds this claim, posted only
                    //     on a confirmed successful flip — a failed flip
                    //     means there is no claim to lease. Write-only: no
                    //     reclamation/dispatch logic reads this yet (Phase 2).
                    acc.unlocked(|r| r.write_lease_comment(issue_number, &sweep_id));

                    // 4c. Claim-then-verify-order tie-break (Issue #6287,
                    //     Epic #6165 Phase 2): re-read every live lease
                    //     comment on the issue and check whether THIS
                    //     dispatcher's own comment is the earliest one, by
                    //     forge-assigned comment order — the closes-the-gap
                    //     mechanism for the exact race the label flip above
                    //     cannot itself prevent (it is unconditionally
                    //     idempotent, so two near-simultaneous dispatchers
                    //     both succeed at it). A losing verdict is recorded
                    //     here and acted on AFTER `note_label_flip` below, so
                    //     flap-detection bookkeeping for this real flip stays
                    //     unconditional either way.
                    //     Since #9453 Phase 3.1 the same call also yields to a
                    //     young LEASELESS foreign `loom:building` — a claim
                    //     from a lane that published no lease record — which
                    //     `yield_identity` reports through this same channel.
                    let decision = acc.unlocked(|r| {
                        r.resolve_lease_order(issue_number, &sweep_id, episode_start)
                    });
                    // #11852: a crash resume re-claims the issue's own
                    // `loom:building`, kept while its PR is open — not a
                    // foreign leaseless claim. Lease-comment order still counts.
                    let decision = match decision {
                        LeaseOrderDecision::YieldToLeaselessClaim { .. }
                            if resume_bypass_pr.is_some() =>
                        {
                            LeaseOrderDecision::Proceed
                        }
                        other => other,
                    };
                    if matches!(decision, LeaseOrderDecision::YieldToLeaselessClaim { .. }) {
                        leaseless_flip_window = Some((episode_start, Utc::now()));
                    }
                    lease_order_yield = decision.yield_identity();
                }
                Err(e) => {
                    log::warn!(
                        "label flip for issue #{issue_number} failed (continuing dispatch): {e}"
                    );
                }
            }
            // Flap detection (#4485): count this claim write and warn if this
            // issue's label is being cycled far faster than a healthy
            // dispatch/complete rhythm can explain.
            acc.reg().note_label_flip(issue_number);
        }

        // 4d. Act on a lost claim-then-verify-order tie-break (Issue #6287):
        //     yield BEFORE any real work — no builder spawn, no worktree
        //     creation/entry — for the losing claim. The forge's
        //     `loom:building` label is deliberately left untouched (it is
        //     already correct: idempotent across both racing flips, and
        //     reverting it here would destroy the earlier claimant's only
        //     cross-host mutex out from under its still-live sweep — the
        //     loom#5270 failure mode). Only this host's own, purely local
        //     side effects are unwound: the peer-claim advertisement (3a)
        //     and the claim lock (3), mirroring the #5236/#4689 unwind
        //     branches below for every side effect this dispatch attempt
        //     itself controls exclusively.
        if let Some((earliest_host, earliest_sweep_id)) = lease_order_yield {
            log::warn!(
                "sweep_registry: YIELDING dispatch of issue #{issue_number} sweep_id={sweep_id} \
                 — an earlier claim (host={earliest_host} sweep={earliest_sweep_id}) already owns \
                 it: either a lease comment with an earlier forge-assigned order (#6287 \
                 claim-then-verify-order tie-break, Epic #6165 Phase 2) or a young leaseless \
                 `loom:building` (#9453 Phase 3.1). Standing down before spawning a builder; the \
                 label is left in place since it already protects the winning claimant's claim."
            );
            {
                // One lock hold: retract, release the claim lock, arm the
                // backoff and drop the in-flight idempotency key together, so
                // a same-key retry never gets a hand-back naming a sweep that
                // has already released its claim (#9572/#9573).
                let sr = acc.reg();
                sr.publish_peer_claim(peer_claims::ClaimKind::Retract, issue_number);
                let _ = sr.release_lock_owned(issue_number, &sweep_id);
                // #6350: a lost lease-order tie-break is a no-progress outcome
                // for THIS host exactly like the reaper's #4366 backstop —
                // arming this host's own per-issue dispatch backoff (#4485)
                // means the very next work-finder tick does not immediately
                // re-attempt (and re-lose) the identical race against the same
                // still-live earlier claimant. A redispatch-rate damper only:
                // it never touches the quarantine tally and does not affect
                // the `loom:building` label the earlier claimant still holds.
                sr.record_dispatch_failure(issue_number);
                // #9572: this attempt is abandoned — release the in-flight
                // idempotency key claimed at step 3.05.
                if let Some(ref key) = idempotency_key {
                    sr.inflight_idempotency.remove(key);
                }
            }
            // #10345: a leaseless yield whose `loom:building` event is provably
            // this dispatcher's own flip (fleet actor, inside the flip window,
            // no foreign lease) is a phantom: revert it in the same step so the
            // issue is not stranded. Anything unverifiable keeps the label.
            if let Some((flip_start, flip_end)) = leaseless_flip_window {
                if acc.unlocked(|r| {
                    r.leaseless_yield_is_own_phantom(issue_number, &sweep_id, flip_start, flip_end)
                }) {
                    log::warn!(
                        "sweep_registry: leaseless yield for issue #{issue_number} \
                         sweep_id={sweep_id} is this dispatcher's own phantom `loom:building` \
                         (#10345) - reverting the label before standing down."
                    );
                    if let Err(e) = acc.unlocked(|r| r.restore_label_to_ready(issue_number)) {
                        log::warn!(
                            "sweep_registry: phantom-claim revert for #{issue_number} failed: {e}"
                        );
                    }
                }
            }
            acc.unlocked(|r| {
                r.post_lease_yield_comment(
                    issue_number,
                    &sweep_id,
                    &earliest_host,
                    &earliest_sweep_id,
                );
            });
            return Err(LeaseOrderDispatchError {
                issue: issue_number,
                sweep_id: sweep_id.clone(),
                earliest_host,
                earliest_sweep_id,
            }
            .into());
        }

        // 4e. Post-claim closed-issue re-verification (Issue #11304): release a
        //     claim the fail-open 2.5 guard let through. See `claim_release.rs`.
        if claim_flipped {
            Self::release_claim_if_closed_after_flip_via(
                acc,
                issue_number,
                &sweep_id,
                idempotency_key.as_ref(),
            )?;
        }

        // 5. Compute the log path and spawn the child.
        //
        // Serialize concurrent child startups (Issue #3887): enforce a minimum
        // wall-clock gap since the previous spawn so a burst of back-to-back
        // dispatches does not launch many `claude`/`mcp-loom` startups in the
        // same ~1s window (the 0-HTTPS MCP-init race). `dispatch` holds the
        // registry mutex here, so the brief stagger sleep also serializes the
        // contended startup step across concurrent dispatch callers. A zero
        // stagger (the default outside production / in tests) is a no-op.
        //
        //     #9573: the spacing is *reserved* under the lock
        //     (`reserve_dispatch_stagger` advances `last_spawn_at` to this
        //     dispatch's wake time, so a concurrent dispatch queues behind
        //     it exactly as it queued behind the sleeping lock holder) and
        //     the sleep itself runs with the lock released.
        let wait = acc.reg().reserve_dispatch_stagger();
        acc.pause(wait);
        let sr = acc.reg();
        let log_path = sr.compute_log_path(issue_number);
        let (child, header_anchor) = match sr.spawn_child_process(
            kind,
            &log_path,
            &sweep_id,
            model,
            effort,
            depends_on,
            admission.admitted.as_ref(),
            selection.as_ref(),
        ) {
            Ok(spawned) => spawned,
            Err(e) => {
                // Issue #5236: `spawn_child_process` can fail for reasons that
                // have nothing to do with the issue itself (e.g. a registered
                // workspace whose `.loom/scripts/` is missing
                // `spawn-worker.sh` — `resolve_spawn_bin` errors before any
                // process is ever spawned). Unlike the #4689 branch in
                // `finish_issue_dispatch`, this is a synchronous `Err`, not a
                // synchronously-observed dead child — but the side effects
                // already applied above (claim lock, peer-claim
                // advertisement, label flip) are identical, and leaving them
                // in place is exactly what wedges every retry: the leaked
                // lock's `owner_pid` is this daemon's own (still-alive) pid,
                // which the #4556 live-claim guard then reads as a
                // confirmed-live claim forever (until an operator manually
                // removes the lock dir). Unwind the same three side effects
                // the #4689 branch reverts, so a second dispatch attempt
                // starts from a clean slate instead of a permanent wedge.
                log::warn!(
                    "sweep_registry: issue #{issue_number} sweep_id={sweep_id} failed to spawn \
                     — reverting the claim lock, label, and peer-claim advertisement instead of \
                     leaking them (#5236): {e:#}"
                );
                if !skip_label_flip {
                    let _ = acc.unlocked(|r| r.restore_label_to_ready(issue_number));
                    acc.reg().note_label_flip(issue_number); // #4485 flap detection
                }
                // #9572: also releases the in-flight idempotency key (3.05).
                acc.reg()
                    .unwind_local_claim(issue_number, &sweep_id, idempotency_key.as_ref());
                return Err(e.context("failed to spawn sweep child"));
            }
        };

        Ok(BeginIssueDispatch::Spawned(Box::new(PreparedIssueDispatch {
            child,
            header_anchor,
            log_path,
            issue_number,
            sweep_id,
            kind: kind.clone(),
            idempotency_key,
            model: model.filter(|m| !m.is_empty()).map(String::from),
            effort: effort.filter(|e| !e.is_empty()).map(String::from),
            depends_on,
            admission,
            story_points,
            model_source,
            dispatch_origin,
            mid_spawn,
        })))
    }

    /// Steps 1 and 1b of the begin step: the idempotency-key dedup against
    /// Running entries and against an already-claimed, not-yet-finished
    /// attempt. Pure in-memory. `Some` is the graceful hand-back to return
    /// instead of dispatching. Run once at the top and again right before the
    /// local claim (#9573), because the forge guards in between run with the
    /// registry lock released.
    ///
    /// A 1b hand-back (`was_new: false`) means "an attempt for this key holds
    /// the claim", NOT "a child was spawned": on the `Shared` path the lock is
    /// released between the local claim (3.05) and the spawn, so a same-key
    /// retry can land during the label flip, the lease-order sleeps or the
    /// stagger and be handed back an attempt that still aborts afterwards
    /// (4d lease-order yield, the 4e closed-after-flip release, or a spawn
    /// failure). Each abort clears the key, so the next retry dispatches anew.
    fn idempotency_dedup(
        &self,
        kind: &SweepKind,
        idempotency_key: Option<&str>,
    ) -> Option<BeginIssueDispatch> {
        let key = idempotency_key?;
        {
            if let Some(existing) = self.find_running_by_key(key) {
                return Some(BeginIssueDispatch::Done(Ok(DispatchOutcome {
                    sweep_id: existing.sweep_id.clone(),
                    pid: existing.pid,
                    token_name: existing.token_name.clone(),
                    log_path: existing.log_path.clone(),
                    was_new: false,
                })));
            }

            // 1b. Idempotency dedup against an ALREADY-CLAIMED, not-yet-finished
            //     attempt for the same key (Issue #9572). Step 1 above only
            //     matches `self.entries`, which `finish_issue_dispatch` does not
            //     populate until AFTER the unlocked account-selection poll
            //     (#6592) — so, before this check existed, a same-key retry
            //     landing inside that window missed both dedup checks and fell
            //     through to the guard chain, which refused it with a hard
            //     `Err` (`lock collision` or the #4556 live-claim guard) instead
            //     of the graceful hand-back a retry gets before or after the
            //     window. `self.inflight_idempotency` is written below, right
            //     after this attempt claims the lock (step 3), and cleared by
            //     `finish_issue_dispatch` — on both its success and error paths
            //     — and by every early `Err` return in this function from that
            //     point onward, so a hit here can only ever name a genuinely
            //     still-in-flight sweep, never a stale/abandoned one.
            //
            //     `pid`/`token_name` are genuinely unknown this early — the
            //     child has not been spawned yet — so this reports `pid: 0` and
            //     `UNKNOWN_TOKEN_NAME`, the same "not yet known" sentinels the
            //     #4689/#6614 preflight-death paths already use elsewhere.
            //     `log_path` is recomputed (not stored) since it is a pure
            //     function of `issue_number`, already available from `kind`.
            if let Some(sweep_id) = self.inflight_idempotency.get(key).cloned() {
                let log_path = match kind {
                    SweepKind::Issue(n) => self.compute_log_path(*n),
                    SweepKind::PrSet(prs) => self.compute_prset_log_path(prs),
                    SweepKind::Step { item, role } => self.compute_step_log_path(*item, *role),
                };
                return Some(BeginIssueDispatch::Done(Ok(DispatchOutcome {
                    sweep_id,
                    pid: 0,
                    token_name: crate::sweep_registry::UNKNOWN_TOKEN_NAME.to_string(),
                    log_path,
                    was_new: false,
                })));
            }
        }
        None
    }
}
