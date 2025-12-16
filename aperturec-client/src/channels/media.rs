use crate::channels::NETWORK_CHANNEL_TIMEOUT;
use crate::frame::{Draw, Framer};

use aperturec_channel::{self as channel, TimeoutReceiver as _};
use aperturec_graphics::display;
use aperturec_utils::channels::SenderExt;

use crossbeam::channel::{Receiver, Sender, bounded, select_biased};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};
use tracing::*;

/// Notifications emitted by the media channel towards the primary thread.
#[derive(Debug)]
pub enum PrimaryThreadNotification {
    /// Media channel hit an unrecoverable error.
    Error(Error),
    /// A frame is ready for composition on the UI thread.
    Draw(Draw),
}
type Ptn = PrimaryThreadNotification;

/// Messages the primary thread can send to adjust the media pipeline.
#[derive(Debug)]
pub enum Notification {
    /// Updated display configuration used to reset the framer.
    DisplayConfiguration(display::DisplayConfiguration),
    /// Ask the media channel threads to terminate.
    Terminate,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("channel receive")]
    ChannelRx(#[from] channel::codec::out_of_order::RxError),
    #[error("channel send")]
    ChannelTx(#[from] channel::codec::out_of_order::TxError),
}

/// Spawn the media channel worker threads.
///
/// A network reader pulls media messages off the QUIC stream, and the main loop feeds them
/// into a `Framer`, which turns buffered media into `Draw` notifications. Display changes from
/// the primary thread cause the framer to reset so sizing stays in sync.
///
/// # Parameters
/// * `mc` - Media channel handle used to receive server-originated media messages.
/// * `pt_tx` - Sends `Draw` or error notifications back to the primary thread.
/// * `from_pt_rx` - Receives display updates or termination requests.
pub fn setup(mut mc: channel::ClientMedia, pt_tx: Sender<Ptn>, from_pt_rx: Receiver<Notification>) {
    let should_stop = Arc::new(AtomicBool::new(false));
    let should_stop_read = should_stop.clone();
    let (from_network_tx, from_network_rx) = bounded(0);

    let network_rx_thread = thread::spawn(move || {
        let _s = debug_span!("mc-network-rx").entered();
        debug!("started");
        loop {
            if should_stop_read.load(Ordering::Acquire) {
                break;
            }
            match mc.receive_timeout(NETWORK_CHANNEL_TIMEOUT) {
                Ok(None) => continue,
                Ok(Some(msg)) => from_network_tx.send_or_warn(Ok(msg)),
                Err(err) => {
                    from_network_tx.send_or_warn(Err(err));
                    break;
                }
            }
        }
        debug!("exiting");
    });

    thread::spawn(move || {
        let _s = debug_span!("mc-main").entered();
        debug!("started");
        let Ok(first_notif) = from_pt_rx.recv() else {
            warn!("no media notifications");
            should_stop.store(true, Ordering::Release);
            return;
        };
        let first_dc = match first_notif {
            Notification::Terminate => {
                warn!("terminated before any media notifications");
                should_stop.store(true, Ordering::Release);
                return;
            }
            Notification::DisplayConfiguration(first_dc) => first_dc,
        };

        let mut framer = Framer::new(first_dc.clone());
        loop {
            if framer.has_draws() {
                for draw in framer.get_draws_and_reset() {
                    pt_tx.send_or_warn(Ptn::Draw(draw));
                }
            }

            select_biased! {
                recv(from_pt_rx) -> pt_msg_res => {
                    let Ok(pt_msg) = pt_msg_res else {
                        warn!("primary died before mc-main");
                        break;
                    };
                    let display_config = match pt_msg {
                        Notification::Terminate => break,
                        Notification::DisplayConfiguration(display_config) => display_config,
                    };
                    if display_config.id > framer.display_config.id {
                        framer = Framer::new(display_config.clone());
                    }
                }
                recv(from_network_rx) -> network_msg_res => {
                    let Ok(network_msg) = network_msg_res else {
                        debug!("mc-network-rx died before mc-main");
                        break;
                    };
                    match network_msg {
                        Ok(msg) => {
                            let Some(msg) = msg.message else {
                                warn!("media message with empty body");
                                continue;
                            };
                            if let Err(error) = framer.report_mm(msg) {
                                warn!(%error, "error processing media message");
                            }
                        },
                        Err(err) => pt_tx.send_or_warn(Ptn::Error(err.into())),
                    }
                },
            }
        }
        should_stop.store(true, Ordering::Release);
        if let Err(error) = network_rx_thread.join() {
            warn!("mc-network-rx panicked: {:?}", error)
        }
        debug!("exited");
    });
}
