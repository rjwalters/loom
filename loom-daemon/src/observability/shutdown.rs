//! A bounded final drain through the existing sender, never a second consumer.
use super::{exporter::Exporter, queue::DurableQueue, ExportStatus};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

pub(super) struct Request {
    deadline: Instant,
    reply: oneshot::Sender<()>,
}
static SENDERS: Mutex<Vec<mpsc::Sender<Request>>> = Mutex::new(Vec::new());

pub(super) fn spawn_sender<E: Exporter + 'static>(
    queue: Arc<DurableQueue>,
    exporter: E,
    batch_size: usize,
    interval: Duration,
    status: Arc<ExportStatus>,
) -> tokio::task::JoinHandle<()> {
    let (tx, rx) = mpsc::channel::<Request>(1);
    {
        let mut senders = SENDERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        senders.retain(|sender| !sender.is_closed());
        if senders.len() < 64 {
            senders.push(tx);
        } else {
            log::warn!("observability: shutdown sender registration limit reached");
        }
    }
    tokio::spawn(async move {
        super::sender::run_sender(queue, &exporter, batch_size, interval, status, rx).await;
    })
}

async fn request(rx: &mut mpsc::Receiver<Request>) -> Request {
    match rx.recv().await {
        Some(request) => request,
        None => std::future::pending().await,
    }
}

fn reply(request: Request, complete: bool) {
    if !complete {
        log::warn!("observability: final drain incomplete; remaining queue retained for restart");
    }
    let _ = request.reply.send(());
}

/// A sleeping sender is at a safe export boundary and can drain immediately.
pub(super) async fn pause<E: Exporter>(
    queue: &DurableQueue,
    exporter: &E,
    batch_size: usize,
    status: &ExportStatus,
    rx: &mut mpsc::Receiver<Request>,
    delay: Duration,
) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(delay) => true,
        request = request(rx) => {
            let complete = drain_until(queue, exporter, batch_size, status, request.deadline).await;
            reply(request, complete);
            false
        }
    }
}

/// Never cancel an in-flight request and immediately re-send its batch. Allow
/// its acknowledged prefix to commit before draining the remainder. If the
/// shared shutdown deadline expires first, stop without initiating a retry.
pub(super) async fn flush<E: Exporter>(
    queue: &DurableQueue,
    exporter: &E,
    batch_size: usize,
    status: &ExportStatus,
    rx: &mut mpsc::Receiver<Request>,
) -> Option<super::sender::FlushOutcome> {
    let pending = super::sender::try_flush(queue, exporter, batch_size, status);
    tokio::pin!(pending);
    tokio::select! {
        outcome = &mut pending => Some(outcome),
        request = request(rx) => {
            let complete = match tokio::time::timeout_at(request.deadline, &mut pending).await {
                Ok(super::sender::FlushOutcome::Sent(_) | super::sender::FlushOutcome::Empty) =>
                    drain_until(queue, exporter, batch_size, status, request.deadline).await,
                Ok(super::sender::FlushOutcome::Failed) | Err(_) => false,
            };
            reply(request, complete);
            None
        }
    }
}

async fn drain_until<E: Exporter>(
    queue: &DurableQueue,
    exporter: &E,
    batch_size: usize,
    status: &ExportStatus,
    deadline: Instant,
) -> bool {
    tokio::time::timeout_at(deadline, async {
        loop {
            match super::sender::try_flush(queue, exporter, batch_size, status).await {
                super::sender::FlushOutcome::Empty => return exporter.flush().await.is_ok(),
                super::sender::FlushOutcome::Sent(_) => {}
                super::sender::FlushOutcome::Failed => return false,
            }
        }
    })
    .await
    .unwrap_or(false)
}

pub async fn flush_before_shutdown(budget: Duration) {
    let senders = {
        let mut registry = SENDERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut *registry)
    };
    let deadline = Instant::now() + budget;
    let mut replies = Vec::new();
    for sender in senders {
        let (reply, received) = oneshot::channel();
        if sender.try_send(Request { deadline, reply }).is_ok() {
            replies.push(received);
        }
    }
    let _ = tokio::time::timeout_at(deadline, async {
        for reply in replies {
            let _ = reply.await;
        }
    })
    .await;
}

/// Intercept shutdown while still in the async IPC layer. The legacy request
/// dispatcher is synchronous and cannot await a final drain.
pub async fn intercept(request: &crate::types::Request) {
    if matches!(request, crate::types::Request::Shutdown) {
        log::info!("Shutdown requested; draining telemetry before exit");
        exit(crate::ipc::EXIT_SHUTDOWN).await;
    }
}

/// Signal/IPC shutdown preserves active work; it exports only already-completed
/// queued spans. SIGKILL cannot run this path and relies on persisted state.
pub async fn exit(code: i32) -> ! {
    flush_before_shutdown(Duration::from_secs(2)).await;
    std::process::exit(code)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::telemetry::{
        RepoVisibility, SweepStartedRecord, TelemetryEnvelope, TelemetryRecord,
    };
    struct Sink {
        stall: bool,
    }
    impl Exporter for Sink {
        async fn emit_batch(
            &self,
            _: &[TelemetryEnvelope],
        ) -> Result<(), super::super::exporter::ExportError> {
            if self.stall {
                std::future::pending::<()>().await;
            }
            Ok(())
        }
    }
    #[tokio::test]
    #[serial_test::serial]
    async fn shutdown_deadline_cancels_inflight_send_without_starting_another() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct PendingSink {
            active: Arc<AtomicUsize>,
            peak: Arc<AtomicUsize>,
            entered: Arc<tokio::sync::Notify>,
        }
        struct Active(Arc<AtomicUsize>);
        impl Drop for Active {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        impl Exporter for PendingSink {
            async fn emit_batch(
                &self,
                _: &[TelemetryEnvelope],
            ) -> Result<(), super::super::exporter::ExportError> {
                let count = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                let _active = Active(self.active.clone());
                self.peak.fetch_max(count, Ordering::SeqCst);
                self.entered.notify_one();
                std::future::pending().await
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.jsonl");
        let queue = Arc::new(DurableQueue::open(path.clone(), 5));
        queue.push(TelemetryEnvelope::new(
            "host",
            TelemetryRecord::SweepStarted(SweepStartedRecord {
                repo: "test/fixture".into(),
                visibility: RepoVisibility::Private,
                issue: 18,
                sweep_id: "shutdown".into(),
                started_at: chrono::Utc::now(),
                model: None,
                effort: None,
                runtime: None,
            }),
        ));
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(tokio::sync::Notify::new());
        let task = spawn_sender(
            queue,
            PendingSink {
                active: active.clone(),
                peak: peak.clone(),
                entered: entered.clone(),
            },
            5,
            Duration::from_millis(1),
            Arc::new(ExportStatus::started("host", "http://localhost", "otlp", 30)),
        );
        tokio::time::timeout(Duration::from_secs(5), entered.notified())
            .await
            .unwrap();
        flush_before_shutdown(Duration::from_millis(30)).await;
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            peak.load(Ordering::SeqCst),
            1,
            "sender and final drain cannot consume concurrently"
        );
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(DurableQueue::open(path, 5).len(), 1);
    }

    #[tokio::test]
    async fn final_drain_is_bounded_and_retains_unsent_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.jsonl");
        let queue = DurableQueue::open(path.clone(), 5);
        queue.push(TelemetryEnvelope::new(
            "host",
            TelemetryRecord::SweepStarted(SweepStartedRecord {
                repo: "test/fixture".into(),
                visibility: RepoVisibility::Private,
                issue: 18,
                sweep_id: "fixture".into(),
                started_at: chrono::Utc::now(),
                model: None,
                effort: None,
                runtime: None,
            }),
        ));
        let status = ExportStatus::started("host", "http://localhost", "otlp", 30);
        assert!(
            !drain_until(
                &queue,
                &Sink { stall: true },
                5,
                &status,
                Instant::now() + Duration::from_millis(30)
            )
            .await
        );
        assert_eq!(DurableQueue::open(path, 5).len(), 1);
        assert!(
            drain_until(
                &queue,
                &Sink { stall: false },
                5,
                &status,
                Instant::now() + Duration::from_secs(1)
            )
            .await
        );
        assert!(queue.is_empty());
    }
}
