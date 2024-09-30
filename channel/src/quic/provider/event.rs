use aperturec_metrics::create_metric;

use s2n_quic::provider::event::{
    self,
    events::{self, Frame},
};

fn application_bytes_in_frame(frame: &Frame) -> Option<usize> {
    match frame {
        Frame::Stream { len, .. } | Frame::Datagram { len, .. } => Some(*len as usize),
        _ => None,
    }
}

create_metric!(Mtu);
create_metric!(RttAverage);
create_metric!(RttVariance);
create_metric!(RttMax);
pub struct MetricsSubscriber;
pub struct MetricsSubscriberContext {
    max_rtt: f64,
}

impl event::Subscriber for MetricsSubscriber {
    type ConnectionContext = MetricsSubscriberContext;

    fn create_connection_context(
        &mut self,
        _: &event::ConnectionMeta,
        _: &event::ConnectionInfo,
    ) -> Self::ConnectionContext {
        Mtu::register();
        RttAverage::register();
        RttVariance::register();
        RttMax::register();

        MetricsSubscriberContext { max_rtt: f64::MIN }
    }

    fn on_mtu_updated(
        &mut self,
        _: &mut Self::ConnectionContext,
        _: &event::ConnectionMeta,
        event: &event::events::MtuUpdated,
    ) {
        Mtu::update(event.mtu);
    }

    fn on_recovery_metrics(
        &mut self,
        ctx: &mut Self::ConnectionContext,
        _: &event::ConnectionMeta,
        event: &event::events::RecoveryMetrics,
    ) {
        RttAverage::update(event.smoothed_rtt.as_secs_f64());
        RttVariance::update(event.rtt_variance.as_secs_f64());

        if event.latest_rtt.as_secs_f64() > ctx.max_rtt {
            ctx.max_rtt = event.latest_rtt.as_secs_f64();
            RttMax::update(ctx.max_rtt);
        }
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
