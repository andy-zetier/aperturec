use crate::channels::spawn_rx_thread;

use aperturec_channel::{self as channel, Sender as _};
use aperturec_protocol::control::{
    self as proto, client_to_server as c2s, server_to_client as s2c,
};
use aperturec_utils::channels::SenderExt;

use crossbeam::channel::{Receiver, Sender, bounded, select_biased};
use std::thread;
use tracing::*;

#[derive(Debug)]
pub enum QuitReason {
    UiExited,
    SigintReceived,
    NetworkError,
}

impl From<QuitReason> for proto::ClientGoodbyeReason {
    fn from(reason: QuitReason) -> Self {
        match reason {
            QuitReason::UiExited => proto::ClientGoodbyeReason::UserRequested,
            QuitReason::NetworkError => proto::ClientGoodbyeReason::NetworkError,
            QuitReason::SigintReceived => proto::ClientGoodbyeReason::Terminating,
        }
    }
}

/// Extension trait for converting server goodbye reasons to user-friendly strings.
///
/// Provides a method to obtain a user-friendly description for primary-thread logging.
trait ServerGoodbyeReasonExt {
    /// Returns a user-friendly description of the goodbye reason.
    fn friendly_str(&self) -> &'static str;
}

impl ServerGoodbyeReasonExt for proto::ServerGoodbyeReason {
    fn friendly_str(&self) -> &'static str {
        match self {
            proto::ServerGoodbyeReason::AuthenticationFailure => {
                "Authentication failure, check auth token"
            }
            proto::ServerGoodbyeReason::ClientExecDisallowed => {
                "Server does not allow client specified applications"
            }
            proto::ServerGoodbyeReason::InactiveTimeout => "Client has been inactive for too long",
            proto::ServerGoodbyeReason::NetworkError => "A network error has occurred",
            proto::ServerGoodbyeReason::OtherLogin => "Server was logged into elsewhere",
            proto::ServerGoodbyeReason::ProcessExited => "Remote application has terminated",
            proto::ServerGoodbyeReason::ProcessLaunchFailed => {
                "Failed to launch remote application, check server logs"
            }
            proto::ServerGoodbyeReason::ShuttingDown => "Server is shutting down",
            _ => self.as_str_name(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    ChannelRx(#[from] channel::codec::in_order::RxError),
    #[error(transparent)]
    ChannelTx(#[from] channel::codec::in_order::TxError),
}

/// Signals emitted by the control channel back to the primary thread.
#[derive(Debug)]
pub enum PrimaryThreadNotification {
    /// Fatal error occurred while reading/writing the control channel.
    Error(Error),
    /// Server sent its initialization payload (capabilities, IDs, etc.).
    ServerInit(proto::ServerInit),
    /// Server is shutting down and provided a goodbye reason string.
    ServerGoodbye(String),
}
type Ptn = PrimaryThreadNotification;

/// Messages that the primary thread can ask the control channel to transmit.
#[derive(Debug)]
pub enum Notification {
    /// Send the client handshake message.
    Init(Box<proto::ClientInit>),
    /// Request a graceful shutdown from the server with a reason and client ID.
    Goodbye {
        reason: proto::ClientGoodbyeReason,
        id: u64,
    },
    /// Stop the control channel threads.
    Terminate,
}

/// Spawn control-channel workers to bridge handshake/teardown messages.
///
/// A network reader forwards `ServerInit` and `ServerGoodbye` messages to the primary thread,
/// while the main loop consumes `Notification`s from the primary thread and sends the
/// corresponding control messages over QUIC. Threads exit when either side closes the control
/// channel (e.g., QUIC connection teardown or sender dropped), or when the primary requests
/// termination and closes its notification channel.
///
/// # Parameters
/// * `client_cc` - Control channel handle split into Rx/Tx halves.
/// * `pt_tx` - Notifies the primary thread of server control messages or errors.
/// * `from_pt_rx` - Receives control notifications originating from the primary thread.
pub fn setup(
    client_cc: channel::ClientControl,
    pt_tx: Sender<Ptn>,
    from_pt_rx: Receiver<Notification>,
) {
    let (cc_rx, mut cc_tx) = client_cc.split();

    let (from_network_tx, from_network_rx) = bounded(0);

    let network_rx_thread = spawn_rx_thread(cc_rx, from_network_tx, debug_span!("cc-network-rx"));

    thread::spawn(move || {
        let _s = debug_span!("cc-main").entered();
        debug!("started");
        loop {
            select_biased! {
                recv(from_pt_rx) -> pt_msg_res => {
                    let Ok(pt_msg) = pt_msg_res else {
                        warn!("primary died before cc-main");
                        break;
                    };
                    let msg = match pt_msg {
                        Notification::Terminate => break,
                        Notification::Init(ci) => c2s::Message::from(*ci),
                        Notification::Goodbye { reason, id } => c2s::Message::from(
                            proto::ClientGoodbyeBuilder::default()
                                .reason(reason)
                                .client_id(id)
                                .build()
                                .expect("build ClientGoodbye"),
                        ),
                    };
                    if let Err(error) = cc_tx.send(msg) {
                        error!(%error);
                        pt_tx.send_or_warn(Ptn::Error(error.into()));
                    }
                }
                recv(from_network_rx) -> network_msg_res => {
                    let Ok(network_msg) = network_msg_res else {
                        debug!("cc-network-rx died before CC main");
                        break;
                    };
                    match network_msg {
                        Ok(s2c::Message::ServerGoodbye(gb)) => {
                            let reason = gb.reason().friendly_str().to_string();
                            pt_tx.send_or_warn(Ptn::ServerGoodbye(reason));
                        }
                        Ok(s2c::Message::ServerInit(si)) => {
                            pt_tx.send_or_warn(Ptn::ServerInit(si));
                        }
                        Err(err) => {
                            pt_tx.send_or_warn(Ptn::Error(err.into()));
                            break;
                        }
                    }
                }
            }
        }
        if let Err(error) = network_rx_thread.join() {
            warn!("cc-network-rx panicked: {error:?}");
        }
        debug!("exiting");
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quit_reason_maps_to_proto_reason() {
        assert_eq!(
            proto::ClientGoodbyeReason::from(QuitReason::UiExited),
            proto::ClientGoodbyeReason::UserRequested
        );
        assert_eq!(
            proto::ClientGoodbyeReason::from(QuitReason::NetworkError),
            proto::ClientGoodbyeReason::NetworkError
        );
        assert_eq!(
            proto::ClientGoodbyeReason::from(QuitReason::SigintReceived),
            proto::ClientGoodbyeReason::Terminating
        );
    }

    #[test]
    fn server_goodbye_reason_has_friendly_strings() {
        assert_eq!(
            proto::ServerGoodbyeReason::AuthenticationFailure.friendly_str(),
            "Authentication failure, check auth token"
        );
        assert_eq!(
            proto::ServerGoodbyeReason::OtherLogin.friendly_str(),
            "Server was logged into elsewhere"
        );
        assert_eq!(
            proto::ServerGoodbyeReason::ProcessExited.friendly_str(),
            "Remote application has terminated"
        );
        assert_eq!(
            proto::ServerGoodbyeReason::ServerGoodbyeUnspecified.friendly_str(),
            proto::ServerGoodbyeReason::ServerGoodbyeUnspecified.as_str_name()
        );
    }

    fn assert_send<T: Send>() {}

    #[test]
    fn notifications_are_send() {
        assert_send::<Notification>();
        assert_send::<PrimaryThreadNotification>();
    }
}
