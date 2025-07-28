//! Utilities for running CPU-bound tasks on a Rayon thread pool from async contexts.
use tokio::sync::oneshot;

/// Spawns a CPU-bound task on the Rayon thread pool.
///
/// This function takes a closure `f` that performs a CPU-bound computation,
/// runs it on a Rayon thread, and returns a future that resolves to its result.
/// If the CPU-bound task panics, the panic is propagated to the awaiter.
pub async fn spawn<F, T>(f: F) -> T
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = oneshot::channel();
    rayon::spawn(move || {
        let _ = tx.send(f());
    });
    // Propagate the panic up to the caller
    rx.await.expect("cpu-bound task panicked")
}
