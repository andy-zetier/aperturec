use crate::backend::Event;

use anyhow::{anyhow, bail, Result};
use aperturec_channel::{self as channel, AsyncReceiver, AsyncSender};
use aperturec_protocol::event::server_to_client as em_s2c;
use aperturec_state_machine::*;
use aperturec_trace::log;
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

#[derive(Stateful, SelfTransitionable, Debug)]
#[state(S)]
pub struct Task<S: State> {
    state: S,
}

#[derive(State)]
pub struct Created {
    ec: channel::AsyncServerEvent,
    event_tx: mpsc::Sender<Event>,
    to_send_rx: mpsc::Receiver<em_s2c::Message>,
}

#[derive(State)]
pub struct Running {
    ct: CancellationToken,
    tasks: JoinSet<Result<()>>,
}

#[derive(State, Debug)]
pub struct Terminated;

pub struct Channels {
    pub event_rx: mpsc::Receiver<Event>,
    pub to_send_tx: mpsc::Sender<em_s2c::Message>,
}

impl Task<Created> {
    pub fn new(ec: channel::AsyncServerEvent) -> (Self, Channels) {
        let (to_send_tx, to_send_rx) = mpsc::channel(1);
        let (event_tx, event_rx) = mpsc::channel(1);
        (
            Task {
                state: Created {
                    ec,
                    event_tx,
                    to_send_rx,
                },
            },
            Channels {
                event_rx,
                to_send_tx,
            },
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

impl Transitionable<Running> for Task<Created> {
    type NextStateful = Task<Running>;

    fn transition(mut self) -> Self::NextStateful {
        let mut js = JoinSet::new();
        let curr_rt = Handle::current();

        let (mut ec_rx, mut ec_tx) = self.state.ec.split();

        js.spawn_on(
            async move {
                while let Some(msg) = self.state.to_send_rx.recv().await {
                    ec_tx.send(msg).await?
                }
                bail!("EC Tx messages exhausted");
            },
            &curr_rt,
        );

        js.spawn_on(
            async move {
                while let Ok(msg) = ec_rx.receive().await {
                    self.state.event_tx.send(msg.try_into()?).await?;
                }
                bail!("EC Rx messages exhausted");
            },
            &curr_rt,
        );

        Task {
            state: Running {
                tasks: js,
                ct: CancellationToken::new(),
            },
        }
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
                        Ok(Ok(())) => {
                            log::trace!("task exited");
                            self.state.ct.cancel();
                            continue;
                        }
                        Ok(Err(e)) => anyhow!("event task exited with internal error: {}", e),
                        Err(e) => anyhow!("event task exited with panic: {}", e),
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
                error: anyhow!("control channel handler errors: {:?}", errors),
            })
        }
    }
}

impl Transitionable<Terminated> for Task<Running> {
    type NextStateful = Task<Terminated>;

    fn transition(self) -> Self::NextStateful {
        Task { state: Terminated }
    }
}
