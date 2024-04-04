use crate::unreliable::*;

use aperturec_state_machine::*;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

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
}
impl SelfTransitionable for Server<Listening> {}

#[derive(State, Debug)]
pub struct AsyncListening {
    socket: tokio::net::UdpSocket,
}
impl SelfTransitionable for Server<AsyncListening> {}

#[derive(State, Debug)]
pub struct Connected {
    socket: std::net::UdpSocket,
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
        let remote_addr: SocketAddr = remote_addr.into();
        Client {
            state: Closed,
            local_addr: local_addr
                .into()
                .unwrap_or_else(|| unspecified_peer_socket_addr(remote_addr)),
            remote_addr,
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
            },
            local_addr: self.local_addr,
        }
    }
}

async fn do_udp_bind_async<A>(addr: &A) -> anyhow::Result<tokio::net::UdpSocket>
where
    A: tokio::net::ToSocketAddrs,
{
    Ok(tokio::net::UdpSocket::bind(addr).await?)
}

fn do_udp_bind<A>(addr: &A) -> anyhow::Result<std::net::UdpSocket>
where
    A: std::net::ToSocketAddrs,
{
    Ok(std::net::UdpSocket::bind(addr)?)
}

fn unspecified_peer_socket_addr(sa: SocketAddr) -> SocketAddr {
    let local_ip = sa.ip();
    let remote_ip: IpAddr = match (local_ip, local_ip.is_loopback()) {
        (IpAddr::V4(_), false) => Ipv4Addr::UNSPECIFIED.into(),
        (IpAddr::V4(_), true) => Ipv4Addr::LOCALHOST.into(),
        (IpAddr::V6(_), false) => Ipv6Addr::UNSPECIFIED.into(),
        (IpAddr::V6(_), true) => Ipv6Addr::LOCALHOST.into(),
    };
    SocketAddr::from((remote_ip, 0))
}

impl TryTransitionable<Listening, Closed> for Server<Closed> {
    type SuccessStateful = Server<Listening>;
    type FailureStateful = Server<Closed>;
    type Error = anyhow::Error;

    fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let socket = try_recover!(do_udp_bind(&self.local_addr), self);
        try_recover!(socket.set_nonblocking(false), self);
        try_recover!(socket.set_read_timeout(None), self);
        let local_addr = try_recover!(socket.local_addr(), self);

        Ok(Server {
            state: Listening { socket },
            local_addr,
        })
    }
}

impl AsyncTryTransitionable<AsyncListening, Closed> for Server<Closed> {
    type SuccessStateful = Server<AsyncListening>;
    type FailureStateful = Server<Closed>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let socket = try_recover_async!(do_udp_bind_async(&self.local_addr), self);
        let local_addr = try_recover!(socket.local_addr(), self);

        Ok(Server {
            state: AsyncListening { socket },
            local_addr,
        })
    }
}

impl TryTransitionable<Connected, Listening> for Server<Listening> {
    type SuccessStateful = Server<Connected>;
    type FailureStateful = Server<Listening>;
    type Error = anyhow::Error;

    fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let (_, remote_addr) = try_recover!(self.state.socket.peek_from(&mut []), self, Listening);
        try_recover!(self.state.socket.connect(remote_addr), self, Listening);

        Ok(Server {
            state: Connected {
                socket: self.state.socket,
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

impl AsyncTryTransitionable<AsyncConnected, AsyncListening> for Server<AsyncListening> {
    type SuccessStateful = Server<AsyncConnected>;
    type FailureStateful = Server<AsyncListening>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let (_, remote_addr) =
            try_recover_async!(self.state.socket.peek_from(&mut []), self, AsyncListening);
        try_recover_async!(self.state.socket.connect(remote_addr), self, AsyncListening);

        Ok(Server {
            state: AsyncConnected {
                socket: self.state.socket,
            },
            local_addr: self.local_addr,
        })
    }
}

impl AsyncTransitionable<Closed> for Server<AsyncListening> {
    type NextStateful = Server<Closed>;

    async fn transition(self) -> Self::NextStateful {
        Server {
            state: Closed,
            local_addr: self.local_addr,
        }
    }
}

impl AsyncTransitionable<Closed> for Server<AsyncConnected> {
    type NextStateful = Server<Closed>;

    async fn transition(self) -> Self::NextStateful {
        Server {
            state: Closed,
            local_addr: self.local_addr,
        }
    }
}

impl TryTransitionable<Connected, Closed> for Client<Closed> {
    type SuccessStateful = Client<Connected>;
    type FailureStateful = Client<Closed>;
    type Error = anyhow::Error;

    fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let socket = try_recover!(do_udp_bind(&self.local_addr), self);
        try_recover!(socket.set_nonblocking(false), self);
        try_recover!(socket.set_read_timeout(None), self);
        try_recover!(socket.connect(self.remote_addr), self);

        Ok(Client {
            local_addr: try_recover!(socket.local_addr(), self),
            state: Connected { socket },
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

impl AsyncTryTransitionable<AsyncConnected, Closed> for Client<Closed> {
    type SuccessStateful = Client<AsyncConnected>;
    type FailureStateful = Client<Closed>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let socket = try_recover_async!(do_udp_bind_async(&self.local_addr), self);
        try_recover_async!(socket.connect(self.remote_addr), self);

        Ok(Client {
            local_addr: try_recover!(socket.local_addr(), self),
            state: AsyncConnected { socket },
            remote_addr: self.remote_addr,
        })
    }
}

impl AsyncTransitionable<Closed> for Client<AsyncConnected> {
    type NextStateful = Client<Closed>;

    async fn transition(self) -> Self::NextStateful {
        Client {
            state: Closed,
            local_addr: self.local_addr,
            remote_addr: self.remote_addr,
        }
    }
}

impl RawReceiver for Server<Connected> {
    fn receive(&mut self, buf: &mut [u8]) -> anyhow::Result<usize> {
        Ok(self.state.socket.recv(buf)?)
    }
}

impl RawReceiver for Client<Connected> {
    fn receive(&mut self, buf: &mut [u8]) -> anyhow::Result<usize> {
        Ok(self.state.socket.recv(buf)?)
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
        Ok(self.state.socket.recv(buf).await?)
    }
}

impl AsyncRawReceiver for Client<AsyncConnected> {
    async fn receive(&mut self, buf: &mut [u8]) -> anyhow::Result<usize> {
        Ok(self.state.socket.recv(buf).await?)
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
