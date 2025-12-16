use aperturec_channel::{self as channel, Sender as _, TimeoutReceiver};
use aperturec_protocol::tunnel as proto;
use aperturec_utils::channels::SenderExt;
use aperturec_utils::info_always;

use crossbeam::channel::{Receiver, RecvTimeoutError, Sender, bounded, select_biased};
use socket2::Socket;
use std::{
    collections::BTreeMap,
    io::{self, Read, Write},
    net::{IpAddr, Ipv4Addr, TcpListener, TcpStream, ToSocketAddrs},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use tracing::*;

const TCP_NETWORK_TIMEOUT: Duration = Duration::from_millis(100);

/// Lifecycle of a TCP stream that backs a logical tunnel stream.
///
/// Streams always start in `Opening`, move to `Opened` once the TCP socket is fully established,
/// and then advance through `HalfClosed` and `FullyClosed` as either side tears down the socket.
/// The shared `should_terminate` flag is carried across the active states to let the read/write
/// worker threads know when the coordinator has requested shutdown.
enum State {
    /// The tunnel stream exists at the QUIC layer but the TCP socket is not ready yet.
    ///
    /// Bytes arriving from the server are buffered until the socket is available so that
    /// early data is not dropped.
    Opening {
        /// Bytes delivered by the remote side before the TCP socket was established locally.
        queued: Vec<u8>,
        /// Shared signal used by the coordinator to ask the read/write threads to exit.
        should_terminate: Arc<AtomicBool>,
    },
    /// Stream is fully established and actively forwarding bytes between TCP and the tunnel.
    Opened {
        /// Channel feeding the write thread with data destined for the TCP socket.
        to_tx: Sender<Vec<u8>>,
        /// Shared signal used by the coordinator to ask the read/write threads to exit.
        should_terminate: Arc<AtomicBool>,
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
    #[error("internal channel disconnected")]
    Disconnected,
}

#[derive(Debug)]
enum StreamState {
    Started {
        id: Id,
        rh: TcpStream,
        wh: TcpStream,
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

fn configure_split_stream(stream: TcpStream) -> io::Result<(TcpStream, TcpStream)> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(TCP_NETWORK_TIMEOUT.into())?;
    let rh = stream.try_clone()?;
    let wh = stream;
    Ok((rh, wh))
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

    streams.insert(
        id,
        State::Opening {
            queued: vec![],
            should_terminate: Arc::new(AtomicBool::new(false)),
        },
    );

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
        .map(TcpStream::connect)
        .filter_map(Result::ok);

        let Some(stream) = stream_iter.next() else {
            warn!("failed to make connection to {forward_addr}:{forward_port}");
            to_quic_tx.send_or_warn(close_tcp_stream(id));
            return;
        };

        let (rh, wh) = match configure_split_stream(stream) {
            Ok((rh, wh)) => (rh, wh),
            Err(error) => {
                warn!(%error, "failed configuring stream");
                to_quic_tx.send_or_warn(close_tcp_stream(id));
                return;
            }
        };

        stream_state_tx.send_or_warn(StreamState::Started { id, rh, wh });
    });
}

fn handle_close_from_server(id: Id, streams: &mut BTreeMap<Id, State>) {
    let _s = trace_span!("close-from-server", ?id).entered();
    match streams.remove(&id) {
        Some(State::Opening {
            should_terminate, ..
        })
        | Some(State::Opened {
            should_terminate, ..
        }) => {
            should_terminate.store(true, Ordering::Release);
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
    mut rh: TcpStream,
    mut wh: TcpStream,
    to_quic_tx: &Sender<proto::Message>,
    stream_state_tx: &Sender<StreamState>,
    client_bound_tunnels: &BTreeMap<u64, proto::Description>,
    streams: &mut BTreeMap<Id, State>,
) {
    let _s = trace_span!("new-stream-established", ?id).entered();
    trace!(?rh, ?wh);

    let (queued, should_terminate) = match streams.remove(&id) {
        Some(State::Opening {
            queued,
            should_terminate,
        }) => (queued, should_terminate),
        Some(State::Opened {
            should_terminate, ..
        }) => {
            warn!("already open, ignoring");
            should_terminate.store(true, Ordering::Release);
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
                (vec![], Arc::new(AtomicBool::new(false)))
            } else {
                warn!("non-existent");
                return;
            }
        }
    };

    let stream_state_tx_rh = stream_state_tx.clone();
    let should_terminate_rh = should_terminate.clone();
    let to_quic_tx = to_quic_tx.clone();
    let read_thread = thread::spawn(move || {
        let _s = trace_span!("tc-stream-read-thread", ?id).entered();
        trace!("starting");
        let result = loop {
            if should_terminate_rh.load(Ordering::Acquire) {
                break Ok(());
            }
            let mut data = vec![0_u8; 0x1000];
            let nbytes = match rh.read(&mut data) {
                Ok(nbytes_read) => nbytes_read,
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => continue,
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
    let should_terminate_wh = should_terminate.clone();
    let write_thread = thread::spawn(move || {
        let _s = trace_span!("tc-stream-write-thread", ?id).entered();
        trace!("starting");
        if let Err(e) = wh.write_all(&queued) {
            stream_state_tx_wh.send_or_warn(StreamState::Finished {
                id,
                result: Err(e.into()),
            });
            return;
        }
        let result = loop {
            if should_terminate_wh.load(Ordering::Acquire) {
                break Ok(());
            }
            let data = match to_tx_rx.recv_timeout(super::ITC_TIMEOUT) {
                Ok(data) => data,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break Err(StreamError::Disconnected),
            };
            if let Err(e) = wh.write_all(&data) {
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
            should_terminate,
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
        Some(State::Opened {
            should_terminate, ..
        }) => {
            trace!("closing first half");
            should_terminate.store(true, Ordering::Release);
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

    let should_stop = Arc::new(AtomicBool::new(false));
    let (from_quic_tx, from_quic_rx) = bounded(0);
    let (to_quic_tx, to_quic_rx) = bounded(0);
    let (stream_state_tx, stream_state_rx) = bounded(0);
    let (error_tx, error_rx) = bounded(0);

    let mut threads = BTreeMap::new();

    for (&tunnel_id, desc) in &client_bound_tunnels {
        let desc = desc.clone();
        debug!(?desc);
        let should_stop_accept = should_stop.clone();
        let stream_state_tx = stream_state_tx.clone();

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

            let listener = match TcpListener::bind((addr, port)) {
                Ok(listener) => Socket::from(listener),
                Err(error) => {
                    warn!(%error, "failed binding {addr}:{port}");
                    return;
                }
            };
            if let Err(error) = listener.set_read_timeout(Some(TCP_NETWORK_TIMEOUT)) {
                warn!(%error, "failed setting socket read timeout");
                return;
            }

            loop {
                if should_stop_accept.load(Ordering::Acquire) {
                    break;
                }
                let stream = match listener.accept() {
                    Ok((stream, _)) => stream.into(),
                    Err(e)
                        if e.kind() == io::ErrorKind::WouldBlock
                            || e.kind() == io::ErrorKind::TimedOut =>
                    {
                        trace!("continue");
                        continue;
                    }
                    Err(error) => {
                        warn!(%error, stream_id, "failed accepting");
                        return;
                    }
                };
                debug!(stream_id, "accepted");
                let (rh, wh) = match configure_split_stream(stream) {
                    Ok((rh, wh)) => (rh, wh),
                    Err(error) => {
                        warn!(%error, "failed configuring stream");
                        continue;
                    }
                };
                stream_state_tx.send_or_warn(StreamState::Started {
                    id: Id::new(tunnel_id, stream_id),
                    rh,
                    wh,
                });
                debug!(stream_id, "forwarded to new stream handler");
                stream_id += 1;
            }
        });

        threads.insert(format!("tc-accept-{tunnel_id}"), accept_thread);
    }

    let should_stop_write = should_stop.clone();
    let error_tx_qtt = error_tx.clone();
    let quic_tx_thread = thread::spawn(move || {
        let _s = debug_span!("tc-quic-tx").entered();
        debug!("started");
        loop {
            if should_stop_write.load(Ordering::Acquire) {
                break;
            }
            match to_quic_rx.recv_timeout(super::ITC_TIMEOUT) {
                Ok(msg) => {
                    if let Err(error) = tc_tx.send(msg) {
                        error_tx_qtt.send_or_warn(ChannelError::from(error));
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        debug!("exiting");
    });
    threads.insert("tc-quic-tx".to_string(), quic_tx_thread);

    let should_stop_read = should_stop.clone();
    let error_tx_qrt = error_tx.clone();
    let quic_rx_thread = thread::spawn(move || {
        let _s = debug_span!("tc-quic-rx").entered();
        debug!("started");
        loop {
            if should_stop_read.load(Ordering::Acquire) {
                break;
            }
            match tc_rx.receive_timeout(super::NETWORK_CHANNEL_TIMEOUT) {
                Ok(None) => continue,
                Ok(Some(msg)) => {
                    trace!(?msg);
                    from_quic_tx.send_or_warn(msg)
                }
                Err(error) => {
                    error_tx_qrt.send_or_warn(ChannelError::from(error));
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
                recv(error_rx) -> error_res => {
                    let Ok(error) = error_res else {
                        warn!("all error emitting threads all died before primary");
                        break;
                    };
                    pt_tx.send_or_warn(Ptn::ChannelError(error));
                }
                recv(from_quic_rx) -> quic_thread_msg_res => {
                    let Ok(s2c) = quic_thread_msg_res else {
                        debug!("tc-quic-rx died before tc-main");
                        break;
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
                        StreamState::Started { id, rh, wh } => {
                            handle_new_stream_established(
                                id,
                                rh,
                                wh,
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
            let (State::Opening {
                ref should_terminate,
                ..
            }
            | State::Opened {
                ref should_terminate,
                ..
            }) = state
            else {
                continue;
            };
            should_terminate.store(true, Ordering::Release);
            if let State::Opened {
                read_thread,
                write_thread,
                ..
            } = state
            {
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
        should_stop.store(true, Ordering::Release);
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
