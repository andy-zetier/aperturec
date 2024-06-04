use anyhow::{anyhow, Result};

use crate::task::encoder;
use aperturec_protocol::control as cm;
use aperturec_state_machine::*;
use aperturec_trace::log;
use futures::StreamExt;
use socket2::{Domain, Socket, Type};
use std::cmp::{max, min};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot, Semaphore, TryAcquireError};
use tokio::task::JoinHandle;
use tokio::time::interval;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_util::sync::CancellationToken;

type FcResponse = (u64, Option<SystemTime>);
type FcOptionResponse = Option<FcResponse>;
type FcRequest = (Command, usize, oneshot::Sender<FcOptionResponse>);
type FcSender = mpsc::UnboundedSender<FcRequest>;
type FcReceiver = mpsc::UnboundedReceiver<FcRequest>;

#[derive(Debug, Clone)]
pub enum Command {
    RateLimitRequest,
    WindowRequest,
}

#[derive(Debug, Clone)]
pub struct FlowControlHandle {
    sender: FcSender,
    commands: Vec<Command>,
}

impl FlowControlHandle {
    fn new(config: &Configuration) -> (Self, FcReceiver) {
        let (sender, rx) = mpsc::unbounded_channel::<FcRequest>();

        //
        // Order is significant here. Check mbps_max (if configured) before checking the window (if
        // configured) to ensure any available window_latency is not skewed due to max bitrate.
        //
        let commands = vec![
            config.mbps_max.map(|_| Command::RateLimitRequest),
            config.initial_window_size.map(|_| Command::WindowRequest),
        ]
        .into_iter()
        .flatten()
        .collect();

        (Self { sender, commands }, rx)
    }

    pub async fn request_to_send<T>(&self, num_bytes: T) -> Result<FcResponse>
    where
        T: Into<usize> + std::marker::Copy,
    {
        let mut response = (0_u64, None);
        for cmd in &self.commands {
            let (response_tx, response_rx) = oneshot::channel();
            self.sender
                .send((cmd.clone(), num_bytes.into(), response_tx))?;
            response = response_rx.await?.unwrap_or(response);
        }

        Ok(response)
    }
}

fn get_send_buffer_size(sock_addr: SocketAddr) -> usize {
    let sock =
        Socket::new(Domain::for_address(sock_addr), Type::DGRAM, None).expect("Socket create");
    sock.send_buffer_size().expect("SO_SNDBUF")
}

#[derive(Debug)]
pub struct Configuration {
    mbps_max: Option<usize>,
    initial_window_size: Option<usize>,
}

impl Configuration {
    pub fn new(
        window_size: Option<usize>,
        server_mbps_max: Option<usize>,
        sock_addr: SocketAddr,
        decoder_count: usize,
        client_mbps_max: usize,
        client_recv_size: usize,
    ) -> Self {
        let initial_window_size = match window_size {
            Some(0) => {
                //
                // The largest buffer size we can use is the smallest size between the Server's
                // send buffer and the Client's receive buffer.
                //
                let buffer_size = min(client_recv_size, get_send_buffer_size(sock_addr));

                // We must be able to send at least one message-worth of bytes.
                let buffer_size = max(buffer_size, encoder::MAX_BYTES_PER_MESSAGE);

                //
                // Each encoder / decoder pair will have its own socket buffer. However, depending
                // on workload, not all of the pairs may be active. Therefore, multiply the buffer
                // size by half the number of available pairs and truncate to the floor to arrive
                // at initial_window_size.
                //
                let multiplier = max(decoder_count, 2) / 2;
                Some(buffer_size * multiplier)
            }
            _ => window_size,
        };

        let mbps_max = match server_mbps_max {
            Some(server_mbps_max) => {
                if client_mbps_max == 0 {
                    Some(server_mbps_max)
                } else {
                    Some(client_mbps_max)
                }
            }
            _ => None,
        };

        Self {
            mbps_max,
            initial_window_size,
        }
    }

    pub fn get_initial_window_size(&self) -> u64 {
        self.initial_window_size.unwrap_or(0) as u64
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
                log::trace!("{} blocking acquire of {} tokens!", self.name, count);
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
                log::trace!("{} refilling {} tokens", self.name, dropped_token_count);
                self.bucket.add_permits(dropped_token_count as usize);
            }
            std::cmp::Ordering::Less => {
                log::trace!("{} removing {} tokens", self.name, -dropped_token_count);
                self.bucket.forget_permits(-dropped_token_count as usize);
            }
            _ => (),
        }
    }

    fn is_sender_waiting(&self) -> bool {
        self.bucket.available_permits() == 0
    }

    fn percent_filled_deferred(&self) -> impl FnOnce() -> f64 {
        let capacity = self.token_capacity.load(Ordering::Relaxed);
        let tokens = self.bucket.available_permits();

        move || (capacity - tokens) as f64 / capacity as f64
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
    cmd_rx: FcReceiver,
    flow_control_rx: mpsc::UnboundedReceiver<cm::WindowAdvance>,
}

#[derive(State, Debug)]
pub struct Running {
    task: JoinHandle<Result<()>>,
    ct: CancellationToken,
}

#[derive(State, Debug)]
pub struct Terminated;

impl Task<Created> {
    pub fn new(
        flow_control_config: Configuration,
        flow_control_rx: mpsc::UnboundedReceiver<cm::WindowAdvance>,
    ) -> (Self, FlowControlHandle) {
        let (handle, cmd_rx) = FlowControlHandle::new(&flow_control_config);
        (
            Task {
                state: Created {
                    config: flow_control_config,
                    cmd_rx,
                    flow_control_rx,
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

impl AsyncTryTransitionable<Running, Created> for Task<Created> {
    type SuccessStateful = Task<Running>;
    type FailureStateful = Task<Created>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let task_ct = CancellationToken::new();
        let ct = task_ct.clone();
        let inner_ct = ct.clone();

        let mut cmd_rx = self.state.cmd_rx;
        let mut fc_stream = UnboundedReceiverStream::new(self.state.flow_control_rx).fuse();

        let task = tokio::spawn(async move {
            log::trace!("Running flow control task");

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

            //
            // Setup Windowing
            //

            let window_tb = Arc::new(TokenBucket::new(
                self.state.config.initial_window_size.unwrap_or(0),
                "Window",
            ));
            let window_id = Arc::new(AtomicU64::new(0));
            let last_latency = Arc::new(Mutex::new(None));

            loop {
                tokio::select! {
                    biased;
                    _ = task_ct.cancelled() => {
                        log::info!("flow control task canceled");
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
                                            response.send(None).expect("Failed to send RateLimitRequest oneshot"),
                                    }
                                });
                            },
                            Command::WindowRequest => {
                                let window_tb = window_tb.clone();
                                let window_id = window_id.clone();
                                let last_latency = last_latency.clone();
                                let ct = inner_ct.clone();
                                tokio::spawn(async move {
                                    tokio::select! {
                                        _ = ct.cancelled() => (),
                                        _ = window_tb.get_tokens(num_bytes) => {
                                            let mut latency = last_latency.lock().unwrap();
                                            response.send(Some((
                                                window_id.load(Ordering::Relaxed),
                                                *latency,
                                            ))).expect("Failed to send WindowRequest oneshot");
                                            *latency = None
                                        },
                                    }
                                });
                            }
                        };
                    }
                    _ = rate_limit_fill_interval.tick()  => {
                        rate_limit_tb.fill();
                    }
                    Some(wa) = fc_stream.next() => {

                        //
                        // To avoid skewed latency, only make latency time stamp available if we
                        // have data currently waiting for the window to be refilled and no data is
                        // waiting on the rate limit token bucket.
                        //
                        if window_tb.is_sender_waiting() && !rate_limit_tb.is_sender_waiting() {
                            let mut latency = last_latency.lock().unwrap();
                            *latency = match wa.latency_timestamp.map(SystemTime::try_from) {
                                Some(Ok(systime)) => Some(systime),
                                Some(Err(err)) => {
                                    log::warn!("Failed to read latency timestamp: {:?}", err);
                                    None
                                },
                                _ => None,
                            }
                        }

                        crate::metrics::WindowFillPercent::update_with(window_tb.percent_filled_deferred());

                        window_id.fetch_add(1, Ordering::Relaxed);
                        window_tb.fill();
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
