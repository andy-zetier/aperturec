use crate::backend::Backend;
use crate::metrics::{
    FramesCut, TrackingBufferDamageRatio, TrackingBufferDisjointAreas, TrackingBufferUpdates,
};

use aperturec_graphics::{prelude::*, rectangle_cover::diff_rectangle_cover};
use aperturec_state_machine::*;

use anyhow::{anyhow, bail, Result};
use futures::{self, future, TryFutureExt};
use ndarray::{prelude::*, AssignElem};
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::*;

#[derive(Debug)]
pub struct SubframeBuffer<M: PixelMap> {
    pub area: Box2D,
    pub pixels: M,
}

#[derive(Debug)]
pub struct Frame {
    pub id: usize,
    pub buffers: Vec<SubframeBuffer<Pixel24Map>>,
}

#[derive(Debug)]
pub struct NewResolution {
    pub resolution: Size,
    pub notify_complete_tx: oneshot::Sender<()>,
}

#[derive(Stateful, SelfTransitionable)]
#[state(S)]
pub struct Task<S: State> {
    state: S,
}

#[derive(State)]
pub struct Created<B: Backend> {
    initial_resolution: Size,
    frame_txs: Vec<mpsc::Sender<Arc<Frame>>>,
    damage_rx: mpsc::UnboundedReceiver<SubframeBuffer<B::PixelMap>>,
    resolution_rx: mpsc::Receiver<NewResolution>,
}

#[derive(State, Debug)]
pub struct Running {
    ct: CancellationToken,
    tasks: JoinSet<Result<()>>,
}

#[derive(State, Debug)]
pub struct Terminated;

#[derive(Debug)]
pub struct Channels<B: Backend> {
    pub frame_rxs: Vec<mpsc::Receiver<Arc<Frame>>>,
    pub damage_tx: mpsc::UnboundedSender<SubframeBuffer<B::PixelMap>>,
    pub resolution_tx: mpsc::Sender<NewResolution>,
}

impl<B: Backend> Task<Created<B>> {
    pub fn new(initial_resolution: Size, num_encoders: usize) -> (Self, Channels<B>) {
        let (frame_txs, frame_rxs) = (0..num_encoders)
            .map(|_| {
                let (tx, rx) = mpsc::channel(1);
                (tx, rx)
            })
            .collect();
        let (damage_tx, damage_rx) = mpsc::unbounded_channel();
        let (resolution_tx, resolution_rx) = mpsc::channel(1);

        (
            Task {
                state: Created {
                    initial_resolution,
                    frame_txs,
                    damage_rx,
                    resolution_rx,
                },
            },
            Channels {
                frame_rxs,
                damage_tx,
                resolution_tx,
            },
        )
    }
}

impl Task<Running> {
    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.state.ct
    }
}

impl<B: Backend> Transitionable<Running> for Task<Created<B>>
where
    for<'p> &'p mut Pixel24: AssignElem<<B::PixelMap as PixelMap>::Pixel>,
{
    type NextStateful = Task<Running>;

    fn transition(mut self) -> Self::NextStateful {
        let mut js = JoinSet::new();
        let curr_rt = Handle::current();

        let tb_size = self.state.initial_resolution;
        let tracking_buffer = Arc::new(Mutex::new(TrackingBuffer::new(tb_size)));

        {
            let tracking_buffer = tracking_buffer.clone();
            js.spawn_on(
                async move {
                    while let Some(nr) = self.state.resolution_rx.recv().await {
                        tracking_buffer.lock().unwrap().resize(nr.resolution);
                        nr.notify_complete_tx.send(()).expect("Failed send");
                    }
                    bail!("failed resolution recv")
                },
                &curr_rt,
            );
        }

        {
            let tracking_buffer = tracking_buffer.clone();
            js.spawn_on(
                async move {
                    while let Some(framebuffer_data) = self.state.damage_rx.recv().await {
                        tracking_buffer
                            .lock()
                            .expect("tracking_buffer poisoned")
                            .update(framebuffer_data);
                    }
                    bail!("damage stream exhausted")
                },
                &curr_rt,
            );
        }

        {
            let tracking_buffer = tracking_buffer.clone();
            let mut damage_handle = tracking_buffer
                .lock()
                .expect("tracking_buffer poisoned")
                .damage_handle();

            js.spawn_on(
                async move {
                    loop {
                        trace!(
                            "Waiting for {} permits and damage",
                            self.state.frame_txs.len()
                        );
                        let (permits, _) = future::try_join(
                            future::try_join_all(
                                self.state.frame_txs.iter().map(|tx| tx.reserve()),
                            )
                            .map_err(anyhow::Error::new),
                            damage_handle.wait_for_damage(),
                        )
                        .await?;
                        trace!("Received permits and damage!");

                        let frame = tracking_buffer
                            .lock()
                            .expect("tracking_buffer poisoned")
                            .cut_frame();

                        if let Some(frame) = frame {
                            let frame = Arc::new(frame);
                            for permit in permits {
                                permit.send(frame.clone());
                            }
                        }
                    }
                },
                &curr_rt,
            );
        }

        Task {
            state: Running {
                tasks: js,
                ct: CancellationToken::new(),
            },
        }
    }
}

impl<B: Backend> Transitionable<Terminated> for Task<Created<B>> {
    type NextStateful = Task<Terminated>;

    fn transition(self) -> Self::NextStateful {
        Task { state: Terminated }
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
                        Ok(Ok(())) => anyhow!("frame sync task exited without internal error"),
                        Ok(Err(e)) => anyhow!("frame sync task exited with internal error: {}", e),
                        Err(e) => anyhow!("frame sync task exited with panic: {}", e),
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
                error: anyhow!("frame sync task errors: {:?}", errors),
            })
        }
    }
}

#[derive(Debug)]
enum Damage {
    None,
    Partial(BoxSet),
    Full,
}

/// A `TrackingBuffer` is used to manage a 2D array of pixels, efficiently tracking changes and damaged areas.
///
/// The buffer consists of:
/// - A 2D array of pixel data (`data`).
/// - A set of boxes representing damaged areas (`damaged_areas`).
/// - A boolean indicating if the entire buffer is damaged (`is_fully_damaged`).
///
/// This structure is particularly useful in graphical applications where partial updates to the pixel data are common.
pub struct TrackingBuffer<BackendPixelMap: PixelMap> {
    size: Size,
    data: Array2<Pixel24>,
    damage: Damage,
    curr_frame_id: usize,
    damage_tx: watch::Sender<()>,
    _pixel_map: PhantomData<BackendPixelMap>,
}

impl<BackendPixelMap> TrackingBuffer<BackendPixelMap>
where
    BackendPixelMap: PixelMap,
    for<'p> &'p mut Pixel24: AssignElem<BackendPixelMap::Pixel>,
{
    pub fn new(resolution: Size) -> Self {
        let (damage_tx, _) = watch::channel(());
        let shape = (resolution.height, resolution.width);
        TrackingBuffer {
            size: resolution,
            data: Array2::from_elem(shape, Pixel24::default()),
            damage: Damage::Full,
            curr_frame_id: 0,
            damage_tx,
            _pixel_map: PhantomData,
        }
    }

    fn resize(&mut self, resolution: Size) {
        let shape = (resolution.height, resolution.width);
        self.data = Array2::from_elem(shape, Pixel24::default());
        self.size = resolution;
        self.clear();
    }

    pub fn update(&mut self, framebuffer_data: SubframeBuffer<BackendPixelMap>) {
        TrackingBufferUpdates::inc();
        // Early exit if the origin point is outside the bounds of the current data array.
        let intersection = match Box2D::from_size(self.size).intersection(&framebuffer_data.area) {
            Some(intersection) => intersection,
            None => return,
        };
        let fb_relative_area = intersection
            .to_i64()
            .translate(-framebuffer_data.area.min.to_vector().to_i64())
            .to_usize();

        let pixmap = framebuffer_data.pixels.as_ndarray();
        let new = pixmap.slice(&fb_relative_area.as_slice());
        tokio::task::block_in_place(|| match self.damage {
            Damage::Full => {
                new.assign_to(self.data.slice_mut(intersection.as_slice()));
                TrackingBufferDamageRatio::observe(1.);
            }
            _ => {
                let curr = self.data.slice(intersection.as_slice());
                let cover_areas = diff_rectangle_cover(curr, new);

                let mut total_area = 0;
                for b in &cover_areas {
                    let tb_relative_rect = b.translate(intersection.min.to_vector());

                    framebuffer_data
                        .pixels
                        .as_ndarray()
                        .slice(b.as_slice())
                        .assign_to(self.data.slice_mut(tb_relative_rect.as_slice()));

                    total_area += b.area();
                    self.mark_stale(tb_relative_rect);
                }
                TrackingBufferDamageRatio::observe(
                    total_area as f64 / intersection.area() as f64 * 100.,
                );
            }
        });
    }

    fn mark_stale(&mut self, stale: Box2D) {
        let intersection = match Box2D::from_size(self.size).intersection(&stale) {
            Some(intersection) => intersection,
            None => return,
        };

        if intersection.area() == self.size.area() {
            self.damage = Damage::Full;
        } else {
            match self.damage {
                Damage::None => self.damage = Damage::Partial(BoxSet::with_initial_box(stale)),
                Damage::Partial(ref mut areas) => areas.add(stale),
                Damage::Full => (),
            }
        }

        let _ = self.damage_tx.send(());
    }

    fn clear(&mut self) {
        self.damage = Damage::None;
    }

    pub fn cut_frame(&mut self) -> Option<Frame> {
        let buffers = match self.damage {
            Damage::None => return None,
            Damage::Partial(ref boxes) => boxes
                .iter()
                .map(|b| {
                    // SAFETY: We initialize the Array2 in the call to `build_init`, so
                    // `assume_init` is safe to call
                    let pixels = unsafe {
                        Array2::build_uninit(b.as_shape(), |dst| {
                            self.data.slice(b.as_slice()).assign_to(dst)
                        })
                        .assume_init()
                    };
                    SubframeBuffer { area: *b, pixels }
                })
                .collect(),
            Damage::Full => {
                // SAFETY: We initialize the Array2 in the call to `build_init`, so
                // `assume_init` is safe to call
                let pixels = unsafe {
                    Array2::build_uninit(self.size.as_shape(), |dst| self.data.assign_to(dst))
                        .assume_init()
                };
                vec![SubframeBuffer {
                    area: Box2D::from_size(self.size),
                    pixels,
                }]
            }
        };

        self.clear();
        let id = self.curr_frame_id;
        self.curr_frame_id += 1;

        FramesCut::inc();
        TrackingBufferDisjointAreas::inc_by(buffers.len() as f64);
        Some(Frame { id, buffers })
    }

    fn damage_handle(&self) -> DamageHandle {
        let mut watch = self.damage_tx.subscribe();

        // A new damage handle always reports there is immediate damage
        watch.mark_changed();
        DamageHandle { watch }
    }
}

struct DamageHandle {
    watch: watch::Receiver<()>,
}

impl DamageHandle {
    async fn wait_for_damage(&mut self) -> Result<()> {
        Ok(self.watch.changed().await?)
    }
}
