# ApertureC State Machine

A generic state machine library which aims to enforce transitions between states
at compile time. Inspired by
[Hoverbear](https://hoverbear.org/blog/rust-state-machine-pattern/).


## Overview

This library is developed primarily for use in ApertureC, however it can likely
be used anywhere a state machine is needed. Much of this code is intended to
prevent run-time checks of a state machine's state for two primary reasons:
1. Run-time checks can incur a performance penalty
2. Run-time checks are often subject to bugs (though Rust is quite good at
   preventing this with things like [Exhaustive
   Matching](https://rustc-dev-guide.rust-lang.org/pat-exhaustive-checking.html).

By using this library to implement your state machine, you will have guarantees
from the Rust compiler that your state machine cannot end up in an invalid or
unplanned-for state.

Users of this library will be most interested in the following traits:
- [`State`](crate::State): A state that your machine can exist in
- [`Stateful`](crate::Stateful): A thing which has some state associated with
    it. This is likely your state machine.
- [`Transitionable`](crate::Transitionable) and
    [`TryTransitionable`](crate::TryTransitionable): Two traits which must be
    implemented to allow your [`Stateful`](crate::Stateful) to transition
    between [`State`](crate::State)s.


## Examples

### Simple Three-State Machine

The following is a three-state machine, which is driven externally and
transitions between each state always succeed. While perhaps unrealistic, it
provides a decent overview to the crate. Transitions go from
`Alpha->Beta->Gamma->Alpha`.

```rust
use aperturec_state_machine::{State, Stateful, Transitionable};

struct Alpha;
impl State for Alpha {}

struct Beta;
impl State for Beta {}

struct Gamma;
impl State for Gamma {}

struct Machine<S: State> {
    state: S
}
impl<S: State> Stateful for Machine<S> {
    type State = S;
}

impl Transitionable<Beta> for Machine<Alpha> {
    type NextStateful = Machine<Beta>;

    fn transition(self) -> Self::NextStateful {
        Machine { state: Beta }
    }
}

impl Transitionable<Gamma> for Machine<Beta> {
    type NextStateful = Machine<Gamma>;

    fn transition(self) -> Self::NextStateful {
        Machine { state: Gamma }
    }
}

impl Transitionable<Alpha> for Machine<Gamma> {
    type NextStateful = Machine<Alpha>;

    fn transition(self) -> Self::NextStateful {
        Machine { state: Alpha }
    }
}

fn main() {
    let alpha = Machine { state: Alpha };
    let beta: Machine<Beta> = alpha.transition();
    let gamma: Machine<Gamma> = beta.transition();
    let alpha_again: Machine<Alpha> = gamma.transition();
}
```


### Fallible Transitions

What happens if you have a more complex state machine where the transition
between two states may succeed or **may fail**. That's where the
[`TryTransitionable`](crate::TryTransitionable) trait comes in. This trait
ensures that if there is a transition from one state to another which can fail,
the machine returns back to a valid state. Imagine our previous example, but the
transition from `Beta->Gamma` can fail. In this case, we want the whole machine
to return to the `Alpha` state  Imagine our previous example, but the transition
from `Beta->Gamma` can fail. In this case, we want the machine to return to the
`Alpha` state.

```rust
# use aperturec_state_machine::{Bailable, Recovered, State, Stateful, Transitionable, TryTransitionable};
# 
# #[derive(Debug)]
# struct Alpha;
# impl State for Alpha {}
# 
# #[derive(Debug)]
# struct Beta;
# impl State for Beta {}
# 
# #[derive(Debug)]
# struct Gamma;
# impl State for Gamma {}
# 
# #[derive(Debug)]
# struct Machine<S: State> {
#     state: S
# }
# impl<S: State> Stateful for Machine<S> {
#     type State = S;
# }
# 
# impl Transitionable<Beta> for Machine<Alpha> {
#     type NextStateful = Machine<Beta>;
# 
#     fn transition(self) -> Self::NextStateful {
#         Machine { state: Beta }
#     }
# }
# 
# #[async_trait::async_trait]
impl TryTransitionable<Gamma, Alpha> for Machine<Beta> {
    type SuccessStateful = Machine<Gamma>;
    type FailureStateful = Machine<Alpha>;
    type Error = anyhow::Error;

    async fn try_transition(self) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        // Always fail for test sake
        Err(Recovered::new(Machine { state: Alpha }, anyhow::anyhow!("We failed :(")))
    }
}

impl Transitionable<Alpha> for Machine<Beta> {
    type NextStateful = Machine<Alpha>;

    fn transition(self) -> Self::NextStateful {
        Machine { state: Alpha }
    }
}
# 
# impl Transitionable<Alpha> for Machine<Gamma> {
#     type NextStateful = Machine<Alpha>;
# 
#     fn transition(self) -> Self::NextStateful {
#         Machine { state: Alpha }
#     }
# }
# 
# #[tokio::main]
async fn main() {
    let alpha = Machine { state: Alpha };
    let beta: Machine<Beta> = alpha.transition();

    // Try the transition from Beta->Gamma and expect an error
    let alpha_again = beta.try_transition().await.expect_err("We should be failing!");
}
```
