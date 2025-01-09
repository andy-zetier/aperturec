use anyhow::Result;
use aperturec_state_machine::*;
use tokio::task::{self, JoinHandle};
use tokio::time::{self, Duration};
use tokio_util::sync::CancellationToken;

const TRIM_DURATION: Duration = Duration::from_secs(60);

#[derive(Stateful, SelfTransitionable, Debug)]
#[state(S)]
pub struct Task<S: State> {
    state: S,
}

#[derive(State)]
pub struct Created;

#[derive(State)]
pub struct Running {
    task: JoinHandle<Result<()>>,
    ct: CancellationToken,
}

#[derive(State, Debug)]
pub struct Terminated;

impl Task<Created> {
    pub fn new() -> Self {
        Task { state: Created }
    }
}

impl Task<Running> {
    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.state.ct
    }
}

impl Transitionable<Running> for Task<Created> {
    type NextStateful = Task<Running>;

    fn transition(self) -> Self::NextStateful {
        let ct = CancellationToken::new();
        let tx_ct = ct.clone();

        let task = task::spawn(async move {
            let mut interval = time::interval(TRIM_DURATION);

            loop {
                tokio::select! {
                    biased;
                    _ = interval.tick() => {
                        // SAFETY: This function is only considered unsafe due to being libc
                        unsafe {
                            libc::malloc_trim(0);
                        }
                    }
                    _ = tx_ct.cancelled() => {
                        break Ok(());
                    }
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
        match self.state.task.await {
            Ok(_) => Ok(Task { state: Terminated }),
            Err(e) => Err(Recovered {
                stateful: Task { state: Terminated },
                error: e.into(),
            }),
        }
    }
}

impl Transitionable<Terminated> for Task<Running> {
    type NextStateful = Task<Terminated>;

    fn transition(self) -> Self::NextStateful {
        Task { state: Terminated }
    }
}
