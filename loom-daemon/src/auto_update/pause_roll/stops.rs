//! H4's tree stops, in parallel and bounded (issue #11051).
//!
//! On the first self-applied floor roll H4 stopped items one after another,
//! several `systemctl --user stop` calls each waited out a 20 s timeout, and
//! the items that missed their safe point were reached only after the 120 s
//! budget had passed. The pause took 2-3 minutes per host. So:
//!
//! - **Stops run on a pool** ([`StopPool`], at most
//!   [`PauseRollTuning::stop_concurrency`] at once). H4 hands a tree to the
//!   pool and goes on: young and unresumable items, items that have parked,
//!   and at the deadline every budget-missed item at once.
//! - **The commit stays on H4's thread.** [`Run::start_stop`] commits the
//!   pause under the drain lock before it hands a tree over, exactly as the
//!   serial stop did, so the operator-interplay rules are unchanged.
//! - **The stop phase is bounded** at the budget plus
//!   [`PauseRollTuning::stop_margin`]. A tree whose teardown has not returned
//!   by then has its process group `SIGKILL`ed ([`PauseHost::force_kill`],
//!   which runs no external command) and is recorded stopped; its teardown
//!   thread is left behind, and the daemon exits soon after.
//!
//! What each stop records (the `stopped` event, `teardown_ms`, `stopped_at`,
//! the paused item's `daemon.roll.item`) is written when its teardown
//! finishes, so the dispositions and events are the serial stop's.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use chrono::Utc;

use super::host::{Candidate, PauseHost};
use super::teardown::TeardownReport;
use super::{PauseRollTuning, Run};
use crate::auto_update::pause_manifest::ItemStatus;

type Job = (usize, Candidate);
type Done = (usize, TeardownReport);

/// Runs H4's teardowns on at most `max` worker threads.
pub(super) struct StopPool {
    host: Arc<dyn PauseHost>,
    jobs: Option<Sender<Job>>,
    queue: Arc<Mutex<Receiver<Job>>>,
    done_tx: Sender<Done>,
    done: Receiver<Done>,
    /// Set when the stop bound passes: a job still queued is not started.
    closed: Arc<AtomicBool>,
    workers: usize,
    max: usize,
    /// Items handed over whose teardown has not been collected.
    pending: BTreeSet<usize>,
}

impl StopPool {
    pub(super) fn new(host: Arc<dyn PauseHost>, max: usize) -> Self {
        let (jobs, queue) = mpsc::channel();
        let (done_tx, done) = mpsc::channel();
        Self {
            host,
            jobs: Some(jobs),
            queue: Arc::new(Mutex::new(queue)),
            done_tx,
            done,
            closed: Arc::new(AtomicBool::new(false)),
            workers: 0,
            max: max.max(1),
            pending: BTreeSet::new(),
        }
    }

    /// Hand `idx`'s tree to the pool. Returns at once.
    fn submit(&mut self, idx: usize, cand: Candidate) {
        self.pending.insert(idx);
        if self.workers < self.max && self.workers < self.pending.len() {
            self.spawn_worker();
        }
        if let Some(jobs) = &self.jobs {
            let _ = jobs.send((idx, cand));
        }
    }

    fn spawn_worker(&mut self) {
        self.workers += 1;
        let (host, queue, done, closed) = (
            Arc::clone(&self.host),
            Arc::clone(&self.queue),
            self.done_tx.clone(),
            Arc::clone(&self.closed),
        );
        std::thread::spawn(move || loop {
            // Holding the lock across `recv` is what hands each job to one
            // worker; the others wait on the lock, not on a busy loop.
            let job = queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .recv();
            let Ok((idx, cand)) = job else {
                return; // the pool is gone
            };
            if closed.load(Ordering::SeqCst) {
                continue; // past the bound: H4 already force-killed it
            }
            let report = host.teardown(&cand);
            if done.send((idx, report)).is_err() {
                return;
            }
        });
    }

    pub(super) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Every finished teardown, without waiting.
    fn finished(&mut self) -> Vec<Done> {
        let out: Vec<Done> = self.done.try_iter().collect();
        for (idx, _) in &out {
            self.pending.remove(idx);
        }
        out
    }

    /// The next finished teardown, waiting until `until` at most.
    fn next_until(&mut self, until: Instant) -> Option<Done> {
        let left = until.saturating_duration_since(Instant::now());
        match self.done.recv_timeout(left) {
            Ok(done) => {
                self.pending.remove(&done.0);
                Some(done)
            }
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => None,
        }
    }

    /// Stop waiting: what is still pending is returned, and a queued job is
    /// never started.
    fn abandon(&mut self) -> Vec<usize> {
        self.closed.store(true, Ordering::SeqCst);
        std::mem::take(&mut self.pending).into_iter().collect()
    }
}

impl Drop for StopPool {
    fn drop(&mut self) {
        // Idle workers see the queue close and exit.
        self.jobs = None;
    }
}

impl PauseRollTuning {
    /// The longest H4's stop phase (steps 4-7) may take: the pause budget and
    /// the stop margin (#11051).
    #[must_use]
    pub fn stop_bound(&self) -> std::time::Duration {
        self.pause_budget + self.stop_margin
    }
}

impl Run<'_> {
    /// Commit the pause (under the drain lock), then hand `idx`'s tree to the
    /// pool. `false` when the pause no longer owns the drain, so nothing was
    /// handed over.
    pub(super) fn start_stop(&mut self, pool: &mut StopPool, idx: usize) -> bool {
        if !self.drain.pause_commit_stop(self.gen()) {
            return false;
        }
        pool.submit(idx, self.work[idx].cand.clone());
        true
    }

    /// Record every teardown that has finished. `true` when one had.
    pub(super) fn collect_stops(&mut self, pool: &mut StopPool) -> bool {
        let done = pool.finished();
        let any = !done.is_empty();
        for (idx, report) in done {
            self.record_stop(idx, &report);
        }
        any
    }

    /// Wait for the teardowns still running, until `bound`. A tree whose
    /// teardown has not returned by then has its process group killed and is
    /// recorded stopped.
    pub(super) fn finish_stops(&mut self, pool: &mut StopPool, bound: Instant) {
        while pool.has_pending() {
            match pool.next_until(bound) {
                Some((idx, report)) => self.record_stop(idx, &report),
                None => break,
            }
        }
        for idx in pool.abandon() {
            self.force_stop(idx, bound);
        }
    }

    /// What a finished teardown records.
    fn record_stop(&mut self, idx: usize, report: &TeardownReport) {
        let w = &mut self.work[idx];
        w.teardown_ms = Some(report.elapsed_ms);
        w.item.stopped_at = Some(Utc::now());
        let id = w.item.id.clone();
        if !report.survivors.is_empty() {
            log::error!(
                "pause_roll: {id}: {} process(es) survived the teardown: {:?}",
                report.survivors.len(),
                report.survivors
            );
        }
        let detail = format!(
            "{} pid(s){}{}",
            report.pids.len(),
            if report.scope_stopped {
                ", scope stopped"
            } else {
                ""
            },
            report
                .scope_note
                .as_deref()
                .map_or_else(String::new, |n| format!(", {n}; used the process tree"))
        );
        // The teardown signalled less than the recorded tree (a recycled pid,
        // or no process table): say so in the log and in the manifest.
        let detail = match &report.reach_note {
            Some(note) => {
                log::error!("pause_roll: {id}: {note}");
                format!("{detail}; {note}")
            }
            None => detail,
        };
        self.event(Some(&id), "stopped", Some(detail));
        self.report_if_paused(idx);
    }

    /// The stop bound passed with `idx`'s teardown still running.
    fn force_stop(&mut self, idx: usize, bound: Instant) {
        let killed = self.host.force_kill(&self.work[idx].cand);
        let w = &mut self.work[idx];
        w.item.stopped_at = Some(Utc::now());
        let id = w.item.id.clone();
        let bound_secs = self.plan.tuning.stop_bound().as_secs();
        let detail = format!(
            "teardown did not finish within the {bound_secs}s stop bound; {}",
            if killed {
                "SIGKILL sent to the process group"
            } else {
                "no process group left to signal"
            }
        );
        log::error!(
            "pause_roll: {id}: {detail} ({}ms past it)",
            Instant::now().saturating_duration_since(bound).as_millis()
        );
        self.event(Some(&id), "stopped", Some(detail));
        self.report_if_paused(idx);
    }

    /// A paused item's `daemon.roll.item` goes out once its tree is stopped,
    /// so it carries the teardown time.
    fn report_if_paused(&mut self, idx: usize) {
        if self.work[idx].item.status == ItemStatus::Paused {
            self.report(idx, "none");
        }
    }
}
