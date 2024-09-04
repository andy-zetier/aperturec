use crate::backend::{Backend, Event, SwapableBackend};
use crate::task::frame_sync::SubframeBuffer;

use aperturec_protocol::event::{self as em, server_to_client as em_s2c};
use aperturec_state_machine::*;

use anyhow::{anyhow, Result};
use futures::StreamExt;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use tokio::sync::mpsc;
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
    damage_tx: mpsc::UnboundedSender<SubframeBuffer<B::PixelMap>>,
    cursor_tx: mpsc::Sender<em_s2c::Message>,
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
pub struct Terminated<B: Backend> {
    backend: SwapableBackend<B>,
}

#[derive(State, Debug)]
pub struct TerminatedWithError<B: Backend> {
    backend: Option<SwapableBackend<B>>,
}

impl<B: Backend> Task<Created<B>> {
    pub fn new(
        backend: SwapableBackend<B>,
        event_rx: mpsc::Receiver<Event>,
        damage_tx: mpsc::UnboundedSender<SubframeBuffer<B::PixelMap>>,
        cursor_tx: mpsc::Sender<em_s2c::Message>,
    ) -> Task<Created<B>> {
        Task {
            state: Created {
                ct: CancellationToken::new(),
                backend,
                event_rx,
                damage_tx,
                cursor_tx,
            },
        }
    }
}

impl<B: Backend> Task<Created<B>> {
    pub fn into_backend(self) -> SwapableBackend<B> {
        self.state.backend
    }
}

impl<B: Backend> Task<Terminated<B>> {
    pub fn into_backend(self) -> SwapableBackend<B> {
        self.state.backend
    }
}

impl<B: Backend> Task<TerminatedWithError<B>> {
    pub fn into_backend(self) -> Option<SwapableBackend<B>> {
        self.state.backend
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
        let mut cursor_cache = HashMap::new();
        let mut last_cursor_id = 0;

        let ct = self.state.ct.clone();
        let task = tokio::task::spawn(async move {
            let res = 'select: loop {
                tokio::select! {
                    biased;
                    _ = ct.cancelled() => break Ok(()),
                    Some(event) = self.state.event_rx.recv() => {
                        if let Err(e) = self.state.backend.notify_event(event).await {
                            break Err(e);
                        }
                    },
                    Some(area) = damage_stream.next() => {
                        let pixels = match self.state.backend.capture_area(area).await {
                            Ok(pixels) => pixels,
                            Err(e) => break Err(e),
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
                                    Err(err) => {
                                        error!("Failed to capture cursor: {}", err);
                                        break 'select Err(err);
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
                            if let Err(err) = self.state.cursor_tx.send(msg).await {
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

impl<B: Backend> AsyncTryTransitionable<Terminated<B>, TerminatedWithError<B>>
    for Task<Running<B>>
{
    type SuccessStateful = Task<Terminated<B>>;
    type FailureStateful = Task<TerminatedWithError<B>>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        match self.state.task.await {
            Ok((backend, Ok(()))) => Ok(Task {
                state: Terminated { backend },
            }),
            Ok((backend, Err(error))) => Err(Recovered {
                stateful: Task {
                    state: TerminatedWithError {
                        backend: Some(backend),
                    },
                },
                error: anyhow!("backend task exited with error: {}", error),
            }),
            Err(error) => Err(Recovered {
                stateful: Task {
                    state: TerminatedWithError { backend: None },
                },
                error: anyhow!("backend task panicked: {}", error),
            }),
        }
    }
}

impl<B: Backend> Transitionable<TerminatedWithError<B>> for Task<Running<B>> {
    type NextStateful = Task<TerminatedWithError<B>>;

    fn transition(self) -> Self::NextStateful {
        Task {
            state: TerminatedWithError { backend: None },
        }
    }
}
