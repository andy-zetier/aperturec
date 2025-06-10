use crate::server;

use aperturec_channel::*;
use aperturec_protocol::control::client_to_server as cm_c2s;
use aperturec_protocol::control::server_to_client as cm_s2c;
use aperturec_state_machine::*;

use anyhow::{Result, anyhow, bail};
use futures::{future, prelude::*};
use tokio::sync::mpsc;
use tokio::task::{self, JoinHandle};
use tokio::time;
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
    tx_task: JoinHandle<Result<()>>,
    rx_task: JoinHandle<Result<()>>,
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
        let ct = CancellationToken::new();

        let (mut cc_rx, mut cc_tx) = self.state.cc.split();

        let tx_ct = ct.clone();
        let tx_task = task::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    Some(msg) = self.state.to_send_rx.recv() => {
                        cc_tx.send(msg).await?;
                    }
                    _ = tx_ct.cancelled() => {
                        time::timeout(
                            server::CHANNEL_FLUSH_TIMEOUT,
                            async {
                                cc_tx.flush().await.unwrap_or_else(|error| {
                                    debug!(%error, "flush control channel");
                                })
                            }
                        ).await.unwrap_or_else(|_| debug!("Timeout flushing control channel"));
                        break Ok(());
                    }
                    else => bail!("CC Tx messages exhausted"),
                }
            }
        });

        let rx_ct = ct.clone();
        let rx_task = task::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    cc_rx_res = cc_rx.receive() => {
                        match cc_rx_res {
                            Ok(cm_c2s::Message::ClientInit(_)) => {
                                warn!("Spurious ClientInit message");
                            }
                            Ok(cm_c2s::Message::ClientGoodbye(_)) => {
                                debug!("Client said goodbye");
                                break Ok(());
                            }
                            Err(e) => bail!("CC Rx error: {}", e),
                        }
                    }
                    _ = rx_ct.cancelled() => {
                        break Ok(());
                    }
                }
            }
        });

        Task {
            state: Running {
                tx_task,
                rx_task,
                ct,
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
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let rx_res = self
            .state
            .rx_task
            .map_err(|e| anyhow!("CC Rx panic: {}", e))
            .and_then(|res| future::ready(res.map_err(|e| anyhow!("CC Rx error: {}", e))))
            .inspect(|_| self.state.ct.cancel())
            .boxed();
        let tx_res = self
            .state
            .tx_task
            .map_err(|e| anyhow!("CC Tx panic: {}", e))
            .and_then(|res| future::ready(res.map_err(|e| anyhow!("CC Tx error: {}", e))))
            .inspect(|_| self.state.ct.cancel())
            .boxed();

        future::try_join(rx_res, tx_res)
            .await
            .map(|_| Task { state: Terminated })
            .map_err(|error| Recovered {
                stateful: Task { state: Terminated },
                error,
            })
    }
}
