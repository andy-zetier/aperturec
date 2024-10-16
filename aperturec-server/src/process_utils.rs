use os_pipe::PipeWriter;
use std::process::Stdio;
use std::str;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::unix::pipe;
use tracing::*;

pub struct StdoutTracer {
    inner: Tracer,
}

pub struct StderrTracer {
    inner: Tracer,
}

struct Tracer {
    pipe_writer: PipeWriter,
}

const TARGET: &str = "STDIO";

macro_rules! construct_tracer {
    ($name:expr, $level:expr) => {{
        let (pipe_reader, pipe_writer) = os_pipe::pipe().expect("os_pipe");
        let span = match $level {
            Level::ERROR => error_span!(target: TARGET, $name),
            Level::WARN => warn_span!(target: TARGET, $name),
            Level::INFO => info_span!(target: TARGET, $name),
            Level::DEBUG => debug_span!(target: TARGET, $name),
            Level::TRACE => trace_span!(target: TARGET, $name),
        };
        tokio::spawn(async move {
            let mut pipe_receiver = BufReader::new(
                pipe::Receiver::from_owned_fd(pipe_reader.into()
            ).expect("from_owned_fd"));
            let mut s = String::default();
            loop {
                match pipe_receiver.read_line(&mut s).await {
                    Ok(_) => {
                        let output = s.trim_end();
                        span.in_scope(|| match $level {
                            Level::ERROR => error!(target: TARGET, %output),
                            Level::WARN => warn!(target: TARGET, %output),
                            Level::INFO => info!(target: TARGET, %output),
                            Level::DEBUG => debug!(target: TARGET, %output),
                            Level::TRACE => trace!(target: TARGET, %output),
                        });
                        s.clear();
                    },
                    Err(_) => {
                        let output = pipe_receiver.fill_buf().await.expect("fill_buf");
                        let len = output.len();
                        span.in_scope(|| match $level {
                            Level::ERROR => error!(target: TARGET, ?output, "non-utf8"),
                            Level::WARN => warn!(target: TARGET, ?output, "non-utf8"),
                            Level::INFO => info!(target: TARGET, ?output, "non-utf8"),
                            Level::DEBUG => debug!(target: TARGET, ?output, "non-utf8"),
                            Level::TRACE => trace!(target: TARGET, ?output, "non-utf8"),
                        });
                        pipe_receiver.consume(len);
                    }
                }
            }
        });
        Tracer {
            pipe_writer,
        }
    }}
}

impl StdoutTracer {
    pub fn new(level: Level) -> Self {
        StdoutTracer {
            inner: construct_tracer!("stdio", level),
        }
    }
}

impl StderrTracer {
    pub fn new(level: Level) -> Self {
        StderrTracer {
            inner: construct_tracer!("stderr", level),
        }
    }
}

macro_rules! delegate_into_stdio {
    ($type:ty, $inner:ident) => {
        impl From<$type> for Stdio {
            fn from(t: $type) -> Stdio {
                t.$inner.into()
            }
        }
    };
}

delegate_into_stdio!(StdoutTracer, inner);
delegate_into_stdio!(StderrTracer, inner);
delegate_into_stdio!(Tracer, pipe_writer);
