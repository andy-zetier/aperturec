use crate::backend::Backend;
use crate::metrics::{
    FramesCut, TrackingBufferDamageRatio, TrackingBufferDisjointAreas, TrackingBufferUpdates,
};

use aperturec_graphics::{display::*, prelude::*, rectangle_cover::diff_rectangle_cover};
use aperturec_state_machine::*;

use anyhow::{Result, anyhow, bail, ensure};
use futures::{self, TryFutureExt, future};
use ndarray::{AssignElem, prelude::*};
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::{self, JoinSet};
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
pub struct SubframeBuffer<M: PixelMap> {
    pub origin: Point,
    pub pixels: M,
}

impl<M: PixelMap> SubframeBuffer<M> {
    pub fn area(&self) -> Rect {
        Rect::new(self.origin, self.pixels.size())
    }
}

#[derive(Debug)]
pub struct Frame {
    pub id: usize,
    pub display_config: usize,
    pub display: usize,
    pub buffers: Vec<SubframeBuffer<Pixel24Map>>,
}

#[derive(Debug)]
pub struct NewDisplayConfig {
    pub config_id: usize,
    pub display_encoder_counts: Vec<(Display, usize)>,
    pub notify_complete_tx: oneshot::Sender<()>,
}

#[derive(Stateful, SelfTransitionable)]
#[state(S)]
pub struct Task<S: State> {
    state: S,
}

#[derive(State)]
pub struct Created<B: Backend> {
    max_encoder_count: usize,
    display_encoder_counts: Vec<(Display, usize)>,
    frame_txs: Vec<mpsc::Sender<Arc<Frame>>>,
    damage_rx: mpsc::UnboundedReceiver<SubframeBuffer<B::PixelMap>>,
    display_config_rx: mpsc::Receiver<NewDisplayConfig>,
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
    pub display_config_tx: mpsc::Sender<NewDisplayConfig>,
}

impl<B: Backend> Task<Created<B>> {
    pub fn new(
        max_encoder_count: usize,
        display_encoder_counts: Vec<(Display, usize)>,
    ) -> (Self, Channels<B>) {
        let (frame_txs, frame_rxs) = (0..max_encoder_count).map(|_| mpsc::channel(1)).collect();
        let (damage_tx, damage_rx) = mpsc::unbounded_channel();
        let (display_config_tx, display_config_rx) = mpsc::channel(1);

        (
            Task {
                state: Created {
                    max_encoder_count,
                    display_encoder_counts,
                    frame_txs,
                    damage_rx,
                    display_config_rx,
                },
            },
            Channels {
                frame_rxs,
                damage_tx,
                display_config_tx,
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

        let mut i = 0;
        let tb_txs: Vec<_> = self
            .state
            .display_encoder_counts
            .into_iter()
            .enumerate()
            .map(|(id, (display, count))| {
                let end = i + count;
                let subset: Vec<_> = self.state.frame_txs[i..end].to_vec();
                i = end;
                (
                    Arc::new(Mutex::new(TrackingBuffer::new(id, display, 0))),
                    Arc::new(Mutex::new(subset)),
                )
            })
            .collect();

        let (revoke_permits_watch_tx, _) = watch::channel(());

        {
            let mut tb_txs = tb_txs.clone();
            let revoke_permits_watch_tx = revoke_permits_watch_tx.clone();
            let max_display_count = tb_txs.len();

            js.spawn_on(
                async move {
                    while let Some(ndc) = self.state.display_config_rx.recv().await {
                        ensure!(
                            ndc.display_encoder_counts.len() == max_display_count,
                            "Display count does not equal max number of displays: {} != {}",
                            ndc.display_encoder_counts.len(),
                            max_display_count,
                        );

                        let (encoder_count, disabled_display_count) =
                            ndc.display_encoder_counts.iter().fold(
                                (0, 0),
                                |(total_encoders, total_disabled), (display, count)| {
                                    (
                                        total_encoders + count,
                                        total_disabled
                                            + if !display.is_enabled { count } else { &0 },
                                    )
                                },
                            );

                        ensure!(
                            encoder_count <= self.state.max_encoder_count,
                            "Encoder count greater than max number of encoders: {} > {}",
                            encoder_count,
                            self.state.max_encoder_count,
                        );

                        ensure!(
                            disabled_display_count == 0,
                            "Disabled Displays have {} encoders assigned, expected 0",
                            disabled_display_count,
                        );

                        //
                        // Redistribute the frame_txs according to how many encoders have been
                        // assigned to the given tracking buffer / Display
                        //
                        let mut i = 0;
                        tb_txs.iter_mut().zip(ndc.display_encoder_counts).for_each(
                            |((tb, tx_subset), (display, count))| {
                                tb.lock()
                                    .expect("tracking buffer poisoned")
                                    .resize(display, ndc.config_id);

                                let end = i + count;
                                let mut subset = tx_subset.lock().expect("tx subset poisoned");
                                *subset = self.state.frame_txs[i..end].to_vec();
                                i = end;
                            },
                        );
                        revoke_permits_watch_tx
                            .send(())
                            .expect("Failed to send revoke permits");
                        let _ = ndc.notify_complete_tx.send(());
                    }
                    bail!("failed resolution recv")
                },
                &curr_rt,
            );
        }

        {
            let mut tracking_buffers: Vec<Arc<_>> =
                tb_txs.iter().map(|(arc, _)| Arc::clone(arc)).collect();
            js.spawn_on(
                async move {
                    while let Some(framebuffer_data) = self.state.damage_rx.recv().await {
                        let fd = Arc::new(framebuffer_data);
                        tracking_buffers.iter_mut().for_each(|tb| {
                            tb.lock()
                                .expect("tracking_buffer poisoned")
                                .update(fd.as_ref())
                        });
                    }
                    bail!("damage stream exhausted")
                },
                &curr_rt,
            );
        }

        {
            let tb_txs_dh: Vec<_> = tb_txs
                .into_iter()
                .map(|(tb, txs)| {
                    let dh = tb.lock().expect("tracking buffer poisoned").damage_handle();
                    (tb, txs, dh)
                })
                .collect();

            //
            // For each tracking buffer, await damage and reserve channel space to send the new
            // frame to the encoders. Channels may have been re-assigned to other tracking buffers
            // while awaiting damage, if so, we need to reserve space on the new channels without
            // waiting for damage.
            //
            for (tb, mut txs, mut dh) in tb_txs_dh.into_iter() {
                txs = txs.clone();
                let mut revoke_watch = revoke_permits_watch_tx.subscribe();
                js.spawn_on(
                    async move {
                        let mut permits_revoked = false;
                        loop {
                            let txs_to_reserve = {
                                let clones = txs
                                    .lock()
                                    .expect("tx vector poisoned")
                                    .iter()
                                    .cloned()
                                    .collect::<Vec<_>>();
                                clones
                            };

                            let dh_fut = async {
                                if !permits_revoked {
                                    dh.wait_for_damage().await
                                } else {
                                    Ok(())
                                }
                            };

                            let (permits, _) = future::try_join(
                                future::try_join_all(
                                    txs_to_reserve.iter().cloned().map(|tx| tx.reserve_owned()),
                                )
                                .map_err(anyhow::Error::new),
                                dh_fut,
                            )
                            .await?;

                            if revoke_watch.has_changed().unwrap_or(false) {
                                let _ = revoke_watch.borrow_and_update();
                                drop(permits);
                                permits_revoked = true;
                                continue;
                            } else {
                                permits_revoked = false;
                            }

                            let frame = tb.lock().expect("tracking buffer poisoned").cut_frame();

                            if let Some(frame) = frame {
                                let frame = Arc::new(frame);
                                for permit in permits.into_iter() {
                                    permit.send(frame.clone());
                                }
                            }
                        }
                    },
                    &curr_rt,
                );
            }
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
    display: Display,
    display_id: usize,
    display_config: usize,
    data: Array2<Pixel24>,
    damage: Damage,
    curr_frame_id: usize,
    damage_watch_tx: watch::Sender<()>,
    _pixel_map: PhantomData<BackendPixelMap>,
}

impl<BackendPixelMap> TrackingBuffer<BackendPixelMap>
where
    BackendPixelMap: PixelMap,
    for<'p> &'p mut Pixel24: AssignElem<BackendPixelMap::Pixel>,
{
    pub fn new(display_id: usize, display: Display, display_config: usize) -> Self {
        let (damage_watch_tx, _) = watch::channel(());
        let shape = (display.area.height(), display.area.width());
        TrackingBuffer {
            display,
            display_id,
            display_config,
            data: Array2::from_elem(shape, Pixel24::default()),
            damage: Damage::Full,
            curr_frame_id: 0,
            damage_watch_tx,
            _pixel_map: PhantomData,
        }
    }

    fn resize(&mut self, display: Display, display_config: usize) {
        self.curr_frame_id = 0;
        self.display_config = display_config;
        self.display = display;
        self.data = Array2::from_elem(self.display.area.as_shape(), Pixel24::default());
        self.clear();

        // Notify damage listeners that the Display has been changed
        self.damage_watch_tx
            .send(())
            .expect("Failed to send damage watch");
    }

    pub fn update(&mut self, framebuffer_data: &SubframeBuffer<BackendPixelMap>) {
        if !self.display.is_enabled {
            return;
        }

        let Some(intersection) = self.display.area.intersection(&framebuffer_data.area()) else {
            return;
        };

        TrackingBufferUpdates::inc();

        let fb_relative_area = intersection
            .to_i64()
            .translate(-framebuffer_data.origin.to_vector().to_i64())
            .to_usize();

        // Shift intersection to be relative to this tracking buffer
        let tb_relative_area = intersection
            .to_i64()
            .translate(-self.display.area.origin.to_vector().to_i64())
            .to_usize();

        let pixmap = framebuffer_data.pixels.as_ndarray();
        let new = pixmap.slice(&fb_relative_area.as_slice());

        task::block_in_place(|| match self.damage {
            Damage::Full => {
                new.assign_to(self.data.slice_mut(tb_relative_area.as_slice()));
                TrackingBufferDamageRatio::observe(1.);
            }
            _ => {
                let curr = self.data.slice(tb_relative_area.as_slice());
                let cover_areas = diff_rectangle_cover(curr, new);

                let mut total_area = 0;
                for b in &cover_areas {
                    let dst_area = b.translate(tb_relative_area.origin.to_vector());

                    new.slice(b.as_slice())
                        .assign_to(self.data.slice_mut(dst_area.as_slice()));

                    total_area += dst_area.area();
                    self.mark_stale(dst_area);
                }
                TrackingBufferDamageRatio::observe(
                    total_area as f64 / intersection.area() as f64 * 100.,
                );
            }
        });
    }

    fn mark_stale(&mut self, stale: Box2D) {
        if stale == self.display.area.to_box2d() {
            self.damage = Damage::Full;
        } else {
            match self.damage {
                Damage::None => self.damage = Damage::Partial(BoxSet::with_initial_box(stale)),
                Damage::Partial(ref mut areas) => areas.add(stale),
                Damage::Full => (),
            }
        }

        let _ = self.damage_watch_tx.send(());
    }

    fn clear(&mut self) {
        self.damage = Damage::None;
    }

    pub fn cut_frame(&mut self) -> Option<Frame> {
        let buffers = match self.damage {
            Damage::None => return None,
            Damage::Partial(ref boxes) => {
                boxes
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

                        SubframeBuffer {
                            origin: b.min,
                            pixels,
                        }
                    })
                    .collect()
            }
            Damage::Full => {
                // SAFETY: We initialize the Array2 in the call to `build_init`, so
                // `assume_init` is safe to call
                let pixels = unsafe {
                    Array2::build_uninit(self.display.area.size.as_shape(), |dst| {
                        self.data.assign_to(dst)
                    })
                    .assume_init()
                };
                vec![SubframeBuffer {
                    origin: Point::zero(),
                    pixels,
                }]
            }
        };

        self.clear();
        let frame_id = self.curr_frame_id;
        self.curr_frame_id += 1;

        FramesCut::inc();
        TrackingBufferDisjointAreas::inc_by(buffers.len() as f64);

        Some(Frame {
            id: frame_id,
            buffers,
            display: self.display_id,
            display_config: self.display_config,
        })
    }

    fn damage_handle(&self) -> DamageHandle {
        let mut watch = self.damage_watch_tx.subscribe();

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
        self.watch.changed().await?;
        Ok(())
    }
}
