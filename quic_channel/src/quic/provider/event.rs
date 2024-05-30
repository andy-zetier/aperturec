use aperturec_trace::log;

use s2n_quic::provider::event::{
    self,
    events::{self, Frame},
};
use tokio::sync::watch;

pub struct MtuSubscriber;

pub type ConnectionId = u64;

pub const DEFAULT_MTU: usize = 1200;

impl event::Subscriber for MtuSubscriber {
    type ConnectionContext = MtuContext;

    fn create_connection_context(
        &mut self,
        _: &event::ConnectionMeta,
        _: &event::ConnectionInfo,
    ) -> Self::ConnectionContext {
        MtuContext {
            subscriptions: watch::channel(DEFAULT_MTU).0,
        }
    }

    fn on_mtu_updated(
        &mut self,
        ctx: &mut Self::ConnectionContext,
        meta: &event::ConnectionMeta,
        event: &event::events::MtuUpdated,
    ) {
        log::debug!(
            "New MTU on path {} connection {}: {} cause {:?}",
            meta.id,
            event.path_id,
            event.mtu,
            event.cause
        );
        ctx.subscriptions.send_replace(event.mtu as usize);
    }
}

pub struct MtuContext {
    subscriptions: watch::Sender<usize>,
}

impl MtuContext {
    pub fn subscribe(&mut self) -> MtuSubscription {
        self.subscriptions.subscribe()
    }
}

pub type MtuSubscription = watch::Receiver<usize>;

pub struct TrxSubscriber;
pub struct TrxContext;

fn application_bytes_in_frame(frame: &Frame) -> Option<usize> {
    match frame {
        Frame::Stream { len, .. } | Frame::Datagram { len, .. } => Some(*len as usize),
        _ => None,
    }
}

impl event::Subscriber for TrxSubscriber {
    type ConnectionContext = TrxContext;

    fn create_connection_context(
        &mut self,
        _: &event::ConnectionMeta,
        _: &event::ConnectionInfo,
    ) -> Self::ConnectionContext {
        TrxContext
    }

    fn on_frame_sent(
        &mut self,
        _context: &mut Self::ConnectionContext,
        _meta: &event::ConnectionMeta,
        event: &events::FrameSent,
    ) {
        if let Some(nbytes) = application_bytes_in_frame(&event.frame) {
            aperturec_metrics::builtins::tx_bytes(nbytes);
        }
    }

    fn on_frame_received(
        &mut self,
        _context: &mut Self::ConnectionContext,
        _meta: &event::ConnectionMeta,
        event: &events::FrameReceived,
    ) {
        if let Some(nbytes) = application_bytes_in_frame(&event.frame) {
            aperturec_metrics::builtins::rx_bytes(nbytes);
        }
    }

    fn on_packet_dropped(
        &mut self,
        _context: &mut Self::ConnectionContext,
        _meta: &event::ConnectionMeta,
        _event: &events::PacketDropped,
    ) {
        aperturec_metrics::builtins::packet_lost(1);
    }

    fn on_packet_lost(
        &mut self,
        _context: &mut Self::ConnectionContext,
        _meta: &event::ConnectionMeta,
        _event: &events::PacketLost,
    ) {
        aperturec_metrics::builtins::packet_lost(1);
    }

    fn on_packet_sent(
        &mut self,
        _context: &mut Self::ConnectionContext,
        _meta: &event::ConnectionMeta,
        _event: &events::PacketSent,
    ) {
        aperturec_metrics::builtins::packet_sent(1);
    }
}
