use crate::backend::Event;
use crate::server;

use anyhow::{anyhow, bail, Result};
use aperturec_channel::{self as channel, AsyncFlushable, AsyncReceiver, AsyncSender};
use aperturec_protocol::event::{client_to_server as em_c2s, server_to_client as em_s2c};
use aperturec_state_machine::*;
use futures::{future, prelude::*};
use std::pin::pin;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
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
    ec: channel::AsyncServerEvent,
    event_tx: mpsc::Sender<Event>,
    to_send_rx: mpsc::Receiver<em_s2c::Message>,
    client_inactive_tx: oneshot::Sender<()>,
    activity_timeout: Option<Duration>,
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
    pub event_rx: mpsc::Receiver<Event>,
    pub to_send_tx: mpsc::Sender<em_s2c::Message>,
    pub client_inactive_rx: oneshot::Receiver<()>,
}

impl Task<Created> {
    pub fn new(
        ec: channel::AsyncServerEvent,
        activity_timeout: Option<Duration>,
    ) -> (Self, Channels) {
        let (to_send_tx, to_send_rx) = mpsc::channel(1);
        let (event_tx, event_rx) = mpsc::channel(1);
        let (client_inactive_tx, client_inactive_rx) = oneshot::channel();
        (
            Task {
                state: Created {
                    ec,
                    event_tx,
                    to_send_rx,
                    client_inactive_tx,
                    activity_timeout,
                },
            },
            Channels {
                event_rx,
                to_send_tx,
                client_inactive_rx,
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
        let ct = CancellationToken::new();

        let (mut ec_rx, mut ec_tx) = self.state.ec.split();

        let tx_ct = ct.clone();
        let tx_task = task::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    Some(msg) = self.state.to_send_rx.recv() => {
                        ec_tx.send(msg).await?;
                    }
                    _ = tx_ct.cancelled() => {
                        time::timeout(
                            server::CHANNEL_FLUSH_TIMEOUT,
                            async {
                                ec_tx.flush().await.unwrap_or_else(|error| {
                                    warn!(%error, "flush event channel");
                                })
                            }
                        ).await.unwrap_or_else(|_| warn!("Timeout flushing event channel"));
                        break Ok(());
                    }
                    else => bail!("EC Tx messages exhausted"),
                }
            }
        });

        let rx_ct = ct.clone();
        let rx_task = task::spawn(async move {
            let timeout = self.state.activity_timeout.unwrap_or(Duration::MAX);
            let mut sleep = pin!(time::sleep(timeout));
            let mut client_inactive_tx = Some(self.state.client_inactive_tx);
            loop {
                tokio::select! {
                    biased;
                    _ = &mut sleep, if client_inactive_tx.is_some() => {
                        info!("Client inactive for {}", humantime::Duration::from(timeout));
                        client_inactive_tx.take().unwrap().send(()).map_err(|_| anyhow!("notify client inactive timeout"))?;
                    }
                    ec_rx_res = ec_rx.receive() => {
                        match ec_rx_res {
                            Ok(msg) => {
                                if matches!(msg, em_c2s::Message::KeyEvent(_) | em_c2s::Message::PointerEvent(_)) {
                                    sleep.set(time::sleep(timeout));
                                }
                                self.state.event_tx.send(msg.try_into()?).await?
                            },
                            Err(e) => bail!("EC Rx error: {}", e),
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
                ct,
                tx_task,
                rx_task,
            },
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
        let rx_res = self
            .state
            .rx_task
            .map_err(|e| anyhow!("EC Rx panic: {}", e))
            .and_then(|res| future::ready(res.map_err(|e| anyhow!("EC Rx error: {}", e))))
            .inspect(|_| self.state.ct.cancel())
            .boxed();
        let tx_res = self
            .state
            .tx_task
            .map_err(|e| anyhow!("EC Tx panic: {}", e))
            .and_then(|res| future::ready(res.map_err(|e| anyhow!("EC Tx error: {}", e))))
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

impl Transitionable<Terminated> for Task<Running> {
    type NextStateful = Task<Terminated>;

    fn transition(self) -> Self::NextStateful {
        Task { state: Terminated }
    }
}
