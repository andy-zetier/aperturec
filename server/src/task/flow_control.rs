use anyhow::{anyhow, Result};

use aperturec_state_machine::*;
use aperturec_trace::log;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot, Semaphore};
use tokio::task::JoinHandle;
use tokio::time::interval;
use tokio_util::sync::CancellationToken;

type FcChannelSender = mpsc::UnboundedSender<(Command, oneshot::Sender<()>)>;
type FcChannelReceiver = mpsc::UnboundedReceiver<(Command, oneshot::Sender<()>)>;

#[derive(Debug, Clone)]
pub struct FlowControlHandle {
    sender: FcChannelSender,
}

impl FlowControlHandle {
    fn new() -> (Self, FcChannelReceiver) {
        let (sender, rx) = mpsc::unbounded_channel::<(Command, oneshot::Sender<()>)>();
        (Self { sender }, rx)
    }

    pub async fn request_to_send<T>(&self, num_bytes: T) -> Result<()>
    where
        T: Into<usize>,
    {
        let (response_tx, response_rx) = oneshot::channel();
        self.sender
            .send((Command::RequestToSend(num_bytes.into()), response_tx))?;
        response_rx.await?;
        Ok(())
    }
}

#[derive(Stateful, SelfTransitionable, Debug)]
#[state(S)]
pub struct Task<S: State> {
    state: S,
}

#[derive(State, Debug)]
pub struct Created {
    mbps_max: usize,
    cmd_rx: FcChannelReceiver,
}

#[derive(State, Debug)]
pub struct Running {
    task: JoinHandle<Result<()>>,
    ct: CancellationToken,
}

#[derive(State, Debug)]
pub struct Terminated;

impl Task<Created> {
    pub fn new(mbps_max: usize) -> (Self, FlowControlHandle) {
        let (handle, cmd_rx) = FlowControlHandle::new();
        (
            Task {
                state: Created { mbps_max, cmd_rx },
            },
            handle,
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

pub enum Command {
    RequestToSend(usize),
}

impl AsyncTryTransitionable<Running, Created> for Task<Created> {
    type SuccessStateful = Task<Running>;
    type FailureStateful = Task<Created>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let task_ct = CancellationToken::new();
        let ct = task_ct.clone();

        // Fill the token bucket a constant number of times per second
        const FILL_MS: usize = 1000 / 8;

        // Capacity is mbps_max -> bits per second -> bytes per second -> bytes, times FILL_MS
        let capacity = (((self.state.mbps_max * 1000000) / 8) / 1000) * FILL_MS;

        let mut cmd_rx = self.state.cmd_rx;

        let task = tokio::spawn(async move {
            log::trace!("Running flow control task");

            let sem = Arc::new(Semaphore::new(capacity));
            let forgot_permit_count = Arc::new(AtomicUsize::new(0));

            let mut fill_interval = interval(Duration::from_millis(FILL_MS.try_into().unwrap()));
            fill_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    biased;
                    _ = task_ct.cancelled() => {
                        log::info!("flow control task canceled");
                        sem.close();
                        break Ok(());
                    }
                    Some((cmd, response)) = cmd_rx.recv() => {
                        let Command::RequestToSend(num_bytes) = cmd;
                        let sem = sem.clone();
                        let forgot_permit_count = forgot_permit_count.clone();
                        tokio::spawn(async move {
                            if cfg!(debug_assertions) && num_bytes > sem.available_permits() {
                                log::trace!("FC blocking acquire of {} permits!", num_bytes);
                            }
                            let permit = sem
                                .acquire_many(num_bytes as u32)
                                .await
                                .expect("Failed to acquire_many");
                            permit.forget();
                            forgot_permit_count.fetch_add(num_bytes, Ordering::Relaxed);
                            response.send(()).expect("Failed to send oneshot");
                        });
                    }
                    _ = fill_interval.tick()  => {
                        if forgot_permit_count.load(Ordering::Relaxed) > 0 {
                            log::trace!("FC adding {} permits", forgot_permit_count.load(Ordering::Relaxed));
                            sem.add_permits(forgot_permit_count.swap(0, Ordering::Relaxed));
                        }
                    }
                    else => break Err(anyhow!("All futures exhausted in flow control task"))
                }
            }
        });

        Ok(Task {
            state: Running { task, ct },
        })
    }
}

impl AsyncTryTransitionable<Terminated, Terminated> for Task<Running> {
    type SuccessStateful = Task<Terminated>;
    type FailureStateful = Task<Terminated>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        match self.state.task.await {
            Ok(Ok(())) => Ok(Task {
                state: Terminated {},
            }),
            Ok(Err(error)) => Err(Recovered::new(
                Task {
                    state: Terminated {},
                },
                error,
            )),
            Err(error) => Err(Recovered::new(
                Task {
                    state: Terminated {},
                },
                error.into(),
            )),
        }
    }
}

impl AsyncTransitionable<Terminated> for Task<Running> {
    type NextStateful = Task<Terminated>;

    async fn transition(self) -> Self::NextStateful {
        self.stop();
        match Handle::current().block_on(self.state.task) {
            Ok(Ok(())) => (),
            Ok(Err(error)) => {
                log::error!(
                    "Flow control task terminated in failure with error {}",
                    error
                )
            }
            Err(join_error) => panic!("Flow control task panicked: {}", join_error),
        }
        Task {
            state: Terminated {},
        }
    }
}
