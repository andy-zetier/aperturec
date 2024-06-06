use super::flow_control::FlowControlHandle;

use aperturec_channel::{self as channel, AsyncGatedServerMedia, AsyncSender};
use aperturec_protocol::media::FramebufferUpdate;
use aperturec_state_machine::*;
use aperturec_trace::log;

use anyhow::{anyhow, Result};
use futures::future::BoxFuture;
use futures::prelude::*;
use futures::stream::FuturesUnordered;
use futures::{Future, StreamExt};
use std::collections::BTreeMap;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

#[derive(Stateful, SelfTransitionable, Debug)]
#[state(S)]
pub struct Task<S: State> {
    state: S,
}

#[derive(State)]
pub struct Created {
    mc: channel::AsyncGatedServerMedia<FlowControlHandle>,
    fbu_tx: mpsc::Receiver<Vec<FramebufferUpdate>>,
    synthetic_missed_frame_txs: BTreeMap<u32, mpsc::UnboundedSender<u64>>,
    ct: CancellationToken,
}

#[derive(State)]
pub struct Running {
    task: JoinHandle<Result<()>>,
    ct: CancellationToken,
}

#[derive(State, Debug)]
pub struct Terminated;

pub struct Channels {
    pub fbu_tx: mpsc::Sender<Vec<FramebufferUpdate>>,
    pub synthetic_missed_frame_rxs: BTreeMap<u32, mpsc::UnboundedReceiver<u64>>,
}

impl Task<Created> {
    pub fn new(
        mc: AsyncGatedServerMedia<FlowControlHandle>,
        n_encoders: usize,
    ) -> (Self, Channels) {
        let (fbu_tx, fbu_rx) = mpsc::channel(n_encoders * 2);
        let mut synthetic_missed_frame_txs = BTreeMap::new();
        let mut synthetic_missed_frame_rxs = BTreeMap::new();
        for id in 0..n_encoders {
            let (tx, rx) = mpsc::unbounded_channel();
            synthetic_missed_frame_txs.insert(id as u32, tx);
            synthetic_missed_frame_rxs.insert(id as u32, rx);
        }

        (
            Task {
                state: Created {
                    mc,
                    fbu_tx: fbu_rx,
                    synthetic_missed_frame_txs,
                    ct: CancellationToken::new(),
                },
            },
            Channels {
                fbu_tx,
                synthetic_missed_frame_rxs,
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
        mut self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let ct = self.state.ct.clone();
        let (last_sent_tx, mut last_sent_rx) = watch::channel(BTreeMap::<u32, u64>::new());
        let mut timeouts = FuturesUnordered::<SyntheticMfrTimeout<'_>>::new();

        let task = tokio::spawn(async move {
            let mut curr_last_sent = BTreeMap::<u32, u64>::new();
            'outer: loop {
                tokio::select! {
                    biased;
                    _ = self.state.ct.cancelled() => {
                        log::info!("media channel handler internally cancelled");
                        break Ok(());
                    }
                    Ok(_) = last_sent_rx.changed() => {
                        let new_last_sent = last_sent_rx.borrow_and_update();
                        new_last_sent.iter()
                            .filter(|(enc, seq)| !curr_last_sent.contains_key(enc) || curr_last_sent.get(enc).unwrap() != *seq)
                            .map(|(enc, seq)| SyntheticMfrTimeout::new(*enc, *seq))
                            .for_each(|new_timeout| {
                                for curr_timeout in timeouts.iter_mut() {
                                    if curr_timeout.encoder == new_timeout.encoder {
                                        if new_timeout.sequence > curr_timeout.sequence {
                                            *curr_timeout = new_timeout;
                                        }
                                        return;
                                    }
                                }

                                timeouts.push(new_timeout);
                            });


                        curr_last_sent.extend(new_last_sent.iter());
                    }
                    Some((encoder, sequence)) = timeouts.next() => {
                        log::trace!("[Encoder {}] Synthetic MFR for {}", encoder, sequence);
                        self.state.synthetic_missed_frame_txs.get(&encoder).unwrap().send(sequence)?;
                    }
                    Some(fbus) = self.state.fbu_tx.recv() => {
                        for fbu in fbus {
                            let sequence = fbu.sequence;
                            let encoder = fbu.decoder_id;
                            if let Err(e) = self.state.mc.send(fbu).await {
                                break 'outer Err(anyhow!("[Encoder {}] Failed to send FBU {}: {}", encoder, sequence, e));
                            }
                            last_sent_tx.send_if_modified(|curr|
                                if let Some(existing) = curr.insert(encoder, sequence) {
                                    existing != sequence
                                } else {
                                    true
                                }
                            );
                        }
                    }
                    else => {
                        break Err(anyhow!("No more FBUs"));
                    }
                }
            }
        });

        Ok(Task {
            state: Running { task, ct },
        })
    }
}

impl Task<Running> {
    async fn complete(self) -> Option<anyhow::Error> {
        match self.state.task.await {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(anyhow!("Media channel handler failed with error: {}", e)),
            Err(e) => Some(anyhow!("Media channel handler panicked: {}", e)),
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
        let task = Task { state: Terminated };
        let error = self.complete().await;
        if let Some(error) = error {
            Err(Recovered {
                stateful: task,
                error,
            })
        } else {
            Ok(task)
        }
    }
}

impl AsyncTransitionable<Terminated> for Task<Running> {
    type NextStateful = Task<Terminated>;

    async fn transition(self) -> Self::NextStateful {
        self.stop();
        if let Some(error) = self.complete().await {
            log::error!("Media channel handler task experienced error: {}", error);
        }
        Task { state: Terminated }
    }
}

struct SyntheticMfrTimeout<'t> {
    encoder: u32,
    sequence: u64,
    timeout: BoxFuture<'t, ()>,
}

impl<'t> Future for SyntheticMfrTimeout<'t> {
    type Output = (u32, u64);

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.timeout
            .as_mut()
            .poll(cx)
            .map(|_| (self.encoder, self.sequence))
    }
}

impl<'t> SyntheticMfrTimeout<'t> {
    // TODO: this value should actually be some function of the RTT and the HB interval. Hardcoding
    // for now to a reasonable value
    const FBU_TIMEOUT: Duration = Duration::from_millis(300);

    fn new(encoder: u32, sequence: u64) -> Self {
        SyntheticMfrTimeout {
            encoder,
            sequence,
            timeout: async move {
                tokio::time::sleep(Self::FBU_TIMEOUT).await;
            }
            .boxed(),
        }
    }
}
