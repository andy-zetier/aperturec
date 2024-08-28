use crate::backend::Backend;
use crate::metrics::TrackingBufferDamageRatio;

use aperturec_graphics::prelude::*;
use aperturec_state_machine::*;
use aperturec_trace::log;

use anyhow::{anyhow, bail, Result};
use futures::{self, future, TryFutureExt};
use ndarray::{prelude::*, AssignElem};
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};
use tokio::runtime::Handle;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

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

#[derive(Stateful, SelfTransitionable)]
#[state(S)]
pub struct Task<S: State> {
    state: S,
}

#[derive(State)]
pub struct Created<B: Backend> {
    resolution: Size,
    frame_txs: Vec<mpsc::Sender<Arc<Frame>>>,
    damage_rx: mpsc::UnboundedReceiver<SubframeBuffer<B::PixelMap>>,
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
}

impl<B: Backend> Task<Created<B>> {
    pub fn new(resolution: Size, num_encoders: usize) -> (Self, Channels<B>) {
        let (frame_txs, frame_rxs) = (0..num_encoders)
            .map(|_| {
                let (tx, rx) = mpsc::channel(1);
                (tx, rx)
            })
            .collect();
        let (damage_tx, damage_rx) = mpsc::unbounded_channel();

        (
            Task {
                state: Created {
                    resolution,
                    frame_txs,
                    damage_rx,
                },
            },
            Channels {
                frame_rxs,
                damage_tx,
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

        let tracking_buffer = Arc::new(Mutex::new(TrackingBuffer::new(self.state.resolution)));

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
                        log::trace!(
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
                        log::trace!("Received permits and damage!");

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
struct TrackingBuffer<BackendPixelMap: PixelMap> {
    area: Box2D,
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
    fn new(resolution: Size) -> Self {
        let (damage_tx, _) = watch::channel(());
        let shape = (resolution.height, resolution.width);
        TrackingBuffer {
            area: Rect::new(Point::new(0, 0), resolution).to_box2d(),
            data: Array2::from_elem(shape, Pixel24::default()),
            damage: Damage::Full,
            curr_frame_id: 0,
            damage_tx,
            _pixel_map: PhantomData,
        }
    }

    fn update(&mut self, framebuffer_data: SubframeBuffer<BackendPixelMap>) {
        // Early exit if the origin point is outside the bounds of the current data array.
        let intersection = match self.area.intersection(&framebuffer_data.area) {
            Some(intersection) => intersection,
            None => return,
        };

        let tb_relative_area = intersection
            .to_i64()
            .translate(-self.area.min.to_vector().to_i64())
            .to_usize();
        let fb_relative_area = intersection
            .to_i64()
            .translate(-framebuffer_data.area.min.to_vector().to_i64())
            .to_usize();

        let pixmap = framebuffer_data.pixels.as_ndarray();
        let new = pixmap.slice(&fb_relative_area.as_slice());
        tokio::task::block_in_place(|| match self.damage {
            Damage::Full => {
                new.assign_to(self.data.slice_mut(tb_relative_area.as_slice()));
                TrackingBufferDamageRatio::observe(1.);
            }
            _ => {
                let curr = self.data.slice(tb_relative_area.as_slice());
                let row_iter = curr.lanes(axis::X).into_iter().zip(new.lanes(axis::X));

                let top = row_iter.clone().position(|(curr, new)| new != curr);

                let (bottom, left, right) = if top.is_some() {
                    let bottom = row_iter
                        .collect::<Vec<_>>()
                        .iter()
                        .rposition(|(curr, new)| new != curr);

                    let cols = curr
                        .lanes(axis::Y)
                        .into_iter()
                        .zip(new.lanes(axis::Y))
                        .collect::<Vec<_>>();
                    let left = cols.iter().position(|(curr, new)| new != curr);
                    let right = cols.iter().rposition(|(curr, new)| new != curr);

                    (bottom, left, right)
                } else {
                    (None, None, None)
                };

                let boundaries = match (left, right, top, bottom) {
                    (Some(l), Some(r), Some(t), Some(b)) => Some((l, r, t, b)),
                    (None, None, None, None) => None,
                    _ => unreachable!("bounds not in sync"),
                };

                if let Some((l, r, t, b)) = boundaries {
                    let min_intersection = Box2D::new(Point::new(l, t), Point::new(r + 1, b + 1))
                        .translate(intersection.min.to_vector());
                    let min_tb_relative_area = min_intersection
                        .to_i64()
                        .translate(-self.area.min.to_vector().to_i64())
                        .to_usize();

                    let min_fb_relative_area = min_intersection
                        .to_i64()
                        .translate(-framebuffer_data.area.min.to_vector().to_i64())
                        .to_usize();

                    framebuffer_data
                        .pixels
                        .as_ndarray()
                        .slice(min_fb_relative_area.as_slice())
                        .assign_to(self.data.slice_mut(min_tb_relative_area.as_slice()));

                    TrackingBufferDamageRatio::observe(
                        min_intersection.area() as f64 / intersection.area() as f64 * 100.,
                    );

                    self.mark_stale(min_intersection);
                } else {
                    TrackingBufferDamageRatio::observe(0.)
                }
            }
        });
    }

    fn mark_stale(&mut self, stale: Box2D) {
        let intersection = match self.area.intersection(&stale) {
            Some(intersection) => intersection,
            None => return,
        };

        if intersection == self.area {
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

    fn cut_frame(&mut self) -> Option<Frame> {
        let buffers = match self.damage {
            Damage::None => return None,
            Damage::Partial(ref boxes) => boxes
                .iter()
                .map(|b| SubframeBuffer {
                    area: *b,
                    pixels: self.data.slice(b.as_slice()).to_owned(),
                })
                .collect(),
            Damage::Full => {
                vec![SubframeBuffer {
                    area: self.area,
                    pixels: self.data.clone(),
                }]
            }
        };

        self.clear();
        let id = self.curr_frame_id;
        self.curr_frame_id += 1;

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
