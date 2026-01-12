pub mod control;
pub mod event;
pub mod media;
pub mod tunnel;

use aperturec_channel as channel;
use crossbeam::channel::Sender;
use std::thread::{self, JoinHandle};
use tracing::Span;
use tracing::*;

/// Spawn a blocking network receive loop that forwards results onto a crossbeam channel.
///
/// Shared helper for channel modules to avoid repeating the same receive-and-forward thread.
pub(crate) fn spawn_rx_thread<R, M, E>(
    mut rx: R,
    tx: Sender<Result<M, E>>,
    span: Span,
) -> JoinHandle<()>
where
    R: channel::Receiver<Message = M, Error = E> + Send + 'static,
    M: Send + 'static,
    E: Send + 'static,
{
    thread::spawn(move || {
        let _s = span.entered();
        debug!("started");
        loop {
            match rx.receive() {
                Ok(msg) => {
                    if tx.send(Ok(msg)).is_err() {
                        debug!("downstream receiver disconnected, exiting rx thread");
                        break;
                    }
                }
                Err(err) => {
                    if tx.send(Err(err)).is_err() {
                        debug!("downstream receiver disconnected, error could not be sent");
                    }
                    break;
                }
            }
        }
        debug!("exiting");
    })
}
