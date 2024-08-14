use crate::backend::{Backend, Event, SwapableBackend};
use crate::task::encoder::{self, Encoder};
use crate::task::event_channel_handler::EVENT_QUEUE;

use aperturec_protocol::common::*;
use aperturec_protocol::event::{self as em, server_to_client as em_s2c};
use aperturec_protocol::{control as cm, media as mm};
use aperturec_state_machine::*;
use aperturec_trace::log;
use aperturec_trace::queue::deq;

use anyhow::{anyhow, Result};
use futures::stream::FuturesUnordered;
use futures::{FutureExt, StreamExt};
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
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

#[derive(State)]
pub struct Created<B: Backend + 'static> {
    backend: SwapableBackend<B>,
    decoders: BTreeMap<u32, (Location, Dimension, Codec)>,
    acked_seq_rx: mpsc::UnboundedReceiver<cm::DecoderSequencePair>,
    missed_frame_rx: mpsc::UnboundedReceiver<cm::MissedFrameReport>,
    event_rx: mpsc::UnboundedReceiver<Event>,
    cursor_tx: mpsc::Sender<em_s2c::Message>,
    fbu_tx: mpsc::Sender<Vec<mm::FramebufferUpdate>>,
    synthetic_missed_frame_rxs: BTreeMap<u32, mpsc::UnboundedReceiver<u64>>,
}

impl<B: Backend + 'static> Task<Created<B>> {
    pub fn into_backend(self) -> SwapableBackend<B> {
        self.state.backend
    }
}

#[derive(State, Debug)]
pub struct Running<B: Backend + 'static> {
    acked_seq_subtask: JoinHandle<Result<()>>,
    acked_seq_subtask_ct: CancellationToken,
    backend_subtask: JoinHandle<(SwapableBackend<B>, Result<()>)>,
    backend_subtask_ct: CancellationToken,
    missed_frame_subtask: JoinHandle<Result<()>>,
    missed_frame_subtask_ct: CancellationToken,
    encoder_subtasks: BTreeMap<
        u32,
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
    encoder_subtask_cts: BTreeMap<u32, CancellationToken>,
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
    backend: SwapableBackend<B>,
}

impl<B: Backend + 'static> Task<Terminated<B>> {
    pub fn into_backend(self) -> SwapableBackend<B> {
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
    pub fn new(
        backend: SwapableBackend<B>,
        decoders: BTreeMap<u32, (Location, Dimension, Codec)>,
        missed_frame_rx: mpsc::UnboundedReceiver<cm::MissedFrameReport>,
        event_rx: mpsc::UnboundedReceiver<Event>,
        cursor_tx: mpsc::Sender<em_s2c::Message>,
        fbu_tx: mpsc::Sender<Vec<mm::FramebufferUpdate>>,
        synthetic_missed_frame_rxs: BTreeMap<u32, mpsc::UnboundedReceiver<u64>>,
    ) -> (Task<Created<B>>, Channels) {
        let (acked_seq_tx, acked_seq_rx) = mpsc::unbounded_channel();

        let task = Task {
            state: Created {
                backend,
                decoders,
                acked_seq_rx,
                missed_frame_rx,
                event_rx,
                cursor_tx,
                fbu_tx,
                synthetic_missed_frame_rxs,
            },
        };

        (task, Channels { acked_seq_tx })
    }
}

impl<B: Backend + 'static> AsyncTryTransitionable<Running<B>, Created<B>> for Task<Created<B>> {
    type SuccessStateful = Task<Running<B>>;
    type FailureStateful = Task<Created<B>>;
    type Error = anyhow::Error;

    async fn try_transition(
        mut self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let mut acked_frame_txs = BTreeMap::new();
        let mut fb_txs = BTreeMap::new();
        let mut missed_frame_txs = BTreeMap::new();
        let mut encoder_subtasks = BTreeMap::new();
        let mut encoder_subtask_cts = BTreeMap::new();

        for (id, (loc, dim, codec)) in self.state.decoders {
            let synthetic_missed_frame_rx = self
                .state
                .synthetic_missed_frame_rxs
                .remove(&id)
                .expect("No synthetic MFR channel for encoder");
            let (enc, enc_channels) = Encoder::new(
                id,
                &loc,
                &dim,
                codec,
                self.state.fbu_tx.clone(),
                synthetic_missed_frame_rx,
            );
            let ct = enc.cancellation_token().clone();
            let subtask: JoinHandle<Result<_, _>> = tokio::spawn(async move {
                let encoder: Encoder<encoder::Running> =
                    match try_transition_async!(enc, encoder::Running) {
                        Ok(enc) => enc,
                        Err(recovered) => {
                            return_recover!(
                                recovered.stateful,
                                "[Encoder {}] Failed to start: {}",
                                id,
                                recovered.error
                            );
                        }
                    };
                try_transition_async!(encoder, encoder::Terminated)
            });
            encoder_subtasks.insert(id, subtask);
            encoder_subtask_cts.insert(id, ct);
            acked_frame_txs.insert(id, enc_channels.acked_frame_tx);
            fb_txs.insert(id, enc_channels.fb_tx);
            missed_frame_txs.insert(id, enc_channels.missed_frame_tx);
        }

        let mut event_stream = UnboundedReceiverStream::new(self.state.event_rx).fuse();

        let acked_seq_subtask_ct = CancellationToken::new();
        let ct = acked_seq_subtask_ct.clone();
        let acked_seq_subtask = tokio::spawn(async move {
            let mut acked_seq_stream = UnboundedReceiverStream::new(self.state.acked_seq_rx).fuse();
            loop {
                tokio::select! {
                    biased;
                    _ = ct.cancelled() => {
                        log::info!("Acked sequence subtask cancelled");
                        break Ok(())
                    }
                    Some(acked_seq_pair) = acked_seq_stream.next() => {
                        match acked_frame_txs.get(&acked_seq_pair.decoder_id) {
                            Some(channel) => channel.send(acked_seq_pair.sequence_id).await?,
                            None => log::warn!("Received HB for untracked decoder {}", acked_seq_pair.decoder_id),
                        }
                    }
                    else => break Err(anyhow!("Acked sequence stream exhausted")),
                }
            }
        });

        let backend_subtask_ct = CancellationToken::new();
        let ct = backend_subtask_ct.clone();
        let backend_subtask = tokio::spawn(async move {
            let mut cursor_cache = HashMap::new();
            let mut last_cursor_id = 0;
            let mut damage_stream = match self.state.backend.damage_stream().await {
                Ok(damage_stream) => damage_stream,
                Err(e) => {
                    log::error!("Failed getting damage stream from backend: {}", e);
                    return (self.state.backend, Err(e));
                }
            };
            let mut cursor_stream = match self.state.backend.cursor_stream().await {
                Ok(cursor_stream) => cursor_stream,
                Err(e) => {
                    log::error!("Failed getting cursor stream from backend: {}", e);
                    return (self.state.backend, Err(e));
                }
            };
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
                    Some(cursor_change) = cursor_stream.next() => {
                        let (id, msg) = match cursor_cache.get(&cursor_change.serial) {
                            Some(id) => (*id, em_s2c::Message::CursorChange(em::CursorChange { id: *id })),
                            None => 'msg: {
                                match self.state.backend.capture_cursor().await {
                                    Err(err) => {
                                        log::error!("Failed to capture cursor: {}", err);
                                        break 'select Err(err);
                                    },
                                    Ok(cursor_image) => {

                                        //
                                        // Captured cursor may not match the original cursor change
                                        // notification. Generate CursorChange if the captured
                                        // cursor has already been sent.
                                        //
                                        if cursor_change.serial != cursor_image.serial {
                                            log::trace!(
                                                "Captured cursor serial does not match originating serial: {} != {}",
                                                cursor_image.serial,
                                                cursor_change.serial
                                            );
                                            if let Some(id) = cursor_cache.get(&cursor_image.serial) {
                                                break 'msg (*id, em_s2c::Message::CursorChange(em::CursorChange { id: *id }));
                                            }
                                        }

                                        //
                                        // Hash cursor data to generate a unique ID
                                        //
                                        let mut hasher = DefaultHasher::new();
                                        cursor_image.hash(&mut hasher);
                                        let id = hasher.finish();

                                        //
                                        // Generate CursorChange if ID is not unique
                                        //
                                        if cursor_cache.values().any(|&value| value == id) {
                                            log::trace!("Cursor {} already exists in cache", id);
                                            cursor_cache.insert(cursor_image.serial, id);
                                            break 'msg (id, em_s2c::Message::CursorChange(em::CursorChange { id }));
                                        }

                                        cursor_cache.insert(cursor_image.serial, id);

                                        (id, em_s2c::Message::CursorImage(em::CursorImage {
                                            id,
                                            width: cursor_image.width.into(),
                                            height: cursor_image.height.into(),
                                            x_hot: cursor_image.x_hot.into(),
                                            y_hot: cursor_image.y_hot.into(),
                                            data: cursor_image.data
                                        }))
                                    }
                                }
                            }
                        };

                        if id != last_cursor_id {
                            if let Err(err) = self.state.cursor_tx.send(msg).await {
                                break 'select Err(err.into())
                            }
                            last_cursor_id = id;
                        }
                    }
                    else => break Err(anyhow!("X event streams exhausted"))
                }
            };
            (self.state.backend, loop_res)
        });

        let missed_frame_subtask_ct = CancellationToken::new();
        let ct = missed_frame_subtask_ct.clone();
        let missed_frame_subtask = tokio::spawn(async move {
            let mut mfr_stream = UnboundedReceiverStream::new(self.state.missed_frame_rx).fuse();
            loop {
                tokio::select! {
                    biased;
                    _ = ct.cancelled() => {
                        log::info!("Missed frame subtask cancelled");
                        break Ok(());
                    }
                    Some(mfr) = mfr_stream.next() => {
                        match missed_frame_txs.get(&mfr.decoder_id) {
                            Some(channel) => {
                                for frame in &mfr.frame_sequence_ids {
                                    channel.send(*frame).await?;
                                }
                            },
                            None => log::warn!("Received missed frame report for invalid decoder {}", mfr.decoder_id),
                        }
                    }
                    else => break Err(anyhow!("Exhausted missed frame report stream"))
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
                encoder_subtasks,
                encoder_subtask_cts,
            },
        })
    }
}

impl<B: Backend + 'static> AsyncTryTransitionable<Terminated<B>, Terminated<B>>
    for Task<Running<B>>
{
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
        let (mut acked_seq_cleanedup, mut backend_cleanedup, mut missed_frame_cleanedup) =
            (false, false, false);

        let loop_res = loop {
            tokio::select! {
                _ = self.state.ct.cancelled(), if !top_level_cleanup_executed => {
                    log::debug!("backend task cancelled");
                    top_level_cleanup_executed = true;
                    self.state.acked_seq_subtask_ct.cancel();
                    self.state.backend_subtask_ct.cancel();
                    self.state.missed_frame_subtask_ct.cancel();
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

impl<B: Backend + 'static> AsyncTransitionable<Terminated<B>> for Task<Running<B>> {
    type NextStateful = Task<Terminated<B>>;

    async fn transition(self) -> Self::NextStateful {
        self.stop();
        let backend = match Handle::current().block_on(self.state.backend_subtask) {
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
