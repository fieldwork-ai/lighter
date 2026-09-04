//! Threads that are kept, not made.
//!
//! A stream lives on two blocking threads, one per direction, for as long as
//! the connection does, so a fixed pool would be a deadlock waiting for its
//! thousandth connection. But spawning them is most of what a connection
//! costs at thousands a second — a stack mapping and a scheduler round trip
//! each. So a thread that finishes a job waits a while for the next one
//! before it goes: connection churn reuses threads, a burst grows the set,
//! and an idle machine sheds them.

use std::sync::Mutex;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::Duration;

type Job = Box<dyn FnOnce() + Send + 'static>;

/// How long an idle thread waits for work before exiting.
const IDLE: Duration = Duration::from_secs(30);

struct Idle {
    /// Senders to parked threads, each waiting for one job, tagged with the
    /// thread's number so a thread can take its own entry back.
    parked: Vec<(u64, Sender<Job>)>,
}

static IDLE_THREADS: Mutex<Idle> = Mutex::new(Idle { parked: Vec::new() });
static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Runs `job` on a parked thread if there is one, or a new one.
pub fn run(name: &'static str, stack: usize, job: impl FnOnce() + Send + 'static) {
    let mut job: Job = Box::new(job);
    loop {
        let sender = IDLE_THREADS
            .lock()
            .expect("worker cache poisoned")
            .parked
            .pop();
        match sender {
            Some((_, sender)) => match sender.send(job) {
                Ok(()) => return,
                // The thread gave up waiting between our pop and our send;
                // the job comes back and the next sender is tried.
                Err(returned) => job = returned.0,
            },
            None => break,
        }
    }
    let (tx, rx) = channel::<Job>();
    let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let _ = std::thread::Builder::new()
        .name(name.into())
        .stack_size(stack)
        .spawn(move || worker(id, tx, rx, job));
}

fn worker(id: u64, tx: Sender<Job>, rx: Receiver<Job>, first: Job) {
    crate::qos::raise_interactive();
    let mut job = first;
    loop {
        job();
        // Park: offer ourselves, then wait for a job or the idle limit. A
        // sender left in the cache after we have gone fails its `send`, and
        // the caller moves on to the next.
        IDLE_THREADS
            .lock()
            .expect("worker cache poisoned")
            .parked
            .push((id, tx.clone()));
        match rx.recv_timeout(IDLE) {
            Ok(next) => job = next,
            Err(RecvTimeoutError::Timeout) => {
                // Take our sender back out if it is still there, so a caller
                // does not waste a try on us; a job that races in anyway is
                // run rather than dropped.
                let mut idle = IDLE_THREADS.lock().expect("worker cache poisoned");
                idle.parked.retain(|(other, _)| *other != id);
                drop(idle);
                match rx.try_recv() {
                    Ok(next) => job = next,
                    Err(_) => return,
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// For a thread count in diagnostics.
pub fn idle_count() -> usize {
    IDLE_THREADS
        .lock()
        .expect("worker cache poisoned")
        .parked
        .len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn a_finished_thread_is_reused() {
        let ran = Arc::new(AtomicUsize::new(0));
        let (done_tx, done_rx) = channel();
        for _ in 0..3 {
            let ran = ran.clone();
            let done_tx = done_tx.clone();
            run("test-worker", 64 << 10, move || {
                ran.fetch_add(1, Ordering::SeqCst);
                let _ = done_tx.send(std::thread::current().id());
            });
            // Sequential: wait for each job so the thread parks before the
            // next request.
            let _ = done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(ran.load(Ordering::SeqCst), 3);
        assert!(idle_count() >= 1, "a parked thread should be waiting");
    }
}
