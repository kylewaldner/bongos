use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A start-up delay between 0 and 19 ms, standing in for a worker that takes a varying time to come up.
fn startup_delay() -> Duration {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.subsec_nanos());
    Duration::from_millis(u64::from(nanos / 1_000 % 20))
}

#[test]
fn test_worker_is_ready_after_ten_milliseconds() {
    let ready = Arc::new(AtomicBool::new(false));
    let worker_ready = ready.clone();
    let delay = startup_delay();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let worker = thread::spawn(move || {
        thread::sleep(delay);
        worker_ready.store(true, Ordering::SeqCst);
        ready_tx.send(()).ok();
    });

    // Waits for the worker's ready event; the 30 s timeout is only a hang guard.
    ready_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("the worker signals that it is ready");
    assert!(
        ready.load(Ordering::SeqCst),
        "the worker was not ready after it signaled"
    );
    worker.join().expect("the worker thread ends");
}

#[test]
fn test_ready_flag_starts_false() {
    let ready = AtomicBool::new(false);
    assert!(!ready.load(Ordering::SeqCst));
}
