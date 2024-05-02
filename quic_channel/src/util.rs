use anyhow::Result;
use std::future::Future;
use tokio::runtime::Runtime as TokioRuntime;

/// Call async stuff synchronously on a provided tokio runtime. This trait is implemented on all
/// types which implement [`Future`], allowing synchronous code to await any future simply by
/// calling `.syncify(&runtime)`.
///
/// Note that some types must have an asynchronous executor available on creation. For these types,
/// use the [`SyncifyLazy`] trait to ensure that the creation of your type happens within the
/// asynchronous executor context.
///
/// See the tokio documentation, specifically
/// [`Runtime::enter`](https://docs.rs/tokio/latest/tokio/runtime/struct.Runtime.html#method.enter)
/// for details.
pub(crate) trait Syncify {
    type Output;

    fn syncify(self, runtime: &TokioRuntime) -> Self::Output;
}

impl<F, O> Syncify for F
where
    F: Future<Output = O>,
{
    type Output = O;

    fn syncify(self, runtime: &TokioRuntime) -> Self::Output {
        runtime.block_on(self)
    }
}

/// Call async stuff synchronously on a provided tokio runtime. This trait is implemented on
/// closures so that the closure is executed within the runtime, allowing things like
/// [`tokio::time::sleep`](https://docs.rs/tokio/latest/tokio/time/fn.sleep.html) to execute
/// without panicking.
///
/// This should likely never be implemented directly, as the blanket implementation on [`FnOnce`]
/// types should be sufficient.
///
/// If you do not care if your code is executed within the provided runtime, and only care if your
/// future is awaited in the provided runtime, consider the more ergonomic [`Syncify`] trait.
pub(crate) trait SyncifyLazy {
    /// Output type of the async callable
    type Output;

    /// Execute the async callable on the provided runtime, running it to completion
    fn syncify_lazy(self, runtime: &TokioRuntime) -> Self::Output;
}

impl<F, O, OF> SyncifyLazy for F
where
    F: FnOnce() -> OF,
    OF: Future<Output = O>,
{
    type Output = O;

    fn syncify_lazy(self, runtime: &TokioRuntime) -> Self::Output {
        runtime.block_on(async { self().await })
    }
}

/// Create a new async runtime with all the tokio features enabled
pub(crate) fn new_async_rt() -> Result<TokioRuntime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?)
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn syncify_sleep() {
        use std::time::{Duration, Instant};

        let rt = new_async_rt().expect("rt");

        let start = Instant::now();
        (|| tokio::time::sleep(Duration::from_millis(100))).syncify_lazy(&rt);
        let end = Instant::now();

        assert!(end - start >= Duration::from_millis(100));
    }

    #[test]
    fn syncify_io() {
        use rand::{distributions::Standard, Rng};

        let rt = new_async_rt().expect("rt");

        let data: Vec<u8> = rand::thread_rng()
            .sample_iter(&Standard)
            .take(256)
            .collect();
        let mut copy = vec![];
        tokio::io::copy_buf(&mut data.as_slice(), &mut copy)
            .syncify(&rt)
            .expect("copy");

        assert_eq!(data, copy);
    }

    #[test]
    fn syncify_multiple_rts() {
        use std::time::{Duration, Instant};

        let rt0 = new_async_rt().expect("rt 0");
        let rt1 = new_async_rt().expect("rt 1");

        let start0 = Instant::now();
        (|| tokio::time::sleep(Duration::from_millis(100))).syncify_lazy(&rt0);
        let end0 = Instant::now();
        let start1 = Instant::now();
        (|| tokio::time::sleep(Duration::from_millis(200))).syncify_lazy(&rt1);
        let end1 = Instant::now();

        assert!(end0 - start0 >= Duration::from_millis(100));
        assert!(end1 - start1 >= Duration::from_millis(200));
        assert!(end1 - start0 >= Duration::from_millis(300));
    }
}
