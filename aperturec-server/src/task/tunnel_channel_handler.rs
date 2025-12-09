use aperturec_channel::*;
use aperturec_protocol::tunnel::*;
use aperturec_state_machine::*;

use anyhow::{Error, Result, anyhow, bail};
use futures::prelude::*;
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpSocket, lookup_host};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::sleep;
use tokio_stream::wrappers::TcpListenerStream;
use tokio_util::sync::CancellationToken;
use tracing::*;

#[derive(Debug)]
pub enum Tunnel {
    ClientBound(Description),
    ServerBound(Description, TcpListener),
}

#[derive(Stateful, SelfTransitionable, Debug)]
#[state(S)]
pub struct Task<S: State> {
    state: S,
}

#[derive(State)]
pub struct Created {
    tc: AsyncServerTunnel,
    tunnels: BTreeMap<u64, Tunnel>,
}

#[derive(State)]
pub struct Running {
    ct: CancellationToken,
    tasks: JoinSet<Result<()>>,
}

#[derive(State, Debug)]
pub struct Terminated;

const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

pub fn generate_responses(
    requests: &BTreeMap<u64, Description>,
) -> (BTreeMap<u64, Response>, BTreeMap<u64, Tunnel>) {
    let client_bound_tunnels = requests
        .iter()
        .filter(|(_, desc)| desc.side() == Side::Client)
        .filter(|(_, desc)| desc.protocol() == Protocol::Tcp)
        .map(|(id, desc)| (*id, Tunnel::ClientBound(desc.clone())));
    let server_bound_tunnels = requests
        .iter()
        .filter(|(_, desc)| desc.side() == Side::Server)
        .filter(|(_, desc)| desc.protocol() == Protocol::Tcp)
        .map::<(_, Result<_>), _>(|(id, desc)| {
            (
                *id,
                (|| {
                    let ip = if desc.bind_address.is_empty() {
                        IpAddr::V4(Ipv4Addr::UNSPECIFIED)
                    } else {
                        desc.bind_address.parse::<IpAddr>()?
                    };
                    let sa = (ip, desc.bind_port.try_into()?).into();
                    let socket = match &sa {
                        SocketAddr::V4(_) => TcpSocket::new_v4(),
                        SocketAddr::V6(_) => TcpSocket::new_v6(),
                    }?;
                    socket.bind(sa)?;
                    let mut desc = desc.clone();
                    let local_addr = socket.local_addr()?;
                    desc.bind_address = local_addr.ip().to_string();
                    desc.bind_port = local_addr.port() as u32;
                    let listener = socket.listen(u32::MAX)?;
                    Ok(Tunnel::ServerBound(desc, listener))
                })(),
            )
        });

    let tunnel_results = client_bound_tunnels
        .map(|(id, tun)| (id, Ok(tun)))
        .chain(server_bound_tunnels)
        .collect::<BTreeMap<_, _>>();

    let responses = tunnel_results
        .iter()
        .map(|(id, tun_res)| {
            (
                *id,
                Response::from(match &tun_res {
                    Ok(Tunnel::ServerBound(desc, _)) | Ok(Tunnel::ClientBound(desc)) => {
                        response::Message::Success(desc.clone())
                    }
                    Err(e) => response::Message::Failure(e.to_string()),
                }),
            )
        })
        .collect();

    let tunnels = tunnel_results
        .into_iter()
        .filter_map(|(id, tun_res)| tun_res.ok().map(|tun| (id, tun)))
        .collect();

    (responses, tunnels)
}

impl Task<Created> {
    pub fn new(tc: AsyncServerTunnel, tunnels: BTreeMap<u64, Tunnel>) -> Self {
        Task {
            state: Created { tc, tunnels },
        }
    }
}

impl Task<Running> {
    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.state.ct
    }
}

impl Transitionable<Running> for Task<Created> {
    type NextStateful = Task<Running>;

    fn transition(self) -> Self::NextStateful {
        let mut js = JoinSet::new();

        let (mut tc_rx, mut tc_tx) = self.state.tc.split();

        let (new_tcp_streams_tx, mut new_tcp_streams_rx) = mpsc::unbounded_channel();
        let (data_to_client_tx, mut data_to_client_rx) = mpsc::unbounded_channel();
        let (closes_to_client_tx, mut closes_to_client_rx) = mpsc::unbounded_channel();
        let (opens_to_client_tx, mut opens_to_client_rx) = mpsc::unbounded_channel();
        let (data_from_client_tx, mut data_from_client_rx) = mpsc::unbounded_channel();
        let (closes_from_client_tx, mut closes_from_client_rx) = mpsc::unbounded_channel();
        let (opens_from_client_tx, mut opens_from_client_rx) = mpsc::unbounded_channel();
        let (drain_timeout_tx, mut drain_timeout_rx) = mpsc::unbounded_channel();

        let (server_bound_tunnels, client_bound_tunnels) = self.state.tunnels.into_iter().fold(
            (BTreeMap::new(), BTreeMap::new()),
            |(mut s, mut c), (id, t)| {
                match t {
                    Tunnel::ClientBound(desc) => {
                        c.insert(id, desc);
                    }
                    Tunnel::ServerBound(_, listener) => {
                        s.insert(id, listener);
                    }
                }
                (s, c)
            },
        );

        let server_bound_tids = server_bound_tunnels
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();

        if !server_bound_tunnels.is_empty() {
            let mut accepts = stream::iter(server_bound_tunnels)
                .map(|(id, listener)| TcpListenerStream::new(listener).map(move |res| (id, res)))
                .flatten_unordered(None);

            let tx = new_tcp_streams_tx.clone();
            let mut sid = 0;
            js.spawn(async move {
                while let Some((tid, Ok(tcp_stream))) = accepts.next().await {
                    tcp_stream.set_linger(None)?;
                    tx.send((tid, sid, tcp_stream))?;
                    sid += 1;
                }
                bail!("tcp accept streams exhausted");
            });
        }

        js.spawn(async move {
            let mut stream_tasks = JoinSet::new();
            let mut txs = BTreeMap::new();
            let mut abort_handles = BTreeMap::new();

            loop {
                tokio::select! {
                    biased;
                    Some((tid, sid)) = opens_from_client_rx.recv() => async {
                        trace!(tid, sid);
                        let (forward_addr, forward_port) =
                            match client_bound_tunnels.get(&tid) {
                                Some(desc) => (desc.forward_address.as_str(), desc.forward_port as u16),
                                None => {
                                    warn!("non-existent");
                                    closes_to_client_tx.send((tid, sid))?;
                                    return Ok(());
                                }
                            };
                        let socket_addrs =
                            match lookup_host((forward_addr, forward_port)).await {
                                Ok(socket_addrs) => socket_addrs,
                                Err(e) => {
                                    warn!(
                                        forward_addr,
                                        forward_port,
                                        %e,
                                        "failed lookup",
                                    );
                                    closes_to_client_tx.send((tid, sid))?;
                                    return Ok(());
                                }
                            };
                        let mut tcp_streams = stream::iter(
                            socket_addrs
                                .map(|sa| {
                                    match sa {
                                        SocketAddr::V4(_) => TcpSocket::new_v4(),
                                        SocketAddr::V6(_) => TcpSocket::new_v6(),
                                    }
                                    .map(|socket| socket.connect(sa))
                                })
                                .filter_map(Result::ok)
                                .map(futures::FutureExt::boxed),
                        )
                        .then(|s| s)
                        .filter_map(|s| future::ready(s.ok()));

                        if let Some(tcp_stream) = tcp_streams.next().await {
                            new_tcp_streams_tx.send((tid, sid, tcp_stream))?;
                        } else {
                            warn!(forward_addr, forward_port, "failed connect");
                            closes_to_client_tx.send((tid, sid))?;
                        }
                        Ok::<_, Error>(())
                    }.instrument(trace_span!("open-from-client")).await?,

                    Some((tid, sid, mut tcp_stream)) = new_tcp_streams_rx.recv() => async {
                        trace!(tid, sid);
                        let (to_tx, mut to_tx_rx) = mpsc::channel::<Vec<u8>>(1);
                        let data_to_client_tx = data_to_client_tx.clone();
                        let ah = stream_tasks.spawn(async move {
                            let res = loop {
                                tokio::select! {
                                    Some(data) = to_tx_rx.recv() => {
                                        if let Err(e) = tcp_stream.write_all(&data).await {
                                            break Err(e.into());
                                        }
                                    }
                                    Ok(()) = tcp_stream.readable() => {
                                        let mut data = vec![0_u8; 0x1000];
                                        let nbytes = match tcp_stream.try_read(&mut data) {
                                            Ok(nbytes) => nbytes,
                                            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                                            Err(e) => break Err(e.into()),
                                        };
                                        if nbytes == 0 {
                                            break Ok(());
                                        } else {
                                            data.truncate(nbytes);
                                            if let Err(e) = data_to_client_tx.send((tid, sid, data)) {
                                                break Err(e.into());
                                            }
                                        }
                                    }
                                    else => break Ok::<_, Error>(()),
                                }
                            };
                            // Loop finished; send FIN to close the write side of the TCP connection.
                            let _ = tcp_stream.shutdown().await;
                            (tid, sid, res)
                        });
                        abort_handles.insert((tid, sid), ah);
                        txs.insert((tid, sid), to_tx);
                        if server_bound_tids.contains(&tid) {
                            opens_to_client_tx.send((tid, sid))?;
                        }
                        Ok::<_, Error>(())
                    }.instrument(trace_span!("new-tcp-stream")).await?,

                    Some(Ok((tid, sid, res))) = stream_tasks.join_next(), if !stream_tasks.is_empty() => async {
                        trace!(tid, sid, ?res);
                        if let Err(e) = res {
                            debug!(%e);
                        }
                        closes_to_client_tx.send((tid, sid))?;
                        txs.remove(&(tid, sid));
                        abort_handles.remove(&(tid, sid));
                        Ok::<_, Error>(())
                    }.instrument(trace_span!("stream-task-result")).await?,

                    Some((tid, sid, data)) = data_to_client_rx.recv() => async {
                        trace!(tid, sid, ?data);
                        let msg = MessageBuilder::default()
                            .tunnel_id(tid)
                            .stream_id(sid)
                            .message(TcpData::new(data))
                            .build()
                            .expect("build tunnel message");
                        tc_tx.send(msg).await?;
                        Ok::<_, Error>(())
                    }.instrument(trace_span!("data-to-client")).await?,

                    Some((tid, sid, data)) = data_from_client_rx.recv() => async {
                        trace!(tid, sid, ?data);
                        if let Some(tx) = txs.get_mut(&(tid, sid)) {
                            // channel task can close at any time if the TcpStream closes
                            let _ = tx.send(data).await;
                        } else {
                            warn!("non-existent, dropping");
                        }
                    }.instrument(trace_span!("data-from-client")).await,

                    Some((tid, sid)) = closes_from_client_rx.recv() => async {
                        trace!(tid, sid, "client closed stream; half-closing write side");
                        // Drop the write channel so the per-stream task sees sender closure,
                        // stops writing, and drains reads until EOF; FIN is sent when the task exits.
                        txs.remove(&(tid, sid));
                        if abort_handles.contains_key(&(tid, sid)) {
                            let drain_timeout_tx = drain_timeout_tx.clone();
                            tokio::spawn(async move {
                                sleep(DRAIN_TIMEOUT).await;
                                let _ = drain_timeout_tx.send((tid, sid));
                            });
                        }
                    }.instrument(trace_span!("closes-from-client")).await,

                    Some((tid, sid)) = drain_timeout_rx.recv() => async {
                        trace!(tid, sid, "drain timeout reached; aborting stream");
                        if let Some(ah) = abort_handles.remove(&(tid, sid)) {
                            ah.abort();
                        }
                        txs.remove(&(tid, sid));
                    }.instrument(trace_span!("drain-timeout")).await,

                    Some((tid, sid)) = closes_to_client_rx.recv() => async {
                        trace!(tid, sid);
                        let msg = MessageBuilder::default()
                            .tunnel_id(tid)
                            .stream_id(sid)
                            .message(CloseTcpStream::new())
                            .build()
                            .expect("build tunnel message");
                        tc_tx.send(msg).await?;
                        Ok::<_, Error>(())
                    }.instrument(trace_span!("closes-to-client")).await?,

                    Some((tid, sid)) = opens_to_client_rx.recv() => async {
                        trace!(tid, sid);
                        let msg = MessageBuilder::default()
                            .tunnel_id(tid)
                            .stream_id(sid)
                            .message(OpenTcpStream::new())
                            .build()
                            .expect("build tunnel message");
                        tc_tx.send(msg).await?;
                        Ok::<_, Error>(())
                    }.instrument(trace_span!("opens-to-client")).await?,

                    else => bail!("dispatch task exhausted"),
                }
            }
        });

        js.spawn(async move {
            while let Ok(c2s) = tc_rx.receive().await {
                match c2s.message {
                    Some(message::Message::OpenTcp(_)) => {
                        opens_from_client_tx.send((c2s.tunnel_id, c2s.stream_id))?;
                    }
                    Some(message::Message::CloseTcp(_)) => {
                        closes_from_client_tx.send((c2s.tunnel_id, c2s.stream_id))?;
                    }
                    Some(message::Message::TcpData(tcp_data)) => {
                        data_from_client_tx.send((c2s.tunnel_id, c2s.stream_id, tcp_data.data))?;
                    }
                    None => warn!("empty bind-to-forward message"),
                }
            }
            bail!("tunnel channel rx exhausted");
        });

        Task {
            state: Running {
                tasks: js,
                ct: CancellationToken::new(),
            },
        }
    }
}

impl Transitionable<Terminated> for Task<Running> {
    type NextStateful = Task<Terminated>;

    fn transition(self) -> Self::NextStateful {
        Task { state: Terminated }
    }
}

impl AsyncTryTransitionable<Terminated, Terminated> for Task<Running> {
    type SuccessStateful = Task<Terminated>;
    type FailureStateful = Task<Terminated>;
    type Error = anyhow::Error;

    async fn try_transition(
        mut self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let stateful = Task { state: Terminated };
        let mut errors = vec![];

        loop {
            tokio::select! {
                _ = self.state.ct.cancelled() => {
                    self.state.tasks.abort_all();
                    break;
                },
                Some(task_res) = self.state.tasks.join_next() => {
                    let error = match task_res {
                        Ok(Ok(())) => {
                            trace!("task exited");
                            self.state.ct.cancel();
                            continue;
                        },
                        Ok(Err(e)) => anyhow!("tunnel channel handler task exited with internal error: {e}"),
                        Err(e) => anyhow!("tunnel channel handler task exited with panic: {e}"),
                    };
                    errors.push(error);
                },
                else => break,
            }
        }

        if errors.is_empty() {
            Ok(stateful)
        } else {
            Err(Recovered {
                stateful,
                error: anyhow!("tunnel channel handler errors: {errors:?}"),
            })
        }
    }
}
