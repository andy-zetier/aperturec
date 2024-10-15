use anyhow::{anyhow, bail, Result};
use aperturec_channel::*;
use aperturec_protocol::control::client_to_server as cm_c2s;
use aperturec_protocol::control::server_to_client as cm_s2c;
use aperturec_state_machine::*;
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::*;

#[derive(Stateful, SelfTransitionable, Debug)]
#[state(S)]
pub struct Task<S: State> {
    state: S,
}

#[derive(State)]
pub struct Created {
    cc: AsyncServerControl,
    to_send_rx: mpsc::Receiver<cm_s2c::Message>,
}

#[derive(State)]
pub struct Running {
    ct: CancellationToken,
    tasks: JoinSet<Result<()>>,
}

#[derive(State, Debug)]
pub struct Terminated;

pub struct Channels {
    pub to_send_tx: mpsc::Sender<cm_s2c::Message>,
}

impl Task<Created> {
    pub fn new(cc: AsyncServerControl) -> (Self, Channels) {
        let (to_send_tx, to_send_rx) = mpsc::channel(1);
        let task = Task {
            state: Created { cc, to_send_rx },
        };
        (task, Channels { to_send_tx })
    }
}

impl Task<Running> {
    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.state.ct
    }
}

impl Transitionable<Running> for Task<Created> {
    type NextStateful = Task<Running>;

    fn transition(mut self) -> Self::NextStateful {
        let mut js = JoinSet::new();
        let curr_rt = Handle::current();

        let (mut cc_rx, mut cc_tx) = self.state.cc.split();

        {
            js.spawn_on(
                async move {
                    while let Some(msg) = self.state.to_send_rx.recv().await {
                        cc_tx.send(msg).await?;
                    }
                    bail!("CC message stream exhausted");
                },
                &curr_rt,
            );
        }

        {
            js.spawn_on(
                async move {
                    loop {
                        match cc_rx.receive().await {
                            Ok(cm_c2s::Message::ClientInit(_)) => {
                                warn!("Spurious ClientInit message");
                            }
                            Ok(cm_c2s::Message::ClientGoodbye(_)) => {
                                info!("Client said goodbye");
                                break Ok(());
                            }
                            Err(e) => bail!("control channel receive error: {}", e),
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
                        Ok(Ok(())) => {
                            trace!("task exited");
                            self.state.ct.cancel();
                            continue;
                        },
                        Ok(Err(e)) => anyhow!("control channel handler task exited with internal error: {}", e),
                        Err(e) => anyhow!("control channel handler task exited with panic: {}", e),
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
