extern crate aperturec_protocol;
extern crate asn1rs;

use aperturec_protocol::common_types::*;
use asn1rs::prelude::*;

#[macro_use]
mod macros;

#[test]
fn serde_duration() {
    serde_type!(DurationMs, DurationMs(1337));
}

#[test]
fn serde_client_id() {
    serde_type!(ClientId, ClientId(8908990));
}

#[test]
fn serde_dimension() {
    serde_type!(
        Dimension,
        Dimension {
            width: 1920,
            height: 1080
        }
    );
}

#[test]
fn serde_location() {
    serde_type!(
        Location,
        Location {
            x_position: 500,
            y_position: 250,
        }
    );
}

#[test]
fn serde_code() {
    serde_type!(Codec, Codec::Raw);
}

#[test]
fn serde_heartbeat_id() {
    serde_type!(HeartbeatId, HeartbeatId(8092));
}
