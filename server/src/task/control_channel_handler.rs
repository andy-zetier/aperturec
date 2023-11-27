use anyhow::{anyhow, Result};
use aperturec_channel::reliable::tcp;
use aperturec_channel::*;
use aperturec_protocol::control_messages as cm;
use aperturec_state_machine::{
    transition, Recovered, SelfTransitionable, State, Stateful, Transitionable, TryTransitionable,
};
use aperturec_trace::log;
use aperturec_trace::queue::{self, enq, trace_queue};
use async_trait::async_trait;
use futures::StreamExt;
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

pub(crate) const MISSED_FRAME_QUEUE: queue::Queue = trace_queue!("cc:missed_frame");

#[derive(Stateful, SelfTransitionable, Debug)]
#[state(S)]
pub struct Task<S: State> {
    state: S,
}

#[derive(State)]
pub struct Created {
    cc: AsyncServerControlChannel,
    fb_update_req_tx: mpsc::UnboundedSender<cm::FramebufferUpdateRequest>,
    hb_resp_tx: mpsc::UnboundedSender<cm::HeartbeatResponse>,
    missed_frame_tx: mpsc::UnboundedSender<cm::MissedFrameReport>,
    to_send_rx: mpsc::Receiver<cm::ServerToClientMessage>,
    received_goodbye: oneshot::Sender<()>,
    ct: CancellationToken,
}

#[derive(State)]
pub struct Running {
    task: JoinHandle<(AsyncServerControlChannel, Result<()>)>,
    ct: CancellationToken,
}

#[derive(State, Debug)]
pub struct Terminated {
    cc: tcp::Server<tcp::Listening>,
}

pub struct Channels {
    pub to_send_tx: mpsc::Sender<cm::ServerToClientMessage>,
    pub fb_update_req_rx: mpsc::UnboundedReceiver<cm::FramebufferUpdateRequest>,
    pub hb_resp_rx: mpsc::UnboundedReceiver<cm::HeartbeatResponse>,
    pub missed_frame_rx: mpsc::UnboundedReceiver<cm::MissedFrameReport>,
    pub received_goodbye: oneshot::Receiver<()>,
}

impl Task<Created> {
    pub fn new(cc: AsyncServerControlChannel) -> (Self, Channels, CancellationToken) {
        let (to_send_tx, to_send_rx) = mpsc::channel(1);
        let (fb_update_req_tx, fb_update_req_rx) = mpsc::unbounded_channel();
        let (hb_resp_tx, hb_resp_rx) = mpsc::unbounded_channel();
        let (missed_frame_tx, missed_frame_rx) = mpsc::unbounded_channel();
        let (received_goodbye_tx, received_goodbye_rx) = oneshot::channel();
        let ct = CancellationToken::new();
        let task = Task {
            state: Created {
                cc,
                fb_update_req_tx,
                hb_resp_tx,
                missed_frame_tx,
                to_send_rx,
                received_goodbye: received_goodbye_tx,
                ct: ct.clone(),
            },
        };
        (
            task,
            Channels {
                to_send_tx,
                fb_update_req_rx,
                hb_resp_rx,
                missed_frame_rx,
                received_goodbye: received_goodbye_rx,
            },
            ct,
        )
    }

    pub fn into_control_channel_server(self) -> tcp::Server<tcp::Listening> {
        transition!(self.state.cc.into_inner(), tcp::Listening)
    }
}

impl Task<Terminated> {
    pub fn into_control_channel_server(self) -> tcp::Server<tcp::Listening> {
        self.state.cc
    }
}

impl Task<Running> {
    pub fn stop(&self) {
        self.state.ct.cancel();
    }
}

#[async_trait]
impl TryTransitionable<Running, Created> for Task<Created> {
    type SuccessStateful = Task<Running>;
    type FailureStateful = Task<Created>;
    type Error = anyhow::Error;

    async fn try_transition(
        mut self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let tx_ct = self.state.ct.clone();
        let rx_ct = self.state.ct.clone();
        let ct = self.state.ct.clone();
        let (mut cc_tx, mut cc_rx) = self.state.cc.split();

        let mut tx_subtask = tokio::spawn(async move {
            let mut to_send_stream = ReceiverStream::new(self.state.to_send_rx).fuse();
            let loop_res = loop {
                tokio::select! {
                    Some(msg) = to_send_stream.next() => {
                        log::trace!("Sending {:?}", msg);
                        if let Err(err) = cc_tx.send(msg.clone()).await {
                            break Err::<(), _>(anyhow!("Unable to send CC msg: {}", err));
                        }
                    }
                    _ = tx_ct.cancelled() => {
                        log::info!("CC Tx subtask cancelled");
                        break Ok(());
                    }
                    else => break Err(anyhow!("No more CC messages to send")),
                }
            };
            (cc_tx, loop_res)
        });

        let mut rx_subtask = tokio::spawn(async move {
            let loop_res = loop {
                tokio::select! {
                    cc_msg_res = cc_rx.receive() => {
                        if let Ok(msg) = &cc_msg_res {
                            log::trace!("Received control channel message {:?}", msg);
                        }
                        match cc_msg_res {
                            Ok(cm::ClientToServerMessage::ClientInit(_)) => {
                                log::warn!("Spurious ClientInit message");
                            }
                            Ok(cm::ClientToServerMessage::ClientGoodbye(_)) => {
                                if self.state.received_goodbye.send(()).is_err() {
                                    log::error!("Failed to send notify goodbye, server probably already dying");
                                }
                                break Ok(());
                            }
                            Ok(cm::ClientToServerMessage::HeartbeatResponse(hb_resp)) => {
                                if let Err(err) = self.state.hb_resp_tx.send(hb_resp) {
                                    break Err(anyhow!("Could not forward HB response: {}", err));
                                }
                            }
                            Ok(cm::ClientToServerMessage::FramebufferUpdateRequest(update_req)) => {
                                if let Err(err) = self.state.fb_update_req_tx.send(update_req) {
                                    break Err(anyhow!("Could not forward update request to FB update request channel: {}", err));
                                }
                            }
                            Ok(cm::ClientToServerMessage::MissedFrameReport(report)) => {
                                if let Err(err) = self.state.missed_frame_tx.send(report) {
                                    break Err(anyhow!("Could not forward missed frame report to missed frame report channel: {}", err));
                                } else {
                                    enq!(MISSED_FRAME_QUEUE);
                                }
                            }
                            Err(e) => break Err(anyhow!("Failed to receive on control channel: {}", e)),
                            msg => break Err(anyhow!("Unhandled control channel message: {:?}", msg)),
                        }
                    }
                    _ = rx_ct.cancelled() => {
                        log::info!("CC Rx subtask cancelled");
                        break Ok(());
                    }
                    else => break Err(anyhow!("No more CC messages to receive"))
                }
            };
            (cc_rx, loop_res)
        });

        let task = tokio::spawn(async move {
            let mut tx_subtask_term = None;
            let mut rx_subtask_term = None;

            loop {
                tokio::select! {
                    tx_subtask_res = &mut tx_subtask, if tx_subtask_term.is_none() => {
                        tx_subtask_term = Some(tx_subtask_res);
                        ct.cancel();
                    }
                    rx_subtask_res = &mut rx_subtask, if rx_subtask_term.is_none() => {
                        rx_subtask_term = Some(rx_subtask_res);
                        ct.cancel();
                    }
                    else => break
                }
            }

            let tx_subtask_term = tx_subtask_term.expect("tx subtask terminated");
            let rx_subtask_term = rx_subtask_term.expect("rx subtask terminated");
            let cc_tx = match tx_subtask_term {
                Ok((cc_tx, Ok(()))) => cc_tx,
                Ok((cc_tx, Err(e))) => {
                    log::error!("CC Tx subtask failed with error: {}", e);
                    cc_tx
                }
                Err(e) => panic!("CC Tx subtask panicked: {}", e),
            };

            let cc_rx = match rx_subtask_term {
                Ok((cc_rx, Ok(()))) => cc_rx,
                Ok((cc_rx, Err(e))) => {
                    log::error!("CC Rx subtask failed with error: {}", e);
                    cc_rx
                }
                Err(e) => panic!("CC Rx subtask panicked: {}", e),
            };

            let cc = AsyncServerControlChannel::unsplit(cc_tx, cc_rx);
            (cc, Ok(()))
        });

        Ok(Task {
            state: Running {
                task,
                ct: self.state.ct,
            },
        })
    }
}

#[async_trait]
impl TryTransitionable<Terminated, Terminated> for Task<Running> {
    type SuccessStateful = Task<Terminated>;
    type FailureStateful = Task<Terminated>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        match self.state.task.await {
            Ok((cc, res)) => {
                let stateful = Task {
                    state: Terminated {
                        cc: transition!(cc.into_inner(), tcp::Listening),
                    },
                };
                match res {
                    Ok(()) => Ok(stateful),
                    Err(err) => Err(Recovered::new(stateful, err)),
                }
            }
            Err(join_err) => panic!(
                "Control channel handler panicked, preventing recovery: {}",
                join_err
            ),
        }
    }
}

impl Transitionable<Terminated> for Task<Running> {
    type NextStateful = Task<Terminated>;

    fn transition(self) -> Self::NextStateful {
        self.stop();
        let cc = match Handle::current().block_on(async move {
            let task = self.state.task;
            task.await
        }) {
            Ok((cc, Ok(()))) => cc,
            Ok((cc, Err(err))) => {
                log::error!(
                    "Control channel handler terminated in failure with error: {}",
                    err
                );
                cc
            }
            Err(join_err) => panic!(
                "Control channel handler panicked, preventing recovery: {}",
                join_err
            ),
        };
        let cc = transition!(cc.into_inner(), tcp::Listening);
        Task {
            state: Terminated { cc },
        }
    }
}
