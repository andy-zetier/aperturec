use s2n_quic::provider::event::{
    self,
    events::{self, Frame},
};

pub(crate) struct TrxEventSubscriber;
pub(crate) struct TrxEventSubscriberCtx;

fn application_bytes_in_frame(frame: &Frame) -> Option<usize> {
    match frame {
        Frame::Stream { len, .. } | Frame::Datagram { len, .. } => Some(*len as usize),
        _ => None,
    }
}

impl event::Subscriber for TrxEventSubscriber {
    type ConnectionContext = TrxEventSubscriberCtx;

    fn create_connection_context(
        &mut self,
        _: &event::ConnectionMeta,
        _: &event::ConnectionInfo,
    ) -> Self::ConnectionContext {
        TrxEventSubscriberCtx
    }

    fn on_frame_sent(
        &mut self,
        _context: &mut Self::ConnectionContext,
        _meta: &event::ConnectionMeta,
        event: &events::FrameSent,
    ) {
        if let Some(nbytes) = application_bytes_in_frame(&event.frame) {
            aperturec_metrics::builtins::tx_bytes(nbytes as usize);
        }
    }

    fn on_frame_received(
        &mut self,
        _context: &mut Self::ConnectionContext,
        _meta: &event::ConnectionMeta,
        event: &events::FrameReceived,
    ) {
        if let Some(nbytes) = application_bytes_in_frame(&event.frame) {
            aperturec_metrics::builtins::rx_bytes(nbytes as usize);
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
