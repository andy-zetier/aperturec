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
//! // Create lock state and display request (single display example)
//! AcLockState *lock_state = ac_lock_state_new(false, false, false);
//! AcDisplay *display = ac_display_new(0, 0, 1920, 1080, true);
//!
//! // Create client (pass pointer to first display and count)
//! AcClient *client = ac_client_new(config, lock_state, display, 1);
//! if (!client) {
//!     AcError *error = ac_last_error();
//!     fprintf(stderr, "Client error: %s\n", ac_error_as_str(error));
//!     ac_error_free(error);
//!     ac_configuration_free(config);
//!     ac_lock_state_free(lock_state);
//!     ac_display_free(display);
//!     return 1;
//! }
//!
//! // Free parameters after successful client creation
//! ac_configuration_free(config);
//! ac_lock_state_free(lock_state);
//! ac_display_free(display);
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
//! // Create lock state and display request (single display example)
//! AcLockState *lock_state = ac_lock_state_new(false, false, false);
//! AcDisplay *display = ac_display_new(0, 0, 1920, 1080, true);
//!
//! // Create client directly from command-line arguments
//! AcClient *client = ac_client_from_args(lock_state, display, 1);
//! if (!client) {
//!     AcError *error = ac_last_error();
//!     fprintf(stderr, "Client error: %s\n", ac_error_as_str(error));
//!     ac_error_free(error);
//!     ac_lock_state_free(lock_state);
//!     ac_display_free(display);
//!     return 1;
//! }
//!
//! // Free parameters after successful client creation
//! ac_lock_state_free(lock_state);
//! ac_display_free(display);
//!
//! // Connect and use as above...
//! AcConnection *connection = ac_client_connect(client);
//! // ...
//! ```
//!
//! ## Convenience usage with URI
//!
//! ```c
//! // Create lock state and display request (single display example)
//! AcLockState *lock_state = ac_lock_state_new(false, false, false);
//! AcDisplay *display = ac_display_new(0, 0, 1920, 1080, true);
//!
//! // Create client directly from URI
//! AcClient *client = ac_client_from_uri("aperturec://server:46454", lock_state, display, 1);
//! if (!client) {
//!     AcError *error = ac_last_error();
//!     fprintf(stderr, "Client error: %s\n", ac_error_as_str(error));
//!     ac_error_free(error);
//!     ac_lock_state_free(lock_state);
//!     ac_display_free(display);
//!     return 1;
//! }
//!
//! // Free parameters after successful client creation
//! ac_lock_state_free(lock_state);
//! ac_display_free(display);
//!
//! // Connect and use as above...
//! AcConnection *connection = ac_client_connect(client);
//! // ...
//! ```
//!
//! # Thread Safety
//!
//! Client objects are not thread-safe. Error state is globally shared - retrieve
//! errors immediately after failures in multi-threaded applications.

use crate::{
    Client, Connection, ConnectionError, Event, EventError, InputError, MetricsError,
    config::{Configuration, ConfigurationError},
    state::LockState,
};

use aperturec_graphics::{display, prelude::*};

use atomicbox::AtomicOptionBox;
use libc::*;
use std::{
    error,
    ffi::{CStr, CString},
    mem, ptr, slice,
    sync::{Mutex, atomic::Ordering},
    time::Duration,
};

static LAST_ERROR: AtomicOptionBox<AcError> = AtomicOptionBox::none();

fn set_last_error<E: Into<AcError>>(error: E) {
    LAST_ERROR.store(Some(Box::new(error.into())), Ordering::Relaxed);
}

// Helper to build a CString while setting a consistent FFI-friendly error on failure.
// Returns None and records an Argument error when the source contains interior nulls.
fn cstring_or_set_error(src: &str, err_msg: &'static str) -> Option<CString> {
    match CString::new(src) {
        Ok(s) => Some(s),
        Err(_) => {
            set_last_error(AcErrorKind::Argument(err_msg));
            None
        }
    }
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
    ($p:ident, ()) => {{
        #[allow(clippy::unused_unit)]
        { as_ref_checked!(@impl $p, ()) }
    }};
    (@impl $p:ident, $retval:expr) => {{
        check_aligned!($p, $retval);
        // SAFETY: We check that the pointer is aligned above and non-null in the as_ref call, but
        // otherwise assume the caller is giving us a valid pointer
        let Some(r) = (unsafe { $p.as_ref() }) else {
            set_last_error(AcErrorKind::Argument(concat!(stringify!($p), " is null")));
            #[allow(clippy::unused_unit)]
            return $retval;
        };
        r
    }};
    ($p:ident, $retval:expr) => {{ as_ref_checked!(@impl $p, $retval) }};
}

macro_rules! as_mut_checked {
    ($p:ident) => {{ as_mut_checked!($p, AcStatus::Err) }};
    ($p:ident, ()) => {{
        #[allow(clippy::unused_unit)]
        { as_mut_checked!(@impl $p, ()) }
    }};
    (@impl $p:ident, $retval:expr) => {{
        check_aligned!($p, $retval);
        // SAFETY: We check that the pointer is aligned above and non-null in the as_mut call, but
        // otherwise assume the caller is giving us a valid pointer
        let Some(m) = (unsafe { $p.as_mut() }) else {
            set_last_error(AcErrorKind::Argument(concat!(stringify!($p), " is null")));
            #[allow(clippy::unused_unit)]
            return $retval;
        };
        m
    }};
    ($p:ident, $retval:expr) => {{ as_mut_checked!(@impl $p, $retval) }};
}

macro_rules! as_slice_checked {
    ($p:ident, $len:ident) => {{ as_slice_checked!($p, $len, AcStatus::Err) }};
    ($p:ident, $len:ident, $retval:expr) => {{
        check_aligned!($p, $retval);
        if $p.is_null() {
            set_last_error(AcErrorKind::Argument(concat!(stringify!($p), " is null")));
            #[allow(clippy::unused_unit)]
            return $retval;
        }
        // SAFETY: pointer is non-null and aligned (checked above); caller guarantees length validity
        unsafe { slice::from_raw_parts($p, $len) }
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

macro_rules! extract_event_variant {
    ($event:expr, $pattern:pat => $binding:ident, (), $expected:literal) => {{
        #[allow(clippy::unused_unit)]
        { extract_event_variant!(@impl $event, $pattern => $binding, (), $expected) }
    }};
    (@impl $event:expr, $pattern:pat => $binding:ident, $default:expr, $expected:literal) => {{
        match &$event.event {
            $pattern => $binding,
            _ => {
                set_last_error(AcErrorKind::Argument(concat!(
                    "event is not a ",
                    $expected,
                    " event"
                )));
                return $default;
            }
        }
    }};
    ($event:expr, $pattern:pat => $binding:ident, $default:expr, $expected:literal) => {{
        extract_event_variant!(@impl $event, $pattern => $binding, $default, $expected)
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
    Metrics(MetricsError),
}

impl error::Error for AcErrorKind {}

/// Mouse button identifier.
#[repr(transparent)]
pub struct AcMouseButton(u32);

/// Keyboard key identifier.
#[repr(transparent)]
pub struct AcKey(u32);

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
    let config_out = as_mut_checked!(config_out);
    *config_out = Box::into_raw(Box::new(AcConfiguration(attempt!(
        Configuration::from_argv()
    ))));
    AcStatus::Ok
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
    let config_out = as_mut_checked!(config_out);
    let uri = as_ref_checked!(uri);

    // SAFETY: uri is null and alignment checked above
    let uri = unsafe { CStr::from_ptr(uri) };
    let uri = attempt!(
        uri.to_str()
            .map_err(|_| AcErrorKind::Argument("uri is not valid UTF-8"))
    );
    let config = attempt!(Configuration::from_uri(uri));
    *config_out = Box::into_raw(Box::new(AcConfiguration(config)));
    AcStatus::Ok
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
    is_caps_locked: bool,
    is_num_locked: bool,
    is_scroll_locked: bool,
) -> *mut AcLockState {
    Box::into_raw(Box::new(AcLockState(LockState {
        is_caps_locked,
        is_num_locked,
        is_scroll_locked,
    })))
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

/// ApertureC client. Not thread-safe.
pub struct AcClient(Client);

/// Creates client
///
/// # Parameters
///
/// * `config` - Configuration for the client
/// * `lock_state` - Initial keyboard lock state
/// * `initial_displays_requested` - Pointer to array of displays to request on connect
/// * `num_displays` - Number of displays in the array
///
/// # Returns
///
/// * Pointer to client on success
/// * Null on error - call [`ac_last_error`] for details
///
/// # Safety
///
/// All parameters must be non-null, properly aligned, and point to valid objects.
/// `initial_displays_requested` must point to at least `num_displays` contiguous `AcDisplay`
/// instances. Caller is responsible for freeing all parameters with their respective free functions
/// after this call completes (whether successful or not).
/// Free returned client with [`ac_client_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_client_new(
    config: *const AcConfiguration,
    lock_state: *const AcLockState,
    initial_displays_requested: *mut AcDisplay,
    num_displays: usize,
) -> *mut AcClient {
    let config = as_ref_checked!(config, ptr::null_mut());
    let lock_state = as_ref_checked!(lock_state, ptr::null_mut());
    let displays = as_slice_checked!(initial_displays_requested, num_displays, ptr::null_mut());

    let client = Client::new(
        config.0.clone(),
        lock_state.0,
        displays.iter().map(|d| &d.0),
    );
    Box::into_raw(Box::new(AcClient(client)))
}

/// Creates client from command-line arguments.
///
/// Convenience wrapper that creates a configuration from command-line arguments
/// and passes it to [`ac_client_new`].
///
/// # Parameters
///
/// * `lock_state` - Initial keyboard lock state
/// * `initial_displays_requested` - Pointer to array of displays to request on connect
/// * `num_displays` - Number of displays in the array
///
/// # Returns
///
/// * Pointer to client on success
/// * Null on error - call [`ac_last_error`] for details
///
/// # Safety
///
/// `lock_state` must be non-null, properly aligned, and point to a valid object.
/// `initial_displays_requested` must be non-null, properly aligned, and point to at least `num_displays`
/// contiguous `AcDisplay` instances.
/// Caller is responsible for freeing both parameters with their respective free functions
/// after this call completes (whether successful or not).
/// Free returned client with [`ac_client_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_client_from_args(
    lock_state: *const AcLockState,
    initial_displays_requested: *const AcDisplay,
    num_displays: usize,
) -> *mut AcClient {
    let lock_state = as_ref_checked!(lock_state, ptr::null_mut());
    let displays = as_slice_checked!(initial_displays_requested, num_displays, ptr::null_mut());

    let config = attempt!(Configuration::from_argv(), ptr::null_mut());
    let client = Client::new(config, lock_state.0, displays.iter().map(|d| &d.0));
    Box::into_raw(Box::new(AcClient(client)))
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
/// * `initial_displays_requested` - Pointer to array of displays to request on connect
/// * `num_displays` - Number of displays in the array
///
/// # Returns
///
/// * Pointer to client on success
/// * Null on error - call [`ac_last_error`] for details
///
/// # Safety
///
/// All input pointers must be non-null, properly aligned, and point to valid, caller-owned
/// objects/arrays for the duration of the call. The caller is responsible for freeing any
/// inputs they allocated after this call (whether successful or not). Free the returned
/// client with [`ac_client_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_client_from_uri(
    uri: *const c_char,
    lock_state: *const AcLockState,
    initial_displays_requested: *const AcDisplay,
    num_displays: usize,
) -> *mut AcClient {
    let uri = as_ref_checked!(uri, ptr::null_mut());
    let lock_state = as_ref_checked!(lock_state, ptr::null_mut());
    let displays = as_slice_checked!(initial_displays_requested, num_displays, ptr::null_mut());

    // SAFETY: uri is non-null and properly aligned (checked above)
    let uri_str = attempt!(
        unsafe { CStr::from_ptr(uri) }
            .to_str()
            .map_err(|_| AcErrorKind::Argument("invalid UTF-8 in URI")),
        ptr::null_mut()
    );

    let config = attempt!(Configuration::from_uri(uri_str), ptr::null_mut());
    let client = Client::new(config, lock_state.0, displays.iter().map(|d| &d.0));
    Box::into_raw(Box::new(AcClient(client)))
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
    let client = as_mut_checked!(client, ptr::null_mut());
    let connection = attempt!(client.0.connect(), ptr::null_mut());
    Box::into_raw(Box::new(AcConnection(connection)))
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

/// Alias for [`ac_connection_disconnect`]
///
/// After calling this function, the connection pointer is invalid and must not be used.
///
/// # Safety
///
/// `connection` must be non-null, properly aligned, and point to a valid `AcConnection`
/// previously returned from [`ac_client_connect`]. Passing null records an error via
/// [`ac_last_error`] and exits early. Must not be called twice on the same pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_connection_free(connection: *mut AcConnection) {
    // SAFETY: The caller guarantees (per this function's safety contract) that connection is a
    // valid pointer to an AcConnection. ac_connection_disconnect will validate the pointer and
    // safely disconnect and free it.
    unsafe { ac_connection_disconnect(connection) };
}

/// Disconnects from the server and frees the connection.
///
/// Sends a disconnect message to the server, closes the connection, and frees all
/// associated resources. After calling this function, the connection pointer is invalid
/// and must not be used.
///
/// # Parameters
///
/// * `connection` - Connection to disconnect and free. Passing null records an error and returns.
///
/// # Safety
///
/// `connection` must be non-null, properly aligned, and point to a valid `AcConnection`
/// previously returned from [`ac_client_connect`]. Passing null records an error via
/// [`ac_last_error`] and exits early. Must not be called twice on the same pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_connection_disconnect(connection: *mut AcConnection) {
    #[allow(clippy::unused_unit)]
    let connection = as_mut_checked!(connection, ());
    // SAFETY: We check above that connection is non-null and properly aligned. The caller
    // guarantees (per this function's safety contract) that connection is a valid pointer
    // originally created from a Box via Box::into_raw in ac_client_connect, so reconstructing
    // the Box here is safe.
    let connection = unsafe { *Box::from_raw(connection) };
    connection.0.disconnect();
}

/// Client event.
///
/// Represents an event received from the server. Use [`ac_event_get_type`] to determine
/// the event type, then use the appropriate accessor functions to inspect the event data.
pub struct AcEvent {
    event: Event,
    server_reason_cstr: Mutex<Option<CString>>,
    error_message_cstr: Mutex<Option<CString>>,
}

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

/// Event type discriminator.
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum AcEventType {
    Draw,
    CursorChange,
    DisplayChange,
    Quit,
}

/// Gets the type of an event.
///
/// Use this to determine which accessor functions are valid for this event.
///
/// # Parameters
///
/// * `event` - Event to inspect. Must be non-null and properly aligned.
///
/// # Returns
///
/// The event type, or `AcEventType::Quit` if event pointer is invalid.
///
/// # Safety
///
/// `event` must be non-null, properly aligned, and point to a valid `AcEvent`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_event_get_type(event: *const AcEvent) -> AcEventType {
    let event = as_ref_checked!(event, AcEventType::Quit);
    match &event.event {
        Event::Draw(_) => AcEventType::Draw,
        Event::CursorChange(_) => AcEventType::CursorChange,
        Event::DisplayChange(_) => AcEventType::DisplayChange,
        Event::Quit(_) => AcEventType::Quit,
    }
}

/// Gets the frame number from a Draw event.
///
/// # Parameters
///
/// * `event` - Event to inspect. Must be a Draw event.
///
/// # Returns
///
/// The frame sequence number, or 0 if event is not a Draw event or pointer is invalid.
///
/// # Safety
///
/// `event` must be non-null, properly aligned, and point to a valid `AcEvent`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_event_draw_get_frame(event: *const AcEvent) -> usize {
    let event = as_ref_checked!(event, 0);
    let draw = extract_event_variant!(event, Event::Draw(draw) => draw, 0, "Draw");
    draw.frame
}

/// Gets the origin (top-left position) from a Draw event.
///
/// # Parameters
///
/// * `event` - Event to inspect. Must be a Draw event.
/// * `x_out` - Output parameter for X coordinate. Must be non-null and properly aligned.
/// * `y_out` - Output parameter for Y coordinate. Must be non-null and properly aligned.
///
/// # Safety
///
/// All pointers must be non-null, properly aligned, and point to valid memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_event_draw_get_origin(
    event: *const AcEvent,
    x_out: *mut usize,
    y_out: *mut usize,
) {
    let event = as_ref_checked!(event, ());
    let x_out = as_mut_checked!(x_out, ());
    let y_out = as_mut_checked!(y_out, ());

    let draw = extract_event_variant!(event, Event::Draw(draw) => draw, (), "Draw");
    *x_out = draw.origin.x;
    *y_out = draw.origin.y;
}

/// Gets the size (width and height) from a Draw event.
///
/// # Parameters
///
/// * `event` - Event to inspect. Must be a Draw event.
/// * `width_out` - Output parameter for width. Must be non-null and properly aligned.
/// * `height_out` - Output parameter for height. Must be non-null and properly aligned.
///
/// # Safety
///
/// All pointers must be non-null, properly aligned, and point to valid memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_event_draw_get_size(
    event: *const AcEvent,
    width_out: *mut usize,
    height_out: *mut usize,
) {
    let event = as_ref_checked!(event, ());
    let width_out = as_mut_checked!(width_out, ());
    let height_out = as_mut_checked!(height_out, ());

    let draw = extract_event_variant!(event, Event::Draw(draw) => draw, (), "Draw");
    let shape = draw.pixels.shape();
    *height_out = shape[0];
    *width_out = shape[1];
}

/// Gets pixel data from a Draw event.
///
/// Returns a pointer to RGB24 pixel data. Each pixel is 3 bytes (R, G, B).
/// The data is valid until the event is freed with [`ac_event_free`].
///
/// # Parameters
///
/// * `event` - Event to inspect. Must be a Draw event.
/// * `width_out` - Output parameter for width. Must be non-null and properly aligned.
/// * `height_out` - Output parameter for height. Must be non-null and properly aligned.
/// * `stride_out` - Output parameter for row stride in bytes. Must be non-null and properly aligned.
///
/// # Returns
///
/// Pointer to pixel data, or null if event is not a Draw event or pointers are invalid.
///
/// # Safety
///
/// All pointers must be non-null, properly aligned, and point to valid memory.
/// The returned pixel data pointer is valid only while the event is alive.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_event_draw_get_pixels(
    event: *const AcEvent,
    width_out: *mut usize,
    height_out: *mut usize,
    stride_out: *mut usize,
) -> *const u8 {
    let event = as_ref_checked!(event, ptr::null());
    let width_out = as_mut_checked!(width_out, ptr::null());
    let height_out = as_mut_checked!(height_out, ptr::null());
    let stride_out = as_mut_checked!(stride_out, ptr::null());

    let draw = extract_event_variant!(event, Event::Draw(draw) => draw, ptr::null(), "Draw");
    let shape = draw.pixels.shape();
    *height_out = shape[0];
    *width_out = shape[1];
    *stride_out = draw.pixels.stride_of(ndarray::Axis(0)) as usize * mem::size_of::<Pixel24>();
    draw.pixels.as_ptr() as *const u8
}

/// Gets the hotspot position from a Cursor event.
///
/// The hotspot indicates which pixel of the cursor image corresponds to the pointer position.
///
/// # Parameters
///
/// * `event` - Event to inspect. Must be a CursorChange event.
/// * `x_out` - Output parameter for X coordinate. Must be non-null and properly aligned.
/// * `y_out` - Output parameter for Y coordinate. Must be non-null and properly aligned.
///
/// # Safety
///
/// All pointers must be non-null, properly aligned, and point to valid memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_event_cursor_get_hotspot(
    event: *const AcEvent,
    x_out: *mut usize,
    y_out: *mut usize,
) {
    let event = as_ref_checked!(event, ());
    let x_out = as_mut_checked!(x_out, ());
    let y_out = as_mut_checked!(y_out, ());

    let cursor =
        extract_event_variant!(event, Event::CursorChange(cursor) => cursor, (), "CursorChange");
    *x_out = cursor.hot.x;
    *y_out = cursor.hot.y;
}

/// Gets the size of the cursor image from a Cursor event.
///
/// # Parameters
///
/// * `event` - Event to inspect. Must be a CursorChange event.
/// * `width_out` - Output parameter for width. Must be non-null and properly aligned.
/// * `height_out` - Output parameter for height. Must be non-null and properly aligned.
///
/// # Safety
///
/// All pointers must be non-null, properly aligned, and point to valid memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_event_cursor_get_size(
    event: *const AcEvent,
    width_out: *mut usize,
    height_out: *mut usize,
) {
    let event = as_ref_checked!(event, ());
    let width_out = as_mut_checked!(width_out, ());
    let height_out = as_mut_checked!(height_out, ());

    let cursor =
        extract_event_variant!(event, Event::CursorChange(cursor) => cursor, (), "CursorChange");
    let shape = cursor.pixels.shape();
    *height_out = shape[0];
    *width_out = shape[1];
}

/// Gets pixel data from a Cursor event.
///
/// Returns a pointer to RGBA32 pixel data. Each pixel is 4 bytes (R, G, B, A).
/// The data is valid until the event is freed with [`ac_event_free`].
///
/// # Parameters
///
/// * `event` - Event to inspect. Must be a CursorChange event.
/// * `width_out` - Output parameter for width. Must be non-null and properly aligned.
/// * `height_out` - Output parameter for height. Must be non-null and properly aligned.
///
/// # Returns
///
/// Pointer to pixel data, or null if event is not a CursorChange event or pointers are invalid.
///
/// # Safety
///
/// All pointers must be non-null, properly aligned, and point to valid memory.
/// The returned pixel data pointer is valid only while the event is alive.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_event_cursor_get_pixels(
    event: *const AcEvent,
    width_out: *mut usize,
    height_out: *mut usize,
) -> *const u8 {
    let event = as_ref_checked!(event, ptr::null());
    let width_out = as_mut_checked!(width_out, ptr::null());
    let height_out = as_mut_checked!(height_out, ptr::null());

    let cursor = extract_event_variant!(event, Event::CursorChange(cursor) => cursor, ptr::null(), "CursorChange");
    let shape = cursor.pixels.shape();
    *height_out = shape[0];
    *width_out = shape[1];
    cursor.pixels.as_ptr() as *const u8
}

/// Quit reason type discriminator.
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum AcQuitReasonType {
    ServerGoodbye,
    UnrecoverableError,
}

/// Gets the quit reason type from a Quit event.
///
/// # Parameters
///
/// * `event` - Event to inspect. Must be a Quit event.
///
/// # Returns
///
/// The quit reason type, or `AcQuitReasonType::UnrecoverableError` if event is not a Quit event or pointer is invalid.
///
/// # Safety
///
/// `event` must be non-null, properly aligned, and point to a valid `AcEvent`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_event_quit_get_reason_type(event: *const AcEvent) -> AcQuitReasonType {
    let event = as_ref_checked!(event, AcQuitReasonType::UnrecoverableError);
    match &event.event {
        Event::Quit(crate::QuitReason::ServerGoodbye { .. }) => AcQuitReasonType::ServerGoodbye,
        Event::Quit(crate::QuitReason::UnrecoverableError(_)) => {
            AcQuitReasonType::UnrecoverableError
        }
        _ => {
            set_last_error(AcErrorKind::Argument("event is not a Quit event"));
            AcQuitReasonType::UnrecoverableError
        }
    }
}

/// Gets the server-provided reason string from a ServerGoodbye quit event.
///
/// Returns a pointer to a null-terminated C string. The string is valid until
/// the event is freed with [`ac_event_free`].
///
/// # Parameters
///
/// * `event` - Event to inspect. Must be a Quit event with ServerGoodbye reason.
///
/// # Returns
///
/// Pointer to reason string, or null if event is not a ServerGoodbye quit event or pointer is invalid.
///
/// # Safety
///
/// `event` must be non-null, properly aligned, and point to a valid `AcEvent`.
/// The returned string pointer is valid only while the event is alive.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_event_quit_get_server_reason(event: *const AcEvent) -> *const c_char {
    let event = as_ref_checked!(event, ptr::null());
    match &event.event {
        Event::Quit(crate::QuitReason::ServerGoodbye { server_reason }) => {
            let mut guard = match event.server_reason_cstr.lock() {
                Ok(g) => g,
                Err(_) => {
                    set_last_error(AcErrorKind::Argument("server reason cache poisoned"));
                    return ptr::null();
                }
            };
            if guard.is_none() {
                *guard = cstring_or_set_error(
                    server_reason.as_str(),
                    "server reason contains interior null bytes",
                );
                if guard.is_none() {
                    return ptr::null();
                }
            }
            guard.as_ref().map(|s| s.as_ptr()).unwrap_or(ptr::null())
        }
        _ => {
            set_last_error(AcErrorKind::Argument(
                "event is not a Quit event with ServerGoodbye reason",
            ));
            ptr::null()
        }
    }
}

/// Gets the error message from an UnrecoverableError quit event.
///
/// Returns a pointer to a null-terminated C string. The string is valid until
/// the event is freed with [`ac_event_free`].
///
/// # Parameters
///
/// * `event` - Event to inspect. Must be a Quit event with UnrecoverableError reason.
///
/// # Returns
///
/// Pointer to error message, or null if event is not an UnrecoverableError quit event or pointer is invalid.
///
/// # Safety
///
/// `event` must be non-null, properly aligned, and point to a valid `AcEvent`.
/// The returned string pointer is valid only while the event is alive.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_event_quit_get_error_message(event: *const AcEvent) -> *const c_char {
    let event = as_ref_checked!(event, ptr::null());
    match &event.event {
        Event::Quit(crate::QuitReason::UnrecoverableError(err)) => {
            let mut guard = match event.error_message_cstr.lock() {
                Ok(g) => g,
                Err(_) => {
                    set_last_error(AcErrorKind::Argument("error message cache poisoned"));
                    return ptr::null();
                }
            };
            if guard.is_none() {
                let error_str = err.to_string();
                *guard =
                    cstring_or_set_error(&error_str, "error message contains interior null bytes");
                if guard.is_none() {
                    return ptr::null();
                }
            }
            guard.as_ref().map(|s| s.as_ptr()).unwrap_or(ptr::null())
        }
        _ => {
            set_last_error(AcErrorKind::Argument(
                "event is not a Quit event with UnrecoverableError reason",
            ));
            ptr::null()
        }
    }
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
    let connection = as_mut_checked!(connection);
    let event_out = as_mut_checked!(event_out);
    if let Some(event) = attempt!(connection.0.poll_event()) {
        *event_out = Box::into_raw(Box::new(AcEvent {
            event,
            server_reason_cstr: Mutex::new(None),
            error_message_cstr: Mutex::new(None),
        }));
    } else {
        *event_out = ptr::null_mut();
    }
    AcStatus::Ok
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
    let connection = as_mut_checked!(connection);
    let event_out = as_mut_checked!(event_out);
    let event = attempt!(connection.0.wait_event());
    *event_out = Box::into_raw(Box::new(AcEvent {
        event,
        server_reason_cstr: Mutex::new(None),
        error_message_cstr: Mutex::new(None),
    }));
    AcStatus::Ok
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
    timeout_ms: u64,
    event_out: *mut *mut AcEvent,
) -> AcStatus {
    let connection = as_mut_checked!(connection);
    let event_out = as_mut_checked!(event_out);
    if let Some(event) = attempt!(
        connection
            .0
            .wait_event_timeout(Duration::from_millis(timeout_ms))
    ) {
        *event_out = Box::into_raw(Box::new(AcEvent {
            event,
            server_reason_cstr: Mutex::new(None),
            error_message_cstr: Mutex::new(None),
        }));
    } else {
        *event_out = ptr::null_mut();
    }
    AcStatus::Ok
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
    x: u64,
    y: u64,
) -> AcStatus {
    let connection = as_mut_checked!(connection);
    let x = attempt!(x.try_into().map_err(|_| {
        AcErrorKind::Argument("x coordinate exceeds usize::MAX for this architecture")
    }));
    let y = attempt!(y.try_into().map_err(|_| {
        AcErrorKind::Argument("y coordinate exceeds usize::MAX for this architecture")
    }));
    attempt!(connection.0.pointer_move(x, y));
    AcStatus::Ok
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
    button: AcMouseButton,
    x: u64,
    y: u64,
) -> AcStatus {
    let connection = as_mut_checked!(connection);
    let x = attempt!(x.try_into().map_err(|_| {
        AcErrorKind::Argument("x coordinate exceeds usize::MAX for this architecture")
    }));
    let y = attempt!(y.try_into().map_err(|_| {
        AcErrorKind::Argument("y coordinate exceeds usize::MAX for this architecture")
    }));
    attempt!(connection.0.mouse_button_press(button.0, x, y));
    AcStatus::Ok
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
    button: AcMouseButton,
    x: u64,
    y: u64,
) -> AcStatus {
    let connection = as_mut_checked!(connection);
    let x = attempt!(x.try_into().map_err(|_| {
        AcErrorKind::Argument("x coordinate exceeds usize::MAX for this architecture")
    }));
    let y = attempt!(y.try_into().map_err(|_| {
        AcErrorKind::Argument("y coordinate exceeds usize::MAX for this architecture")
    }));
    attempt!(connection.0.mouse_button_release(button.0, x, y));
    AcStatus::Ok
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
    key: AcKey,
) -> AcStatus {
    let connection = as_mut_checked!(connection);
    attempt!(connection.0.key_press(key.0));
    AcStatus::Ok
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
    key: AcKey,
) -> AcStatus {
    let connection = as_mut_checked!(connection);
    attempt!(connection.0.key_release(key.0));
    AcStatus::Ok
}

/// Display description.
///
/// Describes a display's area (position and size) and enabled state.
/// Used to request display configurations from the server.
pub struct AcDisplay(display::Display);

/// Creates display with specified area and enabled state.
///
/// # Parameters
///
/// * `x_origin` - X coordinate of display origin
/// * `y_origin` - Y coordinate of display origin
/// * `width` - Display width in pixels
/// * `height` - Display height in pixels
/// * `is_enabled` - Whether display is enabled
///
/// # Returns
///
/// * Pointer to newly allocated display
/// * Never returns null
///
/// # Safety
///
/// Caller must free returned display with [`ac_display_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_display_new(
    x_origin: usize,
    y_origin: usize,
    width: usize,
    height: usize,
    is_enabled: bool,
) -> *mut AcDisplay {
    Box::into_raw(Box::new(AcDisplay(display::Display {
        area: Rect::new(Point::new(x_origin, y_origin), Size::new(width, height)),
        is_enabled,
    })))
}

/// Frees display.
///
/// # Parameters
///
/// * `display` - Display to free, or null (no-op)
///
/// # Safety
///
/// Must not be called twice on same pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_display_free(display: *mut AcDisplay) {
    drop_owned!(display);
}

/// Requests display configuration change.
///
/// Sends a display configuration change request to the server using the provided
/// display array, which describes the desired display configuration.
///
/// # Parameters
///
/// * `connection` - Pointer to the connection. Must be non-null and properly aligned.
/// * `displays` - Pointer to array of displays. Must be non-null and properly aligned.
/// * `num_displays` - Number of displays in the array
///
/// # Returns
///
/// * `AcStatus::Ok` - Request was successfully sent
/// * `AcStatus::Err` - Failed to send request. Call [`ac_last_error`] to retrieve error details.
///
/// # Safety
///
/// The caller must ensure:
/// * `connection` is non-null, properly aligned, and points to a valid `AcConnection`
/// * `displays` is non-null, properly aligned, and points to a valid array of at least `num_displays` elements
/// * All displays in the array are valid `AcDisplay` objects
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_connection_request_display_change(
    connection: *mut AcConnection,
    displays: *const AcDisplay,
    num_displays: usize,
) -> AcStatus {
    let connection = as_mut_checked!(connection);
    let displays = as_slice_checked!(displays, num_displays);
    attempt!(
        connection
            .0
            .request_display_change(displays.iter().map(|d| &d.0))
    );
    AcStatus::Ok
}

/// Display configuration.
///
/// Describes the arrangement of displays and their decoders.
pub struct AcDisplayConfiguration(display::DisplayConfiguration);

/// Frees display configuration.
///
/// # Parameters
///
/// * `config` - Display configuration to free, or null (no-op)
///
/// # Safety
///
/// Must not be called twice on same pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_display_configuration_free(config: *mut AcDisplayConfiguration) {
    drop_owned!(config);
}

/// Gets the configuration ID.
///
/// The ID is incremented each time the display configuration changes.
///
/// # Parameters
///
/// * `config` - Display configuration to inspect. Must be non-null and properly aligned.
///
/// # Returns
///
/// The configuration ID, or 0 if pointer is invalid.
///
/// # Safety
///
/// `config` must be non-null, properly aligned, and point to a valid `AcDisplayConfiguration`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_display_configuration_get_id(
    config: *const AcDisplayConfiguration,
) -> usize {
    let config = as_ref_checked!(config, 0);
    config.0.id
}

/// Gets the number of encoders (decoders) in the configuration.
///
/// # Parameters
///
/// * `config` - Display configuration to inspect. Must be non-null and properly aligned.
///
/// # Returns
///
/// The encoder count, or 0 if pointer is invalid.
///
/// # Safety
///
/// `config` must be non-null, properly aligned, and point to a valid `AcDisplayConfiguration`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_display_configuration_encoder_count(
    config: *const AcDisplayConfiguration,
) -> usize {
    let config = as_ref_checked!(config, 0);
    config.0.encoder_count()
}

/// Gets the number of displays in the configuration.
///
/// # Parameters
///
/// * `config` - Display configuration to inspect. Must be non-null and properly aligned.
///
/// # Returns
///
/// The display count, or 0 if pointer is invalid.
///
/// # Safety
///
/// `config` must be non-null, properly aligned, and point to a valid `AcDisplayConfiguration`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_display_configuration_get_display_count(
    config: *const AcDisplayConfiguration,
) -> usize {
    let config = as_ref_checked!(config, 0);
    config.0.display_decoder_infos.len()
}

/// Gets a display from the configuration at the specified index.
///
/// Copies the display information into the provided output parameter.
///
/// # Parameters
///
/// * `config` - Display configuration to inspect. Must be non-null and properly aligned.
/// * `index` - Index of the display to retrieve (0-based).
/// * `display_out` - Output parameter for the display. Must be non-null and properly aligned.
///
/// # Returns
///
/// * `AcStatus::Ok` - Display was successfully retrieved
/// * `AcStatus::Err` - Index out of bounds or invalid pointer
///
/// # Safety
///
/// All pointers must be non-null, properly aligned, and point to valid memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_display_configuration_get_display_at(
    config: *const AcDisplayConfiguration,
    index: usize,
    display_out: *mut AcDisplay,
) -> AcStatus {
    let config = as_ref_checked!(config);
    let display_out = as_mut_checked!(display_out);

    let ddi = match config.0.display_decoder_infos.get(index) {
        Some(d) => d,
        None => {
            set_last_error(AcErrorKind::Argument("index out of bounds"));
            return AcStatus::Err;
        }
    };
    display_out.0 = ddi.display.clone();
    AcStatus::Ok
}

/// Gets the display configuration from a DisplayChange event.
///
/// Returns an owned copy that the caller must free with [`ac_display_configuration_free`].
///
/// # Parameters
///
/// * `event` - Event to inspect. Must be a DisplayChange event.
///
/// # Returns
///
/// Pointer to display configuration, or null if event is not a DisplayChange event or pointer is invalid.
///
/// # Safety
///
/// `event` must be non-null, properly aligned, and point to a valid `AcEvent`.
/// Caller must free the returned configuration with [`ac_display_configuration_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_event_display_get_configuration(
    event: *const AcEvent,
) -> *mut AcDisplayConfiguration {
    let event = as_ref_checked!(event, ptr::null_mut());
    let dc = extract_event_variant!(event, Event::DisplayChange(dc) => dc, ptr::null_mut(), "DisplayChange");
    Box::into_raw(Box::new(AcDisplayConfiguration(dc.clone())))
}

/// Gets the current display configuration from a connection.
///
/// Returns an owned copy that the caller must free with [`ac_display_configuration_free`].
///
/// # Parameters
///
/// * `connection` - Connection to query. Must be non-null and properly aligned.
///
/// # Returns
///
/// Pointer to display configuration, or null if pointer is invalid.
///
/// # Safety
///
/// `connection` must be non-null, properly aligned, and point to a valid `AcConnection`.
/// Caller must free the returned configuration with [`ac_display_configuration_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ac_connection_display_configuration(
    connection: *const AcConnection,
) -> *mut AcDisplayConfiguration {
    let connection = as_ref_checked!(connection, ptr::null_mut());
    let config = connection.0.display_configuration();
    Box::into_raw(Box::new(AcDisplayConfiguration(config)))
}
