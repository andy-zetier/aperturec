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
use parking_lot::RwLock;
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
    ct: CancellationToken,
}

impl<B: Backend + 'static> Task<Created<B>> {
    pub fn into_backend(self) -> B {
        self.state.backend
    }
}

#[derive(State, Debug)]
pub struct Running<B: Backend + 'static> {
    task: JoinHandle<(B, Result<()>)>,
    ct: CancellationToken,
}

impl<B: Backend + 'static> Task<Running<B>> {
    pub fn stop(&self) {
        self.state.ct.cancel();
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
    ) -> (Task<Created<B>>, Channels, CancellationToken)
    where
        I: IntoIterator<Item = &'d (Decoder, Location, Dimension)>,
    {
        let (acked_seq_tx, acked_seq_rx) = mpsc::unbounded_channel();
        let ct = CancellationToken::new();

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
                ct: ct.clone(),
            },
        };

        (task, Channels { acked_seq_tx }, ct)
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

        let mut acked_frame_txs = BTreeMap::new();
        let mut fb_txs = BTreeMap::new();
        let mut missed_frame_txs = BTreeMap::new();
        let mut encoder_cts = BTreeMap::new();
        let mut encoders = BTreeMap::new();

        for ((decoder, location, dimension), mc_server) in
            self.state.decoders.iter().zip(&self.state.mc_servers)
        {
            let port = decoder.port as u16;
            addr.set_port(port);
            let async_mc_server =
                try_transition_inner_recover!(mc_server.clone(), udp::Closed, |_| self);
            let mc = AsyncServerMediaChannel::new(async_mc_server);
            let codec = self.state.codecs.remove(&port).unwrap_or(Codec::Raw);
            let (encoder, encoder_channels, encoder_ct) =
                Encoder::new(decoder, location, dimension, mc, codec);
            let running = try_transition_inner_recover!(
                encoder,
                encoder::Created<AsyncServerMediaChannel>,
                |_| self
            );
            encoders.insert(port, running);
            acked_frame_txs.insert(port, encoder_channels.acked_frame_tx);
            fb_txs.insert(port, encoder_channels.fb_tx);
            missed_frame_txs.insert(port, encoder_channels.missed_frame_tx);
            encoder_cts.insert(port, encoder_ct);
        }

        let mut damage_stream = try_recover!(self.state.backend.damage_stream().await, self).fuse();
        let mut event_stream = UnboundedReceiverStream::new(self.state.event_rx).fuse();
        let mut fb_update_req_stream =
            UnboundedReceiverStream::new(self.state.fb_update_req_rx).fuse();
        let mut missed_frame_stream =
            UnboundedReceiverStream::new(self.state.missed_frame_rx).fuse();
        let mut acked_seq_stream = UnboundedReceiverStream::new(self.state.acked_seq_rx).fuse();

        let backend = Arc::new(RwLock::new(self.state.backend));

        let ct = self.state.ct.clone();
        let mut acked_seq_subtask = tokio::spawn(async move {
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

        let damage_subtask_backend = backend.clone();
        let ct = self.state.ct.clone();
        let mut damage_subtask = tokio::spawn(async move {
            let loop_res: Result<()> = 'select: loop {
                tokio::select! {
                    biased;
                    _ = ct.cancelled() => {
                        log::info!("Damage subtask cancelled");
                        break Ok(());
                    }
                    Some(damage_area) = damage_stream.next() => {
                        let fbu = Arc::new({
                            let be = damage_subtask_backend.read();
                            match be.capture_area(damage_area).await {
                                Err(err) => break 'select Err(err),
                                Ok(fbu) => fbu,
                            }
                        });
                        for channel in &mut fb_txs.values_mut() {
                            if let Err(err) = channel.send(fbu.clone()) {
                                break 'select Err(err.into());
                            }
                        }
                    }
                    else => break Err(anyhow!("damage stream exhuasted"))
                }
            };
            (
                Arc::into_inner(damage_subtask_backend).map(|lock| lock.into_inner()),
                loop_res,
            )
        });

        let event_subtask_backend = backend.clone();
        let ct = self.state.ct.clone();
        let mut event_subtask = tokio::spawn(async move {
            let loop_res = loop {
                tokio::select! {
                    biased;
                    _ = ct.cancelled() => {
                        log::info!("Event subtask cancelled");
                        break Ok(());
                    }
                    Some(event) = event_stream.next() => {
                        let mut be = event_subtask_backend.write();
                        if let Err(e) = be.notify_event(event).await {
                            log::warn!("Failed to notify backend of event: {}", e);
                            continue;
                        }
                        deq!(EVENT_QUEUE);
                    }
                    else => break Err(anyhow!("event stream exhausted"))
                }
            };
            (
                Arc::into_inner(event_subtask_backend).map(|lock| lock.into_inner()),
                loop_res,
            )
        });

        let ct = self.state.ct.clone();
        let mut missed_frame_subtask = tokio::spawn(async move {
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

        let ct = self.state.ct.clone();
        let mut fb_update_req_subtask = tokio::spawn(async move {
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

        let mut enc_result_futs = FuturesUnordered::new();
        encoders
            .into_iter()
            .map(|(port, enc)| enc.try_transition().map(move |fut| (port, fut)))
            .for_each(|fut| enc_result_futs.push(fut));

        let ct = self.state.ct.clone();
        let task = tokio::spawn(async move {
            log::trace!("Running backend task");
            let mut backend_recovered = None;
            let mut has_cancelled_subtasks = false;
            let loop_res = loop {
                tokio::select! {
                    acked_seq_subtask_res = &mut acked_seq_subtask, if !acked_seq_subtask.is_finished() => {
                        match acked_seq_subtask_res {
                            Ok(Ok(())) => (),
                            Ok(Err(err)) => log::error!("acked sequence subtask exited with error: {}", err),
                            Err(err) => log::error!("acked sequence subtask panicked: {}", err),
                        }
                    }
                    event_subtask_res = &mut event_subtask, if !event_subtask.is_finished() => {
                        match event_subtask_res {
                            Ok((backend_opt, res)) => {
                                if let Err(err) = res {
                                    log::error!("event subtask exited with error: {}", err);
                                }
                                if let Some(backend) = backend_opt {
                                    backend_recovered = Some(backend)
                                }
                            },
                            Err(err) => log::error!("event subtask panicked: {}", err),
                        }
                    }
                    damage_subtask_res = &mut damage_subtask, if !damage_subtask.is_finished() => {
                        match damage_subtask_res {
                            Ok((backend_opt, res)) => {
                                if let Err(err) = res {
                                    log::error!("damage subtask exited with error: {}", err);
                                }
                                if let Some(backend) = backend_opt {
                                    backend_recovered = Some(backend)
                                }
                            },
                            Err(err) => log::error!("damage subtask panicked: {}", err),
                        }
                    }
                    missed_frame_subtask_res = &mut missed_frame_subtask, if !missed_frame_subtask.is_finished() => {
                        match missed_frame_subtask_res {
                            Ok(Ok(())) => (),
                            Ok(Err(err)) => log::error!("acked sequence subtask exited with error: {}", err),
                            Err(err) => log::error!("acked sequence subtask panicked: {}", err),
                        }
                    }
                    fb_update_req_subtask_res = &mut fb_update_req_subtask, if !fb_update_req_subtask.is_finished() => {
                        match fb_update_req_subtask_res {
                            Ok(Ok(())) => (),
                            Ok(Err(err)) => log::error!("fb update req subtask exited with error: {}", err),
                            Err(err) => log::error!("fb update req subtask panicked: {}", err),
                        }
                    }
                    Some((port, enc_res)) = enc_result_futs.next() => {
                        match enc_res {
                            Ok(_) => log::info!("Encoder {} terminated successfully", port),
                            Err(Recovered { error, .. }) => log::info!("Encoder {} terminated with error: {}", port, error),
                        }
                    }
                    _ = self.state.ct.cancelled(), if !has_cancelled_subtasks => {
                        log::info!("backend task cancelled");
                        while let Some((_, ct)) = encoder_cts.pop_first() {
                            ct.cancel();
                        }
                        has_cancelled_subtasks = true;
                    }
                    else => {
                        log::info!("All backend futures exhausted");
                        break Ok::<_, anyhow::Error>(())
                    }
                }
            };
            if backend_recovered.is_none() {
                backend_recovered = Arc::into_inner(backend).map(|lock| lock.into_inner());
            }
            log::trace!("Backend task complete");
            (
                backend_recovered.expect("backend was not recovered"),
                loop_res,
            )
        });

        Ok(Task {
            state: Running { task, ct },
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
        match self.state.task.await {
            Ok((backend, res)) => {
                if let Err(err) = res {
                    log::warn!("backend task returned error: {}", err);
                }
                Ok(Task {
                    state: Terminated { backend },
                })
            }
            Err(err) => panic!("backend task panicked: {}", err),
        }
    }
}

impl<B: Backend + 'static> Transitionable<Terminated<B>> for Task<Running<B>> {
    type NextStateful = Task<Terminated<B>>;

    fn transition(self) -> Self::NextStateful {
        self.stop();
        let backend = match Handle::current().block_on(async move {
            let task = self.state.task;
            task.await
        }) {
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
