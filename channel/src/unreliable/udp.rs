use crate::unreliable::*;

use aperturec_state_machine::*;
use async_trait::async_trait;
use std::net::SocketAddr;

#[derive(Stateful, Debug)]
#[state(S)]
pub struct Server<S: State> {
    pub state: S,
    local_addr: SocketAddr,
}

#[derive(Stateful, Debug)]
#[state(S)]
pub struct Client<S: State> {
    state: S,
    remote_addr: SocketAddr,
}

#[derive(State, Debug)]
pub struct Closed;
impl SelfTransitionable for Server<Closed> {}
impl SelfTransitionable for Client<Closed> {}

#[derive(State, Debug)]
pub struct Listening {
    pub socket: std::net::UdpSocket,
}

#[derive(State, Debug)]
pub struct AsyncListening {
    socket: tokio::net::UdpSocket,
}

#[derive(State, Debug)]
pub struct Connected {
    pub socket: std::net::UdpSocket,
}

#[derive(State, Debug)]
pub struct AsyncConnected {
    socket: tokio::net::UdpSocket,
}

impl Server<Closed> {
    pub fn new<A: Into<SocketAddr>>(local_addr: A) -> Self {
        Server {
            state: Closed,
            local_addr: local_addr.into(),
        }
    }
}

impl Client<Closed> {
    pub fn new<A: Into<SocketAddr>>(remote_addr: A) -> Self {
        Client {
            state: Closed,
            remote_addr: remote_addr.into(),
        }
    }
}

async fn do_udp_bind<'a, A>(addr: &'a A) -> anyhow::Result<std::net::UdpSocket>
where
    A: std::net::ToSocketAddrs + tokio::net::ToSocketAddrs,
{
    let tokio_socket = tokio::net::UdpSocket::bind(addr).await?;
    Ok(tokio_socket.into_std()?)
}

fn socket_sync_to_async(udp_socket: &std::net::UdpSocket) -> anyhow::Result<tokio::net::UdpSocket> {
    let cpy = udp_socket.try_clone()?;
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

        Ok(Server {
            state: Listening { socket },
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

#[async_trait]
impl TryTransitionable<AsyncListening, Closed> for Server<Listening> {
    type SuccessStateful = Server<AsyncListening>;
    type FailureStateful = Server<Closed>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let socket = try_recover!(socket_sync_to_async(&self.state.socket), self);
        Ok(Server {
            state: AsyncListening { socket },
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
        let socket = try_recover!(do_udp_bind(&("0.0.0.0", 0)).await, self);
        try_recover!(socket.connect(self.remote_addr), self);
        Ok(Client {
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
            remote_addr: self.remote_addr,
        })
    }
}

impl RawReceiver for Server<Listening> {
    fn receive(&mut self, buf: &mut [u8]) -> anyhow::Result<usize> {
        let (recv_amt, _) = self.state.socket.recv_from(buf)?;
        Ok(recv_amt)
    }
}

impl RawSender for Client<Connected> {
    fn send(&mut self, buf: &[u8]) -> anyhow::Result<usize> {
        Ok(self.state.socket.send(buf)?)
    }
}

#[async_trait]
impl AsyncRawReceiver for Server<AsyncListening> {
    async fn receive(&mut self, buf: &mut [u8]) -> anyhow::Result<usize> {
        let (recv_amt, _) = self.state.socket.recv_from(buf).await?;
        Ok(recv_amt)
    }
}

#[async_trait]
impl AsyncRawSender for Client<AsyncConnected> {
    async fn send(&mut self, buf: &[u8]) -> anyhow::Result<usize> {
        Ok(self.state.socket.send(buf).await?)
    }
}
