use crate::backend::{Backend, Event, SwapableBackend};
use crate::task::encoder;
use crate::task::frame_sync::{NewResolution, SubframeBuffer};

use aperturec_graphics::{partition::partition, prelude::*};
use aperturec_protocol::common::*;
use aperturec_protocol::event::{self as em, server_to_client as em_s2c};
use aperturec_state_machine::*;

use anyhow::{anyhow, Result};
use futures::StreamExt;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
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
    event_rx: mpsc::Receiver<Event>,
    event_tx: mpsc::Sender<em_s2c::Message>,
    damage_tx: mpsc::UnboundedSender<SubframeBuffer<B::PixelMap>>,
    resolution_tx: mpsc::Sender<NewResolution>,
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

impl<B: Backend> Task<Created<B>> {
    pub fn new(
        backend: SwapableBackend<B>,
        event_rx: mpsc::Receiver<Event>,
        event_tx: mpsc::Sender<em_s2c::Message>,
        damage_tx: mpsc::UnboundedSender<SubframeBuffer<B::PixelMap>>,
        resolution_tx: mpsc::Sender<NewResolution>,
        encoder_command_txs: Vec<mpsc::Sender<encoder::Command>>,
    ) -> Task<Created<B>> {
        Task {
            state: Created {
                ct: CancellationToken::new(),
                backend,
                event_rx,
                event_tx,
                damage_tx,
                resolution_tx,
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
        let mut damage_stream = try_recover_async!(self.state.backend.damage_stream(), self);
        let mut cursor_stream = try_recover_async!(self.state.backend.cursor_stream(), self);

        let ct = self.state.ct.clone();
        let task = tokio::task::spawn(async move {
            let mut cursor_cache = HashMap::new();
            let mut last_cursor_id = 0;
            let mut display_config_id = 0;
            let res = 'select: loop {
                tokio::select! {
                    biased;
                    _ = ct.cancelled() => break Ok(()),
                    exit_status_res = self.state.backend.wait_root_process() => {
                        match exit_status_res {
                            Ok(exit_status) => {
                                if exit_status.success() {
                                    info!("root process exited successfully");
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
                        if let Event::Display { ref size } = event {

                            //
                            // Partition the new resolution across all available encoders
                            //
                            let requested_resolution = Size::new(size.width as usize, size.height as usize);
                            let (new_resolution, partitions) = partition(
                                &requested_resolution,
                                self.state.encoder_command_txs.len(),
                            );

                            let areas = partitions
                                .iter()
                                .enumerate()
                                .map(|(id, b)| (id as u32, DecoderArea {
                                    location: Some(Location::new(b.min.x as u64, b.min.y as u64)),
                                    dimension: Some(Dimension::new(b.width() as u64, b.height() as u64)),
                                }))
                            .collect();

                            //
                            // Ensure requested resolution does not match current resolution
                            //
                            match self.state.backend.resolution().await {
                                Ok(res) if res == new_resolution => {
                                    debug!(?new_resolution, "New resolution matches current resolution");
                                    let msg = em_s2c::Message::DisplayConfiguration(DisplayConfiguration {
                                        id: display_config_id,
                                        display_size: Some(Dimension::new(new_resolution.width as u64, new_resolution.height as u64)),
                                        areas,
                                    });

                                    if let Err(err) = self.state.event_tx.send(msg).await {
                                        break Err(err.into())
                                    }
                                    continue;
                                },
                                Ok(_) => (),
                                Err(e) => {
                                    warn!(error = ?e, "Failed to get resolution");
                                    continue;
                                },
                            };

                            //
                            // Try to set the backend resolution
                            //
                            if let Err(e) = self.state.backend.set_resolution(&new_resolution).await {
                                warn!(error = ?e, ?new_resolution, "Failed to set resolution");
                                continue;
                            }

                            //
                            // Notify Encoders of new partition layout
                            //
                            for (tx, part) in self.state.encoder_command_txs.iter().zip(&partitions) {
                                if let Err(e) = tx.send(encoder::Command::UpdateArea(*part)).await {
                                    break 'select Err(e.into());
                                }
                            }

                            //
                            // Notify tracking buffer of resolution change and await confirmation
                            //
                            let (notify_complete_tx, notify_complete_rx) = oneshot::channel();
                            if let Err(e) = self.state.resolution_tx.send(NewResolution {
                                resolution: new_resolution,
                                notify_complete_tx,
                            }).await {
                                break Err(e.into());
                            }

                            if let Err(e) = notify_complete_rx.await {
                                break Err(e.into());
                            }

                            debug!(?requested_resolution, ?new_resolution);

                            //
                            // Notify Client of successful resolution change
                            //
                            display_config_id += 1;
                            let msg = em_s2c::Message::DisplayConfiguration(DisplayConfiguration {
                                id: display_config_id,
                                display_size: Some(Dimension::new(new_resolution.width as u64, new_resolution.height as u64)),
                                areas,
                            });

                            if let Err(err) = self.state.event_tx.send(msg).await {
                                break Err(err.into())
                            }
                        } else if let Err(e) = self.state.backend.notify_event(event).await {
                            break Err(e);
                        }
                    },
                    Some(area) = damage_stream.next() => {
                        let pixels = match self.state.backend.capture_area(area).await {
                            Ok(pixels) => pixels,
                            Err(e) => {
                                warn!(error = ?e, ?area, "Failed to capture area");
                                continue;
                            }
                        };
                        let area = area.to_box2d();
                        let fb_data = SubframeBuffer { area, pixels };
                        if let Err(e) = self.state.damage_tx.send(fb_data) {
                            break Err(e.into());
                        }
                    }
                    Some(cursor_change) = cursor_stream.next() => {
                        let (id, msg) = match cursor_cache.get(&cursor_change.serial) {
                            Some(id) => (*id, em_s2c::Message::CursorChange(em::CursorChange { id: *id })),
                            None => 'msg: {
                                match self.state.backend.capture_cursor().await {
                                    Err(e) => {
                                        error!(error = ?e, "Failed to capture cursor");
                                        break 'select Err(e);
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
