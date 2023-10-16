use crate::backend::FramebufferUpdate;

use anyhow::{anyhow, Result};
use aperturec_channel::AsyncSender;
use aperturec_protocol::common_types::*;
use aperturec_protocol::control_messages as cm;
use aperturec_protocol::media_messages as mm;
use aperturec_state_machine::{
    Recovered, SelfTransitionable, State, Stateful, Transitionable, TryTransitionable,
};
use async_stream::stream;
use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use euclid::{Point2D, Size2D, UnknownUnit};
use futures::{Stream, StreamExt, TryStreamExt};
use ndarray::{arr0, s, Array2, ArrayView2, AssignElem, Axis, ShapeBuilder};
use parking_lot::RwLock;
use std::cmp::{max, min};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_stream::wrappers::{ReceiverStream, UnboundedReceiverStream};
use tokio_util::sync::{CancellationToken, PollSender};

pub const RAW_BYTES_PER_PIXEL: usize = 4;

#[derive(Clone, Debug, Default)]
pub struct RawPixel {
    pub blue: u8,
    pub green: u8,
    pub red: u8,
    pub alpha: u8,
}

impl Copy for RawPixel {}

pub type Point = Point2D<usize, UnknownUnit>;
pub type Size = Size2D<usize, UnknownUnit>;
pub type Rect = euclid::Rect<usize, UnknownUnit>;

const X_AXIS: Axis = Axis(0);
const Y_AXIS: Axis = Axis(1);

#[derive(Debug)]
struct MessageEncoder {
    codec: Codec,
    max_bytes_per_message: watch::Receiver<usize>,
}

impl MessageEncoder {
    const DEFAULT_MAX_BYTES_PER_MESSAGE: usize = 500;

    fn with_codec(codec: Codec) -> (Self, watch::Sender<usize>) {
        let (max_bytes_tx, max_bytes_rx) =
            watch::channel(MessageEncoder::DEFAULT_MAX_BYTES_PER_MESSAGE);
        (
            MessageEncoder {
                codec,
                max_bytes_per_message: max_bytes_rx,
            },
            max_bytes_tx,
        )
    }

    fn encode_raw(
        &self,
        max_bytes_per_msg: usize,
        curr_loc: &mut Point,
        raw_data: ArrayView2<RawPixel>,
    ) -> (Bytes, Size, bool) {
        const ENC_BYTES_PER_PIXEL: usize = 3;
        const MAX_ROW_WIDTH_PIXELS: usize = 32;

        impl AssignElem<RawPixel> for &mut &mut [u8] {
            fn assign_elem(self, input: RawPixel) {
                self[0] = input.blue;
                self[1] = input.green;
                self[2] = input.red;
            }
        }

        assert!(
            curr_loc.x < raw_data.len_of(X_AXIS) && curr_loc.y < raw_data.len_of(Y_AXIS),
            "Encoding out of bounds: curr ({:?}), raw dim ({:#?})",
            curr_loc,
            raw_data.raw_dim()
        );
        let num_remaining_cols = raw_data.len_of(X_AXIS) - curr_loc.x;
        let width = min(MAX_ROW_WIDTH_PIXELS, num_remaining_cols);

        let num_remaining_rows = raw_data.len_of(Y_AXIS) - curr_loc.y;
        let height = min(
            max_bytes_per_msg / (ENC_BYTES_PER_PIXEL * MAX_ROW_WIDTH_PIXELS),
            num_remaining_rows,
        );
        if width == 0 || height == 0 {
            log::warn!("Encoding dimension is {}x{}", width, height);
            return (Bytes::new(), Size::new(0, 0), true);
        }

        let mut enc = BytesMut::zeroed(height * width * ENC_BYTES_PER_PIXEL);
        let slice = raw_data.slice(s![
            curr_loc.x..curr_loc.x + width,
            curr_loc.y..curr_loc.y + height
        ]);
        {
            let mut chunks_array = Array2::from_shape_vec(
                (slice.len_of(X_AXIS), slice.len_of(Y_AXIS)).f(),
                enc.chunks_mut(ENC_BYTES_PER_PIXEL).collect(),
            )
            .expect("Create chunks array");
            slice.assign_to(&mut chunks_array);
        }

        curr_loc.x += width;
        if curr_loc.x >= raw_data.len_of(X_AXIS) {
            curr_loc.x = 0;
            curr_loc.y += height;
        }

        (
            enc.freeze(),
            Size::new(width, height),
            curr_loc.y >= raw_data.len_of(Y_AXIS),
        )
    }

    fn encode<'s>(
        &'s self,
        int_raw_data: ArrayView2<'s, RawPixel>,
    ) -> impl Stream<Item = (Bytes, Point, Size, Codec)> + 's {
        let mut curr_loc = Point::new(0, 0);
        let mut is_complete = false;
        let max_bytes_per_msg = *self.max_bytes_per_message.borrow();

        stream! {
            while !is_complete {
                let encoded_loc = curr_loc;
                let (bytes, encoded_dim, finished) = match self.codec {
                    Codec::Raw => self.encode_raw(max_bytes_per_msg, &mut curr_loc, int_raw_data),
                    unsupported_codec => panic!("Unsupported codec {:?}", unsupported_codec),
                };
                is_complete = finished;
                yield (bytes, encoded_loc, encoded_dim, self.codec);
            }
        }
    }
}

#[derive(Stateful, Debug)]
#[state(S)]
pub struct Encoder<S: State> {
    state: S,
}

#[derive(State, Debug)]
pub struct Created<Sender: AsyncSender<Message = mm::ServerToClientMessage>> {
    sender: Sender,
    codec: Codec,
    decoder_area: cm::DecoderArea,
    fb_rx: mpsc::UnboundedReceiver<Arc<FramebufferUpdate>>,
    missed_frame_rx: mpsc::UnboundedReceiver<SequenceId>,
    missed_frame_tx: mpsc::UnboundedSender<SequenceId>,
    acked_frame_rx: mpsc::Receiver<(HeartbeatId, SequenceId)>,
    curr_hb_id_rx: watch::Receiver<HeartbeatId>,
    ct: CancellationToken,
}

impl<Sender: AsyncSender<Message = mm::ServerToClientMessage>> SelfTransitionable
    for Encoder<Created<Sender>>
{
}

#[derive(State, Debug)]
pub struct Running {
    task: JoinHandle<anyhow::Result<()>>,
    ct: CancellationToken,
}

#[derive(State, Debug)]
pub struct Terminated {}

#[derive(Debug)]
pub struct Channels {
    pub acked_frame_tx: mpsc::Sender<(HeartbeatId, SequenceId)>,
    pub fb_tx: mpsc::UnboundedSender<Arc<FramebufferUpdate>>,
    pub missed_frame_tx: mpsc::UnboundedSender<SequenceId>,
}

impl<Sender: AsyncSender<Message = mm::ServerToClientMessage>> Encoder<Created<Sender>> {
    pub fn new(
        decoder_area: cm::DecoderArea,
        sender: Sender,
        codec: Codec,
        curr_hb_id_rx: watch::Receiver<HeartbeatId>,
    ) -> (Self, Channels, CancellationToken) {
        let (fb_tx, fb_rx) = mpsc::unbounded_channel();
        let (missed_frame_tx, missed_frame_rx) = mpsc::unbounded_channel();
        let (acked_frame_tx, acked_frame_rx) = mpsc::channel(1);
        let ct = CancellationToken::new();

        let enc = Encoder {
            state: Created {
                sender,
                codec,
                decoder_area,
                fb_rx,
                missed_frame_rx,
                missed_frame_tx: missed_frame_tx.clone(),
                acked_frame_rx,
                curr_hb_id_rx,
                ct: ct.clone(),
            },
        };
        (
            enc,
            Channels {
                fb_tx,
                missed_frame_tx,
                acked_frame_tx,
            },
            ct,
        )
    }
}

struct CachedFramebuffer {
    rect: Rect,
    data: Array2<RawPixel>,
}

impl CachedFramebuffer {
    fn new(decoder_area: &cm::DecoderArea) -> Self {
        let origin = Point::new(
            decoder_area.location.x_position as usize,
            decoder_area.location.y_position as usize,
        );
        let size = Size::new(
            decoder_area.dimension.width as usize,
            decoder_area.dimension.height as usize,
        );
        let rect = Rect { origin, size };
        CachedFramebuffer {
            rect,
            data: Array2::from_elem((size.width, size.height), RawPixel::default()),
        }
    }

    fn update<'s>(
        &'s mut self,
        fb_update: &FramebufferUpdate,
    ) -> Option<(Point, ArrayView2<'s, RawPixel>)> {
        if let Some((screen_relative_loc, raw_data)) = fb_update.get_intersecting_data(&self.rect) {
            let encoder_relative_loc = (screen_relative_loc - self.rect.origin).to_point();

            let (xmin, xmax) = (
                encoder_relative_loc.x,
                encoder_relative_loc.x + raw_data.len_of(X_AXIS),
            );
            let (ymin, ymax) = (
                encoder_relative_loc.y,
                encoder_relative_loc.y + raw_data.len_of(Y_AXIS),
            );
            let mut dst = self.data.slice_mut(s![xmin..xmax, ymin..ymax]);
            dst.assign(&raw_data);
            Some((
                encoder_relative_loc,
                self.data.slice(s![xmin..xmax, ymin..ymax]),
            ))
        } else {
            None
        }
    }

    fn get_raw<'s>(&'s self, loc: &Point, dim: &Size) -> ArrayView2<'s, RawPixel> {
        assert!(
            (loc.x + dim.width) <= self.rect.size.width
                && (loc.y + dim.height) <= self.rect.size.height,
            "Point out of bounds: ({:?}, {:?}) / ({:?}, {:?})",
            loc,
            dim,
            self.rect.origin,
            self.rect.size
        );

        self.data
            .slice(s![loc.x..loc.x + dim.width, loc.y..loc.y + dim.height])
    }
}

#[async_trait]
impl<Sender: AsyncSender<Message = mm::ServerToClientMessage>>
    TryTransitionable<Running, Created<Sender>> for Encoder<Created<Sender>>
{
    type SuccessStateful = Encoder<Running>;
    type FailureStateful = Encoder<Created<Sender>>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let (msg_encoder, max_size_tx) = MessageEncoder::with_codec(self.state.codec);
        const SUCCESSFUL_SEND_INCREASE_SIZE_THRESHOLD: usize = 50;
        const SEND_SIZE_INCREMENT: usize = 10;
        const SEND_SIZE_DECREMENT: usize = 100;
        const SEND_SIZE_MAX: usize = 1400;
        const SEND_SIZE_MIN: usize = 100;

        let mut sender = self.state.sender;
        let (dispatch_tx, dispatch_rx) = mpsc::channel(1);
        let ct = self.state.ct.clone();
        let mut dispatch_subtask: JoinHandle<Result<()>> = tokio::spawn(async move {
            let mut dispatch_stream = ReceiverStream::new(dispatch_rx).fuse();
            let mut num_successful_sends = 0;
            loop {
                tokio::select! {
                    biased;
                    _ = ct.cancelled() => {
                        log::info!("Dispatch task cancelled");
                        break Ok(());
                    }
                    Some(msg) = dispatch_stream.next() => {
                        if let Err(e) = sender.send(msg).await {
                            match e.downcast::<std::io::Error>() {
                                Ok(io_err) => {
                                    num_successful_sends = 0;
                                    if io_err.raw_os_error() == Some(90) {
                                        max_size_tx.send_modify(|max_size| {
                                            let old = *max_size;
                                            if old >= SEND_SIZE_MIN + SEND_SIZE_DECREMENT {
                                                *max_size -= SEND_SIZE_DECREMENT;
                                            }
                                        })
                                    } else {
                                        log::warn!("Dispatch task has IO error {}", io_err);
                                        break Err(io_err.into());
                                    }
                                }
                                Err(e) => {
                                    log::warn!("Dispatch task failed to send message with error {}", e);
                                    break Err(e);
                                }
                            }
                        } else {
                            num_successful_sends += 1;
                            if num_successful_sends > SUCCESSFUL_SEND_INCREASE_SIZE_THRESHOLD {
                                max_size_tx.send_modify(|max_size| {
                                    let old = *max_size;
                                    if old <= SEND_SIZE_MAX - SEND_SIZE_INCREMENT {
                                        *max_size += SEND_SIZE_INCREMENT;
                                    }
                                });
                                num_successful_sends = 0;
                            }
                        }
                    }
                    else => break Err(anyhow!("All futures exhaused in dispatch task"))
                }
            }
        });

        let decoder_area = self.state.decoder_area.clone();
        let responsible_hb_ids = Arc::new(RwLock::new(BTreeMap::new()));
        let responsible_hb_ids_encoder = responsible_hb_ids.clone();

        let mut latest_updates = Array2::<u64>::zeros((
            self.state.decoder_area.dimension.width as usize,
            self.state.decoder_area.dimension.height as usize,
        ));
        let ct = self.state.ct.clone();
        let in_flight = Arc::new(RwLock::new(BTreeMap::<u64, (Point, Size)>::new()));
        let in_flight_encoding = in_flight.clone();
        let mut fb_stream = UnboundedReceiverStream::new(self.state.fb_rx).fuse();
        let mut missed_frame_stream =
            UnboundedReceiverStream::new(self.state.missed_frame_rx).fuse();
        let mut frame_encoding_subtask: JoinHandle<Result<(), anyhow::Error>> = tokio::spawn(
            async move {
                let mut cached_fb = CachedFramebuffer::new(&decoder_area);
                let sequence_no = AtomicU64::new(0);

                let new_frame_sink = PollSender::new(dispatch_tx.clone());
                loop {
                    let (encoder_relative_loc, raw_data) = tokio::select! {
                        biased;
                        _ = ct.cancelled() => {
                            log::info!("frame enoding subtask cancelled");
                            return Ok(());
                        }
                        Some(missed_frame) = missed_frame_stream.next() => {
                            if missed_frame.0 > sequence_no.load(Ordering::Relaxed) {
                                log::warn!("MFR for invalid sequence no: {}", missed_frame.0);
                                continue;
                            }

                            let missed = match in_flight_encoding.write().get(&missed_frame.0) {
                                Some((loc, dim)) => {
                                    let shape = s![loc.x..loc.x+dim.width, loc.y..loc.y+dim.height];
                                    if latest_updates.slice(shape).iter().any(|seq| seq == &missed_frame.0) {
                                        Some((*loc, *dim))
                                    } else {
                                        None
                                    }
                                },
                                None => {
                                    log::warn!("ACK: Reported missed frame for untracked sequence ID {:?}", missed_frame);
                                    Some((Point::new(0, 0),
                                        Size::new(
                                            decoder_area.dimension.width as usize,
                                            decoder_area.dimension.height as usize
                                        )
                                    ))
                                }
                            };
                            if let Some((origin, size)) = missed {
                                (origin, cached_fb.get_raw(&origin, &size))
                            } else {
                                continue;
                            }
                        }
                        Some(fb_update) = fb_stream.next() => {
                            match cached_fb.update(&fb_update) {
                                Some((encoder_relative, int_raw_data)) => (encoder_relative, int_raw_data),
                                None => continue,
                            }
                        }
                        else => return Err(anyhow!("fb update stream exhausted"))
                    };

                    msg_encoder
                        .encode(raw_data)
                        .map(|(bytes, fb_update_loc, dim, codec)| {
                            let seq_id = sequence_no.fetch_add(1, Ordering::Relaxed);
                            let loc = encoder_relative_loc + fb_update_loc.to_vector();
                            let msg = mm::ServerToClientMessage::new_framebuffer_update(
                                mm::FramebufferUpdate::new(vec![mm::RectangleUpdate::new(
                                    SequenceId::new(seq_id),
                                    Location::new(loc.x as u64, loc.y as u64),
                                    mm::Rectangle::new(
                                        codec,
                                        bytes,
                                        Some(Dimension::new(dim.width as u64, dim.height as u64)),
                                    ),
                                )]),
                            );
                            Ok((msg, seq_id, loc, dim))
                        })
                        .inspect_ok(|(_, seq_id, loc, dim)| {
                            let shape = s![loc.x..loc.x + dim.width, loc.y..loc.y + dim.height];
                            latest_updates.slice_mut(shape).assign(&arr0(*seq_id));
                            in_flight_encoding.write().insert(*seq_id, (*loc, *dim));

                            let responsible_hb_id = self.state.curr_hb_id_rx.borrow().0;
                            responsible_hb_ids_encoder
                                .write()
                                .entry(responsible_hb_id)
                                .and_modify(|set: &mut BTreeSet<_>| {
                                    set.insert(*seq_id);
                                })
                                .or_insert(BTreeSet::from([*seq_id]));
                        })
                        .map_ok(|(msg, _, _, _)| msg)
                        .into_stream()
                        .forward(new_frame_sink.clone())
                        .await?;
                }
            },
        );

        let ct = self.state.ct.clone();
        let task = tokio::spawn(async move {
            let mut acked_frame_stream = ReceiverStream::new(self.state.acked_frame_rx).fuse();
            loop {
                tokio::select! {
                    biased;
                    _ = ct.cancelled() => {
                        log::info!("Encoder cancelled");
                        return Ok(());
                    }
                    Some((hb_id, acked_frame_seq)) = acked_frame_stream.next() => {
                        const SEQ_RETAIN_LAG: u64 = 100000;
                        if let Some(mut should_be_acked) = responsible_hb_ids.write().remove(&hb_id.0) {
                            if let Some(greatest_seq) = should_be_acked.last() {
                                if greatest_seq < &acked_frame_seq.0 {
                                    log::trace!("ACK: {} frames unacked", acked_frame_seq.0 - greatest_seq);
                                }
                            }
                            should_be_acked.retain(|seq_id| seq_id > &acked_frame_seq.0);
                            let needs_mfrs = should_be_acked;

                            for seq_id in needs_mfrs {
                                self.state.missed_frame_tx.send(SequenceId(seq_id))?;
                            }
                        }

                        in_flight.write().retain(|seq_k, _| seq_k > &(max(acked_frame_seq.0, SEQ_RETAIN_LAG) - SEQ_RETAIN_LAG));
                    }
                    dispatch_subtask_res = &mut dispatch_subtask, if !dispatch_subtask.is_finished() => {
                        match dispatch_subtask_res {
                            Ok(Ok(())) => {
                                log::info!("Dispatch task exited successfully");
                                break Ok(());
                            },
                            Ok(Err(err)) => {
                                log::warn!("Dispatch task exited with error '{}'", err);
                                break Err(err);
                            }
                            Err(err) => {
                                log::warn!("Dispatch task panicked: '{}'", err);
                                break Err(err.into());
                            }
                        }
                    }
                    frame_encoding_subtask_res = &mut frame_encoding_subtask, if !frame_encoding_subtask.is_finished() => {
                        match frame_encoding_subtask_res {
                            Ok(Ok(())) => {
                                log::info!("Frame encoding subtask exited");
                                break Ok(());
                            },
                            Ok(Err(err)) => {
                                log::warn!("Frame encoding subtask exited with error: {}", err);
                                break Err(err);
                            }
                            Err(err) => {
                                log::warn!("Frame encoding subtask panicked: {}", err);
                                break Err(err.into());
                            }
                        }
                    }
                    else => break Err(anyhow!("All futures exhaused in encoder task"))
                }
            }
        });

        Ok(Encoder {
            state: Running {
                task,
                ct: self.state.ct,
            },
        })
    }
}

impl Encoder<Running> {
    pub fn stop(&self) {
        self.state.ct.cancel();
    }
}

#[async_trait]
impl TryTransitionable<Terminated, Terminated> for Encoder<Running> {
    type SuccessStateful = Encoder<Terminated>;
    type FailureStateful = Encoder<Terminated>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let res_res = self.state.task.await;
        let stateful = Encoder {
            state: Terminated {},
        };
        match res_res {
            Ok(Ok(())) => {
                log::info!("Encoder exited successfully");
                Ok(stateful)
            }
            Ok(Err(e)) => Err(Recovered {
                stateful,
                error: anyhow!("Encoder exited with internal error {}", e),
            }),
            Err(e) => Err(Recovered {
                stateful,
                error: anyhow!("Encoder exited with joining error {}", e),
            }),
        }
    }
}

impl Transitionable<Terminated> for Encoder<Running> {
    type NextStateful = Encoder<Terminated>;

    fn transition(self) -> Self::NextStateful {
        Encoder {
            state: Terminated {},
        }
    }
}
