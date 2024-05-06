#![doc = include_str!("../README.md")]

#[macro_use]
mod macros;

/// Derive for [`State`] trait
/// ```
/// # use aperturec_state_machine::*;
/// #[derive(State)]
/// struct MyState;
/// ```
pub use aperturec_state_machine_derive::State;

/// Derive for [`Stateful`] trait
///
/// Users of this derive must specify the [`State`] with an attrubute. For example:
/// ```
/// # use aperturec_state_machine::*;
/// #[derive(Stateful)]
/// #[state(S)]
/// struct MyMachine<S: State> {
///     state: S,
/// }
/// ```
pub use aperturec_state_machine_derive::Stateful;

/// Derive for [`SelfTransitionable`]
///
/// Users must also implement [`Stateful`], and can do so via the provided derive
/// ```
/// # use aperturec_state_machine::*;
/// #[derive(Stateful, SelfTransitionable)]
/// #[state(S)]
/// struct MyMachine<S: State> {
///     state: S,
/// }
/// ```
pub use aperturec_state_machine_derive::SelfTransitionable;

/// A state which a [`Stateful`] can be in
///
/// The [`State`] trait itself is bare, only used for type enforcement of generic [`Stateful`] objects.
pub trait State {}

/// Types which can be in one of any given number of [`State`]s at a given time
pub trait Stateful {
    /// The `State` type a given `Stateful` can be in
    type State: State;
}

/// [`Stateful`]s which can transition between different [`State`]s
///
/// A transition between [`State`]s for [`Stateful`]s implementing [`Transitionable`] **must** be
/// successful. If the transition between different [`State`]s can fail, see the
/// [`TryTransitionable`] trait.
pub trait Transitionable<N>: Stateful
where
    N: State,
{
    /// The [`Stateful`] that the transition will force the current [`Stateful`] into
    type NextStateful: Stateful<State = N>;

    /// Transition [`Self`] into the [`Self::NextStateful`]
    ///
    /// Note that [`Self`] is consumed by this method. This enforces that a [`Stateful`] cannot
    /// exist in multiple [`State`]s at the same time.
    fn transition(self) -> Self::NextStateful;
}

mod async_transitionable {
    use super::*;

    /// Async variant of [`Transitionable`]
    #[trait_variant::make(AsyncTransitionable: Send + Sync)]
    #[allow(dead_code)]
    pub trait LocalAsyncTransitionable<N>: Stateful
    where
        N: State,
    {
        /// The [`Stateful`] that the transition will force the current [`Stateful`] into
        type NextStateful: Stateful<State = N>;

        /// Transition [`Self`] into the [`Self::NextStateful`]
        ///
        /// Note that [`Self`] is consumed by this method. This enforces that a [`Stateful`] cannot
        /// exist in multiple [`State`]s at the same time.
        async fn transition(self) -> Self::NextStateful;
    }
}
pub use async_transitionable::AsyncTransitionable;

impl<S, N> AsyncTransitionable<N> for S
where
    S: Stateful + Transitionable<N> + Send + Sync,
    N: State,
{
    type NextStateful = S::NextStateful;

    async fn transition(self) -> Self::NextStateful {
        <Self as Transitionable<N>>::transition(self)
    }
}

/// [`Stateful`]s which can transition back to themselves
///
/// This is a helper trait enabling simpler implementations of [`TryTransitionable`] for
/// [`Stateful`]s where the [`TryTransitionable::FailureStateful`] is the current [`Stateful`]. A
/// blanket implementation for [`Transitionable`] exists so that all [`SelfTransitionable`]
/// [`Stateful`]s can transition back to themselves.
pub trait SelfTransitionable: Stateful {}

impl<S: SelfTransitionable> Transitionable<S::State> for S {
    type NextStateful = Self;
    fn transition(self) -> Self::NextStateful {
        self
    }
}

/// The error value of a failed [`TryTransitionable`]
///
/// If a [`TryTransitionable::try_transition`] fails, a [`Recovered`] struct is returned. This
/// struct contains the recovered stateful of the failed transition, and an error value which
/// caused the `TryTransitionable::try_transition`] to fail.
#[derive(Debug)]
pub struct Recovered<S, E>
where
    S: Stateful,
    E: Into<anyhow::Error>,
{
    /// The new [`Stateful`] after a failed [`TryTransitionable::try_transition`]
    pub stateful: S,

    /// The error which caused [`TryTransitionable::try_transition`] to fail
    pub error: E,
}

impl<S, E> Recovered<S, E>
where
    S: Stateful,
    E: Into<anyhow::Error>,
{
    /// Convenience function to create a new [`Recovered`] struct
    pub fn new(stateful: S, error: E) -> Self {
        Recovered { stateful, error }
    }
}

/// [`Stateful`]s which can transition between different [`State`]s, but **may fail** when
/// attempting to do so
///
/// A transition between [`State`]s for a [`Stateful`] implementing [`TryTransitionable`] **may
/// fail**. If the [`Self::try_transition`] succeeds, a new [`TryTransitionable::SuccessStateful`]
/// is returned. However if the [`Self::try_transition`] fails, a [`Recovered`] is returned. Type
/// bounds on the [`Self::FailureStateful`] type ensures that the current [`Stateful`] can
/// [`Transitionable::transition`] to [`Self::FailureStateful`] without error.
///
/// Many implementors will return the current [`Stateful`] if they fail. [`Stateful`]s with this
/// behavior should implement the [`SelfTransitionable`] trait.
pub trait TryTransitionable<N, R>: Transitionable<R>
where
    N: State,
    R: State,
{
    /// New [`Stateful`] if the [`Self::try_transition`] succeeds
    type SuccessStateful: Stateful<State = N>;

    /// New [`Stateful`] if the [`Self::try_transition`] fails. This will be encapsulated in a
    /// [`Recovered`]
    type FailureStateful: Stateful<State = R>;

    /// The error which caused the [`Self::try_transition`] to fail
    type Error: Into<anyhow::Error>;

    /// Attempt to transition the current [`Stateful`] into [`Self::SuccessStateful`]
    ///
    /// On success, a new [`Self::SuccessStateful`] is returned. On failure, a [`Recovered`] is
    /// returned, with the new [`Self::FailureStateful`] and the error which caused the failure
    fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>>;
}

mod async_try_transitionable {
    use super::*;

    /// Async variant of [`TryTransitionable`]
    #[trait_variant::make(AsyncTryTransitionable: Send + Sync)]
    #[allow(dead_code)]
    pub trait LocalAsyncTryTransitionable<N, R>: AsyncTransitionable<R>
    where
        N: State,
        R: State,
    {
        /// New [`Stateful`] if the [`Self::try_transition`] succeeds
        type SuccessStateful: Stateful<State = N>;

        /// New [`Stateful`] if the [`Self::try_transition`] fails. This will be encapsulated in a
        /// [`Recovered`]
        type FailureStateful: Stateful<State = R>;

        /// The error which caused the [`Self::try_transition`] to fail
        type Error: Into<anyhow::Error>;

        /// Attempt to transition the current [`Stateful`] into [`Self::SuccessStateful`]
        ///
        /// On success, a new [`Self::SuccessStateful`] is returned. On failure, a [`Recovered`] is
        /// returned, with the new [`Self::FailureStateful`] and the error which caused the failure
        async fn try_transition(
            self,
        ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>>;
    }
}
pub use async_try_transitionable::AsyncTryTransitionable;

/// Convenience trait to convert [`Result<S, Recovered<F, E>>`] into the error `E`
///
/// Useful for when users of a [`TryTransitionable`] [`Stateful`] do not want to recover the state
/// machine in the [`Recovered`] state.
///
/// Note this is implemented via a blanket implementation on all [`Result<S, Recovered<F, E>>`]
/// types. It should probably not be implemented manually.
pub trait Bailable<N, R>
where
    N: State,
    R: State,
{
    /// The [`Stateful`] a [`TryTransitionable`] returns on success
    type SuccessStateful: Stateful<State = N>;
    /// The error a [`TryTransitionable`] returns via [`Recovered`] on error
    type Error: Into<anyhow::Error>;

    /// Get the underlying error from a failed [`TryTransitionable::try_transition`], or the new
    /// [`Stateful`] if the transition was successful.
    fn into_error(self) -> Result<Self::SuccessStateful, Self::Error>;
}

impl<N, R, S, F, E> Bailable<N, R> for Result<S, Recovered<F, E>>
where
    N: State,
    R: State,
    S: Stateful<State = N>,
    F: Stateful<State = R>,
    E: Into<anyhow::Error>,
{
    type SuccessStateful = S;
    type Error = E;

    fn into_error(self) -> Result<Self::SuccessStateful, Self::Error> {
        self.map_err(|recovered| recovered.error)
    }
}

#[cfg(test)]
mod test {
    use crate::{Recovered, State, Stateful, Transitionable, TryTransitionable};

    macro_rules! empty_states {
        ($($state:ident),+) => {
            $(
                #[derive(State, Debug, Eq, PartialEq)]
                struct $state;
            )*
        };
    }

    macro_rules! transitions {
        ($machine:ident, $(($start:ident,$end:ident)),+) => {
            #[derive(Stateful, Debug, Eq, PartialEq)]
            #[state(S)]
            struct $machine<S: State> {
                state: S
            }
            $(
                impl Transitionable<$end> for $machine<$start> {
                    type NextStateful = $machine<$end>;

                    fn transition(self) -> Self::NextStateful {
                        $machine { state: $end }
                    }
                }
            )*
        };
    }

    #[test]
    fn two_state() {
        empty_states!(A, B);
        transitions!(Machine, (A, B), (B, A));

        let a = Machine { state: A };
        let b: Machine<B> = a.transition();
        assert_eq!(b.state, B);
    }

    #[test]
    fn several_state() {
        empty_states!(A, B, C, D, E);
        transitions!(
            Machine,
            (A, B),
            (B, C),
            (C, D),
            (D, E),
            (E, A),
            (D, C),
            (D, B)
        );

        let a = Machine { state: A };
        let b: Machine<B> = a.transition();
        let c: Machine<C> = b.transition();
        let d: Machine<D> = c.transition();
        let b: Machine<B> = <_ as Transitionable<B>>::transition(d);
        assert_eq!(b.state, B);
        let c: Machine<C> = b.transition();
        let d: Machine<D> = c.transition();
        let c: Machine<C> = <_ as Transitionable<C>>::transition(d);
        assert_eq!(c.state, C);
        let d: Machine<D> = c.transition();
        assert_eq!(d.state, D);
        let e: Machine<E> = <_ as Transitionable<E>>::transition(d);
        assert_eq!(e.state, E);
        let a: Machine<A> = e.transition();
        assert_eq!(a.state, A);
    }

    #[tokio::test]
    async fn fallible_transition() {
        empty_states!(A, B, C);
        transitions!(Machine, (A, B), (B, A), (C, A));

        impl Machine<B> {
            fn should_transition(&mut self) -> bool {
                unsafe {
                    static mut SHOULD_TRANSITION: bool = false;

                    if !SHOULD_TRANSITION {
                        SHOULD_TRANSITION = true;
                        false
                    } else {
                        true
                    }
                }
            }
        }

        impl TryTransitionable<C, A> for Machine<B> {
            type SuccessStateful = Machine<C>;
            type FailureStateful = Machine<A>;
            type Error = anyhow::Error;

            fn try_transition(
                mut self,
            ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>>
            {
                if self.should_transition() {
                    Ok(Machine { state: C })
                } else {
                    Err(Recovered {
                        stateful: Machine { state: A },
                        error: anyhow::anyhow!("Failed"),
                    })
                }
            }
        }

        let a = Machine { state: A };
        let b: Machine<B> = a.transition();

        let recovered = b.try_transition().err().unwrap();
        assert_eq!(recovered.stateful.state, A);

        let success = recovered.stateful.transition().try_transition().unwrap();
        assert_eq!(success.state, C);
    }
}
