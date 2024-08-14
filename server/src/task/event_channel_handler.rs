use crate::backend::Event;

use anyhow::{anyhow, Result};
use aperturec_channel::{self as channel, AsyncReceiver, AsyncSender};
use aperturec_protocol::event::server_to_client as em_s2c;
use aperturec_state_machine::*;
use aperturec_trace::log;
use aperturec_trace::queue::{self, enq, trace_queue};
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

pub(crate) const EVENT_QUEUE: queue::Queue = trace_queue!("ec:event");

#[derive(Stateful, SelfTransitionable, Debug)]
#[state(S)]
pub struct Task<S: State> {
    state: S,
}

#[derive(State)]
pub struct Created {
    ec: channel::AsyncServerEvent,
    event_tx: mpsc::UnboundedSender<Event>,
    to_send_rx: mpsc::Receiver<em_s2c::Message>,
}

#[derive(State)]
pub struct Running {
    tx_subtask: JoinHandle<Result<()>>,
    tx_subtask_ct: CancellationToken,
    rx_subtask: JoinHandle<Result<()>>,
    rx_subtask_ct: CancellationToken,
    ct: CancellationToken,
}

#[derive(State, Debug)]
pub struct Terminated;

pub struct Channels {
    pub event_rx: mpsc::UnboundedReceiver<Event>,
    pub to_send_tx: mpsc::Sender<em_s2c::Message>,
}

impl Task<Created> {
    pub fn new(ec: channel::AsyncServerEvent) -> (Self, Channels) {
        let (to_send_tx, to_send_rx) = mpsc::channel(1);
        let (event_tx, event_rx) = mpsc::unbounded_channel();
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

impl AsyncTryTransitionable<Running, Created> for Task<Created> {
    type SuccessStateful = Task<Running>;
    type FailureStateful = Task<Created>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let (mut ec_rx, mut ec_tx) = self.state.ec.split();

        let tx_ct = CancellationToken::new();
        let ct = tx_ct.clone();
        let tx_subtask = tokio::spawn(async move {
            let mut to_send_stream = ReceiverStream::new(self.state.to_send_rx);
            let mut stream_closed = false;

            loop {
                tokio::select! {
                    biased;
                    _ = ct.cancelled(), if !stream_closed => {
                        log::debug!("EC Tx subtask cancelled");
                        stream_closed = true;
                        to_send_stream.close();
                    }
                    msg_opt = to_send_stream.next() => {
                        match msg_opt {
                            Some(msg) => {
                                log::trace!("Sending {:?}", msg);
                                ec_tx.send(msg).await?;
                            },
                            None => {
                                log::trace!("EC messages exhausted");
                                break Ok(());
                            },
                        }
                    }
                    else => break Ok(())
                }
            }
        });

        let rx_ct = CancellationToken::new();
        let ct = rx_ct.clone();
        let rx_subtask = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = ct.cancelled() => {
                        log::debug!("EC Rx subtask cancelled");
                        break Ok(());
                    },
                    ec_msg_res = ec_rx.receive() => {
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
                }
            }
        });

        Ok(Task {
            state: Running {
                tx_subtask,
                tx_subtask_ct: tx_ct,
                rx_subtask,
                rx_subtask_ct: rx_ct,
                ct: CancellationToken::new(),
            },
        })
    }
}

impl AsyncTryTransitionable<Terminated, Terminated> for Task<Running> {
    type SuccessStateful = Task<Terminated>;
    type FailureStateful = Task<Terminated>;
    type Error = anyhow::Error;

    async fn try_transition(
        mut self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let mut top_level_cleanup_executed = false;
        let mut tx_subtask_term = None;
        let mut rx_subtask_term = None;

        loop {
            tokio::select! {
                _ = self.state.ct.cancelled(), if !top_level_cleanup_executed => {
                    log::debug!("EC task cancelled");
                    top_level_cleanup_executed = true;
                    self.state.tx_subtask_ct.cancel();
                    self.state.rx_subtask_ct.cancel();
                }
                tx_subtask_res = &mut self.state.tx_subtask, if tx_subtask_term.is_none() => {
                    tx_subtask_term = Some(tx_subtask_res);
                    self.state.ct.cancel();
                }
                rx_subtask_res = &mut self.state.rx_subtask, if rx_subtask_term.is_none() => {
                    rx_subtask_term = Some(rx_subtask_res);
                    self.state.ct.cancel();
                }
                else => break
            }
        }

        match tx_subtask_term.expect("tx subtask not recovered") {
            Ok(Ok(())) => (),
            Ok(Err(e)) => log::error!("EC Tx subtask failed with error: {}", e),
            Err(e) => log::error!("EC Tx subtask panicked: {}", e),
        }

        match rx_subtask_term.expect("rx subtask not recovered") {
            Ok(Ok(())) => (),
            Ok(Err(e)) => log::error!("EC Rx subtask failed with error: {}", e),
            Err(e) => log::error!("EC Rx subtask panicked: {}", e),
        }

        Ok(Task { state: Terminated })
    }
}

impl AsyncTransitionable<Terminated> for Task<Running> {
    type NextStateful = Task<Terminated>;

    async fn transition(self) -> Self::NextStateful {
        self.stop();

        match self.state.tx_subtask.await {
            Ok(Ok(())) => (),
            Ok(Err(e)) => log::error!("EC Tx subtask failed with error: {}", e),
            Err(e) => log::error!("EC Tx subtask panicked: {}", e),
        }

        match self.state.rx_subtask.await {
            Ok(Ok(())) => (),
            Ok(Err(e)) => log::error!("EC Rx subtask failed with error: {}", e),
            Err(e) => log::error!("EC Rx subtask panicked: {}", e),
        }
        Task { state: Terminated }
    }
}
