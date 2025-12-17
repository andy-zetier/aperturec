use aperturec_channel::{self as channel, Receiver as _, Sender as _};
use aperturec_protocol::tunnel as proto;
use aperturec_utils::{channels::SenderExt, info_always};

use crossbeam::channel::{Receiver, Sender, bounded, select_biased};
use mio::{
    Events, Interest, Poll, Token, Waker,
    net::{TcpListener, TcpStream},
};
use std::{
    collections::BTreeMap,
    io::{self, Read, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream as StdTcpStream, ToSocketAddrs},
    sync::Arc,
    thread::{self, JoinHandle},
};
use tracing::*;

/// Lifecycle of a TCP stream that backs a logical tunnel stream.
///
/// Streams always start in `Opening`, move to `Opened` once the TCP socket is fully established,
/// and then advance through `HalfClosed` and `FullyClosed` as either side tears down the socket.
enum State {
    /// The tunnel stream exists at the QUIC layer but the TCP socket is not ready yet.
    ///
    /// Bytes arriving from the server are buffered until the socket is available so that
    /// early data is not dropped.
    Opening {
        /// Bytes delivered by the remote side before the TCP socket was established locally.
        queued: Vec<u8>,
    },
    /// Stream is fully established and actively forwarding bytes between TCP and the tunnel.
    Opened {
        /// Channel feeding the write thread with data destined for the TCP socket.
        to_tx: Sender<Vec<u8>>,
        /// Handle used to forcefully wake/blocking read during shutdown.
        stream: Arc<TcpStream>,
        read_thread: JoinHandle<()>,
        write_thread: JoinHandle<()>,
    },
    /// One half of the duplex stream has shut down; the other may still be draining.
    HalfClosed,
    /// Both halves have completed and the owning entry can be cleaned up.
    FullyClosed,
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
}

#[derive(Debug, thiserror::Error)]
enum StreamError {
    #[error(transparent)]
    IO(#[from] io::Error),
}

#[derive(Debug)]
enum StreamState {
    Started {
        id: Id,
        stream: Arc<TcpStream>,
    },
    Finished {
        id: Id,
        result: Result<(), StreamError>,
    },
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
) {
    let _s = trace_span!("open-from-server", ?id).entered();
    let desc = if let Some(desc) = server_bound_tunnels.get(&id.tunnel) {
        desc
    } else {
        warn!(?id, "open tcp for non-server-bound tunnel");
        to_quic_tx.send_or_warn(close_tcp_stream(id));
        return;
    };
    if desc.side() == proto::Side::Client {
        warn!(?id, "open tcp for client-bound tunnel");
        to_quic_tx.send_or_warn(close_tcp_stream(id));
        return;
    }
    if streams.contains_key(&id) {
        warn!(?id, "open for existing tunnel/stream");
        to_quic_tx.send_or_warn(close_tcp_stream(id));
        return;
    }

    streams.insert(id, State::Opening { queued: vec![] });

    let to_quic_tx = to_quic_tx.clone();
    let stream_state_tx = stream_state_tx.clone();
    let (forward_addr, forward_port) = (desc.forward_address.clone(), desc.forward_port as u16);
    thread::spawn(move || {
        let mut stream_iter = match (forward_addr.as_str(), forward_port).to_socket_addrs() {
            Ok(addrs) => addrs,
            Err(error) => {
                warn!(%error, "failed to resolve address");
                Vec::default().into_iter()
            }
        }
        .map(StdTcpStream::connect)
        .filter_map(Result::ok);

        let Some(stream) = stream_iter.next() else {
            warn!("failed to make connection to {forward_addr}:{forward_port}");
            to_quic_tx.send_or_warn(close_tcp_stream(id));
            return;
        };
        stream_state_tx.send_or_warn(StreamState::Started {
            id,
            stream: Arc::new(TcpStream::from_std(stream)),
        });
    });
}

fn handle_close_from_server(id: Id, streams: &mut BTreeMap<Id, State>) {
    let _s = trace_span!("close-from-server", ?id).entered();
    match streams.remove(&id) {
        Some(State::Opening { .. }) | Some(State::Opened { .. }) => {
            trace!("marking half closed");
            streams.insert(id, State::HalfClosed);
        }
        Some(State::HalfClosed) => {
            trace!("marking fully closed");
            streams.insert(id, State::FullyClosed);
        }
        Some(State::FullyClosed) => (),
        None => warn!(
            ?id,
            "Close from server for tunnel/stream which does not exist"
        ),
    }
}

fn handle_data_from_server(
    id: Id,
    data: Vec<u8>,
    to_quic_tx: &Sender<proto::Message>,
    streams: &mut BTreeMap<Id, State>,
) {
    let _s = trace_span!("data-from-server", ?id).entered();
    trace!(?data);
    match streams.get_mut(&id) {
        Some(State::Opened { to_tx, .. }) => {
            trace!("opened");
            to_tx.send_or_warn(data);
        }
        Some(State::Opening { queued, .. }) => {
            trace!("opening");
            queued.extend(data);
        }
        Some(State::HalfClosed) => warn!("half closed"),
        Some(State::FullyClosed) => to_quic_tx.send_or_warn(close_tcp_stream(id)),
        None => warn!(?id, "non-existent tunnel/stream"),
    }
}

fn handle_new_stream_established(
    id: Id,
    stream: Arc<TcpStream>,
    to_quic_tx: &Sender<proto::Message>,
    stream_state_tx: &Sender<StreamState>,
    client_bound_tunnels: &BTreeMap<u64, proto::Description>,
    streams: &mut BTreeMap<Id, State>,
) {
    let _s = trace_span!("new-stream-established", ?id).entered();
    trace!(?stream);

    let queued = match streams.remove(&id) {
        Some(State::Opening { queued }) => queued,
        Some(State::Opened { .. }) => {
            warn!("already open, ignoring");
            return;
        }
        Some(State::HalfClosed) => {
            warn!("half closed");
            return;
        }
        Some(State::FullyClosed) => {
            warn!("fully closed");
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

    let stream_state_tx_rh = stream_state_tx.clone();
    let to_quic_tx = to_quic_tx.clone();
    let rh = stream.clone();
    let read_thread = thread::spawn(move || {
        let _s = trace_span!("tc-stream-read-thread", ?id).entered();
        trace!("starting");
        let result = loop {
            let mut data = vec![0_u8; 0x1000];
            let nbytes = match rh.as_ref().read(&mut data) {
                Ok(nbytes_read) => nbytes_read,
                Err(e) => break Err(e.into()),
            };
            if nbytes == 0 {
                break Ok(());
            } else {
                data.truncate(nbytes);
                to_quic_tx.send_or_warn(tcp_data(id, data));
            }
        };
        stream_state_tx_rh.send_or_warn(StreamState::Finished { id, result });
        trace!("exiting");
    });

    let (to_tx, to_tx_rx) = bounded::<Vec<u8>>(1);
    let stream_state_tx_wh = stream_state_tx.clone();
    let wh = stream.clone();
    let write_thread = thread::spawn(move || {
        let _s = trace_span!("tc-stream-write-thread", ?id).entered();
        trace!("starting");
        if let Err(e) = wh.as_ref().write_all(&queued) {
            stream_state_tx_wh.send_or_warn(StreamState::Finished {
                id,
                result: Err(e.into()),
            });
            return;
        }
        let result = loop {
            let Ok(data) = to_tx_rx.recv() else {
                break Ok(());
            };
            if let Err(e) = wh.as_ref().write_all(&data) {
                break Err(e.into());
            }
        };
        stream_state_tx_wh.send_or_warn(StreamState::Finished { id, result });
        trace!("exiting");
    });

    streams.insert(
        id,
        State::Opened {
            to_tx,
            stream,
            read_thread,
            write_thread,
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
        Some(State::Opening { queued, .. }) => warn!(?queued, "closing opening tunnel"),
        Some(State::Opened { .. }) => {
            trace!("closing first half");
            streams.insert(id, State::HalfClosed);
        }
        Some(State::HalfClosed) => {
            trace!("closing second half");
            streams.insert(id, State::FullyClosed);
        }
        Some(State::FullyClosed) => {
            to_quic_tx.send_or_warn(close_tcp_stream(id));
        }
        None => warn!("non-existent"),
    }
}

/// Spin up the worker threads that shuttle data for all allocated tunnels.
///
/// This sets up TCP listeners for client-bound tunnels, connector threads for server-bound
/// tunnels, the QUIC Rx/Tx loops, and the coordinator that translates tunnel open/close/data
/// messages. Errors encountered by any worker are forwarded to the primary thread as
/// `PrimaryThreadNotification::ChannelError`.
fn initialize_threads(
    tc: channel::ClientTunnel,
    pt_tx: Sender<Ptn>,
    from_pt_rx: Receiver<Notification>,
    server_bound_tunnels: BTreeMap<u64, proto::Description>,
    client_bound_tunnels: BTreeMap<u64, proto::Description>,
) {
    let (mut tc_rx, mut tc_tx) = tc.split();

    let (from_quic_tx, from_quic_rx) = bounded(0);
    let (to_quic_tx, to_quic_rx) = bounded(0);
    let (stream_state_tx, stream_state_rx) = bounded(0);

    let mut threads = BTreeMap::new();
    let mut accept_wakers: BTreeMap<u64, Waker> = BTreeMap::new();

    for (&tunnel_id, desc) in &client_bound_tunnels {
        let desc = desc.clone();
        debug!(?desc);
        let stream_state_tx = stream_state_tx.clone();
        let (waker_tx, waker_rx) = bounded(1);

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
                        return;
                    }
                }
            };
            let Ok(port) = u16::try_from(desc.bind_port) else {
                warn!("failed converting {} to 16-bit port", desc.bind_port);
                return;
            };

            let mut listener = match TcpListener::bind(SocketAddr::new(addr, port)) {
                Ok(listener) => listener,
                Err(error) => {
                    warn!(%error, "failed binding {addr}:{port}");
                    return;
                }
            };

            // Register listener with mio and create a waker we can trigger on shutdown.
            let mut poll = match Poll::new() {
                Ok(p) => p,
                Err(error) => {
                    warn!(%error, "failed to create poll");
                    return;
                }
            };
            let listener_token = Token(0);
            let waker_token = Token(1);

            // Register listener with mio and create a waker we can trigger on shutdown.
            if let Err(error) =
                poll.registry()
                    .register(&mut listener, listener_token, Interest::READABLE)
            {
                warn!(%error, "failed to register listener with poll");
                return;
            }
            let waker = match Waker::new(poll.registry(), waker_token) {
                Ok(w) => w,
                Err(error) => {
                    warn!(%error, "failed to create waker");
                    return;
                }
            };
            let _ = waker_tx.send(waker);
            let mut events = Events::with_capacity(16);

            loop {
                if let Err(error) = poll.poll(&mut events, None) {
                    warn!(%error, "poll failed");
                    break;
                }

                for event in events.iter() {
                    let token = event.token();
                    if token == listener_token {
                        match listener.accept() {
                            Ok((stream, _)) => {
                                debug!(stream_id, "accepted");
                                let stream_state = StreamState::Started {
                                    id: Id::new(tunnel_id, stream_id),
                                    stream: Arc::new(stream),
                                };
                                if stream_state_tx.send(stream_state).is_err() {
                                    debug!("tc-main is gone, exiting");
                                    break;
                                };
                                debug!(stream_id, "forwarded to new stream handler");
                                stream_id += 1;
                            }
                            Err(error) => {
                                warn!(%error, stream_id, "failed accepting");
                                break;
                            }
                        }
                    } else if token == waker_token {
                        debug!("waker signaled, exiting accept loop");
                        return;
                    }
                }
            }
        });

        threads.insert(format!("tc-accept-{tunnel_id}"), accept_thread);
        if let Ok(waker) = waker_rx.recv() {
            accept_wakers.insert(tunnel_id, waker);
        }
    }

    let pt_tx_qtt = pt_tx.clone();
    let quic_tx_thread = thread::spawn(move || {
        let _s = debug_span!("tc-quic-tx").entered();
        debug!("started");
        loop {
            let Ok(msg) = to_quic_rx.recv() else {
                break;
            };
            if let Err(error) = tc_tx.send(msg) {
                pt_tx_qtt.send_or_warn(Ptn::ChannelError(error.into()));
                break;
            }
        }
        debug!("exiting");
    });
    threads.insert("tc-quic-tx".to_string(), quic_tx_thread);

    let quic_rx_thread = thread::spawn(move || {
        let _s = debug_span!("tc-quic-rx").entered();
        debug!("started");
        loop {
            match tc_rx.receive() {
                Ok(msg) => from_quic_tx.send_or_warn(Ok(msg)),
                Err(err) => {
                    from_quic_tx.send_or_warn(Err(err));
                    break;
                }
            }
        }
        debug!("exiting");
    });
    threads.insert("tc-quic-rx".to_string(), quic_rx_thread);

    let mut streams = BTreeMap::new();
    thread::spawn(move || {
        let _s = debug_span!("tc-main").entered();
        debug!("started");
        loop {
            select_biased! {
                recv(from_pt_rx) -> pt_msg_res => {
                    let Ok(pt_msg) = pt_msg_res else {
                        warn!("primary died before tc-main");
                        break;
                    };
                    trace!(?pt_msg, "received message from primary");
                    match pt_msg {
                        Notification::Allocations(_) => warn!("received gratuitous tunnel allocations, ignoring"),
                        Notification::Terminate => break,
                    }
                }
                recv(from_quic_rx) -> quic_thread_msg_res => {
                    let Ok(quic_res) = quic_thread_msg_res else {
                        debug!("tc-quic-rx died before tc-main");
                        break;
                    };
                    let s2c = match quic_res {
                        Ok(msg) => msg,
                        Err(error) => {
                            pt_tx.send_or_warn(Ptn::ChannelError(error.into()));
                            continue;
                        }
                    };
                    let id = Id::new(s2c.tunnel_id, s2c.stream_id);
                    match s2c.message {
                        Some(proto::message::Message::OpenTcp(_)) => {
                            handle_open_from_server(
                                id,
                                &to_quic_tx,
                                &stream_state_tx,
                                &server_bound_tunnels,
                                &mut streams
                            );
                        }
                        Some(proto::message::Message::CloseTcp(_)) => {
                            handle_close_from_server(
                                id,
                                &mut streams
                            );
                        }
                        Some(proto::message::Message::TcpData(tcp_data)) => {
                            handle_data_from_server(
                                id,
                                tcp_data.data,
                                &to_quic_tx,
                                &mut streams,
                            );
                        }
                        None => warn!("empty bodied tunnel message")
                    }
                }
                recv(stream_state_rx) -> stream_state_msg_res => {
                    let Ok(stream_state_msg) = stream_state_msg_res else {
                        warn!("all stream_state message emitting threads died before tc-main");
                        break;
                    };

                    match stream_state_msg {
                        StreamState::Started { id, stream } => {
                            handle_new_stream_established(
                                id,
                                stream,
                                &to_quic_tx,
                                &stream_state_tx,
                                &client_bound_tunnels,
                                &mut streams,
                            );
                        },
                        StreamState::Finished { id, result } => {
                            handle_stream_result(
                                id,
                                result,
                                &to_quic_tx,
                                &mut streams
                            )
                        }
                    }
                },
            }
        }
        debug!("terminating stream threads");
        for (id, state) in streams {
            if let State::Opened {
                stream,
                read_thread,
                write_thread,
                ..
            } = state
            {
                let _ = stream.shutdown(std::net::Shutdown::Read);
                let _ = stream.shutdown(std::net::Shutdown::Both);
                if let Err(error) = read_thread.join() {
                    warn!(?id, "read thread panicked: {error:?}");
                }
                if let Err(error) = write_thread.join() {
                    warn!(?id, "write thread panicked: {error:?}");
                }
            }
            to_quic_tx.send_or_warn(close_tcp_stream(id));
        }
        debug!("terminating remaining threads");
        for waker in accept_wakers.values() {
            let _ = waker.wake();
        }
        for (thread_name, jh) in threads {
            trace!(thread_name, "waiting");
            if let Err(error) = jh.join() {
                warn!("{thread_name} panicked: {error:?}");
            }
        }
        debug!("exiting");
    });
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
                    info_always!(
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
