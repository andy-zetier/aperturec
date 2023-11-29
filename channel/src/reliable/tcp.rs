use super::*;

use aperturec_state_machine::*;
use async_trait::async_trait;
use std::io;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::pin::{pin, Pin};
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

#[derive(Stateful, Debug)]
#[state(S)]
pub struct Server<S: State> {
    state: S,
    addr: SocketAddr,
    is_nonblocking: bool,
}

#[derive(State, Debug)]
pub struct Closed;
impl SelfTransitionable for Server<Closed> {}

#[derive(State, Debug)]
pub struct Listening {
    listener: tokio::net::TcpListener,
}
impl SelfTransitionable for Server<Listening> {}

#[derive(State, Debug)]
pub struct Accepted {
    stream: std::net::TcpStream,
    listener: tokio::net::TcpListener,
}

impl AsRef<std::net::TcpStream> for Server<Accepted> {
    fn as_ref(&self) -> &std::net::TcpStream {
        &self.state.stream
    }
}

#[derive(State, Debug)]
pub struct AsyncAccepted {
    stream: tokio::net::TcpStream,
    listener: tokio::net::TcpListener,
}

impl AsRef<tokio::net::TcpStream> for Server<AsyncAccepted> {
    fn as_ref(&self) -> &tokio::net::TcpStream {
        &self.state.stream
    }
}

impl<T: State> Server<T> {
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Server<Closed> {
    pub fn new<A: Into<SocketAddr>>(addr: A) -> Self {
        Server {
            state: Closed,
            addr: addr.into(),
            is_nonblocking: true,
        }
    }

    pub fn new_blocking<A: Into<SocketAddr>>(addr: A) -> Self {
        Server {
            state: Closed,
            addr: addr.into(),
            is_nonblocking: false,
        }
    }
}

#[async_trait]
impl TryTransitionable<Listening, Closed> for Server<Closed> {
    type SuccessStateful = Server<Listening>;
    type FailureStateful = Server<Closed>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let listener = try_recover!(tokio::net::TcpListener::bind(self.addr).await, self);
        let local_addr = try_recover!(listener.local_addr(), self);

        Ok(Server {
            state: Listening { listener },
            addr: local_addr,
            is_nonblocking: self.is_nonblocking,
        })
    }
}

impl Transitionable<Closed> for Server<Listening> {
    type NextStateful = Server<Closed>;

    fn transition(self) -> Self::NextStateful {
        Server {
            addr: self.addr,
            state: Closed,
            is_nonblocking: self.is_nonblocking,
        }
    }
}

#[async_trait]
impl TryTransitionable<Accepted, Listening> for Server<Listening> {
    type SuccessStateful = Server<Accepted>;
    type FailureStateful = Server<Listening>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let (stream, _) = try_recover!(self.state.listener.accept().await, self, Listening);
        let stream = try_recover!(stream.into_std(), self, Listening);

        // Disable Nagle's algorithm for lower latency
        try_recover!(stream.set_nodelay(true), self, Listening);

        try_recover!(stream.set_nonblocking(self.is_nonblocking), self, Listening);

        Ok(Server {
            state: Accepted {
                stream,
                listener: self.state.listener,
            },
            addr: self.addr,
            is_nonblocking: self.is_nonblocking,
        })
    }
}

#[async_trait]
impl TryTransitionable<AsyncAccepted, Listening> for Server<Accepted> {
    type SuccessStateful = Server<AsyncAccepted>;
    type FailureStateful = Server<Listening>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let std = try_recover!(self.state.stream.try_clone(), self, Listening);
        try_recover!(std.set_nonblocking(true), self);
        let stream = try_recover!(tokio::net::TcpStream::from_std(std), self, Listening);
        Ok(Server {
            state: AsyncAccepted {
                stream,
                listener: self.state.listener,
            },
            addr: self.addr,
            is_nonblocking: true,
        })
    }
}

impl Transitionable<Listening> for Server<Accepted> {
    type NextStateful = Server<Listening>;

    fn transition(self) -> Self::NextStateful {
        Server {
            state: Listening {
                listener: self.state.listener,
            },
            addr: self.addr,
            is_nonblocking: self.is_nonblocking,
        }
    }
}

impl Transitionable<Listening> for Server<AsyncAccepted> {
    type NextStateful = Server<Listening>;

    fn transition(self) -> Self::NextStateful {
        Server {
            state: Listening {
                listener: self.state.listener,
            },
            addr: self.addr,
            is_nonblocking: self.is_nonblocking,
        }
    }
}

impl Read for Server<Accepted> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.state.stream.read(buf)
    }
}

impl Write for Server<Accepted> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.state.stream.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.state.stream.flush()
    }
}

impl NonblockableIO for Server<Accepted> {
    fn is_nonblocking(&self) -> bool {
        self.is_nonblocking
    }
}

impl AsyncRead for Server<AsyncAccepted> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        pin!(&mut self.state.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for Server<AsyncAccepted> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        pin!(&mut self.state.stream).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        pin!(&mut self.state.stream).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        pin!(&mut self.state.stream).poll_shutdown(cx)
    }
}

#[derive(Stateful, Debug)]
#[state(S)]
pub struct Client<S: State> {
    state: S,
    addr: SocketAddr,
    is_nonblocking: bool,
}

impl SelfTransitionable for Client<Closed> {}

#[derive(State, Debug)]
pub struct Connected {
    stream: std::net::TcpStream,
}

#[derive(State, Debug)]
pub struct AsyncConnected {
    stream: tokio::net::TcpStream,
}

impl Client<Closed> {
    pub fn new<A: Into<SocketAddr>>(addr: A) -> Self {
        Client {
            state: Closed,
            addr: addr.into(),
            is_nonblocking: true,
        }
    }

    pub fn new_blocking<A: Into<SocketAddr>>(addr: A) -> Self {
        Client {
            state: Closed,
            addr: addr.into(),
            is_nonblocking: false,
        }
    }
}

impl Clone for Client<Connected> {
    fn clone(&self) -> Self {
        Self {
            state: Connected {
                stream: self
                    .state
                    .stream
                    .try_clone()
                    .expect("clone connected client"),
            },
            addr: self.addr.clone(),
            is_nonblocking: self.is_nonblocking,
        }
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
        let socket = match self.addr {
            SocketAddr::V4(_) => try_recover!(tokio::net::TcpSocket::new_v4(), self),
            SocketAddr::V6(_) => try_recover!(tokio::net::TcpSocket::new_v6(), self),
        };

        let stream = try_recover!(socket.connect(self.addr).await, self);
        let stream = try_recover!(stream.into_std(), self);

        // Disable Nagle's algorithm for lower latency
        try_recover!(stream.set_nodelay(true), self);

        try_recover!(stream.set_nonblocking(self.is_nonblocking), self);

        Ok(Client {
            addr: self.addr,
            state: Connected { stream },
            is_nonblocking: self.is_nonblocking,
        })
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
        let std: std::net::TcpStream = try_recover!(self.state.stream.try_clone(), self);
        try_recover!(std.set_nonblocking(true), self, Closed);
        let stream = try_recover!(tokio::net::TcpStream::from_std(std), self);

        Ok(Client {
            state: AsyncConnected { stream },
            addr: self.addr,
            is_nonblocking: true,
        })
    }
}

impl Transitionable<Closed> for Client<Connected> {
    type NextStateful = Client<Closed>;

    fn transition(self) -> Self::NextStateful {
        Client {
            state: Closed,
            addr: self.addr,
            is_nonblocking: self.is_nonblocking,
        }
    }
}

impl Read for Client<Connected> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.state.stream.read(buf)
    }
}

impl Write for Client<Connected> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.state.stream.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.state.stream.flush()
    }
}

impl NonblockableIO for Client<Connected> {
    fn is_nonblocking(&self) -> bool {
        self.is_nonblocking
    }
}

impl AsyncRead for Client<AsyncConnected> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        pin!(&mut self.state.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for Client<AsyncConnected> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        pin!(&mut self.state.stream).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        pin!(&mut self.state.stream).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        pin!(&mut self.state.stream).poll_shutdown(cx)
    }
}
