use crate::unreliable::*;

use aperturec_state_machine::*;
use async_trait::async_trait;
use std::net::SocketAddr;

#[derive(Stateful, Debug)]
#[state(S)]
pub struct Server<S: State> {
    state: S,
    local_addr: SocketAddr,
}

#[derive(Stateful, Debug)]
#[state(S)]
pub struct Client<S: State> {
    state: S,
    local_addr: SocketAddr,
    remote_addr: SocketAddr,
}

#[derive(State, Debug)]
pub struct Closed;
impl SelfTransitionable for Server<Closed> {}
impl SelfTransitionable for Client<Closed> {}

#[derive(State, Debug)]
pub struct Listening {
    socket: std::net::UdpSocket,
    last_remote_addr: SocketAddr,
}

#[derive(State, Debug)]
pub struct Connected {
    socket: std::net::UdpSocket,
    remote_addr: SocketAddr,
}

#[derive(State, Debug)]
pub struct AsyncConnected {
    socket: tokio::net::UdpSocket,
}

impl<T: State> Server<T> {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

impl Server<Closed> {
    pub fn new<A: Into<SocketAddr>>(local_addr: A) -> Self {
        Server {
            state: Closed,
            local_addr: local_addr.into(),
        }
    }

    pub fn new_blocking<A: Into<SocketAddr>>(local_addr: A) -> Self {
        Server {
            state: Closed,
            local_addr: local_addr.into(),
        }
    }
}

impl<T: State> Client<T> {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

impl Client<Closed> {
    pub fn new(
        remote_addr: impl Into<SocketAddr>,
        local_addr: impl Into<Option<SocketAddr>>,
    ) -> Self {
        Client {
            state: Closed,
            local_addr: if let Some(addr) = local_addr.into() {
                addr
            } else {
                "0.0.0.0:0".parse().unwrap()
            },
            remote_addr: remote_addr.into(),
        }
    }

    pub fn new_blocking(
        remote_addr: impl Into<SocketAddr>,
        local_addr: impl Into<Option<SocketAddr>>,
    ) -> Self {
        Client {
            state: Closed,
            local_addr: if let Some(addr) = local_addr.into() {
                addr
            } else {
                "0.0.0.0:0".parse().unwrap()
            },
            remote_addr: remote_addr.into(),
        }
    }
}

impl Clone for Client<Connected> {
    fn clone(&self) -> Self {
        Self {
            state: Connected {
                socket: self
                    .state
                    .socket
                    .try_clone()
                    .expect("clone connected client"),
                remote_addr: self.remote_addr,
            },
            local_addr: self.local_addr,
            remote_addr: self.remote_addr,
        }
    }
}

impl Clone for Server<Listening> {
    fn clone(&self) -> Self {
        Self {
            state: Listening {
                socket: self
                    .state
                    .socket
                    .try_clone()
                    .expect("clone listening server"),
                last_remote_addr: self.state.last_remote_addr,
            },
            local_addr: self.local_addr,
        }
    }
}

impl Clone for Server<Connected> {
    fn clone(&self) -> Self {
        Self {
            state: Connected {
                socket: self
                    .state
                    .socket
                    .try_clone()
                    .expect("clone connected server"),
                remote_addr: self.state.remote_addr,
            },
            local_addr: self.local_addr,
        }
    }
}

async fn do_udp_bind<A>(addr: &A) -> anyhow::Result<std::net::UdpSocket>
where
    A: std::net::ToSocketAddrs + tokio::net::ToSocketAddrs,
{
    let tokio_socket = tokio::net::UdpSocket::bind(addr).await?;
    Ok(tokio_socket.into_std()?)
}

fn socket_sync_to_async(udp_socket: &std::net::UdpSocket) -> anyhow::Result<tokio::net::UdpSocket> {
    let cpy = udp_socket.try_clone()?;
    cpy.set_nonblocking(true)?;
    Ok(tokio::net::UdpSocket::from_std(cpy)?)
}

#[async_trait]
impl TryTransitionable<Listening, Closed> for Server<Closed> {
    type SuccessStateful = Server<Listening>;
    type FailureStateful = Server<Closed>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let socket = try_recover!(do_udp_bind(&self.local_addr).await, self);
        try_recover!(socket.set_nonblocking(false), self);
        let local_addr = try_recover!(socket.local_addr(), self);

        Ok(Server {
            state: Listening {
                socket,
                last_remote_addr: "0.0.0.0:0".parse().unwrap(),
            },
            local_addr,
        })
    }
}

#[async_trait]
impl TryTransitionable<Connected, Closed> for Server<Listening> {
    type SuccessStateful = Server<Connected>;
    type FailureStateful = Server<Closed>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        try_recover!(self.state.socket.connect(self.state.last_remote_addr), self);

        Ok(Server {
            state: Connected {
                socket: self.state.socket,
                remote_addr: self.state.last_remote_addr,
            },
            local_addr: self.local_addr,
        })
    }
}

impl Transitionable<Closed> for Server<Listening> {
    type NextStateful = Server<Closed>;

    fn transition(self) -> Self::NextStateful {
        Server {
            state: Closed,
            local_addr: self.local_addr,
        }
    }
}

impl Transitionable<Closed> for Server<Connected> {
    type NextStateful = Server<Closed>;

    fn transition(self) -> Self::NextStateful {
        Server {
            state: Closed,
            local_addr: self.local_addr,
        }
    }
}

#[async_trait]
impl TryTransitionable<AsyncConnected, Closed> for Server<Connected> {
    type SuccessStateful = Server<AsyncConnected>;
    type FailureStateful = Server<Closed>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let socket = try_recover!(socket_sync_to_async(&self.state.socket), self);
        Ok(Server {
            state: AsyncConnected { socket },
            local_addr: self.local_addr,
        })
    }
}

#[async_trait]
impl TryTransitionable<Connected, Closed> for Client<Closed> {
    type SuccessStateful = Client<Connected>;
    type FailureStateful = Client<Closed>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let socket = try_recover!(do_udp_bind(&self.local_addr).await, self);
        try_recover!(socket.set_nonblocking(false), self);
        try_recover!(socket.connect(self.remote_addr), self);

        Ok(Client {
            local_addr: socket.local_addr().unwrap(),
            state: Connected {
                socket,
                remote_addr: self.remote_addr,
            },
            remote_addr: self.remote_addr,
        })
    }
}

impl Transitionable<Closed> for Client<Connected> {
    type NextStateful = Client<Closed>;

    fn transition(self) -> Self::NextStateful {
        Client {
            state: Closed,
            local_addr: self.local_addr,
            remote_addr: self.remote_addr,
        }
    }
}

#[async_trait]
impl TryTransitionable<AsyncConnected, Closed> for Client<Connected> {
    type SuccessStateful = Client<AsyncConnected>;
    type FailureStateful = Client<Closed>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let socket = try_recover!(socket_sync_to_async(&self.state.socket), self);
        Ok(Client {
            state: AsyncConnected { socket },
            local_addr: self.local_addr,
            remote_addr: self.remote_addr,
        })
    }
}

impl RawReceiver for Server<Listening> {
    fn receive(&mut self, buf: &mut [u8]) -> anyhow::Result<usize> {
        let (recv_amt, src_addr) = self.state.socket.recv_from(buf)?;
        self.state.last_remote_addr = src_addr;
        Ok(recv_amt)
    }
}

impl RawReceiver for Server<Connected> {
    fn receive(&mut self, buf: &mut [u8]) -> anyhow::Result<usize> {
        let (recv_amt, src_addr) = self.state.socket.recv_from(buf)?;
        assert_eq!(src_addr, self.state.remote_addr);
        Ok(recv_amt)
    }
}

impl RawReceiver for Client<Connected> {
    fn receive(&mut self, buf: &mut [u8]) -> anyhow::Result<usize> {
        let (recv_amt, _) = self.state.socket.recv_from(buf)?;
        Ok(recv_amt)
    }
}

impl RawSender for Server<Connected> {
    fn send(&mut self, buf: &[u8]) -> anyhow::Result<usize> {
        Ok(self.state.socket.send(buf)?)
    }
}

impl RawSender for Client<Connected> {
    fn send(&mut self, buf: &[u8]) -> anyhow::Result<usize> {
        Ok(self.state.socket.send(buf)?)
    }
}

impl AsyncRawReceiver for Server<AsyncConnected> {
    async fn receive(&mut self, buf: &mut [u8]) -> anyhow::Result<usize> {
        let (recv_amt, _) = self.state.socket.recv_from(buf).await?;
        Ok(recv_amt)
    }
}

impl AsyncRawReceiver for Client<AsyncConnected> {
    async fn receive(&mut self, buf: &mut [u8]) -> anyhow::Result<usize> {
        let (recv_amt, _) = self.state.socket.recv_from(buf).await?;
        Ok(recv_amt)
    }
}

impl AsyncRawSender for Server<AsyncConnected> {
    async fn send(&mut self, buf: &[u8]) -> anyhow::Result<usize> {
        Ok(self.state.socket.send(buf).await?)
    }
}

impl AsyncRawSender for Client<AsyncConnected> {
    async fn send(&mut self, buf: &[u8]) -> anyhow::Result<usize> {
        Ok(self.state.socket.send(buf).await?)
    }
}
