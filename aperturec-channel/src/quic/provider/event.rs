use aperturec_metrics::create_metric;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

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
    recent_rtts: BTreeMap<Instant, Duration>,
}

impl MetricsSubscriberContext {
    const RTT_RETENTION_TIME: Duration = Duration::from_secs(1);
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

        MetricsSubscriberContext {
            recent_rtts: BTreeMap::default(),
        }
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
        let now = Instant::now();

        ctx.recent_rtts.retain(|&record_time, _| {
            now.duration_since(record_time) < Self::ConnectionContext::RTT_RETENTION_TIME
        });
        ctx.recent_rtts.insert(now, event.latest_rtt);
        let mean = ctx
            .recent_rtts
            .values()
            .map(Duration::as_secs_f64)
            .sum::<f64>()
            / ctx.recent_rtts.len() as f64;
        RttAverage::update(mean);
        let variance = ctx
            .recent_rtts
            .values()
            .map(Duration::as_secs_f64)
            .map(|x| (x - mean).powi(2))
            .sum::<f64>()
            / ctx.recent_rtts.len() as f64;
        RttVariance::update(variance);
        let max = ctx
            .recent_rtts
            .values()
            .max()
            .map(Duration::as_secs_f64)
            .unwrap();
        RttMax::update(max);
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

    fn on_connection_closed(
        &mut self,
        _context: &mut Self::ConnectionContext,
        _meta: &event::ConnectionMeta,
        _event: &events::ConnectionClosed,
    ) {
        Mtu::update(f64::NAN);
        RttAverage::update(f64::NAN);
        RttVariance::update(f64::NAN);
        RttMax::update(f64::NAN);
    }
}
