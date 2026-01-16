#![cfg(feature = "ffi-lib")]

use aperturec_metrics::ffi::{
    AcMetricsExporterType, AcMetricsStatus, ac_metrics_error_as_str, ac_metrics_error_free,
    ac_metrics_exporter_free, ac_metrics_exporter_new, ac_metrics_init, ac_metrics_initialized,
    ac_metrics_last_error,
};
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::ptr;

fn take_last_error_string() -> Option<String> {
    let error = unsafe { ac_metrics_last_error() };
    if error.is_null() {
        return None;
    }
    let message_ptr = unsafe { ac_metrics_error_as_str(error) };
    let message = if message_ptr.is_null() {
        None
    } else {
        Some(
            unsafe { CStr::from_ptr(message_ptr) }
                .to_string_lossy()
                .into_owned(),
        )
    };
    unsafe { ac_metrics_error_free(error) };
    message
}

#[test]
fn ffi_exporter_log_is_created() {
    unsafe {
        let exporter = ac_metrics_exporter_new(AcMetricsExporterType::Log, ptr::null());
        assert!(!exporter.is_null());
        ac_metrics_exporter_free(exporter);
    }
}

#[test]
fn ffi_exporter_rejects_null_config() {
    unsafe {
        let exporter = ac_metrics_exporter_new(AcMetricsExporterType::Csv, ptr::null());
        assert!(exporter.is_null());
        let message = take_last_error_string().expect("expected error message");
        assert_eq!(message, "config must not be null for this exporter type");
    }
}

#[test]
fn ffi_exporter_rejects_invalid_utf8_config() {
    static BAD_CONFIG: [u8; 2] = [0xff, 0x00];

    unsafe {
        let config_ptr = BAD_CONFIG.as_ptr() as *const c_char;
        let exporter = ac_metrics_exporter_new(AcMetricsExporterType::Prometheus, config_ptr);
        assert!(exporter.is_null());
        let message = take_last_error_string().expect("expected error message");
        assert_eq!(message, "invalid UTF-8 in config string");
    }
}

#[test]
fn ffi_metrics_init_reports_state() {
    unsafe {
        let was_initialized = ac_metrics_initialized();
        let exporter = ac_metrics_exporter_new(AcMetricsExporterType::Log, ptr::null());
        assert!(!exporter.is_null());

        let mut exporters = [exporter];
        let status = ac_metrics_init(exporters.as_mut_ptr(), exporters.len());

        if was_initialized {
            assert_eq!(status, AcMetricsStatus::Err);
            let message = take_last_error_string().expect("expected error message");
            assert_eq!(message, "metrics already initialized");
            ac_metrics_exporter_free(exporter);
            return;
        }

        assert_eq!(status, AcMetricsStatus::Ok);
        assert!(ac_metrics_initialized());
    }
}

#[test]
fn ffi_exporter_accepts_valid_config() {
    let path = std::env::temp_dir().join("aperturec_metrics_test.csv");
    let config = CString::new(path.to_string_lossy().as_bytes()).expect("config string");

    unsafe {
        let exporter = ac_metrics_exporter_new(AcMetricsExporterType::Csv, config.as_ptr());
        assert!(!exporter.is_null());
        ac_metrics_exporter_free(exporter);
    }
}
