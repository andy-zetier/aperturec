/// Try some operation and return the required [`Recovered`](crate::Recovered) if the operation fails
///
/// This is useful where the [`std::ops::Try`] (`?`) operator might seem appropriate. However since we
/// must capture the current [`Stateful`](crate::Stateful), we cannot use `?` directly.
///
/// ```
/// use anyhow::anyhow;
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
/// impl TryTransitionable<Running, Init> for Server<Init> {
///     type SuccessStateful = Server<Running>;
///     type FailureStateful = Server<Init>;
///     type Error = anyhow::Error;
///
///     fn try_transition(
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
        try_recover!($try_expr, $recoverable, _)
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

/// Async variant of [`try_recover`](crate::try_recover). This must be instantiated within an
/// `async` block.
#[macro_export]
macro_rules! try_recover_async {
    ($try_expr:expr, $recoverable:expr) => {{
        try_recover_async!($try_expr, $recoverable, _)
    }};
    ($try_expr:expr, $recoverable:expr, $new_state:ty) => {{
        match $try_expr.await {
            Ok(ok) => ok,
            Err(err) => {
                let new_stateful =
                    <_ as AsyncTransitionable<$new_state>>::transition($recoverable).await;
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
/// impl TryTransitionable<Running, Init> for Server<Init> {
///     type SuccessStateful = Server<Running>;
///     type FailureStateful = Server<Init>;
///     type Error = anyhow::Error;
///
///     fn try_transition(
///         self,
///     ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
///         return_recover!(self, "We always fail :(")
///         // We could also specify the target type with the second argument:
///         // return_recover!(self, Init, "we always fail :(")
///     }
/// }
/// ```
#[macro_export]
macro_rules! return_recover {
    ($recoverable:expr, $fmt:literal $(, $args:tt )*) => {{
        return_recover!($recoverable, _, $fmt $(, $args )*)
    }};
    ($recoverable:expr, $target:ty, $fmt:literal $(, $args:tt )*) => {{
        return Err(
            Recovered::new(
                <_ as Transitionable<$target>>::transition($recoverable),
                anyhow::anyhow!($fmt, $( $args ),*)
            )
        );
    }};
}

/// Async variant of [`return_recover`](crate::return_recover). This must be instantiated within an
/// `async` block.
#[macro_export]
macro_rules! return_recover_async {
    ($recoverable:expr, $fmt:literal $(, $args:tt )*) => {{
        return_recover_async!($recoverable, _, $fmt, $(, $args )*)
    }};
    ($recoverable:expr, $target:ty, $fmt:literal $(, $args:tt )*) => {{
        return Err(
            Recovered::new(
                <_ as Transitionable<$target>>::transition($recoverable).await,
                anyhow::anyhow!($fmt, $(, $args )*)
            )
        );
    }};
}

/// Try some operation and on failure, assign a given exiting binding to the recovered
/// [`Stateful`](crate::Stateful), then `continue`
///
/// This is useful in a loop:
/// ```
/// use anyhow::anyhow;
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
/// impl TryTransitionable<Running, Init> for Server<Init> {
///     type SuccessStateful = Server<Running>;
///     type FailureStateful = Server<Init>;
///     type Error = anyhow::Error;
///
///     fn try_transition(
///         self,
///     ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
///         return_recover!(self, "We always fail :(")
///     }
/// }
///
/// fn some_loop_func() {
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
        try_transition_continue!($stateful, $original_binding, |_| {})
    }};
    ($stateful:expr, $original_binding:ident, $error_handling:expr) => {{
        match <_ as TryTransitionable<_, _>>::try_transition($stateful) {
            Ok(ok) => ok,
            Err(recovered) => {
                $error_handling(recovered.error);
                $original_binding = recovered.stateful;
                continue;
            }
        }
    }};
}

/// Async variant of [`try_transition_continue`](crate::try_transition_continue). This must be
/// instantiated within an `async` block.
#[macro_export]
macro_rules! try_transition_continue_async {
    ($stateful:expr, $original_binding:ident) => {{
        try_transition_continue_async!($stateful, $original_binding, |_| async {})
    }};
    ($stateful:expr, $original_binding:ident, $error_handling:expr) => {{
        match <_ as AsyncTryTransitionable<_, _>>::try_transition($stateful).await {
            Ok(ok) => ok,
            Err(recovered) => {
                $error_handling(recovered.error).await;
                $original_binding = recovered.stateful;
                continue;
            }
        }
    }};
}

/// Attempt to transition a [`TryTransitionable`](crate::TryTransitionable) within another
/// [`TryTransitionable`](crate::TryTransitionable) machine, recovering the outer state
/// machine if the inner transition fails
#[macro_export]
macro_rules! try_transition_inner_recover {
    ($inner:expr, $recover_constructor:expr) => {{
        try_transition_inner_recover!($inner, _, _, $recover_constructor)
    }};
    ($inner:expr, $inner_recovered_state:ty, $recover_constructor:expr) => {{
        try_transition_inner_recover!($inner, _, $inner_recovered_state, $recover_constructor)
    }};
    ($inner:expr, $inner_target_state: ty, $inner_recovered_state:ty, $recover_constructor:expr) => {{
        match <_ as TryTransitionable<$inner_target_state, $inner_recovered_state>>::try_transition(
            $inner,
        ) {
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

/// Async variant of [`try_transition_inner_recover`](crate::try_transition_inner_recover). This
/// must be instantiated within an `async` block.
///
/// The `$recover_constructor` must return a future (e.g. `|_| async { .. }`)
#[macro_export]
macro_rules! try_transition_inner_recover_async {
    ($inner:expr, $recover_constructor:expr) => {{
        try_transition_inner_recover_async!($inner, _, _, $recover_constructor)
    }};
    ($inner:expr, $inner_recovered_state:ty, $recover_constructor:expr) => {{
        try_transition_inner_recover_async!($inner, _, $inner_recovered_state, $recover_constructor)
    }};
    ($inner:expr, $inner_target_state: ty, $inner_recovered_state:ty, $recover_constructor:expr) => {{
        match <_ as AsyncTryTransitionable<$inner_target_state, $inner_recovered_state>>::try_transition($inner).await
        {
            Ok(ok) => ok,
            Err(recovered) => {
                let inner_recovered =
                    <_ as AsyncTransitionable<$inner_recovered_state>>::transition(
                        recovered.stateful,
                    )
                    .await;
                let new_stateful = $recover_constructor(inner_recovered).await;
                let error = recovered.error;
                return Err(Recovered::new(new_stateful, error));
            }
        }
    }};
}

/// Explicitly transition a [`Transitionable`](crate::Transitionable) [`Stateful`](crate::Stateful)
/// to an optionally provided state. This is useful when there are more than one valid transitions
/// for a [`Stateful`](crate::Stateful).
#[macro_export]
macro_rules! transition {
    ($stateful:expr) => {{
        transition!($stateful, _)
    }};
    ($stateful:expr, $state:ty) => {{
        <_ as Transitionable<$state>>::transition($stateful)
    }};
}

/// Async variant of [`transition`](crate::transition). This must be instantiated within an `async`
/// block.
#[macro_export]
macro_rules! transition_async {
    ($stateful:expr) => {{
        transition_async!($stateful, _)
    }};
    ($stateful:expr, $state:ty) => {{
        <_ as AsyncTransitionable<$state>>::transition($stateful).await
    }};
}

/// Explicitly try to transition a [`TryTransitionable`](crate::TryTransitionable)
/// [`Stateful`](crate::Stateful) to an optionally provided state with an optionally provided
/// recovery state. This is useful when there are more than one valid transitions for a
/// [`Stateful`](crate::Stateful).
#[macro_export]
macro_rules! try_transition {
    ($stateful:expr) => {{
        try_transition!($stateful, _)
    }};
    ($stateful:expr, $next_state:ty) => {{
        try_transition!($stateful, $next_state, _)
    }};
    ($stateful:expr, $next_state:ty, $recovery_state:ty) => {{
        <_ as TryTransitionable<$next_state, $recovery_state>>::try_transition($stateful)
    }};
}

/// Async variant of [`try_transition`](crate::try_transition). This must be instantiated within an
/// `async` block.
#[macro_export]
macro_rules! try_transition_async {
    ($stateful:expr) => {{
        try_transition_async!($stateful, _)
    }};
    ($stateful:expr, $next_state:ty) => {{
        try_transition_async!($stateful, $next_state, _)
    }};
    ($stateful:expr, $next_state:ty, $recovery_state:ty) => {{
        <_ as AsyncTryTransitionable<$next_state, $recovery_state>>::try_transition($stateful).await
    }};
}
