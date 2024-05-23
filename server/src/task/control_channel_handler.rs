use anyhow::{anyhow, Result};
use aperturec_channel::reliable::tcp;
use aperturec_channel::*;
use aperturec_protocol::control as cm;
use aperturec_protocol::control::client_to_server as cm_c2s;
use aperturec_protocol::control::server_to_client as cm_s2c;
use aperturec_state_machine::*;
use aperturec_trace::log;
use aperturec_trace::queue::{self, enq, trace_queue};
use futures::StreamExt;
use tokio::sync::mpsc;
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
    flow_control_tx: mpsc::UnboundedSender<cm::WindowAdvance>,
    to_send_rx: mpsc::Receiver<cm_s2c::Message>,
}

#[derive(State)]
pub struct Running {
    tx_subtask: JoinHandle<(AsyncServerControlChannelWriteHalf, Result<()>)>,
    tx_subtask_ct: CancellationToken,
    rx_subtask: JoinHandle<(AsyncServerControlChannelReadHalf, Result<()>)>,
    rx_subtask_ct: CancellationToken,
    ct: CancellationToken,
}

#[derive(State, Debug)]
pub struct Terminated {
    cc: tcp::Server<tcp::AsyncListening>,
}

pub struct Channels {
    pub to_send_tx: mpsc::Sender<cm_s2c::Message>,
    pub fb_update_req_rx: mpsc::UnboundedReceiver<cm::FramebufferUpdateRequest>,
    pub hb_resp_rx: mpsc::UnboundedReceiver<cm::HeartbeatResponse>,
    pub missed_frame_rx: mpsc::UnboundedReceiver<cm::MissedFrameReport>,
    pub flow_control_rx: mpsc::UnboundedReceiver<cm::WindowAdvance>,
}

impl Task<Created> {
    pub fn new(cc: AsyncServerControlChannel) -> (Self, Channels) {
        let (to_send_tx, to_send_rx) = mpsc::channel(1);
        let (fb_update_req_tx, fb_update_req_rx) = mpsc::unbounded_channel();
        let (hb_resp_tx, hb_resp_rx) = mpsc::unbounded_channel();
        let (missed_frame_tx, missed_frame_rx) = mpsc::unbounded_channel();
        let (flow_control_tx, flow_control_rx) = mpsc::unbounded_channel();
        let task = Task {
            state: Created {
                cc,
                fb_update_req_tx,
                hb_resp_tx,
                missed_frame_tx,
                flow_control_tx,
                to_send_rx,
            },
        };
        (
            task,
            Channels {
                to_send_tx,
                fb_update_req_rx,
                hb_resp_rx,
                flow_control_rx,
                missed_frame_rx,
            },
        )
    }

    pub async fn into_control_channel_server(self) -> tcp::Server<tcp::AsyncListening> {
        transition_async!(self.state.cc.into_inner(), tcp::AsyncListening)
    }
}

impl Task<Terminated> {
    pub fn into_control_channel_server(self) -> tcp::Server<tcp::AsyncListening> {
        self.state.cc
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
        let (mut cc_tx, mut cc_rx) = self.state.cc.split();

        let tx_ct = CancellationToken::new();
        let ct = tx_ct.clone();
        let mut stream_closed = false;
        let tx_subtask = tokio::spawn(async move {
            let mut to_send_stream = ReceiverStream::new(self.state.to_send_rx);
            let loop_res = loop {
                tokio::select! {
                    biased;
                    _ = ct.cancelled(), if !stream_closed => {
                        log::debug!("CC Tx subtask cancelled");
                        stream_closed = true;
                        to_send_stream.close();
                    }
                    msg_opt = to_send_stream.next() => {
                        match msg_opt {
                            Some(msg) => {
                                log::trace!("Sending {:?}", msg);
                                if let Err(err) = cc_tx.send(msg).await {
                                    break Err::<(), _>(anyhow!("Unable to send CC msg: {}", err));
                                }
                            },
                            None => {
                                log::trace!("CC messages exhausted");
                                break Ok(());
                            },
                        }
                    }
                    else => break Ok(())
                }
            };
            (cc_tx, loop_res)
        });

        let rx_ct = CancellationToken::new();
        let ct = rx_ct.clone();
        let rx_subtask = tokio::spawn(async move {
            let loop_res = loop {
                tokio::select! {
                    biased;
                    _ = ct.cancelled() => {
                        log::debug!("CC Rx subtask cancelled");
                        break Ok(());
                    },
                    cc_msg_res = cc_rx.receive() => {
                        if let Ok(msg) = &cc_msg_res {
                            log::trace!("Received control channel message {:?}", msg);
                        }
                        match cc_msg_res {
                            Ok(cm_c2s::Message::ClientInit(_)) => {
                                log::warn!("Spurious ClientInit message");
                            }
                            Ok(cm_c2s::Message::ClientGoodbye(_)) => {
                                log::info!("Client said goodbye");
                                break Ok(());
                            }
                            Ok(cm_c2s::Message::HeartbeatResponse(hb_resp)) => {
                                if let Err(err) = self.state.hb_resp_tx.send(hb_resp) {
                                    break Err(anyhow!("Could not forward HB response: {}", err));
                                }
                            }
                            Ok(cm_c2s::Message::FramebufferUpdateRequest(update_req)) => {
                                if let Err(err) = self.state.fb_update_req_tx.send(update_req) {
                                    break Err(anyhow!("Could not forward update request to FB update request channel: {}", err));
                                }
                            }
                            Ok(cm_c2s::Message::MissedFrameReport(report)) => {
                                if let Err(err) = self.state.missed_frame_tx.send(report) {
                                    break Err(anyhow!("Could not forward missed frame report to missed frame report channel: {}", err));
                                } else {
                                    enq!(MISSED_FRAME_QUEUE);
                                }
                            }
                            Ok(cm_c2s::Message::WindowAdvance(wa)) => {
                                if let Err(err) = self.state.flow_control_tx.send(wa) {
                                    break Err(anyhow!("Could not forward window advance to flow control channel: {}", err));
                                }
                            }
                            Err(e) => break Err(anyhow!("Failed to receive on control channel: {}", e)),
                        }
                    }
                    else => break Err(anyhow!("CC Rx has no more messages but is not cancelled"))
                }
            };
            (cc_rx, loop_res)
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
                    log::debug!("CC task cancelled");
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

        let cc_tx = match tx_subtask_term.expect("tx subtask not recovered") {
            Ok((cc_tx, Ok(()))) => cc_tx,
            Ok((cc_tx, Err(e))) => {
                log::error!("CC Tx subtask failed with error: {}", e);
                cc_tx
            }
            Err(e) => panic!("CC Tx subtask panicked: {}", e),
        };

        let cc_rx = match rx_subtask_term.expect("rx subtask not recovered") {
            Ok((cc_rx, Ok(()))) => cc_rx,
            Ok((cc_rx, Err(e))) => {
                log::error!("CC Rx subtask failed with error: {}", e);
                cc_rx
            }
            Err(e) => panic!("CC Rx subtask panicked: {}", e),
        };

        Ok(Task {
            state: Terminated {
                cc: transition_async!(
                    AsyncServerControlChannel::unsplit(cc_tx, cc_rx).into_inner(),
                    tcp::AsyncListening
                ),
            },
        })
    }
}

impl AsyncTransitionable<Terminated> for Task<Running> {
    type NextStateful = Task<Terminated>;

    async fn transition(self) -> Self::NextStateful {
        self.stop();
        match futures::join!(self.state.tx_subtask, self.state.rx_subtask) {
            (Ok((cc_tx, tx_res)), Ok((cc_rx, rx_res))) => {
                if let Err(tx_err) = tx_res {
                    log::error!("CC Tx subtask encountered error: {}", tx_err);
                }
                if let Err(rx_err) = rx_res {
                    log::error!("CC Rx subtask encountered error: {}", rx_err);
                }
                Task {
                    state: Terminated {
                        cc: transition_async!(
                            AsyncServerControlChannel::unsplit(cc_tx, cc_rx).into_inner(),
                            tcp::AsyncListening
                        ),
                    },
                }
            }
            (Err(tx_join_err), _) => {
                panic!("CC tx subtask panicked: {}", tx_join_err);
            }
            (_, Err(rx_join_err)) => {
                panic!("CC rx subtask panicked: {}", rx_join_err);
            }
        }
    }
}
