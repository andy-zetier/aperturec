use anyhow::{anyhow, Result};

use aperturec_protocol::control as cm;
use aperturec_protocol::control::server_to_client as cm_s2c;
use aperturec_state_machine::*;
use aperturec_trace::log;
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
    cc_send_tx: mpsc::Sender<cm_s2c::Message>,
    acked_seq_tx: mpsc::UnboundedSender<cm::DecoderSequencePair>,
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
        cc_send_tx: mpsc::Sender<cm_s2c::Message>,
    ) -> Self {
        let task = Task {
            state: Created {
                cc_send_tx,
                hb_req_interval,
                hb_resp_interval,
                hb_resp_rx,
                acked_seq_tx,
            },
        };
        task
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

        let task = tokio::spawn(async move {
            log::trace!("Running heartbeat task");
            let hb_req_interval = time::interval(self.state.hb_req_interval);
            tokio::pin!(hb_req_interval);

            let mut hb_id = 0;

            let mut hb_resp_stream = UnboundedReceiverStream::new(self.state.hb_resp_rx).fuse();
            let mut hb_resp_timers = FuturesUnordered::new();
            let mut unacked_hb_reqs = LinearSet::new();
            loop {
                tokio::select! {
                    biased;
                    _ = task_ct.cancelled() => {
                        log::info!("heartbeat task cancelled");
                        break Ok(());
                    }
                    Some(hb_resp) = hb_resp_stream.next() => {
                        if !unacked_hb_reqs.contains(&hb_resp.heartbeat_id) {
                            log::warn!("Received unsolicied HB response {:?}", hb_resp);
                        } else {
                            metrics::rtt_incoming(hb_resp.heartbeat_id);
                            unacked_hb_reqs.retain(|hb_id: &u64| hb_id > &hb_resp.heartbeat_id);
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
                            log::warn!("No response received for HB {} in time", expired_hb_resp_id);
                            self.state.cc_send_tx.send(cm::ServerGoodbye::new(cm::ServerGoodbyeReason::NetworkError.into()).into()).await?;
                            log::trace!("Sent server goodbye message");
                            break Err(anyhow!("Did not receive heartbeat response for {:?} in required interval {:?}", expired_hb_resp_id, self.state.hb_resp_interval));
                        }
                    }
                    _ = hb_req_interval.tick() => {
                        hb_id += 1;
                        let req = cm::HeartbeatRequest::new(hb_id);
                        metrics::rtt_outgoing(hb_id);
                        self.state.cc_send_tx.send(req.into()).await?;

                        unacked_hb_reqs.insert(hb_id);
                        let task_hb_id = hb_id;
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
