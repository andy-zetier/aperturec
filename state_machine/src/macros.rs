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
/// struct Init;
/// struct Running;
/// impl State for Init {}
/// impl State for Running {}
///
/// struct Server<S: State> {
///     state: S
/// }
/// impl SelfTransitionable for Server<Init> {}
///
/// impl<S: State> Stateful for Server<S> {
///     type State = S;
/// }
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
            Err(err) => return Err(Recovered::new($recoverable.transition(), err.into())),
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
/// struct Init;
/// struct Running;
/// impl State for Init {}
/// impl State for Running {}
///
/// struct Server<S: State> {
///     state: S
/// }
/// impl SelfTransitionable for Server<Init> {}
///
/// impl<S: State> Stateful for Server<S> {
///     type State = S;
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
        return Err(Recovered::new($recoverable.transition(), anyhow!($($arg)*)));
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
/// struct Init;
/// struct Running;
/// impl State for Init {}
/// impl State for Running {}
///
/// struct Server<S: State> {
///     state: S
/// }
/// impl SelfTransitionable for Server<Init> {}
///
/// impl<S: State> Stateful for Server<S> {
///     type State = S;
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
                $original_binding = recovered.stateful;
                continue;
            }
        }
    }};
}
