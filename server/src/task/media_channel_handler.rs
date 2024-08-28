use super::rate_limit::RateLimitHandle;

use aperturec_channel::{self as channel, AsyncSender, AsyncServerMedia};
use aperturec_protocol::media::server_to_client as mm_s2c;
use aperturec_state_machine::*;

use anyhow::{anyhow, bail, Result};
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
        let task = task::spawn(async move {
            while let Some(msg) = self.state.mm_rx.recv().await {
                self.state.mc.send(msg.into()).await?;
            }
            bail!("media message stream exhausted");
        });

        Task {
            state: Running {
                task,
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
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let abort_handle = self.state.task.abort_handle();
        let stateful = Task { state: Terminated };
        tokio::select! {
            _ = self.state.ct.cancelled() => {
                abort_handle.abort();
                Ok(stateful)
            },
            task_res = self.state.task => {
                let error = match task_res {
                    Ok(Ok(())) => anyhow!("media channel handler task exited without internal error"),
                    Ok(Err(e)) => anyhow!("media channel handler task exited with internal error: {}", e),
                    Err(e) => anyhow!("media channel handler task exited with panic: {}", e),
                };
                Err(Recovered { stateful, error })
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
