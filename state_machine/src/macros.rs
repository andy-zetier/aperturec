/// Try some operation and return the required [`Recovered`](crate::Recovered) if the operation fails
///
/// This is useful where the [`std::ops::Try`] (`?`) operator might seem appropriate. However since we
/// must capture the current [`Stateful`](crate::Stateful), we cannot use `?` directly.
///
/// ```
/// use anyhow::anyhow;
/// use async_trait::async_trait;
/// use aperturec_state_machine::{try_recover, Recovered, SelfTransitionable, State, Stateful, Transitionable, TryTransitionable};
///
/// #[derive(State)]
/// struct Init;
/// #[derive(State)]
/// struct Running;
///
/// #[derive(Stateful)]
/// #[state(S)]
/// struct Server<S: State> {
///     state: S
/// }
/// impl SelfTransitionable for Server<Init> {}
///
/// fn do_thing() -> anyhow::Result<()> {
///     anyhow::bail!("We always fail")
/// }
///
/// #[async_trait]
/// impl TryTransitionable<Running, Init> for Server<Init> {
///     type SuccessStateful = Server<Running>;
///     type FailureStateful = Server<Init>;
///     type Error = anyhow::Error;
///
///     async fn try_transition(
///         self,
///     ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
///         // Since `do_thing` always fails, we always bail out here with a Server<Init> stateful.
///         // However, if `do_thing` succeeded, we'd move on to the rest of this function
///         let var = try_recover!(do_thing(), self);
///
///         todo!("We didn't implement this yet")
///     }
/// }
/// ```
#[macro_export]
macro_rules! try_recover {
    ($try_expr:expr, $recoverable:expr) => {{
        match $try_expr {
            Ok(ok) => ok,
            Err(err) => {
                return Err(Recovered::new($recoverable.transition(), err.into()));
            }
        }
    }};
    ($try_expr:expr, $recoverable:expr, $new_state:ty) => {{
        match $try_expr {
            Ok(ok) => ok,
            Err(err) => {
                let new_stateful = <_ as Transitionable<$new_state>>::transition($recoverable);
                return Err(Recovered::new(new_stateful, err.into()));
            }
        }
    }};
}

/// Return the required [`Recovered`](crate::Recovered) with a provided error message
///
/// This is useful for explicitly bailing out of a
/// [`try_transition`](crate::TryTransitionable::try_transition) which fails:
///
/// ```
/// use anyhow::anyhow;
/// use async_trait::async_trait;
/// use aperturec_state_machine::{return_recover, try_transition_continue, Recovered, SelfTransitionable, State, Stateful, Transitionable, TryTransitionable};
///
/// #[derive(State)]
/// struct Init;
/// #[derive(State)]
/// struct Running;
///
/// #[derive(Stateful, SelfTransitionable)]
/// #[state(S)]
/// struct Server<S: State> {
///     state: S
/// }
///
/// #[async_trait]
/// impl TryTransitionable<Running, Init> for Server<Init> {
///     type SuccessStateful = Server<Running>;
///     type FailureStateful = Server<Init>;
///     type Error = anyhow::Error;
///
///     async fn try_transition(
///         self,
///     ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
///         return_recover!(self, "We always fail :(")
///     }
/// }
///
/// ```
#[macro_export]
macro_rules! return_recover {
    ($recoverable:expr, $($arg:tt)*) => {{
        return Err(Recovered::new($recoverable.transition(), anyhow::anyhow!($($arg)*)));
    }};
}

/// Try some operation and on failure, assign a given exiting binding to the recovered
/// [`Stateful`](crate::Stateful), then `continue`
///
/// This is useful in a loop:
/// ```
/// use anyhow::anyhow;
/// use async_trait::async_trait;
/// use aperturec_state_machine::{return_recover, try_transition_continue, Recovered, SelfTransitionable, State, Stateful, Transitionable, TryTransitionable};
///
/// #[derive(State)]
/// struct Init;
/// #[derive(State)]
/// struct Running;
///
/// #[derive(Stateful, SelfTransitionable)]
/// #[state(S)]
/// struct Server<S: State> {
///     state: S
/// }
///
/// #[async_trait]
/// impl TryTransitionable<Running, Init> for Server<Init> {
///     type SuccessStateful = Server<Running>;
///     type FailureStateful = Server<Init>;
///     type Error = anyhow::Error;
///
///     async fn try_transition(
///         self,
///     ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
///         return_recover!(self, "We always fail :(")
///     }
/// }
///
/// #[tokio::main]
/// async fn some_loop_func() {
///     let mut server = Server { state: Init };
///
///     loop {
///         // Keep attempting to transition the server into `Running` until we succeed. Because
///         // always fail, this will actually just loop forever!
///         let running: Server<Running> = try_transition_continue!(server, server);
///         panic!("Server is running!");
///     }
/// }
/// ```
#[macro_export]
macro_rules! try_transition_continue {
    ($stateful:expr, $original_binding:ident) => {{
        match $stateful.try_transition().await {
            Ok(ok) => ok,
            Err(recovered) => {
                log::error!("Transition failed with error: {}", recovered.error);
                $original_binding = recovered.stateful;
                continue;
            }
        }
    }};
}

/// Attempt to transition a [`TryTransitionable`](crate::TryTransitionable) within another
/// [`TryTransitionable`](crate::TryTransitionable) machine, recovering the outer state
/// machine if the inner transition fails
///
/// A recovery expression must be provided as the third argument, which will be used to re-construct
/// the outer state machine's state during the recovery process
#[macro_export]
macro_rules! try_transition_inner_recover {
    ($inner:expr, $inner_recovered_state:ty, $recover_constructor:expr) => {{
        match $inner.try_transition().await {
            Ok(ok) => ok,
            Err(recovered) => {
                let inner_recovered =
                    <_ as Transitionable<$inner_recovered_state>>::transition(recovered.stateful);
                let new_stateful = $recover_constructor(inner_recovered);
                let error = recovered.error;
                return Err(Recovered::new(new_stateful, error));
            }
        }
    }};
}

/// Explicitly transition a [`Transitionable`](crate::Transitionable) [`Stateful`](crate::Stateful)
/// to a provided state. This is useful when there are more than one valid transitions for a
/// [`Stateful`](crate::Stateful).
#[macro_export]
macro_rules! transition {
    ($stateful:expr, $state:ty) => {{
        <_ as Transitionable<$state>>::transition($stateful)
    }};
}
