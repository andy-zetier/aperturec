use super::rate_limit::RateLimitHandle;

use crate::metrics::MediaChannelSendLatency;
use aperturec_channel::{self as channel, AsyncSender, AsyncServerMedia};
use aperturec_metrics::time;
use aperturec_protocol::media::server_to_client as mm_s2c;
use aperturec_state_machine::*;

use anyhow::{Result, anyhow, bail};
use tokio::sync::mpsc;
use tokio::task::{self, JoinHandle};
use tokio_util::sync::CancellationToken;

#[derive(Stateful, SelfTransitionable, Debug)]
#[state(S)]
pub struct Task<S: State> {
    state: S,
}

#[derive(State)]
pub struct Created {
    mc: channel::AsyncGatedServerMedia<RateLimitHandle>,
    mm_rx: mpsc::Receiver<mm_s2c::Message>,
}

#[derive(State)]
pub struct Running {
    task: JoinHandle<Result<()>>,
    ct: CancellationToken,
}

#[derive(State, Debug)]
pub struct Terminated;

pub struct Channels {
    pub mm_tx: mpsc::Sender<mm_s2c::Message>,
}

impl Task<Created> {
    pub fn new(mc: AsyncServerMedia, rl: RateLimitHandle) -> (Self, Channels) {
        let (mm_tx, mm_rx) = mpsc::channel(1);

        (
            Task {
                state: Created {
                    mc: mc.gated(rl),
                    mm_rx,
                },
            },
            Channels { mm_tx },
        )
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

        let tx_ct = ct.clone();
        let task = task::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    Some(msg) = self.state.mm_rx.recv() => {
                        time!(MediaChannelSendLatency, self.state.mc.send(msg.into()).await?);
                    }
                    _ = tx_ct.cancelled() => {
                        break Ok(());
                    }
                    else => bail!("MC Tx messages exhausted"),
                }
            }
        });

        Task {
            state: Running { ct, task },
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
        let stateful = Task { state: Terminated };
        match self.state.task.await {
            Err(e) => Err(Recovered {
                stateful,
                error: anyhow!("MC Tx error: {e}"),
            }),
            Ok(Err(e)) => Err(Recovered {
                stateful,
                error: anyhow!("MC Tx error: {e}"),
            }),
            _ => Ok(stateful),
        }
    }
}

impl Transitionable<Terminated> for Task<Running> {
    type NextStateful = Task<Terminated>;

    fn transition(self) -> Self::NextStateful {
        Task { state: Terminated }
    }
}
