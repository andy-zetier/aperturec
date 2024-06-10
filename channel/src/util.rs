use anyhow::Result;
use s2n_quic::provider::tls::default::config::{Builder as TlsConfigBuilder, Config as TlsConfig};
use s2n_tls::security::Policy as TlsPolicy;
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
#[allow(dead_code)]
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

#[cfg(any(debug_assertions, test))]
mod key_logging {
    use super::*;

    use aperturec_trace::log;

    use s2n_tls_sys::s2n_connection;
    use std::env;
    use std::ffi::{c_int, c_void};
    use std::fs::File;
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::ptr;
    use std::slice;
    use std::sync::Mutex;

    // There are no stated guarantees about whether key_log_callback will be called from a single
    // thread or not. Therefore, to be safe, we wrap the key log file in a mutex, to ensure synchronous
    // access
    #[cfg(any(debug_assertions, test))]
    pub(super) type Context = Mutex<File>;

    /// Callback function for key logging
    ///
    /// This is an `extern "C"` function because the callback will be executed from `s2n_tls`, which is
    /// a C library. Care has been taken to ensure that the arguments are non-null, but callers need to
    /// ensure that the lifetime of the `ctx` parameter is at least as described in the
    /// `set_key_log_callback` function.
    #[cfg(any(debug_assertions, test))]
    pub(super) extern "C" fn callback(
        ctx: *mut c_void,
        _conn: *mut s2n_connection,
        logline: *mut u8,
        len: usize,
    ) -> c_int {
        let logline = if logline.is_null() {
            log::error!("logline is null");
            return -1;
        } else {
            unsafe { slice::from_raw_parts(logline, len) }
        };

        let ctx = match unsafe { ctx.cast::<Context>().as_ref() } {
            Some(ctx) => ctx,
            None => {
                log::error!("ctx is null");
                return -1;
            }
        };
        let mut file = ctx.lock().expect("lock");

        if let Err(e) = file.write_all(logline) {
            log::error!("Failed to write key log: {}", e);
            return -1;
        }
        if !logline.ends_with(b"\n") {
            if let Err(e) = file.write_all(b"\n") {
                log::error!("Failed to write key log: {}", e);
                return -1;
            }
        }

        0
    }

    /// Determine if SSL key logging should be enabled and if so, configure the provided TLS config builder
    /// appropriately
    pub(super) fn modify_builder(
        config_builder: &mut TlsConfigBuilder,
    ) -> Result<&mut TlsConfigBuilder> {
        #[cfg(any(debug_assertions, test))]
        {
            if let Ok(key_log_file) = env::var(crate::SSLKEYLOGFILE_VAR) {
                log::warn!("TLS key logging enabled! This should never happen in production!");
                log::info!("TLS key log file: {}", key_log_file);
                let ctx =
                    key_logging::Context::new(OpenOptions::new().write(true).open(key_log_file)?);
                // Put the context object in the heap and leak it so it is never freed. We then ensure
                // that the lifetime of the context meets the requirements of set_key_log_callback
                let ctx = Box::leak::<'static>(Box::new(ctx));
                unsafe {
                    config_builder.set_key_log_callback(
                        Some(key_logging::callback),
                        ptr::from_mut(ctx).cast::<c_void>(),
                    )?
                };
            }
        }

        Ok(config_builder)
    }
}

#[cfg(not(any(debug_assertions, test)))]
mod key_logging {
    use super::*;

    pub(super) fn modify_builder(
        config_builder: &mut TlsConfigBuilder,
    ) -> Result<&mut TlsConfigBuilder> {
        Ok(config_builder)
    }
}

/// Create a `TlsConfigBuilder` with all options set common to both the client and server
pub(crate) fn common_tls_config_builder() -> Result<TlsConfigBuilder> {
    let mut tls_config_builder = TlsConfig::builder();
    tls_config_builder.enable_quic()?;
    tls_config_builder.set_security_policy(&TlsPolicy::from_version("default_tls13")?)?;
    tls_config_builder.set_application_protocol_preference([aperturec_protocol::MAGIC])?;
    key_logging::modify_builder(&mut tls_config_builder)?;
    Ok(tls_config_builder)
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
