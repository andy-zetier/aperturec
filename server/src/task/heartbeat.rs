use anyhow::{anyhow, Result};
use aperturec_protocol::common_types::*;
use aperturec_protocol::control_messages as cm;
use aperturec_state_machine::{
    Recovered, SelfTransitionable, State, Stateful, Transitionable, TryTransitionable,
};
use async_trait::async_trait;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use linear_map::set::LinearSet;
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_util::sync::CancellationToken;

use crate::metrics;

#[derive(Stateful, SelfTransitionable, Debug)]
#[state(S)]
pub struct Task<S: State> {
    state: S,
}

#[derive(State, Debug)]
pub struct Created {
    hb_req_interval: Duration,
    hb_resp_interval: Duration,
    hb_resp_rx: mpsc::UnboundedReceiver<cm::HeartbeatResponse>,
    cc_send_tx: mpsc::Sender<cm::ServerToClientMessage>,
    acked_seq_tx: mpsc::UnboundedSender<cm::DecoderSequencePair>,
    ct: CancellationToken,
}

#[derive(State, Debug)]
pub struct Running {
    task: JoinHandle<Result<()>>,
    ct: CancellationToken,
}

#[derive(State, Debug)]
pub struct Terminated {}

impl Task<Created> {
    pub fn new(
        hb_req_interval: Duration,
        hb_resp_interval: Duration,
        acked_seq_tx: mpsc::UnboundedSender<cm::DecoderSequencePair>,
        hb_resp_rx: mpsc::UnboundedReceiver<cm::HeartbeatResponse>,
        cc_send_tx: mpsc::Sender<cm::ServerToClientMessage>,
    ) -> (Self, CancellationToken) {
        let ct = CancellationToken::new();
        let task = Task {
            state: Created {
                cc_send_tx,
                hb_req_interval,
                hb_resp_interval,
                hb_resp_rx,
                acked_seq_tx,
                ct: ct.clone(),
            },
        };
        (task, ct)
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
        let ct = self.state.ct.clone();
        let task = tokio::spawn(async move {
            log::trace!("Running heartbeat task");
            let hb_req_interval = time::interval(self.state.hb_req_interval);
            tokio::pin!(hb_req_interval);

            let mut hb_id = HeartbeatId::new(0);

            let mut hb_resp_stream = UnboundedReceiverStream::new(self.state.hb_resp_rx).fuse();
            let mut hb_resp_timers = FuturesUnordered::new();
            let mut unacked_hb_reqs = LinearSet::new();
            loop {
                tokio::select! {
                    biased;
                    _ = self.state.ct.cancelled() => {
                        log::info!("heartbeat task cancelled");
                        break Ok(());
                    }
                    Some(hb_resp) = hb_resp_stream.next() => {
                        if !unacked_hb_reqs.contains(&hb_resp.heartbeat_id) {
                            log::warn!("Received unsolicied HB response {:?}", hb_resp);
                        } else {
                            metrics::rtt_incoming(hb_resp.heartbeat_id.0);
                            unacked_hb_reqs.retain(|hb_id: &HeartbeatId| hb_id.0 > hb_resp.heartbeat_id.0);
                            for pair in hb_resp.last_sequence_ids {
                                self.state.acked_seq_tx.send(pair)?;
                            }
                        }
                    }
                    Some(expired_hb_resp_task_res) = &mut hb_resp_timers.next(), if !hb_resp_timers.is_empty() => {
                        let expired_hb_resp_id = match expired_hb_resp_task_res {
                            Ok(expired_hb_resp_id) => expired_hb_resp_id,
                            Err(join_error) => break Err(anyhow!("Failed to join HB resp task: {}", join_error)),
                        };
                        if unacked_hb_reqs.contains(&expired_hb_resp_id) {
                            let server_goodbye_msg = cm::ServerToClientMessage::new_server_goodbye(
                                cm::ServerGoodbye::new(cm::ServerGoodbyeReason::new_network_error())
                            );
                            self.state.cc_send_tx.send(server_goodbye_msg).await?;
                            log::trace!("Sent server goodbye message");
                            break Err(anyhow!("Did not receive heartbeat response for {:?} in required interval {:?}", expired_hb_resp_id, self.state.hb_resp_interval));
                        }
                    }
                    _ = hb_req_interval.tick() => {
                        hb_id.0 += 1;
                        let req = cm::HeartbeatRequest::new(hb_id.clone());
                        let msg = cm::ServerToClientMessage::new_heartbeat_request(req);
                        metrics::rtt_outgoing(hb_id.0);
                        self.state.cc_send_tx.send(msg).await?;


                        unacked_hb_reqs.insert(hb_id.clone());
                        let task_hb_id = hb_id.clone();
                        hb_resp_timers.push(tokio::spawn(async move {
                            time::sleep(self.state.hb_resp_interval).await;
                            task_hb_id
                        }));
                    }
                    else => break Err(anyhow!("All futures exhaused in heartbeat task"))
                }
            }
        });
        Ok(Task {
            state: Running { task, ct },
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

impl Transitionable<Terminated> for Task<Running> {
    type NextStateful = Task<Terminated>;

    fn transition(self) -> Self::NextStateful {
        self.stop();
        match Handle::current().block_on(async move {
            let task = self.state.task;
            task.await
        }) {
            Ok(Ok(())) => (),
            Ok(Err(error)) => {
                log::error!("heartbeat task terminated in failure with error {}", error)
            }
            Err(join_error) => panic!("Heartbeat task panicked: {}", join_error),
        }
        Task {
            state: Terminated {},
        }
    }
}
