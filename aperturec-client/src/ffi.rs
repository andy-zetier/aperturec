//! C-compatible FFI for ApertureC client library.
//!
//! # Usage
//!
//! ## Basic usage with explicit configuration
//!
//! ```c
//! // Create configuration
//! AcConfiguration *config;
//! if (ac_configuration_from_argv(&config) != AcStatus_Ok) {
//!     AcError *error = ac_last_error();
//!     fprintf(stderr, "Config error: %s\n", ac_error_as_str(error));
//!     ac_error_free(error);
//!     return 1;
//! }
//!
//! // Create lock state and monitor geometry
//! AcLockState *lock_state = ac_lock_state_new(false, false, false);
//! AcMonitorsGeometry *geometry = ac_monitors_geometry_new();
//! if (ac_monitors_geometry_add_monitor_all_usable(geometry, 0, 0, 1920, 1080) != AcStatus_Ok) {
//!     AcError *error = ac_last_error();
//!     fprintf(stderr, "Geometry error: %s\n", ac_error_as_str(error));
//!     ac_error_free(error);
//!     ac_configuration_free(config);
//!     ac_lock_state_free(lock_state);
//!     ac_monitors_geometry_free(geometry);
//!     return 1;
//! }
//!
//! // Create client
//! AcClient *client = ac_client_new(config, lock_state, geometry);
//! if (!client) {
//!     AcError *error = ac_last_error();
//!     fprintf(stderr, "Client error: %s\n", ac_error_as_str(error));
//!     ac_error_free(error);
//!     ac_configuration_free(config);
//!     ac_lock_state_free(lock_state);
//!     ac_monitors_geometry_free(geometry);
//!     return 1;
//! }
//!
//! // Free parameters after successful client creation
//! ac_configuration_free(config);
//! ac_lock_state_free(lock_state);
//! ac_monitors_geometry_free(geometry);
//!
//! // Connect to server
//! AcConnection *connection = ac_client_connect(client);
//! if (!connection) {
//!     AcError *error = ac_last_error();
//!     fprintf(stderr, "Connect error: %s\n", ac_error_as_str(error));
//!     ac_error_free(error);
//!     ac_client_free(client);
//!     return 1;
//! }
//!
//! // Poll for events
//! AcEvent *event;
//! while (ac_connection_poll_event(connection, &event) == AcStatus_Ok) {
//!     // Process event
//!     ac_event_free(event);
//! }
//!
//! // Cleanup - connection_free disconnects and frees the connection
//! ac_connection_free(connection);
//! ac_client_free(client);
//! ```
//!
//! ## Convenience usage with command-line arguments
//!
//! ```c
//! // Create lock state and monitor geometry
//! AcLockState *lock_state = ac_lock_state_new(false, false, false);
//! AcMonitorsGeometry *geometry = ac_monitors_geometry_new();
//! ac_monitors_geometry_add_monitor_all_usable(geometry, 0, 0, 1920, 1080);
//!
//! // Create client directly from command-line arguments
//! AcClient *client = ac_client_from_args(lock_state, geometry);
//! if (!client) {
//!     AcError *error = ac_last_error();
//!     fprintf(stderr, "Client error: %s\n", ac_error_as_str(error));
//!     ac_error_free(error);
//!     ac_lock_state_free(lock_state);
//!     ac_monitors_geometry_free(geometry);
//!     return 1;
//! }
//!
//! // Free parameters after successful client creation
//! ac_lock_state_free(lock_state);
//! ac_monitors_geometry_free(geometry);
//!
//! // Connect and use as above...
//! AcConnection *connection = ac_client_connect(client);
//! // ...
//! ```
//!
//! ## Convenience usage with URI
//!
//! ```c
//! // Create lock state and monitor geometry
//! AcLockState *lock_state = ac_lock_state_new(false, false, false);
//! AcMonitorsGeometry *geometry = ac_monitors_geometry_new();
//! ac_monitors_geometry_add_monitor_all_usable(geometry, 0, 0, 1920, 1080);
//!
//! // Create client directly from URI
//! AcClient *client = ac_client_from_uri("aperturec://server:46454", lock_state, geometry);
//! if (!client) {
//!     AcError *error = ac_last_error();
//!     fprintf(stderr, "Client error: %s\n", ac_error_as_str(error));
//!     ac_error_free(error);
//!     ac_lock_state_free(lock_state);
//!     ac_monitors_geometry_free(geometry);
//!     return 1;
//! }
//!
//! // Free parameters after successful client creation
//! ac_lock_state_free(lock_state);
//! ac_monitors_geometry_free(geometry);
//!
//! // Connect and use as above...
//! AcConnection *connection = ac_client_connect(client);
//! // ...
//! ```
//!
//! # Thread Safety
//!
//! `AcClient` and `AcConnection` objects are not thread-safe. Error state is globally
//! shared - retrieve errors immediately after failures in multi-threaded applications.

// The following types are stubs to eventually be implemented as libclient takes shape. They are
// here now so that the ffi module can compile without the rest of libclient
struct Client;
struct Connection;
#[derive(Debug, thiserror::Error)]
enum ConnectionError {}
struct Event;
#[derive(Debug, thiserror::Error)]
enum EventError {}
#[derive(Debug, thiserror::Error)]
enum InputError {}
pub struct Key;
pub struct MouseButton;
struct Configuration;
#[derive(Debug, thiserror::Error)]
enum ConfigurationError {}
struct LockState;
struct MonitorsGeometry;

use atomicbox::AtomicOptionBox;
use libc::*;
use std::{error, ffi::CString, ptr, sync::atomic::Ordering};

static LAST_ERROR: AtomicOptionBox<AcError> = AtomicOptionBox::none();

fn set_last_error<E: Into<AcError>>(error: E) {
    LAST_ERROR.store(Some(Box::new(error.into())), Ordering::Relaxed);
}

impl<E: Into<AcErrorKind>> From<E> for AcError {
    fn from(k: E) -> Self {
        AcError {
            kind: k.into(),
            disp: None,
        }
    }
}

macro_rules! check_aligned {
    ($p:ident, $retval:expr) => {{
        if !$p.is_aligned() {
            set_last_error(AcErrorKind::Argument(concat!(
                stringify!($p),
                " is unaligned"
            )));
            return $retval;
        }
    }};
}

macro_rules! as_ref_checked {
    ($p:ident) => {{ as_ref_checked!($p, AcStatus::Err) }};
    ($p:ident, $retval:expr) => {{
        check_aligned!($p, $retval);
        // SAFETY: We check that the pointer is aligned above and non-null in the as_ref call, but
        // otherwise assume the caller is giving us a valid pointer
        let Some(r) = (unsafe { $p.as_ref() }) else {
            set_last_error(AcErrorKind::Argument(concat!(stringify!($p), " is null")));
            return $retval;
        };
        r
    }};
}

macro_rules! as_mut_checked {
    ($p:ident) => {{ as_mut_checked!($p, AcStatus::Err) }};
    ($p:ident, $retval:expr) => {{
        check_aligned!($p, $retval);
        // SAFETY: We check that the pointer is aligned above and non-null in the as_mut call, but
        // otherwise assume the caller is giving us a valid pointer
        let Some(m) = (unsafe { $p.as_mut() }) else {
            set_last_error(AcErrorKind::Argument(concat!(stringify!($p), " is null")));
            return $retval;
        };
        m
    }};
}

macro_rules! into_owned_unchecked {
    // SAFETY: since this is `unchecked`, we assume the instantiator checks the pointer is
    // non-null, aligned, and a pointer derived from a Box
    ($p:ident) => {{ unsafe { Box::from_raw($p) } }};
}

macro_rules! drop_owned {
    ($p:ident) => {{
        check_aligned!($p, ());
        if !$p.is_null() {
            drop(into_owned_unchecked!($p));
        }
    }};
}

#[allow(unused_macros)]
macro_rules! attempt {
    ($e:expr) => {{ attempt!($e, AcStatus::Err) }};
    ($e:expr, $retval:expr) => {{
        match $e {
            Ok(v) => v,
            Err(error) => {
                set_last_error(error);
                return $retval;
            }
        }
    }};
}

/// Retrieves and clears last error.
///
/// # Returns
///
/// * Pointer to error if one occurred
/// * Null if no error
///
/// # Safety
///
/// Returned error must be freed with [`ac_error_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_last_error() -> *mut AcError {
    LAST_ERROR
        .take(Ordering::Relaxed)
        .map(Box::into_raw)
        .unwrap_or(ptr::null_mut())
}

/// Function return status. On `Err`, call [`ac_last_error`] for details.
#[repr(C)]
pub enum AcStatus {
    Ok,
    Err,
}

/// Error object. Get message with [`ac_error_as_str`], free with [`ac_error_free`].
pub struct AcError {
    kind: AcErrorKind,
    disp: Option<CString>,
}

/// Frees error.
///
/// # Parameters
///
/// * `error` - Error to free, or null (no-op)
///
/// # Safety
///
/// Must not be called twice on same pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_error_free(error: *mut AcError) {
    drop_owned!(error);
}

/// Returns error message string.
///
/// # Returns
///
/// * Pointer to null-terminated C string
/// * Null if error pointer is invalid
///
/// # Safety
///
/// Returned string valid until error is freed. Do not modify or free the string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_error_as_str(error: *mut AcError) -> *const c_char {
    let error = as_mut_checked!(error, ptr::null());
    error
        .disp
        .get_or_insert_with(|| {
            CString::new(format!("{}", error.kind)).expect("create error string")
        })
        .as_ptr()
}

#[derive(Debug, derive_more::From, derive_more::Display)]
enum AcErrorKind {
    #[from(skip)]
    Argument(&'static str),
    Configuration(ConfigurationError),
    Connection(ConnectionError),
    Event(EventError),
    Input(InputError),
}

impl error::Error for AcErrorKind {}

/// Mouse button identifier.
#[repr(transparent)]
pub struct AcMouseButton(u32);

impl TryFrom<AcMouseButton> for MouseButton {
    type Error = AcError;

    fn try_from(_mb: AcMouseButton) -> Result<MouseButton, AcError> {
        todo!()
    }
}

/// Keyboard key identifier.
#[repr(transparent)]
pub struct AcKey(u32);

impl TryFrom<AcKey> for Key {
    type Error = AcError;

    fn try_from(_key: AcKey) -> Result<Key, AcError> {
        todo!()
    }
}

/// Client configuration
pub struct AcConfiguration(Configuration);

/// Creates configuration from command-line arguments.
///
/// # Parameters
///
/// * `config_out` - Receives pointer to newly allocated configuration
///
/// # Returns
///
/// * `AcStatus::Ok` on success
/// * `AcStatus::Err` on failure - call [`ac_last_error`] for details
///
/// # Safety
///
/// Caller must free returned configuration with [`ac_configuration_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_configuration_from_argv(
    config_out: *mut *mut AcConfiguration,
) -> AcStatus {
    let _config_out = as_mut_checked!(config_out);
    todo!()
}

/// Creates configuration from URI.
///
/// # Parameters
///
/// * `uri` - Null-terminated UTF-8 string
/// * `config_out` - Receives pointer to newly allocated configuration
///
/// # Returns
///
/// * `AcStatus::Ok` on success
/// * `AcStatus::Err` on failure - call [`ac_last_error`] for details
///
/// # Safety
///
/// `uri` must be valid for call duration.
/// Caller must free returned configuration with [`ac_configuration_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_configuration_from_uri(
    uri: *const c_char,
    config_out: *mut *mut AcConfiguration,
) -> AcStatus {
    let _config_out = as_mut_checked!(config_out);
    let _uri = as_ref_checked!(uri);
    todo!()
}

/// Frees configuration.
///
/// # Parameters
///
/// * `config` - Configuration to free, or null (no-op)
///
/// # Safety
///
/// Must not be called twice on same pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_configuration_free(config: *mut AcConfiguration) {
    drop_owned!(config);
}

/// Keyboard lock state (Caps Lock, Num Lock, Scroll Lock)
pub struct AcLockState(LockState);

/// Creates lock state with specified lock settings.
///
/// # Returns
///
/// * Pointer to newly allocated lock state
/// * Never returns null
///
/// # Safety
///
/// Caller must free returned lock state with [`ac_lock_state_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_lock_state_new(
    _is_caps_locked: bool,
    _is_num_locked: bool,
    _is_scroll_locked: bool,
) -> *mut AcLockState {
    todo!()
}

/// Frees lock state.
///
/// # Parameters
///
/// * `lock_state` - Lock state to free, or null (no-op)
///
/// # Safety
///
/// Must not be called twice on same pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_lock_state_free(lock_state: *mut AcLockState) {
    drop_owned!(lock_state);
}

/// Monitor configuration specifying usable and total areas
pub struct AcMonitorsGeometry(MonitorsGeometry);

/// Creates empty monitor geometry. Add monitors with [`ac_monitors_geometry_add_monitor`].
///
/// # Returns
///
/// * Pointer to newly allocated empty monitor geometry
/// * Never returns null
///
/// # Safety
///
/// Caller must free returned geometry with [`ac_monitors_geometry_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_monitors_geometry_new() -> *mut AcMonitorsGeometry {
    todo!()
}

/// Adds monitor where entire area is usable (no window decorations, taskbars, etc.).
///
/// # Parameters
///
/// * `monitors_geometry` - Geometry to add monitor to
/// * `origin_x` - X coordinate of monitor origin
/// * `origin_y` - Y coordinate of monitor origin
/// * `width` - Monitor width in pixels
/// * `height` - Monitor height in pixels
///
/// # Returns
///
/// * `AcStatus::Ok` on success
/// * `AcStatus::Err` if monitor overlaps existing monitors
///
/// # Safety
///
/// `monitors_geometry` must be non-null, properly aligned, and point to a valid `AcMonitorsGeometry`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_monitors_geometry_add_monitor_all_usable(
    monitors_geometry: *mut AcMonitorsGeometry,
    origin_x: usize,
    origin_y: usize,
    width: usize,
    height: usize,
) -> AcStatus {
    unsafe {
        ac_monitors_geometry_add_monitor(
            monitors_geometry,
            origin_x,
            origin_y,
            width,
            height,
            origin_x,
            origin_y,
            width,
            height,
        )
    }
}

/// Adds monitor with separate usable and total areas.
///
/// Usable area excludes window decorations, taskbars, etc. Must be contained within total area.
///
/// # Parameters
///
/// * `monitors_geometry` - Geometry to add monitor to
/// * `usable_origin_x` - X coordinate of usable area origin
/// * `usable_origin_y` - Y coordinate of usable area origin
/// * `usable_width` - Usable area width in pixels
/// * `usable_height` - Usable area height in pixels
/// * `total_origin_x` - X coordinate of total area origin
/// * `total_origin_y` - Y coordinate of total area origin
/// * `total_width` - Total area width in pixels
/// * `total_height` - Total area height in pixels
///
/// # Returns
///
/// * `AcStatus::Ok` on success
/// * `AcStatus::Err` if usable exceeds total or monitors overlap
///
/// # Safety
///
/// `monitors_geometry` must be non-null, properly aligned, and point to a valid `AcMonitorsGeometry`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_monitors_geometry_add_monitor(
    monitors_geometry: *mut AcMonitorsGeometry,
    _usable_origin_x: usize,
    _usable_origin_y: usize,
    _usable_width: usize,
    _usable_height: usize,
    _total_origin_x: usize,
    _total_origin_y: usize,
    _total_width: usize,
    _total_height: usize,
) -> AcStatus {
    let _monitors_geometry = as_mut_checked!(monitors_geometry);
    todo!()
}

/// Frees monitor geometry.
///
/// # Parameters
///
/// * `monitors_geometry` - Monitor geometry to free, or null (no-op)
///
/// # Safety
///
/// Must not be called twice on same pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_monitors_geometry_free(monitors_geometry: *mut AcMonitorsGeometry) {
    drop_owned!(monitors_geometry);
}

/// ApertureC client. Not thread-safe.
pub struct AcClient(Client);

/// Creates client
///
/// # Parameters
///
/// * `config` - Configuration for the client
/// * `lock_state` - Initial keyboard lock state
/// * `monitors_geometry` - Monitor configuration
///
/// # Returns
///
/// * Pointer to client on success
/// * Null on error - call [`ac_last_error`] for details
///
/// # Safety
///
/// All parameters must be non-null, properly aligned, and point to valid objects.
/// Caller is responsible for freeing all parameters with their respective free functions
/// after this call completes (whether successful or not).
/// Free returned client with [`ac_client_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_client_new(
    config: *mut AcConfiguration,
    lock_state: *mut AcLockState,
    monitors_geometry: *mut AcMonitorsGeometry,
) -> *mut AcClient {
    let _config = as_mut_checked!(config, ptr::null_mut());
    let _lock_state = as_ref_checked!(lock_state, ptr::null_mut());
    let _monitors_geometry = as_ref_checked!(monitors_geometry, ptr::null_mut());

    todo!()
}

/// Creates client from command-line arguments.
///
/// Convenience wrapper that creates a configuration from command-line arguments
/// and passes it to [`ac_client_new`].
///
/// # Parameters
///
/// * `lock_state` - Initial keyboard lock state
/// * `monitors_geometry` - Monitor configuration
///
/// # Returns
///
/// * Pointer to client on success
/// * Null on error - call [`ac_last_error`] for details
///
/// # Safety
///
/// Both parameters must be non-null, properly aligned, and point to valid objects.
/// Caller is responsible for freeing both parameters with their respective free functions
/// after this call completes (whether successful or not).
/// Free returned client with [`ac_client_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_client_from_args(
    lock_state: *mut AcLockState,
    monitors_geometry: *mut AcMonitorsGeometry,
) -> *mut AcClient {
    let _lock_state = as_ref_checked!(lock_state, ptr::null_mut());
    let _monitors_geometry = as_ref_checked!(monitors_geometry, ptr::null_mut());
    todo!()
}

/// Creates client from URI.
///
/// Convenience wrapper that creates a configuration from a URI string
/// and passes it to [`ac_client_new`].
///
/// # Parameters
///
/// * `uri` - Null-terminated UTF-8 string containing connection URI
/// * `lock_state` - Initial keyboard lock state
/// * `monitors_geometry` - Monitor configuration
///
/// # Returns
///
/// * Pointer to client on success
/// * Null on error - call [`ac_last_error`] for details
///
/// # Safety
///
/// `uri` must be valid for call duration.
/// Both `lock_state` and `monitors_geometry` must be non-null, properly aligned, and point to valid objects.
/// Caller is responsible for freeing both parameters with their respective free functions
/// after this call completes (whether successful or not).
/// Free returned client with [`ac_client_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_client_from_uri(
    uri: *const c_char,
    lock_state: *mut AcLockState,
    monitors_geometry: *mut AcMonitorsGeometry,
) -> *mut AcClient {
    let _uri = as_ref_checked!(uri, ptr::null_mut());
    let _lock_state = as_ref_checked!(lock_state, ptr::null_mut());
    let _monitors_geometry = as_ref_checked!(monitors_geometry, ptr::null_mut());
    todo!()
}

/// Active connection to ApertureC server. Not thread-safe.
///
/// Obtained via [`ac_client_connect`]. Free with [`ac_connection_disconnect`] or
/// [`ac_connection_free`] (which safely disconnects if necessary before freeing).
pub struct AcConnection(Connection);

/// Connects client to remote ApertureC server.
///
/// Initiates connection and returns an active connection handle. The connection can be
/// used to send input events and receive display updates.
///
/// # Parameters
///
/// * `client` - Client to connect
///
/// # Returns
///
/// * Pointer to connection on success
/// * Null on error - call [`ac_last_error`] for details
///
/// # Safety
///
/// `client` must be non-null, properly aligned, and point to a valid `AcClient`.
/// Caller must eventually disconnect and free the returned connection.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_client_connect(client: *mut AcClient) -> *mut AcConnection {
    let _client = as_mut_checked!(client, ptr::null_mut());
    todo!()
}

/// Frees client.
///
/// # Parameters
///
/// * `client` - Client to free, or null (no-op)
///
/// # Safety
///
/// `client` must be a valid pointer previously returned from [`ac_client_new`],
/// [`ac_client_from_args`], or [`ac_client_from_uri`].
/// Must not be called twice on same pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_client_free(client: *mut AcClient) {
    drop_owned!(client);
}

/// Disconnects from server and frees the connection.
///
/// After calling this function, the connection pointer is invalid and must not be used.
/// This function disconnects and frees the connection object. If you only want to disconnect
/// but keep the connection object allocated, use [`ac_connection_disconnect`]. For the common
/// case of both disconnecting and freeing, use [`ac_connection_free`].
///
/// # Parameters
///
/// * `connection` - Connection to disconnect and free
///
/// # Returns
///
/// * `AcStatus::Ok` on success
/// * `AcStatus::Err` on failure - call [`ac_last_error`] for details
///
/// # Safety
///
/// `connection` must be non-null, properly aligned, and point to a valid `AcConnection`
/// previously returned from [`ac_client_connect`].
/// Must not be called twice on same pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_connection_disconnect(connection: *mut AcConnection) -> AcStatus {
    let _connection = as_mut_checked!(connection);
    todo!()
}

/// Frees connection, disconnecting first if necessary.
///
/// If the connection is still active, it will be disconnected before being freed.
/// This function is idempotent-safe for already-disconnected connections and safe
/// to call instead of [`ac_connection_disconnect`].
///
/// # Parameters
///
/// * `connection` - Connection to free, or null (no-op)
///
/// # Safety
///
/// `connection` must be a valid pointer previously returned from [`ac_client_connect`],
/// or null. Must not be called twice on same pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_connection_free(connection: *mut AcConnection) {
    #[allow(clippy::unused_unit)]
    let _connection = as_mut_checked!(connection, ());
    todo!()
}

/// Client event (inspection functions not yet implemented).
// TODO: remove allow once event inspection functions are implemented
#[allow(dead_code)]
pub struct AcEvent(Event);

/// Frees event.
///
/// # Parameters
///
/// * `event` - Event to free, or null (no-op)
///
/// # Safety
///
/// Must not be called twice on same pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_event_free(event: *mut AcEvent) {
    drop_owned!(event);
}

/// FFI wrapper for [`Connection::poll_event`].
///
/// See [`Connection::poll_event`] for details. If an event is available, it is written to
/// `event_out`. If no event is available, the function returns successfully but leaves
/// `event_out` unchanged.
///
/// # Parameters
///
/// * `connection` - Pointer to the connection. Must be non-null and properly aligned.
/// * `event_out` - Output parameter that receives a pointer to the event if one is available.
///   Must be non-null and properly aligned. Events must be freed with [`ac_event_free`].
///
/// # Returns
///
/// * `AcStatus::Ok` - Poll succeeded (event may or may not be available)
/// * `AcStatus::Err` - An error occurred. Call [`ac_last_error`] to retrieve error details.
///
/// # Safety
///
/// The caller must ensure:
/// * `connection` is non-null, properly aligned, and points to a valid `AcConnection`
/// * `event_out` is non-null, properly aligned, and points to valid memory
/// * Any event pointer written to `event_out` is eventually freed with [`ac_event_free`]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_connection_poll_event(
    connection: *mut AcConnection,
    event_out: *mut *mut AcEvent,
) -> AcStatus {
    let _connection = as_mut_checked!(connection);
    let _event_out = as_mut_checked!(event_out);
    todo!()
}

/// FFI wrapper for [`Connection::wait_event`].
///
/// See [`Connection::wait_event`] for details. Use [`ac_connection_poll_event`] for non-blocking
/// operation or [`ac_connection_wait_event_timeout`] to wait with a timeout.
///
/// # Parameters
///
/// * `connection` - Pointer to the connection. Must be non-null and properly aligned.
/// * `event_out` - Output parameter that receives a pointer to the event.
///   Must be non-null and properly aligned. Events must be freed with [`ac_event_free`].
///
/// # Returns
///
/// * `AcStatus::Ok` - An event was successfully retrieved and written to `event_out`
/// * `AcStatus::Err` - An error occurred. Call [`ac_last_error`] to retrieve error details.
///
/// # Safety
///
/// The caller must ensure:
/// * `connection` is non-null, properly aligned, and points to a valid `AcConnection`
/// * `event_out` is non-null, properly aligned, and points to valid memory
/// * The event pointer written to `event_out` is eventually freed with [`ac_event_free`]
/// * The calling thread can safely block until an event arrives
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_connection_wait_event(
    connection: *mut AcConnection,
    event_out: *mut *mut AcEvent,
) -> AcStatus {
    let _connection = as_mut_checked!(connection);
    let _event_out = as_mut_checked!(event_out);
    todo!()
}

/// FFI wrapper for [`Connection::wait_event_timeout`].
///
/// See [`Connection::wait_event_timeout`] for details. If an event arrives before the timeout,
/// it is written to `event_out`. If the timeout expires without an event, the function
/// returns successfully but leaves `event_out` unchanged.
///
/// # Parameters
///
/// * `connection` - Pointer to the connection. Must be non-null and properly aligned.
/// * `timeout_ms` - Maximum time to wait in milliseconds.
/// * `event_out` - Output parameter that receives a pointer to the event if one arrives.
///   Must be non-null and properly aligned. Events must be freed with [`ac_event_free`].
///
/// # Returns
///
/// * `AcStatus::Ok` - Operation succeeded (event may or may not be available)
/// * `AcStatus::Err` - An error occurred. Call [`ac_last_error`] to retrieve error details.
///
/// # Safety
///
/// The caller must ensure:
/// * `connection` is non-null, properly aligned, and points to a valid `AcConnection`
/// * `event_out` is non-null, properly aligned, and points to valid memory
/// * Any event pointer written to `event_out` is eventually freed with [`ac_event_free`]
/// * The calling thread can safely block for up to `timeout_ms` milliseconds
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_connection_wait_event_timeout(
    connection: *mut AcConnection,
    _timeout_ms: u64,
    event_out: *mut *mut AcEvent,
) -> AcStatus {
    let _connection = as_mut_checked!(connection);
    let _event_out = as_mut_checked!(event_out);
    todo!()
}

/// FFI wrapper for [`Connection::pointer_move`].
///
/// See [`Connection::pointer_move`] for details.
///
/// # Parameters
///
/// * `connection` - Pointer to the connection. Must be non-null and properly aligned.
/// * `x` - X coordinate of the mouse pointer position.
/// * `y` - Y coordinate of the mouse pointer position.
///
/// # Returns
///
/// * `AcStatus::Ok` - Event was successfully sent
/// * `AcStatus::Err` - Failed to send event. Call [`ac_last_error`] to retrieve error details.
///
/// # Safety
///
/// The caller must ensure:
/// * `connection` is non-null, properly aligned, and points to a valid `AcConnection`
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_connection_pointer_move(
    connection: *mut AcConnection,
    _x: u64,
    _y: u64,
) -> AcStatus {
    let _connection = as_mut_checked!(connection);
    todo!()
}

/// FFI wrapper for [`Connection::mouse_button_press`].
///
/// See [`Connection::mouse_button_press`] for details.
///
/// # Parameters
///
/// * `connection` - Pointer to the connection. Must be non-null and properly aligned.
/// * `button` - The mouse button that was pressed.
/// * `x` - X coordinate of the mouse pointer position.
/// * `y` - Y coordinate of the mouse pointer position.
///
/// # Returns
///
/// * `AcStatus::Ok` - Event was successfully sent
/// * `AcStatus::Err` - Failed to send event. Call [`ac_last_error`] to retrieve error details.
///
/// # Safety
///
/// The caller must ensure:
/// * `connection` is non-null, properly aligned, and points to a valid `AcConnection`
/// * `button` is a valid `AcMouseButton` value
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_connection_mouse_button_press(
    connection: *mut AcConnection,
    _button: AcMouseButton,
    _x: u64,
    _y: u64,
) -> AcStatus {
    let _connection = as_mut_checked!(connection);
    todo!()
}

/// FFI wrapper for [`Connection::mouse_button_release`].
///
/// See [`Connection::mouse_button_release`] for details.
///
/// # Parameters
///
/// * `connection` - Pointer to the connection. Must be non-null and properly aligned.
/// * `button` - The mouse button that was released.
/// * `x` - X coordinate of the mouse pointer position.
/// * `y` - Y coordinate of the mouse pointer position.
///
/// # Returns
///
/// * `AcStatus::Ok` - Event was successfully sent
/// * `AcStatus::Err` - Failed to send event. Call [`ac_last_error`] to retrieve error details.
///
/// # Safety
///
/// The caller must ensure:
/// * `connection` is non-null, properly aligned, and points to a valid `AcConnection`
/// * `button` is a valid `AcMouseButton` value
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_connection_mouse_button_release(
    connection: *mut AcConnection,
    _button: AcMouseButton,
    _x: u64,
    _y: u64,
) -> AcStatus {
    let _connection = as_mut_checked!(connection);
    todo!()
}

/// FFI wrapper for [`Connection::scroll`].
///
/// See [`Connection::scroll`] for details on scroll direction semantics.
///
/// # Parameters
///
/// * `connection` - Pointer to the connection. Must be non-null and properly aligned.
/// * `delta_x` - Horizontal scroll amount.
/// * `delta_y` - Vertical scroll amount.
///
/// # Returns
///
/// * `AcStatus::Ok` - Event was successfully sent
/// * `AcStatus::Err` - Failed to send event. Call [`ac_last_error`] to retrieve error details.
///
/// # Safety
///
/// The caller must ensure:
/// * `connection` is non-null, properly aligned, and points to a valid `AcConnection`
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_connection_scroll(
    connection: *mut AcConnection,
    _delta_x: f64,
    _delta_y: f64,
) -> AcStatus {
    let _connection = as_mut_checked!(connection);
    todo!()
}

/// FFI wrapper for [`Connection::key_press`].
///
/// See [`Connection::key_press`] for details.
///
/// # Parameters
///
/// * `connection` - Pointer to the connection. Must be non-null and properly aligned.
/// * `key` - The key that was pressed.
///
/// # Returns
///
/// * `AcStatus::Ok` - Event was successfully sent
/// * `AcStatus::Err` - Failed to send event. Call [`ac_last_error`] to retrieve error details.
///
/// # Safety
///
/// The caller must ensure:
/// * `connection` is non-null, properly aligned, and points to a valid `AcConnection`
/// * `key` is a valid `AcKey` value
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_connection_key_press(
    connection: *mut AcConnection,
    _key: AcKey,
) -> AcStatus {
    let _connection = as_mut_checked!(connection);
    todo!()
}

/// FFI wrapper for [`Connection::key_release`].
///
/// See [`Connection::key_release`] for details.
///
/// # Parameters
///
/// * `connection` - Pointer to the connection. Must be non-null and properly aligned.
/// * `key` - The key that was released.
///
/// # Returns
///
/// * `AcStatus::Ok` - Event was successfully sent
/// * `AcStatus::Err` - Failed to send event. Call [`ac_last_error`] to retrieve error details.
///
/// # Safety
///
/// The caller must ensure:
/// * `connection` is non-null, properly aligned, and points to a valid `AcConnection`
/// * `key` is a valid `AcKey` value
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_connection_key_release(
    connection: *mut AcConnection,
    _key: AcKey,
) -> AcStatus {
    let _connection = as_mut_checked!(connection);
    todo!()
}
