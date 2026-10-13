use alexandria_pipeline::embedding::granite::worker::Worker;
use std::future::{Future, poll_fn};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::Poll;
use tokio::sync::mpsc;

// Only the forward operation is substituted. The real OS thread, bounded queue,
// admission backpressure and response/cancellation channels remain under test.
struct Controlled {
    worker: Worker,
    started: mpsc::UnboundedReceiver<String>,
    release: std::sync::mpsc::Sender<()>,
    completed: Arc<AtomicUsize>,
}

impl Controlled {
    async fn new(panic_after_release: bool) -> Self {
        let (started_tx, started) = mpsc::unbounded_channel();
        let (release, gate) = std::sync::mpsc::channel();
        let completed = Arc::new(AtomicUsize::new(0));
        let counter = completed.clone();
        let worker = Worker::start(move || {
            Ok(move |text: &str| {
                started_tx.send(text.to_owned()).unwrap();
                gate.recv().unwrap();
                assert!(!panic_after_release, "synthetic forward panic");
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(vec![text.len() as f32])
            })
        })
        .await
        .unwrap();
        Self {
            worker,
            started,
            release,
            completed,
        }
    }
}

impl Drop for Controlled {
    fn drop(&mut self) {
        // Failed assertions must not leave a test forward blocked on its gate.
        for _ in 0..8 {
            let _ = self.release.send(());
        }
    }
}

#[tokio::test]
async fn one_active_one_queued() {
    let mut control = Controlled::new(false).await;
    let first = control.worker.submit("a").await.unwrap();
    assert_eq!(control.started.recv().await.as_deref(), Some("a"));
    let second = control.worker.submit("bb").await.unwrap();
    let third = control.worker.submit("ccc");
    tokio::pin!(third);
    assert!(matches!(
        poll_fn(|cx| Poll::Ready(third.as_mut().poll(cx))).await,
        Poll::Pending
    ));
    control.release.send(()).unwrap();
    assert_eq!(first.receive().await.unwrap(), vec![1.0]);
    let third = third.await.unwrap();
    assert_eq!(control.started.recv().await.as_deref(), Some("bb"));
    control.release.send(()).unwrap();
    assert_eq!(second.receive().await.unwrap(), vec![2.0]);
    assert_eq!(control.started.recv().await.as_deref(), Some("ccc"));
    control.release.send(()).unwrap();
    assert_eq!(third.receive().await.unwrap(), vec![3.0]);
}

#[tokio::test]
async fn cancelled_queued_job_not_executed() {
    let mut control = Controlled::new(false).await;
    let active = control.worker.submit("active").await.unwrap();
    assert_eq!(control.started.recv().await.as_deref(), Some("active"));
    let cancelled = control.worker.submit("cancelled").await.unwrap();
    drop(cancelled);
    control.release.send(()).unwrap();
    active.receive().await.unwrap();
    let after = control.worker.submit("after").await.unwrap();
    assert_eq!(control.started.recv().await.as_deref(), Some("after"));
    control.release.send(()).unwrap();
    assert_eq!(after.receive().await.unwrap(), vec![5.0]);
    assert_eq!(control.completed.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn cancelled_active_job_finishes_without_delivery() {
    let mut control = Controlled::new(false).await;
    let active = control.worker.submit("active").await.unwrap();
    assert_eq!(control.started.recv().await.as_deref(), Some("active"));
    drop(active);
    let after = control.worker.submit("after").await.unwrap();
    control.release.send(()).unwrap();
    assert_eq!(control.started.recv().await.as_deref(), Some("after"));
    assert_eq!(
        control.completed.load(Ordering::SeqCst),
        1,
        "an active forward finishes rather than being interrupted"
    );
    control.release.send(()).unwrap();
    assert_eq!(after.receive().await.unwrap(), vec![5.0]);
}

#[tokio::test]
async fn ordered_bulk_results() {
    let worker = Worker::start(|| Ok(|text: &str| Ok(vec![text.len() as f32])))
        .await
        .unwrap();
    assert_eq!(
        worker.embed(&["one", "longer", "x"]).await.unwrap(),
        vec![vec![3.0], vec![6.0], vec![1.0]]
    );
}

#[tokio::test]
async fn worker_exit_closes_pending_requests() {
    let mut control = Controlled::new(true).await;
    let active = control.worker.submit("active").await.unwrap();
    assert_eq!(control.started.recv().await.as_deref(), Some("active"));
    let queued = control.worker.submit("queued").await.unwrap();
    control.release.send(()).unwrap();
    assert!(active.receive().await.is_err());
    assert!(queued.receive().await.is_err());
    assert!(control.worker.submit("no replacement").await.is_err());
}

#[tokio::test]
async fn empty_bulk_returns_empty() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let worker = Worker::start(move || {
        Ok(move |_: &str| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(vec![1.0])
        })
    })
    .await
    .unwrap();
    assert!(worker.embed(&[]).await.unwrap().is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn initialization_finishes_before_readiness() {
    let (started, mut signal) = mpsc::unbounded_channel();
    let (release, gate) = std::sync::mpsc::channel();
    let start = Worker::start(move || {
        started.send(()).unwrap();
        gate.recv().unwrap();
        Ok(|_: &str| Ok(vec![1.0]))
    });
    tokio::pin!(start);
    assert!(matches!(
        poll_fn(|cx| Poll::Ready(start.as_mut().poll(cx))).await,
        Poll::Pending
    ));
    signal.recv().await.unwrap();
    release.send(()).unwrap();
    let worker = start.await.unwrap();
    assert_eq!(worker.embed(&["ready"]).await.unwrap(), vec![vec![1.0]]);
}

#[tokio::test]
async fn initialization_failure_refuses_readiness() {
    type Forward = fn(&str) -> anyhow::Result<Vec<f32>>;
    let failed =
        Worker::start(|| -> anyhow::Result<Forward> { anyhow::bail!("synthetic load failure") })
            .await;
    assert!(failed.is_err());
}

#[tokio::test]
async fn model_is_owned_on_the_named_worker_thread() {
    let worker = Worker::start(|| {
        assert_eq!(std::thread::current().name(), Some("alexandria-granite"));
        let state = std::rc::Rc::new(7.0_f32);
        Ok(move |_: &str| Ok(vec![*state]))
    })
    .await
    .unwrap();
    assert_eq!(worker.embed(&["owned"]).await.unwrap(), vec![vec![7.0]]);
}
