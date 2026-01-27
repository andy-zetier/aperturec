use crate::channels::spawn_rx_thread;

use aperturec_channel::{self as channel, Sender as _};
use aperturec_protocol::tunnel as proto;
use aperturec_utils::channels::SenderExt;

use crossbeam::channel::{
    Receiver, RecvTimeoutError, SendTimeoutError, Sender, bounded, select_biased,
};
use mio::{
    Events, Interest, Poll, Token, Waker,
    net::{TcpListener, TcpStream},
};
use std::{
    collections::{BTreeMap, VecDeque},
    io::{self, Read, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream as StdTcpStream, ToSocketAddrs},
    ops::ControlFlow,
    sync::Arc,
    thread::{self, JoinHandle},
    time::Duration,
};
use tracing::*;

/// Single-result handshake channel between accept thread and tc-main.
const ACCEPT_WAKER_CHAN_CAP: usize = 1;
/// How long tc-main waits for each accept thread to report its waker (startup guard).
const ACCEPT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(1);
/// Per-connector TCP connect timeout for server-bound tunnels.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Bound on delivering StreamState::Started from connector to tc-main (avoid hanging thread).
const CONNECTOR_NOTIFY_TIMEOUT: Duration = Duration::from_secs(1);
/// Capacity of IO write queue feeding the per-stream poll loop.
const IO_WRITE_CHAN_CAP: usize = 16;
/// mio event buffer size; tuned to cover listener + waker plus a few ready sockets.
const POLL_EVENT_CAPACITY: usize = 16;
/// Token assigned to the socket in mio polls (shared convention across threads).
const TOKEN_SOCKET: Token = Token(0);
/// Token assigned to the waker in mio polls (shared convention across threads).
const TOKEN_WAKER: Token = Token(1);

mod io_waker {
    use super::*;

    use std::sync::atomic::{AtomicU8, Ordering};

    const NONE: u8 = 0b00;
    const DATA: u8 = 0b01;
    const CANCEL: u8 = 0b10;

    #[derive(Debug, Clone, Copy)]
    pub struct WakeFlags {
        bits: u8,
    }

    impl WakeFlags {
        fn new(bits: u8) -> Self {
            Self { bits }
        }

        pub fn cancel(&self) -> bool {
            self.bits & CANCEL != 0
        }
    }

    /// Convenience wrapper that couples wake flags with the mio waker used to deliver them.
    #[derive(Debug, Clone)]
    pub struct WakeHandle {
        flags: Arc<AtomicU8>,
        waker: Arc<Waker>,
    }

    impl WakeHandle {
        pub fn new_with_waker(waker: Arc<Waker>) -> Self {
            Self {
                flags: Arc::new(AtomicU8::new(NONE)),
                waker,
            }
        }

        pub fn notify_data(&self) {
            self.flags.fetch_or(DATA, Ordering::Release);
            if let Err(error) = self.waker.wake() {
                warn!(%error, "WakeHandle::notify_data failed to wake poller");
            }
        }

        pub fn notify_cancel(&self) {
            self.flags.fetch_or(CANCEL, Ordering::Release);
            if let Err(error) = self.waker.wake() {
                warn!(%error, "WakeHandle::notify_cancel failed to wake poller");
            }
        }

        pub fn take(&self) -> WakeFlags {
            WakeFlags::new(self.flags.swap(NONE, Ordering::AcqRel))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use mio::{Poll, Token};

        fn make_handle() -> WakeHandle {
            let poll = Box::leak(Box::new(Poll::new().expect("create poll")));
            // Leak the poll so its registry outlives the waker; fine for tests.
            let waker = Arc::new(Waker::new(poll.registry(), Token(0)).expect("create waker"));
            WakeHandle::new_with_waker(waker)
        }

        #[test]
        fn data_flag_round_trips() {
            let handle = make_handle();
            handle.notify_data();
            let flags = handle.take();
            assert!(flags.bits & DATA != 0);
            assert!(!flags.cancel());
        }

        #[test]
        fn cancel_flag_round_trips() {
            let handle = make_handle();
            handle.notify_cancel();
            let flags = handle.take();
            assert!(flags.bits & DATA == 0);
            assert!(flags.cancel());
        }

        #[test]
        fn both_flags_round_trip() {
            let handle = make_handle();
            handle.notify_data();
            handle.notify_cancel();
            let flags = handle.take();
            assert!(flags.bits & DATA != 0);
            assert!(flags.cancel());
        }
    }
}

/// Stream lifecycle tracked per tunnel stream.
enum State {
    /// Server instructed open; TCP connect in flight; buffer early data.
    PendingLocalConnect { buffered: Vec<u8> },
    /// IO running with explicit direction flags.
    Open {
        io: IoHandles,
        inbound_open: bool,
        outbound_open: bool,
    },
    /// Teardown/flush phase; no new inbound data should be accepted.
    Draining {
        io: IoHandles,
        inbound_open: bool,
        outbound_open: bool,
    },
}

#[derive(Debug)]
struct IoHandles {
    /// Sender feeding the per-stream IO loop's outbound write queue.
    to_tx: Sender<Vec<u8>>,
    /// WakeHandle used to poke the poll loop for data or cancellation.
    wake: io_waker::WakeHandle,
    /// JoinHandle for the per-stream IO thread, used during teardown.
    io_thread: JoinHandle<()>,
}

/// Notifications emitted by tunnel workers to inform the primary thread.
#[derive(Debug)]
pub enum PrimaryThreadNotification {
    /// Fatal tunnel error that requires surfacing to the user.
    ChannelError(ChannelError),
}
type Ptn = PrimaryThreadNotification;

/// Internal coordination messages for tunnel channel setup.
#[derive(Debug)]
pub enum Notification {
    /// Map of tunnel IDs to the server's allocation responses.
    Allocations(BTreeMap<u64, proto::Response>),
    /// Instruct the client tunnel channel threads to terminate
    Terminate,
}

#[derive(Debug, thiserror::Error)]
pub enum ChannelError {
    #[error(transparent)]
    ChannelRx(#[from] channel::codec::in_order::RxError),
    #[error(transparent)]
    ChannelTx(#[from] channel::codec::in_order::TxError),
    #[error("tunnel {tunnel_id} accept thread failed to initialize: {reason}")]
    TunnelAcceptInitFailed {
        tunnel_id: u64,
        #[source]
        reason: AcceptInitError,
    },
}

#[derive(Debug, thiserror::Error)]
enum StreamError {
    #[error(transparent)]
    IO(#[from] io::Error),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum AcceptInitError {
    #[error("failed parsing '{addr}' as IP address: {source}")]
    ParseBindAddress {
        addr: String,
        #[source]
        source: std::net::AddrParseError,
    },
    #[error("failed converting {port} to 16-bit port")]
    InvalidPort { port: u32 },
    #[error("failed binding {addr}:{port}: {source}")]
    Bind {
        addr: IpAddr,
        port: u16,
        #[source]
        source: io::Error,
    },
    #[error("failed to create poll: {0}")]
    Poll(#[source] io::Error),
    #[error("failed to register listener with poll: {0}")]
    Register(#[source] io::Error),
    #[error("failed to create waker: {0}")]
    Waker(#[source] io::Error),
    #[error("accept thread did not report initialization result")]
    NoHandshake,
}

#[derive(Debug)]
enum StreamState {
    Started {
        id: Id,
        stream: TcpStream,
    },
    Finished {
        id: Id,
        result: Result<(), StreamError>,
    },
}

type ThreadMap = BTreeMap<String, JoinHandle<()>>;
type QuicRxResult = Result<proto::Message, channel::codec::in_order::RxError>;

struct TcMainInputs {
    from_pt_rx: Receiver<Notification>,
    from_quic_rx: Receiver<QuicRxResult>,
    stream_state_rx: Receiver<StreamState>,
    to_quic_tx: Sender<proto::Message>,
    stream_state_tx: Sender<StreamState>,
}

struct TunnelMaps {
    server_bound: BTreeMap<u64, proto::Description>,
    client_bound: BTreeMap<u64, proto::Description>,
}

struct WorkerHandles {
    accept_wakers: BTreeMap<u64, Waker>,
    threads: ThreadMap,
}

/// Start per-tunnel acceptors for all client-bound tunnels and return their shutdown wakers plus
/// join handles. Each acceptor performs a waker handshake so startup can fail fast instead of
/// hanging waiting for a missing waker.
fn spawn_accept_threads(
    client_bound_tunnels: &BTreeMap<u64, proto::Description>,
    stream_state_tx: &Sender<StreamState>,
    pt_tx: &Sender<Ptn>,
) -> (BTreeMap<u64, Waker>, ThreadMap) {
    let mut accept_wakers: BTreeMap<u64, Waker> = BTreeMap::new();
    let mut threads: ThreadMap = BTreeMap::new();

    for (&tunnel_id, desc) in client_bound_tunnels {
        let desc = desc.clone();
        debug!(?desc);
        let stream_state_tx = stream_state_tx.clone();
        let (waker_tx, waker_rx) = bounded::<Result<Waker, AcceptInitError>>(ACCEPT_WAKER_CHAN_CAP);

        let accept_thread = thread::spawn(move || {
            let _s = debug_span!("tc-accept", tunnel_id).entered();
            let mut stream_id = 0;

            let addr = if desc.bind_address.is_empty() {
                // TODO: fix IPv6 behavior. Right now, our default is just to bind to IPv4,
                // which may not always be right
                IpAddr::V4(Ipv4Addr::UNSPECIFIED)
            } else {
                match desc.bind_address.parse::<IpAddr>() {
                    Ok(addr) => addr,
                    Err(error) => {
                        warn!(%error, "failed parsing '{}' as IP address", desc.bind_address);
                        waker_tx
                            .send(Err(AcceptInitError::ParseBindAddress {
                                addr: desc.bind_address.clone(),
                                source: error,
                            }))
                            .expect("waker handshake channel should be alive during accept init");
                        return;
                    }
                }
            };
            let Ok(port) = u16::try_from(desc.bind_port) else {
                warn!("failed converting {} to 16-bit port", desc.bind_port);
                waker_tx
                    .send(Err(AcceptInitError::InvalidPort {
                        port: desc.bind_port,
                    }))
                    .expect("waker handshake channel should be alive during accept init");
                return;
            };

            let mut listener = match TcpListener::bind(SocketAddr::new(addr, port)) {
                Ok(listener) => listener,
                Err(error) => {
                    warn!(%error, "failed binding {addr}:{port}");
                    waker_tx
                        .send(Err(AcceptInitError::Bind {
                            addr,
                            port,
                            source: error,
                        }))
                        .expect("waker handshake channel should be alive during accept init");
                    return;
                }
            };
            trace!(?addr, ?port, "bound");

            let mut poll = match Poll::new() {
                Ok(p) => p,
                Err(error) => {
                    warn!(%error, "failed to create poll");
                    waker_tx
                        .send(Err(AcceptInitError::Poll(error)))
                        .expect("waker handshake channel should be alive during accept init");
                    return;
                }
            };
            let listener_token = TOKEN_SOCKET;
            let waker_token = TOKEN_WAKER;

            // Register listener with mio and create a waker we can trigger on shutdown.
            if let Err(error) =
                poll.registry()
                    .register(&mut listener, listener_token, Interest::READABLE)
            {
                warn!(%error, "failed to register listener with poll");
                waker_tx
                    .send(Err(AcceptInitError::Register(error)))
                    .expect("waker handshake channel should be alive during accept init");
                return;
            }
            let waker = match Waker::new(poll.registry(), waker_token) {
                Ok(w) => w,
                Err(error) => {
                    warn!(%error, "failed to create waker");
                    waker_tx
                        .send(Err(AcceptInitError::Waker(error)))
                        .expect("waker handshake channel should be alive during accept init");
                    return;
                }
            };
            waker_tx
                .send(Ok(waker))
                .expect("waker handshake channel should be alive during accept init");
            let mut events = Events::with_capacity(POLL_EVENT_CAPACITY);

            'accept_event_loop: loop {
                trace!("waiting for events");
                if let Err(error) = poll.poll(&mut events, None) {
                    warn!(%error, "poll failed");
                    break 'accept_event_loop;
                }

                for event in events.iter() {
                    trace!(?event);
                    let token = event.token();
                    if token == listener_token {
                        match listener.accept() {
                            Ok((stream, _)) => {
                                debug!(stream_id, "accepted");
                                let stream_state = StreamState::Started {
                                    id: Id::new(tunnel_id, stream_id),
                                    stream,
                                };
                                if stream_state_tx.send(stream_state).is_err() {
                                    debug!("tc-main is gone, exiting");
                                    break 'accept_event_loop;
                                };
                                debug!(stream_id, "forwarded to new stream handler");
                                stream_id += 1;
                            }
                            Err(error)
                                if matches!(
                                    error.kind(),
                                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                                ) =>
                            {
                                trace!(%error, "accept would-block/interrupted");
                                continue;
                            }
                            Err(error) => {
                                warn!(%error, stream_id, "failed accepting");
                                break 'accept_event_loop;
                            }
                        }
                    } else if token == waker_token {
                        debug!("waker signaled, exiting accept loop");
                        return;
                    }
                }
            }
        });

        match waker_rx.recv_timeout(ACCEPT_HANDSHAKE_TIMEOUT) {
            Ok(Ok(waker)) => {
                accept_wakers.insert(tunnel_id, waker);
                threads.insert(format!("tc-accept-{tunnel_id}"), accept_thread);
            }
            Ok(Err(err)) => handle_accept_init_failure(tunnel_id, err, pt_tx, accept_thread),
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                handle_accept_init_failure(
                    tunnel_id,
                    AcceptInitError::NoHandshake,
                    pt_tx,
                    accept_thread,
                )
            }
        }
    }

    (accept_wakers, threads)
}

fn handle_accept_init_failure(
    tunnel_id: u64,
    reason: AcceptInitError,
    pt_tx: &Sender<Ptn>,
    accept_thread: JoinHandle<()>,
) {
    warn!(?tunnel_id, %reason, "accept thread failed to initialize");
    pt_tx.send_or_warn(Ptn::ChannelError(ChannelError::TunnelAcceptInitFailed {
        tunnel_id,
        reason,
    }));
    if let Err(join_err) = accept_thread.join() {
        warn!(?tunnel_id, "accept thread panicked: {join_err:?}");
    }
}

/// Launch the QUIC transmit/receive worker threads and return their join handles.
fn spawn_quic_threads(
    tc_rx: channel::ClientTunnelReceiveHalf,
    mut tc_tx: channel::ClientTunnelSendHalf,
    to_quic_rx: Receiver<proto::Message>,
    from_quic_tx: Sender<QuicRxResult>,
    pt_tx: Sender<Ptn>,
) -> ThreadMap {
    let mut threads = ThreadMap::new();

    let quic_tx_thread = thread::spawn(move || {
        let _s = debug_span!("tc-quic-tx").entered();
        debug!("started");
        while let Ok(msg) = to_quic_rx.recv() {
            if let Err(error) = tc_tx.send(msg) {
                pt_tx.send_or_warn(Ptn::ChannelError(error.into()));
                break;
            }
        }
        debug!("exiting");
    });
    threads.insert("tc-quic-tx".to_string(), quic_tx_thread);

    let quic_rx_thread = spawn_rx_thread(tc_rx, from_quic_tx, debug_span!("tc-quic-rx"));
    threads.insert("tc-quic-rx".to_string(), quic_rx_thread);

    threads
}

/// Coordinator loop that multiplexes primary notifications, QUIC messages, and stream state
/// updates, and owns orderly shutdown of all tunnel workers. Spawns a detached thread; no handle
/// is needed because it drains and joins all worker threads before exit.
fn spawn_tc_main(
    inputs: TcMainInputs,
    tunnels: TunnelMaps,
    workers: WorkerHandles,
    pt_tx: Sender<Ptn>,
) {
    let TcMainInputs {
        from_pt_rx,
        from_quic_rx,
        stream_state_rx,
        to_quic_tx,
        stream_state_tx,
    } = inputs;
    let TunnelMaps {
        server_bound: server_bound_tunnels,
        client_bound: client_bound_tunnels,
    } = tunnels;
    let WorkerHandles {
        accept_wakers,
        mut threads,
    } = workers;
    let mut streams = BTreeMap::new();

    thread::spawn(move || {
        let _s = debug_span!("tc-main").entered();
        debug!("started");
        loop {
            select_biased! {
                recv(from_pt_rx) -> pt_msg_res => {
                    if handle_primary_notification(pt_msg_res).is_break() {
                        break;
                    }
                }
                recv(from_quic_rx) -> quic_thread_msg_res => {
                    let cf = handle_quic_message(
                        quic_thread_msg_res,
                        &to_quic_tx,
                        &stream_state_tx,
                        &server_bound_tunnels,
                        &mut streams,
                        &pt_tx,
                    );
                    match cf {
                        ControlFlow::Break(()) => break,
                        ControlFlow::Continue(Some((name, handle))) => {
                            threads.insert(name, handle);
                        }
                        ControlFlow::Continue(None) => continue,
                    }
                }
                recv(stream_state_rx) -> stream_state_msg_res => {
                    if handle_stream_state_message(
                        stream_state_msg_res,
                        &to_quic_tx,
                        &stream_state_tx,
                        &client_bound_tunnels,
                        &mut streams,
                    ).is_break() {
                        break;
                    }
                },
            }
        }
        // Stop accepting new messages so zero-capacity senders fail fast instead of blocking.
        drop(stream_state_rx);
        drop(from_quic_rx);
        debug!("terminating stream threads");
        for (id, state) in streams {
            match state {
                State::Open {
                    io, inbound_open, ..
                }
                | State::Draining {
                    io, inbound_open, ..
                } => {
                    drain_stream(id, io, inbound_open, &to_quic_tx);
                }
                State::PendingLocalConnect { .. } => {
                    to_quic_tx.send_or_warn(close_tcp_stream(id));
                }
            }
        }
        debug!("terminating accept threads");
        for (tunnel_id, waker) in accept_wakers.iter() {
            if let Err(error) = waker.wake() {
                warn!(
                    tunnel_id,
                    %error,
                    "failed to wake accept thread"
                );
            }
        }
        // Drop our sender so tc-quic-tx unblocks on recv() and can exit cleanly.
        // At this point all shutdown messages have been emitted.
        drop(to_quic_tx);
        for (thread_name, jh) in threads {
            trace!(thread_name, "waiting");
            if let Err(error) = jh.join() {
                warn!("{thread_name} panicked: {error:?}");
            }
        }
        debug!("exiting");
    });
}

fn open_tcp_stream(id: impl Into<Id>) -> proto::Message {
    let id = id.into();
    proto::MessageBuilder::default()
        .tunnel_id(id.tunnel)
        .stream_id(id.stream)
        .message(proto::OpenTcpStream::new())
        .build()
        .expect("build tunnel message")
}

fn close_tcp_stream(id: impl Into<Id>) -> proto::Message {
    let id = id.into();
    proto::MessageBuilder::default()
        .tunnel_id(id.tunnel)
        .stream_id(id.stream)
        .message(proto::CloseTcpStream::new())
        .build()
        .expect("build tunnel message")
}

fn tcp_data(id: impl Into<Id>, data: Vec<u8>) -> proto::Message {
    let id = id.into();
    proto::MessageBuilder::default()
        .tunnel_id(id.tunnel)
        .stream_id(id.stream)
        .message(proto::TcpData::new(data))
        .build()
        .expect("build tunnel message")
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Copy, derive_new::new)]
struct Id {
    tunnel: u64,
    stream: u64,
}

fn handle_open_from_server(
    id: Id,
    to_quic_tx: &Sender<proto::Message>,
    stream_state_tx: &Sender<StreamState>,
    server_bound_tunnels: &BTreeMap<u64, proto::Description>,
    streams: &mut BTreeMap<Id, State>,
) -> Option<JoinHandle<()>> {
    let desc = if let Some(desc) = server_bound_tunnels.get(&id.tunnel) {
        desc
    } else {
        warn!(?id, "open tcp for non-server-bound tunnel");
        to_quic_tx.send_or_warn(close_tcp_stream(id));
        return None;
    };
    if desc.side() == proto::Side::Client {
        warn!(?id, "open tcp for client-bound tunnel");
        to_quic_tx.send_or_warn(close_tcp_stream(id));
        return None;
    }
    if streams.contains_key(&id) {
        warn!(?id, "open for existing tunnel/stream");
        to_quic_tx.send_or_warn(close_tcp_stream(id));
        return None;
    }

    streams.insert(id, State::PendingLocalConnect { buffered: vec![] });

    let to_quic_tx = to_quic_tx.clone();
    let stream_state_tx = stream_state_tx.clone();
    let (forward_addr, forward_port) = (desc.forward_address.clone(), desc.forward_port as u16);
    Some(thread::spawn(move || {
        let _s = debug_span!("tc-conn", ?id).entered();
        let addrs = match (forward_addr.as_str(), forward_port).to_socket_addrs() {
            Ok(addrs) => addrs.collect::<Vec<_>>(),
            Err(error) => {
                warn!(%error, "failed to resolve address");
                Vec::new()
            }
        };

        let mut last_err: Option<io::Error> = None;
        let stream = addrs.into_iter().find_map(|addr| {
            match StdTcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
                Ok(stream) => Some(stream),
                Err(error) => {
                    last_err = Some(error);
                    None
                }
            }
        });

        let Some(stream) = stream else {
            if let Some(error) = last_err {
                warn!(%error, "failed to connect to {forward_addr}:{forward_port}");
            } else {
                warn!("failed to make connection to {forward_addr}:{forward_port}");
            }
            to_quic_tx.send_or_warn(close_tcp_stream(id));
            return;
        };

        if let Err(error) = stream.set_nonblocking(true) {
            warn!(%error, "failed to set stream nonblocking for {forward_addr}:{forward_port}");
            to_quic_tx.send_or_warn(close_tcp_stream(id));
            return;
        }

        // Use a bounded wait so the connector can't hang forever if tc-main is alive but not
        // draining `stream_state_rx` (e.g. stuck mid-shutdown). After the timeout we proactively
        // close the QUIC stream to avoid leaving the server side half-open.
        match stream_state_tx.send_timeout(
            StreamState::Started {
                id,
                stream: TcpStream::from_std(stream),
            },
            CONNECTOR_NOTIFY_TIMEOUT,
        ) {
            Ok(_) => {}
            Err(SendTimeoutError::Timeout(_)) => {
                warn!("timed out sending StreamState::Started for {forward_addr}:{forward_port}");
                to_quic_tx.send_or_warn(close_tcp_stream(id));
            }
            Err(SendTimeoutError::Disconnected(_)) => {
                debug!("tc-main dropped before connector reported success");
            }
        }
    }))
}

fn handle_close_from_server(id: Id, streams: &mut BTreeMap<Id, State>) {
    let _s = trace_span!("close-from-server", ?id).entered();
    match streams.remove(&id) {
        Some(State::PendingLocalConnect { .. }) => {
            trace!("remote closed before local connect completed");
            // nothing to flush; entry removed
        }
        Some(State::Open {
            io, outbound_open, ..
        }) => {
            trace!("remote closed; moving to draining");
            streams.insert(
                id,
                State::Draining {
                    io,
                    inbound_open: false,
                    outbound_open,
                },
            );
        }
        Some(State::Draining {
            io, outbound_open, ..
        }) => {
            trace!("already draining; marking inbound closed");
            streams.insert(
                id,
                State::Draining {
                    io,
                    inbound_open: false,
                    outbound_open,
                },
            );
        }
        None => warn!(
            ?id,
            "Close from server for tunnel/stream which does not exist"
        ),
    }
}

fn handle_data_from_server(id: Id, data: Vec<u8>, streams: &mut BTreeMap<Id, State>) {
    let _s = trace_span!("data-from-server", ?id).entered();
    trace!(?data);
    match streams.get_mut(&id) {
        Some(State::Open {
            io, inbound_open, ..
        }) => {
            if !*inbound_open {
                warn!("received data after inbound closed");
                return;
            }
            io.to_tx.send_or_warn(data);
            io.wake.notify_data();
        }
        Some(State::Draining { inbound_open, .. }) => {
            warn!(?inbound_open, "dropping data while draining");
        }
        Some(State::PendingLocalConnect { buffered }) => {
            trace!("buffering during connect");
            buffered.extend(data);
        }
        None => warn!(?id, "non-existent tunnel/stream"),
    }
}

fn handle_new_stream_established(
    id: Id,
    stream: TcpStream,
    to_quic_tx: &Sender<proto::Message>,
    stream_state_tx: &Sender<StreamState>,
    client_bound_tunnels: &BTreeMap<u64, proto::Description>,
    streams: &mut BTreeMap<Id, State>,
) {
    let _s = trace_span!("new-stream-established", ?id).entered();
    trace!(?stream);

    let queued = match streams.remove(&id) {
        Some(State::PendingLocalConnect { buffered }) => buffered,
        Some(State::Open { .. }) | Some(State::Draining { .. }) => {
            warn!("already open, ignoring");
            return;
        }
        None => {
            trace!("sending open to server");
            if client_bound_tunnels.contains_key(&id.tunnel) {
                to_quic_tx.send_or_warn(open_tcp_stream(id));
                trace!("sent");
                vec![]
            } else {
                warn!("non-existent");
                return;
            }
        }
    };

    let (to_tx, to_tx_rx) = bounded::<Vec<u8>>(IO_WRITE_CHAN_CAP);
    // Poll + waker are created outside the thread so we can share the handle for wakeups.
    let socket_token = TOKEN_SOCKET;
    let waker_token = TOKEN_WAKER;

    let mut poll = match Poll::new() {
        Ok(p) => p,
        Err(e) => {
            warn!(?e, "failed to create poll");
            stream_state_tx.send_or_warn(StreamState::Finished {
                id,
                result: Err(e.into()),
            });
            return;
        }
    };

    let waker = match Waker::new(poll.registry(), waker_token) {
        Ok(w) => Arc::new(w),
        Err(e) => {
            warn!(?e, "failed to create waker");
            stream_state_tx.send_or_warn(StreamState::Finished {
                id,
                result: Err(e.into()),
            });
            return;
        }
    };

    let wake_handle = io_waker::WakeHandle::new_with_waker(waker.clone());
    let stream_state_tx_for_thread = stream_state_tx.clone();
    let to_quic_tx = to_quic_tx.clone();
    let wake_for_thread = wake_handle.clone();
    let wake_for_map = wake_handle.clone();

    let io_thread = thread::spawn(move || {
        let _s = debug_span!("tc-stream-io-thread", ?id).entered();
        trace!("starting");

        let mut events = Events::with_capacity(POLL_EVENT_CAPACITY);
        let mut stream = stream;
        if let Err(e) = poll
            .registry()
            .register(&mut stream, socket_token, Interest::READABLE)
        {
            warn!(?e, "failed to register stream with poll");
            stream_state_tx_for_thread.send_or_warn(StreamState::Finished {
                id,
                result: Err(e.into()),
            });
            return;
        }

        let mut write_queue: VecDeque<Vec<u8>> = VecDeque::new();
        if !queued.is_empty() {
            write_queue.push_back(queued);
        }

        let mut buf = [0u8; 0x1000];
        let mut current_interest = Interest::READABLE;
        let mut outbound_closed = false;
        let mut cancel_requested = false;

        let result = 'io_event_loop: loop {
            // pick up newly queued writes from the channel
            loop {
                match to_tx_rx.try_recv() {
                    Ok(data) => write_queue.push_back(data),
                    Err(crossbeam::channel::TryRecvError::Empty) => break,
                    Err(crossbeam::channel::TryRecvError::Disconnected) => {
                        outbound_closed = true;
                        break;
                    }
                }
            }
            trace!(?write_queue);

            if outbound_closed && write_queue.is_empty() {
                if let Err(error) = stream.shutdown(std::net::Shutdown::Both) {
                    warn!(%error, "failed to shutdown stream after outbound closed");
                }
                break 'io_event_loop Ok(());
            }

            if cancel_requested && write_queue.is_empty() {
                if let Err(error) = stream.shutdown(std::net::Shutdown::Both) {
                    warn!(%error, "failed to shutdown stream after cancellation");
                }
                break 'io_event_loop Ok(());
            }

            let desired_interest = if cancel_requested {
                Interest::WRITABLE
            } else if write_queue.is_empty() {
                Interest::READABLE
            } else {
                Interest::READABLE | Interest::WRITABLE
            };
            trace!(?desired_interest);

            if desired_interest != current_interest {
                if let Err(e) =
                    poll.registry()
                        .reregister(&mut stream, socket_token, desired_interest)
                {
                    break 'io_event_loop Err(e.into());
                }
                current_interest = desired_interest;
            }

            trace!("waiting for events");
            if let Err(e) = poll.poll(&mut events, None) {
                break Err(e.into());
            }

            // Handle wake signals before processing socket events so cancellation wins over reads.
            let flags = wake_for_thread.take();
            if flags.cancel() {
                cancel_requested = true;
                if write_queue.is_empty() {
                    if let Err(error) = stream.shutdown(std::net::Shutdown::Both) {
                        warn!(%error, "failed to shutdown stream after cancel signal");
                    }
                    break 'io_event_loop Ok(());
                }
            }
            // If data was signaled, we'll pick it up by draining the channel at the top of the loop
            // on the next iteration; no need to skip already-collected events.

            'event_iter: for event in events.iter() {
                trace!(?event);
                let token = event.token();
                if token == socket_token {
                    if event.is_readable() && !cancel_requested {
                        'read_loop: loop {
                            match stream.read(&mut buf) {
                                Ok(0) => {
                                    trace!("read done");
                                    if let Err(error) = stream.shutdown(std::net::Shutdown::Both) {
                                        warn!(%error, "failed to shutdown stream after EOF");
                                    }
                                    break 'io_event_loop Ok(());
                                }
                                Ok(n) => {
                                    let mut data = Vec::with_capacity(n);
                                    data.extend_from_slice(&buf[..n]);
                                    trace!(?data, "received");
                                    to_quic_tx.send_or_warn(tcp_data(id, data));
                                }
                                Err(e)
                                    if matches!(
                                        e.kind(),
                                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                                    ) =>
                                {
                                    trace!("would-block/interrupted");
                                    break 'read_loop;
                                }
                                Err(e) => break 'io_event_loop Err(e.into()),
                            }
                        }
                    }

                    if event.is_writable() && !write_queue.is_empty() {
                        'write_loop: while let Some(front) = write_queue.front_mut() {
                            match stream.write(front) {
                                Ok(0) => {
                                    break 'io_event_loop Err(io::Error::new(
                                        io::ErrorKind::WriteZero,
                                        "tcp write returned 0 bytes",
                                    )
                                    .into());
                                }
                                Ok(n) if n == front.len() => {
                                    write_queue.pop_front();
                                }
                                Ok(n) => {
                                    front.drain(0..n);
                                }
                                Err(e)
                                    if matches!(
                                        e.kind(),
                                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                                    ) =>
                                {
                                    break 'write_loop;
                                }
                                Err(e) => break 'io_event_loop Err(e.into()),
                            }
                        }
                    }
                } else if token == waker_token {
                    // Wake flags already handled above; nothing to do.
                    continue 'event_iter;
                }
            }
        };

        stream_state_tx_for_thread.send_or_warn(StreamState::Finished { id, result });
        trace!("exiting");
    });

    streams.insert(
        id,
        State::Open {
            io: IoHandles {
                to_tx,
                wake: wake_for_map,
                io_thread,
            },
            inbound_open: true,
            outbound_open: true,
        },
    );
}

fn handle_stream_result(
    id: Id,
    result: Result<(), StreamError>,
    to_quic_tx: &Sender<proto::Message>,
    streams: &mut BTreeMap<Id, State>,
) {
    let _s = trace_span!("stream-thread-terminated", ?id).entered();
    trace!(?result);
    match streams.remove(&id) {
        Some(State::PendingLocalConnect { buffered }) => {
            warn!(?buffered, "stream ended before connect finished")
        }
        Some(State::Open { inbound_open, .. }) | Some(State::Draining { inbound_open, .. }) => {
            trace!("closing stream");
            if inbound_open {
                to_quic_tx.send_or_warn(close_tcp_stream(id));
            }
        }
        None => warn!("non-existent"),
    }
}

fn drain_stream(id: Id, io: IoHandles, inbound_open: bool, to_quic_tx: &Sender<proto::Message>) {
    io.wake.notify_cancel();
    if let Err(error) = io.io_thread.join() {
        warn!(?id, "io thread panicked: {error:?}");
    }
    if inbound_open {
        to_quic_tx.send_or_warn(close_tcp_stream(id));
    }
}

/// Spin up the worker threads that shuttle data for all allocated tunnels.
///
/// Sets up TCP listeners for client-bound tunnels, the QUIC Rx/Tx loops, and the coordinator
/// that translates tunnel open/close/data messages while managing stream lifecycles and shutdown.
/// Errors encountered by any worker are forwarded to the primary thread as
/// `PrimaryThreadNotification::ChannelError`.
fn initialize_threads(
    tc: channel::ClientTunnel,
    pt_tx: Sender<Ptn>,
    from_pt_rx: Receiver<Notification>,
    server_bound_tunnels: BTreeMap<u64, proto::Description>,
    client_bound_tunnels: BTreeMap<u64, proto::Description>,
) {
    let (tc_rx, tc_tx) = tc.split();

    let (from_quic_tx, from_quic_rx) = bounded(0);
    let (to_quic_tx, to_quic_rx) = bounded(0);
    let (stream_state_tx, stream_state_rx) = bounded(0);

    let (accept_wakers, accept_threads) =
        spawn_accept_threads(&client_bound_tunnels, &stream_state_tx, &pt_tx);
    let quic_threads = spawn_quic_threads(tc_rx, tc_tx, to_quic_rx, from_quic_tx, pt_tx.clone());
    let mut threads = accept_threads;
    threads.extend(quic_threads);

    let inputs = TcMainInputs {
        from_pt_rx,
        from_quic_rx,
        stream_state_rx,
        to_quic_tx,
        stream_state_tx,
    };
    let tunnels = TunnelMaps {
        server_bound: server_bound_tunnels,
        client_bound: client_bound_tunnels,
    };
    let workers = WorkerHandles {
        accept_wakers,
        threads,
    };

    spawn_tc_main(inputs, tunnels, workers, pt_tx);
}

/// Entry point for tunnel handling. Waits for server allocation responses, then starts the
/// tunnel workers via `initialize_threads`.
///
/// # Parameters
/// * `requested_tunnels` - Desired tunnel descriptions keyed by tunnel ID.
/// * `tc` - Tunnel channel handle split into Rx/Tx halves once allocations arrive.
/// * `pt_tx` - For sending `PrimaryThreadNotification::ChannelError` to the primary thread.
/// * `from_pt_rx` - Receives allocation maps or termination requests from the primary thread.
pub fn setup(
    requested_tunnels: BTreeMap<u64, proto::Description>,
    tc: channel::ClientTunnel,
    pt_tx: Sender<Ptn>,
    from_pt_rx: Receiver<Notification>,
) {
    thread::spawn(move || {
        let _s = debug_span!("tc-setup").entered();
        let Ok(first_notif) = from_pt_rx.recv() else {
            warn!("no tunnel notifications");
            return;
        };
        let allocated_tunnels = match first_notif {
            Notification::Terminate => return,
            Notification::Allocations(allocated_tunnels) => allocated_tunnels,
        };

        let mut server_bound_tunnels = BTreeMap::new();
        let mut client_bound_tunnels = BTreeMap::new();
        for tunnel_id in requested_tunnels.keys() {
            let Some(response) = allocated_tunnels.get(tunnel_id) else {
                warn!(tunnel_id, "no response for tunnel request");
                continue;
            };
            let Some(response_message) = response.message.as_ref() else {
                warn!(tunnel_id, "empty response message");
                continue;
            };

            match response_message {
                proto::response::Message::Success(desc) => {
                    let Ok(this_side) = proto::Side::try_from(desc.side) else {
                        warn!("failed to determine tunnel side");
                        continue;
                    };
                    let other_side = if this_side == proto::Side::Client {
                        proto::Side::Server
                    } else {
                        proto::Side::Client
                    };
                    info!(
                        "allocated {:?} port {}:{} for forward to {}:{} on {:?}",
                        this_side,
                        desc.bind_address,
                        desc.bind_port,
                        desc.forward_address,
                        desc.forward_port,
                        other_side,
                    );

                    match this_side {
                        proto::Side::Server => {
                            server_bound_tunnels.insert(*tunnel_id, desc.clone())
                        }
                        proto::Side::Client => {
                            client_bound_tunnels.insert(*tunnel_id, desc.clone())
                        }
                    };
                }
                proto::response::Message::Failure(reason) => {
                    if let Some(desc) = requested_tunnels.get(tunnel_id) {
                        warn!(
                            "server failed binding: {}:{}: '{reason}'",
                            desc.bind_address, desc.bind_port,
                        );
                    } else {
                        warn!(?tunnel_id, "server failed binding for unknown tunnel ID");
                    }
                }
            }
        }

        debug!(?server_bound_tunnels);
        debug!(?client_bound_tunnels);

        initialize_threads(
            tc,
            pt_tx,
            from_pt_rx,
            server_bound_tunnels,
            client_bound_tunnels,
        )
    });
}

fn handle_primary_notification(
    pt_msg_res: Result<Notification, crossbeam::channel::RecvError>,
) -> ControlFlow<()> {
    let Ok(pt_msg) = pt_msg_res else {
        warn!("primary died before tc-main");
        return ControlFlow::Break(());
    };
    trace!(?pt_msg, "received message from primary");
    match pt_msg {
        Notification::Allocations(_) => {
            warn!("received gratuitous tunnel allocations, ignoring");
            ControlFlow::Continue(())
        }
        Notification::Terminate => ControlFlow::Break(()),
    }
}

fn handle_quic_message(
    quic_thread_msg_res: Result<QuicRxResult, crossbeam::channel::RecvError>,
    to_quic_tx: &Sender<proto::Message>,
    stream_state_tx: &Sender<StreamState>,
    server_bound_tunnels: &BTreeMap<u64, proto::Description>,
    streams: &mut BTreeMap<Id, State>,
    pt_tx: &Sender<Ptn>,
) -> ControlFlow<(), Option<(String, JoinHandle<()>)>> {
    let Ok(quic_res) = quic_thread_msg_res else {
        debug!("tc-quic-rx died before tc-main");
        return ControlFlow::Break(());
    };
    let s2c = match quic_res {
        Ok(msg) => msg,
        Err(error) => {
            pt_tx.send_or_warn(Ptn::ChannelError(error.into()));
            return ControlFlow::Continue(None);
        }
    };
    let id = Id::new(s2c.tunnel_id, s2c.stream_id);
    match s2c.message {
        Some(proto::message::Message::OpenTcp(_)) => {
            if let Some(conn_handle) = handle_open_from_server(
                id,
                to_quic_tx,
                stream_state_tx,
                server_bound_tunnels,
                streams,
            ) {
                let name = format!("tc-conn-{}-{}", id.tunnel, id.stream);
                return ControlFlow::Continue(Some((name, conn_handle)));
            }
        }
        Some(proto::message::Message::CloseTcp(_)) => {
            handle_close_from_server(id, streams);
        }
        Some(proto::message::Message::TcpData(tcp_data)) => {
            handle_data_from_server(id, tcp_data.data, streams);
        }
        None => warn!("empty bodied tunnel message"),
    }
    ControlFlow::Continue(None)
}

fn handle_stream_state_message(
    stream_state_msg_res: Result<StreamState, crossbeam::channel::RecvError>,
    to_quic_tx: &Sender<proto::Message>,
    stream_state_tx: &Sender<StreamState>,
    client_bound_tunnels: &BTreeMap<u64, proto::Description>,
    streams: &mut BTreeMap<Id, State>,
) -> ControlFlow<()> {
    let Ok(stream_state_msg) = stream_state_msg_res else {
        warn!("all stream_state message emitting threads died before tc-main");
        return ControlFlow::Break(());
    };

    match stream_state_msg {
        StreamState::Started { id, stream } => {
            handle_new_stream_established(
                id,
                stream,
                to_quic_tx,
                stream_state_tx,
                client_bound_tunnels,
                streams,
            );
        }
        StreamState::Finished { id, result } => {
            handle_stream_result(id, result, to_quic_tx, streams)
        }
    }
    ControlFlow::Continue(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_io_handles() -> (IoHandles, Receiver<Vec<u8>>) {
        let (to_tx, to_rx) = bounded::<Vec<u8>>(IO_WRITE_CHAN_CAP);
        let poll = Box::leak(Box::new(Poll::new().expect("poll")));
        // Leak the poll so the registry outlives the waker; tests are process-bound so this is safe.
        let waker = Arc::new(Waker::new(poll.registry(), TOKEN_WAKER).expect("waker"));
        let wake = io_waker::WakeHandle::new_with_waker(waker);
        let io_thread = thread::spawn(|| {});
        (
            IoHandles {
                to_tx,
                wake,
                io_thread,
            },
            to_rx,
        )
    }

    #[test]
    fn close_moves_open_to_draining_and_marks_inbound_closed() {
        let mut streams = BTreeMap::new();
        let id = Id::new(1, 1);
        let (io, _) = test_io_handles();
        streams.insert(
            id,
            State::Open {
                io,
                inbound_open: true,
                outbound_open: true,
            },
        );

        handle_close_from_server(id, &mut streams);

        match streams.get(&id) {
            Some(State::Draining {
                inbound_open,
                outbound_open,
                ..
            }) => {
                assert!(!*inbound_open);
                assert!(*outbound_open);
            }
            _ => panic!("expected Draining state"),
        }
    }

    #[test]
    fn close_updates_existing_draining_state() {
        let mut streams = BTreeMap::new();
        let id = Id::new(2, 1);
        let (io, _) = test_io_handles();
        streams.insert(
            id,
            State::Draining {
                io,
                inbound_open: true,
                outbound_open: false,
            },
        );

        handle_close_from_server(id, &mut streams);

        match streams.get(&id) {
            Some(State::Draining {
                inbound_open,
                outbound_open,
                ..
            }) => {
                assert!(!*inbound_open);
                assert!(!*outbound_open);
            }
            _ => panic!("expected Draining state"),
        }
    }

    #[test]
    fn close_removes_pending_connect() {
        let mut streams = BTreeMap::new();
        let id = Id::new(3, 1);
        streams.insert(id, State::PendingLocalConnect { buffered: vec![] });

        handle_close_from_server(id, &mut streams);

        assert!(!streams.contains_key(&id));
    }

    #[test]
    fn data_buffers_while_pending_connect() {
        let mut streams = BTreeMap::new();
        let id = Id::new(4, 1);
        streams.insert(
            id,
            State::PendingLocalConnect {
                buffered: vec![1, 2],
            },
        );
        handle_data_from_server(id, vec![3, 4], &mut streams);

        match streams.get(&id) {
            Some(State::PendingLocalConnect { buffered }) => {
                assert_eq!(buffered, &vec![1, 2, 3, 4]);
            }
            _ => panic!("expected PendingLocalConnect"),
        }
    }

    #[test]
    fn data_forwards_when_open() {
        let mut streams = BTreeMap::new();
        let id = Id::new(5, 1);
        let (io, to_rx) = test_io_handles();
        streams.insert(
            id,
            State::Open {
                io,
                inbound_open: true,
                outbound_open: true,
            },
        );

        handle_data_from_server(id, vec![9, 8, 7], &mut streams);

        let forwarded = to_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("data forwarded");
        assert_eq!(forwarded, vec![9, 8, 7]);
    }
}
