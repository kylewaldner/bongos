use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

#[test]
fn test_worker_is_ready_after_ten_milliseconds() {
    let ready = Arc::new(AtomicBool::new(false));
    let worker_ready = ready.clone();
    let (ready_tx, ready_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        thread::sleep(Duration::from_millis(5));
        worker_ready.store(true, Ordering::SeqCst);
        ready_tx.send(()).ok();
    });

    // The timeout only guards against a hang; readiness is signaled over the channel.
    ready_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("the worker signals that it is ready");
    assert!(
        ready.load(Ordering::SeqCst),
        "the worker signaled ready without setting the flag"
    );
    worker.join().expect("the worker thread ends");
}

#[test]
fn test_ready_flag_starts_false() {
    let ready = AtomicBool::new(false);
    assert!(!ready.load(Ordering::SeqCst));
}
