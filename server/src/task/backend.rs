use crate::backend::{Backend, Event};
use crate::task::encoder::{self, Encoder};
use crate::task::event_channel_handler::EVENT_QUEUE;

use anyhow::{anyhow, Result};
use aperturec_channel::unreliable::udp;
use aperturec_channel::AsyncServerMediaChannel;
use aperturec_protocol::common::*;
use aperturec_protocol::control as cm;
use aperturec_state_machine::{
    try_recover, try_transition_inner_recover, Recovered, SelfTransitionable, State, Stateful,
    Transitionable, TryTransitionable,
};
use aperturec_trace::log;
use aperturec_trace::queue::deq;
use async_trait::async_trait;
use futures::stream::FuturesUnordered;
use futures::{FutureExt, StreamExt};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_util::sync::CancellationToken;

#[derive(Stateful, SelfTransitionable, Debug)]
#[state(S)]
pub struct Task<S: State> {
    state: S,
}

#[derive(State, Debug)]
pub struct Created<B: Backend + 'static> {
    backend: B,
    event_rx: mpsc::UnboundedReceiver<Event>,
    fb_update_req_rx: mpsc::UnboundedReceiver<cm::FramebufferUpdateRequest>,
    missed_frame_rx: mpsc::UnboundedReceiver<cm::MissedFrameReport>,
    acked_seq_rx: mpsc::UnboundedReceiver<cm::DecoderSequencePair>,
    decoders: Vec<(Decoder, Location, Dimension)>,
    mc_servers: Vec<udp::Server<udp::Connected>>,
    codecs: BTreeMap<u16, Codec>,
    client_addr: SocketAddr,
}

impl<B: Backend + 'static> Task<Created<B>> {
    pub fn into_backend(self) -> B {
        self.state.backend
    }
}

#[derive(State, Debug)]
pub struct Running<B: Backend + 'static> {
    acked_seq_subtask: JoinHandle<Result<()>>,
    acked_seq_subtask_ct: CancellationToken,
    backend_subtask: JoinHandle<(B, Result<()>)>,
    backend_subtask_ct: CancellationToken,
    missed_frame_subtask: JoinHandle<Result<()>>,
    missed_frame_subtask_ct: CancellationToken,
    fb_update_req_subtask: JoinHandle<Result<()>>,
    fb_update_req_subtask_ct: CancellationToken,
    encoder_subtasks: BTreeMap<
        u16,
        JoinHandle<
            Result<
                encoder::Encoder<encoder::Terminated>,
                aperturec_state_machine::Recovered<
                    encoder::Encoder<encoder::Terminated>,
                    anyhow::Error,
                >,
            >,
        >,
    >,
    encoder_subtask_cts: BTreeMap<u16, CancellationToken>,
    ct: CancellationToken,
}

impl<B: Backend + 'static> Task<Running<B>> {
    pub fn stop(&self) {
        self.state.ct.cancel();
    }

    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.state.ct
    }
}

#[derive(State, Debug)]
pub struct Terminated<B: Backend + 'static> {
    backend: B,
}

impl<B: Backend + 'static> Task<Terminated<B>> {
    pub fn into_backend(self) -> B {
        self.state.backend
    }

    pub fn root_process_exited(&self) -> bool {
        self.state.backend.root_process_exited()
    }
}

pub struct Channels {
    pub acked_seq_tx: mpsc::UnboundedSender<cm::DecoderSequencePair>,
}

impl<B: Backend + 'static> Task<Created<B>> {
    pub fn new<'d, I>(
        backend: B,
        decoders: I,
        mc_servers: Vec<udp::Server<udp::Connected>>,
        codecs: BTreeMap<u16, Codec>,
        client_addr: &SocketAddr,
        event_rx: mpsc::UnboundedReceiver<Event>,
        fb_update_req_rx: mpsc::UnboundedReceiver<cm::FramebufferUpdateRequest>,
        missed_frame_rx: mpsc::UnboundedReceiver<cm::MissedFrameReport>,
    ) -> (Task<Created<B>>, Channels)
    where
        I: IntoIterator<Item = &'d (Decoder, Location, Dimension)>,
    {
        let (acked_seq_tx, acked_seq_rx) = mpsc::unbounded_channel();

        let task = Task {
            state: Created {
                backend,
                event_rx,
                fb_update_req_rx,
                missed_frame_rx,
                acked_seq_rx,
                decoders: decoders.into_iter().cloned().collect(),
                mc_servers,
                codecs,
                client_addr: *client_addr,
            },
        };

        (task, Channels { acked_seq_tx })
    }
}

#[async_trait]
impl<B: Backend + 'static> TryTransitionable<Running<B>, Created<B>> for Task<Created<B>> {
    type SuccessStateful = Task<Running<B>>;
    type FailureStateful = Task<Created<B>>;
    type Error = anyhow::Error;

    async fn try_transition(
        mut self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let mut addr = self.state.client_addr;

        let subtask_ct = CancellationToken::new();

        let mut acked_frame_txs = BTreeMap::new();
        let mut fb_txs = BTreeMap::new();
        let mut missed_frame_txs = BTreeMap::new();
        let mut encoder_subtasks = BTreeMap::new();
        let mut encoder_subtask_cts = BTreeMap::new();

        for ((decoder, location, dimension), mc_server) in
            self.state.decoders.iter().zip(&self.state.mc_servers)
        {
            let port = decoder.port as u16;
            addr.set_port(port);
            let async_mc_server =
                try_transition_inner_recover!(mc_server.clone(), udp::Closed, |_| self);
            let mc = AsyncServerMediaChannel::new(async_mc_server);
            let codec = self.state.codecs.remove(&port).unwrap_or(Codec::Raw);
            let (encoder, encoder_channels) =
                Encoder::new(decoder, location, dimension, mc, codec, subtask_ct.clone());
            let running = try_transition_inner_recover!(
                encoder,
                encoder::Created<AsyncServerMediaChannel>,
                |_| self
            );
            let ct = running.cancellation_token().clone();
            let subtask = tokio::spawn(async move { running.try_transition().await });
            encoder_subtasks.insert(port, subtask);
            encoder_subtask_cts.insert(port, ct);
            acked_frame_txs.insert(port, encoder_channels.acked_frame_tx);
            fb_txs.insert(port, encoder_channels.fb_tx);
            missed_frame_txs.insert(port, encoder_channels.missed_frame_tx);
        }

        let mut damage_stream = try_recover!(self.state.backend.damage_stream().await, self).fuse();
        let mut event_stream = UnboundedReceiverStream::new(self.state.event_rx).fuse();
        let mut fb_update_req_stream =
            UnboundedReceiverStream::new(self.state.fb_update_req_rx).fuse();
        let mut missed_frame_stream =
            UnboundedReceiverStream::new(self.state.missed_frame_rx).fuse();
        let mut acked_seq_stream = UnboundedReceiverStream::new(self.state.acked_seq_rx).fuse();

        let acked_seq_subtask_ct = CancellationToken::new();
        let ct = acked_seq_subtask_ct.clone();
        let acked_seq_subtask = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = ct.cancelled() => {
                        log::info!("Acked sequence subtask cancelled");
                        break Ok(())
                    }
                    Some(acked_seq_pair) = acked_seq_stream.next() => {
                        let decoder = match acked_seq_pair.decoder {
                            Some(decoder) => decoder,
                            None => {
                                log::warn!("Received ACK without decoder");
                                continue;
                            }
                        };
                        match acked_frame_txs.get(&(decoder.port as u16)) {
                            Some(channel) => channel.send(acked_seq_pair.sequence_id).await?,
                            None => log::warn!("Received HB for untracked decoder {}", decoder.port),
                        }
                    }
                    else => break Err(anyhow!("Acked sequence stream exhausted")),
                }
            }
        });

        let backend_subtask_ct = CancellationToken::new();
        let ct = backend_subtask_ct.clone();
        let backend_subtask = tokio::spawn(async move {
            let loop_res = 'select: loop {
                tokio::select! {
                    biased;
                    _ = ct.cancelled() => {
                        log::info!("Backend subtask cancelled");
                        break Ok(());
                    }
                    _ = self.state.backend.wait_root_process() => {
                        log::debug!("Root process exited");
                        break Ok(());
                    }
                    Some(event) = event_stream.next() => {
                        if let Err(e) = self.state.backend.notify_event(event).await {
                            log::warn!("Failed to notify backend of event: {}", e);
                            continue;
                        }
                        deq!(EVENT_QUEUE);
                    }
                    Some(damage_area) = damage_stream.next() => {
                        let fbu = Arc::new({
                            match self.state.backend.capture_area(damage_area).await {
                                Err(err) => {
                                    log::error!("Failed to capture screen: {}", err);
                                    break 'select Err(err);
                                },
                                Ok(fbu) => fbu,
                            }
                        });
                        for channel in &mut fb_txs.values_mut() {
                            if let Err(err) = channel.send(fbu.clone()) {
                                log::error!("Failed to send FBU to encoder: {}", err);
                                break 'select Err(err.into());
                            }
                        }
                    }
                    else => break Err(anyhow!("damage stream exhuasted"))
                }
            };
            (self.state.backend, loop_res)
        });

        let missed_frame_subtask_ct = CancellationToken::new();
        let ct = missed_frame_subtask_ct.clone();
        let missed_frame_subtask = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = ct.cancelled() => {
                        log::info!("Missed frame subtask cancelled");
                        break Ok(());
                    }
                    Some(missed_frame_report) = missed_frame_stream.next() => {
                        let decoder = match &missed_frame_report.decoder {
                            Some(decoder) => decoder,
                            None => {
                                log::warn!("Received MFR without decoder");
                                continue;
                            },
                        };

                        aperturec_metrics::builtins::packet_lost(missed_frame_report.frame_sequence_ids.len());
                        match missed_frame_txs.get(&(decoder.port as u16)) {
                            Some(channel) => {
                                for frame in &missed_frame_report.frame_sequence_ids {
                                    channel.send(*frame)?;
                                }
                            },
                            None => log::warn!("Received missed frame report for invalid decoder {}", decoder.port),
                        }
                    }
                    else => break Err(anyhow!("Exhausted missed frame report stream"))
                }
            }
        });

        let fb_update_req_subtask_ct = CancellationToken::new();
        let ct = fb_update_req_subtask_ct.clone();
        let fb_update_req_subtask = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = ct.cancelled() => {
                        log::info!("Framebuffer update request subtask cancelled");
                        break Ok(());
                    }
                    Some(_) = fb_update_req_stream.next() => {
                        log::warn!("Explicit framebuffer update requests not yet supported");
                    }
                    else => break Err(anyhow!("Exhausted framebuffer update request stream"))
                }
            }
        });
        Ok(Task {
            state: Running {
                ct: CancellationToken::new(),
                acked_seq_subtask,
                acked_seq_subtask_ct,
                backend_subtask,
                backend_subtask_ct,
                missed_frame_subtask,
                missed_frame_subtask_ct,
                fb_update_req_subtask,
                fb_update_req_subtask_ct,
                encoder_subtasks,
                encoder_subtask_cts,
            },
        })
    }
}

#[async_trait]
impl<B: Backend + 'static> TryTransitionable<Terminated<B>, Terminated<B>> for Task<Running<B>> {
    type SuccessStateful = Task<Terminated<B>>;
    type FailureStateful = Task<Terminated<B>>;
    type Error = anyhow::Error;

    async fn try_transition(
        mut self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let mut enc_result_futs = FuturesUnordered::new();
        self.state
            .encoder_subtasks
            .into_iter()
            .map(|(port, subtask)| subtask.map(move |fut| (port, fut)))
            .for_each(|fut| enc_result_futs.push(fut));

        let mut top_level_cleanup_executed = false;
        let mut backend_recovered = None;
        let (
            mut acked_seq_cleanedup,
            mut backend_cleanedup,
            mut missed_frame_cleanedup,
            mut fb_update_req_cleanedup,
        ) = (false, false, false, false);

        let loop_res = loop {
            tokio::select! {
                _ = self.state.ct.cancelled(), if !top_level_cleanup_executed => {
                    log::debug!("backend task cancelled");
                    top_level_cleanup_executed = true;
                    self.state.acked_seq_subtask_ct.cancel();
                    self.state.backend_subtask_ct.cancel();
                    self.state.missed_frame_subtask_ct.cancel();
                    self.state.fb_update_req_subtask_ct.cancel();
                    self.state.encoder_subtask_cts.iter().for_each(|(_, ct)| ct.cancel());
                }
                acked_seq_subtask_res = &mut self.state.acked_seq_subtask, if !acked_seq_cleanedup => {
                    log::debug!("acked seq subtask exiting");
                    self.state.ct.cancel();
                    acked_seq_cleanedup = true;
                    match acked_seq_subtask_res {
                        Ok(Ok(())) => (),
                        Ok(Err(err)) => log::error!("acked sequence subtask exited with error: {}", err),
                        Err(err) => log::error!("acked sequence subtask panicked: {}", err),
                    }
                }
                backend_subtask_res = &mut self.state.backend_subtask, if !backend_cleanedup => {
                    log::debug!("backend subtask exiting");
                    self.state.ct.cancel();
                    backend_cleanedup = true;
                    match backend_subtask_res {
                        Ok((backend, res)) => {
                            if let Err(err) = res {
                                log::error!("backend subtask exited with error: {}", err);
                            }
                            backend_recovered = Some(backend);
                        },
                        Err(err) => log::error!("backend subtask panicked: {}", err),
                    }
                }
                missed_frame_subtask_res = &mut self.state.missed_frame_subtask, if !missed_frame_cleanedup => {
                    log::debug!("missed frame subtask exiting");
                    self.state.ct.cancel();
                    missed_frame_cleanedup = true;
                    match missed_frame_subtask_res {
                        Ok(Ok(())) => (),
                        Ok(Err(err)) => log::error!("acked sequence subtask exited with error: {}", err),
                        Err(err) => log::error!("acked sequence subtask panicked: {}", err),
                    }
                }
                fb_update_req_subtask_res = &mut self.state.fb_update_req_subtask, if !fb_update_req_cleanedup => {
                    log::debug!("FB update req subtask exiting");
                    self.state.ct.cancel();
                    fb_update_req_cleanedup = true;
                    match fb_update_req_subtask_res {
                        Ok(Ok(())) => (),
                        Ok(Err(err)) => log::error!("fb update req subtask exited with error: {}", err),
                        Err(err) => log::error!("fb update req subtask panicked: {}", err),
                    }
                }
                Some((port, enc_res)) = enc_result_futs.next() => {
                    log::debug!("Encoder {} exiting", port);
                    self.state.ct.cancel();
                    match enc_res {
                        Ok(Ok(_)) => log::info!("Encoder {} terminated successfully", port),
                        Ok(Err(Recovered { error, .. })) => log::info!("Encoder {} terminated with error: {}", port, error),
                        Err(err) => log::error!("Encoder {} panicked: {}", port, err),
                    }
                }
                else => {
                    log::info!("All backend futures exhausted");
                    break Ok::<_, anyhow::Error>(())
                }
            }
        };
        log::trace!("Backend task complete");

        let task = Task {
            state: Terminated {
                backend: backend_recovered.expect("backend was not recovered"),
            },
        };

        match loop_res {
            Ok(()) => Ok(task),
            Err(error) => Err(Recovered {
                stateful: task,
                error,
            }),
        }
    }
}

impl<B: Backend + 'static> Transitionable<Terminated<B>> for Task<Running<B>> {
    type NextStateful = Task<Terminated<B>>;

    fn transition(self) -> Self::NextStateful {
        self.stop();
        let backend =
            match Handle::current().block_on(async move { self.state.backend_subtask.await }) {
                Ok((backend, Ok(()))) => backend,
                Ok((backend, Err(err))) => {
                    log::error!("backend task terminated in failure with error {}", err);
                    backend
                }
                Err(join_err) => panic!("Join error in backend task: {}", join_err),
            };
        Task {
            state: Terminated { backend },
        }
    }
}
