use crate::backend::Event;

use anyhow::{anyhow, Result};
use aperturec_channel::reliable::tcp;
use aperturec_channel::*;
use aperturec_state_machine::{
    transition, Recovered, SelfTransitionable, State, Stateful, Transitionable, TryTransitionable,
};
use aperturec_trace::log;
use aperturec_trace::queue::{self, enq, trace_queue};
use async_trait::async_trait;
use tokio::runtime::Handle;
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
    ec: AsyncServerEventChannel,
    event_tx: mpsc::UnboundedSender<Event>,
    ct: CancellationToken,
}

#[derive(State)]
pub struct Running {
    task: JoinHandle<(AsyncServerEventChannel, Result<()>)>,
    ct: CancellationToken,
}

#[derive(State, Debug)]
pub struct Terminated {
    ec: tcp::Server<tcp::Listening>,
}

pub struct Channels {
    pub event_rx: mpsc::UnboundedReceiver<Event>,
}

impl Task<Created> {
    pub fn new(ec: AsyncServerEventChannel) -> (Self, Channels, CancellationToken) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let ct = CancellationToken::new();
        (
            Task {
                state: Created {
                    ec,
                    event_tx,
                    ct: ct.clone(),
                },
            },
            Channels { event_rx },
            ct,
        )
    }

    pub fn into_event_channel_server(self) -> tcp::Server<tcp::Listening> {
        transition!(self.state.ec.into_inner(), tcp::Listening)
    }
}

impl Task<Terminated> {
    pub fn into_event_channel_server(self) -> tcp::Server<tcp::Listening> {
        self.state.ec
    }
}

#[async_trait]
impl TryTransitionable<Running, Created> for Task<Created> {
    type SuccessStateful = Task<Running>;
    type FailureStateful = Task<Created>;
    type Error = anyhow::Error;

    async fn try_transition(
        mut self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let ct = self.state.ct.clone();

        let task = tokio::spawn(async move {
            let loop_res = loop {
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
            };
            (self.state.ec, loop_res)
        });

        Ok(Task {
            state: Running { task, ct },
        })
    }
}

#[async_trait]
impl TryTransitionable<Terminated, Terminated> for Task<Running> {
    type SuccessStateful = Task<Terminated>;
    type FailureStateful = Task<Terminated>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        match self.state.task.await {
            Ok((ec, res)) => {
                let stateful = Task {
                    state: Terminated {
                        ec: transition!(ec.into_inner(), tcp::Listening),
                    },
                };
                match res {
                    Ok(()) => Ok(stateful),
                    Err(err) => Err(Recovered::new(stateful, err)),
                }
            }
            Err(join_err) => panic!("Event channel handler panicked: {}", join_err),
        }
    }
}

impl Transitionable<Terminated> for Task<Running> {
    type NextStateful = Task<Terminated>;

    fn transition(self) -> Self::NextStateful {
        self.state.ct.cancel();
        let task = self.state.task;
        let ec = match Handle::current().block_on(task) {
            Ok((ec, Ok(()))) => ec,
            Ok((ec, Err(err))) => {
                log::error!(
                    "Event channel handler terminated in failure with error {}",
                    err
                );
                ec
            }
            Err(join_err) => panic!("Event channel handler panicked: {}", join_err),
        };
        let ec = transition!(ec.into_inner(), tcp::Listening);
        Task {
            state: Terminated { ec },
        }
    }
}
