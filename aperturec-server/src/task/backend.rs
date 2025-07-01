use crate::backend::{Backend, Event, SwapableBackend};
use crate::metrics::CaptureLatency;
use crate::task::encoder;
use crate::task::frame_sync::{NewDisplayConfig, SubframeBuffer};

use aperturec_graphics::{
    display::Display, display::DisplayExtent, partition::partition_displays, prelude::*,
};
use aperturec_protocol::common::*;
use aperturec_protocol::event::{self as em, server_to_client as em_s2c};
use aperturec_state_machine::*;

use anyhow::{Result, anyhow, ensure};
use futures::{FutureExt, StreamExt, stream};
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Instant;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::*;

#[derive(Stateful, SelfTransitionable, Debug)]
#[state(S)]
pub struct Task<S: State> {
    state: S,
}

#[derive(State)]
pub struct Created<B: Backend> {
    ct: CancellationToken,
    backend: SwapableBackend<B>,
    initial_display_extent: Size,
    event_rx: mpsc::Receiver<Event>,
    event_tx: mpsc::Sender<em_s2c::Message>,
    damage_tx: mpsc::UnboundedSender<SubframeBuffer<B::PixelMap>>,
    display_config_tx: mpsc::Sender<NewDisplayConfig>,
    encoder_command_txs: Vec<mpsc::Sender<encoder::Command>>,
}

#[derive(State, Debug)]
pub struct Running<B: Backend> {
    ct: CancellationToken,
    task: JoinHandle<(SwapableBackend<B>, Result<()>)>,
}

impl<B: Backend> Task<Running<B>> {
    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.state.ct
    }
}

#[derive(State, Debug)]
pub struct TerminatedWithBackend<B: Backend> {
    backend: SwapableBackend<B>,
    error: Option<anyhow::Error>,
}

#[derive(State, Debug)]
pub struct TerminatedWithoutBackend;

pub fn display_config_from_parts(
    id: u64,
    displays: &[Display],
    areas: &[Vec<Box2D>],
) -> Result<DisplayConfiguration> {
    ensure!(
        displays.len() == areas.len(),
        "Display and area count do not match: {} != {}",
        displays.len(),
        areas.len()
    );

    let display_decoder_infos = displays
        .iter()
        .zip(areas)
        .map(|(display, areas)| {
            let display_info: DisplayInfo = display.clone().try_into()?;
            Ok(DisplayDecoderInfoBuilder::default()
                .display(display_info)
                .decoder_areas(
                    areas
                        .iter()
                        .map(|b2d| (*b2d).try_into())
                        .collect::<Result<Vec<Rectangle>, _>>()?,
                )
                .build()?)
        })
        .collect::<Result<Vec<DisplayDecoderInfo>>>()?;

    Ok(DisplayConfigurationBuilder::default()
        .id(id)
        .display_decoder_infos(display_decoder_infos)
        .build()?)
}

impl<B: Backend> Task<Created<B>> {
    pub fn new(
        backend: SwapableBackend<B>,
        initial_display_extent: Size,
        event_rx: mpsc::Receiver<Event>,
        event_tx: mpsc::Sender<em_s2c::Message>,
        damage_tx: mpsc::UnboundedSender<SubframeBuffer<B::PixelMap>>,
        display_config_tx: mpsc::Sender<NewDisplayConfig>,
        encoder_command_txs: Vec<mpsc::Sender<encoder::Command>>,
    ) -> Task<Created<B>> {
        Task {
            state: Created {
                ct: CancellationToken::new(),
                backend,
                initial_display_extent,
                event_rx,
                event_tx,
                damage_tx,
                display_config_tx,
                encoder_command_txs,
            },
        }
    }
}

impl<B: Backend> Task<Created<B>> {
    pub fn into_backend(self) -> SwapableBackend<B> {
        self.state.backend
    }
}

impl<B: Backend> Task<TerminatedWithBackend<B>> {
    pub fn components(self) -> (SwapableBackend<B>, Option<anyhow::Error>) {
        (self.state.backend, self.state.error)
    }
}

impl<B: Backend + 'static> AsyncTryTransitionable<Running<B>, Created<B>> for Task<Created<B>> {
    type SuccessStateful = Task<Running<B>>;
    type FailureStateful = Task<Created<B>>;
    type Error = anyhow::Error;

    async fn try_transition(
        mut self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let mut damage_stream =
            try_recover_async!(self.state.backend.damage_stream(), self).boxed();
        let mut cursor_stream = try_recover_async!(self.state.backend.cursor_stream(), self);

        let ct = self.state.ct.clone();
        let task = tokio::task::spawn(async move {
            let mut cursor_cache = HashMap::new();
            let mut last_cursor_id = 0;
            let mut display_config_id = 0;
            let mut display_extent = Rect::from_size(self.state.initial_display_extent);
            let res = 'select: loop {
                tokio::select! {
                    biased;
                    _ = ct.cancelled() => break Ok(()),
                    exit_status_res = self.state.backend.wait_root_process() => {
                        match exit_status_res {
                            Ok(exit_status) => {
                                if exit_status.success() {
                                    debug!("root process exited successfully");
                                    break Ok(());
                                } else {
                                    break Err(anyhow!("root process exited with exit status: {}", exit_status));
                                }
                            }
                            Err(error) => {
                                break Err(anyhow!("root process IO error: {}", error));
                            }
                        }
                    }
                    Some(event) = self.state.event_rx.recv() => {
                        if let Event::Display { ref displays } = event {

                            let client_displays = displays;
                            debug!(?client_displays);

                            //
                            // Try to set the backend resolution
                            //
                            let displays = match self.state.backend.set_displays(client_displays.to_vec()).await {
                                Ok(success) => {
                                    if !success.changed {
                                        debug!(?client_displays, "Requested display configuration matches current, refreshing");
                                    }
                                    success.displays
                                }
                                Err(displays) => {
                                    warn!("Failed to set displays, repartitioning");
                                    displays
                                }
                            };

                            //
                            // Partiton the final Displays across all encoders
                            //
                            let encoder_areas = partition_displays(
                                &displays,
                                self.state.encoder_command_txs.len(),
                            ).expect("Failed to re-partition displays");

                            //
                            // Generate New Display Configuration
                            //
                            display_config_id += 1;
                            let display_configuration = match display_config_from_parts(
                                display_config_id as u64,
                                &displays,
                                &encoder_areas)
                            {
                                Ok(dc) => dc,
                                Err(err) => {
                                    warn!("Failed to build display configuration: {}", err);
                                    continue;
                                }
                            };

                            //
                            // Notify tracking buffer of resolution change and await confirmation
                            //
                            let (notify_complete_tx, notify_complete_rx) = oneshot::channel();
                            if let Err(e) = self.state.display_config_tx.send(NewDisplayConfig {
                                display_encoder_counts: displays
                                    .clone()
                                    .into_iter()
                                    .zip(encoder_areas.iter().map(|a| a.len()))
                                    .collect(),
                                config_id: display_config_id,
                                notify_complete_tx,
                            }).await {
                                break Err(e.into());
                            }

                            //
                            // Reconfigure Encoders
                            //
                            for ((enc_id, area), tx) in encoder_areas
                                .iter()
                                .flat_map(|areas| areas.iter().enumerate())
                                .zip(self.state.encoder_command_txs.iter())
                            {
                                if let Err(e) = tx.send(encoder::Command::Reconfigure((enc_id, *area))).await {
                                    break 'select Err(e.into());
                                }
                            }

                            // Release tracking buffer
                            if let Err(e) = notify_complete_rx.await {
                                break Err(e.into());
                            }

                            debug!(?display_configuration);

                            //
                            // Notify Client of successful resolution change
                            //
                            let msg = em_s2c::Message::DisplayConfiguration(display_configuration);
                            if let Err(e) = self.state.event_tx.send(msg).await {
                                break Err(e.into())
                            }

                            display_extent = match displays.derive_extent() {
                                Ok(extent) => Rect::from_size(extent),
                                Err(e) => break Err(e.into()),
                            };

                            // Force fullscreen damage for enabled displays
                            damage_stream = stream::iter(displays.into_iter().filter(|d| d.is_enabled).map(|d| d.area))
                                .chain(damage_stream)
                                .boxed();
                        } else if let Err(e) = self.state.backend.notify_event(event).await {
                            break Err(e);
                        }
                    },
                    Some(area) = damage_stream.next() => {
                        let mut bounding = area;
                        while let Some(Some(area)) = damage_stream.next().now_or_never() {
                            bounding = bounding.union(&area);
                        }
                        if let Some(area) = display_extent.intersection(&bounding) {
                            let start = Instant::now();
                            let pixels = match self.state.backend.capture_area(area).await {
                                Ok(pixels) => {
                                    CaptureLatency::observe(Instant::now().duration_since(start).as_secs_f64() * 1000.0);
                                    pixels
                                },
                                Err(e) => {
                                    warn!(error = ?e, ?area, "Failed to capture area");
                                    continue;
                                }
                            };
                            let fb_data = SubframeBuffer { origin: area.origin, pixels };
                            if let Err(e) = self.state.damage_tx.send(fb_data) {
                                break Err(e.into());
                            }
                        }
                    }
                    Some(cursor_change) = cursor_stream.next() => {
                        let (id, msg) = match cursor_cache.get(&cursor_change.serial) {
                            Some(id) => (*id, em_s2c::Message::CursorChange(em::CursorChange { id: *id })),
                            None => 'msg: {
                                match self.state.backend.capture_cursor().await {
                                    Err(e) => {
                                        warn!(error = ?e, "Failed to capture cursor");
                                        continue 'select;
                                    },
                                    Ok(cursor_image) => {

                                        //
                                        // Captured cursor may not match the original cursor change
                                        // notification. Generate CursorChange if the captured
                                        // cursor has already been sent.
                                        //
                                        if cursor_change.serial != cursor_image.serial {
                                            trace!(
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
                                            trace!("Cursor {} already exists in cache", id);
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
                            if let Err(err) = self.state.event_tx.send(msg).await {
                                break 'select Err(err.into())
                            }
                            last_cursor_id = id;
                        }
                    }
                    else => break Err(anyhow!("backend stream exhausted")),
                }
            };

            (self.state.backend, res)
        });

        Ok(Task {
            state: Running {
                task,
                ct: self.state.ct,
            },
        })
    }
}

impl<B: Backend> AsyncTryTransitionable<TerminatedWithBackend<B>, TerminatedWithoutBackend>
    for Task<Running<B>>
{
    type SuccessStateful = Task<TerminatedWithBackend<B>>;
    type FailureStateful = Task<TerminatedWithoutBackend>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        match self.state.task.await {
            Ok((backend, Ok(()))) => Ok(Task {
                state: TerminatedWithBackend {
                    backend,
                    error: None,
                },
            }),
            Ok((backend, Err(error))) => Ok(Task {
                state: TerminatedWithBackend {
                    backend,
                    error: Some(error),
                },
            }),
            Err(error) => Err(Recovered {
                stateful: Task {
                    state: TerminatedWithoutBackend,
                },
                error: anyhow!("backend task panicked: {:?}", error),
            }),
        }
    }
}

impl<B: Backend> Transitionable<TerminatedWithoutBackend> for Task<Running<B>> {
    type NextStateful = Task<TerminatedWithoutBackend>;

    fn transition(self) -> Self::NextStateful {
        Task {
            state: TerminatedWithoutBackend,
        }
    }
}
