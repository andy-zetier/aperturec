use aperturec_state_machine::*;
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
}

#[derive(State, Debug)]
pub struct Closed;
impl SelfTransitionable for Server<Closed> {}

#[derive(State, Debug)]
pub struct Listening {
    listener: std::net::TcpListener,
}
impl SelfTransitionable for Server<Listening> {}

#[derive(State, Debug)]
pub struct AsyncListening {
    listener: tokio::net::TcpListener,
}
impl SelfTransitionable for Server<AsyncListening> {}

#[derive(State, Debug)]
pub struct Accepted {
    stream: std::net::TcpStream,
    listener: std::net::TcpListener,
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
        }
    }
}

impl TryTransitionable<Listening, Closed> for Server<Closed> {
    type SuccessStateful = Server<Listening>;
    type FailureStateful = Server<Closed>;
    type Error = anyhow::Error;

    fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let listener = try_recover!(std::net::TcpListener::bind(self.addr), self);
        let local_addr = try_recover!(listener.local_addr(), self);

        Ok(Server {
            state: Listening { listener },
            addr: local_addr,
        })
    }
}

impl Transitionable<Closed> for Server<Listening> {
    type NextStateful = Server<Closed>;

    fn transition(self) -> Self::NextStateful {
        Server {
            addr: self.addr,
            state: Closed,
        }
    }
}

impl AsyncTryTransitionable<AsyncListening, Closed> for Server<Closed> {
    type SuccessStateful = Server<AsyncListening>;
    type FailureStateful = Server<Closed>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let listener = try_recover_async!(tokio::net::TcpListener::bind(self.addr), self);
        let local_addr = try_recover!(listener.local_addr(), self);

        Ok(Server {
            state: AsyncListening { listener },
            addr: local_addr,
        })
    }
}

impl Transitionable<Closed> for Server<AsyncListening> {
    type NextStateful = Server<Closed>;

    fn transition(self) -> Self::NextStateful {
        Server {
            addr: self.addr,
            state: Closed,
        }
    }
}

impl TryTransitionable<Accepted, Listening> for Server<Listening> {
    type SuccessStateful = Server<Accepted>;
    type FailureStateful = Server<Listening>;
    type Error = anyhow::Error;

    fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let (stream, _) = try_recover!(self.state.listener.accept(), self, Listening);

        // Disable Nagle's algorithm for lower latency
        try_recover!(stream.set_nodelay(true), self, Listening);

        try_recover!(stream.set_nonblocking(false), self, Listening);
        Ok(Server {
            state: Accepted {
                stream,
                listener: self.state.listener,
            },
            addr: self.addr,
        })
    }
}

impl AsyncTryTransitionable<AsyncAccepted, AsyncListening> for Server<AsyncListening> {
    type SuccessStateful = Server<AsyncAccepted>;
    type FailureStateful = Server<AsyncListening>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let (stream, _) = try_recover_async!(self.state.listener.accept(), self, AsyncListening);

        // Disable Nagle's algorithm for lower latency
        try_recover!(stream.set_nodelay(true), self, AsyncListening);

        Ok(Server {
            state: AsyncAccepted {
                stream,
                listener: self.state.listener,
            },
            addr: self.addr,
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
        }
    }
}

impl Transitionable<AsyncListening> for Server<AsyncAccepted> {
    type NextStateful = Server<AsyncListening>;

    fn transition(self) -> Self::NextStateful {
        Server {
            state: AsyncListening {
                listener: self.state.listener,
            },
            addr: self.addr,
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
            addr: self.addr,
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
        let stream = try_recover!(std::net::TcpStream::connect(self.addr), self);

        // Disable Nagle's algorithm for lower latency
        try_recover!(stream.set_nodelay(true), self);

        try_recover!(stream.set_nonblocking(false), self);
        Ok(Client {
            addr: self.addr,
            state: Connected { stream },
        })
    }
}

impl AsyncTryTransitionable<AsyncConnected, Closed> for Client<Closed> {
    type SuccessStateful = Client<AsyncConnected>;
    type FailureStateful = Client<Closed>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let stream = try_recover_async!(tokio::net::TcpStream::connect(self.addr), self);

        // Disable Nagle's algorithm for lower latency
        try_recover!(stream.set_nodelay(true), self);
        Ok(Client {
            addr: self.addr,
            state: AsyncConnected { stream },
        })
    }
}

impl Transitionable<Closed> for Client<Connected> {
    type NextStateful = Client<Closed>;

    fn transition(self) -> Self::NextStateful {
        Client {
            state: Closed,
            addr: self.addr,
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
