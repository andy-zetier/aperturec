use aperturec_channel::gate::*;
use aperturec_state_machine::*;

use anyhow::{anyhow, Result};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, Semaphore, TryAcquireError};
use tokio::task::JoinHandle;
use tokio::time::interval;
use tokio_util::sync::CancellationToken;
use tracing::*;

type RlRequest = (Command, usize, oneshot::Sender<()>);
type RlSender = mpsc::UnboundedSender<RlRequest>;
type RlReceiver = mpsc::UnboundedReceiver<RlRequest>;

#[derive(Debug, Clone)]
pub enum Command {
    RateLimitRequest,
}

#[derive(Debug, Clone)]
pub struct RateLimitHandle {
    sender: RlSender,
    commands: Vec<Command>,
}

impl RateLimitHandle {
    fn new(config: &Configuration) -> (Self, RlReceiver) {
        let (sender, rx) = mpsc::unbounded_channel::<RlRequest>();

        let commands = vec![config.mbps_max.map(|_| Command::RateLimitRequest)]
            .into_iter()
            .flatten()
            .collect();

        (Self { sender, commands }, rx)
    }

    pub async fn request_to_send<T>(&self, num_bytes: T) -> Result<()>
    where
        T: Into<usize> + std::marker::Copy,
    {
        for cmd in &self.commands {
            let (response_tx, response_rx) = oneshot::channel();
            self.sender
                .send((cmd.clone(), num_bytes.into(), response_tx))?;
            response_rx.await?;
        }

        Ok(())
    }
}

impl AsyncGate for RateLimitHandle {
    async fn wait(&self, msg_len: usize) -> anyhow::Result<()> {
        self.request_to_send(msg_len).await
    }
}

#[derive(Debug)]
pub struct Configuration {
    mbps_max: Option<usize>,
}

impl Configuration {
    pub fn new(server_mbps_max: Option<usize>, client_mbps_max: usize) -> Self {
        let mbps_max = match (server_mbps_max, client_mbps_max) {
            // Server capped, client “unlimited”  -> use server cap
            (Some(server_max), 0) => Some(server_max),

            // Both capped                       -> use the lower (stricter) cap
            (Some(server_max), client_max) => Some(std::cmp::min(server_max, client_max)),

            // No server cap, client “unlimited” -> no cap
            (None, 0) => None,

            // No server cap, client capped      -> use client cap
            (None, client_max) => Some(client_max),
        };

        Self { mbps_max }
    }
}

#[derive(Debug)]
pub struct TokenBucket {
    bucket: Semaphore,
    token_capacity: AtomicUsize,
    name: &'static str,
}

impl TokenBucket {
    fn new(token_capacity: usize, name: &'static str) -> Self {
        Self {
            bucket: Semaphore::new(token_capacity),
            token_capacity: token_capacity.into(),
            name,
        }
    }

    async fn get_tokens<T>(&self, count: T)
    where
        T: Into<usize>,
    {
        let count = count.into();

        match self.bucket.try_acquire_many(count as u32) {
            Ok(permit) => permit,
            Err(TryAcquireError::Closed) => panic!("Failed to acquire_many!"),
            Err(TryAcquireError::NoPermits) => {
                trace!("{} blocking acquire of {} tokens!", self.name, count);
                self.bucket
                    .acquire_many(count as u32)
                    .await
                    .expect("Failed to acquire_many!")
            }
        }
        .forget();
    }

    fn fill(&self) {
        let dropped_token_count = self.token_capacity.load(Ordering::Relaxed) as i128
            - self.bucket.available_permits() as i128;

        match dropped_token_count.cmp(&0) {
            std::cmp::Ordering::Greater => {
                trace!("{} refilling {} tokens", self.name, dropped_token_count);
                self.bucket.add_permits(dropped_token_count as usize);
            }
            std::cmp::Ordering::Less => {
                trace!("{} removing {} tokens", self.name, -dropped_token_count);
                self.bucket.forget_permits(-dropped_token_count as usize);
            }
            _ => (),
        }
    }
}

#[derive(Stateful, SelfTransitionable, Debug)]
#[state(S)]
pub struct Task<S: State> {
    state: S,
}

#[derive(State, Debug)]
pub struct Created {
    config: Configuration,
    cmd_rx: RlReceiver,
}

#[derive(State, Debug)]
pub struct Running {
    task: JoinHandle<Result<()>>,
    ct: CancellationToken,
}

#[derive(State, Debug)]
pub struct Terminated;

impl Task<Created> {
    pub fn new(rate_limit_config: Configuration) -> (Self, RateLimitHandle) {
        let (handle, cmd_rx) = RateLimitHandle::new(&rate_limit_config);
        (
            Task {
                state: Created {
                    config: rate_limit_config,
                    cmd_rx,
                },
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

impl Transitionable<Running> for Task<Created> {
    type NextStateful = Task<Running>;

    fn transition(self) -> Self::NextStateful {
        let task_ct = CancellationToken::new();
        let ct = task_ct.clone();
        let inner_ct = ct.clone();

        let mut cmd_rx = self.state.cmd_rx;

        let task = tokio::spawn(async move {
            trace!("Running rate limit task");

            //
            // Setup Rate Limiting
            //

            // Fill the token bucket a constant number of times per second
            const RATE_LIMIT_FILL_MS: usize = 1000 / 8;

            // Capacity is mbps_max -> bits per second -> bytes per second -> bytes, times FILL_MS
            let rate_limit_capacity = (((self.state.config.mbps_max.unwrap_or(0) * 1000000) / 8)
                / 1000)
                * RATE_LIMIT_FILL_MS;
            let rate_limit_tb = Arc::new(TokenBucket::new(rate_limit_capacity, "Rate Limit"));

            let mut rate_limit_fill_interval = interval(Duration::from_millis(
                RATE_LIMIT_FILL_MS.try_into().unwrap(),
            ));
            rate_limit_fill_interval
                .set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    biased;
                    _ = task_ct.cancelled() => {
                        debug!("rate limit task canceled");
                        break Ok(());
                    }
                    Some((cmd, num_bytes, response)) = cmd_rx.recv() => {
                        match cmd {
                            Command::RateLimitRequest => {
                                let rate_limit_tb = rate_limit_tb.clone();
                                let ct = inner_ct.clone();
                                tokio::spawn(async move {
                                    tokio::select! {
                                        _ = ct.cancelled() => (),
                                        _ = rate_limit_tb.get_tokens(num_bytes) =>
                                            response.send(()).expect("Failed to send RateLimitRequest oneshot"),
                                    }
                                });
                            },
                        };
                    }
                    _ = rate_limit_fill_interval.tick()  => {
                        rate_limit_tb.fill();
                    }
                    else => break Err(anyhow!("All futures exhausted in rate limit task"))
                }
            }
        });

        Task {
            state: Running { task, ct },
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
                    Ok(Ok(())) => anyhow!("rate limit task exited without internal error"),
                    Ok(Err(e)) => anyhow!("rate limit task exited with internal error: {}", e),
                    Err(e) => anyhow!("rate limit task exited with panic: {}", e),
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
