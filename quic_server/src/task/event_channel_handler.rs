use crate::backend::Event;

use anyhow::{anyhow, Result};
use aperturec_channel::{self as channel, AsyncReceiver};
use aperturec_state_machine::*;
use aperturec_trace::log;
use aperturec_trace::queue::{self, enq, trace_queue};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub(crate) const EVENT_QUEUE: queue::Queue = trace_queue!("ec:event");

#[derive(Stateful, SelfTransitionable, Debug)]
#[state(S)]
pub struct Task<S: State> {
    state: S,
}

#[derive(State)]
pub struct Created {
    ec: channel::AsyncServerEvent,
    event_tx: mpsc::UnboundedSender<Event>,
    ct: CancellationToken,
}

#[derive(State)]
pub struct Running {
    task: JoinHandle<Result<()>>,
    ct: CancellationToken,
}

#[derive(State, Debug)]
pub struct Terminated;

pub struct Channels {
    pub event_rx: mpsc::UnboundedReceiver<Event>,
}

impl Task<Created> {
    pub fn new(ec: channel::AsyncServerEvent) -> (Self, Channels) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        (
            Task {
                state: Created {
                    ec,
                    event_tx,
                    ct: CancellationToken::new(),
                },
            },
            Channels { event_rx },
        )
    }
}

impl Task<Running> {
    pub fn stop(&self) {
        self.state.ct.cancel();
    }

    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.state.ct
    }
}

impl AsyncTryTransitionable<Running, Created> for Task<Created> {
    type SuccessStateful = Task<Running>;
    type FailureStateful = Task<Created>;
    type Error = anyhow::Error;

    async fn try_transition(
        mut self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let ct = self.state.ct.clone();

        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    ec_msg_res = self.state.ec.receive() => {
                        let ec_msg = match ec_msg_res {
                            Ok(ec_msg) => ec_msg,
                            Err(err) => break Err(anyhow!("Could not receive event from EC: {}", err)),
                        };

                        let event = match ec_msg.try_into() {
                            Ok(event) => event,
                            Err(err) => break Err(anyhow!("Could not convert EC message to event: {}", err)),
                        };

                        match self.state.event_tx.send(event) {
                            Ok(()) => {
                                enq!(EVENT_QUEUE);
                            },
                            Err(err) => break Err(anyhow!("Could not send event to backend: {}", err)),
                        }
                    }
                    _ = self.state.ct.cancelled() => {
                        log::info!("event channel handler internally cancelled");
                        break Ok(());
                    }
                }
            }
        });

        Ok(Task {
            state: Running { task, ct },
        })
    }
}

impl Task<Running> {
    async fn complete(self) -> Option<anyhow::Error> {
        match self.state.task.await {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(anyhow!("Event channel handler failed with error: {}", e)),
            Err(e) => Some(anyhow!("Event channel handler panicked: {}", e)),
        }
    }
}

impl AsyncTryTransitionable<Terminated, Terminated> for Task<Running> {
    type SuccessStateful = Task<Terminated>;
    type FailureStateful = Task<Terminated>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let task = Task { state: Terminated };
        let error = self.complete().await;
        if let Some(error) = error {
            Err(Recovered {
                stateful: task,
                error,
            })
        } else {
            Ok(task)
        }
    }
}

impl AsyncTransitionable<Terminated> for Task<Running> {
    type NextStateful = Task<Terminated>;

    async fn transition(self) -> Self::NextStateful {
        self.stop();
        if let Some(error) = self.complete().await {
            log::error!("Event channel handler task experienced error: {}", error);
        }
        Task { state: Terminated }
    }
}
