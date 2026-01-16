//! C-compatible FFI for ApertureC metrics.
//!
//! The C API is only available when building with the `ffi-lib` feature enabled.
//!
//! # Usage
//!
//! ```c
//! // Create an exporter
//! AcMetricsExporter *exporter =
//!     ac_metrics_exporter_new(Log, NULL);
//! if (!exporter) {
//!     AcMetricsError *error = ac_metrics_last_error();
//!     fprintf(stderr, "Metrics error: %s\n", ac_metrics_error_as_str(error));
//!     ac_metrics_error_free(error);
//!     return 1;
//! }
//!
//! // Initialize metrics (ac_metrics_init takes ownership of exporters)
//! AcMetricsExporter *exporters[] = { exporter };
//! if (ac_metrics_init(exporters, 1) != AcMetricsStatus_Ok) {
//!     AcMetricsError *error = ac_metrics_last_error();
//!     fprintf(stderr, "Metrics error: %s\n", ac_metrics_error_as_str(error));
//!     ac_metrics_error_free(error);
//!     return 1;
//! }
//! ```

use crate::{
    MetricsInitializer,
    exporters::{CsvExporter, Exporter, LogExporter, PrometheusExporter, PushgatewayExporter},
    metrics_initialized,
};
use atomicbox::AtomicOptionBox;
use std::{
    ffi::{CStr, CString},
    os::raw::c_char,
    process, ptr, slice,
    sync::atomic::Ordering,
};
use tracing::Level;

/// Status for C API calls.
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum AcMetricsStatus {
    Ok = 0,
    Err = 1,
}

/// Opaque error for C callers.
pub struct AcMetricsError {
    message: CString,
}

static LAST_ERROR: AtomicOptionBox<AcMetricsError> = AtomicOptionBox::none();

fn set_last_error(message: impl Into<String>) {
    let message = message.into();
    let c_message =
        CString::new(message).unwrap_or_else(|_| CString::new("invalid error").unwrap());
    LAST_ERROR.store(
        Some(Box::new(AcMetricsError { message: c_message })),
        Ordering::AcqRel,
    );
}

fn take_last_error() -> Option<Box<AcMetricsError>> {
    LAST_ERROR.take(Ordering::AcqRel)
}

/// Returns the last error for the current process.
///
/// # Safety
///
/// Caller owns the returned pointer and must free it with [`ac_metrics_error_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_metrics_last_error() -> *mut AcMetricsError {
    take_last_error()
        .map(Box::into_raw)
        .unwrap_or(ptr::null_mut())
}

/// Returns the error string.
///
/// # Safety
///
/// `error` must be a valid pointer returned by [`ac_metrics_last_error`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_metrics_error_as_str(error: *const AcMetricsError) -> *const c_char {
    // SAFETY: caller guarantees error points to a valid AcMetricsError or is null.
    let Some(error) = (unsafe { error.as_ref() }) else {
        return ptr::null();
    };
    error.message.as_ptr()
}

/// Frees an error returned by [`ac_metrics_last_error`].
///
/// # Safety
///
/// Must not be called twice on the same pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_metrics_error_free(error: *mut AcMetricsError) {
    if !error.is_null() {
        // SAFETY: caller guarantees this pointer was returned by ac_metrics_last_error.
        drop(unsafe { Box::from_raw(error) });
    }
}

/// Metrics exporter.
pub struct AcMetricsExporter(Exporter);

/// Metrics exporter type.
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum AcMetricsExporterType {
    Log,
    Csv,
    Prometheus,
    Pushgateway,
}

fn config_string(config: *const c_char) -> Result<String, AcMetricsStatus> {
    if config.is_null() {
        set_last_error("config must not be null for this exporter type");
        return Err(AcMetricsStatus::Err);
    }
    // SAFETY: config is non-null and required by the FFI contract to be a valid
    // null-terminated C string.
    match unsafe { CStr::from_ptr(config) }.to_str() {
        Ok(s) => Ok(s.to_string()),
        Err(_) => {
            set_last_error("invalid UTF-8 in config string");
            Err(AcMetricsStatus::Err)
        }
    }
}

/// Creates a new metrics exporter.
///
/// # Safety
///
/// If `config` is non-null, it must point to a valid null-terminated C string. Providing a
/// non-null-terminated string may result in out-of-bounds reads.
/// Caller must free returned exporter with [`ac_metrics_exporter_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_metrics_exporter_new(
    exporter_type: AcMetricsExporterType,
    config: *const c_char,
) -> *mut AcMetricsExporter {
    let exporter_result = match exporter_type {
        AcMetricsExporterType::Log => LogExporter::new(Level::DEBUG).map(Exporter::Log),
        AcMetricsExporterType::Csv => {
            let Ok(path) = config_string(config) else {
                return ptr::null_mut();
            };
            CsvExporter::new(path).map(Exporter::Csv)
        }
        AcMetricsExporterType::Prometheus => {
            let Ok(addr) = config_string(config) else {
                return ptr::null_mut();
            };
            PrometheusExporter::new(&addr).map(Exporter::Prometheus)
        }
        AcMetricsExporterType::Pushgateway => {
            let Ok(url) = config_string(config) else {
                return ptr::null_mut();
            };
            PushgatewayExporter::new(url, env!("CARGO_CRATE_NAME").to_owned(), process::id())
                .map(Exporter::Pushgateway)
        }
    };

    match exporter_result {
        Ok(exporter) => Box::into_raw(Box::new(AcMetricsExporter(exporter))),
        Err(e) => {
            set_last_error(format!("failed to create exporter: {e}"));
            ptr::null_mut()
        }
    }
}

/// Frees a metrics exporter.
///
/// # Safety
///
/// Must not be called twice on same pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_metrics_exporter_free(exporter: *mut AcMetricsExporter) {
    if !exporter.is_null() {
        // SAFETY: caller guarantees this pointer was returned by ac_metrics_exporter_new.
        drop(unsafe { Box::from_raw(exporter) });
    }
}

/// Initializes metrics for the process.
///
/// Takes ownership of the exporters (they will be freed as part of initialization), even when
/// initialization fails.
///
/// # Safety
///
/// `exporters` must be non-null and point to a valid array of at least `count` exporter pointers.
/// All exporter pointers in the array must be non-null and point to valid [`AcMetricsExporter`]
/// objects. `count` must match the actual number of elements in the array; otherwise this function
/// may read out of bounds.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_metrics_init(
    exporters: *mut *mut AcMetricsExporter,
    count: usize,
) -> AcMetricsStatus {
    if exporters.is_null() {
        set_last_error("exporters pointer is null");
        return AcMetricsStatus::Err;
    }
    if !exporters.is_aligned() {
        set_last_error("exporters pointer is unaligned");
        return AcMetricsStatus::Err;
    }
    if count == 0 {
        set_last_error("no exporters provided");
        return AcMetricsStatus::Err;
    }

    let exporters = unsafe { slice::from_raw_parts_mut(exporters, count) };

    for &ptr in exporters.iter() {
        if ptr.is_null() {
            set_last_error("exporter array contains null pointer");
            return AcMetricsStatus::Err;
        }
        if !ptr.is_aligned() {
            set_last_error("exporter pointer is unaligned");
            return AcMetricsStatus::Err;
        }
    }

    let rust_exporters = exporters
        .iter()
        .map(|&ptr| {
            // SAFETY: ptr is non-null, aligned, and points to a valid AcMetricsExporter.
            let exp = unsafe { Box::from_raw(ptr) };
            exp.0
        })
        .collect::<Vec<_>>();

    if metrics_initialized() {
        set_last_error("metrics already initialized");
        return AcMetricsStatus::Err;
    }

    match MetricsInitializer::default()
        .with_poll_rate_from_secs(3)
        .with_exporters(rust_exporters)
        .init()
    {
        Ok(_) => AcMetricsStatus::Ok,
        Err(e) => {
            set_last_error(format!("metrics initialization failed: {e}"));
            AcMetricsStatus::Err
        }
    }
}

/// Returns whether metrics are initialized.
#[unsafe(no_mangle)]
pub extern "C" fn ac_metrics_initialized() -> bool {
    metrics_initialized()
}
